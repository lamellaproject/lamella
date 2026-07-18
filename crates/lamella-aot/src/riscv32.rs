//! The RV32IM (RISC-V) target code generator.

use alloc::vec::Vec;

use lamella_asm_riscv32::{BranchCond, Encoder, Label, Reg};
use lamella_ir::{
    BinOp, CmpOp, ConvKind, Function, Inst, MirType, StaticOwner, Terminator, TypeHandle, ValueId,
};

use crate::resolver::{TypeMeta, VtableEntry, descriptor_symbol};
pub use crate::resolver::DescQualifiers;
pub use crate::stackmaps::AssemblyStatics;
use crate::stackmaps::{
    encode_stackmap_record, pinned_values, STACKMAP_KIND_MANAGED_PTR, STACKMAP_KIND_OBJECT_REF,
    STACKMAP_KIND_PINNED, STACKMAP_KIND_TAGGED, STACKMAP_MODE_METHOD_SLOTS, STACKMAP_MODE_STATICS,
};

/// Why a function could not be lowered to RV32IM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowerError {
    /// The function did not pass IR verification.
    NotWellFormed,
    /// An instruction or shape this backend does not lower yet.
    Unsupported,
    /// The all-spilled frame's slot offsets exceed the 12-bit lw/sw immediate (a function past
    /// ~500 values).
    TooManyValues,
    /// A control-flow shape this backend does not handle.
    ControlFlowUnsupported,
    /// The final image could not be assembled (an out-of-range branch).
    CodeTooLarge,
}

/// The target register profile. `Rv32im` uses all 32 registers + hardware mul/div (QEMU `virt` and
/// larger cores); `Rv32ec` -- the CH32V003 and other tiny cores -- is RV32E(C): only x0-x15 exist and
/// there is no M-extension. RV32E therefore takes an EMPTY allocatable pool (so every value-bearing
/// function goes down the all-spilled path, which stays within x0-x15), swaps `t6` for `s1` as the
/// spilled scratch ([`spilled_scratch`]), caps arguments at a0-a5 (six), and lowers mul/div/rem to
/// soft-routine calls.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum RiscvProfile {
    /// RV32IM: 32 registers, hardware mul/div. The default.
    #[default]
    Rv32im,
    /// RV32E(C): 16 registers (x0-x15 only), no M-extension.
    Rv32ec,
}

/// The number of argument registers a profile passes in registers: a0-a7 on RV32IM, a0-a5 on RV32E.
fn arg_reg_count(profile: RiscvProfile) -> usize {
    match profile {
        RiscvProfile::Rv32im => 8,
        RiscvProfile::Rv32ec => 6,
    }
}

/// The callee-saved registers the trivial value map hands out (RV32IM: s0-s11 = x8, x9, x18-x27).
/// Callee-saved means a value survives a `call` without spilling -- the prologue saves each one the
/// function uses and the epilogue restores it. RV32E has no x18-x31, so its pool is EMPTY: every
/// value-bearing function takes the all-spilled path instead, which this backend keeps within x0-x15.
/// `a0`-`a7`/`a0`-`a5` carry call arguments and the return value; `ra`/`sp`/`x0` are ABI-reserved.
fn allocatable(profile: RiscvProfile) -> Vec<Reg> {
    let r = |n: u8| Reg::new(n).unwrap_or(Reg::ZERO);
    match profile {
        RiscvProfile::Rv32im => {
            alloc::vec![
                r(8),
                r(9),
                r(18),
                r(19),
                r(20),
                r(21),
                r(22),
                r(23),
                r(24),
                r(25),
                r(26),
                r(27)
            ]
        }
        RiscvProfile::Rv32ec => Vec::new(),
    }
}

/// The argument/return register `a<index>` (x10-x17), or `None` past the eighth. RV32E callers
/// pre-validate that no function passes more than six arguments (see [`arg_reg_count`]), so on that
/// profile this is only ever indexed `0..6` (a0-a5).
fn arg_reg(index: usize) -> Option<Reg> {
    (index < 8).then(|| Reg::new(10 + index as u8).unwrap_or(Reg::ZERO))
}

/// The all-spilled path's extra scratch register -- the array-element address, and the fourth temporary
/// in the int64 helpers. `t6` (x31) on RV32IM; `s1` (x9) on RV32E, which has no x31. The RV32E spilled
/// prologue saves + restores `s1`, so borrowing it here never clobbers a caller's value.
fn spilled_scratch(profile: RiscvProfile) -> Reg {
    let n = match profile {
        RiscvProfile::Rv32im => 31,
        RiscvProfile::Rv32ec => 9,
    };
    Reg::new(n).unwrap_or(Reg::ZERO)
}

/// `t6` (x31), the REGISTER-path array-addressing scratch. RV32E never takes the register path for a
/// value-bearing function (its pool is empty), so this stays x31; the all-spilled path uses
/// [`spilled_scratch`] instead.
fn scratch() -> Reg {
    Reg::new(31).unwrap_or(Reg::ZERO)
}

/// The number of argument REGISTERS a call-like instruction marshals -- so RV32E can reject one that
/// would spill past a0-a5 into a6/a7. A direct/native/indirect/delegate call passes its `args` in
/// a0..; a virtual or interface call passes the receiver in a0 and its `args` from a1, so one more.
fn register_arg_count(inst: &Inst) -> usize {
    match inst {
        Inst::Call { args, .. }
        | Inst::CallNative { args, .. }
        | Inst::CallIndirect { args, .. }
        | Inst::InvokeDelegate { args, .. } => args.len(),
        Inst::CallVirtual { args, .. } | Inst::CallInterface { args, .. } => args.len() + 1,
        _ => 0,
    }
}

/// The number of argument WORDS a `Call`/`CallNative` must pass on the STACK -- the overflow past the
/// profile's argument registers (a0-a7 on RV32IM, a0-a5 on RV32E), which [`marshal_call_args`] stores into
/// the outgoing-args area at the BOTTOM of the caller's spilled frame (`sp+0..`). Mirrors that function's
/// register packing exactly: a wide (>2-word) value-type argument rides ONE register (its slot ADDRESS, by
/// reference), an int64/f64/small-struct a register PAIR, a scalar one; and a wide RESULT reserves a0 for
/// the sret pointer so the explicit arguments start at a1. Returns 0 for a call that fits in registers, or
/// a non-`Call`/`CallNative` inst -- the dispatch kinds (indirect/virtual/interface/delegate) still cap at
/// the argument registers (they do not model stack-passed arguments).
fn call_stack_words(
    inst: &Inst,
    result: ValueId,
    value_types: &[MirType],
    profile: RiscvProfile,
) -> usize {
    let args = match inst {
        Inst::Call { args, .. } | Inst::CallNative { args, .. } => args,
        _ => return 0,
    };
    let regs = arg_reg_count(profile);
    let mut reg = (value_words(value_types, result) > 2) as usize;
    let mut stack = 0usize;
    for &arg in args {
        let words = value_words(value_types, arg);
        let units = if words > 2 { 1 } else { words as usize };
        for _ in 0..units {
            if reg < regs {
                reg += 1;
            } else {
                stack += 1;
            }
        }
    }
    stack
}

/// True if `op` is an integer mul/div/rem -- the operations RV32E must lower to a soft-routine CALL,
/// since it has no M-extension. Add/sub, bitwise, and shifts stay native (base RV32I).
fn is_soft_int_binop(op: BinOp) -> bool {
    matches!(
        op,
        BinOp::Mul | BinOp::DivSigned | BinOp::DivUnsigned | BinOp::RemSigned | BinOp::RemUnsigned
    )
}

/// The compiler-rt soft-routine symbol for an integer mul/div/rem `op`, called `(a0, a1) -> a0`. These
/// are the standard names a `libgcc`/`compiler-builtins` (or the hand-written provider) exports.
fn soft_int_routine(op: BinOp) -> &'static str {
    match op {
        BinOp::Mul => "__mulsi3",
        BinOp::DivSigned => "__divsi3",
        BinOp::DivUnsigned => "__udivsi3",
        BinOp::RemSigned => "__modsi3",
        BinOp::RemUnsigned => "__umodsi3",
        _ => "__mulsi3",
    }
}

/// The compiler-rt soft routine for an int64 (two-word) DIV/REM `op`, called `(a0:a1, a2:a3) -> a0:a1`.
/// There is no inline 64-bit division on either profile, so these route to a call regardless of RV32IM /
/// RV32E. `None` for a non-div/rem op (int64 mul is handled separately -- inline on RV32IM, `__muldi3`
/// on RV32E; add/sub/shift/bitwise lower inline via [`emit_i64_binary`]).
fn int64_soft_routine(op: BinOp) -> Option<&'static str> {
    match op {
        BinOp::DivSigned => Some("__divdi3"),
        BinOp::DivUnsigned => Some("__udivdi3"),
        BinOp::RemSigned => Some("__moddi3"),
        BinOp::RemUnsigned => Some("__umoddi3"),
        _ => None,
    }
}

/// Whether `value` is a float (`f32`/`f64`). The RV32IM/RV32E cores have no FPU, so every float op lowers
/// to a `compiler_builtins`/libgcc soft-float CALL (the compiler-rt names, NOT ARM's `__aeabi_*`); an
/// `f32` rides one register (a0), an `f64` a register pair (a0:a1), matching the multi-word value ABI.
fn is_float(value_types: &[MirType], value: ValueId) -> bool {
    matches!(
        value_types.get(value.index()),
        Some(MirType::F32 | MirType::F64)
    )
}

/// The compiler-rt soft-float routine for a float ARITHMETIC `Binary`, keyed by the (float) result width;
/// `None` when the result is not a float (an integer op). `f32` -> `__addsf3`/... called `(a0, a1) -> a0`;
/// `f64` -> `__adddf3`/... called `(a0:a1, a2:a3) -> a0:a1`. Float `%` has no compiler-rt helper (it is a
/// library `fmod`), so `Rem` returns `None` and the caller rejects a float remainder as `Unsupported`.
fn soft_float_arith(op: BinOp, result_ty: Option<&MirType>) -> Option<&'static str> {
    let is_f64 = matches!(result_ty, Some(MirType::F64));
    if !is_f64 && !matches!(result_ty, Some(MirType::F32)) {
        return None;
    }
    Some(match (op, is_f64) {
        (BinOp::Add, true) => "__adddf3",
        (BinOp::Sub, true) => "__subdf3",
        (BinOp::Mul, true) => "__muldf3",
        (BinOp::DivSigned | BinOp::DivUnsigned, true) => "__divdf3",
        (BinOp::Add, false) => "__addsf3",
        (BinOp::Sub, false) => "__subsf3",
        (BinOp::Mul, false) => "__mulsf3",
        (BinOp::DivSigned | BinOp::DivUnsigned, false) => "__divsf3",
        _ => return None,
    })
}

/// The compiler-rt soft routine for a float CONVERSION the no-FPU core cannot do inline, with the source
/// and destination WORD counts (for the register-pair marshalling: `f64`/`i64` span a0:a1, the rest a0).
/// `None` for an integer widen/narrow, which stays inline in [`emit_convert`].
fn soft_float_convert(kind: ConvKind) -> Option<(&'static str, u32, u32)> {
    Some(match kind {
        ConvKind::IntToFloat32 => ("__floatsisf", 1, 1),
        ConvKind::IntToFloat64 => ("__floatsidf", 1, 2),
        ConvKind::UIntToFloat64 => ("__floatunsidf", 1, 2),
        ConvKind::LongToFloat64 => ("__floatdidf", 2, 2),
        ConvKind::LongToFloat32 => ("__floatdisf", 2, 1),
        ConvKind::ULongToFloat64 => ("__floatundidf", 2, 2),
        ConvKind::Float32ToInt => ("__fixsfsi", 1, 1),
        ConvKind::Float64ToInt => ("__fixdfsi", 2, 1),
        ConvKind::Float32ToFloat64 => ("__extendsfdf2", 1, 2),
        ConvKind::Float64ToFloat32 => ("__truncdfsf2", 2, 1),
        _ => return None,
    })
}

/// The compiler-rt comparison routine SUFFIX for a float `Compare`, plus the SIGNED integer test (of the
/// routine's returned int, against zero) that yields the CLI bool -- reused through [`materialize_compare`]
/// with `a0` vs `x0`. The routines are ORDERED and return `<0`/`0`/`>0`; the CLI's unordered forms
/// (`clt.un` etc.) select the opposite-sense routine and read its sign the other way -- e.g. `clt.un`
/// (a<b OR NaN) = `!(a>=b ordered)` = `__gedf2(...) < 0`. The full name is `__<suffix>df2`/`__<suffix>sf2`.
fn float_compare_plan(op: CmpOp) -> (&'static str, CmpOp) {
    match op {
        CmpOp::Eq => ("eq", CmpOp::Eq),
        CmpOp::Ne => ("ne", CmpOp::Ne),
        CmpOp::SignedLt => ("lt", CmpOp::SignedLt),
        CmpOp::SignedGt => ("gt", CmpOp::SignedGt),
        CmpOp::SignedLe => ("le", CmpOp::SignedLe),
        CmpOp::SignedGe => ("ge", CmpOp::SignedGe),
        CmpOp::UnsignedLt => ("ge", CmpOp::SignedLt),
        CmpOp::UnsignedGt => ("le", CmpOp::SignedGt),
        CmpOp::UnsignedLe => ("gt", CmpOp::SignedLe),
        CmpOp::UnsignedGe => ("lt", CmpOp::SignedGe),
    }
}

/// True if `inst` lowers to a soft-float CALL on the FPU-less RISC-V cores -- a float arithmetic `Binary`,
/// a float `Compare`, or a float `Convert`. Such a function is non-leaf (its `ra` is saved) and takes the
/// all-spilled path (which hosts the call marshalling), the same as an integer soft-routine call.
fn inst_is_softfloat_call(inst: &Inst, result: ValueId, value_types: &[MirType]) -> bool {
    match inst {
        Inst::Binary { .. } => is_float(value_types, result),
        Inst::Compare { lhs, .. } => is_float(value_types, *lhs),
        Inst::Convert { kind, .. } => soft_float_convert(*kind).is_some(),
        _ => false,
    }
}

/// Emits `product = a * b` (low 32 bits). RV32IM uses the hardware `mul`; RV32E has no M-extension, so
/// it shifts and adds INLINE (no call, so it never clobbers other live registers). The RV32E path
/// MUTATES `a` (shifted left) and `b` (shifted down to zero), so the caller must not need either after
/// -- true at the array-address helpers + the int64 cross-terms, where the operands are dead once the
/// product is formed. `product`/`a`/`b`/`tmp` must be four distinct registers.
fn emit_soft_mul32(
    enc: &mut Encoder,
    product: Reg,
    a: Reg,
    b: Reg,
    tmp: Reg,
    profile: RiscvProfile,
) {
    if !matches!(profile, RiscvProfile::Rv32ec) {
        enc.mul(product, a, b);
        return;
    }
    enc.li(product, 0);
    let top = enc.new_label();
    let skip = enc.new_label();
    let done = enc.new_label();
    enc.bind_label(top);
    enc.branch(BranchCond::Eq, b, Reg::ZERO, done);
    enc.andi(tmp, b, 1);
    enc.branch(BranchCond::Eq, tmp, Reg::ZERO, skip);
    enc.add(product, product, a);
    enc.bind_label(skip);
    enc.slli(a, a, 1);
    enc.srli(b, b, 1);
    enc.j(top);
    enc.bind_label(done);
}

/// The absolute RAM address where the module's static-field region begins -- a static field at MIR
/// byte `offset` lives at `STATIC_FIELD_BASE + offset`. Placed above the boot image (start of RAM),
/// the heap (`0x8010_0000`), and the stack (grows down from `0x8020_0000`) on the QEMU `virt` board,
/// which zeroes RAM at reset, so an unwritten static reads 0 (the CIL default). Mirrors ARM32's fixed
/// static base; a device build threads the linker-provided `.bss` address instead.
const STATIC_FIELD_BASE: u32 = 0x8030_0000;

/// Marks a call relocation whose target is an EXTERNAL symbol (an `Inst::CallNative`) rather than an
/// intra-module function index, so `lower_object` maps it to an undefined symbol the linker resolves
/// from another object. The high bit, disjoint from the small function/extern indices it flags.
const EXTERN_SYMBOL_FLAG: u32 = 0x8000_0000;

/// Marks a relocation whose low bits are a TYPE HANDLE, targeting that type's canonical descriptor
/// symbol (`__lamella_typedesc_<handle>`) rather than a function. `lower_object` resolves it to the
/// descriptor's symbol index (its WORDS, via a `+vtable_bytes` addend). Bit 30, disjoint from
/// [`EXTERN_SYMBOL_FLAG`]; a type handle is a small metadata row, so it never collides.
const DESC_SYMBOL_FLAG: u32 = 0x4000_0000;

/// A `desc_relocs` target standing for a statics-region base (`__lamella_statics_<hash>`, laid by
/// the linker in the machine's RAM window). The low bits carry which region: 0 = this assembly's
/// OWN, `ordinal + 1` = the reference at that ordinal in the build's reference list (the ARM
/// encoding). Matched by TOP BYTE, which no other target can wear: an `EXTERN_SYMBOL_FLAG` /
/// `DESC_SYMBOL_FLAG` target's own flag bit puts it far above, and a plain function index far
/// below.
const STATICS_BASE_SYMBOL_FLAG: u32 = 0x1000_0000;
/// The reference-ordinal capacity of [`STATICS_BASE_SYMBOL_FLAG`]'s payload -- matching the ARM
/// encoding's, and asserted loud at the emission site rather than silently aliasing a region.
const STATICS_MAX_REFERENCES: u8 = 16;
/// The ONE VES-global in-flight-exception word (`__lamella_eh_tag` = the entry region's word 0):
/// every assembly's throw/catch names the SAME symbol, which is what keeps a library throw
/// visible to a program catch. Exact-equality target (no payload).
const EH_TAG_SYMBOL_FLAG: u32 = 0x0800_0000;

/// One emitted descriptor SYMBOL on the object path: `(type handle, its vtable-start byte offset, its
/// total byte size, its vtable byte size)`. The symbol value is the vtable start (so it spans
/// vtable+words+itable and `--gc-sections` copies the whole descriptor + follows its relocations); a
/// reference to the descriptor resolves to the WORDS = symbol + vtable_bytes.
type DescSym = (u32, u32, u32, u32);

/// One descriptor `R_LAMELLA_REL_DESC` relocation on the object path: `(site byte offset, target, signed
/// addend)`. The target is a plain function index, an `EXTERN_SYMBOL_FLAG`-tagged extern (an inherited
/// library virtual), or a `DESC_SYMBOL_FLAG`-tagged type handle (a base_ptr chain link); the addend pins
/// the slot to its descriptor so `S + A - P` reduces to `entry - type_desc`.
type DescReloc = (u32, u32, i32);

/// Where an allocation's `lamella_gc_alloc(size [a0], &TypeDesc [a1]) -> block* [a0]` call resolves.
/// The flat image calls a FIXED absolute address (the runtime bump stub baked into the image); the
/// relocatable object calls an EXTERN symbol the linker resolves against a gc_alloc provider object.
/// `None` means no allocator is wired, so an allocation is rejected (loud).
#[derive(Clone, Copy)]
enum AllocSite {
    /// No allocator wired: an `Alloc`/`AllocArray`/`AllocArray2D` is `Unsupported`.
    None,
    /// Flat path: call the runtime allocator at this fixed absolute address.
    Address(u32),
    /// Object path: call the `lamella_gc_alloc` extern at this interned symbol index (an
    /// `R_RISCV_CALL_PLT` the linker resolves) -- like an `Inst::CallNative` target.
    Extern(u32),
}

/// Interns `name` into the module's extern-symbol table, returning its index (deduplicating a repeat).
/// The RISC-V twin of the ARM backend's helper: a `CallNative { symbol: i }` names `externs[i]`.
fn intern_extern(externs: &mut Vec<alloc::string::String>, name: &str) -> u32 {
    if let Some(i) = externs.iter().position(|s| s == name) {
        i as u32
    } else {
        externs.push(name.into());
        (externs.len() - 1) as u32
    }
}

/// Does the function contain a heap allocation (so the object path must wire `lamella_gc_alloc`, and
/// the spilled frame must save `ra` for the call)?
fn func_allocates(func: &Function) -> bool {
    func.blocks.iter().any(|b| {
        b.insts.iter().any(|(_, i)| {
            matches!(
                i,
                Inst::Alloc { .. }
                    | Inst::AllocLike { .. }
                    | Inst::AllocArray { .. }
                    | Inst::AllocArray2D { .. }
                    | Inst::AllocArrayMD { .. }
            )
        })
    })
}

/// Rewrites each `Inst::PInvoke { import, args }` to an `Inst::CallNative { symbol, args }`, interning
/// the import name into `externs` -- the object path resolves a P/Invoke through the linker exactly as
/// it resolves a `CallNative` (the ARM `lower_runtime_calls` PInvoke arm, RISC-V side). Marshalling
/// P/Invokes (`str_to_native`/`frame_end`) are ordinary imports the same rewrite interns.
fn rewrite_pinvoke(func: &Function, externs: &mut Vec<alloc::string::String>) -> Function {
    let mut func = func.clone();
    for block in &mut func.blocks {
        for (_, inst) in &mut block.insts {
            if let Inst::PInvoke { import, args } = inst {
                let symbol = intern_extern(externs, import);
                *inst = Inst::CallNative {
                    symbol,
                    args: core::mem::take(args),
                };
            }
        }
    }
    func
}

/// Emits the `lamella_gc_alloc` call (a0 = size, a1 = &TypeDesc already set by the caller; result in
/// a0). The flat path calls the fixed runtime address via a scratch register; the object path emits an
/// `R_RISCV_CALL_PLT` to the interned extern (like [`emit_call`] of a `CallNative`). No allocator wired
/// is a loud `Unsupported`.
fn emit_alloc_call(
    enc: &mut Encoder,
    alloc: AllocSite,
    func_labels: &[Label],
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
) -> Result<(), LowerError> {
    match alloc {
        AllocSite::None => Err(LowerError::Unsupported),
        AllocSite::Address(addr) => {
            enc.li(Reg::T0, addr as i32);
            enc.jalr(Reg::RA, Reg::T0, 0);
            Ok(())
        }
        AllocSite::Extern(symbol) => emit_call(
            enc,
            func_labels,
            relocs,
            relocate,
            EXTERN_SYMBOL_FLAG | symbol,
        ),
    }
}

/// Lowers a single [`Function`] to RV32IM machine code -- a one-function module.
pub fn lower(func: &Function) -> Result<Vec<u8>, LowerError> {
    lower_module(core::slice::from_ref(func))
}

/// Lowers a module of [`Function`]s to RV32IM machine code with the calling convention: each
/// function gets an entry label, a `Call` jumps to it (`jal`), arguments pass in a0-a7 and the
/// result returns in a0. Module order fixes call indices -- function 0 is the entry. Values live
/// in callee-saved registers so they survive a call; each function saves the ones it uses.
pub fn lower_module(funcs: &[Function]) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(funcs, None, &[], RiscvProfile::Rv32im)
}

/// As [`lower_module`], but for a chosen register [`RiscvProfile`] -- e.g. `Rv32ec` restricts the output
/// to the CH32V003's x0-x15 and lowers scalar i32 mul/div/rem to soft-routine calls. A self-contained
/// flat image (no GC).
pub fn lower_module_profile(
    funcs: &[Function],
    profile: RiscvProfile,
) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(funcs, None, &[], profile)
}

/// As [`lower_module`], but with the garbage-collected allocator threaded: `Alloc` lowers to a
/// `lamella_gc_alloc(payload_size [a0], &TypeDesc [a1]) -> payload* [a0]` call at `alloc_addr`, and
/// each function's emitted TypeDescs follow its code (addressed PC-relatively via `la`).
pub fn lower_module_gc(funcs: &[Function], alloc_addr: u32) -> Result<Vec<u8>, LowerError> {
    lower_module_gc_with_descriptors(funcs, alloc_addr, &[])
}


/// As [`lower_module_gc`], but also threading the per-type descriptors so a virtual type's `Alloc`
/// lays its vtable + a descriptor and writes the descriptor pointer at `obj-4`, and `CallVirtual`
/// dispatches through it. A plain type (no vtable) keeps the header-free layout.
pub fn lower_module_gc_with_descriptors(
    funcs: &[Function],
    alloc_addr: u32,
    descriptors: &[TypeMeta],
) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(funcs, Some(alloc_addr), descriptors, RiscvProfile::Rv32im)
}

fn lower_module_inner(
    funcs: &[Function],
    alloc_addr: Option<u32>,
    descriptors: &[TypeMeta],
    profile: RiscvProfile,
) -> Result<Vec<u8>, LowerError> {
    let alloc = match alloc_addr {
        Some(addr) => AllocSite::Address(addr),
        None => AllocSite::None,
    };
    lower_module_to_image(funcs, alloc, descriptors, &mut Vec::new(), profile, false, false)
        .map(|(bytes, ..)| bytes)
}

/// A lowered module: the code bytes, each function's entry offset, the call relocations as `(auipc
/// offset, callee index)` pairs, the object path's type-descriptor symbols + their
/// `R_LAMELLA_REL_DESC` relocations (both empty on the flat, non-relocatable path), and each
/// function's `.lamella_stackmaps` METHOD_SLOTS record material (`None` = a leaf or a stub --
/// never observed mid-walk, no record).
type LoweredModule = (
    Vec<u8>,
    Vec<u32>,
    Vec<(u32, u32)>,
    Vec<DescSym>,
    Vec<DescReloc>,
    Vec<Option<MethodRecordInfo>>,
);

/// One function's `.lamella_stackmaps` METHOD_SLOTS record material (the shared format in
/// [`crate::stackmaps`]): the fixed frame-hop constants its ONE prologue establishes, and its
/// liveness-free root rows. The RISC-V frame is a single `addi sp, sp, -frame` (no push list), so
/// `frame_words` is the whole frame and `ret_ra_word` is the saved-`ra` slot's word offset --
/// `ra_off/4` on the spilled path, 0 on the register path (`ra` is stored first there).
struct MethodRecordInfo {
    /// SP delta from the stopped SP to the caller's SP, in words (`frame / 4`).
    frame_words: u16,
    /// Word offset from the stopped SP of the saved return address.
    ret_ra_word: u16,
    /// Root rows, `slot word offset | kind << 14`. Empty on the register path: the routing gate
    /// ([`crate::regalloc::Liveness::any_ref_live_across_safepoint`]) proves such a frame holds no
    /// live reference at any PC a walk can observe, so its record only carries the hop past it.
    roots: Vec<u16>,
}

/// One emitted type descriptor for an allocated reference type. The vtable is laid BEFORE the
/// descriptor (slot k at `desc - 4 - k*4`) as `func - desc` diffs; the `words` are the fixed header
/// `[payload, nrefs, tag, base_ptr]` plus ref_offsets; the itable is laid AFTER as `[count, (tag,
/// method diff)...]`. So `CallVirtual` reads a vtable slot, `CallInterface` searches the itable, and
/// the fixed `nrefs@4` lets dispatch find the itable at `desc + 16 + nrefs*4`. A vtable/itable method
/// is `Some(module function index)` or `None` (an unresolvable inherited implementation).
struct DescEmit {
    label: Label,
    vtable: Vec<Option<u32>>,
    words: Vec<u32>,
    itable: Vec<(u32, Option<u32>)>,
    /// The immediate in-program base type's handle (`None` for an array/element descriptor or a type
    /// whose base leaves this image). Lays the base_ptr@12 word as a `func`-style diff to the base's
    /// descriptor -- the relative step a `CastClassScan` adds to walk the base chain.
    base: Option<TypeHandle>,
}

/// The fixed descriptor header word count (`[payload, nrefs, tag, base_ptr]`); ref_offsets and then
/// the itable follow. `nrefs` sits at word 1 so dispatch computes the itable offset from it.
const DESC_HEADER_WORDS: u32 = 4;

/// A function's emitted type descriptors, one per allocated reference type.
type TypeDescs = Vec<DescEmit>;

/// Lowers a module and also reports each function's byte offset in the image (its entry point) --
/// the basis for the symbol table when emitting a relocatable object.
fn lower_module_to_image(
    funcs: &[Function],
    alloc: AllocSite,
    descriptors: &[TypeMeta],
    externs: &mut Vec<alloc::string::String>,
    profile: RiscvProfile,
    relocate: bool,
    tolerant: bool,
) -> Result<LoweredModule, LowerError> {
    let mut program = funcs.to_vec();
    if !relocate {
        crate::stringgen::lower_string_equals(&mut program);
        crate::stringgen::lower_string_concat(&mut program);
        crate::stringgen::lower_int_to_string(&mut program);
    }
    let funcs: &[Function] = &program;
    if !tolerant {
        for func in funcs {
            if lamella_ir::verify(func).is_err() {
                return Err(LowerError::NotWellFormed);
            }
        }
    }
    let mut enc = Encoder::new();
    let func_labels: Vec<Label> = (0..funcs.len()).map(|_| enc.new_label()).collect();
    let mut offsets: Vec<u32> = Vec::with_capacity(funcs.len());
    let mut call_relocs: Vec<(u32, u32)> = Vec::new();
    let mut desc_relocs: Vec<DescReloc> = Vec::new();
    let mut type_descs: TypeDescs = Vec::new();
    let mut type_desc_labels: Vec<(TypeHandle, Label)> = Vec::new();
    let mut method_records: Vec<Option<MethodRecordInfo>> = Vec::with_capacity(funcs.len());
    for (index, func) in funcs.iter().enumerate() {
        enc.bind_label(func_labels[index]);
        offsets.push(enc.position());
        if tolerant {
            let mut scratch = Encoder::new();
            let scratch_labels: Vec<Label> =
                (0..funcs.len()).map(|_| scratch.new_label()).collect();
            scratch.bind_label(scratch_labels[index]);
            let mut s_descs: TypeDescs = Vec::new();
            let mut s_desc_labels: Vec<(TypeHandle, Label)> = Vec::new();
            let mut s_call_relocs: Vec<(u32, u32)> = Vec::new();
            let mut s_desc_relocs: Vec<DescReloc> = Vec::new();
            let mut s_externs = externs.clone();
            let lowered = lamella_ir::verify(func).is_ok()
                && lower_function(
                    &mut scratch,
                    func,
                    &scratch_labels,
                    alloc,
                    descriptors,
                    &mut s_descs,
                    &mut s_desc_labels,
                    &mut s_externs,
                    profile,
                    &mut s_call_relocs,
                    &mut s_desc_relocs,
                    relocate,
                )
                .is_ok();
            if lowered {
                method_records.push(
                    lower_function(
                        &mut enc,
                        func,
                        &func_labels,
                        alloc,
                        descriptors,
                        &mut type_descs,
                        &mut type_desc_labels,
                        externs,
                        profile,
                        &mut call_relocs,
                        &mut desc_relocs,
                        relocate,
                    )
                    .expect("a method that lowered in the dry run lowers for real"),
                );
            } else {
                enc.ret();
                method_records.push(None);
            }
        } else {
            method_records.push(lower_function(
                &mut enc,
                func,
                &func_labels,
                alloc,
                descriptors,
                &mut type_descs,
                &mut type_desc_labels,
                externs,
                profile,
                &mut call_relocs,
                &mut desc_relocs,
                relocate,
            )?);
        }
    }
    if tolerant {
        for meta in descriptors {
            let Some(words) = meta.words.as_deref() else {
                continue;
            };
            let vtable = descriptor_vtable(descriptors, meta.handle, externs);
            let itable = descriptor_itable(descriptors, meta.handle, externs);
            let words = words.to_vec();
            match type_desc_labels.iter().position(|(h, _)| *h == meta.handle) {
                Some(idx) => {
                    type_descs[idx] = DescEmit {
                        label: type_desc_labels[idx].1,
                        vtable,
                        words,
                        itable,
                        base: meta.base,
                    };
                }
                None => {
                    let label = enc.new_label();
                    type_descs.push(DescEmit {
                        label,
                        vtable,
                        words,
                        itable,
                        base: meta.base,
                    });
                    type_desc_labels.push((meta.handle, label));
                }
            }
        }
    }
    let desc_syms = emit_descriptors(
        &mut enc,
        &type_descs,
        &type_desc_labels,
        &func_labels,
        descriptors,
        relocate,
        &mut desc_relocs,
    );
    let bytes = enc
        .finish()
        .map(|assembled| assembled.bytes)
        .map_err(|_| LowerError::CodeTooLarge)?;
    Ok((bytes, offsets, call_relocs, desc_syms, desc_relocs, method_records))
}

/// Lowers a module into an ELF32 relocatable object: each function becomes a global `STT_FUNC`
/// symbol (named by `names[i]`) at its entry offset, and every call becomes an `R_RISCV_CALL_PLT`
/// relocation to the callee's symbol -- so a linker (ours or another) resolves them and can see the
/// call graph for `--gc-sections`. `names` must have one entry per function in `funcs`.
///
/// The object path resolves the runtime seams through the linker: a `PInvoke` is rewritten to a
/// `CallNative` of its import name, a heap allocation calls the `lamella_gc_alloc` extern, and each such
/// name (plus any the caller passes in `externs`, whose indices lead so a hand-built `CallNative` still
/// names them) becomes an UNDEFINED symbol. `externs` may be `&[]`; the pass discovers the rest.
///
/// `descriptors` are the per-type vtables/itables (the resolver's `type_descriptors()`). Each type gets
/// ONE canonical `__lamella_typedesc_<handle>` symbol laid after the function code, its vtable/itable/
/// base_ptr slots emitted as `R_LAMELLA_REL_DESC` relocations the linker resolves -- so a slot can point
/// ACROSS the link at an inherited library virtual, and `--gc-sections` keeps/drops each descriptor by
/// reachability. An `Alloc`/`TypeDescAddr` reaches its descriptor through a per-function REL_DESC pool
/// word (`emit_desc_words_addr`), not a bare `la`, so the collector follows the Alloc -> descriptor edge.
/// Thus a dispatched/cast type's `Alloc` writes its `obj-4` descriptor pointer and `CallVirtual`/
/// `CallInterface`/`castclass` work over the link AND survive a gc re-layout. Pass `&[]` for no dispatch.
pub fn lower_object(
    funcs: &[Function],
    names: &[&str],
    externs: &[&str],
    descriptors: &[TypeMeta],
) -> Result<Vec<u8>, LowerError> {
    lower_object_profile(funcs, names, externs, descriptors, RiscvProfile::Rv32im)
}

/// As [`lower_object`], but for a chosen register [`RiscvProfile`]. `Rv32ec` restricts every function to
/// the CH32V003's x0-x15 (empty allocatable pool -> all-spilled path, `s1` scratch, a0-a5 arguments) and
/// lowers scalar i32 mul/div/rem to soft-routine calls -- the CH32V003 register and instruction model.
pub fn lower_object_profile(
    funcs: &[Function],
    names: &[&str],
    externs: &[&str],
    descriptors: &[TypeMeta],
    profile: RiscvProfile,
) -> Result<Vec<u8>, LowerError> {
    lower_object_relocatable(
        funcs,
        names,
        externs,
        descriptors,
        None,
        &[],
        &DescQualifiers::default(),
        profile,
        false,
    )
}

/// As [`lower_object_profile`], with the assembly's statics record ([`AssemblyStatics`]: region
/// identity + size + GLOBAL-roots rows) attached -- `ldsfld`/`stsfld` then address the
/// linker-placed `__lamella_statics_<hash>` region (and the shared `__lamella_eh_tag` word 0)
/// instead of the flat path's baked base, so trims and multi-object links stay sound, and the
/// region's ref-typed rows ride a mode-2 `__lamella_smstat_<hash>` stack-map record the root
/// walker enumerates. A statics-bearing assembly built WITHOUT a record fails loud at the
/// relocation step.
pub fn lower_object_profile_statics(
    funcs: &[Function],
    names: &[&str],
    externs: &[&str],
    descriptors: &[TypeMeta],
    statics: Option<&AssemblyStatics>,
    profile: RiscvProfile,
) -> Result<Vec<u8>, LowerError> {
    lower_object_relocatable(
        funcs,
        names,
        externs,
        descriptors,
        statics,
        &[],
        &DescQualifiers::default(),
        profile,
        false,
    )
}

/// As [`lower_object_profile_statics`], with the ORDERED reference-assembly statics regions
/// attached: `reference_statics[ordinal]` is that reference's `__lamella_statics_<ownerhash>`
/// symbol -- the same identity the owner's own object defines, so a cross-assembly `ldsfld`
/// (`StaticOwner::Reference`) and the owner's own access land on ONE linker-placed region.
/// The ordinals match the reference list the resolver built the MIR against, and `qualifiers`
/// carries the SAME ordinal order for descriptor-symbol identity ([`DescQualifiers`]): a
/// reference-owned type's descriptor names its OWNER's hash + token, matching the owning
/// library's own emission.
#[allow(clippy::too_many_arguments)]
pub fn lower_object_profile_statics_references(
    funcs: &[Function],
    names: &[&str],
    externs: &[&str],
    descriptors: &[TypeMeta],
    statics: Option<&AssemblyStatics>,
    reference_statics: &[&str],
    qualifiers: &DescQualifiers,
    profile: RiscvProfile,
) -> Result<Vec<u8>, LowerError> {
    lower_object_relocatable(
        funcs,
        names,
        externs,
        descriptors,
        statics,
        reference_statics,
        qualifiers,
        profile,
        false,
    )
}

/// The RISC-V twin of arm32 [`crate::arm32::lower_object_library`]: lowers a LIBRARY object (corlib) with
/// per-method dry-run TOLERANCE -- a method that fails to lower is stubbed to a bare `ret` and the build
/// continues, so one un-lowerable corlib method does not sink the whole library. `--gc-sections` drops an
/// unreached stub; a program that actually calls one surfaces the gap loudly (its own build stays fatal).
pub fn lower_object_library(
    funcs: &[Function],
    names: &[&str],
    externs: &[&str],
    descriptors: &[TypeMeta],
) -> Result<Vec<u8>, LowerError> {
    lower_object_relocatable(
        funcs,
        names,
        externs,
        descriptors,
        None,
        &[],
        &DescQualifiers::default(),
        RiscvProfile::Rv32im,
        true,
    )
}

/// As [`lower_object_library`], with the library's statics region attached (see
/// [`lower_object_profile_statics`]): the library's own `ldsfld`/`stsfld` resolve against its own
/// `__lamella_statics_<hash>` region rather than the flat base a program's rows would collide
/// with. `qualifiers.own` carries the library's hash, so its own descriptors take the qualified
/// `__lamella_typedesc_<ownhash>_<token>` names a referencing program's emission matches.
pub fn lower_object_library_statics(
    funcs: &[Function],
    names: &[&str],
    externs: &[&str],
    descriptors: &[TypeMeta],
    statics: Option<&AssemblyStatics>,
    reference_statics: &[&str],
    qualifiers: &DescQualifiers,
) -> Result<Vec<u8>, LowerError> {
    lower_object_relocatable(
        funcs,
        names,
        externs,
        descriptors,
        statics,
        reference_statics,
        qualifiers,
        RiscvProfile::Rv32im,
        true,
    )
}

/// The shared relocatable-object lowering for [`lower_object_profile`] (a program, `tolerant == false` --
/// every method must lower) and [`lower_object_library`] (a library, `tolerant == true` -- an un-lowerable
/// method is stubbed). `tolerant` threads to [`lower_module_to_image`]'s per-method dry-run.
#[allow(clippy::too_many_arguments)]
fn lower_object_relocatable(
    funcs: &[Function],
    names: &[&str],
    externs: &[&str],
    descriptors: &[TypeMeta],
    statics: Option<&AssemblyStatics>,
    reference_statics: &[&str],
    qualifiers: &DescQualifiers,
    profile: RiscvProfile,
    tolerant: bool,
) -> Result<Vec<u8>, LowerError> {
    let mut extern_names: Vec<alloc::string::String> =
        externs.iter().map(|s| (*s).into()).collect();
    let program: Vec<Function> = funcs
        .iter()
        .map(|f| rewrite_pinvoke(f, &mut extern_names))
        .collect();
    let alloc = if program.iter().any(func_allocates) {
        AllocSite::Extern(intern_extern(&mut extern_names, "lamella_gc_alloc"))
    } else {
        AllocSite::None
    };
    let (mut text, offsets, call_relocs, desc_syms, desc_relocs, method_records) =
        lower_module_to_image(
            &program,
            alloc,
            descriptors,
            &mut extern_names,
            profile,
            true,
            tolerant,
        )?;
    let method_records: Vec<(usize, MethodRecordInfo)> = method_records
        .into_iter()
        .enumerate()
        .filter_map(|(i, record)| record.map(|record| (i, record)))
        .collect();
    let smrec_names: Vec<alloc::string::String> = method_records
        .iter()
        .map(|&(i, _)| alloc::format!("{}{}", lamella_elf::STACKMAP_RECORD_PREFIX, names[i]))
        .collect();
    let region_name = statics.map(AssemblyStatics::region_symbol);
    let smstat_name = statics.map(AssemblyStatics::record_symbol);
    let mut symbols: Vec<lamella_elf::Symbol> = (0..program.len())
        .map(|i| {
            let end = offsets.get(i + 1).copied().unwrap_or(text.len() as u32);
            lamella_elf::Symbol {
                name: names[i],
                value: offsets[i],
                size: end - offsets[i],
                binding: lamella_elf::Binding::Global,
                kind: lamella_elf::SymbolType::Func,
                section: lamella_elf::SymbolSection::Text,
            }
        })
        .collect();
    for name in &extern_names {
        symbols.push(lamella_elf::Symbol {
            name: name.as_str(),
            value: 0,
            size: 0,
            binding: lamella_elf::Binding::Global,
            kind: lamella_elf::SymbolType::NoType,
            section: lamella_elf::SymbolSection::Undefined,
        });
    }
    let desc_names: Vec<alloc::string::String> = desc_syms
        .iter()
        .map(|(handle, ..)| descriptor_symbol(*handle, qualifiers))
        .collect();
    let mut desc_index: Vec<(u32, u32, u32)> = Vec::with_capacity(desc_syms.len());
    for (i, &(handle, vtable_start, total_size, vtable_bytes)) in desc_syms.iter().enumerate() {
        desc_index.push((handle, symbols.len() as u32, vtable_bytes));
        symbols.push(lamella_elf::Symbol {
            name: desc_names[i].as_str(),
            value: vtable_start,
            size: total_size,
            binding: if tolerant {
                lamella_elf::Binding::Weak
            } else {
                lamella_elf::Binding::Global
            },
            kind: lamella_elf::SymbolType::NoType,
            section: lamella_elf::SymbolSection::Text,
        });
    }
    let mut undef_desc_handles: Vec<u32> = desc_relocs
        .iter()
        .filter(|&&(_, target, _)| target & DESC_SYMBOL_FLAG != 0)
        .map(|&(_, target, _)| target & !DESC_SYMBOL_FLAG)
        .filter(|handle| !desc_index.iter().any(|(h, _, _)| h == handle))
        .collect();
    undef_desc_handles.sort_unstable();
    undef_desc_handles.dedup();
    let undef_desc_names: Vec<alloc::string::String> = undef_desc_handles
        .iter()
        .map(|&handle| descriptor_symbol(handle, qualifiers))
        .collect();
    let undef_desc_index: alloc::collections::BTreeMap<u32, u32> = undef_desc_handles
        .iter()
        .zip(&undef_desc_names)
        .map(|(&handle, name)| {
            let index = symbols.len() as u32;
            symbols.push(lamella_elf::Symbol {
                name: name.as_str(),
                value: 0,
                size: 0,
                binding: lamella_elf::Binding::Global,
                kind: lamella_elf::SymbolType::NoType,
                section: lamella_elf::SymbolSection::Undefined,
            });
            (handle, index)
        })
        .collect();
    let own_region_referenced = desc_relocs
        .iter()
        .any(|&(_, target, _)| target == STATICS_BASE_SYMBOL_FLAG);
    let emit_statics_record = statics
        .is_some_and(|s| own_region_referenced || s.roots.iter().any(|&row| row & 0x3FFF != 0));
    let statics_index = statics.filter(|_| emit_statics_record).map(|s| {
        let index = symbols.len() as u32;
        symbols.push(lamella_elf::Symbol {
            name: region_name
                .as_deref()
                .expect("a statics record derives its region symbol"),
            value: 0,
            size: s.region_bytes,
            binding: lamella_elf::Binding::Global,
            kind: lamella_elf::SymbolType::NoType,
            section: lamella_elf::SymbolSection::Undefined,
        });
        index
    });
    let mut ref_statics_index: alloc::collections::BTreeMap<u32, u32> =
        alloc::collections::BTreeMap::new();
    for &(_, target, _) in &desc_relocs {
        if target >> 24 != STATICS_BASE_SYMBOL_FLAG >> 24 {
            continue;
        }
        let payload = target & 0x00ff_ffff;
        if payload == 0 {
            continue;
        }
        let ordinal = payload - 1;
        if ref_statics_index.contains_key(&ordinal) {
            continue;
        }
        let name = reference_statics
            .get(ordinal as usize)
            .unwrap_or_else(|| panic!("reference ordinal {ordinal} has no statics region name"));
        let index = symbols.len() as u32;
        symbols.push(lamella_elf::Symbol {
            name,
            value: 0,
            size: 0,
            binding: lamella_elf::Binding::Global,
            kind: lamella_elf::SymbolType::NoType,
            section: lamella_elf::SymbolSection::Undefined,
        });
        ref_statics_index.insert(ordinal, index);
    }
    let eh_tag_index = desc_relocs
        .iter()
        .any(|&(_, target, _)| target == EH_TAG_SYMBOL_FLAG)
        .then(|| {
            let index = symbols.len() as u32;
            symbols.push(lamella_elf::Symbol {
                name: lamella_elf::EH_TAG_SYMBOL,
                value: 0,
                size: 0,
                binding: lamella_elf::Binding::Global,
                kind: lamella_elf::SymbolType::NoType,
                section: lamella_elf::SymbolSection::Undefined,
            });
            index
        });
    let mut relocations: Vec<lamella_elf::Relocation> = call_relocs
        .iter()
        .map(|&(offset, callee)| lamella_elf::Relocation {
            offset,
            symbol: if callee & EXTERN_SYMBOL_FLAG != 0 {
                program.len() as u32 + (callee & !EXTERN_SYMBOL_FLAG)
            } else {
                callee
            },
            kind: lamella_elf::riscv::R_RISCV_CALL_PLT,
            addend: 0,
        })
        .collect();
    for &(offset, target, addend) in &desc_relocs {
        let (symbol, final_addend) = if target & DESC_SYMBOL_FLAG != 0 {
            let handle = target & !DESC_SYMBOL_FLAG;
            match desc_index.iter().copied().find(|(h, _, _)| *h == handle) {
                Some((_, symbol_index, vtable_bytes)) => {
                    (symbol_index, addend + vtable_bytes as i32)
                }
                None => (undef_desc_index[&handle], addend),
            }
        } else if target & EXTERN_SYMBOL_FLAG != 0 {
            (
                program.len() as u32 + (target & !EXTERN_SYMBOL_FLAG),
                addend,
            )
        } else if target == EH_TAG_SYMBOL_FLAG {
            (
                eh_tag_index.expect("an EH-tag relocation appended its symbol above"),
                addend,
            )
        } else if target >> 24 == STATICS_BASE_SYMBOL_FLAG >> 24 {
            match target & 0x00ff_ffff {
                0 => (
                    statics_index
                        .expect("a statics-flagged relocation requires a threaded statics region"),
                    addend,
                ),
                payload => (ref_statics_index[&(payload - 1)], addend),
            }
        } else {
            (target, addend)
        };
        relocations.push(lamella_elf::Relocation {
            offset,
            symbol,
            kind: lamella_elf::riscv::R_LAMELLA_REL_DESC,
            addend: final_addend,
        });
    }
    let code_end = text.len() as u32;
    for (record_index, (i, record)) in method_records.iter().enumerate() {
        while text.len() % 4 != 0 {
            text.push(0);
        }
        let rec_offset = text.len() as u32;
        let end = offsets.get(i + 1).copied().unwrap_or(code_end);
        encode_stackmap_record(
            &mut text,
            0,
            end - offsets[*i],
            STACKMAP_MODE_METHOD_SLOTS,
            record.frame_words,
            record.ret_ra_word,
            &record.roots,
        );
        symbols.push(lamella_elf::Symbol {
            name: smrec_names[record_index].as_str(),
            value: rec_offset,
            size: text.len() as u32 - rec_offset,
            binding: lamella_elf::Binding::Weak,
            kind: lamella_elf::SymbolType::NoType,
            section: lamella_elf::SymbolSection::Text,
        });
        relocations.push(lamella_elf::Relocation {
            offset: rec_offset,
            symbol: *i as u32,
            kind: lamella_elf::riscv::R_RISCV_32,
            addend: 0,
        });
    }
    if emit_statics_record {
        let statics = statics.expect("the record decision proved a statics record exists");
        while text.len() % 4 != 0 {
            text.push(0);
        }
        let rec_offset = text.len() as u32;
        encode_stackmap_record(
            &mut text,
            0,
            statics.region_bytes,
            STACKMAP_MODE_STATICS,
            0,
            0,
            &statics.roots,
        );
        symbols.push(lamella_elf::Symbol {
            name: smstat_name
                .as_deref()
                .expect("a statics record derives its symbol name"),
            value: rec_offset,
            size: text.len() as u32 - rec_offset,
            binding: lamella_elf::Binding::Weak,
            kind: lamella_elf::SymbolType::NoType,
            section: lamella_elf::SymbolSection::Text,
        });
        relocations.push(lamella_elf::Relocation {
            offset: rec_offset,
            symbol: statics_index.expect("a statics record appends its region symbol"),
            kind: lamella_elf::riscv::R_RISCV_32,
            addend: 0,
        });
    }
    Ok(lamella_elf::write_relocatable_object(
        lamella_elf::Machine::RiscV,
        &text,
        &symbols,
        &relocations,
    ))
}

/// Lowers one function into `enc`: a prologue that allocates a frame and saves the callee-saved
/// registers it uses (plus `ra` if it calls), the incoming arguments moved from a0-a7 into the
/// entry block's parameters, the block bodies, and -- at each return -- a value moved to a0 then
/// the saved registers restored and `ret`. Returns the function's `.lamella_stackmaps`
/// METHOD_SLOTS record material (`None` for a leaf -- it can never appear mid-walk).
#[allow(clippy::too_many_arguments)]
fn lower_function(
    enc: &mut Encoder,
    func: &Function,
    func_labels: &[Label],
    alloc: AllocSite,
    descriptors: &[TypeMeta],
    type_descs: &mut TypeDescs,
    type_desc_labels: &mut Vec<(TypeHandle, Label)>,
    externs: &mut Vec<alloc::string::String>,
    profile: RiscvProfile,
    relocs: &mut Vec<(u32, u32)>,
    desc_relocs: &mut Vec<DescReloc>,
    relocate: bool,
) -> Result<Option<MethodRecordInfo>, LowerError> {
    let pool = allocatable(profile);
    let value_count = func.value_types.len();
    let allocates = func_allocates(func);
    let has_value_types = func.value_types.iter().any(|t| {
        matches!(
            t,
            MirType::ValueType { .. } | MirType::I64 | MirType::F32 | MirType::F64
        )
    });
    let has_dispatch = func.blocks.iter().any(|b| {
        b.insts.iter().any(|(_, i)| {
            matches!(
                i,
                Inst::FuncAddr { .. }
                    | Inst::CallIndirect { .. }
                    | Inst::InvokeDelegate { .. }
                    | Inst::CallVirtual { .. }
                    | Inst::CallInterface { .. }
                    | Inst::VirtualFuncAddr { .. }
                    | Inst::LoadTypeDesc { .. }
                    | Inst::TypeDescAddr { .. }
                    | Inst::CastClassScan { .. }
                    | Inst::CallNative { .. }
                    | Inst::AllocArrayMD { .. }
                    | Inst::ArrayMDLoad { .. }
                    | Inst::ArrayMDStore { .. }
            )
        })
    });
    let has_string_literal = func.blocks.iter().any(|b| {
        b.insts
            .iter()
            .any(|(_, i)| matches!(i, Inst::StringLiteral { .. }))
    });
    let has_stack_call = func.blocks.iter().any(|b| {
        b.insts
            .iter()
            .any(|(r, i)| call_stack_words(i, *r, &func.value_types, profile) > 0)
    });
    let has_statics = relocate
        && func.blocks.iter().any(|b| {
            b.insts.iter().any(|(_, i)| {
                matches!(i, Inst::StaticLoad { .. } | Inst::StaticStore { .. })
            })
        });
    let has_homeless_ref = relocate
        && func
            .blocks
            .iter()
            .any(|b| b.insts.iter().any(|(_, i)| crate::regalloc::is_safepoint(i)))
        && crate::regalloc::Liveness::analyze(func).any_ref_live_across_safepoint(func);
    if value_count > pool.len()
        || allocates
        || has_value_types
        || has_dispatch
        || has_string_literal
        || has_stack_call
        || has_statics
        || has_homeless_ref
    {
        return lower_function_spilled(
            enc,
            func,
            func_labels,
            alloc,
            descriptors,
            type_descs,
            type_desc_labels,
            externs,
            profile,
            relocs,
            desc_relocs,
            relocate,
        );
    }
    let reg = |v: ValueId| pool[v.index()];
    let used = &pool[..value_count];
    let has_calls = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| matches!(i, Inst::Call { .. })));
    let saved = value_count + has_calls as usize;
    let frame = (saved * 4).div_ceil(16) * 16;

    if frame > 0 {
        enc.addi(Reg::SP, Reg::SP, -(frame as i32));
    }
    let mut offset = 0i32;
    if has_calls {
        enc.sw(Reg::RA, Reg::SP, offset);
        offset += 4;
    }
    for &r in used {
        enc.sw(r, Reg::SP, offset);
        offset += 4;
    }
    let entry = &func.blocks[func.entry.index()];
    for (i, &param) in entry.params.iter().enumerate() {
        let arg = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
        if reg(param) != arg {
            enc.mv(reg(param), arg);
        }
    }

    let block_labels: Vec<Label> = (0..func.blocks.len()).map(|_| enc.new_label()).collect();
    if func.entry != lamella_ir::BlockId(0) {
        enc.j(block_labels[func.entry.index()]);
    }

    for (index, block) in func.blocks.iter().enumerate() {
        enc.bind_label(block_labels[index]);

        for (result, inst) in &block.insts {
            lower_inst(
                enc,
                &reg,
                &func.value_types,
                func_labels,
                *result,
                inst,
                relocs,
                relocate,
            )?;
        }

        match &block.terminator {
            Some(Terminator::Return(value)) => {
                if let Some(v) = value {
                    enc.mv(Reg::A0, reg(*v));
                }
                let mut offset = 0i32;
                if has_calls {
                    enc.lw(Reg::RA, Reg::SP, offset);
                    offset += 4;
                }
                for &r in used {
                    enc.lw(r, Reg::SP, offset);
                    offset += 4;
                }
                if frame > 0 {
                    enc.addi(Reg::SP, Reg::SP, frame as i32);
                }
                enc.ret();
            }
            Some(Terminator::Jump { target, args }) => {
                let params = &func
                    .block(*target)
                    .ok_or(LowerError::ControlFlowUnsupported)?
                    .params;
                if args.len() != params.len() {
                    return Err(LowerError::ControlFlowUnsupported);
                }
                for (p, a) in params.iter().zip(args) {
                    if reg(*p) != reg(*a) {
                        enc.mv(reg(*p), reg(*a));
                    }
                }
                enc.j(block_labels[target.index()]);
            }
            Some(Terminator::Branch {
                cond,
                if_true,
                true_args,
                if_false,
                false_args,
            }) => {
                if !true_args.is_empty() || !false_args.is_empty() {
                    return Err(LowerError::ControlFlowUnsupported);
                }
                let true_label = block_labels[if_true.index()];
                let false_label = block_labels[if_false.index()];
                enc.branch(BranchCond::Ne, reg(*cond), Reg::ZERO, true_label);
                enc.j(false_label);
            }
            Some(Terminator::Unreachable) => enc.ebreak(),
            None => return Err(LowerError::ControlFlowUnsupported),
        }
    }
    Ok(has_calls.then(|| MethodRecordInfo {
        frame_words: (frame / 4) as u16,
        ret_ra_word: 0,
        roots: Vec::new(),
    }))
}

/// Emits a call to function `callee`: in object mode (`relocate`) an `auipc`+`jalr` pair whose
/// target is left for a `R_RISCV_CALL_PLT` relocation (the site recorded in `relocs`); otherwise a
/// resolved `jal` to the callee's intra-module label.
fn emit_call(
    enc: &mut Encoder,
    func_labels: &[Label],
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
    callee: u32,
) -> Result<(), LowerError> {
    if relocate {
        relocs.push((enc.position(), callee));
        enc.auipc(Reg::RA, 0);
        enc.jalr(Reg::RA, Reg::RA, 0);
    } else {
        let label = *func_labels
            .get(callee as usize)
            .ok_or(LowerError::ControlFlowUnsupported)?;
        enc.jal(Reg::RA, label);
    }
    Ok(())
}

/// Lowers one value-defining instruction into its assigned register.
#[allow(clippy::too_many_arguments)]
fn lower_inst(
    enc: &mut Encoder,
    reg: &impl Fn(ValueId) -> Reg,
    value_types: &[MirType],
    func_labels: &[Label],
    result: ValueId,
    inst: &Inst,
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
) -> Result<(), LowerError> {
    match inst {
        Inst::ConstInt { value, .. } => enc.li(reg(result), *value as i32),
        Inst::Call { callee, args } => {
            for (i, &arg) in args.iter().enumerate() {
                let target = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                if reg(arg) != target {
                    enc.mv(target, reg(arg));
                }
            }
            emit_call(enc, func_labels, relocs, relocate, *callee)?;
            if reg(result) != Reg::A0 {
                enc.mv(reg(result), Reg::A0);
            }
        }
        Inst::Load {
            address,
            width,
            signed,
        } => match (*width, *signed) {
            (1, true) => enc.lb(reg(result), reg(*address), 0),
            (1, false) => enc.lbu(reg(result), reg(*address), 0),
            (2, true) => enc.lh(reg(result), reg(*address), 0),
            (2, false) => enc.lhu(reg(result), reg(*address), 0),
            _ => enc.lw(reg(result), reg(*address), 0),
        },
        Inst::Store {
            address,
            value,
            width,
        } => match *width {
            1 => enc.sb(reg(*value), reg(*address), 0),
            2 => enc.sh(reg(*value), reg(*address), 0),
            _ => enc.sw(reg(*value), reg(*address), 0),
        },
        Inst::FieldLoad { base, offset } => {
            if !is_pointer(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            enc.lw(reg(result), reg(*base), field_offset(*offset)?);
        }
        Inst::FieldStore {
            base,
            offset,
            value,
        } => {
            if !is_pointer(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            enc.sw(reg(*value), reg(*base), field_offset(*offset)?);
        }
        Inst::FieldLoadNarrow {
            base,
            offset,
            size,
            signed,
        } => {
            if !is_pointer(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            let off = field_offset(*offset)?;
            match (*size, *signed) {
                (1, false) => enc.lbu(reg(result), reg(*base), off),
                (1, true) => enc.lb(reg(result), reg(*base), off),
                (2, false) => enc.lhu(reg(result), reg(*base), off),
                (2, true) => enc.lh(reg(result), reg(*base), off),
                _ => return Err(LowerError::Unsupported),
            }
        }
        Inst::FieldStoreNarrow {
            base,
            offset,
            value,
            size,
        } => {
            if !is_pointer(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            let off = field_offset(*offset)?;
            match *size {
                1 => enc.sb(reg(*value), reg(*base), off),
                2 => enc.sh(reg(*value), reg(*base), off),
                _ => return Err(LowerError::Unsupported),
            }
        }
        Inst::FieldAddr { base, offset } => {
            if !is_pointer(value_types, *base) {
                return Err(LowerError::Unsupported);
            }
            enc.addi(reg(result), reg(*base), field_offset(*offset)?);
        }
        Inst::ArrayLoad {
            array,
            index,
            element_size,
            signed,
        } => {
            if !matches!(*element_size, 1 | 2 | 4) {
                return Err(LowerError::Unsupported);
            }
            emit_element_address(
                enc,
                reg(*array),
                reg(*index),
                *element_size,
                RiscvProfile::Rv32im,
            );
            match (*element_size, *signed) {
                (1, true) => enc.lb(reg(result), scratch(), 4),
                (1, false) => enc.lbu(reg(result), scratch(), 4),
                (2, true) => enc.lh(reg(result), scratch(), 4),
                (2, false) => enc.lhu(reg(result), scratch(), 4),
                _ => enc.lw(reg(result), scratch(), 4),
            }
        }
        Inst::ArrayStore {
            array,
            index,
            value,
            element_size,
        } => {
            if !matches!(*element_size, 1 | 2 | 4) {
                return Err(LowerError::Unsupported);
            }
            emit_element_address(
                enc,
                reg(*array),
                reg(*index),
                *element_size,
                RiscvProfile::Rv32im,
            );
            match *element_size {
                1 => enc.sb(reg(*value), scratch(), 4),
                2 => enc.sh(reg(*value), scratch(), 4),
                _ => enc.sw(reg(*value), scratch(), 4),
            }
        }
        Inst::ArrayElemAddr {
            array,
            index,
            element_size,
        } => {
            emit_element_address(
                enc,
                reg(*array),
                reg(*index),
                *element_size,
                RiscvProfile::Rv32im,
            );
            enc.addi(reg(result), scratch(), 4);
        }
        Inst::Binary { op, lhs, rhs } => {
            let (d, a, b) = (reg(result), reg(*lhs), reg(*rhs));
            match op {
                BinOp::Add => enc.add(d, a, b),
                BinOp::Sub => enc.sub(d, a, b),
                BinOp::And => enc.and(d, a, b),
                BinOp::Or => enc.or(d, a, b),
                BinOp::Xor => enc.xor(d, a, b),
                BinOp::Mul => enc.mul(d, a, b),
                BinOp::DivSigned => enc.div(d, a, b),
                BinOp::DivUnsigned => enc.divu(d, a, b),
                BinOp::RemSigned => enc.rem(d, a, b),
                BinOp::RemUnsigned => enc.remu(d, a, b),
                BinOp::Shl => enc.sll(d, a, b),
                BinOp::ShrSigned => enc.sra(d, a, b),
                BinOp::ShrUnsigned => enc.srl(d, a, b),
            }
        }
        Inst::Compare { op, lhs, rhs } => {
            materialize_compare(enc, reg(result), reg(*lhs), reg(*rhs), *op);
        }
        Inst::Convert { value, kind } => emit_convert(enc, reg(result), reg(*value), *kind)?,
        Inst::CopyBlock { dst, src, size } => {
            enc.mv(Reg::T0, reg(*dst));
            enc.mv(Reg::T1, reg(*src));
            enc.mv(Reg::T2, reg(*size));
            emit_copy_block(enc, Reg::T0, Reg::T1, Reg::T2, RiscvProfile::Rv32im);
        }
        Inst::FillBlock { dst, value, size } => {
            enc.mv(Reg::T0, reg(*dst));
            enc.mv(Reg::T1, reg(*value));
            enc.mv(Reg::T2, reg(*size));
            emit_fill_block(enc, Reg::T0, Reg::T1, Reg::T2);
        }
        Inst::StaticLoad { owner, offset } => {
            if !matches!(owner, StaticOwner::Own) {
                return Err(LowerError::Unsupported);
            }
            enc.li(reg(result), (STATIC_FIELD_BASE + *offset) as i32);
            enc.lw(reg(result), reg(result), 0);
        }
        Inst::StaticStore {
            owner,
            offset,
            value,
        } => {
            if !matches!(owner, StaticOwner::Own) {
                return Err(LowerError::Unsupported);
            }
            enc.li(scratch(), (STATIC_FIELD_BASE + *offset) as i32);
            enc.sw(reg(*value), scratch(), 0);
        }
        Inst::Array2DLoad {
            array,
            index0,
            index1,
            element_size,
            signed,
        } => {
            if !matches!(*element_size, 1 | 2 | 4) {
                return Err(LowerError::Unsupported);
            }
            emit_2d_element_address(
                enc,
                reg(*array),
                reg(*index0),
                reg(*index1),
                *element_size,
                Reg::T0,
                Reg::T1,
                RiscvProfile::Rv32im,
            );
            match (*element_size, *signed) {
                (1, true) => enc.lb(reg(result), scratch(), 0),
                (1, false) => enc.lbu(reg(result), scratch(), 0),
                (2, true) => enc.lh(reg(result), scratch(), 0),
                (2, false) => enc.lhu(reg(result), scratch(), 0),
                _ => enc.lw(reg(result), scratch(), 0),
            }
        }
        Inst::Array2DStore {
            array,
            index0,
            index1,
            value,
            element_size,
        } => {
            if !matches!(*element_size, 1 | 2 | 4) {
                return Err(LowerError::Unsupported);
            }
            emit_2d_element_address(
                enc,
                reg(*array),
                reg(*index0),
                reg(*index1),
                *element_size,
                Reg::T0,
                Reg::T1,
                RiscvProfile::Rv32im,
            );
            match *element_size {
                1 => enc.sb(reg(*value), scratch(), 0),
                2 => enc.sh(reg(*value), scratch(), 0),
                _ => enc.sw(reg(*value), scratch(), 0),
            }
        }
        _ => return Err(LowerError::Unsupported),
    }
    Ok(())
}

/// The all-spilled frame's value-slot layout: each value's byte offset from SP (the first slot
/// sits just past the `out_args_bytes` outgoing-args area) and the total bytes used. ONE source
/// shared by `lower_function_spilled`'s emitted loads/stores and the stack-map record builder
/// below (the ARM `spilled_slot_offsets` twin), so a record's root offsets can never drift from
/// the slots the code actually writes.
fn spilled_slot_offsets(func: &Function, out_args_bytes: i32) -> (Vec<i32>, i32) {
    let mut offsets: Vec<i32> = Vec::with_capacity(func.value_types.len());
    let mut used = out_args_bytes;
    for ty in &func.value_types {
        offsets.push(used);
        used += ty.stack_slot_bytes() as i32;
    }
    (offsets, used)
}

/// The METHOD_SLOTS root list for one all-spilled riscv frame: EVERY ref-typed value's slot
/// (each value owns a slot on this path, so the enumeration is complete by construction --
/// liveness-free, made sound by the prologue's ref-slot zero-init). `offsets` are the frame's
/// value-slot byte offsets from SP, the SAME computation the emitted loads/stores use, so a
/// record's rows can never drift from the slots the code writes. Kinds mirror the ARM builder:
/// an `ObjectRef` a `RefToInt` derives a raw pointer from in an anchor-seam-calling function is
/// PINNED ([`pinned_values`]); a `ManagedPtr` relocates base-only; a `PyValue` traces by tag.
fn method_record_roots(
    func: &Function,
    externs: &[alloc::string::String],
    offsets: &[i32],
) -> Vec<u16> {
    let pinned = pinned_values(func, externs);
    let mut roots = Vec::new();
    for (v, ty) in func.value_types.iter().enumerate() {
        let kind = match ty {
            MirType::ObjectRef if pinned[v] => STACKMAP_KIND_PINNED,
            MirType::ObjectRef => STACKMAP_KIND_OBJECT_REF,
            MirType::ManagedPtr => STACKMAP_KIND_MANAGED_PTR,
            MirType::PyValue => STACKMAP_KIND_TAGGED,
            _ => continue,
        };
        roots.push(((offsets[v] / 4) as u16) | (kind << 14));
    }
    roots
}

/// Lowers a function whose value count exceeds the 12 callee-saved registers into an ALL-SPILLED
/// frame: every value gets a 4-byte stack slot, each instruction loads its operands into the
/// `t0`-`t2` scratch registers, computes, and stores the result back. Nothing live sits in a
/// register across a call, so the caller's values survive with no callee-saved bookkeeping (only
/// `ra` is saved). This lifts the register-only path's value-count cap. The frame's slot offsets
/// must fit the 12-bit `lw`/`sw` immediate, so a function past ~500 values is rejected (deferred).
/// Block parameters move slot-to-slot through `t0`: every value has a distinct slot, so the
/// sequential move is sound (the register path's no-alias assumption).
#[allow(clippy::too_many_arguments)]
fn lower_function_spilled(
    enc: &mut Encoder,
    func: &Function,
    func_labels: &[Label],
    alloc: AllocSite,
    descriptors: &[TypeMeta],
    type_descs: &mut TypeDescs,
    type_desc_labels: &mut Vec<(TypeHandle, Label)>,
    externs: &mut Vec<alloc::string::String>,
    profile: RiscvProfile,
    relocs: &mut Vec<(u32, u32)>,
    desc_relocs: &mut Vec<DescReloc>,
    relocate: bool,
) -> Result<Option<MethodRecordInfo>, LowerError> {
    let max_args = arg_reg_count(profile);
    let arg_overflow = func.blocks[func.entry.index()].params.len() > max_args
        || func.blocks.iter().flat_map(|b| &b.insts).any(|(_, inst)| {
            !matches!(inst, Inst::Call { .. } | Inst::CallNative { .. })
                && register_arg_count(inst) > max_args
        });
    if arg_overflow {
        return Err(LowerError::ControlFlowUnsupported);
    }
    let saves_scratch = matches!(profile, RiscvProfile::Rv32ec);
    let has_calls = func.blocks.iter().any(|b| {
        b.insts.iter().any(|(r, i)| {
            matches!(
                i,
                Inst::Call { .. }
                    | Inst::Alloc { .. }
                    | Inst::AllocLike { .. }
                    | Inst::AllocArray { .. }
                    | Inst::AllocArray2D { .. }
                    | Inst::AllocArrayMD { .. }
                    | Inst::CallIndirect { .. }
                    | Inst::InvokeDelegate { .. }
                    | Inst::CallVirtual { .. }
                    | Inst::CallInterface { .. }
                    | Inst::CallNative { .. }
            ) || matches!(
                i,
                Inst::Binary { op, .. }
                    if int64_soft_routine(*op).is_some() && is_i64(&func.value_types, *r)
            ) || matches!(
                (profile, i),
                (RiscvProfile::Rv32ec, Inst::Binary { op, .. })
                    if (is_soft_int_binop(*op) && !is_i64(&func.value_types, *r))
                        || (matches!(op, BinOp::Mul) && is_i64(&func.value_types, *r))
            ) || inst_is_softfloat_call(i, *r, &func.value_types)
        })
    });
    let out_args_words = func
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .map(|(result, inst)| call_stack_words(inst, *result, &func.value_types, profile))
        .max()
        .unwrap_or(0);
    let out_args_bytes = out_args_words as i32 * 4;
    let (offsets, used) = spilled_slot_offsets(func, out_args_bytes);
    let ra_off = used;
    let scratch_off = ra_off + has_calls as i32 * 4;
    let returns_sret = func
        .ret
        .as_ref()
        .is_some_and(|t| t.stack_slot_bytes() / 4 > 2);
    let sret_off = scratch_off + saves_scratch as i32 * 4;
    let invokes_delegate = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| matches!(i, Inst::InvokeDelegate { .. })));
    let mc_off = sret_off + returns_sret as i32 * 4;
    let frame = ((used
        + has_calls as i32 * 4
        + saves_scratch as i32 * 4
        + returns_sret as i32 * 4
        + invokes_delegate as i32 * 8) as usize)
        .div_ceil(16)
        * 16;
    if frame > 2047 {
        return Err(LowerError::TooManyValues);
    }
    let has_safepoint = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| crate::regalloc::is_safepoint(i)));
    let method_record = has_safepoint.then(|| {
        debug_assert!(has_calls, "a safepoint is always a call: `ra` is saved");
        MethodRecordInfo {
            frame_words: (frame / 4) as u16,
            ret_ra_word: (ra_off / 4) as u16,
            roots: method_record_roots(func, externs, &offsets),
        }
    });
    let slot = |v: ValueId| offsets[v.index()];
    let mut string_blobs: Vec<(Label, Vec<u16>)> = Vec::new();
    let mut desc_ptr_pool: Vec<(TypeHandle, Label)> = Vec::new();
    let mut func_ptr_pool: Vec<(u32, Label)> = Vec::new();
    let mut statics_ptr_pool: Vec<((u32, i32), Label)> = Vec::new();

    if frame > 0 {
        enc.addi(Reg::SP, Reg::SP, -(frame as i32));
    }
    if has_calls {
        enc.sw(Reg::RA, Reg::SP, ra_off);
    }
    if saves_scratch {
        enc.sw(spilled_scratch(profile), Reg::SP, scratch_off);
    }
    let entry = &func.blocks[func.entry.index()];
    let mut arg_index = 0usize;
    if returns_sret {
        enc.sw(Reg::A0, Reg::SP, sret_off);
        arg_index = 1;
    }
    for &param in &entry.params {
        let words = value_words(&func.value_types, param);
        if words > 2 {
            if arg_index >= arg_reg_count(profile) {
                return Err(LowerError::ControlFlowUnsupported);
            }
            let ptr = arg_reg(arg_index).ok_or(LowerError::ControlFlowUnsupported)?;
            for w in 0..slot_words(&func.value_types, param) as i32 {
                enc.lw(Reg::T0, ptr, w * 4);
                enc.sw(Reg::T0, Reg::SP, slot(param) + w * 4);
            }
            arg_index += 1;
            continue;
        }
        for w in 0..words {
            if arg_index >= arg_reg_count(profile) {
                return Err(LowerError::ControlFlowUnsupported);
            }
            let arg = arg_reg(arg_index).ok_or(LowerError::ControlFlowUnsupported)?;
            enc.sw(arg, Reg::SP, slot(param) + w as i32 * 4);
            arg_index += 1;
        }
    }

    if relocate && has_safepoint {
        for (v, ty) in func.value_types.iter().enumerate() {
            if (ty.is_gc_reference() || ty.is_tagged_value())
                && !entry.params.contains(&ValueId(v as u32))
            {
                enc.sw(Reg::ZERO, Reg::SP, offsets[v]);
            }
        }
    }

    let block_labels: Vec<Label> = (0..func.blocks.len()).map(|_| enc.new_label()).collect();
    if func.entry != lamella_ir::BlockId(0) {
        enc.j(block_labels[func.entry.index()]);
    }

    for (index, block) in func.blocks.iter().enumerate() {
        enc.bind_label(block_labels[index]);
        for (result, inst) in &block.insts {
            lower_inst_spilled(
                enc,
                &slot,
                &func.value_types,
                func_labels,
                *result,
                inst,
                alloc,
                descriptors,
                type_descs,
                type_desc_labels,
                &mut string_blobs,
                &mut desc_ptr_pool,
                &mut func_ptr_pool,
                &mut statics_ptr_pool,
                externs,
                profile,
                relocs,
                relocate,
                mc_off,
            )?;
        }
        match &block.terminator {
            Some(Terminator::Return(value)) => {
                if let Some(v) = value {
                    let words = value_words(&func.value_types, *v);
                    if words > 2 {
                        enc.lw(Reg::T0, Reg::SP, sret_off);
                        for w in 0..slot_words(&func.value_types, *v) as i32 {
                            enc.lw(Reg::T1, Reg::SP, slot(*v) + w * 4);
                            enc.sw(Reg::T1, Reg::T0, w * 4);
                        }
                        enc.mv(Reg::A0, Reg::T0);
                    } else {
                        enc.lw(Reg::A0, Reg::SP, slot(*v));
                        if words >= 2 {
                            enc.lw(Reg::A1, Reg::SP, slot(*v) + 4);
                        }
                    }
                }
                if has_calls {
                    enc.lw(Reg::RA, Reg::SP, ra_off);
                }
                if saves_scratch {
                    enc.lw(spilled_scratch(profile), Reg::SP, scratch_off);
                }
                if frame > 0 {
                    enc.addi(Reg::SP, Reg::SP, frame as i32);
                }
                enc.ret();
            }
            Some(Terminator::Jump { target, args }) => {
                let params = &func
                    .block(*target)
                    .ok_or(LowerError::ControlFlowUnsupported)?
                    .params;
                if args.len() != params.len() {
                    return Err(LowerError::ControlFlowUnsupported);
                }
                for (p, a) in params.iter().zip(args) {
                    enc.lw(Reg::T0, Reg::SP, slot(*a));
                    enc.sw(Reg::T0, Reg::SP, slot(*p));
                }
                enc.j(block_labels[target.index()]);
            }
            Some(Terminator::Branch {
                cond,
                if_true,
                true_args,
                if_false,
                false_args,
            }) => {
                if !true_args.is_empty() || !false_args.is_empty() {
                    return Err(LowerError::ControlFlowUnsupported);
                }
                let true_label = block_labels[if_true.index()];
                let false_label = block_labels[if_false.index()];
                enc.lw(Reg::T0, Reg::SP, slot(*cond));
                enc.branch(BranchCond::Ne, Reg::T0, Reg::ZERO, true_label);
                enc.j(false_label);
            }
            Some(Terminator::Unreachable) => enc.ebreak(),
            None => return Err(LowerError::ControlFlowUnsupported),
        }
    }
    let mut ancestor = 0;
    while ancestor < type_desc_labels.len() {
        let handle = type_desc_labels[ancestor].0;
        if let Some(base) = descriptors
            .iter()
            .find(|d| d.handle == handle)
            .and_then(|d| d.base)
        {
            if crate::resolver::reference_handle_parts(base).is_none()
                && !type_desc_labels.iter().any(|(h, _)| *h == base)
            {
                let label = enc.new_label();
                type_descs.push(DescEmit {
                    label,
                    vtable: Vec::new(),
                    words: alloc::vec![0, 0, descriptor_tag(descriptors, base), 0],
                    itable: Vec::new(),
                    base: descriptors
                        .iter()
                        .find(|d| d.handle == base)
                        .and_then(|d| d.base),
                });
                type_desc_labels.push((base, label));
            }
        }
        ancestor += 1;
    }
    for (handle, label) in &desc_ptr_pool {
        enc.bind_label(*label);
        desc_relocs.push((enc.position(), DESC_SYMBOL_FLAG | handle.0, 0));
        enc.emit_word(0);
    }
    for (func_index, label) in &func_ptr_pool {
        enc.bind_label(*label);
        desc_relocs.push((enc.position(), *func_index, 0));
        enc.emit_word(0);
    }
    for ((target, addend), label) in &statics_ptr_pool {
        enc.bind_label(*label);
        desc_relocs.push((enc.position(), *target, *addend));
        enc.emit_word(0);
    }
    for (label, units) in &string_blobs {
        enc.bind_label(*label);
        enc.emit_word(units.len() as u32);
        for pair in units.chunks(2) {
            let lo = u32::from(pair[0]);
            let hi = pair.get(1).map_or(0, |&u| u32::from(u));
            enc.emit_word(lo | (hi << 16));
        }
    }
    Ok(method_record)
}

/// Lowers one instruction in the all-spilled frame: load operands from their slots into `t0`-`t2`,
/// compute, store the result back to its slot. Mirrors [`lower_inst`] but slot-based; `t6` stays
/// the array-addressing scratch and a0-a7 carry call arguments.
#[allow(clippy::too_many_arguments)]
fn lower_inst_spilled(
    enc: &mut Encoder,
    slot: &impl Fn(ValueId) -> i32,
    value_types: &[MirType],
    func_labels: &[Label],
    result: ValueId,
    inst: &Inst,
    alloc: AllocSite,
    descriptors: &[TypeMeta],
    type_descs: &mut TypeDescs,
    type_desc_labels: &mut Vec<(TypeHandle, Label)>,
    string_blobs: &mut Vec<(Label, Vec<u16>)>,
    desc_ptr_pool: &mut Vec<(TypeHandle, Label)>,
    func_ptr_pool: &mut Vec<(u32, Label)>,
    statics_ptr_pool: &mut Vec<((u32, i32), Label)>,
    externs: &mut Vec<alloc::string::String>,
    profile: RiscvProfile,
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
    mc_off: i32,
) -> Result<(), LowerError> {
    let (t0, t1, t2) = (Reg::T0, Reg::T1, Reg::T2);
    let scratch = spilled_scratch(profile);
    match inst {
        Inst::ConstInt { ty, value } => {
            enc.li(t0, *value as i32);
            enc.sw(t0, Reg::SP, slot(result));
            if matches!(ty, MirType::I64 | MirType::F64) {
                enc.li(t0, (*value >> 32) as i32);
                enc.sw(t0, Reg::SP, slot(result) + 4);
            }
        }
        Inst::Widen { value, signed } => {
            enc.lw(t0, Reg::SP, slot(*value));
            enc.sw(t0, Reg::SP, slot(result));
            if *signed {
                enc.srai(t0, t0, 31);
            } else {
                enc.li(t0, 0);
            }
            enc.sw(t0, Reg::SP, slot(result) + 4);
        }
        Inst::Truncate { value } => {
            enc.lw(t0, Reg::SP, slot(*value));
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::Binary { op, lhs, rhs } => {
            if is_i64(value_types, result) {
                let routine = int64_soft_routine(*op).or_else(|| {
                    (matches!(profile, RiscvProfile::Rv32ec) && matches!(op, BinOp::Mul))
                        .then_some("__muldi3")
                });
                if let Some(routine) = routine {
                    if !relocate {
                        return Err(LowerError::Unsupported);
                    }
                    let (a2, a3) = (
                        Reg::new(12).unwrap_or(Reg::ZERO),
                        Reg::new(13).unwrap_or(Reg::ZERO),
                    );
                    enc.lw(Reg::A0, Reg::SP, slot(*lhs));
                    enc.lw(Reg::A1, Reg::SP, slot(*lhs) + 4);
                    enc.lw(a2, Reg::SP, slot(*rhs));
                    enc.lw(a3, Reg::SP, slot(*rhs) + 4);
                    let target = EXTERN_SYMBOL_FLAG | intern_extern(externs, routine);
                    emit_call(enc, func_labels, relocs, relocate, target)?;
                    enc.sw(Reg::A0, Reg::SP, slot(result));
                    enc.sw(Reg::A1, Reg::SP, slot(result) + 4);
                } else {
                    emit_i64_binary(enc, slot, *op, result, *lhs, *rhs, profile)?;
                }
            } else if is_float(value_types, result) {
                let Some(routine) = soft_float_arith(*op, value_types.get(result.index())) else {
                    return Err(LowerError::Unsupported);
                };
                if !relocate {
                    return Err(LowerError::Unsupported);
                }
                if matches!(value_types.get(result.index()), Some(MirType::F64)) {
                    let (a2, a3) = (
                        Reg::new(12).unwrap_or(Reg::ZERO),
                        Reg::new(13).unwrap_or(Reg::ZERO),
                    );
                    enc.lw(Reg::A0, Reg::SP, slot(*lhs));
                    enc.lw(Reg::A1, Reg::SP, slot(*lhs) + 4);
                    enc.lw(a2, Reg::SP, slot(*rhs));
                    enc.lw(a3, Reg::SP, slot(*rhs) + 4);
                    let target = EXTERN_SYMBOL_FLAG | intern_extern(externs, routine);
                    emit_call(enc, func_labels, relocs, relocate, target)?;
                    enc.sw(Reg::A0, Reg::SP, slot(result));
                    enc.sw(Reg::A1, Reg::SP, slot(result) + 4);
                } else {
                    enc.lw(Reg::A0, Reg::SP, slot(*lhs));
                    enc.lw(Reg::A1, Reg::SP, slot(*rhs));
                    let target = EXTERN_SYMBOL_FLAG | intern_extern(externs, routine);
                    emit_call(enc, func_labels, relocs, relocate, target)?;
                    enc.sw(Reg::A0, Reg::SP, slot(result));
                }
            } else if matches!(profile, RiscvProfile::Rv32ec) && is_soft_int_binop(*op) {
                enc.lw(Reg::A0, Reg::SP, slot(*lhs));
                enc.lw(Reg::A1, Reg::SP, slot(*rhs));
                let target = EXTERN_SYMBOL_FLAG | intern_extern(externs, soft_int_routine(*op));
                emit_call(enc, func_labels, relocs, relocate, target)?;
                enc.sw(Reg::A0, Reg::SP, slot(result));
            } else {
                enc.lw(t0, Reg::SP, slot(*lhs));
                enc.lw(t1, Reg::SP, slot(*rhs));
                match op {
                    BinOp::Add => enc.add(t0, t0, t1),
                    BinOp::Sub => enc.sub(t0, t0, t1),
                    BinOp::And => enc.and(t0, t0, t1),
                    BinOp::Or => enc.or(t0, t0, t1),
                    BinOp::Xor => enc.xor(t0, t0, t1),
                    BinOp::Mul => enc.mul(t0, t0, t1),
                    BinOp::DivSigned => enc.div(t0, t0, t1),
                    BinOp::DivUnsigned => enc.divu(t0, t0, t1),
                    BinOp::RemSigned => enc.rem(t0, t0, t1),
                    BinOp::RemUnsigned => enc.remu(t0, t0, t1),
                    BinOp::Shl => enc.sll(t0, t0, t1),
                    BinOp::ShrSigned => enc.sra(t0, t0, t1),
                    BinOp::ShrUnsigned => enc.srl(t0, t0, t1),
                }
                enc.sw(t0, Reg::SP, slot(result));
            }
        }
        Inst::Compare { op, lhs, rhs } => {
            if is_i64(value_types, *lhs) {
                emit_i64_compare(enc, slot, *op, result, *lhs, *rhs, profile)?;
            } else if is_float(value_types, *lhs) {
                if !relocate {
                    return Err(LowerError::Unsupported);
                }
                let (suffix, result_op) = float_compare_plan(*op);
                let is_f64 = matches!(value_types.get(lhs.index()), Some(MirType::F64));
                let name = alloc::format!("__{}{}2", suffix, if is_f64 { "df" } else { "sf" });
                if is_f64 {
                    let (a2, a3) = (
                        Reg::new(12).unwrap_or(Reg::ZERO),
                        Reg::new(13).unwrap_or(Reg::ZERO),
                    );
                    enc.lw(Reg::A0, Reg::SP, slot(*lhs));
                    enc.lw(Reg::A1, Reg::SP, slot(*lhs) + 4);
                    enc.lw(a2, Reg::SP, slot(*rhs));
                    enc.lw(a3, Reg::SP, slot(*rhs) + 4);
                } else {
                    enc.lw(Reg::A0, Reg::SP, slot(*lhs));
                    enc.lw(Reg::A1, Reg::SP, slot(*rhs));
                }
                let target = EXTERN_SYMBOL_FLAG | intern_extern(externs, &name);
                emit_call(enc, func_labels, relocs, relocate, target)?;
                materialize_compare(enc, t0, Reg::A0, Reg::ZERO, result_op);
                enc.sw(t0, Reg::SP, slot(result));
            } else {
                enc.lw(t0, Reg::SP, slot(*lhs));
                enc.lw(t1, Reg::SP, slot(*rhs));
                materialize_compare(enc, t2, t0, t1, *op);
                enc.sw(t2, Reg::SP, slot(result));
            }
        }
        Inst::Convert { value, kind } => {
            if let Some((routine, src_words, dst_words)) = soft_float_convert(*kind) {
                if !relocate {
                    return Err(LowerError::Unsupported);
                }
                enc.lw(Reg::A0, Reg::SP, slot(*value));
                if src_words >= 2 {
                    enc.lw(Reg::A1, Reg::SP, slot(*value) + 4);
                }
                let target = EXTERN_SYMBOL_FLAG | intern_extern(externs, routine);
                emit_call(enc, func_labels, relocs, relocate, target)?;
                enc.sw(Reg::A0, Reg::SP, slot(result));
                if dst_words >= 2 {
                    enc.sw(Reg::A1, Reg::SP, slot(result) + 4);
                }
            } else {
                enc.lw(t0, Reg::SP, slot(*value));
                emit_convert(enc, t0, t0, *kind)?;
                enc.sw(t0, Reg::SP, slot(result));
            }
        }
        Inst::CopyBlock { dst, src, size } => {
            enc.lw(t0, Reg::SP, slot(*dst));
            enc.lw(t1, Reg::SP, slot(*src));
            enc.lw(t2, Reg::SP, slot(*size));
            emit_copy_block(enc, t0, t1, t2, profile);
        }
        Inst::FillBlock { dst, value, size } => {
            enc.lw(t0, Reg::SP, slot(*dst));
            enc.lw(t1, Reg::SP, slot(*value));
            enc.lw(t2, Reg::SP, slot(*size));
            emit_fill_block(enc, t0, t1, t2);
        }
        Inst::StaticLoad { owner, offset } => {
            emit_static_addr(enc, t0, t1, owner, *offset, statics_ptr_pool, relocate)?;
            enc.lw(t0, t0, 0);
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::StaticStore {
            owner,
            offset,
            value,
        } => {
            emit_static_addr(enc, t0, t1, owner, *offset, statics_ptr_pool, relocate)?;
            enc.lw(t1, Reg::SP, slot(*value));
            enc.sw(t1, t0, 0);
        }
        Inst::AllocArray2D {
            handle,
            dim0,
            dim1,
            element_size,
        } => {
            let desc_label = match type_desc_labels.iter().find(|(h, _)| h == handle) {
                Some((_, l)) => *l,
                None => {
                    let l = enc.new_label();
                    type_descs.push(DescEmit {
                        label: l,
                        vtable: Vec::new(),
                        words: alloc::vec![*element_size, 0, 0],
                        itable: Vec::new(),
                        base: None,
                    });
                    type_desc_labels.push((*handle, l));
                    l
                }
            };
            enc.lw(t0, Reg::SP, slot(*dim0));
            enc.lw(t1, Reg::SP, slot(*dim1));
            emit_soft_mul32(enc, t2, t0, t1, scratch, profile);
            if element_size.is_power_of_two() {
                enc.slli(t2, t2, element_size.trailing_zeros());
            } else {
                enc.li(t1, *element_size as i32);
                enc.mul(t2, t2, t1);
            }
            enc.addi(Reg::A0, t2, 8);
            emit_desc_words_addr(enc, Reg::A1, t0, *handle, desc_label, desc_ptr_pool, relocate);
            emit_alloc_call(enc, alloc, func_labels, relocs, relocate)?;
            let ok = enc.new_label();
            enc.branch(BranchCond::Ne, Reg::A0, Reg::ZERO, ok);
            enc.ebreak();
            enc.bind_label(ok);
            enc.lw(t0, Reg::SP, slot(*dim0));
            enc.sw(t0, Reg::A0, 0);
            enc.lw(t0, Reg::SP, slot(*dim1));
            enc.sw(t0, Reg::A0, 4);
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::Array2DLoad {
            array,
            index0,
            index1,
            element_size,
            signed,
        } => {
            if !matches!(*element_size, 1 | 2 | 4) {
                return Err(LowerError::Unsupported);
            }
            enc.lw(t0, Reg::SP, slot(*array));
            enc.lw(t1, Reg::SP, slot(*index0));
            enc.lw(t2, Reg::SP, slot(*index1));
            emit_2d_element_address(enc, t0, t1, t2, *element_size, Reg::A0, Reg::A1, profile);
            match (*element_size, *signed) {
                (1, true) => enc.lb(t0, scratch, 0),
                (1, false) => enc.lbu(t0, scratch, 0),
                (2, true) => enc.lh(t0, scratch, 0),
                (2, false) => enc.lhu(t0, scratch, 0),
                _ => enc.lw(t0, scratch, 0),
            }
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::Array2DStore {
            array,
            index0,
            index1,
            value,
            element_size,
        } => {
            if !matches!(*element_size, 1 | 2 | 4) {
                return Err(LowerError::Unsupported);
            }
            enc.lw(t0, Reg::SP, slot(*array));
            enc.lw(t1, Reg::SP, slot(*index0));
            enc.lw(t2, Reg::SP, slot(*index1));
            emit_2d_element_address(enc, t0, t1, t2, *element_size, Reg::A0, Reg::A1, profile);
            enc.lw(t0, Reg::SP, slot(*value));
            match *element_size {
                1 => enc.sb(t0, scratch, 0),
                2 => enc.sh(t0, scratch, 0),
                _ => enc.sw(t0, scratch, 0),
            }
        }
        Inst::AllocArrayMD {
            handle,
            dims,
            element_size,
        } => {
            if matches!(profile, RiscvProfile::Rv32ec) {
                return Err(LowerError::Unsupported);
            }
            let n = dims.len() as i32;
            let header = 4 * n;
            let desc_label = match type_desc_labels.iter().find(|(h, _)| h == handle) {
                Some((_, l)) => *l,
                None => {
                    let l = enc.new_label();
                    type_descs.push(DescEmit {
                        label: l,
                        vtable: Vec::new(),
                        words: alloc::vec![*element_size, 0, 0],
                        itable: Vec::new(),
                        base: None,
                    });
                    type_desc_labels.push((*handle, l));
                    l
                }
            };
            enc.lw(t0, Reg::SP, slot(dims[0]));
            for d in &dims[1..] {
                enc.lw(t1, Reg::SP, slot(*d));
                enc.mul(t0, t0, t1);
            }
            if element_size.is_power_of_two() {
                enc.slli(t0, t0, element_size.trailing_zeros());
            } else {
                enc.li(t1, *element_size as i32);
                enc.mul(t0, t0, t1);
            }
            enc.addi(Reg::A0, t0, header);
            emit_desc_words_addr(enc, Reg::A1, t0, *handle, desc_label, desc_ptr_pool, relocate);
            emit_alloc_call(enc, alloc, func_labels, relocs, relocate)?;
            let ok = enc.new_label();
            enc.branch(BranchCond::Ne, Reg::A0, Reg::ZERO, ok);
            enc.ebreak();
            enc.bind_label(ok);
            for (k, d) in dims.iter().enumerate() {
                enc.lw(t0, Reg::SP, slot(*d));
                enc.sw(t0, Reg::A0, 4 * k as i32);
            }
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::ArrayMDLoad {
            array,
            indices,
            element_size,
            signed,
        } => {
            if matches!(profile, RiscvProfile::Rv32ec) || !matches!(*element_size, 1 | 2 | 4) {
                return Err(LowerError::Unsupported);
            }
            enc.lw(t0, Reg::SP, slot(*array));
            emit_md_element_address(enc, t0, t1, t2, scratch, slot, indices, *element_size);
            match (*element_size, *signed) {
                (1, true) => enc.lb(t0, t1, 0),
                (1, false) => enc.lbu(t0, t1, 0),
                (2, true) => enc.lh(t0, t1, 0),
                (2, false) => enc.lhu(t0, t1, 0),
                _ => enc.lw(t0, t1, 0),
            }
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::ArrayMDStore {
            array,
            indices,
            value,
            element_size,
        } => {
            if matches!(profile, RiscvProfile::Rv32ec) || !matches!(*element_size, 1 | 2 | 4) {
                return Err(LowerError::Unsupported);
            }
            enc.lw(t0, Reg::SP, slot(*array));
            emit_md_element_address(enc, t0, t1, t2, scratch, slot, indices, *element_size);
            enc.lw(t0, Reg::SP, slot(*value));
            match *element_size {
                1 => enc.sb(t0, t1, 0),
                2 => enc.sh(t0, t1, 0),
                _ => enc.sw(t0, t1, 0),
            }
        }
        Inst::Load {
            address,
            width,
            signed,
        } => {
            enc.lw(t0, Reg::SP, slot(*address));
            match (*width, *signed) {
                (1, true) => enc.lb(t1, t0, 0),
                (1, false) => enc.lbu(t1, t0, 0),
                (2, true) => enc.lh(t1, t0, 0),
                (2, false) => enc.lhu(t1, t0, 0),
                _ => enc.lw(t1, t0, 0),
            }
            enc.sw(t1, Reg::SP, slot(result));
        }
        Inst::Store {
            address,
            value,
            width,
        } => {
            enc.lw(t0, Reg::SP, slot(*address));
            enc.lw(t1, Reg::SP, slot(*value));
            match *width {
                1 => enc.sb(t1, t0, 0),
                2 => enc.sh(t1, t0, 0),
                _ => enc.sw(t1, t0, 0),
            }
        }
        Inst::FieldLoad { base, offset } => {
            let ptr = is_pointer(value_types, *base);
            if let Some((full_words, rem)) = value_type_copy_extent(value_types, result) {
                if ptr {
                    enc.lw(t0, Reg::SP, slot(*base));
                }
                for w in 0..full_words {
                    let foff = *offset + w * 4;
                    if ptr {
                        enc.lw(t1, t0, field_offset(foff)?);
                    } else {
                        enc.lw(t1, Reg::SP, slot(*base) + foff as i32);
                    }
                    enc.sw(t1, Reg::SP, slot(result) + (w * 4) as i32);
                }
                for k in 0..rem {
                    let at = full_words * 4 + k;
                    if ptr {
                        enc.lbu(t1, t0, field_offset(*offset + at)?);
                    } else {
                        enc.lbu(t1, Reg::SP, slot(*base) + (*offset + at) as i32);
                    }
                    enc.sb(t1, Reg::SP, slot(result) + at as i32);
                }
            } else if ptr {
                enc.lw(t0, Reg::SP, slot(*base));
                enc.lw(t1, t0, field_offset(*offset)?);
                enc.sw(t1, Reg::SP, slot(result));
            } else {
                enc.lw(t1, Reg::SP, slot(*base) + *offset as i32);
                enc.sw(t1, Reg::SP, slot(result));
            }
        }
        Inst::FieldStore {
            base,
            offset,
            value,
        } => {
            let ptr = is_pointer(value_types, *base);
            if let Some((full_words, rem)) = value_type_copy_extent(value_types, *value) {
                if ptr {
                    enc.lw(t0, Reg::SP, slot(*base));
                }
                for w in 0..full_words {
                    let foff = *offset + w * 4;
                    enc.lw(t1, Reg::SP, slot(*value) + (w * 4) as i32);
                    if ptr {
                        enc.sw(t1, t0, field_offset(foff)?);
                    } else {
                        enc.sw(t1, Reg::SP, slot(*base) + foff as i32);
                    }
                }
                for k in 0..rem {
                    let at = full_words * 4 + k;
                    enc.lbu(t1, Reg::SP, slot(*value) + at as i32);
                    if ptr {
                        enc.sb(t1, t0, field_offset(*offset + at)?);
                    } else {
                        enc.sb(t1, Reg::SP, slot(*base) + (*offset + at) as i32);
                    }
                }
            } else if ptr {
                enc.lw(t0, Reg::SP, slot(*base));
                enc.lw(t1, Reg::SP, slot(*value));
                enc.sw(t1, t0, field_offset(*offset)?);
            } else {
                enc.lw(t1, Reg::SP, slot(*value));
                enc.sw(t1, Reg::SP, slot(*base) + *offset as i32);
            }
        }
        Inst::FieldLoadNarrow {
            base,
            offset,
            size,
            signed,
        } => {
            if is_pointer(value_types, *base) {
                enc.lw(t0, Reg::SP, slot(*base));
            } else {
                enc.addi(t0, Reg::SP, slot(*base));
            }
            let off = field_offset(*offset)?;
            match (*size, *signed) {
                (1, false) => enc.lbu(t1, t0, off),
                (1, true) => enc.lb(t1, t0, off),
                (2, false) => enc.lhu(t1, t0, off),
                (2, true) => enc.lh(t1, t0, off),
                _ => return Err(LowerError::Unsupported),
            }
            enc.sw(t1, Reg::SP, slot(result));
        }
        Inst::FieldStoreNarrow {
            base,
            offset,
            value,
            size,
        } => {
            if is_pointer(value_types, *base) {
                enc.lw(t0, Reg::SP, slot(*base));
            } else {
                enc.addi(t0, Reg::SP, slot(*base));
            }
            enc.lw(t1, Reg::SP, slot(*value));
            let off = field_offset(*offset)?;
            match *size {
                1 => enc.sb(t1, t0, off),
                2 => enc.sh(t1, t0, off),
                _ => return Err(LowerError::Unsupported),
            }
        }
        Inst::FieldAddr { base, offset } => {
            if is_pointer(value_types, *base) {
                enc.lw(t0, Reg::SP, slot(*base));
                enc.addi(t1, t0, field_offset(*offset)?);
            } else {
                enc.addi(t1, Reg::SP, slot(*base) + *offset as i32);
            }
            enc.sw(t1, Reg::SP, slot(result));
        }
        Inst::InitStruct => {
            let words = slot_words(value_types, result);
            enc.li(t0, 0);
            for w in 0..words {
                enc.sw(t0, Reg::SP, slot(result) + (w * 4) as i32);
            }
        }
        Inst::CopyStruct { src } => {
            let words = slot_words(value_types, result);
            for w in 0..words {
                let off = (w * 4) as i32;
                enc.lw(t0, Reg::SP, slot(*src) + off);
                enc.sw(t0, Reg::SP, slot(result) + off);
            }
        }
        Inst::ArrayLoad {
            array,
            index,
            element_size,
            signed,
        } => {
            if !matches!(*element_size, 1 | 2 | 4 | 8) {
                return Err(LowerError::Unsupported);
            }
            enc.lw(t0, Reg::SP, slot(*array));
            enc.lw(t1, Reg::SP, slot(*index));
            emit_element_address(enc, t0, t1, *element_size, profile);
            match (*element_size, *signed) {
                (1, true) => enc.lb(t2, scratch, 4),
                (1, false) => enc.lbu(t2, scratch, 4),
                (2, true) => enc.lh(t2, scratch, 4),
                (2, false) => enc.lhu(t2, scratch, 4),
                (8, _) => {
                    enc.lw(t2, scratch, 4);
                    enc.sw(t2, Reg::SP, slot(result));
                    enc.lw(t2, scratch, 8);
                    enc.sw(t2, Reg::SP, slot(result) + 4);
                    return Ok(());
                }
                _ => enc.lw(t2, scratch, 4),
            }
            enc.sw(t2, Reg::SP, slot(result));
        }
        Inst::ArrayStore {
            array,
            index,
            value,
            element_size,
        } => {
            if !matches!(*element_size, 1 | 2 | 4 | 8) {
                return Err(LowerError::Unsupported);
            }
            enc.lw(t0, Reg::SP, slot(*array));
            enc.lw(t1, Reg::SP, slot(*index));
            emit_element_address(enc, t0, t1, *element_size, profile);
            if *element_size == 8 {
                enc.lw(t2, Reg::SP, slot(*value));
                enc.sw(t2, scratch, 4);
                enc.lw(t2, Reg::SP, slot(*value) + 4);
                enc.sw(t2, scratch, 8);
                return Ok(());
            }
            enc.lw(t2, Reg::SP, slot(*value));
            match *element_size {
                1 => enc.sb(t2, scratch, 4),
                2 => enc.sh(t2, scratch, 4),
                _ => enc.sw(t2, scratch, 4),
            }
        }
        Inst::ArrayElemAddr {
            array,
            index,
            element_size,
        } => {
            enc.lw(t0, Reg::SP, slot(*array));
            enc.lw(t1, Reg::SP, slot(*index));
            emit_element_address(enc, t0, t1, *element_size, profile);
            enc.addi(t2, scratch, 4);
            enc.sw(t2, Reg::SP, slot(result));
        }
        Inst::Alloc {
            handle,
            payload_size,
            ref_offsets,
        } => {
            let has_descriptor = descriptors
                .iter()
                .find(|d| d.handle == *handle)
                .is_some_and(|m| !m.vtable.is_empty() || !m.itable.is_empty());
            let desc_label = descriptor_label(
                enc,
                *handle,
                descriptors,
                *payload_size,
                ref_offsets,
                type_descs,
                type_desc_labels,
                externs,
            );
            enc.li(
                Reg::A0,
                (*payload_size + if has_descriptor { 4 } else { 0 }) as i32,
            );
            emit_desc_words_addr(enc, Reg::A1, t0, *handle, desc_label, desc_ptr_pool, relocate);
            emit_alloc_call(enc, alloc, func_labels, relocs, relocate)?;
            let ok = enc.new_label();
            enc.branch(BranchCond::Ne, Reg::A0, Reg::ZERO, ok);
            enc.ebreak();
            enc.bind_label(ok);
            if has_descriptor {
                emit_desc_words_addr(enc, t0, t1, *handle, desc_label, desc_ptr_pool, relocate);
                enc.sw(t0, Reg::A0, 0);
                enc.addi(Reg::A0, Reg::A0, 4);
            }
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::AllocLike {
            proto,
            payload_size,
        } => {
            enc.lw(t0, Reg::SP, slot(*proto));
            enc.lw(Reg::A1, t0, -4);
            enc.li(Reg::A0, (*payload_size + 4) as i32);
            emit_alloc_call(enc, alloc, func_labels, relocs, relocate)?;
            let ok = enc.new_label();
            enc.branch(BranchCond::Ne, Reg::A0, Reg::ZERO, ok);
            enc.ebreak();
            enc.bind_label(ok);
            enc.lw(t0, Reg::SP, slot(*proto));
            enc.lw(t1, t0, -4);
            enc.sw(t1, Reg::A0, 0);
            enc.addi(Reg::A0, Reg::A0, 4);
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::AllocArray {
            handle,
            length,
            element_size,
        } => {
            let desc_label = match type_desc_labels.iter().find(|(h, _)| h == handle) {
                Some((_, l)) => *l,
                None => {
                    let l = enc.new_label();
                    type_descs.push(DescEmit {
                        label: l,
                        vtable: Vec::new(),
                        words: alloc::vec![*element_size, 0],
                        itable: Vec::new(),
                        base: None,
                    });
                    type_desc_labels.push((*handle, l));
                    l
                }
            };
            enc.lw(t0, Reg::SP, slot(*length));
            if element_size.is_power_of_two() {
                enc.slli(t0, t0, element_size.trailing_zeros());
            } else {
                enc.li(t1, *element_size as i32);
                enc.mul(t0, t0, t1);
            }
            enc.addi(Reg::A0, t0, 4);
            emit_desc_words_addr(enc, Reg::A1, t0, *handle, desc_label, desc_ptr_pool, relocate);
            emit_alloc_call(enc, alloc, func_labels, relocs, relocate)?;
            let ok = enc.new_label();
            enc.branch(BranchCond::Ne, Reg::A0, Reg::ZERO, ok);
            enc.ebreak();
            enc.bind_label(ok);
            enc.lw(t0, Reg::SP, slot(*length));
            enc.sw(t0, Reg::A0, 0);
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::Call { callee, args } => {
            let first = emit_sret_arg(enc, slot, value_types, result);
            marshal_call_args(enc, slot, value_types, args, first, profile)?;
            emit_call(enc, func_labels, relocs, relocate, *callee)?;
            if first == 0 {
                store_call_result(enc, slot, value_types, result);
            }
        }
        Inst::CallNative { symbol, args } => {
            if !relocate {
                return Err(LowerError::Unsupported);
            }
            let first = emit_sret_arg(enc, slot, value_types, result);
            marshal_call_args(enc, slot, value_types, args, first, profile)?;
            emit_call(
                enc,
                func_labels,
                relocs,
                relocate,
                EXTERN_SYMBOL_FLAG | *symbol,
            )?;
            if first == 0 {
                store_call_result(enc, slot, value_types, result);
            }
        }
        Inst::FuncAddr { func } => {
            if relocate {
                let word = match func_ptr_pool.iter().find(|(f, _)| f == func) {
                    Some((_, l)) => *l,
                    None => {
                        let l = enc.new_label();
                        func_ptr_pool.push((*func, l));
                        l
                    }
                };
                enc.la(t0, word);
                enc.lw(t1, t0, 0);
                enc.add(t0, t0, t1);
            } else {
                let label = *func_labels
                    .get(*func as usize)
                    .ok_or(LowerError::ControlFlowUnsupported)?;
                enc.la(t0, label);
            }
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::CallIndirect { target, args, .. } => {
            enc.lw(scratch, Reg::SP, slot(*target));
            for (i, &arg) in args.iter().enumerate() {
                let r = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(r, Reg::SP, slot(arg));
            }
            enc.jalr(Reg::RA, scratch, 0);
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::InvokeDelegate { delegate, args, .. } => {
            if args.len() > 7 {
                return Err(LowerError::ControlFlowUnsupported);
            }
            let mloop = enc.new_label();
            let multi = enc.new_label();
            let dispatch = enc.new_label();
            let static_call = enc.new_label();
            let do_call = enc.new_label();
            let mdone = enc.new_label();
            enc.sw(Reg::ZERO, Reg::SP, mc_off);
            enc.bind_label(mloop);
            enc.lw(t0, Reg::SP, slot(*delegate));
            enc.lw(t1, t0, 8);
            enc.branch(BranchCond::Ne, t1, Reg::ZERO, multi);
            enc.lw(t2, Reg::SP, mc_off);
            enc.branch(BranchCond::Ne, t2, Reg::ZERO, mdone);
            enc.j(dispatch);
            enc.bind_label(multi);
            enc.lw(t2, t1, 0);
            enc.lw(t0, Reg::SP, mc_off);
            enc.branch(BranchCond::GeU, t0, t2, mdone);
            enc.slli(t0, t0, 2);
            enc.add(t0, t1, t0);
            enc.lw(t0, t0, 4);
            enc.bind_label(dispatch);
            enc.lw(scratch, t0, 4);
            enc.lw(t1, t0, 0);
            enc.branch(BranchCond::Eq, t1, Reg::ZERO, static_call);
            enc.mv(Reg::A0, t1);
            for (i, &arg) in args.iter().enumerate() {
                let r = arg_reg(i + 1).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(r, Reg::SP, slot(arg));
            }
            enc.j(do_call);
            enc.bind_label(static_call);
            for (i, &arg) in args.iter().enumerate() {
                let r = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(r, Reg::SP, slot(arg));
            }
            enc.bind_label(do_call);
            enc.jalr(Reg::RA, scratch, 0);
            enc.sw(Reg::A0, Reg::SP, mc_off + 4);
            enc.lw(t0, Reg::SP, mc_off);
            enc.addi(t0, t0, 1);
            enc.sw(t0, Reg::SP, mc_off);
            enc.j(mloop);
            enc.bind_label(mdone);
            enc.lw(Reg::A0, Reg::SP, mc_off + 4);
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::CallVirtual {
            slot: vslot, args, ..
        } => {
            let receiver = *args.first().ok_or(LowerError::ControlFlowUnsupported)?;
            let entry_off = vslot
                .checked_mul(4)
                .and_then(|x| x.checked_add(4))
                .filter(|&o| o <= 2047)
                .ok_or(LowerError::Unsupported)?;
            enc.lw(t0, Reg::SP, slot(receiver));
            enc.lw(t0, t0, -4);
            enc.lw(t1, t0, -(entry_off as i32));
            enc.add(scratch, t0, t1);
            for (i, &arg) in args.iter().enumerate() {
                let r = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(r, Reg::SP, slot(arg));
            }
            enc.jalr(Reg::RA, scratch, 0);
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::CallInterface { tag, args, .. } => {
            let receiver = *args.first().ok_or(LowerError::ControlFlowUnsupported)?;
            enc.li(Reg::A0, *tag as i32);
            enc.lw(t0, Reg::SP, slot(receiver));
            enc.lw(t0, t0, -4);
            enc.lw(t1, t0, 4);
            enc.slli(t1, t1, 2);
            enc.addi(t1, t1, (DESC_HEADER_WORDS * 4) as i32);
            enc.add(t1, t0, t1);
            enc.lw(t2, t1, 0);
            enc.addi(t1, t1, 4);
            let loop_top = enc.new_label();
            let notfound = enc.new_label();
            let found = enc.new_label();
            enc.bind_label(loop_top);
            enc.branch(BranchCond::Eq, t2, Reg::ZERO, notfound);
            enc.lw(scratch, t1, 0);
            enc.branch(BranchCond::Eq, scratch, Reg::A0, found);
            enc.addi(t1, t1, 8);
            enc.addi(t2, t2, -1);
            enc.j(loop_top);
            enc.bind_label(notfound);
            enc.ebreak();
            enc.bind_label(found);
            enc.lw(scratch, t1, 4);
            enc.add(scratch, t0, scratch);
            for (i, &arg) in args.iter().enumerate() {
                let r = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(r, Reg::SP, slot(arg));
            }
            enc.jalr(Reg::RA, scratch, 0);
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::VirtualFuncAddr {
            object,
            slot: vslot,
        } => {
            let entry_off = vslot
                .checked_mul(4)
                .and_then(|x| x.checked_add(4))
                .filter(|&o| o <= 2047)
                .ok_or(LowerError::Unsupported)?;
            enc.lw(t0, Reg::SP, slot(*object));
            enc.lw(t0, t0, -4);
            enc.lw(t1, t0, -(entry_off as i32));
            enc.add(t0, t0, t1);
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::LoadTypeDesc { object } => {
            enc.lw(t0, Reg::SP, slot(*object));
            enc.lw(t0, t0, -4);
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::TypeDescAddr { handle } => {
            let desc_label = descriptor_label(
                enc,
                *handle,
                descriptors,
                0,
                &[],
                type_descs,
                type_desc_labels,
                externs,
            );
            emit_desc_words_addr(enc, t0, t1, *handle, desc_label, desc_ptr_pool, relocate);
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::CastClassScan { args } => {
            let start = *args.first().ok_or(LowerError::ControlFlowUnsupported)?;
            let target = *args.get(1).ok_or(LowerError::ControlFlowUnsupported)?;
            enc.lw(t0, Reg::SP, slot(start));
            enc.lw(t1, Reg::SP, slot(target));
            let search = enc.new_label();
            let found = enc.new_label();
            let miss = enc.new_label();
            let done = enc.new_label();
            enc.bind_label(search);
            enc.branch(BranchCond::Eq, t0, t1, found);
            enc.lw(t2, t0, 12);
            enc.branch(BranchCond::Eq, t2, Reg::ZERO, miss);
            enc.add(t0, t0, t2);
            enc.j(search);
            enc.bind_label(found);
            enc.li(t0, 1);
            enc.j(done);
            enc.bind_label(miss);
            enc.li(t0, 0);
            enc.bind_label(done);
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::StringLiteral { utf16 } => {
            let label = match string_blobs
                .iter()
                .find(|(_, u)| u.as_slice() == utf16.as_ref())
            {
                Some((l, _)) => *l,
                None => {
                    let l = enc.new_label();
                    string_blobs.push((l, utf16.to_vec()));
                    l
                }
            };
            enc.la(t0, label);
            enc.sw(t0, Reg::SP, slot(result));
        }
        _ => return Err(LowerError::Unsupported),
    }
    Ok(())
}

/// Emits an int64 (two-word) arithmetic/bitwise op over slot pairs: the lo word at `slot(v)`, the hi
/// at `slot(v)+4`. Add/Sub propagate the carry/borrow via `sltu`; And/Or/Xor are per-word. Mul (needs
/// `mulhu` + cross terms), the shifts (a barrel across words), and div/rem (a soft `__divdi3`) are
/// deferred -- an int64 form of those rejects as `Unsupported`.
fn emit_i64_binary(
    enc: &mut Encoder,
    slot: &impl Fn(ValueId) -> i32,
    op: BinOp,
    result: ValueId,
    lhs: ValueId,
    rhs: ValueId,
    profile: RiscvProfile,
) -> Result<(), LowerError> {
    let (t0, t1, t2, carry) = (Reg::T0, Reg::T1, Reg::T2, spilled_scratch(profile));
    match op {
        BinOp::Add => {
            enc.lw(t0, Reg::SP, slot(lhs));
            enc.lw(t1, Reg::SP, slot(rhs));
            enc.add(t2, t0, t1);
            enc.sltu(carry, t2, t0);
            enc.sw(t2, Reg::SP, slot(result));
            enc.lw(t0, Reg::SP, slot(lhs) + 4);
            enc.lw(t1, Reg::SP, slot(rhs) + 4);
            enc.add(t0, t0, t1);
            enc.add(t0, t0, carry);
            enc.sw(t0, Reg::SP, slot(result) + 4);
        }
        BinOp::Sub => {
            enc.lw(t0, Reg::SP, slot(lhs));
            enc.lw(t1, Reg::SP, slot(rhs));
            enc.sltu(carry, t0, t1);
            enc.sub(t2, t0, t1);
            enc.sw(t2, Reg::SP, slot(result));
            enc.lw(t0, Reg::SP, slot(lhs) + 4);
            enc.lw(t1, Reg::SP, slot(rhs) + 4);
            enc.sub(t0, t0, t1);
            enc.sub(t0, t0, carry);
            enc.sw(t0, Reg::SP, slot(result) + 4);
        }
        BinOp::And | BinOp::Or | BinOp::Xor => {
            for off in [0i32, 4] {
                enc.lw(t0, Reg::SP, slot(lhs) + off);
                enc.lw(t1, Reg::SP, slot(rhs) + off);
                match op {
                    BinOp::And => enc.and(t0, t0, t1),
                    BinOp::Or => enc.or(t0, t0, t1),
                    _ => enc.xor(t0, t0, t1),
                }
                enc.sw(t0, Reg::SP, slot(result) + off);
            }
        }
        BinOp::Mul => {
            let acc = arg_reg(0).ok_or(LowerError::ControlFlowUnsupported)?;
            enc.lw(t0, Reg::SP, slot(lhs));
            enc.lw(t1, Reg::SP, slot(rhs));
            enc.mul(t2, t0, t1);
            enc.sw(t2, Reg::SP, slot(result));
            enc.mulhu(t2, t0, t1);
            enc.lw(acc, Reg::SP, slot(rhs) + 4);
            enc.mul(acc, t0, acc);
            enc.add(t2, t2, acc);
            enc.lw(acc, Reg::SP, slot(lhs) + 4);
            enc.mul(acc, acc, t1);
            enc.add(t2, t2, acc);
            enc.sw(t2, Reg::SP, slot(result) + 4);
        }
        BinOp::Shl | BinOp::ShrSigned | BinOp::ShrUnsigned => {
            emit_i64_shift(enc, slot, op, result, lhs, rhs, profile)?;
        }
        _ => return Err(LowerError::Unsupported),
    }
    Ok(())
}

/// Emits an int64 (two-word) variable shift over slot pairs. The amount is the low word of `rhs`
/// (0-63). Three cases: `sh == 0` copies the input; `1 <= sh < 32` shifts each word and folds the
/// bits crossing the word boundary; `sh >= 32` shifts by `sh - 32` across the words and zero/sign
/// fills the vacated word. A left shift fills the low word with 0; a logical right the high with 0;
/// an arithmetic right the high with the sign.
fn emit_i64_shift(
    enc: &mut Encoder,
    slot: &impl Fn(ValueId) -> i32,
    op: BinOp,
    result: ValueId,
    lhs: ValueId,
    rhs: ValueId,
    profile: RiscvProfile,
) -> Result<(), LowerError> {
    let (lo, hi, sh, tmp) = (Reg::T0, Reg::T1, Reg::T2, spilled_scratch(profile));
    let lo_out = arg_reg(0).ok_or(LowerError::ControlFlowUnsupported)?;
    let hi_out = arg_reg(1).ok_or(LowerError::ControlFlowUnsupported)?;
    enc.lw(lo, Reg::SP, slot(lhs));
    enc.lw(hi, Reg::SP, slot(lhs) + 4);
    enc.lw(sh, Reg::SP, slot(rhs));
    let ge32 = enc.new_label();
    let small = enc.new_label();
    let store = enc.new_label();
    enc.andi(tmp, sh, 32);
    enc.branch(BranchCond::Ne, tmp, Reg::ZERO, ge32);
    enc.branch(BranchCond::Ne, sh, Reg::ZERO, small);
    enc.mv(lo_out, lo);
    enc.mv(hi_out, hi);
    enc.j(store);
    enc.bind_label(small);
    enc.li(tmp, 32);
    enc.sub(tmp, tmp, sh);
    match op {
        BinOp::Shl => {
            enc.sll(hi_out, hi, sh);
            enc.srl(tmp, lo, tmp);
            enc.or(hi_out, hi_out, tmp);
            enc.sll(lo_out, lo, sh);
        }
        BinOp::ShrUnsigned => {
            enc.srl(lo_out, lo, sh);
            enc.sll(tmp, hi, tmp);
            enc.or(lo_out, lo_out, tmp);
            enc.srl(hi_out, hi, sh);
        }
        BinOp::ShrSigned => {
            enc.srl(lo_out, lo, sh);
            enc.sll(tmp, hi, tmp);
            enc.or(lo_out, lo_out, tmp);
            enc.sra(hi_out, hi, sh);
        }
        _ => return Err(LowerError::Unsupported),
    }
    enc.j(store);
    enc.bind_label(ge32);
    enc.andi(sh, sh, 31);
    match op {
        BinOp::Shl => {
            enc.sll(hi_out, lo, sh);
            enc.li(lo_out, 0);
        }
        BinOp::ShrUnsigned => {
            enc.srl(lo_out, hi, sh);
            enc.li(hi_out, 0);
        }
        BinOp::ShrSigned => {
            enc.sra(lo_out, hi, sh);
            enc.srai(hi_out, hi, 31);
        }
        _ => return Err(LowerError::Unsupported),
    }
    enc.bind_label(store);
    enc.sw(lo_out, Reg::SP, slot(result));
    enc.sw(hi_out, Reg::SP, slot(result) + 4);
    Ok(())
}

/// Emits an int64 comparison over slot pairs, producing an `int32` 0/1. Equality XORs both words and
/// tests the union for zero; an ordering compares the hi word (signed for a signed op, unsigned
/// otherwise) and, when the hi words are equal, the lo word unsigned -- with the operands swapped for
/// `>`/`>=` and the result negated for `<=`/`>=`.
fn emit_i64_compare(
    enc: &mut Encoder,
    slot: &impl Fn(ValueId) -> i32,
    op: CmpOp,
    result: ValueId,
    lhs: ValueId,
    rhs: ValueId,
    profile: RiscvProfile,
) -> Result<(), LowerError> {
    let (t0, t1, t2, tmp) = (Reg::T0, Reg::T1, Reg::T2, spilled_scratch(profile));
    let hi0 = arg_reg(0).ok_or(LowerError::ControlFlowUnsupported)?;
    let hi1 = arg_reg(1).ok_or(LowerError::ControlFlowUnsupported)?;
    if matches!(op, CmpOp::Eq | CmpOp::Ne) {
        enc.lw(t0, Reg::SP, slot(lhs));
        enc.lw(t1, Reg::SP, slot(rhs));
        enc.xor(t0, t0, t1);
        enc.lw(t2, Reg::SP, slot(lhs) + 4);
        enc.lw(tmp, Reg::SP, slot(rhs) + 4);
        enc.xor(t2, t2, tmp);
        enc.or(t0, t0, t2);
        match op {
            CmpOp::Eq => enc.sltiu(t0, t0, 1),
            _ => enc.sltu(t0, Reg::ZERO, t0),
        }
        enc.sw(t0, Reg::SP, slot(result));
        return Ok(());
    }
    let (a, b, hi_signed, negate) = match op {
        CmpOp::SignedLt => (lhs, rhs, true, false),
        CmpOp::SignedGe => (lhs, rhs, true, true),
        CmpOp::SignedGt => (rhs, lhs, true, false),
        CmpOp::SignedLe => (rhs, lhs, true, true),
        CmpOp::UnsignedLt => (lhs, rhs, false, false),
        CmpOp::UnsignedGe => (lhs, rhs, false, true),
        CmpOp::UnsignedGt => (rhs, lhs, false, false),
        CmpOp::UnsignedLe => (rhs, lhs, false, true),
        CmpOp::Eq | CmpOp::Ne => unreachable!("handled above"),
    };
    enc.lw(hi0, Reg::SP, slot(a) + 4);
    enc.lw(hi1, Reg::SP, slot(b) + 4);
    if hi_signed {
        enc.slt(t2, hi0, hi1);
    } else {
        enc.sltu(t2, hi0, hi1);
    }
    enc.lw(t0, Reg::SP, slot(a));
    enc.lw(t1, Reg::SP, slot(b));
    enc.sltu(tmp, t0, t1);
    enc.xor(hi0, hi0, hi1);
    enc.sltiu(hi0, hi0, 1);
    enc.and(tmp, tmp, hi0);
    enc.or(t2, t2, tmp);
    if negate {
        enc.xori(t2, t2, 1);
    }
    enc.sw(t2, Reg::SP, slot(result));
    Ok(())
}

/// Materializes `dest = (lhs <op> rhs) ? 1 : 0` from the `slt`/`sltu` set-less-than primitives.
fn materialize_compare(enc: &mut Encoder, dest: Reg, lhs: Reg, rhs: Reg, op: CmpOp) {
    match op {
        CmpOp::SignedLt => enc.slt(dest, lhs, rhs),
        CmpOp::SignedGt => enc.slt(dest, rhs, lhs),
        CmpOp::UnsignedLt => enc.sltu(dest, lhs, rhs),
        CmpOp::UnsignedGt => enc.sltu(dest, rhs, lhs),
        CmpOp::SignedGe => {
            enc.slt(dest, lhs, rhs);
            enc.xori(dest, dest, 1);
        }
        CmpOp::SignedLe => {
            enc.slt(dest, rhs, lhs);
            enc.xori(dest, dest, 1);
        }
        CmpOp::UnsignedGe => {
            enc.sltu(dest, lhs, rhs);
            enc.xori(dest, dest, 1);
        }
        CmpOp::UnsignedLe => {
            enc.sltu(dest, rhs, lhs);
            enc.xori(dest, dest, 1);
        }
        CmpOp::Eq => {
            enc.sub(dest, lhs, rhs);
            enc.sltiu(dest, dest, 1);
        }
        CmpOp::Ne => {
            enc.sub(dest, lhs, rhs);
            enc.sltu(dest, Reg::ZERO, dest);
        }
    }
}

/// Emits `dest = convert(src)` for a sub-word integer conversion or a single-word reinterpret. The
/// sub-word forms narrow to the low 8/16 bits and re-extend to the 32-bit stack width, signed
/// (`slli`/`srai` shift pair) or unsigned (`andi` mask / `slli`+`srli`); the reference/integer
/// reinterprets are a no-op move (both are one machine word). `dest` and `src` may be the same
/// register (the spilled path converts in `t0`). Float conversions need the soft-float helpers
/// (a `CallNative` boundary) and are not lowered yet.
fn emit_convert(enc: &mut Encoder, dest: Reg, src: Reg, kind: ConvKind) -> Result<(), LowerError> {
    match kind {
        ConvKind::SignExtend8 => {
            enc.slli(dest, src, 24);
            enc.srai(dest, dest, 24);
        }
        ConvKind::ZeroExtend8 => enc.andi(dest, src, 0xff),
        ConvKind::SignExtend16 => {
            enc.slli(dest, src, 16);
            enc.srai(dest, dest, 16);
        }
        ConvKind::ZeroExtend16 => {
            enc.slli(dest, src, 16);
            enc.srli(dest, dest, 16);
        }
        ConvKind::RefToInt | ConvKind::IntToRef => {
            if dest != src {
                enc.mv(dest, src);
            }
        }
        ConvKind::Float32ToInt
        | ConvKind::IntToFloat32
        | ConvKind::Float64ToInt
        | ConvKind::IntToFloat64
        | ConvKind::LongToFloat64
        | ConvKind::Float32ToFloat64
        | ConvKind::Float64ToFloat32
        | ConvKind::LongToFloat32
        | ConvKind::UIntToFloat64
        | ConvKind::ULongToFloat64 => return Err(LowerError::Unsupported),
    }
    Ok(())
}

/// Whether `value` is a pointer -- a heap ObjectRef or a managed pointer (`this` / `&field`). The
/// register-only path can dereference only a pointer base; the all-spilled path additionally handles
/// a value-type base living in its own multi-word stack slot.
fn is_pointer(value_types: &[MirType], value: ValueId) -> bool {
    matches!(
        value_types.get(value.index()),
        Some(MirType::ObjectRef | MirType::ManagedPtr)
    )
}

/// The vtable of the type `handle`, as one entry per slot: a module function index (`Some`) or
/// `None` for an inherited referenced-assembly implementation the flat path cannot resolve. Empty
/// when the type has no descriptor or no virtual methods (a plain, non-dispatched object).
fn descriptor_vtable(
    descriptors: &[TypeMeta],
    handle: TypeHandle,
    externs: &mut Vec<alloc::string::String>,
) -> Vec<Option<u32>> {
    descriptors
        .iter()
        .find(|d| d.handle == handle)
        .map_or_else(Vec::new, |meta| {
            meta.vtable
                .iter()
                .map(|entry| match entry {
                    VtableEntry::Func(index) => Some(*index),
                    VtableEntry::Extern(symbol) => {
                        Some(EXTERN_SYMBOL_FLAG | intern_extern(externs, symbol))
                    }
                })
                .collect()
        })
}

/// Emits the module's deduplicated type descriptors after all function code, one per handle
/// (`type_descs[i]` pairs with `type_desc_labels[i]`). The vtable is laid BEFORE each descriptor (slot
/// `k` at `desc - 4 - 4k`), the words next (`desc.label` binds the words start = obj-4 + the dispatch
/// base), and the itable after (`[count, (tag, method)...]`).
///
/// On the FLAT path each vtable/itable slot is an in-image `entry - desc` diff (`emit_word_diff`,
/// resolved at finish, local functions only). On the OBJECT path each is an `R_LAMELLA_REL_DESC`
/// relocation whose target is a function index, an `EXTERN_SYMBOL_FLAG`-tagged inherited library virtual,
/// or (base_ptr@12) a `DESC_SYMBOL_FLAG`-tagged base handle -- so a slot can point across the link and
/// survive `--gc-sections`; each descriptor also becomes a `__lamella_typedesc_<handle>` symbol. The
/// per-slot addend pins the slot to its descriptor so the linker's `S + A - P` reduces to `entry - desc`.
fn emit_descriptors(
    enc: &mut Encoder,
    type_descs: &[DescEmit],
    type_desc_labels: &[(TypeHandle, Label)],
    func_labels: &[Label],
    descriptors: &[TypeMeta],
    relocate: bool,
    desc_relocs: &mut Vec<DescReloc>,
) -> Vec<DescSym> {
    let mut desc_syms: Vec<DescSym> = Vec::new();
    for (desc, (handle, _)) in type_descs.iter().zip(type_desc_labels) {
        let vtable_start = enc.position();
        for (k, slot) in desc.vtable.iter().enumerate().rev() {
            match slot {
                Some(target) if relocate => {
                    desc_relocs.push((enc.position(), *target, -(4 + 4 * k as i32)));
                    enc.emit_word(0);
                }
                Some(target) => enc.emit_word_diff(desc.label, func_labels[*target as usize]),
                None => enc.emit_word(0),
            }
        }
        enc.bind_label(desc.label);
        let words_bytes = desc.words.len() as i32 * 4;
        for (idx, &w) in desc.words.iter().enumerate() {
            if idx == 3 {
                let base_in_object = desc
                    .base
                    .filter(|b| type_desc_labels.iter().any(|(h, _)| h == b));
                match (base_in_object, desc.base) {
                    (Some(base), _) if relocate => {
                        desc_relocs.push((enc.position(), DESC_SYMBOL_FLAG | base.0, 12));
                        enc.emit_word(0);
                    }
                    (Some(base), _) => {
                        let label = type_desc_labels
                            .iter()
                            .find(|(h, _)| *h == base)
                            .map(|(_, l)| *l)
                            .expect("base descriptor present");
                        enc.emit_word_diff(desc.label, label);
                    }
                    (None, Some(base))
                        if relocate
                            && crate::resolver::reference_handle_parts(base).is_some() =>
                    {
                        let vtable_bytes = descriptors
                            .iter()
                            .find(|m| m.handle == base)
                            .map_or(0, |m| m.vtable.len() as i32 * 4);
                        desc_relocs.push((
                            enc.position(),
                            DESC_SYMBOL_FLAG | base.0,
                            vtable_bytes + 12,
                        ));
                        enc.emit_word(0);
                    }
                    _ => enc.emit_word(0),
                }
            } else {
                enc.emit_word(w);
            }
        }
        enc.emit_word(desc.itable.len() as u32);
        for (i, (tag, method)) in desc.itable.iter().enumerate() {
            enc.emit_word(*tag);
            match method {
                Some(target) if relocate => {
                    desc_relocs.push((enc.position(), *target, words_bytes + 8 + 8 * i as i32));
                    enc.emit_word(0);
                }
                Some(target) => enc.emit_word_diff(desc.label, func_labels[*target as usize]),
                None => enc.emit_word(0),
            }
        }
        desc_syms.push((
            handle.0,
            vtable_start,
            enc.position() - vtable_start,
            desc.vtable.len() as u32 * 4,
        ));
    }
    desc_syms
}

/// Materializes `dst = &static` for a `ldsfld`/`stsfld` slot. Flat path: the fixed
/// `STATIC_FIELD_BASE + offset` (the layout is final, and the emission is byte-identical to the
/// pre-region form). Object path (`relocate`): an indirection pool word -- `region - word` as an
/// exact-target `R_LAMELLA_REL_DESC` against the assembly's OWN `__lamella_statics_<hash>`
/// region symbol (the field's byte offset rides as the reloc addend), or the shared
/// `__lamella_eh_tag` for the reserved word 0 -- reconstituted `la dst, word; lw tmp; add`, so
/// the linker's RAM-window placement and any `--gc-sections` re-layout stay authoritative.
/// A REFERENCE-owned static (`StaticOwner::Reference`) addresses the OWNER's region -- the same
/// `__lamella_statics_<ownerhash>` symbol the owner's own object defines, at the owner's dense
/// slot -- so one placed region serves both sides of the link. It exists only on the object path;
/// the flat path has no second region to name, so it rejects there.
/// `tmp` must differ from `dst`; both are caller-scratch at the call sites.
fn emit_static_addr(
    enc: &mut Encoder,
    dst: Reg,
    tmp: Reg,
    owner: &StaticOwner,
    offset: u32,
    statics_ptr_pool: &mut Vec<((u32, i32), Label)>,
    relocate: bool,
) -> Result<(), LowerError> {
    if !relocate {
        return match owner {
            StaticOwner::Own => {
                enc.li(dst, (STATIC_FIELD_BASE + offset) as i32);
                Ok(())
            }
            StaticOwner::Reference(_) => Err(LowerError::Unsupported),
        };
    }
    let key = match owner {
        StaticOwner::Own if offset == crate::cil::G_EXCEPTION_TAG_OFFSET => {
            (EH_TAG_SYMBOL_FLAG, 0i32)
        }
        StaticOwner::Own => (STATICS_BASE_SYMBOL_FLAG, offset as i32),
        StaticOwner::Reference(ordinal) => {
            assert!(
                *ordinal < STATICS_MAX_REFERENCES,
                "statics symbol out of encoding range (reference ordinal {ordinal})"
            );
            (
                STATICS_BASE_SYMBOL_FLAG | (u32::from(*ordinal) + 1),
                offset as i32,
            )
        }
    };
    let word = match statics_ptr_pool.iter().find(|(k, _)| *k == key) {
        Some((_, l)) => *l,
        None => {
            let l = enc.new_label();
            statics_ptr_pool.push((key, l));
            l
        }
    };
    enc.la(dst, word);
    enc.lw(tmp, dst, 0);
    enc.add(dst, dst, tmp);
    Ok(())
}

/// Emits `dst = &TypeDesc` (the canonical descriptor's WORDS address -- the value obj-4 holds and
/// `CallVirtual`/`castclass` compare) for `handle`, choosing a `--gc-sections`-robust reference.
///
/// A bare `la dst, <descriptor>` names the module-level descriptor via a PC-relative pair the encoder
/// resolves IN PLACE -- correct on the flat path and a non-collecting link, but it emits NO relocation,
/// so `garbage_collect` cannot see the allocating-function -> descriptor edge: it would drop the
/// descriptor, and the baked `la` would then address whatever re-layout left there. So on the OBJECT
/// path (`relocate`) the reference is INDIRECT through a per-function pool word: the word holds `words -
/// word` as an `R_LAMELLA_REL_DESC` (a relocation `garbage_collect` DOES follow to keep the descriptor,
/// and which the linker re-applies under re-layout), and `la dst,word; lw tmp,0(dst); add dst,dst,tmp`
/// reconstitutes the absolute words address position-independently. The pool word rides INSIDE this
/// function's code (deduplicated by handle), so its own local `la` survives re-layout the way a string
/// literal's does. `tmp` must differ from `dst`; both are caller-scratch at the call sites.
fn emit_desc_words_addr(
    enc: &mut Encoder,
    dst: Reg,
    tmp: Reg,
    handle: TypeHandle,
    desc_label: Label,
    desc_ptr_pool: &mut Vec<(TypeHandle, Label)>,
    relocate: bool,
) {
    if !relocate {
        enc.la(dst, desc_label);
        return;
    }
    let word = match desc_ptr_pool.iter().find(|(h, _)| *h == handle) {
        Some((_, l)) => *l,
        None => {
            let l = enc.new_label();
            desc_ptr_pool.push((handle, l));
            l
        }
    };
    enc.la(dst, word);
    enc.lw(tmp, dst, 0);
    enc.add(dst, dst, tmp);
}

/// Finds or creates the canonical descriptor label for `handle`, deduplicated so an `Alloc` and a
/// `TypeDescAddr` of the same type reference ONE descriptor (a type-identity compare is by address).
/// A freshly created descriptor carries the fixed header + the type's vtable/itable/tag from
/// `descriptors`; a cast-target-only type (never allocated) passes `payload_size = 0` and no
/// ref_offsets. The emission loop lays each entry's vtable before and itable after.
#[allow(clippy::too_many_arguments)]
fn descriptor_label(
    enc: &mut Encoder,
    handle: TypeHandle,
    descriptors: &[TypeMeta],
    payload_size: u32,
    ref_offsets: &[u32],
    type_descs: &mut TypeDescs,
    type_desc_labels: &mut Vec<(TypeHandle, Label)>,
    externs: &mut Vec<alloc::string::String>,
) -> Label {
    if let Some((_, label)) = type_desc_labels.iter().find(|(h, _)| *h == handle) {
        return *label;
    }
    let label = enc.new_label();
    let mut words = Vec::with_capacity(DESC_HEADER_WORDS as usize + ref_offsets.len());
    words.push(payload_size);
    words.push(ref_offsets.len() as u32);
    words.push(descriptor_tag(descriptors, handle));
    words.push(0);
    words.extend_from_slice(ref_offsets);
    type_descs.push(DescEmit {
        label,
        vtable: descriptor_vtable(descriptors, handle, externs),
        words,
        itable: descriptor_itable(descriptors, handle, externs),
        base: descriptors
            .iter()
            .find(|d| d.handle == handle)
            .and_then(|d| d.base),
    });
    type_desc_labels.push((handle, label));
    label
}

/// The itable of the type `handle` -- `(interface-method tag, implementation)` per entry, laid
/// after the descriptor for `CallInterface` to search. Empty when the type implements no interfaces.
/// Like [`descriptor_vtable`], a referenced-assembly implementation becomes an `EXTERN_SYMBOL_FLAG`-
/// tagged interned extern the linker resolves against the owning library's export -- so an interface
/// call on a PROGRAM-allocated library object dispatches ACROSS the link into the library's method.
fn descriptor_itable(
    descriptors: &[TypeMeta],
    handle: TypeHandle,
    externs: &mut Vec<alloc::string::String>,
) -> Vec<(u32, Option<u32>)> {
    descriptors
        .iter()
        .find(|d| d.handle == handle)
        .map_or_else(Vec::new, |meta| {
            meta.itable
                .iter()
                .map(|(tag, impl_)| {
                    let target = match impl_ {
                        VtableEntry::Func(index) => *index,
                        VtableEntry::Extern(symbol) => {
                            EXTERN_SYMBOL_FLAG | intern_extern(externs, symbol)
                        }
                    };
                    (*tag, Some(target))
                })
                .collect()
        })
}

/// The FNV identity tag of the type `handle` (`0` when it has no descriptor), stored in the fixed
/// header at word 2 for a mixed-mode / type-identity compare.
fn descriptor_tag(descriptors: &[TypeMeta], handle: TypeHandle) -> u32 {
    descriptors
        .iter()
        .find(|d| d.handle == handle)
        .map_or(0, |meta| meta.type_tag)
}

/// Whether `value` is an `int64` -- lowered as a register/slot PAIR (lo word at the slot, hi word at
/// slot+4) on the all-spilled path.
fn is_i64(value_types: &[MirType], value: ValueId) -> bool {
    matches!(value_types.get(value.index()), Some(MirType::I64))
}

/// The whole-word count and sub-word tail (`(size / 4, size % 4)`) of `value`'s value type, or
/// `None` if it is not a value type. Used for a struct-valued field copy, which moves only the
/// struct's own bytes -- not slot padding, which could clobber an adjacent field when the base is
/// a heap object -- and moves the 1..3-byte tail of a non-word-multiple struct WIDTH-EXACT: a
/// `size / 4`-only count is zero for a sub-word struct, silently dropping its payload (the boxed
/// 1-byte `PinValue` bug the ARM backend fixed first), and a word-wide tail copy would touch
/// whatever packs after the struct.
fn value_type_copy_extent(value_types: &[MirType], value: ValueId) -> Option<(u32, u32)> {
    match value_types.get(value.index()) {
        Some(MirType::ValueType { size, .. }) => Some((size / 4, size % 4)),
        _ => None,
    }
}

/// The number of 4-byte words a value occupies in the argument/return registers: 2 for an int64 or a
/// small (<= 8-byte) value type -- a register PAIR -- and 1 for a scalar. A value type's count rounds
/// UP (`div_ceil`): a 1..3-byte struct still occupies one register and a 5..7-byte one a pair -- the
/// old `size / 4` marshalled a sub-word struct ARGUMENT as ZERO words (both sides skip it, so the
/// callee read a garbage slot) and dropped the tail word of a 5..7-byte one; 9..11 bytes now round to
/// 3 = the by-reference path instead of a tail-dropping pair. The word-padded slots make the extra
/// bytes well-defined on both sides. A value type wider than 2 words is passed/returned by reference
/// (its single pointer register), so callers test `> 2` to take that path.
fn value_words(value_types: &[MirType], value: ValueId) -> u32 {
    match value_types.get(value.index()) {
        Some(MirType::I64 | MirType::F64) => 2,
        Some(MirType::ValueType { size, .. }) => size.div_ceil(4),
        _ => 1,
    }
}

/// Marshals a call's arguments from their slots into the argument registers, starting at register index
/// `first` (0 for a static call, 1 when a0 already holds a receiver). Each value takes consecutive
/// registers per [`value_words`] -- an int64 / small value type spans a pair. Arguments PAST the profile's
/// registers (a0-a7 on RV32IM, a0-a5 on RV32E) spill to the STACK: the caller reserves an outgoing-args
/// area at the BOTTOM of its spilled frame (`sp+0..`, sized by [`call_stack_words`]), so each overflow word
/// lands at `sp + 4*k` -- exactly where the callee reads it at `sp + its_frame + 4*k`. Loaded through the
/// spilled scratch register (the arguments live in memory slots, so no register-source hazard).
fn marshal_call_args(
    enc: &mut Encoder,
    slot: &impl Fn(ValueId) -> i32,
    value_types: &[MirType],
    args: &[ValueId],
    first: usize,
    profile: RiscvProfile,
) -> Result<(), LowerError> {
    let regs = arg_reg_count(profile);
    let scratch = spilled_scratch(profile);
    let mut reg = first;
    let mut stack_word = 0i32;
    for &arg in args {
        let words = value_words(value_types, arg);
        if words > 2 {
            if reg < regs {
                let target = arg_reg(reg).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.addi(target, Reg::SP, slot(arg));
                reg += 1;
            } else {
                enc.addi(scratch, Reg::SP, slot(arg));
                enc.sw(scratch, Reg::SP, stack_word * 4);
                stack_word += 1;
            }
            continue;
        }
        for w in 0..words {
            if reg < regs {
                let target = arg_reg(reg).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(target, Reg::SP, slot(arg) + w as i32 * 4);
                reg += 1;
            } else {
                enc.lw(scratch, Reg::SP, slot(arg) + w as i32 * 4);
                enc.sw(scratch, Reg::SP, stack_word * 4);
                stack_word += 1;
            }
        }
    }
    Ok(())
}

/// Stores a call's result from the return registers a0(:a1) into its slot -- a register pair (two words)
/// for an int64 / small value type, one word otherwise.
fn store_call_result(
    enc: &mut Encoder,
    slot: &impl Fn(ValueId) -> i32,
    value_types: &[MirType],
    result: ValueId,
) {
    enc.sw(Reg::A0, Reg::SP, slot(result));
    if value_words(value_types, result) >= 2 {
        enc.sw(Reg::A1, Reg::SP, slot(result) + 4);
    }
}

/// For a call whose result is a value type wider than two words (returned by reference / sret): emits
/// `a0 = &result` so the callee writes the value into the result slot, and returns 1 so the explicit
/// arguments marshal from a1. Returns 0 for a register-returned result -- nothing emitted, and the caller
/// stores the returned a0(:a1) with [`store_call_result`] instead.
fn emit_sret_arg(
    enc: &mut Encoder,
    slot: &impl Fn(ValueId) -> i32,
    value_types: &[MirType],
    result: ValueId,
) -> usize {
    if value_words(value_types, result) > 2 {
        enc.addi(Reg::A0, Reg::SP, slot(result));
        1
    } else {
        0
    }
}

/// The padded slot word count of `value` (`stack_slot_bytes / 4`) -- the whole value-type slot,
/// used to zero (`initobj`) or copy (`ldobj`/`stobj`) a struct local including any tail padding.
fn slot_words(value_types: &[MirType], value: ValueId) -> u32 {
    value_types
        .get(value.index())
        .map_or(0, |t| t.stack_slot_bytes() / 4)
}

/// Converts a field/element byte offset to the signed 12-bit immediate RISC-V `lw`/`sw`/`addi`
/// take, or `Unsupported` if it does not fit. This backend does not materialize a large offset into
/// the base register yet -- every struct/array layout it lowers is well within the 2 KiB reach.
fn field_offset(offset: u32) -> Result<i32, LowerError> {
    if offset <= 2047 {
        Ok(offset as i32)
    } else {
        Err(LowerError::Unsupported)
    }
}

/// Emits the bounds check and element-address computation for array access: traps (`ebreak`) unless
/// `index < length` (the length at `[array+0]`, compared UNSIGNED so a negative index -- a huge
/// unsigned value -- traps too, matching IndexOutOfRangeException's effect), then leaves
/// `array + index*element_size` in [`scratch`] so the caller's access at offset 4 hits the element
/// past the length word. The `[u32 length]` prefix is one word regardless of element size, so the +4
/// is element-size-independent. A power-of-two size scales with a shift; any other with a multiply.
fn emit_element_address(
    enc: &mut Encoder,
    array: Reg,
    index: Reg,
    element_size: u32,
    profile: RiscvProfile,
) {
    let s = spilled_scratch(profile);
    enc.lw(s, array, 0);
    let ok = enc.new_label();
    enc.branch(BranchCond::LtU, index, s, ok);
    enc.ebreak();
    enc.bind_label(ok);
    if element_size.is_power_of_two() {
        enc.slli(s, index, element_size.trailing_zeros());
    } else {
        enc.li(s, element_size as i32);
        enc.mul(s, index, s);
    }
    enc.add(s, array, s);
}

/// Emits the two per-dimension bounds checks and element-address computation for a 2-D rectangular
/// array `[u32 dim0][u32 dim1][elements...]`: traps unless `index0 < dim0` (`[array+0]`) and
/// `index1 < dim1` (`[array+4]`), both UNSIGNED (a negative index traps too), then leaves
/// `array + 8 + (index0*dim1 + index1)*element_size` in [`scratch`] so the caller accesses the
/// element at offset 0. `array`/`index0`/`index1` are read-only; `ta`/`tb` are caller-supplied temps
/// distinct from those and from `t6` (the register path passes t0/t1, the spilled path a0/a1).
#[allow(clippy::too_many_arguments)]
fn emit_2d_element_address(
    enc: &mut Encoder,
    array: Reg,
    index0: Reg,
    index1: Reg,
    element_size: u32,
    ta: Reg,
    tb: Reg,
    profile: RiscvProfile,
) {
    let s = spilled_scratch(profile);
    enc.lw(ta, array, 0);
    let ok0 = enc.new_label();
    enc.branch(BranchCond::LtU, index0, ta, ok0);
    enc.ebreak();
    enc.bind_label(ok0);
    enc.lw(tb, array, 4);
    let ok1 = enc.new_label();
    enc.branch(BranchCond::LtU, index1, tb, ok1);
    enc.ebreak();
    enc.bind_label(ok1);
    emit_soft_mul32(enc, s, index0, tb, ta, profile);
    enc.add(s, s, index1);
    if element_size.is_power_of_two() {
        enc.slli(s, s, element_size.trailing_zeros());
    } else {
        enc.li(ta, element_size as i32);
        enc.mul(s, s, ta);
    }
    enc.add(s, array, s);
    enc.addi(s, s, 8);
}

/// Emits the rank-N (row-major) element address into `out`, with a per-dimension bounds check (each
/// `indices[k] < dim_k`, `dim_k` read from `[base + 4*k]`; an out-of-range index traps). The flat index is
/// the Horner fold `((...(i0*dim1 + i1)*dim2 + i2)...) + i(N-1)`, then `* element_size`, then `+ base +
/// 4*N` (past the N dimension words). `base` is preserved; `tmp`/`tmp2` are scratch. RV32IM only (uses
/// `mul`); `base`/`out`/`tmp`/`tmp2` must be four distinct registers.
#[allow(clippy::too_many_arguments)]
fn emit_md_element_address(
    enc: &mut Encoder,
    base: Reg,
    out: Reg,
    tmp: Reg,
    tmp2: Reg,
    slot: &impl Fn(ValueId) -> i32,
    indices: &[ValueId],
    element_size: u32,
) {
    let n = indices.len();
    enc.lw(out, Reg::SP, slot(indices[0]));
    enc.lw(tmp, base, 0);
    let ok = enc.new_label();
    enc.branch(BranchCond::LtU, out, tmp, ok);
    enc.ebreak();
    enc.bind_label(ok);
    for (k, &idx) in indices.iter().enumerate().skip(1) {
        enc.lw(tmp, base, 4 * k as i32);
        enc.mul(out, out, tmp);
        enc.lw(tmp2, Reg::SP, slot(idx));
        let okk = enc.new_label();
        enc.branch(BranchCond::LtU, tmp2, tmp, okk);
        enc.ebreak();
        enc.bind_label(okk);
        enc.add(out, out, tmp2);
    }
    if element_size.is_power_of_two() {
        enc.slli(out, out, element_size.trailing_zeros());
    } else {
        enc.li(tmp, element_size as i32);
        enc.mul(out, out, tmp);
    }
    enc.add(out, base, out);
    enc.addi(out, out, 4 * n as i32);
}

/// Emits a byte-copy loop (`cpblk`): copies `size` bytes from `src` to `dst`, using `t6` (the array
/// scratch, free outside an array op) as the transfer register. `dst`/`src`/`size` are scratch
/// registers the loop mutates; it is test-first, so a zero size copies nothing.
fn emit_copy_block(enc: &mut Encoder, dst: Reg, src: Reg, size: Reg, profile: RiscvProfile) {
    let s = spilled_scratch(profile);
    let body = enc.new_label();
    let done = enc.new_label();
    enc.branch(BranchCond::Eq, size, Reg::ZERO, done);
    enc.bind_label(body);
    enc.lbu(s, src, 0);
    enc.sb(s, dst, 0);
    enc.addi(dst, dst, 1);
    enc.addi(src, src, 1);
    enc.addi(size, size, -1);
    enc.branch(BranchCond::Ne, size, Reg::ZERO, body);
    enc.bind_label(done);
}

/// Emits a byte-fill loop (`initblk`): writes the low byte of `value` to `size` bytes at `dst`.
/// `dst`/`size` are scratch registers the loop mutates; `value` is read each iteration, not mutated.
fn emit_fill_block(enc: &mut Encoder, dst: Reg, value: Reg, size: Reg) {
    let body = enc.new_label();
    let done = enc.new_label();
    enc.branch(BranchCond::Eq, size, Reg::ZERO, done);
    enc.bind_label(body);
    enc.sb(value, dst, 0);
    enc.addi(dst, dst, 1);
    enc.addi(size, size, -1);
    enc.branch(BranchCond::Ne, size, Reg::ZERO, body);
    enc.bind_label(done);
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_ir::{BasicBlock, BlockId};

    #[test]
    fn rv32e_register_model_stays_within_x0_x15() {
        assert!(
            allocatable(RiscvProfile::Rv32ec).is_empty(),
            "RV32E has no callee-saved pool (x18-x27 do not exist) -> every function spills"
        );
        assert_eq!(
            spilled_scratch(RiscvProfile::Rv32ec).number(),
            9,
            "RV32E borrows s1 (x9) as scratch (RV32IM's t6/x31 does not exist)"
        );
        assert_eq!(arg_reg_count(RiscvProfile::Rv32ec), 6, "RV32E passes a0-a5");
        for i in 0..arg_reg_count(RiscvProfile::Rv32ec) {
            assert!(
                arg_reg(i).expect("RV32E argument register").number() < 16,
                "argument {i} is within x0-x15"
            );
        }
        assert_eq!(allocatable(RiscvProfile::Rv32im).len(), 12);
        assert_eq!(spilled_scratch(RiscvProfile::Rv32im).number(), 31);
    }

    #[test]
    fn rv32e_maps_int_mul_div_rem_to_soft_routines() {
        for (op, name) in [
            (BinOp::Mul, "__mulsi3"),
            (BinOp::DivSigned, "__divsi3"),
            (BinOp::DivUnsigned, "__udivsi3"),
            (BinOp::RemSigned, "__modsi3"),
            (BinOp::RemUnsigned, "__umodsi3"),
        ] {
            assert!(
                is_soft_int_binop(op),
                "{op:?} needs a soft routine on RV32E"
            );
            assert_eq!(soft_int_routine(op), name);
        }
        for op in [
            BinOp::Add,
            BinOp::Sub,
            BinOp::And,
            BinOp::Or,
            BinOp::Xor,
            BinOp::Shl,
            BinOp::ShrSigned,
            BinOp::ShrUnsigned,
        ] {
            assert!(
                !is_soft_int_binop(op),
                "{op:?} stays a native RV32I instruction"
            );
        }
    }

    /// A reference-field round-trip: store 40 at `[base+4]`, load it back, add 2, store the 42
    /// through a computed field address, and load it -- exercising FieldStore/FieldLoad/FieldAddr
    /// and raw Store/Load over a pointer base. `base` is a ConstInt typed as an ObjectRef.
    fn field_function() -> Function {
        let i32t = MirType::I32;
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![
                MirType::ObjectRef,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                MirType::ManagedPtr,
                i32t,
                i32t,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::ObjectRef,
                            value: 0x8010_0000,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 40,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::FieldStore {
                            base: ValueId(0),
                            offset: 4,
                            value: ValueId(1),
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 4,
                        },
                    ),
                    (ValueId(4), Inst::ConstInt { ty: i32t, value: 2 }),
                    (
                        ValueId(5),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(3),
                            rhs: ValueId(4),
                        },
                    ),
                    (
                        ValueId(6),
                        Inst::FieldAddr {
                            base: ValueId(0),
                            offset: 8,
                        },
                    ),
                    (
                        ValueId(7),
                        Inst::Store {
                            address: ValueId(6),
                            value: ValueId(5),
                            width: 4,
                        },
                    ),
                    (
                        ValueId(8),
                        Inst::Load {
                            address: ValueId(6),
                            width: 4,
                            signed: false,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(8)))),
            }],
        }
    }

    #[test]
    fn lowers_reference_field_access() {
        let func = field_function();
        assert!(lamella_ir::verify(&func).is_ok());
        let code = lower(&func).expect("field access lowers to RV32IM");
        assert!(!code.is_empty());
    }

    #[test]
    fn object_statics_ride_the_region_symbol() {
        let i32t = MirType::I32;
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 42,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::StaticStore {
                            owner: StaticOwner::Own,
                            offset: 4,
                            value: ValueId(0),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let statics = AssemblyStatics {
            suffix: "deadbeef".into(),
            region_bytes: 8,
            roots: Vec::new(),
        };
        let obj = lower_object_profile_statics(
            &[func],
            &["f0"],
            &[],
            &[],
            Some(&statics),
            RiscvProfile::Rv32im,
        )
        .expect("a statics-bearing function lowers on the object path");
        let parsed = lamella_elf::read_object(&obj).expect("read the built object");
        let (index, region) = parsed
            .symbols
            .iter()
            .enumerate()
            .find(|(_, s)| s.name == "__lamella_statics_deadbeef")
            .expect("the region symbol is referenced");
        assert!(!region.defined, "the region is UNDEFINED -- the linker places it");
        assert_eq!(region.size, 8, "the sized reference carries region_bytes");
        assert!(
            parsed
                .relocations
                .iter()
                .any(|r| r.symbol == index as u32 && r.addend == 4),
            "a pool-word relocation addresses the region at the field's byte offset"
        );
    }

    #[test]
    fn a_reference_owned_static_names_the_owners_region() {
        let i32t = MirType::I32;
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::StaticLoad {
                        owner: StaticOwner::Reference(0),
                        offset: 8,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let statics = AssemblyStatics {
            suffix: "0badf00d".into(),
            region_bytes: 4,
            roots: Vec::new(),
        };
        let obj = lower_object_profile_statics_references(
            &[func],
            &["f0"],
            &[],
            &[],
            Some(&statics),
            &["__lamella_statics_beefcafe"],
            &DescQualifiers::default(),
            RiscvProfile::Rv32im,
        )
        .expect("a reference-owned static lowers");
        let parsed = lamella_elf::read_object(&obj).expect("read the built object");
        let owner = parsed
            .symbols
            .iter()
            .enumerate()
            .find(|(_, s)| s.name == "__lamella_statics_beefcafe")
            .expect("the owner's region is referenced");
        assert!(!owner.1.defined && owner.1.size == 0, "the owner region is undefined, size 0");
        assert!(
            parsed
                .relocations
                .iter()
                .any(|r| r.symbol == owner.0 as u32 && r.addend == 8),
            "the pool word addresses the owner's region at the field's byte offset"
        );
        assert!(
            !parsed.symbols.iter().any(|s| s.name == "__lamella_statics_0badf00d"),
            "this assembly's own region is not referenced when it has no own-static access"
        );
    }

    /// Decodes one encoded stack-map record -- the test-side mirror of the walker's parse (the
    /// ARM tests carry the same helper; the byte format is `crate::stackmaps`' shared layout).
    fn decode_stackmap_record(bytes: &[u8]) -> (u32, u32, u16, u16, u16, Vec<u16>) {
        let word =
            |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        let half = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let count = half(14) as usize;
        let roots = (0..count).map(|i| half(16 + 2 * i)).collect();
        (word(0), word(4), half(8), half(10), half(12), roots)
    }

    /// A leaf callee for the record fixtures: no safepoint, so it must get NO record.
    fn record_leaf() -> Function {
        Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: 7,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        }
    }

    #[test]
    fn lower_object_emits_method_records_with_function_relocations() {
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::Alloc {
                            handle: TypeHandle(0),
                            payload_size: 4,
                            ref_offsets: Vec::new().into_boxed_slice(),
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::Call {
                            callee: 1,
                            args: Vec::new(),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let obj = lamella_elf::read_object(
            &lower_object(&[main, record_leaf()], &["main", "g"], &[], &[]).unwrap(),
        )
        .unwrap();
        let rec = obj
            .symbols
            .iter()
            .find(|s| s.name == "__lamella_smrec_main")
            .expect("main gets a method record");
        assert!(rec.defined && rec.binding == lamella_elf::Binding::Weak);
        assert!(
            !obj.symbols.iter().any(|s| s.name == "__lamella_smrec_g"),
            "a leaf (no safepoint) gets no record -- it can never appear mid-walk"
        );
        let bytes = &obj.text[rec.value as usize..(rec.value + rec.size) as usize];
        let (_, code_size, mode, frame_words, ret_ra_word, roots) = decode_stackmap_record(bytes);
        assert_eq!(mode, STACKMAP_MODE_METHOD_SLOTS);
        assert!(code_size > 0);
        assert_eq!(frame_words, 4);
        assert_eq!(ret_ra_word, 2, "ra is saved just past the two value slots");
        assert_eq!(roots, vec![STACKMAP_KIND_OBJECT_REF << 14]);
        let main_index = obj
            .symbols
            .iter()
            .position(|s| s.name == "main")
            .expect("main symbol") as u32;
        assert!(
            obj.relocations.iter().any(|r| r.offset == rec.value
                && r.symbol == main_index
                && r.kind == lamella_elf::riscv::R_RISCV_32),
            "the func_addr word carries an R_RISCV_32 to the function symbol"
        );
        let mut z = Encoder::new();
        z.sw(Reg::ZERO, Reg::SP, 0);
        let zero_store = z.finish().unwrap().bytes;
        assert!(
            obj.text.windows(zero_store.len()).any(|w| w == &zero_store[..]),
            "the spilled prologue zeroes the never-written ref slot"
        );
    }

    /// The MEMORY-HOMING gate, pinned: an ObjectRef LIVE ACROSS a call must take the all-spilled
    /// path (its record then enumerates the slot); the SAME shape with the ref DEAD across the
    /// call keeps the register path and gets a HOP-ONLY record (no roots, `ra` at word 0). A ref
    /// surviving a safepoint in a callee-saved register would be invisible to the collector --
    /// this is the tripwire should `any_ref_live_across_safepoint` routing ever loosen.
    #[test]
    fn a_ref_live_across_a_call_takes_the_spilled_path() {
        let build = |live_across: bool| {
            let insts = if live_across {
                vec![
                    (
                        ValueId(1),
                        Inst::Call {
                            callee: 1,
                            args: Vec::new(),
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::RefToInt,
                        },
                    ),
                ]
            } else {
                vec![
                    (
                        ValueId(2),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::RefToInt,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::Call {
                            callee: 1,
                            args: Vec::new(),
                        },
                    ),
                ]
            };
            let main = Function {
                params: vec![MirType::ObjectRef],
                ret: Some(MirType::I32),
                value_types: vec![MirType::ObjectRef, MirType::I32, MirType::I32],
                entry: BlockId(0),
                blocks: vec![BasicBlock {
                    params: vec![ValueId(0)],
                    insts,
                    terminator: Some(Terminator::Return(Some(ValueId(2)))),
                }],
            };
            assert!(lamella_ir::verify(&main).is_ok());
            let obj = lamella_elf::read_object(
                &lower_object(&[main, record_leaf()], &["main", "g"], &[], &[]).unwrap(),
            )
            .unwrap();
            let rec = obj
                .symbols
                .iter()
                .find(|s| s.name == "__lamella_smrec_main")
                .expect("a call-bearing function gets a record")
                .clone();
            let bytes = &obj.text[rec.value as usize..(rec.value + rec.size) as usize];
            decode_stackmap_record(bytes)
        };
        let (_, _, _, _, ret_ra_word, roots) = build(true);
        assert_eq!(
            roots,
            vec![STACKMAP_KIND_OBJECT_REF << 14],
            "live-across: the spilled path homes the ref in slot 0 and the record names it"
        );
        assert_eq!(ret_ra_word, 3, "spilled: ra sits past the three value slots");
        let (_, _, _, _, ret_ra_word, roots) = build(false);
        assert!(
            roots.is_empty(),
            "dead-across: the register path's record is hop-only (no live ref is observable)"
        );
        assert_eq!(ret_ra_word, 0, "register path: ra is the first word stored");
    }

    /// WELD: every live root the target-agnostic per-safepoint analysis reports must appear among
    /// the METHOD_SLOTS record's slots at the same offset -- the record enumerates a superset
    /// (all ref slots), and this keeps `method_record_roots` from drifting from
    /// `regalloc::safepoint_roots` (the ARM twin's obligation).
    #[test]
    fn method_record_roots_cover_every_per_site_live_root() {
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef, MirType::I32, MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::Alloc {
                            handle: TypeHandle(0),
                            payload_size: 4,
                            ref_offsets: Vec::new().into_boxed_slice(),
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::Call {
                            callee: 1,
                            args: Vec::new(),
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Alloc {
                            handle: TypeHandle(0),
                            payload_size: 4,
                            ref_offsets: Vec::new().into_boxed_slice(),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let (offsets, _) = spilled_slot_offsets(&func, 0);
        let record_slots: Vec<i32> = method_record_roots(&func, &[], &offsets)
            .iter()
            .map(|r| i32::from(r & 0x3FFF) * 4)
            .collect();
        let per_site = crate::regalloc::safepoint_roots(&func, &func.value_types);
        let mut live_sites = 0;
        for block_roots in &per_site {
            for roots in block_roots.iter().flatten() {
                for v in roots {
                    live_sites += 1;
                    assert!(
                        record_slots.contains(&offsets[v.index()]),
                        "live root v{} (slot {}) missing from the method record {record_slots:?}",
                        v.index(),
                        offsets[v.index()]
                    );
                }
            }
        }
        assert!(live_sites > 0, "the fixture must exercise live roots");
    }

    #[test]
    fn a_seam_shaped_function_pins_its_reftoint_source() {
        let seam = Function {
            params: vec![MirType::ObjectRef],
            ret: Some(MirType::I32),
            value_types: vec![MirType::ObjectRef, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![
                    (
                        ValueId(1),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::RefToInt,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::CallNative {
                            symbol: 0,
                            args: vec![ValueId(1)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let (offsets, _) = spilled_slot_offsets(&seam, 0);
        let externs = [alloc::string::String::from("lamella_thread_recv_poll")];
        let roots = method_record_roots(&seam, &externs, &offsets);
        assert_eq!(roots, vec![STACKMAP_KIND_PINNED << 14]);
        let externs = [alloc::string::String::from("lamella_console_write")];
        let roots = method_record_roots(&seam, &externs, &offsets);
        assert_eq!(roots, vec![STACKMAP_KIND_OBJECT_REF << 14]);
    }

    #[test]
    fn statics_rows_emit_the_mode2_record() {
        let statics = AssemblyStatics {
            suffix: alloc::string::String::from("0badf00d"),
            region_bytes: 12,
            roots: vec![
                STACKMAP_KIND_MANAGED_PTR << 14,
                2 | (STACKMAP_KIND_OBJECT_REF << 14),
            ],
        };
        let obj = lamella_elf::read_object(
            &lower_object_profile_statics(
                &[record_leaf()],
                &["main"],
                &[],
                &[],
                Some(&statics),
                RiscvProfile::Rv32im,
            )
            .unwrap(),
        )
        .unwrap();
        let rec = obj
            .symbols
            .iter()
            .find(|s| s.name == "__lamella_smstat_0badf00d")
            .expect("the statics record is emitted under the assembly's suffix");
        let bytes = &obj.text[rec.value as usize..(rec.value + rec.size) as usize];
        let (base, region, mode, _, _, roots) = decode_stackmap_record(bytes);
        assert_eq!(mode, STACKMAP_MODE_STATICS);
        assert_eq!(base, 0, "the base word is emitted 0 and patched by relocation");
        assert_eq!(region, 12);
        assert_eq!(roots, statics.roots);
        let reloc = obj
            .relocations
            .iter()
            .find(|r| r.offset == rec.value)
            .expect("the record's base word carries a relocation");
        let target = &obj.symbols[reloc.symbol as usize];
        assert_eq!(target.name, "__lamella_statics_0badf00d");
        assert!(!target.defined, "the region is linker-placed");
        assert_eq!(target.size, 12, "the sized reference drives the RAM layout");
    }

    /// The N-reference descriptor-identity scheme on riscv (the ARM slice-2 twin, same shared
    /// `descriptor_symbol`): a LIBRARY's own descriptor and a PROGRAM's reference-owned emission
    /// of the SAME type produce ONE symbol (`__lamella_typedesc_<ownerhash>_<token>`, weak in the
    /// library, strong in the program -- the dedupe that preserves identity-by-address).
    #[test]
    fn descriptor_symbols_qualify_by_owner_across_the_link() {
        let alloc_fn = |handle: u32| Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: TypeHandle(handle),
                        payload_size: 4,
                        ref_offsets: Vec::new().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let lib_qualifiers = DescQualifiers {
            own: Some("0bd4d82a".into()),
            references: Vec::new(),
        };
        let lib = lamella_elf::read_object(
            &lower_object_library_statics(
                &[alloc_fn(0x0200_0005)],
                &["L0bd4d82a.f1"],
                &[],
                &[],
                None,
                &[],
                &lib_qualifiers,
            )
            .expect("the library lowers"),
        )
        .expect("read the library object");
        let reference_handle = (crate::resolver::REFERENCE_HANDLE_TABLE << 24) | (1 << 20) | 5;
        let prog_qualifiers = DescQualifiers {
            own: None,
            references: vec!["aaaaaaaa".into(), "0bd4d82a".into()],
        };
        let prog = lamella_elf::read_object(
            &lower_object_profile_statics_references(
                &[alloc_fn(reference_handle)],
                &["f0"],
                &[],
                &[],
                None,
                &[],
                &prog_qualifiers,
                RiscvProfile::Rv32im,
            )
            .expect("the program lowers"),
        )
        .expect("read the program object");
        let qualified = "__lamella_typedesc_0bd4d82a_33554437";
        let lib_desc = lib
            .symbols
            .iter()
            .find(|s| s.name == qualified)
            .expect("the library's own descriptor takes the owner-qualified name");
        assert_eq!(lib_desc.binding, lamella_elf::Binding::Weak, "library copies are weak");
        let prog_desc = prog
            .symbols
            .iter()
            .find(|s| s.name == qualified)
            .expect("the program's reference-owned descriptor takes the SAME name");
        assert_eq!(
            prog_desc.binding,
            lamella_elf::Binding::Global,
            "the program's copy is strong, so the link dedupes to it"
        );
        for obj in [&lib, &prog] {
            assert!(
                !obj.symbols.iter().any(|s| s.name == "__lamella_typedesc_33554437"),
                "the unqualified raw-token name (the collision surface) must not appear"
            );
        }
    }

    #[test]
    fn a_compare_reused_by_a_later_block_branch_materializes() {
        let i32t = MirType::I32;
        let func = Function {
            params: vec![i32t],
            ret: Some(i32t),
            value_types: vec![i32t, i32t, i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![
                BasicBlock {
                    params: vec![ValueId(0)],
                    insts: vec![
                        (ValueId(1), Inst::ConstInt { ty: i32t, value: 0 }),
                        (
                            ValueId(2),
                            Inst::Compare {
                                op: CmpOp::Eq,
                                lhs: ValueId(0),
                                rhs: ValueId(1),
                            },
                        ),
                    ],
                    terminator: Some(Terminator::Branch {
                        cond: ValueId(2),
                        if_true: BlockId(1),
                        true_args: Vec::new(),
                        if_false: BlockId(1),
                        false_args: Vec::new(),
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Some(Terminator::Branch {
                        cond: ValueId(2),
                        if_true: BlockId(2),
                        true_args: Vec::new(),
                        if_false: BlockId(3),
                        false_args: Vec::new(),
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![(ValueId(3), Inst::ConstInt { ty: i32t, value: 1 })],
                    terminator: Some(Terminator::Return(Some(ValueId(3)))),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![(ValueId(4), Inst::ConstInt { ty: i32t, value: 0 })],
                    terminator: Some(Terminator::Return(Some(ValueId(4)))),
                },
            ],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let code = lower(&func).expect("a reused compare lowers");
        let words: Vec<u32> = code
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert!(
            words
                .iter()
                .any(|w| w & 0x0000_707F == 0x0000_3013 && (w >> 20) == 1),
            "the Eq compare materializes its 0/1 via SLTIU ..., 1"
        );
        let bne_on_value = words
            .iter()
            .filter(|w| *w & 0x0000_707F == 0x0000_1063 && ((*w >> 20) & 31) == 0)
            .count();
        assert!(
            bne_on_value >= 2,
            "both branch sites test the materialized compare against zero (found {bne_on_value})"
        );
    }

    #[test]
    fn value_type_call_words_round_up() {
        let types = |size| {
            vec![MirType::ValueType {
                handle: lamella_ir::TypeHandle(0),
                size,
            }]
        };
        assert_eq!(value_words(&types(1), ValueId(0)), 1);
        assert_eq!(value_words(&types(3), ValueId(0)), 1);
        assert_eq!(value_words(&types(4), ValueId(0)), 1);
        assert_eq!(value_words(&types(5), ValueId(0)), 2);
        assert_eq!(value_words(&types(8), ValueId(0)), 2);
        assert_eq!(
            value_words(&types(9), ValueId(0)),
            3,
            "9..11 bytes go by reference (> 2)"
        );
    }

    #[test]
    fn a_sub_word_struct_field_copies_width_exact() {
        let flag = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 1,
        };
        let func = Function {
            params: vec![MirType::ObjectRef],
            ret: Some(MirType::I32),
            value_types: vec![MirType::ObjectRef, flag, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![
                    (
                        ValueId(1),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 0,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::FieldStore {
                            base: ValueId(0),
                            offset: 0,
                            value: ValueId(1),
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 0,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(3)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let code = lower(&func).expect("a sub-word struct field copy lowers");
        let words: Vec<u32> = code
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert!(
            words.iter().any(|w| w & 0x0000_707F == 0x0000_4003),
            "the sub-word field load goes through LBU"
        );
        assert!(
            words.iter().any(|w| w & 0x0000_707F == 0x0000_0023),
            "the sub-word field store goes through SB"
        );
    }

    #[test]
    fn classifies_pointer_bases() {
        let types = [
            MirType::ObjectRef,
            MirType::ManagedPtr,
            MirType::I32,
            MirType::ValueType {
                handle: lamella_ir::TypeHandle(0),
                size: 8,
            },
        ];
        assert!(is_pointer(&types, ValueId(0)));
        assert!(is_pointer(&types, ValueId(1)));
        assert!(!is_pointer(&types, ValueId(2)));
        assert!(!is_pointer(&types, ValueId(3)));
    }

    /// A two-element `int[]` hand-laid in RAM: set the length, store a[0]=20 and a[1]=22, load
    /// them back and sum -> 42. Exercises ArrayStore/ArrayLoad (with the bounds check) over a
    /// pointer base; the length word at offset 0 is set with a FieldStore.
    fn array_function() -> Function {
        let i32t = MirType::I32;
        let cint = |v: i64| Inst::ConstInt { ty: i32t, value: v };
        Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![
                MirType::ObjectRef,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
                i32t,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::ObjectRef,
                            value: 0x8010_0000,
                        },
                    ),
                    (ValueId(1), cint(2)),
                    (
                        ValueId(2),
                        Inst::FieldStore {
                            base: ValueId(0),
                            offset: 0,
                            value: ValueId(1),
                        },
                    ),
                    (ValueId(3), cint(20)),
                    (ValueId(4), cint(0)),
                    (
                        ValueId(5),
                        Inst::ArrayStore {
                            array: ValueId(0),
                            index: ValueId(4),
                            value: ValueId(3),
                            element_size: 4,
                        },
                    ),
                    (ValueId(6), cint(22)),
                    (ValueId(7), cint(1)),
                    (
                        ValueId(8),
                        Inst::ArrayStore {
                            array: ValueId(0),
                            index: ValueId(7),
                            value: ValueId(6),
                            element_size: 4,
                        },
                    ),
                    (
                        ValueId(9),
                        Inst::ArrayLoad {
                            array: ValueId(0),
                            index: ValueId(4),
                            element_size: 4,
                            signed: false,
                        },
                    ),
                    (
                        ValueId(10),
                        Inst::ArrayLoad {
                            array: ValueId(0),
                            index: ValueId(7),
                            element_size: 4,
                            signed: false,
                        },
                    ),
                    (
                        ValueId(11),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(9),
                            rhs: ValueId(10),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(11)))),
            }],
        }
    }

    #[test]
    fn lowers_word_array_access() {
        let func = array_function();
        assert!(lamella_ir::verify(&func).is_ok());
        let code = lower(&func).expect("word array access lowers to RV32IM");
        assert!(!code.is_empty());
    }

    #[test]
    fn lowers_sub_word_array_elements() {
        for size in [1u32, 2] {
            let mut func = array_function();
            for (_, inst) in &mut func.blocks[0].insts {
                match inst {
                    Inst::ArrayStore { element_size, .. }
                    | Inst::ArrayLoad { element_size, .. } => *element_size = size,
                    _ => {}
                }
            }
            assert!(lamella_ir::verify(&func).is_ok());
            assert!(lower(&func).is_ok(), "element_size {size} lowers");
        }
    }

    #[test]
    fn register_path_rejects_eight_byte_array_elements() {
        let mut func = array_function();
        if let Inst::ArrayStore { element_size, .. } = &mut func.blocks[0].insts[5].1 {
            *element_size = 8;
        }
        assert_eq!(lower(&func), Err(LowerError::Unsupported));
    }

    #[test]
    fn lowers_array_element_address() {
        let i32t = MirType::I32;
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![MirType::ObjectRef, i32t, MirType::ManagedPtr, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::ObjectRef,
                            value: 0x8010_0000,
                        },
                    ),
                    (ValueId(1), Inst::ConstInt { ty: i32t, value: 1 }),
                    (
                        ValueId(2),
                        Inst::ArrayElemAddr {
                            array: ValueId(0),
                            index: ValueId(1),
                            element_size: 4,
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::Load {
                            address: ValueId(2),
                            width: 4,
                            signed: false,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(3)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(lower(&func).is_ok(), "ldelema lowers to RV32IM");
    }

    #[test]
    fn lowers_a_call() {
        let i32t = MirType::I32;
        let main = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 40,
                        },
                    ),
                    (ValueId(1), Inst::ConstInt { ty: i32t, value: 2 }),
                    (
                        ValueId(2),
                        Inst::Call {
                            callee: 1,
                            args: vec![ValueId(0), ValueId(1)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let add = Function {
            params: vec![i32t, i32t],
            ret: Some(i32t),
            value_types: vec![i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![(
                    ValueId(2),
                    Inst::Binary {
                        op: BinOp::Add,
                        lhs: ValueId(0),
                        rhs: ValueId(1),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let code = lower_module(&[main, add]).expect("a module with a call lowers");
        assert!(!code.is_empty());
    }

    /// A one-conversion function typed by the kind's own result and input: `RefToInt` reads an
    /// ObjectRef, the rest read an int; the result carries `kind.result_type()`. The sub-word and
    /// reinterpret forms lower; a float conversion, which needs the soft-float helpers, is rejected.
    fn convert_function(kind: ConvKind) -> Function {
        let result_ty = kind.result_type();
        let input_ty = match kind {
            ConvKind::RefToInt => MirType::ObjectRef,
            _ => MirType::I32,
        };
        Function {
            params: Vec::new(),
            ret: Some(result_ty),
            value_types: vec![input_ty, result_ty],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: input_ty,
                            value: 0xAA,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::Convert {
                            value: ValueId(0),
                            kind,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        }
    }

    #[test]
    fn lowers_sub_word_and_reinterpret_conversions() {
        for kind in [
            ConvKind::SignExtend8,
            ConvKind::ZeroExtend8,
            ConvKind::SignExtend16,
            ConvKind::ZeroExtend16,
            ConvKind::RefToInt,
            ConvKind::IntToRef,
        ] {
            let func = convert_function(kind);
            assert!(lamella_ir::verify(&func).is_ok(), "{kind:?} must verify");
            assert!(lower(&func).is_ok(), "{kind:?} must lower");
        }
    }

    #[test]
    fn the_flat_path_rejects_float_conversion() {
        let func = convert_function(ConvKind::IntToFloat32);
        assert_eq!(lower(&func), Err(LowerError::Unsupported));
    }

    #[test]
    fn the_object_path_lowers_float_ops() {
        let f64t = MirType::F64;
        let i32t = MirType::I32;
        let n = ValueId;
        let f64c = |bits: f64| Inst::ConstInt {
            ty: f64t,
            value: bits.to_bits() as i64,
        };
        let arith = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![f64t, f64t, f64t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (n(0), f64c(6.0)),
                    (n(1), f64c(7.0)),
                    (
                        n(2),
                        Inst::Binary {
                            op: BinOp::Mul,
                            lhs: n(0),
                            rhs: n(1),
                        },
                    ),
                    (
                        n(3),
                        Inst::Convert {
                            value: n(2),
                            kind: ConvKind::Float64ToInt,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(3)))),
            }],
        };
        let cmp = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![f64t, f64t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (n(0), f64c(6.0)),
                    (n(1), f64c(7.0)),
                    (
                        n(2),
                        Inst::Compare {
                            op: CmpOp::SignedLt,
                            lhs: n(0),
                            rhs: n(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(2)))),
            }],
        };
        assert!(
            lower_object(&[arith], &["f0"], &[], &[]).is_ok(),
            "float arithmetic + conversion lower on the object path"
        );
        assert!(
            lower_object(&[cmp], &["f0"], &[], &[]).is_ok(),
            "a float compare lowers on the object path"
        );
    }

    #[test]
    fn lowers_wide_value_types_by_reference_and_sret() {
        let n = ValueId;
        let s = MirType::ValueType {
            handle: TypeHandle(1),
            size: 12,
        };
        let make = Function {
            params: Vec::new(),
            ret: Some(s),
            value_types: vec![s, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (n(0), Inst::InitStruct),
                    (n(1), Inst::ConstInt { ty: MirType::I32, value: 42 }),
                    (
                        n(2),
                        Inst::FieldStore {
                            base: n(0),
                            offset: 0,
                            value: n(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(0)))),
            }],
        };
        let take = Function {
            params: vec![s],
            ret: Some(MirType::I32),
            value_types: vec![s, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![n(0)],
                insts: vec![(n(1), Inst::FieldLoad { base: n(0), offset: 0 })],
                terminator: Some(Terminator::Return(Some(n(1)))),
            }],
        };
        assert!(
            lower_module(&[make]).is_ok(),
            "a 3-word sret return lowers"
        );
        assert!(
            lower_module(&[take]).is_ok(),
            "a 3-word by-value parameter lowers"
        );
    }

    #[test]
    fn lowers_value_type_struct_locals() {
        let i32t = MirType::I32;
        let point = MirType::ValueType {
            handle: TypeHandle(1),
            size: 8,
        };
        let n = ValueId;
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![point, i32t, i32t, point, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (n(0), Inst::InitStruct),
                    (
                        n(1),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 42,
                        },
                    ),
                    (
                        n(2),
                        Inst::FieldStore {
                            base: n(0),
                            offset: 0,
                            value: n(1),
                        },
                    ),
                    (n(3), Inst::CopyStruct { src: n(0) }),
                    (
                        n(4),
                        Inst::FieldLoad {
                            base: n(3),
                            offset: 0,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(4)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(lower(&func).is_ok(), "struct local lowers to RV32IM");
    }

    #[test]
    fn lowers_small_value_type_call_return() {
        let i32t = MirType::I32;
        let point = MirType::ValueType {
            handle: TypeHandle(1),
            size: 8,
        };
        let n = ValueId;
        let make = Function {
            params: Vec::new(),
            ret: Some(point),
            value_types: vec![point],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(n(0), Inst::InitStruct)],
                terminator: Some(Terminator::Return(Some(n(0)))),
            }],
        };
        let main = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![point, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        n(0),
                        Inst::Call {
                            callee: 1,
                            args: Vec::new(),
                        },
                    ),
                    (
                        n(1),
                        Inst::FieldLoad {
                            base: n(0),
                            offset: 0,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(1)))),
            }],
        };
        assert!(
            lower_module(&[main, make]).is_ok(),
            "a small value-type call return rides a0:a1"
        );
    }

    #[test]
    fn lowers_block_copy_and_fill() {
        let i32t = MirType::I32;
        let n = ValueId;
        let func = Function {
            params: Vec::new(),
            ret: None,
            value_types: vec![MirType::ObjectRef, MirType::ObjectRef, i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        n(0),
                        Inst::ConstInt {
                            ty: MirType::ObjectRef,
                            value: 0x8010_0000,
                        },
                    ),
                    (
                        n(1),
                        Inst::ConstInt {
                            ty: MirType::ObjectRef,
                            value: 0x8010_0100,
                        },
                    ),
                    (n(2), Inst::ConstInt { ty: i32t, value: 8 }),
                    (
                        n(3),
                        Inst::CopyBlock {
                            dst: n(1),
                            src: n(0),
                            size: n(2),
                        },
                    ),
                    (
                        n(4),
                        Inst::FillBlock {
                            dst: n(0),
                            value: n(2),
                            size: n(2),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(None)),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(lower(&func).is_ok(), "cpblk/initblk lower to RV32IM");
    }

    #[test]
    fn lowers_static_field_access() {
        let i32t = MirType::I32;
        let n = ValueId;
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        n(0),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 42,
                        },
                    ),
                    (
                        n(1),
                        Inst::StaticStore {
                            owner: lamella_ir::StaticOwner::Own,
                            offset: 8,
                            value: n(0),
                        },
                    ),
                    (
                        n(2),
                        Inst::StaticLoad {
                            owner: lamella_ir::StaticOwner::Own,
                            offset: 8,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(2)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(lower(&func).is_ok(), "static field access lowers to RV32IM");
    }

    #[test]
    fn lowers_2d_array_access() {
        let i32t = MirType::I32;
        let n = ValueId;
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![i32t, i32t, MirType::ObjectRef, i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (n(0), Inst::ConstInt { ty: i32t, value: 2 }),
                    (n(1), Inst::ConstInt { ty: i32t, value: 2 }),
                    (
                        n(2),
                        Inst::AllocArray2D {
                            handle: TypeHandle(3),
                            dim0: n(0),
                            dim1: n(1),
                            element_size: 4,
                        },
                    ),
                    (
                        n(3),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 42,
                        },
                    ),
                    (
                        n(4),
                        Inst::Array2DStore {
                            array: n(2),
                            index0: n(0),
                            index1: n(0),
                            value: n(3),
                            element_size: 4,
                        },
                    ),
                    (
                        n(5),
                        Inst::Array2DLoad {
                            array: n(2),
                            index0: n(0),
                            index1: n(0),
                            element_size: 4,
                            signed: false,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(5)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(
            lower_module_gc(core::slice::from_ref(&func), 0x8000_0004).is_ok(),
            "2-D array access lowers to RV32IM"
        );
    }

    #[test]
    fn lowers_func_addr_and_indirect_call() {
        let i32t = MirType::I32;
        let n = ValueId;
        let main = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![i32t; 4],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (n(0), Inst::FuncAddr { func: 1 }),
                    (
                        n(1),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 40,
                        },
                    ),
                    (n(2), Inst::ConstInt { ty: i32t, value: 2 }),
                    (
                        n(3),
                        Inst::CallIndirect {
                            target: n(0),
                            args: vec![n(1), n(2)],
                            returns_value: true,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(3)))),
            }],
        };
        let add = Function {
            params: vec![i32t, i32t],
            ret: Some(i32t),
            value_types: vec![i32t; 3],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![n(0), n(1)],
                insts: vec![(
                    n(2),
                    Inst::Binary {
                        op: BinOp::Add,
                        lhs: n(0),
                        rhs: n(1),
                    },
                )],
                terminator: Some(Terminator::Return(Some(n(2)))),
            }],
        };
        assert!(lower_module(&[main, add]).is_ok(), "ldftn/calli lowers");
    }

    #[test]
    fn lowers_delegate_invoke() {
        let i32t = MirType::I32;
        let n = ValueId;
        let main = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![i32t, MirType::ObjectRef, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (n(0), Inst::FuncAddr { func: 1 }),
                    (
                        n(1),
                        Inst::ConstInt {
                            ty: MirType::ObjectRef,
                            value: 0x8010_0000,
                        },
                    ),
                    (
                        n(2),
                        Inst::FieldStore {
                            base: n(1),
                            offset: 4,
                            value: n(0),
                        },
                    ),
                    (
                        n(3),
                        Inst::InvokeDelegate {
                            delegate: n(1),
                            args: Vec::new(),
                            returns_value: true,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(3)))),
            }],
        };
        let add = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    n(0),
                    Inst::ConstInt {
                        ty: i32t,
                        value: 42,
                    },
                )],
                terminator: Some(Terminator::Return(Some(n(0)))),
            }],
        };
        assert!(lower_module(&[main, add]).is_ok(), "delegate invoke lowers");
    }

    #[test]
    fn lowers_virtual_dispatch_with_a_vtable() {
        let i32t = MirType::I32;
        let n = ValueId;
        let main = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![MirType::ObjectRef, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        n(0),
                        Inst::Alloc {
                            handle: TypeHandle(2),
                            payload_size: 4,
                            ref_offsets: Vec::new().into_boxed_slice(),
                        },
                    ),
                    (
                        n(1),
                        Inst::CallVirtual {
                            slot: 0,
                            args: vec![n(0)],
                            returns_value: true,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(1)))),
            }],
        };
        let leaf = |v: i64| Function {
            params: vec![MirType::ObjectRef],
            ret: Some(i32t),
            value_types: vec![MirType::ObjectRef, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![n(0)],
                insts: vec![(n(1), Inst::ConstInt { ty: i32t, value: v })],
                terminator: Some(Terminator::Return(Some(n(1)))),
            }],
        };
        let descriptors = vec![
            TypeMeta {
                handle: TypeHandle(1),
                type_tag: 0,
                vtable: vec![VtableEntry::Func(1)],
                itable: Vec::new(),
                base: None,
                words: None,
            },
            TypeMeta {
                handle: TypeHandle(2),
                type_tag: 0,
                vtable: vec![VtableEntry::Func(2)],
                itable: Vec::new(),
                base: Some(TypeHandle(1)),
                words: None,
            },
        ];
        let funcs = [main, leaf(4), leaf(2)];
        assert!(
            lower_module_gc_with_descriptors(&funcs, 0x8000_0004, &descriptors).is_ok(),
            "virtual dispatch with a vtable lowers to RV32IM"
        );
    }

    #[test]
    fn library_descriptors_are_weak_program_descriptors_global() {
        let allocates = || Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: TypeHandle(7),
                        payload_size: 12,
                        ref_offsets: Vec::new().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let descriptors = vec![TypeMeta {
            handle: TypeHandle(7),
            type_tag: 0,
            vtable: Vec::new(),
            itable: Vec::new(),
            base: None,
            words: None,
        }];
        let name = alloc::format!("{}7", lamella_elf::TYPE_DESC_PREFIX);
        let binding_of = |object: &[u8]| {
            lamella_elf::read_object(object)
                .expect("parse the emitted object")
                .symbols
                .iter()
                .find(|s| s.name == name)
                .expect("the descriptor symbol is emitted")
                .binding
        };
        let library = lower_object_library(&[allocates()], &["m"], &[], &descriptors)
            .expect("library lowers");
        let program =
            lower_object(&[allocates()], &["m"], &[], &descriptors).expect("program lowers");
        assert_eq!(binding_of(&library), lamella_elf::Binding::Weak);
        assert_eq!(binding_of(&program), lamella_elf::Binding::Global);
    }

    #[test]
    fn lowers_interface_dispatch_with_an_itable() {
        let i32t = MirType::I32;
        let n = ValueId;
        let tag = 0x8000_1234u32;
        let main = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![MirType::ObjectRef, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        n(0),
                        Inst::Alloc {
                            handle: TypeHandle(1),
                            payload_size: 4,
                            ref_offsets: Vec::new().into_boxed_slice(),
                        },
                    ),
                    (
                        n(1),
                        Inst::CallInterface {
                            tag,
                            args: vec![n(0)],
                            returns_value: true,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(1)))),
            }],
        };
        let area = Function {
            params: vec![MirType::ObjectRef],
            ret: Some(i32t),
            value_types: vec![MirType::ObjectRef, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![n(0)],
                insts: vec![(
                    n(1),
                    Inst::ConstInt {
                        ty: i32t,
                        value: 42,
                    },
                )],
                terminator: Some(Terminator::Return(Some(n(1)))),
            }],
        };
        let descriptors = vec![TypeMeta {
            handle: TypeHandle(1),
            type_tag: 0,
            vtable: Vec::new(),
            itable: vec![(tag, VtableEntry::Func(1))],
            base: None,
            words: None,
        }];
        assert!(
            lower_module_gc_with_descriptors(&[main, area], 0x8000_0004, &descriptors).is_ok(),
            "interface dispatch with an itable lowers to RV32IM"
        );
    }

    #[test]
    fn lowers_type_identity_compare() {
        let i32t = MirType::I32;
        let n = ValueId;
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![MirType::ObjectRef, i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        n(0),
                        Inst::Alloc {
                            handle: TypeHandle(1),
                            payload_size: 4,
                            ref_offsets: Vec::new().into_boxed_slice(),
                        },
                    ),
                    (n(1), Inst::LoadTypeDesc { object: n(0) }),
                    (
                        n(2),
                        Inst::TypeDescAddr {
                            handle: TypeHandle(2),
                        },
                    ),
                    (
                        n(3),
                        Inst::Compare {
                            op: CmpOp::Eq,
                            lhs: n(1),
                            rhs: n(2),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(3)))),
            }],
        };
        assert!(
            lower_module_gc_with_descriptors(core::slice::from_ref(&func), 0x8000_0004, &[])
                .is_ok(),
            "LoadTypeDesc/TypeDescAddr identity compare lowers to RV32IM"
        );
    }

    #[test]
    fn lowers_castclass_chain_scan_with_synthesized_ancestor() {
        let i32t = MirType::I32;
        let n = ValueId;
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![MirType::ObjectRef, i32t, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        n(0),
                        Inst::Alloc {
                            handle: TypeHandle(3),
                            payload_size: 4,
                            ref_offsets: Vec::new().into_boxed_slice(),
                        },
                    ),
                    (n(1), Inst::LoadTypeDesc { object: n(0) }),
                    (
                        n(2),
                        Inst::TypeDescAddr {
                            handle: TypeHandle(1),
                        },
                    ),
                    (
                        n(3),
                        Inst::CastClassScan {
                            args: vec![n(1), n(2)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(3)))),
            }],
        };
        let descriptors = vec![
            TypeMeta {
                handle: TypeHandle(1),
                type_tag: 0,
                vtable: Vec::new(),
                itable: Vec::new(),
                base: None,
                words: None,
            },
            TypeMeta {
                handle: TypeHandle(2),
                type_tag: 0,
                vtable: Vec::new(),
                itable: Vec::new(),
                base: Some(TypeHandle(1)),
                words: None,
            },
            TypeMeta {
                handle: TypeHandle(3),
                type_tag: 0,
                vtable: Vec::new(),
                itable: Vec::new(),
                base: Some(TypeHandle(2)),
                words: None,
            },
        ];
        assert!(
            lower_module_gc_with_descriptors(
                core::slice::from_ref(&func),
                0x8000_0004,
                &descriptors
            )
            .is_ok(),
            "castclass base-pointer chain scan + synthesized ancestor lowers to RV32IM"
        );
    }

    #[test]
    fn lower_object_emits_a_callnative_extern_reloc() {
        let i32t = MirType::I32;
        let n = ValueId;
        let main = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        n(0),
                        Inst::ConstInt {
                            ty: i32t,
                            value: 14,
                        },
                    ),
                    (
                        n(1),
                        Inst::CallNative {
                            symbol: 0,
                            args: vec![n(0)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(1)))),
            }],
        };
        let bytes =
            lower_object(&[main], &["main"], &["triple"], &[]).expect("lower_object with extern");
        let obj = lamella_elf::read_object(&bytes).expect("read the object back");
        let triple = obj
            .symbols
            .iter()
            .find(|s| s.name == "triple")
            .expect("the extern `triple` symbol is present");
        assert!(
            !triple.defined,
            "`triple` is an undefined extern the linker resolves"
        );
        assert!(
            obj.relocations.iter().any(|r| {
                r.kind == lamella_elf::riscv::R_RISCV_CALL_PLT
                    && obj.symbols.get(r.symbol as usize).map(|s| s.name.as_str()) == Some("triple")
            }),
            "an R_RISCV_CALL_PLT relocation names `triple`"
        );
    }

    #[test]
    fn lowers_virtual_func_addr() {
        let i32t = MirType::I32;
        let n = ValueId;
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![MirType::ObjectRef, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        n(0),
                        Inst::ConstInt {
                            ty: MirType::ObjectRef,
                            value: 0x8010_0000,
                        },
                    ),
                    (
                        n(1),
                        Inst::VirtualFuncAddr {
                            object: n(0),
                            slot: 0,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(1)))),
            }],
        };
        assert!(lower(&func).is_ok(), "ldvirtftn lowers to RV32IM");
    }

    #[test]
    fn lowers_int64_arithmetic() {
        let i64t = MirType::I64;
        let i32t = MirType::I32;
        let n = ValueId;
        let func = Function {
            params: Vec::new(),
            ret: Some(i32t),
            value_types: vec![i64t, i32t, i64t, i64t, i32t, i32t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        n(0),
                        Inst::ConstInt {
                            ty: i64t,
                            value: 0x1_0000_0029,
                        },
                    ),
                    (n(1), Inst::ConstInt { ty: i32t, value: 1 }),
                    (
                        n(2),
                        Inst::Widen {
                            value: n(1),
                            signed: true,
                        },
                    ),
                    (
                        n(3),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: n(0),
                            rhs: n(2),
                        },
                    ),
                    (
                        n(4),
                        Inst::Compare {
                            op: CmpOp::SignedLt,
                            lhs: n(0),
                            rhs: n(3),
                        },
                    ),
                    (n(5), Inst::Truncate { value: n(3) }),
                ],
                terminator: Some(Terminator::Return(Some(n(5)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(lower(&func).is_ok(), "int64 arithmetic lowers to RV32IM");
    }

    #[test]
    fn the_flat_path_rejects_int64_divide() {
        let i64t = MirType::I64;
        let n = ValueId;
        let func = Function {
            params: Vec::new(),
            ret: Some(i64t),
            value_types: vec![i64t, i64t],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (n(0), Inst::ConstInt { ty: i64t, value: 3 }),
                    (
                        n(1),
                        Inst::Binary {
                            op: BinOp::DivSigned,
                            lhs: n(0),
                            rhs: n(0),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(1)))),
            }],
        };
        assert_eq!(lower(&func), Err(LowerError::Unsupported));
    }
}
