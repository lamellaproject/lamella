//! The RV32IM (RISC-V) target code generator.

use alloc::vec::Vec;

use lamella_asm_riscv32::{BranchCond, Encoder, Label, Reg};
use lamella_ir::{BinOp, CmpOp, ConvKind, Function, Inst, MirType, Terminator, TypeHandle, ValueId};

use crate::resolver::{TypeMeta, VtableEntry};

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

/// The callee-saved registers the trivial value map hands out, in order: s0-s11 (x8, x9, x18-x27).
/// Callee-saved means a value survives a `call` without spilling -- the prologue saves each one the
/// function uses and the epilogue restores it. `a0`-`a7` carry call arguments and the return value;
/// `t6` is array-addressing scratch ([`scratch`]); `ra`/`sp`/`x0` are reserved by the ABI.
fn allocatable() -> [Reg; 12] {
    let r = |n: u8| Reg::new(n).unwrap_or(Reg::ZERO);
    [
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
        r(27),
    ]
}

/// The argument/return register `a<index>` (x10-x17), or `None` past the eighth (stack-passed
/// arguments are not lowered by this backend yet).
fn arg_reg(index: usize) -> Option<Reg> {
    (index < 8).then(|| Reg::new(10 + index as u8).unwrap_or(Reg::ZERO))
}

/// `t6` (x31), reserved out of the allocatable pool as scratch for array addressing -- the length
/// load, the index scaling, and the element address -- so it never aliases an allocated value.
fn scratch() -> Reg {
    Reg::new(31).unwrap_or(Reg::ZERO)
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
                Inst::Alloc { .. } | Inst::AllocArray { .. } | Inst::AllocArray2D { .. }
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
        AllocSite::Extern(symbol) => {
            emit_call(enc, func_labels, relocs, relocate, EXTERN_SYMBOL_FLAG | symbol)
        }
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
    lower_module_inner(funcs, None, &[])
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
    lower_module_inner(funcs, Some(alloc_addr), descriptors)
}

fn lower_module_inner(
    funcs: &[Function],
    alloc_addr: Option<u32>,
    descriptors: &[TypeMeta],
) -> Result<Vec<u8>, LowerError> {
    let alloc = match alloc_addr {
        Some(addr) => AllocSite::Address(addr),
        None => AllocSite::None,
    };
    lower_module_to_image(funcs, alloc, descriptors, false).map(|(bytes, _, _)| bytes)
}

/// A lowered module: the code bytes, each function's entry offset, and the call relocations as
/// `(auipc offset, callee index)` pairs (empty unless lowering for a relocatable object).
type LoweredModule = (Vec<u8>, Vec<u32>, Vec<(u32, u32)>);

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
    relocate: bool,
) -> Result<LoweredModule, LowerError> {
    let mut program = funcs.to_vec();
    if !relocate {
        crate::stringgen::lower_string_equals(&mut program);
        crate::stringgen::lower_string_concat(&mut program);
        crate::stringgen::lower_int_to_string(&mut program);
    }
    let funcs: &[Function] = &program;
    for func in funcs {
        if lamella_ir::verify(func).is_err() {
            return Err(LowerError::NotWellFormed);
        }
    }
    let mut enc = Encoder::new();
    let func_labels: Vec<Label> = (0..funcs.len()).map(|_| enc.new_label()).collect();
    let mut offsets: Vec<u32> = Vec::with_capacity(funcs.len());
    let mut call_relocs: Vec<(u32, u32)> = Vec::new();
    for (index, func) in funcs.iter().enumerate() {
        enc.bind_label(func_labels[index]);
        offsets.push(enc.position());
        lower_function(
            &mut enc,
            func,
            &func_labels,
            alloc,
            descriptors,
            &mut call_relocs,
            relocate,
        )?;
    }
    let bytes = enc
        .finish()
        .map(|assembled| assembled.bytes)
        .map_err(|_| LowerError::CodeTooLarge)?;
    Ok((bytes, offsets, call_relocs))
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
/// `descriptors` are the per-type vtables/itables (the resolver's `type_descriptors()`), laid IN-IMAGE
/// after each allocating function's code (addressed PC-relatively via `la`, so position-independent
/// through the link) -- so a dispatched/cast type's `Alloc` writes its `obj-4` descriptor pointer and
/// `CallVirtual`/`CallInterface`/`castclass` work over the link, exactly as on the flat path. Pass `&[]`
/// for a module with no dispatch. (The LINKABLE-symbol descriptor lane -- for `--gc-sections` and
/// cross-assembly vtables -- is a later brick; these in-image descriptors serve self-contained programs.)
pub fn lower_object(
    funcs: &[Function],
    names: &[&str],
    externs: &[&str],
    descriptors: &[TypeMeta],
) -> Result<Vec<u8>, LowerError> {
    let mut extern_names: Vec<alloc::string::String> = externs.iter().map(|s| (*s).into()).collect();
    let program: Vec<Function> = funcs
        .iter()
        .map(|f| rewrite_pinvoke(f, &mut extern_names))
        .collect();
    let alloc = if program.iter().any(func_allocates) {
        AllocSite::Extern(intern_extern(&mut extern_names, "lamella_gc_alloc"))
    } else {
        AllocSite::None
    };
    let (text, offsets, call_relocs) = lower_module_to_image(&program, alloc, descriptors, true)?;
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
    let relocations: Vec<lamella_elf::Relocation> = call_relocs
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
/// the saved registers restored and `ret`.
fn lower_function(
    enc: &mut Encoder,
    func: &Function,
    func_labels: &[Label],
    alloc: AllocSite,
    descriptors: &[TypeMeta],
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
) -> Result<(), LowerError> {
    let pool = allocatable();
    let value_count = func.value_types.len();
    let allocates = func_allocates(func);
    let has_value_types = func
        .value_types
        .iter()
        .any(|t| matches!(t, MirType::ValueType { .. } | MirType::I64));
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
            )
        })
    });
    let has_string_literal = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| matches!(i, Inst::StringLiteral { .. })));
    if value_count > pool.len()
        || allocates
        || has_value_types
        || has_dispatch
        || has_string_literal
    {
        return lower_function_spilled(
            enc,
            func,
            func_labels,
            alloc,
            descriptors,
            relocs,
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

        let fused = match &block.terminator {
            Some(Terminator::Branch { cond, .. }) => match block.insts.last() {
                Some((r, Inst::Compare { op, lhs, rhs })) if r == cond => Some((*op, *lhs, *rhs)),
                _ => None,
            },
            _ => None,
        };
        let body = if fused.is_some() {
            &block.insts[..block.insts.len() - 1]
        } else {
            &block.insts[..]
        };

        for (result, inst) in body {
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
                match fused {
                    Some((op, lhs, rhs)) => {
                        let (cond, a, b) = branch_for(op, reg(lhs), reg(rhs));
                        enc.branch(cond, a, b, true_label);
                    }
                    None => enc.branch(BranchCond::Ne, reg(*cond), Reg::ZERO, true_label),
                }
                enc.j(false_label);
            }
            Some(Terminator::Unreachable) => enc.ebreak(),
            None => return Err(LowerError::ControlFlowUnsupported),
        }
    }
    Ok(())
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
            emit_element_address(enc, reg(*array), reg(*index), *element_size);
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
            emit_element_address(enc, reg(*array), reg(*index), *element_size);
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
            emit_element_address(enc, reg(*array), reg(*index), *element_size);
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
            emit_copy_block(enc, Reg::T0, Reg::T1, Reg::T2);
        }
        Inst::FillBlock { dst, value, size } => {
            enc.mv(Reg::T0, reg(*dst));
            enc.mv(Reg::T1, reg(*value));
            enc.mv(Reg::T2, reg(*size));
            emit_fill_block(enc, Reg::T0, Reg::T1, Reg::T2);
        }
        Inst::StaticLoad { offset } => {
            enc.li(reg(result), (STATIC_FIELD_BASE + *offset) as i32);
            enc.lw(reg(result), reg(result), 0);
        }
        Inst::StaticStore { offset, value } => {
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

/// Lowers a function whose value count exceeds the 12 callee-saved registers into an ALL-SPILLED
/// frame: every value gets a 4-byte stack slot, each instruction loads its operands into the
/// `t0`-`t2` scratch registers, computes, and stores the result back. Nothing live sits in a
/// register across a call, so the caller's values survive with no callee-saved bookkeeping (only
/// `ra` is saved). This lifts the register-only path's value-count cap. The frame's slot offsets
/// must fit the 12-bit `lw`/`sw` immediate, so a function past ~500 values is rejected (deferred).
/// Block parameters move slot-to-slot through `t0`: every value has a distinct slot, so the
/// sequential move is sound (the register path's no-alias assumption).
fn lower_function_spilled(
    enc: &mut Encoder,
    func: &Function,
    func_labels: &[Label],
    alloc: AllocSite,
    descriptors: &[TypeMeta],
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
) -> Result<(), LowerError> {
    let value_count = func.value_types.len();
    let has_calls = func.blocks.iter().any(|b| {
        b.insts.iter().any(|(_, i)| {
            matches!(
                i,
                Inst::Call { .. }
                    | Inst::Alloc { .. }
                    | Inst::AllocArray { .. }
                    | Inst::AllocArray2D { .. }
                    | Inst::CallIndirect { .. }
                    | Inst::InvokeDelegate { .. }
                    | Inst::CallVirtual { .. }
                    | Inst::CallInterface { .. }
                    | Inst::CallNative { .. }
            )
        })
    });
    let mut offsets: Vec<i32> = Vec::with_capacity(value_count);
    let mut used = 0i32;
    for ty in &func.value_types {
        offsets.push(used);
        used += ty.stack_slot_bytes() as i32;
    }
    let ra_off = used;
    let frame = ((used + has_calls as i32 * 4) as usize).div_ceil(16) * 16;
    if frame > 2047 {
        return Err(LowerError::TooManyValues);
    }
    let slot = |v: ValueId| offsets[v.index()];
    let mut type_descs: TypeDescs = Vec::new();
    let mut type_desc_labels: Vec<(TypeHandle, Label)> = Vec::new();
    let mut string_blobs: Vec<(Label, Vec<u16>)> = Vec::new();

    if frame > 0 {
        enc.addi(Reg::SP, Reg::SP, -(frame as i32));
    }
    if has_calls {
        enc.sw(Reg::RA, Reg::SP, ra_off);
    }
    let entry = &func.blocks[func.entry.index()];
    for (i, &param) in entry.params.iter().enumerate() {
        let arg = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
        enc.sw(arg, Reg::SP, slot(param));
    }

    let block_labels: Vec<Label> = (0..func.blocks.len()).map(|_| enc.new_label()).collect();
    if func.entry != lamella_ir::BlockId(0) {
        enc.j(block_labels[func.entry.index()]);
    }

    for (index, block) in func.blocks.iter().enumerate() {
        enc.bind_label(block_labels[index]);
        let fused = match &block.terminator {
            Some(Terminator::Branch { cond, .. }) => match block.insts.last() {
                Some((r, Inst::Compare { op, lhs, rhs })) if r == cond => Some((*op, *lhs, *rhs)),
                _ => None,
            },
            _ => None,
        };
        let body = if fused.is_some() {
            &block.insts[..block.insts.len() - 1]
        } else {
            &block.insts[..]
        };
        for (result, inst) in body {
            lower_inst_spilled(
                enc,
                &slot,
                &func.value_types,
                func_labels,
                *result,
                inst,
                alloc,
                descriptors,
                &mut type_descs,
                &mut type_desc_labels,
                &mut string_blobs,
                relocs,
                relocate,
            )?;
        }
        match &block.terminator {
            Some(Terminator::Return(value)) => {
                if let Some(v) = value {
                    enc.lw(Reg::A0, Reg::SP, slot(*v));
                }
                if has_calls {
                    enc.lw(Reg::RA, Reg::SP, ra_off);
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
                match fused {
                    Some((op, lhs, rhs)) => {
                        enc.lw(Reg::T0, Reg::SP, slot(lhs));
                        enc.lw(Reg::T1, Reg::SP, slot(rhs));
                        let (cond, a, b) = branch_for(op, Reg::T0, Reg::T1);
                        enc.branch(cond, a, b, true_label);
                    }
                    None => {
                        enc.lw(Reg::T0, Reg::SP, slot(*cond));
                        enc.branch(BranchCond::Ne, Reg::T0, Reg::ZERO, true_label);
                    }
                }
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
            if !type_desc_labels.iter().any(|(h, _)| *h == base) {
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
    for desc in &type_descs {
        for slot in desc.vtable.iter().rev() {
            match slot {
                Some(idx) => enc.emit_word_diff(desc.label, func_labels[*idx as usize]),
                None => enc.emit_word(0),
            }
        }
        enc.bind_label(desc.label);
        for (idx, &w) in desc.words.iter().enumerate() {
            if idx == 3 {
                match desc.base.and_then(|b| {
                    type_desc_labels
                        .iter()
                        .find(|(h, _)| *h == b)
                        .map(|(_, l)| *l)
                }) {
                    Some(base_label) => enc.emit_word_diff(desc.label, base_label),
                    None => enc.emit_word(0),
                }
            } else {
                enc.emit_word(w);
            }
        }
        enc.emit_word(desc.itable.len() as u32);
        for (tag, method) in &desc.itable {
            enc.emit_word(*tag);
            match method {
                Some(idx) => enc.emit_word_diff(desc.label, func_labels[*idx as usize]),
                None => enc.emit_word(0),
            }
        }
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
    Ok(())
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
    relocs: &mut Vec<(u32, u32)>,
    relocate: bool,
) -> Result<(), LowerError> {
    let (t0, t1, t2) = (Reg::T0, Reg::T1, Reg::T2);
    match inst {
        Inst::ConstInt { ty, value } => {
            enc.li(t0, *value as i32);
            enc.sw(t0, Reg::SP, slot(result));
            if matches!(ty, MirType::I64) {
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
                emit_i64_binary(enc, slot, *op, result, *lhs, *rhs)?;
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
                emit_i64_compare(enc, slot, *op, result, *lhs, *rhs)?;
            } else {
                enc.lw(t0, Reg::SP, slot(*lhs));
                enc.lw(t1, Reg::SP, slot(*rhs));
                materialize_compare(enc, t2, t0, t1, *op);
                enc.sw(t2, Reg::SP, slot(result));
            }
        }
        Inst::Convert { value, kind } => {
            enc.lw(t0, Reg::SP, slot(*value));
            emit_convert(enc, t0, t0, *kind)?;
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::CopyBlock { dst, src, size } => {
            enc.lw(t0, Reg::SP, slot(*dst));
            enc.lw(t1, Reg::SP, slot(*src));
            enc.lw(t2, Reg::SP, slot(*size));
            emit_copy_block(enc, t0, t1, t2);
        }
        Inst::FillBlock { dst, value, size } => {
            enc.lw(t0, Reg::SP, slot(*dst));
            enc.lw(t1, Reg::SP, slot(*value));
            enc.lw(t2, Reg::SP, slot(*size));
            emit_fill_block(enc, t0, t1, t2);
        }
        Inst::StaticLoad { offset } => {
            enc.li(t0, (STATIC_FIELD_BASE + *offset) as i32);
            enc.lw(t0, t0, 0);
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::StaticStore { offset, value } => {
            enc.li(t0, (STATIC_FIELD_BASE + *offset) as i32);
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
            enc.mul(t0, t0, t1);
            enc.li(t1, *element_size as i32);
            enc.mul(t0, t0, t1);
            enc.addi(Reg::A0, t0, 8);
            enc.la(Reg::A1, desc_label);
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
            emit_2d_element_address(enc, t0, t1, t2, *element_size, Reg::A0, Reg::A1);
            match (*element_size, *signed) {
                (1, true) => enc.lb(t0, scratch(), 0),
                (1, false) => enc.lbu(t0, scratch(), 0),
                (2, true) => enc.lh(t0, scratch(), 0),
                (2, false) => enc.lhu(t0, scratch(), 0),
                _ => enc.lw(t0, scratch(), 0),
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
            emit_2d_element_address(enc, t0, t1, t2, *element_size, Reg::A0, Reg::A1);
            enc.lw(t0, Reg::SP, slot(*value));
            match *element_size {
                1 => enc.sb(t0, scratch(), 0),
                2 => enc.sh(t0, scratch(), 0),
                _ => enc.sw(t0, scratch(), 0),
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
            if let Some(words) = value_type_words(value_types, result) {
                if ptr {
                    enc.lw(t0, Reg::SP, slot(*base));
                }
                for w in 0..words {
                    let foff = *offset + w * 4;
                    if ptr {
                        enc.lw(t1, t0, field_offset(foff)?);
                    } else {
                        enc.lw(t1, Reg::SP, slot(*base) + foff as i32);
                    }
                    enc.sw(t1, Reg::SP, slot(result) + (w * 4) as i32);
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
            if let Some(words) = value_type_words(value_types, *value) {
                if ptr {
                    enc.lw(t0, Reg::SP, slot(*base));
                }
                for w in 0..words {
                    let foff = *offset + w * 4;
                    enc.lw(t1, Reg::SP, slot(*value) + (w * 4) as i32);
                    if ptr {
                        enc.sw(t1, t0, field_offset(foff)?);
                    } else {
                        enc.sw(t1, Reg::SP, slot(*base) + foff as i32);
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
            emit_element_address(enc, t0, t1, *element_size);
            match (*element_size, *signed) {
                (1, true) => enc.lb(t2, scratch(), 4),
                (1, false) => enc.lbu(t2, scratch(), 4),
                (2, true) => enc.lh(t2, scratch(), 4),
                (2, false) => enc.lhu(t2, scratch(), 4),
                (8, _) => {
                    enc.lw(t2, scratch(), 4);
                    enc.sw(t2, Reg::SP, slot(result));
                    enc.lw(t2, scratch(), 8);
                    enc.sw(t2, Reg::SP, slot(result) + 4);
                    return Ok(());
                }
                _ => enc.lw(t2, scratch(), 4),
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
            emit_element_address(enc, t0, t1, *element_size);
            if *element_size == 8 {
                enc.lw(t2, Reg::SP, slot(*value));
                enc.sw(t2, scratch(), 4);
                enc.lw(t2, Reg::SP, slot(*value) + 4);
                enc.sw(t2, scratch(), 8);
                return Ok(());
            }
            enc.lw(t2, Reg::SP, slot(*value));
            match *element_size {
                1 => enc.sb(t2, scratch(), 4),
                2 => enc.sh(t2, scratch(), 4),
                _ => enc.sw(t2, scratch(), 4),
            }
        }
        Inst::ArrayElemAddr {
            array,
            index,
            element_size,
        } => {
            enc.lw(t0, Reg::SP, slot(*array));
            enc.lw(t1, Reg::SP, slot(*index));
            emit_element_address(enc, t0, t1, *element_size);
            enc.addi(t2, scratch(), 4);
            enc.sw(t2, Reg::SP, slot(result));
        }
        Inst::Alloc {
            handle,
            payload_size,
            ref_offsets,
        } => {
            let has_descriptor = !descriptor_vtable(descriptors, *handle).is_empty()
                || !descriptor_itable(descriptors, *handle).is_empty();
            let desc_label = descriptor_label(
                enc,
                *handle,
                descriptors,
                *payload_size,
                ref_offsets,
                type_descs,
                type_desc_labels,
            );
            enc.li(Reg::A0, (*payload_size + if has_descriptor { 4 } else { 0 }) as i32);
            enc.la(Reg::A1, desc_label);
            emit_alloc_call(enc, alloc, func_labels, relocs, relocate)?;
            let ok = enc.new_label();
            enc.branch(BranchCond::Ne, Reg::A0, Reg::ZERO, ok);
            enc.ebreak();
            enc.bind_label(ok);
            if has_descriptor {
                enc.la(t0, desc_label);
                enc.sw(t0, Reg::A0, 0);
                enc.addi(Reg::A0, Reg::A0, 4);
            }
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
            enc.li(t1, *element_size as i32);
            enc.mul(t0, t0, t1);
            enc.addi(Reg::A0, t0, 4);
            enc.la(Reg::A1, desc_label);
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
            if value_type_words(value_types, result).is_some()
                || args.iter().any(|&a| value_type_words(value_types, a).is_some())
            {
                return Err(LowerError::Unsupported);
            }
            for (i, &arg) in args.iter().enumerate() {
                let target = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(target, Reg::SP, slot(arg));
            }
            emit_call(enc, func_labels, relocs, relocate, *callee)?;
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::CallNative { symbol, args } => {
            if !relocate {
                return Err(LowerError::Unsupported);
            }
            for (i, &arg) in args.iter().enumerate() {
                let target = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(target, Reg::SP, slot(arg));
            }
            emit_call(enc, func_labels, relocs, relocate, EXTERN_SYMBOL_FLAG | *symbol)?;
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::FuncAddr { func } => {
            let label = *func_labels
                .get(*func as usize)
                .ok_or(LowerError::ControlFlowUnsupported)?;
            enc.la(t0, label);
            enc.sw(t0, Reg::SP, slot(result));
        }
        Inst::CallIndirect { target, args, .. } => {
            enc.lw(scratch(), Reg::SP, slot(*target));
            for (i, &arg) in args.iter().enumerate() {
                let r = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(r, Reg::SP, slot(arg));
            }
            enc.jalr(Reg::RA, scratch(), 0);
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::InvokeDelegate { delegate, args, .. } => {
            if args.len() > 7 {
                return Err(LowerError::ControlFlowUnsupported);
            }
            enc.lw(t0, Reg::SP, slot(*delegate));
            enc.lw(scratch(), t0, 4);
            enc.lw(t1, t0, 0);
            let static_call = enc.new_label();
            let do_call = enc.new_label();
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
            enc.jalr(Reg::RA, scratch(), 0);
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
            enc.add(scratch(), t0, t1);
            for (i, &arg) in args.iter().enumerate() {
                let r = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(r, Reg::SP, slot(arg));
            }
            enc.jalr(Reg::RA, scratch(), 0);
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
            enc.lw(scratch(), t1, 0);
            enc.branch(BranchCond::Eq, scratch(), Reg::A0, found);
            enc.addi(t1, t1, 8);
            enc.addi(t2, t2, -1);
            enc.j(loop_top);
            enc.bind_label(notfound);
            enc.ebreak();
            enc.bind_label(found);
            enc.lw(scratch(), t1, 4);
            enc.add(scratch(), t0, scratch());
            for (i, &arg) in args.iter().enumerate() {
                let r = arg_reg(i).ok_or(LowerError::ControlFlowUnsupported)?;
                enc.lw(r, Reg::SP, slot(arg));
            }
            enc.jalr(Reg::RA, scratch(), 0);
            enc.sw(Reg::A0, Reg::SP, slot(result));
        }
        Inst::VirtualFuncAddr { object, slot: vslot } => {
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
            let desc_label =
                descriptor_label(enc, *handle, descriptors, 0, &[], type_descs, type_desc_labels);
            enc.la(t0, desc_label);
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
            let label = match string_blobs.iter().find(|(_, u)| u.as_slice() == utf16.as_ref()) {
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

/// The RISC-V branch condition and operand order so that `b<cond> a, b` is taken exactly when
/// `lhs <op> rhs` holds (the IR branch goes to `if_true` when the comparison is true).
fn branch_for(op: CmpOp, lhs: Reg, rhs: Reg) -> (BranchCond, Reg, Reg) {
    match op {
        CmpOp::Eq => (BranchCond::Eq, lhs, rhs),
        CmpOp::Ne => (BranchCond::Ne, lhs, rhs),
        CmpOp::SignedLt => (BranchCond::Lt, lhs, rhs),
        CmpOp::SignedGe => (BranchCond::Ge, lhs, rhs),
        CmpOp::SignedGt => (BranchCond::Lt, rhs, lhs),
        CmpOp::SignedLe => (BranchCond::Ge, rhs, lhs),
        CmpOp::UnsignedLt => (BranchCond::LtU, lhs, rhs),
        CmpOp::UnsignedGe => (BranchCond::GeU, lhs, rhs),
        CmpOp::UnsignedGt => (BranchCond::LtU, rhs, lhs),
        CmpOp::UnsignedLe => (BranchCond::GeU, rhs, lhs),
    }
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
) -> Result<(), LowerError> {
    let (t0, t1, t2, carry) = (Reg::T0, Reg::T1, Reg::T2, scratch());
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
            emit_i64_shift(enc, slot, op, result, lhs, rhs)?;
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
) -> Result<(), LowerError> {
    let (lo, hi, sh, tmp) = (Reg::T0, Reg::T1, Reg::T2, scratch());
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
) -> Result<(), LowerError> {
    let (t0, t1, t2, tmp) = (Reg::T0, Reg::T1, Reg::T2, scratch());
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
fn descriptor_vtable(descriptors: &[TypeMeta], handle: TypeHandle) -> Vec<Option<u32>> {
    descriptors
        .iter()
        .find(|d| d.handle == handle)
        .map_or_else(Vec::new, |meta| {
            meta.vtable
                .iter()
                .map(|entry| match entry {
                    VtableEntry::Func(index) => Some(*index),
                    VtableEntry::Extern(_) => None,
                })
                .collect()
        })
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
        vtable: descriptor_vtable(descriptors, handle),
        words,
        itable: descriptor_itable(descriptors, handle),
        base: descriptors
            .iter()
            .find(|d| d.handle == handle)
            .and_then(|d| d.base),
    });
    type_desc_labels.push((handle, label));
    label
}

/// The itable of the type `handle` -- `(interface-method tag, module function index)` per entry, laid
/// after the descriptor for `CallInterface` to search. Empty when the type implements no interfaces.
fn descriptor_itable(descriptors: &[TypeMeta], handle: TypeHandle) -> Vec<(u32, Option<u32>)> {
    descriptors
        .iter()
        .find(|d| d.handle == handle)
        .map_or_else(Vec::new, |meta| {
            meta.itable
                .iter()
                .map(|(tag, index)| (*tag, Some(*index)))
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

/// The raw word count (`size / 4`) of `value`'s value type, or `None` if it is not a value type.
/// Used for a struct-valued field copy, which writes only the struct's own words -- not slot padding,
/// which could clobber an adjacent field when the base is a heap object.
fn value_type_words(value_types: &[MirType], value: ValueId) -> Option<u32> {
    match value_types.get(value.index()) {
        Some(MirType::ValueType { size, .. }) => Some(size / 4),
        _ => None,
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
fn emit_element_address(enc: &mut Encoder, array: Reg, index: Reg, element_size: u32) {
    let s = scratch();
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
fn emit_2d_element_address(
    enc: &mut Encoder,
    array: Reg,
    index0: Reg,
    index1: Reg,
    element_size: u32,
    ta: Reg,
    tb: Reg,
) {
    let s = scratch();
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
    enc.mul(s, index0, tb);
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

/// Emits a byte-copy loop (`cpblk`): copies `size` bytes from `src` to `dst`, using `t6` (the array
/// scratch, free outside an array op) as the transfer register. `dst`/`src`/`size` are scratch
/// registers the loop mutates; it is test-first, so a zero size copies nothing.
fn emit_copy_block(enc: &mut Encoder, dst: Reg, src: Reg, size: Reg) {
    let body = enc.new_label();
    let done = enc.new_label();
    enc.branch(BranchCond::Eq, size, Reg::ZERO, done);
    enc.bind_label(body);
    enc.lbu(scratch(), src, 0);
    enc.sb(scratch(), dst, 0);
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
    fn rejects_float_conversions() {
        let func = convert_function(ConvKind::IntToFloat32);
        assert_eq!(lower(&func), Err(LowerError::Unsupported));
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
    fn defers_value_type_call_return() {
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
        assert_eq!(lower_module(&[main, make]), Err(LowerError::Unsupported));
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
                            offset: 8,
                            value: n(0),
                        },
                    ),
                    (n(2), Inst::StaticLoad { offset: 8 }),
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
            },
            TypeMeta {
                handle: TypeHandle(2),
                type_tag: 0,
                vtable: vec![VtableEntry::Func(2)],
                itable: Vec::new(),
                base: Some(TypeHandle(1)),
            },
        ];
        let funcs = [main, leaf(4), leaf(2)];
        assert!(
            lower_module_gc_with_descriptors(&funcs, 0x8000_0004, &descriptors).is_ok(),
            "virtual dispatch with a vtable lowers to RV32IM"
        );
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
            itable: vec![(tag, 1)],
            base: None,
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
            lower_module_gc_with_descriptors(core::slice::from_ref(&func), 0x8000_0004, &[]).is_ok(),
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
            },
            TypeMeta {
                handle: TypeHandle(2),
                type_tag: 0,
                vtable: Vec::new(),
                itable: Vec::new(),
                base: Some(TypeHandle(1)),
            },
            TypeMeta {
                handle: TypeHandle(3),
                type_tag: 0,
                vtable: Vec::new(),
                itable: Vec::new(),
                base: Some(TypeHandle(2)),
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
                    (n(0), Inst::ConstInt { ty: i32t, value: 14 }),
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
        assert!(!triple.defined, "`triple` is an undefined extern the linker resolves");
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
    fn rejects_int64_divide() {
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
                    (
                        n(0),
                        Inst::ConstInt {
                            ty: i64t,
                            value: 3,
                        },
                    ),
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
