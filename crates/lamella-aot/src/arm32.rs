//! Lowering the middle IR to ARMv6-M Thumb machine code.

use alloc::boxed::Box;
use alloc::vec::Vec;

use lamella_asm_arm32::{AssembleError, Cond, Encoder, Label, Reg, RelocKind};
use lamella_ir::{
    BinOp, BlockId, CmpOp, ConvKind, Function, Inst, MirType, StaticOwner, Terminator, ValueId,
};

use crate::target::TargetLowering;

/// Why a [`Function`] could not be lowered by this first ARMv6-M tracer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerError {
    /// The function did not pass [`lamella_ir::verify`].
    NotWellFormed,
    /// A control-flow shape this tracer does not handle yet: a branch target with
    /// parameters (merges must go through Jump) or a dangling block reference.
    ControlFlowUnsupported,
    /// The function needs more stack or registers than this lowering provides: a
    /// branching function with more than eight values, a spilled frame past the
    /// SUB SP reach, or more than four parameters.
    TooManyValues,
    /// A value's type is not an integer; floats and references are not lowered yet.
    NonIntegerValue,
    /// A reference could not reach its target once the code was laid out: a constant's pool entry
    /// past the ~1 KB literal reach, an `adr`'s blob, or a branch, after every relaxation and
    /// veneer the encoder can apply.
    ///
    /// CARRIES THE FAILING SITE, because "too large" on its own does not say WHICH reach ran out,
    /// and the four have nothing in common but the symptom -- a pool word islands beside its load,
    /// an `adr` veneers to an unbounded computed address, an unconditional branch veneers through
    /// the pool, and an already-widened conditional branch has no further tier at all on ARMv6-M.
    /// The encoder knows which one gave up and at what offset; discarding that left a deferred
    /// method needing a disassembly to classify.
    CodeTooLarge {
        /// The failing reference's post-relaxation byte offset and kind, when the encoder named
        /// one. `None` when the failure was not a reach -- an unbound label, an unencodable
        /// operand, or a layout query that could not be answered.
        site: Option<(u32, RelocKind)>,
    },
    /// The function contains a call, which the single-function lowering cannot
    /// resolve; calls are lowered by the program (module) lowering.
    CallUnsupported,
    /// A string literal holds a UTF-16 code unit this build's string storage cannot represent: a LONE
    /// surrogate under `string-utf8`, whose encoding has no form for one. REFUSED rather than replaced
    /// with U+FFFD, because string construction never loses data and the offending unit is known here,
    /// at compile time (see `stringgen::encode_string_bytes`). The default tier and
    /// `string-utf8-wtf8` never produce this.
    UnencodableStringUnit {
        /// The offending UTF-16 code unit.
        unit: u16,
        /// Its index in the literal, in UTF-16 code units.
        index: u32,
    },
}

/// Maps a string blob's encoding refusal into this backend's error, so the four blob-emission sites
/// across the three backends carry the same two facts and the encoder stays the ONE place that decides
/// whether a unit is representable.
fn unencodable<T>(
    result: Result<T, crate::stringgen::UnencodableUnit>,
) -> Result<T, LowerError> {
    result.map_err(|e| LowerError::UnencodableStringUnit {
        unit: e.unit,
        index: e.index,
    })
}

/// Maps an assembly failure into [`LowerError::CodeTooLarge`], KEEPING the site the encoder named.
///
/// Every reach failure arrives here from a `finish` (or a layout query on the same image), and the
/// encoder is the only thing that knows which reference gave up and where. Mapping with `|_|`
/// throws that away and leaves "the function is too large" -- true, useless, and indistinguishable
/// between a pool word that should have islanded and a conditional branch that has no further tier.
/// The other two failures are not reaches and carry no site.
fn reach_failure(err: AssembleError) -> LowerError {
    match err {
        AssembleError::BranchOutOfRange { at, kind } => LowerError::CodeTooLarge {
            site: Some((at, kind)),
        },
        AssembleError::UnboundLabel(_) | AssembleError::UnencodableOperand => {
            LowerError::CodeTooLarge { site: None }
        }
    }
}

/// The ARM condition code that tests a MIR comparison.
fn cmpop_to_cond(op: CmpOp) -> Cond {
    match op {
        CmpOp::Eq => Cond::Eq,
        CmpOp::Ne => Cond::Ne,
        CmpOp::SignedLt => Cond::LessThan,
        CmpOp::SignedLe => Cond::LessOrEqual,
        CmpOp::SignedGt => Cond::GreaterThan,
        CmpOp::SignedGe => Cond::GreaterOrEqual,
        CmpOp::UnsignedLt => Cond::CarryClear,
        CmpOp::UnsignedLe => Cond::LowerOrSame,
        CmpOp::UnsignedGt => Cond::Higher,
        CmpOp::UnsignedGe => Cond::CarrySet,
    }
}

/// Emits `dest = (lhs <op> rhs) ? 1 : 0`. Nothing comes between the compare and the
/// branch: materializing the 0 or 1 sets the condition flags, so it must follow the
/// branch rather than sit between the compare and it.
fn materialize_compare(
    enc: &mut Encoder,
    dest: Reg,
    lhs: Reg,
    rhs: Reg,
    op: CmpOp,
) -> Result<(), AssembleError> {
    enc.cmp_reg(lhs, rhs)?;
    materialize_from_flags(enc, dest, cmpop_to_cond(op))
}

/// Sets `dest` to 1 if the current condition flags satisfy `cond`, else 0 -- a branchful
/// select, since the M0 has no conditional-set. The caller has already set the flags.
fn materialize_from_flags(enc: &mut Encoder, dest: Reg, cond: Cond) -> Result<(), AssembleError> {
    let one = enc.new_label();
    let done = enc.new_label();
    enc.b_cond(cond, one);
    enc.movs_imm(dest, 0)?;
    enc.b(done);
    enc.bind_label(one);
    enc.movs_imm(dest, 1)?;
    enc.bind_label(done);
    Ok(())
}

/// Emits register-to-register moves so they take effect as if simultaneous: each
/// is emitted once nothing else still needs its destination as a source, and a
/// cycle (such as a register swap) is broken by rescuing one value through the
/// scratch register r12 (IP), which the trivial allocator never uses.
fn emit_parallel_move(enc: &mut Encoder, moves: &[(Reg, Reg)]) {
    const SCRATCH: Reg = Reg::R12;
    let mut pending: Vec<(Reg, Reg)> = moves.iter().copied().filter(|(d, s)| d != s).collect();
    while !pending.is_empty() {
        let free = pending
            .iter()
            .position(|(d, _)| !pending.iter().any(|(_, s)| s == d));
        match free {
            Some(i) => {
                let (d, s) = pending.remove(i);
                enc.mov_reg(d, s);
            }
            None => {
                let stuck = pending[0].0;
                enc.mov_reg(SCRATCH, stuck);
                for m in pending.iter_mut() {
                    if m.1 == stuck {
                        m.1 = SCRATCH;
                    }
                }
            }
        }
    }
}

/// Lowers one value-defining instruction into the trivially-allocated registers.
fn lower_inst(
    enc: &mut Encoder,
    pool: &mut Vec<(Label, u32)>,
    result: ValueId,
    inst: &Inst,
    assign: &impl Fn(ValueId) -> Reg,
) -> Result<(), LowerError> {
    match inst {
        Inst::PyIntrinsic { .. } => return Err(LowerError::CallUnsupported),
        Inst::FuncAddr { .. }
        | Inst::VirtualFuncAddr { .. }
        | Inst::CallIndirect { .. }
        | Inst::CallNative { .. }
        | Inst::InvokeDelegate { .. }
        | Inst::TypeDescLiteral { .. }
        | Inst::PInvoke { .. } => {
            return Err(LowerError::CallUnsupported);
        }
        Inst::CopyBlock { .. } | Inst::FillBlock { .. } => {
            return Err(LowerError::CallUnsupported);
        }
        Inst::ConstInt { value, .. } => {
            if let Ok(imm) = u8::try_from(*value) {
                enc.movs_imm(assign(result), imm)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                let entry = enc.new_label();
                enc.ldr_literal(assign(result), entry)
                    .map_err(|_| LowerError::TooManyValues)?;
                pool.push((entry, *value as u32));
            }
        }
        Inst::Binary { op, lhs, rhs } => {
            let (d, a, b) = (assign(result), assign(*lhs), assign(*rhs));
            let emitted = match op {
                BinOp::Add => enc.adds(d, a, b),
                BinOp::Sub => enc.subs(d, a, b),
                BinOp::And => commutative(enc, d, a, b, Encoder::ands),
                BinOp::Or => commutative(enc, d, a, b, Encoder::orrs),
                BinOp::Xor => commutative(enc, d, a, b, Encoder::eors),
                BinOp::Mul => commutative(enc, d, a, b, Encoder::muls),
                BinOp::Shl => shift(enc, d, a, b, Encoder::lsls_reg),
                BinOp::ShrSigned => shift(enc, d, a, b, Encoder::asrs_reg),
                BinOp::ShrUnsigned => shift(enc, d, a, b, Encoder::lsrs_reg),
                BinOp::DivSigned | BinOp::DivUnsigned | BinOp::RemSigned | BinOp::RemUnsigned => {
                    return Err(LowerError::CallUnsupported);
                }
            };
            emitted.map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::Compare { op, lhs, rhs } => {
            materialize_compare(enc, assign(result), assign(*lhs), assign(*rhs), *op)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::Store {
            address,
            value,
            width,
        } => {
            emit_sized_store(enc, assign(*value), assign(*address), *width)?;
        }
        Inst::Load {
            address,
            width,
            signed,
        } => {
            emit_sized_load(enc, assign(result), assign(*address), *width, *signed)?;
        }
        Inst::Convert { value, kind } => {
            if matches!(
                kind,
                ConvKind::Float32ToInt
                    | ConvKind::IntToFloat32
                    | ConvKind::Float64ToInt
                    | ConvKind::IntToFloat64
                    | ConvKind::LongToFloat64
                    | ConvKind::Float32ToFloat64
                    | ConvKind::Float64ToFloat32
                    | ConvKind::LongToFloat32
                    | ConvKind::UIntToFloat64
                    | ConvKind::ULongToFloat64
            ) {
                return Err(LowerError::CallUnsupported);
            }
            extend_for(enc, assign(result), assign(*value), *kind)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::Widen { .. }
        | Inst::Truncate { .. }
        | Inst::InitStruct
        | Inst::FieldLoad { .. }
        | Inst::FieldStore { .. }
        | Inst::FieldLoadNarrow { .. }
        | Inst::FieldStoreNarrow { .. }
        | Inst::FieldAddr { .. }
        | Inst::CopyStruct { .. } => return Err(LowerError::CallUnsupported),
        Inst::Call { .. }
        | Inst::CallVirtual { .. }
        | Inst::CallInterface { .. }
        | Inst::CastClassScan { .. }
        | Inst::InterfaceHasTag { .. }
        | Inst::TypeName { .. } => {
            return Err(LowerError::CallUnsupported);
        }
        Inst::SemihostWrite { .. }
        | Inst::WriteInt { .. }
        | Inst::StringLiteral { .. }
        | Inst::StringEquals { .. }
        | Inst::StringConcat { .. }
        | Inst::IntToString { .. }
        | Inst::Alloc { .. }
        | Inst::AllocLike { .. }
        | Inst::AllocDescribed { .. }
        | Inst::AllocArray { .. }
        | Inst::ArrayLoad { .. }
        | Inst::ArrayStore { .. }
        | Inst::ArrayElemAddr { .. }
        | Inst::AllocArray2D { .. }
        | Inst::Array2DLoad { .. }
        | Inst::Array2DStore { .. }
        | Inst::AllocArrayMD { .. }
        | Inst::ArrayMDLoad { .. }
        | Inst::ArrayMDStore { .. }
        | Inst::StaticLoad { .. }
        | Inst::StaticStore { .. }
        | Inst::LoadTypeDesc { .. }
        | Inst::TypeDescAddr { .. } => {
            return Err(LowerError::CallUnsupported);
        }
    }
    Ok(())
}

/// Emits a commutative two-address operation `d = a op b`, where `d` may reuse the
/// register of `a` or `b`: keep `d` holding one operand, then combine with the other.
fn commutative(
    enc: &mut Encoder,
    d: Reg,
    a: Reg,
    b: Reg,
    op: impl Fn(&mut Encoder, Reg, Reg) -> Result<(), AssembleError>,
) -> Result<(), AssembleError> {
    let other = if d == b {
        a
    } else {
        if d != a {
            enc.mov_reg(d, a);
        }
        b
    };
    op(enc, d, other)
}

/// Emits a non-commutative shift `d = a shifted by b`, where `d` may reuse `a` or
/// `b`. If `d` holds `b`, the shift amount is rescued through the scratch register
/// before `d` is overwritten with `a`.
fn shift(
    enc: &mut Encoder,
    d: Reg,
    a: Reg,
    b: Reg,
    op: impl Fn(&mut Encoder, Reg, Reg) -> Result<(), AssembleError>,
) -> Result<(), AssembleError> {
    if d == b && d != a {
        enc.mov_reg(Reg::R12, b);
        enc.mov_reg(d, a);
        op(enc, d, Reg::R12)
    } else {
        if d != a {
            enc.mov_reg(d, a);
        }
        op(enc, d, b)
    }
}

/// Emits the sign/zero-extend that realizes a [`ConvKind`] (`d = ext(m)`).
fn extend_for(enc: &mut Encoder, rd: Reg, rm: Reg, kind: ConvKind) -> Result<(), AssembleError> {
    match kind {
        ConvKind::SignExtend8 => enc.sxtb(rd, rm),
        ConvKind::ZeroExtend8 => enc.uxtb(rd, rm),
        ConvKind::SignExtend16 => enc.sxth(rd, rm),
        ConvKind::ZeroExtend16 => enc.uxth(rd, rm),
        ConvKind::Float32ToInt
        | ConvKind::IntToFloat32
        | ConvKind::Float64ToInt
        | ConvKind::IntToFloat64
        | ConvKind::LongToFloat64
        | ConvKind::Float32ToFloat64
        | ConvKind::Float64ToFloat32
        | ConvKind::LongToFloat32
        | ConvKind::UIntToFloat64
        | ConvKind::ULongToFloat64 => Ok(()),
        ConvKind::IntToRef | ConvKind::RefToInt | ConvKind::ToNativeInt => {
            if rd != rm {
                enc.mov_reg(rd, rm);
            }
            Ok(())
        }
    }
}

/// Loads a 32-bit constant into `reg` -- inline if it fits a `MOVS #imm8`, else from the
/// literal pool.
/// Leaves the ADDRESS of static-region byte `offset` (in `owner`'s region) in r0. On the
/// RELOCATING (object) path the address is a linker-resolved pool word: an OWN offset 0 -- the
/// reserved MIR-level EH-tag marker (`cil::G_EXCEPTION_TAG_OFFSET`) -- resolves to the ONE
/// VES-global `__lamella_eh_tag` word every assembly shares (a per-assembly row 0 would break a
/// cross-assembly throw/catch), any other OWN offset resolves to THIS assembly's own region
/// symbol plus the slot's addend, so two linked assemblies' statics can never stomp each other,
/// and a REFERENCE-owned offset (a cross-assembly `ldsfld`/`stsfld`) resolves to the
/// OWNING assembly's region symbol -- its slot 0 is reserved like every region's, so a reference
/// offset is never 0 and needs no EH split. The linker-free flat path keeps the fixed
/// `STATIC_FIELD_BASE` layout -- a self-contained single-assembly image, where another assembly's
/// region does not exist to address.
fn static_slot_addr(
    enc: &mut Encoder,
    pool: &mut Vec<(Label, u32)>,
    sym_pool: &mut Vec<(Label, u32, i32)>,
    relocate: bool,
    owner: StaticOwner,
    offset: u32,
) -> Result<(), LowerError> {
    if !relocate {
        return match owner {
            StaticOwner::Own => load_const_word(enc, pool, Reg::R0, STATIC_FIELD_BASE + offset),
            StaticOwner::Reference(_) => Err(LowerError::CallUnsupported),
        };
    }
    let label = enc.new_label();
    match owner {
        StaticOwner::Own if offset == crate::cil::G_EXCEPTION_TAG_OFFSET => {
            sym_pool.push((label, EH_TAG_SYMBOL_FLAG, 0));
        }
        StaticOwner::Own => sym_pool.push((label, STATICS_BASE_SYMBOL_FLAG, offset as i32)),
        StaticOwner::Reference(ordinal) => {
            assert!(
                ordinal < 16,
                "statics symbol out of encoding range (reference ordinal {ordinal})"
            );
            sym_pool.push((
                label,
                STATICS_BASE_SYMBOL_FLAG | (u32::from(ordinal) + 1),
                offset as i32,
            ));
        }
    }
    enc.ldr_literal(Reg::R0, label)
        .map_err(|_| LowerError::TooManyValues)
}

/// Loads spill-slot word `off` into `rt`. Within the Thumb-1 `LDR [SP,#imm8*4]` reach (<= 1020)
/// this is that one instruction; past it the address SELF-ASSEMBLES through the destination --
/// build the offset in `rt` (movs/lsls/adds), rebase onto SP (`add rt, sp, rt`), load through it
/// -- so a LOAD never needs a scratch register. The sequence clobbers flags; slot traffic sits at
/// instruction boundaries (operand loads before any compare, result stores after its
/// materialization), so no live flags exist there.
fn slot_load(enc: &mut Encoder, rt: Reg, off: u16) -> Result<(), LowerError> {
    if off <= 1020 {
        return enc.ldr_sp(rt, off).map_err(|_| LowerError::TooManyValues);
    }
    let e = |_| LowerError::TooManyValues;
    enc.movs_imm(rt, (off >> 8) as u8).map_err(e)?;
    enc.lsls_imm(rt, rt, 8).map_err(e)?;
    enc.adds_imm8(rt, (off & 0xff) as u8).map_err(e)?;
    enc.add_sp_reg(rt).map_err(e)?;
    enc.ldr_imm(rt, rt, 0).map_err(e)
}

/// Leaves the ADDRESS of spill-slot byte `off` in `rd` -- the `ADD Rd, SP, #imm8*4` shape
/// (a slot-resident struct's field address, an sret pointer, the py argv base), extended past the
/// encoding's 1020 reach by self-assembling the offset in `rd` like [`slot_load`]. No scratch,
/// no memory access.
fn slot_addr(enc: &mut Encoder, rd: Reg, off: u16) -> Result<(), LowerError> {
    if off <= 1020 {
        return enc.add_sp_imm(rd, off).map_err(|_| LowerError::TooManyValues);
    }
    let e = |_| LowerError::TooManyValues;
    enc.movs_imm(rd, (off >> 8) as u8).map_err(e)?;
    enc.lsls_imm(rd, rd, 8).map_err(e)?;
    enc.adds_imm8(rd, (off & 0xff) as u8).map_err(e)?;
    enc.add_sp_reg(rd).map_err(e)
}

/// Stores `rt` into spill-slot word `off` -- [`slot_load`]'s twin, except a far STORE must build
/// the address in a SCRATCH register (the value occupies `rt`). SCRATCH POLICY: the caller names
/// a low register that is dead at the store site. The spilled path shuttles operands through
/// r0-r3 and every result store happens after its instruction's emission has consumed them, so
/// r3 (or r2 beside an r0:r1 pair, r1 in the post-spill prologue) is free by construction at
/// every converted site -- r4-r7 are NOT candidates (only pushed for delegate bodies). Within
/// SP-immediate reach the scratch is untouched.
fn slot_store(enc: &mut Encoder, rt: Reg, off: u16, scratch: Reg) -> Result<(), LowerError> {
    if off <= 1020 {
        return enc.str_sp(rt, off).map_err(|_| LowerError::TooManyValues);
    }
    debug_assert_ne!(rt, scratch, "the far store builds its address in the scratch");
    let e = |_| LowerError::TooManyValues;
    enc.movs_imm(scratch, (off >> 8) as u8).map_err(e)?;
    enc.lsls_imm(scratch, scratch, 8).map_err(e)?;
    enc.adds_imm8(scratch, (off & 0xff) as u8).map_err(e)?;
    enc.add_sp_reg(scratch).map_err(e)?;
    enc.str_imm(rt, scratch, 0).map_err(e)
}

/// Walks `addr` forward until `offset` fits the narrow load/store's imm5 reach (31 bytes for a
/// byte access, 62 for a halfword), returning the residual offset. `addr` is a scratch COPY of
/// the base (the narrow arms load/derive it fresh), so advancing it in place is free.
fn narrow_reach(enc: &mut Encoder, addr: Reg, offset: u32, reach: u32) -> Result<u8, LowerError> {
    let mut off = offset;
    while off > reach {
        let step = off.saturating_sub(reach).min(255) as u8;
        enc.adds_imm8(addr, step)
            .map_err(|_| LowerError::TooManyValues)?;
        off -= u32::from(step);
    }
    Ok(off as u8)
}

/// Loads the `size`-byte (1 or 2) field at `addr + offset` into `rt`, zero- or sign-extended to
/// the I32 result -- `LDRB`/`LDRH` plus `SXTB`/`SXTH` for the signed widths (Thumb-1 has no
/// immediate-offset LDRSB/LDRSH). A halfword field's offset is even by layout; an odd one is an
/// UnencodableOperand, surfacing as a LOUD lowering error rather than a rotated read.
fn narrow_load_at(
    enc: &mut Encoder,
    rt: Reg,
    addr: Reg,
    offset: u32,
    size: u8,
    signed: bool,
) -> Result<(), LowerError> {
    let e = |_| LowerError::TooManyValues;
    match size {
        1 => {
            let off = narrow_reach(enc, addr, offset, 31)?;
            enc.ldrb_imm(rt, addr, off).map_err(e)?;
            if signed {
                enc.sxtb(rt, rt).map_err(e)?;
            }
        }
        2 => {
            let off = narrow_reach(enc, addr, offset, 62)?;
            enc.ldrh_imm(rt, addr, off).map_err(e)?;
            if signed {
                enc.sxth(rt, rt).map_err(e)?;
            }
        }
        _ => return Err(LowerError::CallUnsupported),
    }
    Ok(())
}

/// Stores the low `size` bytes (1 or 2) of `rt` at `addr + offset` -- [`narrow_load_at`]'s twin
/// (`STRB`/`STRH`), the store that CANNOT be word-wide: it would stomp the neighboring fields.
fn narrow_store_at(
    enc: &mut Encoder,
    rt: Reg,
    addr: Reg,
    offset: u32,
    size: u8,
) -> Result<(), LowerError> {
    let e = |_| LowerError::TooManyValues;
    match size {
        1 => {
            let off = narrow_reach(enc, addr, offset, 31)?;
            enc.strb_imm(rt, addr, off).map_err(e)
        }
        2 => {
            let off = narrow_reach(enc, addr, offset, 62)?;
            enc.strh_imm(rt, addr, off).map_err(e)
        }
        _ => Err(LowerError::CallUnsupported),
    }
}

fn load_const_word(
    enc: &mut Encoder,
    pool: &mut Vec<(Label, u32)>,
    reg: Reg,
    value: u32,
) -> Result<(), LowerError> {
    if let Ok(imm) = u8::try_from(value) {
        enc.movs_imm(reg, imm)
            .map_err(|_| LowerError::TooManyValues)?;
    } else {
        let entry = enc.new_label();
        enc.ldr_literal(reg, entry)
            .map_err(|_| LowerError::TooManyValues)?;
        pool.push((entry, value));
    }
    Ok(())
}

/// The 8-byte-aligned outgoing STACK bytes a call's arguments need past the argument registers
/// (`start_reg`..r3) -- the planning half of [`load_call_args`] with no emission. The spilled path
/// reserves the widest such area at the frame BOTTOM (see [`out_args_bytes`]) so a call needs no
/// around-call SP motion. Kept in lockstep with [`load_call_args`]'s own `stack_used` tally.
fn outgoing_stack_bytes(value_types: &[MirType], args: &[ValueId], start_reg: u8) -> u16 {
    let mut reg = start_reg;
    let mut stack_used = 0u16;
    for &a in args {
        let ty = value_types.get(a.0 as usize).copied();
        let words = ty.map_or(1, |t| (t.stack_slot_bytes() / 4).max(1));
        if matches!(ty, Some(MirType::I64 | MirType::F64)) && reg % 2 == 1 {
            reg += 1;
        }
        for _ in 0..words {
            if reg < 4 {
                reg += 1;
            } else {
                stack_used += 4;
            }
        }
    }
    (stack_used + 7) & !7
}

/// The outgoing-args area reserved at the BOTTOM of a spilled frame: the widest stack-argument area
/// any call in `func` needs, so SP stays FIXED across every call. A per-call `sub sp`/`add sp` would
/// move SP while the callee can park, and a GC walk of this (caller) frame would then read its roots
/// against the wrong SP. `0` when no call spills arguments, keeping the frame byte-identical. Each
/// call is measured at the SAME `start_reg` its lowering hands to [`load_call_args`], so the
/// reservation always covers the actual overflow: the dispatch kinds (`CallVirtual`/`CallInterface`/
/// `CallIndirect`) pass their receiver as `args[0]` from r0 (start_reg 0), while an `InvokeDelegate`
/// to an instance target and a big-struct (sret) `Call` both reserve r0 for the `this`/return pointer
/// and shift their args to r1.. (start_reg 1). For <=3 args both starts spill zero words, so only a
/// 4+-arg delegate or sret call grows the area -- every existing frame stays byte-identical.
fn out_args_bytes(func: &Function) -> u16 {
    func.blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|(result, i)| match i {
            Inst::Call { args, .. }
                if matches!(
                    func.value_type(*result),
                    Some(MirType::ValueType { size, .. }) if size > 4
                ) =>
            {
                Some(outgoing_stack_bytes(&func.value_types, args, 1))
            }
            Inst::Call { args, .. }
            | Inst::CallIndirect { args, .. }
            | Inst::CallNative { args, .. }
            | Inst::CallVirtual { args, .. }
            | Inst::CallInterface { args, .. } => {
                Some(outgoing_stack_bytes(&func.value_types, args, 0))
            }
            Inst::InvokeDelegate { args, .. } => {
                Some(outgoing_stack_bytes(&func.value_types, args, 1))
            }
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

/// Loads call arguments per the AAPCS: each word into a register (`start_reg`..r3, a
/// doubleword even-aligned), then the remainder into the reserved outgoing-args area at the frame
/// bottom (`[sp + k]`, from [`out_args_bytes`]). SP does NOT move around the call, so a value slot
/// stays at its fixed `slot(a)` offset and a GC walk of a parking call reads this frame correctly.
fn load_call_args(
    enc: &mut Encoder,
    value_types: &[MirType],
    slot: &impl Fn(ValueId) -> u16,
    args: &[ValueId],
    start_reg: u8,
) -> Result<(), LowerError> {
    let mut reg = start_reg;
    let mut reg_plan: Vec<(u8, ValueId, u16)> = Vec::new();
    let mut stack_plan: Vec<(u16, ValueId, u16)> = Vec::new();
    let mut stack_used = 0u16;
    for &a in args {
        let ty = value_types.get(a.0 as usize).copied();
        let words = ty.map_or(1, |t| (t.stack_slot_bytes() / 4).max(1));
        if matches!(ty, Some(MirType::I64 | MirType::F64)) && reg % 2 == 1 {
            reg += 1;
        }
        for w in 0..words {
            let woff = (w as u16) * 4;
            if reg < 4 {
                reg_plan.push((reg, a, woff));
                reg += 1;
            } else {
                stack_plan.push((stack_used, a, woff));
                stack_used += 4;
            }
        }
    }
    for &(stack_off, a, woff) in &stack_plan {
        slot_load(enc, Reg::R3, slot(a) + woff)?;
        enc.str_sp(Reg::R3, stack_off)
            .map_err(|_| LowerError::TooManyValues)?;
    }
    for &(r, a, woff) in &reg_plan {
        let dst = Reg::new(r).ok_or(LowerError::CallUnsupported)?;
        slot_load(enc, dst, slot(a) + woff)?;
    }
    Ok(())
}

/// Whether a field-access base is a pointer to dereference -- a managed pointer (`this`) or
/// a heap object reference -- rather than a value type held inline in its own stack slot.
fn is_pointer_base(value_types: &[MirType], base: ValueId) -> bool {
    matches!(
        value_types.get(base.0 as usize),
        Some(MirType::ManagedPtr | MirType::ObjectRef)
    )
}

/// Lowers one instruction of a spilled function: load its operands from their
/// stack slots into scratch registers (r0-r3), compute, and leave the result in
/// r0 for the caller to store.
#[allow(clippy::too_many_arguments)]
fn lower_spilled_inst(
    enc: &mut Encoder,
    pool: &mut Vec<(Label, u32)>,
    sym_pool: &mut Vec<(Label, u32, i32)>,
    strings: &mut Vec<(Label, Box<[u8]>)>,
    string_blobs: &mut Vec<(Label, Box<[u16]>)>,
    value_types: &[MirType],
    slot: &impl Fn(ValueId) -> u16,
    inst: &Inst,
    result_ty: Option<MirType>,
    func_labels: &[Label],
    relocate: bool,
    blob_table: Option<&[Box<[u16]>]>,
    console_symbol: Option<u32>,
) -> Result<Option<u32>, LowerError> {
    match inst {
        Inst::PyIntrinsic { .. } => return Err(LowerError::CallUnsupported),
        Inst::ConstInt {
            ty: MirType::I64 | MirType::F64,
            value,
        } => {
            load_const_word(enc, pool, Reg::R0, *value as u32)?;
            load_const_word(enc, pool, Reg::R1, (*value >> 32) as u32)?;
        }
        Inst::ConstInt { value, .. } => {
            load_const_word(enc, pool, Reg::R0, *value as u32)?;
        }
        Inst::Binary { op, lhs, rhs } if value_types.get(lhs.0 as usize) == Some(&MirType::I64) => {
            let (a, b) = (slot(*lhs), slot(*rhs));
            slot_load(enc, Reg::R0, a)?;
            slot_load(enc, Reg::R1, a + 4)?;
            slot_load(enc, Reg::R2, b)?;
            slot_load(enc, Reg::R3, b + 4)?;
            match op {
                BinOp::Add => {
                    enc.adds(Reg::R0, Reg::R0, Reg::R2)
                        .map_err(|_| LowerError::TooManyValues)?;
                    enc.adcs(Reg::R1, Reg::R3)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
                BinOp::Sub => {
                    enc.subs(Reg::R0, Reg::R0, Reg::R2)
                        .map_err(|_| LowerError::TooManyValues)?;
                    enc.sbcs(Reg::R1, Reg::R3)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
                BinOp::And => {
                    enc.ands(Reg::R0, Reg::R2)
                        .map_err(|_| LowerError::TooManyValues)?;
                    enc.ands(Reg::R1, Reg::R3)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
                BinOp::Or => {
                    enc.orrs(Reg::R0, Reg::R2)
                        .map_err(|_| LowerError::TooManyValues)?;
                    enc.orrs(Reg::R1, Reg::R3)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
                BinOp::Xor => {
                    enc.eors(Reg::R0, Reg::R2)
                        .map_err(|_| LowerError::TooManyValues)?;
                    enc.eors(Reg::R1, Reg::R3)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
                BinOp::Mul => emit_mul64(enc)?,
                BinOp::Shl => emit_shl64(enc)?,
                BinOp::ShrSigned => emit_shr64(enc, true)?,
                BinOp::ShrUnsigned => emit_shr64(enc, false)?,
                BinOp::DivSigned => emit_divmod64(enc, true, false)?,
                BinOp::DivUnsigned => emit_divmod64(enc, false, false)?,
                BinOp::RemSigned => emit_divmod64(enc, true, true)?,
                BinOp::RemUnsigned => emit_divmod64(enc, false, true)?,
            }
        }
        Inst::Binary { op, lhs, rhs } => {
            if matches!(
                value_types.get(lhs.0 as usize),
                Some(MirType::F32 | MirType::F64)
            ) {
                return Err(LowerError::CallUnsupported);
            }
            slot_load(enc, Reg::R0, slot(*lhs))?;
            slot_load(enc, Reg::R1, slot(*rhs))?;
            match op {
                BinOp::DivSigned => emit_divmod32(enc, true, false)?,
                BinOp::DivUnsigned => emit_divmod32(enc, false, false)?,
                BinOp::RemSigned => emit_divmod32(enc, true, true)?,
                BinOp::RemUnsigned => emit_divmod32(enc, false, true)?,
                _ => {
                    let emitted = match op {
                        BinOp::Add => enc.adds(Reg::R0, Reg::R0, Reg::R1),
                        BinOp::Sub => enc.subs(Reg::R0, Reg::R0, Reg::R1),
                        BinOp::And => enc.ands(Reg::R0, Reg::R1),
                        BinOp::Or => enc.orrs(Reg::R0, Reg::R1),
                        BinOp::Xor => enc.eors(Reg::R0, Reg::R1),
                        BinOp::Mul => enc.muls(Reg::R0, Reg::R1),
                        BinOp::Shl => enc.lsls_reg(Reg::R0, Reg::R1),
                        BinOp::ShrSigned => enc.asrs_reg(Reg::R0, Reg::R1),
                        BinOp::ShrUnsigned => enc.lsrs_reg(Reg::R0, Reg::R1),
                        _ => unreachable!("div/rem handled above"),
                    };
                    emitted.map_err(|_| LowerError::TooManyValues)?;
                }
            }
        }
        Inst::Compare { op, lhs, rhs }
            if value_types.get(lhs.0 as usize) == Some(&MirType::I64) =>
        {
            if matches!(op, CmpOp::Eq | CmpOp::Ne) {
                let (a, b) = (slot(*lhs), slot(*rhs));
                slot_load(enc, Reg::R0, a)?;
                slot_load(enc, Reg::R1, a + 4)?;
                slot_load(enc, Reg::R2, b)?;
                slot_load(enc, Reg::R3, b + 4)?;
                enc.eors(Reg::R0, Reg::R2)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.eors(Reg::R1, Reg::R3)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.orrs(Reg::R0, Reg::R1)
                    .map_err(|_| LowerError::TooManyValues)?;
                materialize_from_flags(enc, Reg::R0, cmpop_to_cond(*op))
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                let (swap, cond) = match op {
                    CmpOp::SignedLt => (false, Cond::LessThan),
                    CmpOp::SignedGe => (false, Cond::GreaterOrEqual),
                    CmpOp::SignedGt => (true, Cond::LessThan),
                    CmpOp::SignedLe => (true, Cond::GreaterOrEqual),
                    CmpOp::UnsignedLt => (false, Cond::CarryClear),
                    CmpOp::UnsignedGe => (false, Cond::CarrySet),
                    CmpOp::UnsignedGt => (true, Cond::CarryClear),
                    CmpOp::UnsignedLe => (true, Cond::CarrySet),
                    CmpOp::Eq | CmpOp::Ne => (false, Cond::Eq),
                };
                let (min, sub) = if swap { (*rhs, *lhs) } else { (*lhs, *rhs) };
                let (m, s) = (slot(min), slot(sub));
                slot_load(enc, Reg::R0, m)?;
                slot_load(enc, Reg::R1, m + 4)?;
                slot_load(enc, Reg::R2, s)?;
                slot_load(enc, Reg::R3, s + 4)?;
                enc.subs(Reg::R0, Reg::R0, Reg::R2)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.sbcs(Reg::R1, Reg::R3)
                    .map_err(|_| LowerError::TooManyValues)?;
                materialize_from_flags(enc, Reg::R0, cond)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::Compare { op, lhs, rhs } => {
            if matches!(
                value_types.get(lhs.0 as usize),
                Some(MirType::F32 | MirType::F64)
            ) {
                return Err(LowerError::CallUnsupported);
            }
            slot_load(enc, Reg::R0, slot(*lhs))?;
            slot_load(enc, Reg::R1, slot(*rhs))?;
            materialize_compare(enc, Reg::R0, Reg::R0, Reg::R1, *op)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::Call { callee, args } => {
            load_call_args(enc, value_types, slot, args, 0)?;
            if relocate {
                enc.bl_symbol(*callee);
            } else {
                let target = *func_labels
                    .get(*callee as usize)
                    .ok_or(LowerError::CallUnsupported)?;
                enc.bl(target);
            }
            let return_pc = enc.safepoint_label();
            return Ok(Some(return_pc));
        }
        Inst::FuncAddr { func } => {
            if !relocate {
                return Err(LowerError::CallUnsupported);
            }
            let label = enc.new_label();
            sym_pool.push((label, *func, 0));
            enc.ldr_literal(Reg::R0, label)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::VirtualFuncAddr { object, slot: vslot } => {
            let entry_offset = vslot
                .checked_mul(4)
                .and_then(|x| x.checked_add(4))
                .filter(|&offset| offset <= 255)
                .ok_or(LowerError::TooManyValues)?;
            slot_load(enc, Reg::R0, slot(*object))?;
            enc.subs_imm8(Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R0, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.mov_reg(Reg::R1, Reg::R0);
            enc.subs_imm8(Reg::R1, entry_offset as u8)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R1, Reg::R1, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds(Reg::R0, Reg::R0, Reg::R1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds_imm8(Reg::R0, 1)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::TypeDescLiteral { handle, .. } => {
            let label = enc.new_label();
            sym_pool.push((label, DESC_SYMBOL_FLAG | *handle, 0));
            enc.ldr_literal(Reg::R0, label)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::CallIndirect { target, args, .. } => {
            slot_load(enc, Reg::R0, slot(*target))?;
            enc.mov_reg(Reg::R12, Reg::R0);
            load_call_args(enc, value_types, slot, args, 0)?;
            enc.blx(Reg::R12);
            let return_pc = enc.safepoint_label();
            return Ok(Some(return_pc));
        }
        Inst::InvokeDelegate { delegate, args, .. } => {
            let mloop = enc.new_label();
            let multi = enc.new_label();
            let dispatch = enc.new_label();
            let d_static = enc.new_label();
            let d_call = enc.new_label();
            let mdone = enc.new_label();
            let e = |_| LowerError::TooManyValues;
            enc.movs_imm(Reg::R4, 0).map_err(e)?;
            enc.bind_label(mloop);
            slot_load(enc, Reg::R3, slot(*delegate))?;
            enc.ldr_imm(Reg::R1, Reg::R3, 8).map_err(e)?;
            enc.cmp_imm(Reg::R1, 0).map_err(e)?;
            enc.b_cond(Cond::Ne, multi);
            enc.cmp_imm(Reg::R4, 1).map_err(e)?;
            enc.b_cond(Cond::GreaterOrEqual, mdone);
            enc.b(dispatch);
            enc.bind_label(multi);
            enc.ldr_imm(Reg::R2, Reg::R1, 0).map_err(e)?;
            enc.cmp_reg(Reg::R4, Reg::R2).map_err(e)?;
            enc.b_cond(Cond::GreaterOrEqual, mdone);
            enc.lsls_imm(Reg::R2, Reg::R4, 2).map_err(e)?;
            enc.adds_imm3(Reg::R2, Reg::R2, 4).map_err(e)?;
            enc.ldr_reg(Reg::R3, Reg::R1, Reg::R2).map_err(e)?;
            enc.bind_label(dispatch);
            enc.ldr_imm(Reg::R2, Reg::R3, 4).map_err(e)?;
            enc.mov_reg(Reg::R12, Reg::R2);
            enc.ldr_imm(Reg::R0, Reg::R3, 0).map_err(e)?;
            enc.cmp_imm(Reg::R0, 0).map_err(e)?;
            enc.b_cond(Cond::Eq, d_static);
            load_call_args(enc, value_types, slot, args, 1)?;
            enc.b(d_call);
            enc.bind_label(d_static);
            load_call_args(enc, value_types, slot, args, 0)?;
            enc.bind_label(d_call);
            enc.blx(Reg::R12);
            let return_pc = enc.safepoint_label();
            enc.movs_reg(Reg::R5, Reg::R0).map_err(e)?;
            enc.adds_imm8(Reg::R4, 1).map_err(e)?;
            enc.b(mloop);
            enc.bind_label(mdone);
            enc.movs_reg(Reg::R0, Reg::R5).map_err(e)?;
            return Ok(Some(return_pc));
        }
        Inst::PInvoke { .. } => {
            return Err(LowerError::CallUnsupported);
        }
        Inst::CallNative { symbol, args } => {
            if !relocate {
                return Err(LowerError::CallUnsupported);
            }
            load_call_args(enc, value_types, slot, args, 0)?;
            enc.bl_symbol(EXTERN_SYMBOL_FLAG | *symbol);
            let return_pc = enc.safepoint_label();
            return Ok(Some(return_pc));
        }
        Inst::CallVirtual {
            slot: vtable_slot,
            args,
            ..
        } => {
            let receiver = *args.first().ok_or(LowerError::CallUnsupported)?;
            let entry_offset = vtable_slot
                .checked_mul(4)
                .and_then(|x| x.checked_add(4))
                .filter(|&offset| offset <= 255)
                .ok_or(LowerError::TooManyValues)?;
            slot_load(enc, Reg::R0, slot(receiver))?;
            enc.subs_imm8(Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R0, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.mov_reg(Reg::R1, Reg::R0);
            enc.subs_imm8(Reg::R1, entry_offset as u8)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R1, Reg::R1, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds(Reg::R0, Reg::R0, Reg::R1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds_imm8(Reg::R0, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.mov_reg(Reg::R12, Reg::R0);
            load_call_args(enc, value_types, slot, args, 0)?;
            enc.blx(Reg::R12);
            let return_pc = enc.safepoint_label();
            return Ok(Some(return_pc));
        }
        Inst::CallInterface { tag, args, .. } => {
            let receiver = *args.first().ok_or(LowerError::CallUnsupported)?;
            slot_load(enc, Reg::R0, slot(receiver))?;
            enc.subs_imm8(Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R0, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R1, Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.lsls_imm(Reg::R1, Reg::R1, 2)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds_imm8(Reg::R1, 16)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds(Reg::R1, Reg::R0, Reg::R1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R2, Reg::R1, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds_imm8(Reg::R1, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            load_const_word(enc, pool, Reg::R3, *tag)?;
            let search = enc.new_label();
            let found = enc.new_label();
            enc.bind_label(search);
            enc.ldr_imm(Reg::R0, Reg::R1, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.cmp_reg(Reg::R0, Reg::R3)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Eq, found);
            enc.adds_imm8(Reg::R1, 8)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.subs_imm8(Reg::R2, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Ne, search);
            enc.udf(0);
            enc.bind_label(found);
            enc.ldr_imm(Reg::R0, Reg::R1, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            slot_load(enc, Reg::R3, slot(receiver))?;
            enc.subs_imm8(Reg::R3, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R3, Reg::R3, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds(Reg::R0, Reg::R3, Reg::R0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds_imm8(Reg::R0, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.mov_reg(Reg::R12, Reg::R0);
            load_call_args(enc, value_types, slot, args, 0)?;
            enc.blx(Reg::R12);
            let return_pc = enc.safepoint_label();
            return Ok(Some(return_pc));
        }
        Inst::CastClassScan { args } => {
            let start = *args.first().ok_or(LowerError::CallUnsupported)?;
            let target = *args.get(1).ok_or(LowerError::CallUnsupported)?;
            slot_load(enc, Reg::R0, slot(start))?;
            slot_load(enc, Reg::R2, slot(target))?;
            let search = enc.new_label();
            let found = enc.new_label();
            let miss = enc.new_label();
            let done = enc.new_label();
            enc.cmp_imm(Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Eq, miss);
            enc.bind_label(search);
            enc.cmp_reg(Reg::R0, Reg::R2)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Eq, found);
            enc.ldr_imm(Reg::R1, Reg::R0, 12)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.cmp_imm(Reg::R1, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Eq, miss);
            enc.adds(Reg::R0, Reg::R0, Reg::R1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b(search);
            enc.bind_label(found);
            enc.movs_imm(Reg::R0, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b(done);
            enc.bind_label(miss);
            enc.movs_imm(Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.bind_label(done);
        }
        Inst::TypeName { descriptor } => {
            let miss = enc.new_label();
            let done = enc.new_label();
            slot_load(enc, Reg::R0, slot(*descriptor))?;
            enc.cmp_imm(Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Eq, miss);
            enc.ldr_imm(Reg::R1, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.lsrs_imm(Reg::R1, Reg::R1, 24)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.cmp_imm(Reg::R1, (ARRAY_DESC_MARK >> 24) as u8)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Eq, miss);
            enc.ldr_imm(Reg::R1, Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.lsls_imm(Reg::R1, Reg::R1, 2)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds_imm8(Reg::R1, 16)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds(Reg::R1, Reg::R0, Reg::R1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R2, Reg::R1, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.lsls_imm(Reg::R2, Reg::R2, 3)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds(Reg::R1, Reg::R1, Reg::R2)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R2, Reg::R1, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.cmp_imm(Reg::R2, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Eq, miss);
            enc.adds(Reg::R0, Reg::R0, Reg::R2)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b(done);
            enc.bind_label(miss);
            enc.movs_imm(Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.bind_label(done);
        }
        Inst::InterfaceHasTag { descriptor, tag } => {
            let miss = enc.new_label();
            let found = enc.new_label();
            let search = enc.new_label();
            let done = enc.new_label();
            slot_load(enc, Reg::R0, slot(*descriptor))?;
            enc.cmp_imm(Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Eq, miss);
            enc.ldr_imm(Reg::R1, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.lsrs_imm(Reg::R1, Reg::R1, 24)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.cmp_imm(Reg::R1, (ARRAY_DESC_MARK >> 24) as u8)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Eq, miss);
            enc.ldr_imm(Reg::R1, Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.lsls_imm(Reg::R1, Reg::R1, 2)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds_imm8(Reg::R1, 16)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds(Reg::R1, Reg::R0, Reg::R1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R2, Reg::R1, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.cmp_imm(Reg::R2, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Eq, miss);
            enc.adds_imm8(Reg::R1, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            load_const_word(enc, pool, Reg::R3, *tag)?;
            enc.bind_label(search);
            enc.ldr_imm(Reg::R0, Reg::R1, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.cmp_reg(Reg::R0, Reg::R3)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Eq, found);
            enc.adds_imm8(Reg::R1, 8)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.subs_imm8(Reg::R2, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Ne, search);
            enc.bind_label(miss);
            enc.movs_imm(Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b(done);
            enc.bind_label(found);
            enc.movs_imm(Reg::R0, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.bind_label(done);
        }
        Inst::Store {
            address,
            value,
            width,
        } => {
            slot_load(enc, Reg::R0, slot(*address))?;
            slot_load(enc, Reg::R1, slot(*value))?;
            if *width == 8 {
                enc.str_imm(Reg::R1, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                slot_load(enc, Reg::R1, slot(*value) + 4)?;
                enc.str_imm(Reg::R1, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                emit_sized_store(enc, Reg::R1, Reg::R0, *width)?;
            }
        }
        Inst::Load {
            address,
            width,
            signed,
        } => {
            slot_load(enc, Reg::R0, slot(*address))?;
            if *width == 8 {
                enc.ldr_imm(Reg::R1, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_imm(Reg::R0, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                emit_sized_load(enc, Reg::R0, Reg::R0, *width, *signed)?;
            }
        }
        Inst::CopyBlock { dst, src, size } => {
            slot_load(enc, Reg::R0, slot(*dst))?;
            slot_load(enc, Reg::R1, slot(*src))?;
            slot_load(enc, Reg::R2, slot(*size))?;
            let body = enc.new_label();
            let test = enc.new_label();
            enc.b(test);
            enc.bind_label(body);
            enc.ldrb_imm(Reg::R3, Reg::R1, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.strb_imm(Reg::R3, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds_imm3(Reg::R0, Reg::R0, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds_imm3(Reg::R1, Reg::R1, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.subs_imm3(Reg::R2, Reg::R2, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.bind_label(test);
            enc.cmp_imm(Reg::R2, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Ne, body);
        }
        Inst::FillBlock { dst, value, size } => {
            slot_load(enc, Reg::R0, slot(*dst))?;
            slot_load(enc, Reg::R1, slot(*value))?;
            slot_load(enc, Reg::R2, slot(*size))?;
            let body = enc.new_label();
            let test = enc.new_label();
            enc.b(test);
            enc.bind_label(body);
            enc.strb_imm(Reg::R1, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds_imm3(Reg::R0, Reg::R0, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.subs_imm3(Reg::R2, Reg::R2, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.bind_label(test);
            enc.cmp_imm(Reg::R2, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.b_cond(Cond::Ne, body);
        }
        Inst::FieldLoad { base, offset } => {
            let two_words = matches!(result_ty, Some(MirType::I64 | MirType::F64));
            if is_pointer_base(value_types, *base) {
                slot_load(enc, Reg::R2, slot(*base))?;
                enc.ldr_imm(Reg::R0, Reg::R2, *offset as u16)
                    .map_err(|_| LowerError::TooManyValues)?;
                if two_words {
                    enc.ldr_imm(Reg::R1, Reg::R2, *offset as u16 + 4)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
            } else {
                slot_load(enc, Reg::R0, slot(*base) + *offset as u16)?;
                if two_words {
                    slot_load(enc, Reg::R1, slot(*base) + *offset as u16 + 4)?;
                }
            }
        }
        Inst::FieldStore {
            base,
            offset,
            value,
        } => {
            let two_words = matches!(
                value_types.get(value.0 as usize),
                Some(MirType::I64 | MirType::F64)
            );
            let base_ptr = is_pointer_base(value_types, *base);
            if base_ptr {
                slot_load(enc, Reg::R1, slot(*base))?;
            }
            slot_load(enc, Reg::R0, slot(*value))?;
            if base_ptr {
                enc.str_imm(Reg::R0, Reg::R1, *offset as u16)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                slot_store(enc, Reg::R0, slot(*base) + *offset as u16, Reg::R2)?;
            }
            if two_words {
                slot_load(enc, Reg::R0, slot(*value) + 4)?;
                if base_ptr {
                    enc.str_imm(Reg::R0, Reg::R1, *offset as u16 + 4)
                        .map_err(|_| LowerError::TooManyValues)?;
                } else {
                    slot_store(enc, Reg::R0, slot(*base) + *offset as u16 + 4, Reg::R2)?;
                }
            }
        }
        Inst::FieldLoadNarrow {
            base,
            offset,
            size,
            signed,
        } => {
            if is_pointer_base(value_types, *base) {
                slot_load(enc, Reg::R1, slot(*base))?;
            } else {
                slot_addr(enc, Reg::R1, slot(*base))?;
            }
            narrow_load_at(enc, Reg::R0, Reg::R1, *offset, *size, *signed)?;
        }
        Inst::FieldStoreNarrow {
            base,
            offset,
            value,
            size,
        } => {
            if is_pointer_base(value_types, *base) {
                slot_load(enc, Reg::R1, slot(*base))?;
            } else {
                slot_addr(enc, Reg::R1, slot(*base))?;
            }
            slot_load(enc, Reg::R0, slot(*value))?;
            narrow_store_at(enc, Reg::R0, Reg::R1, *offset, *size)?;
        }
        Inst::FieldAddr { base, offset } => {
            if is_pointer_base(value_types, *base) {
                slot_load(enc, Reg::R0, slot(*base))?;
                if *offset != 0 {
                    enc.adds_imm8(Reg::R0, *offset as u8)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
            } else {
                slot_addr(enc, Reg::R0, slot(*base) + *offset as u16)?;
            }
        }
        Inst::InitStruct | Inst::CopyStruct { .. } => {}
        Inst::Convert { value, kind } => {
            slot_load(enc, Reg::R0, slot(*value))?;
            if matches!(kind, ConvKind::Float32ToInt) {
                emit_f2i(enc)?;
            } else if matches!(kind, ConvKind::IntToFloat32) {
                emit_i2f(enc)?;
            } else if aeabi_convert_helper(*kind).is_some() {
                return Err(LowerError::CallUnsupported);
            } else {
                extend_for(enc, Reg::R0, Reg::R0, *kind).map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::Widen { value, signed } => {
            slot_load(enc, Reg::R0, slot(*value))?;
            if *signed {
                enc.asrs_imm(Reg::R1, Reg::R0, 31)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                enc.movs_imm(Reg::R1, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::Truncate { value } => {
            slot_load(enc, Reg::R0, slot(*value))?;
        }
        Inst::SemihostWrite { text } => match console_symbol {
            Some(symbol) => {
                let bytes: &[u8] = match text.split_last() {
                    Some((0, head)) => head,
                    _ => text,
                };
                let entry = enc.new_label();
                strings.push((entry, bytes.into()));
                enc.adr(Reg::R0, entry)
                    .map_err(|_| LowerError::TooManyValues)?;
                load_const_word(enc, pool, Reg::R1, bytes.len() as u32)?;
                enc.bl_symbol(EXTERN_SYMBOL_FLAG | symbol);
            }
            None => {
                let entry = enc.new_label();
                strings.push((entry, text.clone()));
                enc.adr(Reg::R1, entry)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.movs_imm(Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.bkpt(0xAB);
            }
        },
        Inst::WriteInt { value } => {
            slot_load(enc, Reg::R0, slot(*value))?;
            emit_write_int(enc)?;
        }
        Inst::StringLiteral { utf16 } => match blob_table {
            Some(table) => {
                let id = table
                    .iter()
                    .position(|b| b.as_ref() == utf16.as_ref())
                    .expect("the emit_object_pass pre-scan registers every string literal")
                    as u32;
                let label = enc.new_label();
                sym_pool.push((label, STRING_SYMBOL_FLAG | id, 0));
                enc.ldr_literal(Reg::R0, label)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
            None => {
                let entry = enc.new_label();
                string_blobs.push((entry, utf16.clone()));
                enc.adr(Reg::R0, entry)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
        },
        Inst::StringEquals { lhs, rhs } => {
            slot_load(enc, Reg::R0, slot(*lhs))?;
            slot_load(enc, Reg::R1, slot(*rhs))?;
            emit_string_equals(enc)?;
        }
        Inst::StringConcat { .. } | Inst::IntToString { .. } => {
            return Err(LowerError::CallUnsupported);
        }
        Inst::ArrayLoad {
            array,
            index,
            element_size,
            signed,
        } => {
            slot_load(enc, Reg::R0, slot(*array))?;
            slot_load(enc, Reg::R1, slot(*index))?;
            emit_array_bounds_check(enc)?;
            scale_index(enc, pool, *element_size)?;
            enc.adds_imm3(Reg::R0, Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            if *element_size == 8 {
                enc.adds(Reg::R2, Reg::R0, Reg::R1)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_imm(Reg::R0, Reg::R2, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_imm(Reg::R1, Reg::R2, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                match (*element_size, *signed) {
                    (1, true) => enc.ldrsb_reg(Reg::R0, Reg::R0, Reg::R1),
                    (1, false) => enc.ldrb_reg(Reg::R0, Reg::R0, Reg::R1),
                    (2, true) => enc.ldrsh_reg(Reg::R0, Reg::R0, Reg::R1),
                    (2, false) => enc.ldrh_reg(Reg::R0, Reg::R0, Reg::R1),
                    _ => enc.ldr_reg(Reg::R0, Reg::R0, Reg::R1),
                }
                .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::ArrayStore {
            array,
            index,
            value,
            element_size,
        } => {
            slot_load(enc, Reg::R0, slot(*array))?;
            slot_load(enc, Reg::R1, slot(*index))?;
            emit_array_bounds_check(enc)?;
            scale_index(enc, pool, *element_size)?;
            enc.adds_imm3(Reg::R0, Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            if *element_size == 8 {
                enc.adds(Reg::R0, Reg::R0, Reg::R1)
                    .map_err(|_| LowerError::TooManyValues)?;
                slot_load(enc, Reg::R2, slot(*value))?;
                slot_load(enc, Reg::R3, slot(*value) + 4)?;
                enc.str_imm(Reg::R2, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_imm(Reg::R3, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                slot_load(enc, Reg::R2, slot(*value))?;
                match *element_size {
                    1 => enc.strb_reg(Reg::R2, Reg::R0, Reg::R1),
                    2 => enc.strh_reg(Reg::R2, Reg::R0, Reg::R1),
                    _ => enc.str_reg(Reg::R2, Reg::R0, Reg::R1),
                }
                .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::ArrayElemAddr {
            array,
            index,
            element_size,
        } => {
            slot_load(enc, Reg::R0, slot(*array))?;
            slot_load(enc, Reg::R1, slot(*index))?;
            emit_array_bounds_check(enc)?;
            scale_index(enc, pool, *element_size)?;
            enc.adds_imm3(Reg::R0, Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds(Reg::R0, Reg::R0, Reg::R1)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::StaticLoad { owner, offset } => {
            static_slot_addr(enc, pool, sym_pool, relocate, *owner, *offset)?;
            if matches!(result_ty, Some(MirType::I64 | MirType::F64)) {
                enc.ldr_imm(Reg::R1, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
            enc.ldr_imm(Reg::R0, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::StaticStore {
            owner,
            offset,
            value,
        } => {
            static_slot_addr(enc, pool, sym_pool, relocate, *owner, *offset)?;
            slot_load(enc, Reg::R1, slot(*value))?;
            enc.str_imm(Reg::R1, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            if matches!(
                value_types.get(value.0 as usize),
                Some(MirType::I64 | MirType::F64)
            ) {
                slot_load(enc, Reg::R1, slot(*value) + 4)?;
                enc.str_imm(Reg::R1, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::Alloc { .. }
        | Inst::AllocLike { .. }
        | Inst::AllocDescribed { .. }
        | Inst::AllocArray { .. }
        | Inst::AllocArray2D { .. }
        | Inst::AllocArrayMD { .. }
        | Inst::LoadTypeDesc { .. }
        | Inst::TypeDescAddr { .. } => {
            return Err(LowerError::CallUnsupported);
        }
        Inst::Array2DLoad {
            array,
            index0,
            index1,
            element_size,
            signed,
        } => {
            slot_load(enc, Reg::R0, slot(*array))?;
            slot_load(enc, Reg::R1, slot(*index0))?;
            emit_dim_bounds_check(enc, 0)?;
            slot_load(enc, Reg::R1, slot(*index1))?;
            emit_dim_bounds_check(enc, 4)?;
            slot_load(enc, Reg::R1, slot(*index0))?;
            enc.ldr_imm(Reg::R2, Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.muls(Reg::R1, Reg::R2)
                .map_err(|_| LowerError::TooManyValues)?;
            slot_load(enc, Reg::R2, slot(*index1))?;
            enc.adds(Reg::R1, Reg::R1, Reg::R2)
                .map_err(|_| LowerError::TooManyValues)?;
            scale_index(enc, pool, *element_size)?;
            enc.adds_imm8(Reg::R0, 8)
                .map_err(|_| LowerError::TooManyValues)?;
            if *element_size == 8 {
                enc.adds(Reg::R2, Reg::R0, Reg::R1)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_imm(Reg::R0, Reg::R2, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_imm(Reg::R1, Reg::R2, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                match (*element_size, *signed) {
                    (1, true) => enc.ldrsb_reg(Reg::R0, Reg::R0, Reg::R1),
                    (1, false) => enc.ldrb_reg(Reg::R0, Reg::R0, Reg::R1),
                    (2, true) => enc.ldrsh_reg(Reg::R0, Reg::R0, Reg::R1),
                    (2, false) => enc.ldrh_reg(Reg::R0, Reg::R0, Reg::R1),
                    _ => enc.ldr_reg(Reg::R0, Reg::R0, Reg::R1),
                }
                .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::Array2DStore {
            array,
            index0,
            index1,
            value,
            element_size,
        } => {
            slot_load(enc, Reg::R0, slot(*array))?;
            slot_load(enc, Reg::R1, slot(*index0))?;
            emit_dim_bounds_check(enc, 0)?;
            slot_load(enc, Reg::R1, slot(*index1))?;
            emit_dim_bounds_check(enc, 4)?;
            slot_load(enc, Reg::R1, slot(*index0))?;
            enc.ldr_imm(Reg::R2, Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.muls(Reg::R1, Reg::R2)
                .map_err(|_| LowerError::TooManyValues)?;
            slot_load(enc, Reg::R2, slot(*index1))?;
            enc.adds(Reg::R1, Reg::R1, Reg::R2)
                .map_err(|_| LowerError::TooManyValues)?;
            scale_index(enc, pool, *element_size)?;
            enc.adds_imm8(Reg::R0, 8)
                .map_err(|_| LowerError::TooManyValues)?;
            if *element_size == 8 {
                enc.adds(Reg::R0, Reg::R0, Reg::R1)
                    .map_err(|_| LowerError::TooManyValues)?;
                slot_load(enc, Reg::R2, slot(*value))?;
                slot_load(enc, Reg::R3, slot(*value) + 4)?;
                enc.str_imm(Reg::R2, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_imm(Reg::R3, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                slot_load(enc, Reg::R2, slot(*value))?;
                match *element_size {
                    1 => enc.strb_reg(Reg::R2, Reg::R0, Reg::R1),
                    2 => enc.strh_reg(Reg::R2, Reg::R0, Reg::R1),
                    _ => enc.str_reg(Reg::R2, Reg::R0, Reg::R1),
                }
                .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::ArrayMDLoad {
            array,
            indices,
            element_size,
            signed,
        } => {
            slot_load(enc, Reg::R0, slot(*array))?;
            emit_md_element_address(enc, pool, slot, indices, *element_size)?;
            if *element_size == 8 {
                enc.ldr_imm(Reg::R1, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_imm(Reg::R0, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                emit_sized_load(enc, Reg::R0, Reg::R0, *element_size, *signed)?;
            }
        }
        Inst::ArrayMDStore {
            array,
            indices,
            value,
            element_size,
        } => {
            slot_load(enc, Reg::R0, slot(*array))?;
            emit_md_element_address(enc, pool, slot, indices, *element_size)?;
            if *element_size == 8 {
                slot_load(enc, Reg::R1, slot(*value))?;
                slot_load(enc, Reg::R2, slot(*value) + 4)?;
                enc.str_imm(Reg::R1, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_imm(Reg::R2, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                slot_load(enc, Reg::R1, slot(*value))?;
                emit_sized_store(enc, Reg::R1, Reg::R0, *element_size)?;
            }
        }
    }
    Ok(None)
}

/// The absolute base of the FLAT path's static-field storage in RAM (`lower_module*` -- the
/// linker-free self-contained image, where one shared region is exactly right). The OBJECT path
/// does not use this constant: each assembly's accesses relocate against its own
/// `__lamella_statics_<hash>` symbol and `lamella-link` places the regions -- its per-machine
/// default window starts at this same value, so flat and linked images share one RAM plan.
pub const STATIC_FIELD_BASE: u32 = 0x2000_1000;

/// Emits the array bounds check: with `r0` = the array and `r1` = the index, traps (`udf`) unless
/// `index < length` (the length at `[array+0]`), compared UNSIGNED so a negative index -- a huge
/// unsigned value -- traps too, matching `IndexOutOfRangeException`'s effect. Until the exception
/// model lands, an out-of-range access aborts rather than throwing a catchable exception.
fn emit_array_bounds_check(enc: &mut Encoder) -> Result<(), LowerError> {
    emit_dim_bounds_check(enc, 0)
}

/// Emits a `width`-byte store of `rt` to `[rn]` (offset 0): `strb` (1), `strh` (2), or `str` (4) --
/// the indirect-store primitive shared by the register and spilled paths.
fn emit_sized_store(enc: &mut Encoder, rt: Reg, rn: Reg, width: u32) -> Result<(), LowerError> {
    match width {
        1 => enc.strb_imm(rt, rn, 0),
        2 => enc.strh_imm(rt, rn, 0),
        _ => enc.str_imm(rt, rn, 0),
    }
    .map_err(|_| LowerError::TooManyValues)
}

/// Emits a `width`-byte load from `[rn]` into `rt` (offset 0), then sign-extends a sub-word result
/// when `signed` (`ldrb`+`sxtb` / `ldrh`+`sxth` / `ldr`) -- the indirect-load primitive shared by the
/// register and spilled paths. (Thumb-1 has no immediate-offset `ldrsb`/`ldrsh`, hence the extend.)
fn emit_sized_load(
    enc: &mut Encoder,
    rt: Reg,
    rn: Reg,
    width: u32,
    signed: bool,
) -> Result<(), LowerError> {
    match width {
        1 => {
            enc.ldrb_imm(rt, rn, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            if signed {
                enc.sxtb(rt, rt).map_err(|_| LowerError::TooManyValues)?;
            }
        }
        2 => {
            enc.ldrh_imm(rt, rn, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            if signed {
                enc.sxth(rt, rt).map_err(|_| LowerError::TooManyValues)?;
            }
        }
        _ => enc
            .ldr_imm(rt, rn, 0)
            .map_err(|_| LowerError::TooManyValues)?,
    }
    Ok(())
}

/// Bounds-checks the index in `r1` against the dimension word at `[r0 + dim_offset]` (an array's
/// length at offset 0, or a 2-D array's second dimension at offset 4), trapping (`udf`) when out of
/// range. The compare is unsigned, so a negative index (a huge unsigned value) traps too. Clobbers r2.
fn emit_dim_bounds_check(enc: &mut Encoder, dim_offset: u16) -> Result<(), LowerError> {
    enc.ldr_imm(Reg::R2, Reg::R0, dim_offset)
        .map_err(|_| LowerError::TooManyValues)?;
    enc.cmp_reg(Reg::R1, Reg::R2)
        .map_err(|_| LowerError::TooManyValues)?;
    let ok = enc.new_label();
    enc.b_cond(Cond::CarryClear, ok);
    enc.udf(0);
    enc.bind_label(ok);
    Ok(())
}

/// With `r0` = the array base, computes the address of rank-N element `(indices[0..N])` into `r0`,
/// bounds-checking each index against its dimension word `[array + 4*k]` (unsigned, so a negative
/// index -- a huge unsigned value -- traps too; `udf` on failure). The flat index is the Horner fold
/// `((..(i0*dim1 + i1)*dim2 + i2)..)*dim(N-1) + i(N-1)`; the element sits at `array + 4*N +
/// flat*element_size`. Clobbers r1, r2, r3. (The N-1 products use `muls` -- ARM has hardware multiply,
/// unlike RV32E; the rank fits `ldr [rN,#imm5*4]`/`adds #imm8`, i.e. up to 32, the CLI's rank ceiling.)
fn emit_md_element_address(
    enc: &mut Encoder,
    pool: &mut Vec<(Label, u32)>,
    slot: &impl Fn(ValueId) -> u16,
    indices: &[ValueId],
    element_size: u32,
) -> Result<(), LowerError> {
    let oops = |_| LowerError::TooManyValues;
    let n = indices.len();
    slot_load(enc, Reg::R1, slot(indices[0]))?;
    emit_dim_bounds_check(enc, 0)?;
    for (k, &idx) in indices.iter().enumerate().skip(1) {
        enc.ldr_imm(Reg::R2, Reg::R0, (4 * k) as u16).map_err(oops)?;
        enc.muls(Reg::R1, Reg::R2).map_err(oops)?;
        slot_load(enc, Reg::R3, slot(idx))?;
        enc.cmp_reg(Reg::R3, Reg::R2).map_err(oops)?;
        let ok = enc.new_label();
        enc.b_cond(Cond::CarryClear, ok);
        enc.udf(0);
        enc.bind_label(ok);
        enc.adds(Reg::R1, Reg::R1, Reg::R3).map_err(oops)?;
    }
    scale_index(enc, pool, element_size)?;
    enc.adds(Reg::R0, Reg::R0, Reg::R1).map_err(oops)?;
    enc.adds_imm8(Reg::R0, (4 * n) as u8).map_err(oops)?;
    Ok(())
}

/// Emits the soft `conv.i4` from a float32: with the IEEE-754 bit pattern in r0, leaves the value
/// truncated toward zero as a signed int32 in r0. ARMv6-M has no FPU, so this is done by hand from
/// the fields: `value = (-1)^sign * (1.mantissa) * 2^(exp-127)`, so the integer part is the 24-bit
/// significand `(1<<23)|mantissa` shifted by `exp-150` (right when exp <= 150, left above), then
/// negated for a set sign bit; an exponent below 127 (magnitude < 1) gives 0. (Overflow past 2^31
/// is left undefined, like the hardware convert.) r1-r3 are scratch.
fn emit_f2i(enc: &mut Encoder) -> Result<(), LowerError> {
    let oops = |_| LowerError::TooManyValues;
    let to_zero = enc.new_label();
    let shift_left = enc.new_label();
    let apply_sign = enc.new_label();
    let store = enc.new_label();
    let end = enc.new_label();
    enc.lsrs_imm(Reg::R1, Reg::R0, 23).map_err(oops)?;
    enc.movs_imm(Reg::R2, 0xFF).map_err(oops)?;
    enc.ands(Reg::R1, Reg::R2).map_err(oops)?;
    enc.cmp_imm(Reg::R1, 127).map_err(oops)?;
    enc.b_cond(Cond::LessThan, to_zero);
    enc.lsls_imm(Reg::R2, Reg::R0, 9).map_err(oops)?;
    enc.lsrs_imm(Reg::R2, Reg::R2, 9).map_err(oops)?;
    enc.movs_imm(Reg::R3, 1).map_err(oops)?;
    enc.lsls_imm(Reg::R3, Reg::R3, 23).map_err(oops)?;
    enc.orrs(Reg::R2, Reg::R3).map_err(oops)?;
    enc.movs_imm(Reg::R3, 150).map_err(oops)?;
    enc.subs(Reg::R3, Reg::R3, Reg::R1).map_err(oops)?;
    enc.cmp_imm(Reg::R3, 0).map_err(oops)?;
    enc.b_cond(Cond::LessThan, shift_left);
    enc.lsrs_reg(Reg::R2, Reg::R3).map_err(oops)?;
    enc.b(apply_sign);
    enc.bind_label(shift_left);
    enc.rsbs(Reg::R3, Reg::R3).map_err(oops)?;
    enc.lsls_reg(Reg::R2, Reg::R3).map_err(oops)?;
    enc.bind_label(apply_sign);
    enc.lsrs_imm(Reg::R1, Reg::R0, 31).map_err(oops)?;
    enc.cmp_imm(Reg::R1, 0).map_err(oops)?;
    enc.b_cond(Cond::Eq, store);
    enc.rsbs(Reg::R2, Reg::R2).map_err(oops)?;
    enc.bind_label(store);
    enc.mov_reg(Reg::R0, Reg::R2);
    enc.b(end);
    enc.bind_label(to_zero);
    enc.movs_imm(Reg::R0, 0).map_err(oops)?;
    enc.bind_label(end);
    Ok(())
}

/// Emits the soft `conv.r4` from a signed int32: with the value in r0, leaves its IEEE-754 float32
/// bit pattern in r0. ARMv6-M has no FPU (and no `clz`), so the magnitude is normalized by a shift
/// loop: sign and `|v|` are split out, `|v|` is shifted left until its top bit is the implicit 1,
/// and the exponent (`158 - shifts`) and the 23-bit mantissa (the next bits) are assembled with the
/// sign. Exact for magnitudes below 2^24; larger values truncate the low bits (round-to-nearest is
/// unsupported). r1-r3 are scratch.
///
/// THE FLAT (linker-free) PATH ONLY, since that truncation is a wrong answer wherever it can be
/// avoided: the linked path routes this conversion to the archive's `__aeabi_i2f`, which rounds to
/// nearest ([`aeabi_convert_helper`]). A self-contained single-assembly image has no helper to call,
/// so it keeps this and its 2^24 limit.
fn emit_i2f(enc: &mut Encoder) -> Result<(), LowerError> {
    let oops = |_| LowerError::TooManyValues;
    let done = enc.new_label();
    let norm_loop = enc.new_label();
    let norm_done = enc.new_label();
    enc.cmp_imm(Reg::R0, 0).map_err(oops)?;
    enc.b_cond(Cond::Eq, done);
    enc.lsrs_imm(Reg::R2, Reg::R0, 31).map_err(oops)?;
    enc.asrs_imm(Reg::R3, Reg::R0, 31).map_err(oops)?;
    enc.eors(Reg::R0, Reg::R3).map_err(oops)?;
    enc.subs(Reg::R1, Reg::R0, Reg::R3).map_err(oops)?;
    enc.movs_imm(Reg::R3, 0).map_err(oops)?;
    enc.bind_label(norm_loop);
    enc.lsrs_imm(Reg::R0, Reg::R1, 31).map_err(oops)?;
    enc.cmp_imm(Reg::R0, 0).map_err(oops)?;
    enc.b_cond(Cond::Ne, norm_done);
    enc.lsls_imm(Reg::R1, Reg::R1, 1).map_err(oops)?;
    enc.adds_imm8(Reg::R3, 1).map_err(oops)?;
    enc.b(norm_loop);
    enc.bind_label(norm_done);
    enc.movs_imm(Reg::R0, 158).map_err(oops)?;
    enc.subs(Reg::R0, Reg::R0, Reg::R3).map_err(oops)?;
    enc.lsrs_imm(Reg::R1, Reg::R1, 8).map_err(oops)?;
    enc.movs_imm(Reg::R3, 1).map_err(oops)?;
    enc.lsls_imm(Reg::R3, Reg::R3, 23).map_err(oops)?;
    enc.subs_imm8(Reg::R3, 1).map_err(oops)?;
    enc.ands(Reg::R1, Reg::R3).map_err(oops)?;
    enc.lsls_imm(Reg::R0, Reg::R0, 23).map_err(oops)?;
    enc.orrs(Reg::R1, Reg::R0).map_err(oops)?;
    enc.lsls_imm(Reg::R2, Reg::R2, 31).map_err(oops)?;
    enc.orrs(Reg::R1, Reg::R2).map_err(oops)?;
    enc.mov_reg(Reg::R0, Reg::R1);
    enc.bind_label(done);
    Ok(())
}

/// Emits a 32-bit integer divide/remainder for the divide-less Cortex-M0: dividend in r0, divisor in
/// r1, the quotient (or the remainder, when `remainder`) left in r0. `signed` divides the magnitudes
/// and re-applies the sign (the quotient's is `sign(n) ^ sign(d)`, the remainder's is `sign(n)`). The
/// core is a restoring binary long division: 32 iterations, each shifting one dividend bit (high to
/// low) into a running remainder and subtracting the divisor when it fits, setting that quotient bit.
/// r4-r7 are saved/restored. Division by zero is left undefined here (no trap) -- a checked-context
/// DivideByZeroException is a follow-up.
fn emit_divmod32(enc: &mut Encoder, signed: bool, remainder: bool) -> Result<(), LowerError> {
    let oops = |_| LowerError::TooManyValues;
    let div_ok = enc.new_label();
    enc.cmp_imm(Reg::R1, 0).map_err(oops)?;
    enc.b_cond(Cond::Ne, div_ok);
    enc.udf(0);
    enc.bind_label(div_ok);
    enc.push_registers(0xF0, false);
    if signed {
        enc.movs_imm(Reg::R4, 31).map_err(oops)?;
        enc.mov_reg(Reg::R2, Reg::R0);
        enc.asrs_reg(Reg::R2, Reg::R4).map_err(oops)?;
        enc.mov_reg(Reg::R3, Reg::R1);
        enc.asrs_reg(Reg::R3, Reg::R4).map_err(oops)?;
        enc.mov_reg(Reg::R7, Reg::R2);
        if !remainder {
            enc.eors(Reg::R7, Reg::R3).map_err(oops)?;
        }
        enc.eors(Reg::R0, Reg::R2).map_err(oops)?;
        enc.subs(Reg::R0, Reg::R0, Reg::R2).map_err(oops)?;
        enc.eors(Reg::R1, Reg::R3).map_err(oops)?;
        enc.subs(Reg::R1, Reg::R1, Reg::R3).map_err(oops)?;
    }
    enc.movs_imm(Reg::R3, 0).map_err(oops)?;
    enc.movs_imm(Reg::R2, 0).map_err(oops)?;
    enc.movs_imm(Reg::R4, 32).map_err(oops)?;
    let loop_top = enc.new_label();
    let skip = enc.new_label();
    enc.bind_label(loop_top);
    enc.subs_imm8(Reg::R4, 1).map_err(oops)?;
    enc.lsls_imm(Reg::R2, Reg::R2, 1).map_err(oops)?;
    enc.mov_reg(Reg::R5, Reg::R0);
    enc.lsrs_reg(Reg::R5, Reg::R4).map_err(oops)?;
    enc.movs_imm(Reg::R6, 1).map_err(oops)?;
    enc.ands(Reg::R5, Reg::R6).map_err(oops)?;
    enc.orrs(Reg::R2, Reg::R5).map_err(oops)?;
    enc.cmp_reg(Reg::R2, Reg::R1).map_err(oops)?;
    enc.b_cond(Cond::CarryClear, skip);
    enc.subs(Reg::R2, Reg::R2, Reg::R1).map_err(oops)?;
    enc.movs_imm(Reg::R5, 1).map_err(oops)?;
    enc.lsls_reg(Reg::R5, Reg::R4).map_err(oops)?;
    enc.orrs(Reg::R3, Reg::R5).map_err(oops)?;
    enc.bind_label(skip);
    enc.cmp_imm(Reg::R4, 0).map_err(oops)?;
    enc.b_cond(Cond::Ne, loop_top);
    if remainder {
        enc.mov_reg(Reg::R0, Reg::R2);
    } else {
        enc.mov_reg(Reg::R0, Reg::R3);
    }
    if signed {
        enc.cmp_imm(Reg::R7, 0).map_err(oops)?;
        let nonneg = enc.new_label();
        enc.b_cond(Cond::Eq, nonneg);
        enc.movs_imm(Reg::R5, 0).map_err(oops)?;
        enc.subs(Reg::R0, Reg::R5, Reg::R0).map_err(oops)?;
        enc.bind_label(nonneg);
    }
    enc.pop_registers(0xF0, false);
    Ok(())
}

/// 64-bit soft div/rem (there is no 64-bit hardware divide on M-profile). The dividend `a` is in r0:r1, the
/// divisor `b` in r2:r3; the result (quotient or remainder) is left in r0:r1. A restoring long division: the
/// {rem:a} 128-bit value shifts left 1 per step, the dividend's MSB entering `rem` while the quotient bit
/// enters `a`'s LSB -- so `a` becomes the quotient IN PLACE, keeping the working set within r0-r7. `signed`
/// divides magnitudes (branchless 64-bit abs) and re-applies the sign. Divide-by-zero traps (inline UDF),
/// like [`emit_divmod32`].
fn emit_divmod64(enc: &mut Encoder, signed: bool, remainder: bool) -> Result<(), LowerError> {
    let oops = |_| LowerError::TooManyValues;
    let div_ok = enc.new_label();
    enc.cmp_imm(Reg::R2, 0).map_err(oops)?;
    enc.b_cond(Cond::Ne, div_ok);
    enc.cmp_imm(Reg::R3, 0).map_err(oops)?;
    enc.b_cond(Cond::Ne, div_ok);
    enc.udf(0);
    enc.bind_label(div_ok);
    enc.push_registers(0xF0, false);
    if signed {
        enc.movs_imm(Reg::R4, 31).map_err(oops)?;
        enc.mov_reg(Reg::R5, Reg::R1);
        enc.asrs_reg(Reg::R5, Reg::R4).map_err(oops)?;
        enc.mov_reg(Reg::R6, Reg::R3);
        enc.asrs_reg(Reg::R6, Reg::R4).map_err(oops)?;
        enc.mov_reg(Reg::R7, Reg::R5);
        if !remainder {
            enc.eors(Reg::R7, Reg::R6).map_err(oops)?;
        }
        enc.eors(Reg::R0, Reg::R5).map_err(oops)?;
        enc.eors(Reg::R1, Reg::R5).map_err(oops)?;
        enc.subs(Reg::R0, Reg::R0, Reg::R5).map_err(oops)?;
        enc.sbcs(Reg::R1, Reg::R5).map_err(oops)?;
        enc.eors(Reg::R2, Reg::R6).map_err(oops)?;
        enc.eors(Reg::R3, Reg::R6).map_err(oops)?;
        enc.subs(Reg::R2, Reg::R2, Reg::R6).map_err(oops)?;
        enc.sbcs(Reg::R3, Reg::R6).map_err(oops)?;
    }
    enc.movs_imm(Reg::R4, 0).map_err(oops)?;
    enc.movs_imm(Reg::R5, 0).map_err(oops)?;
    enc.movs_imm(Reg::R6, 64).map_err(oops)?;
    let loop_top = enc.new_label();
    let set_bit = enc.new_label();
    let after = enc.new_label();
    enc.bind_label(loop_top);
    enc.lsls_imm(Reg::R0, Reg::R0, 1).map_err(oops)?;
    enc.adcs(Reg::R1, Reg::R1).map_err(oops)?;
    enc.adcs(Reg::R4, Reg::R4).map_err(oops)?;
    enc.adcs(Reg::R5, Reg::R5).map_err(oops)?;
    enc.subs(Reg::R4, Reg::R4, Reg::R2).map_err(oops)?;
    enc.sbcs(Reg::R5, Reg::R3).map_err(oops)?;
    enc.b_cond(Cond::CarrySet, set_bit);
    enc.adds(Reg::R4, Reg::R4, Reg::R2).map_err(oops)?;
    enc.adcs(Reg::R5, Reg::R3).map_err(oops)?;
    enc.b(after);
    enc.bind_label(set_bit);
    enc.adds_imm8(Reg::R0, 1).map_err(oops)?;
    enc.bind_label(after);
    enc.subs_imm8(Reg::R6, 1).map_err(oops)?;
    enc.b_cond(Cond::Ne, loop_top);
    if remainder {
        enc.mov_reg(Reg::R0, Reg::R4);
        enc.mov_reg(Reg::R1, Reg::R5);
    }
    if signed {
        enc.cmp_imm(Reg::R7, 0).map_err(oops)?;
        let nonneg = enc.new_label();
        enc.b_cond(Cond::Eq, nonneg);
        enc.movs_imm(Reg::R4, 0).map_err(oops)?;
        enc.subs(Reg::R0, Reg::R4, Reg::R0).map_err(oops)?;
        enc.sbcs(Reg::R4, Reg::R1).map_err(oops)?;
        enc.mov_reg(Reg::R1, Reg::R4);
        enc.bind_label(nonneg);
    }
    enc.pop_registers(0xF0, false);
    Ok(())
}

/// Emits a 64-bit multiply `a * b` (mod 2^64) with `a` in r0:r1 (lo:hi) and `b` in r2:r3, leaving
/// the product in r0:r1. ARMv6-M has only the truncating 32x32->32 `MULS`, so the full 32x32->64 of
/// the low halves is built from the four 16x16 partial products (each fits 32 bits); the cross terms
/// a_lo*b_hi and a_hi*b_lo are scaled by 2^32, so only their low 32 bits reach the high word.
/// r4-r7 are saved and restored, so nothing the caller holds in them is disturbed.
fn emit_mul64(enc: &mut Encoder) -> Result<(), LowerError> {
    let oops = |_| LowerError::TooManyValues;
    enc.push_registers(0xF0, false);
    enc.mov_reg(Reg::R4, Reg::R0);
    enc.muls(Reg::R4, Reg::R3).map_err(oops)?;
    enc.muls(Reg::R1, Reg::R2).map_err(oops)?;
    enc.adds(Reg::R4, Reg::R4, Reg::R1).map_err(oops)?;
    enc.uxth(Reg::R1, Reg::R0).map_err(oops)?;
    enc.lsrs_imm(Reg::R0, Reg::R0, 16).map_err(oops)?;
    enc.uxth(Reg::R3, Reg::R2).map_err(oops)?;
    enc.lsrs_imm(Reg::R2, Reg::R2, 16).map_err(oops)?;
    enc.mov_reg(Reg::R5, Reg::R1);
    enc.muls(Reg::R5, Reg::R3).map_err(oops)?;
    enc.mov_reg(Reg::R6, Reg::R0);
    enc.muls(Reg::R6, Reg::R2).map_err(oops)?;
    enc.muls(Reg::R1, Reg::R2).map_err(oops)?;
    enc.muls(Reg::R0, Reg::R3).map_err(oops)?;
    enc.lsls_imm(Reg::R7, Reg::R1, 16).map_err(oops)?;
    enc.lsrs_imm(Reg::R1, Reg::R1, 16).map_err(oops)?;
    enc.adds(Reg::R5, Reg::R5, Reg::R7).map_err(oops)?;
    enc.adcs(Reg::R6, Reg::R1).map_err(oops)?;
    enc.lsls_imm(Reg::R7, Reg::R0, 16).map_err(oops)?;
    enc.lsrs_imm(Reg::R0, Reg::R0, 16).map_err(oops)?;
    enc.adds(Reg::R5, Reg::R5, Reg::R7).map_err(oops)?;
    enc.adcs(Reg::R6, Reg::R0).map_err(oops)?;
    enc.adds(Reg::R6, Reg::R6, Reg::R4).map_err(oops)?;
    enc.mov_reg(Reg::R0, Reg::R5);
    enc.mov_reg(Reg::R1, Reg::R6);
    enc.pop_registers(0xF0, false);
    Ok(())
}

/// Emits a 64-bit left shift `a << n` with `a` in r0:r1 (lo:hi) and the count in r2, leaving the
/// result in r0:r1. C# masks the count to 6 bits, and a register shift past 31 must be split, so
/// `n >= 32` and `n < 32` are separate paths. r4-r7 are saved/restored.
fn emit_shl64(enc: &mut Encoder) -> Result<(), LowerError> {
    let oops = |_| LowerError::TooManyValues;
    enc.push_registers(0xF0, false);
    enc.movs_imm(Reg::R3, 63).map_err(oops)?;
    enc.ands(Reg::R2, Reg::R3).map_err(oops)?;
    enc.cmp_imm(Reg::R2, 32).map_err(oops)?;
    let ge32 = enc.new_label();
    let done = enc.new_label();
    enc.b_cond(Cond::CarrySet, ge32);
    enc.mov_reg(Reg::R4, Reg::R0);
    enc.lsls_reg(Reg::R4, Reg::R2).map_err(oops)?;
    enc.mov_reg(Reg::R5, Reg::R1);
    enc.lsls_reg(Reg::R5, Reg::R2).map_err(oops)?;
    enc.movs_imm(Reg::R6, 32).map_err(oops)?;
    enc.subs(Reg::R6, Reg::R6, Reg::R2).map_err(oops)?;
    enc.mov_reg(Reg::R7, Reg::R0);
    enc.lsrs_reg(Reg::R7, Reg::R6).map_err(oops)?;
    enc.orrs(Reg::R5, Reg::R7).map_err(oops)?;
    enc.b(done);
    enc.bind_label(ge32);
    enc.movs_imm(Reg::R4, 0).map_err(oops)?;
    enc.movs_imm(Reg::R6, 32).map_err(oops)?;
    enc.subs(Reg::R6, Reg::R2, Reg::R6).map_err(oops)?;
    enc.mov_reg(Reg::R5, Reg::R0);
    enc.lsls_reg(Reg::R5, Reg::R6).map_err(oops)?;
    enc.bind_label(done);
    enc.mov_reg(Reg::R0, Reg::R4);
    enc.mov_reg(Reg::R1, Reg::R5);
    enc.pop_registers(0xF0, false);
    Ok(())
}

/// Emits a 64-bit right shift `a >> n` with `a` in r0:r1 (lo:hi) and the count in r2, leaving the
/// result in r0:r1. `signed` selects arithmetic (sign-filling, for `long`) over logical (zero-fill,
/// for `ulong`); the high-word fill differs only in the `n >= 32` case. r4-r7 are saved/restored.
fn emit_shr64(enc: &mut Encoder, signed: bool) -> Result<(), LowerError> {
    let oops = |_| LowerError::TooManyValues;
    enc.push_registers(0xF0, false);
    enc.movs_imm(Reg::R3, 63).map_err(oops)?;
    enc.ands(Reg::R2, Reg::R3).map_err(oops)?;
    enc.cmp_imm(Reg::R2, 32).map_err(oops)?;
    let ge32 = enc.new_label();
    let done = enc.new_label();
    enc.b_cond(Cond::CarrySet, ge32);
    enc.mov_reg(Reg::R4, Reg::R0);
    enc.lsrs_reg(Reg::R4, Reg::R2).map_err(oops)?;
    enc.movs_imm(Reg::R6, 32).map_err(oops)?;
    enc.subs(Reg::R6, Reg::R6, Reg::R2).map_err(oops)?;
    enc.mov_reg(Reg::R7, Reg::R1);
    enc.lsls_reg(Reg::R7, Reg::R6).map_err(oops)?;
    enc.orrs(Reg::R4, Reg::R7).map_err(oops)?;
    enc.mov_reg(Reg::R5, Reg::R1);
    if signed {
        enc.asrs_reg(Reg::R5, Reg::R2).map_err(oops)?;
    } else {
        enc.lsrs_reg(Reg::R5, Reg::R2).map_err(oops)?;
    }
    enc.b(done);
    enc.bind_label(ge32);
    enc.movs_imm(Reg::R6, 32).map_err(oops)?;
    enc.subs(Reg::R6, Reg::R2, Reg::R6).map_err(oops)?;
    enc.mov_reg(Reg::R4, Reg::R1);
    if signed {
        enc.asrs_reg(Reg::R4, Reg::R6).map_err(oops)?;
        enc.asrs_imm(Reg::R5, Reg::R1, 31).map_err(oops)?;
    } else {
        enc.lsrs_reg(Reg::R4, Reg::R6).map_err(oops)?;
        enc.movs_imm(Reg::R5, 0).map_err(oops)?;
    }
    enc.bind_label(done);
    enc.mov_reg(Reg::R0, Reg::R4);
    enc.mov_reg(Reg::R1, Reg::R5);
    enc.pop_registers(0xF0, false);
    Ok(())
}

/// Scales the array index in `r1` by `element_size` in place: a shift for a power of two, else a
/// multiply (the constant goes through `r2`). Leaves `r1 *= element_size`.
fn scale_index(
    enc: &mut Encoder,
    pool: &mut Vec<(Label, u32)>,
    element_size: u32,
) -> Result<(), LowerError> {
    if element_size == 1 {
        return Ok(());
    }
    if element_size.is_power_of_two() {
        enc.lsls_imm(Reg::R1, Reg::R1, element_size.trailing_zeros() as u8)
            .map_err(|_| LowerError::TooManyValues)?;
    } else {
        load_const_word(enc, pool, Reg::R2, element_size)?;
        enc.muls(Reg::R1, Reg::R2)
            .map_err(|_| LowerError::TooManyValues)?;
    }
    Ok(())
}

/// Emits the `Console.WriteLine(int)` routine: format the signed int already in `r0` as
/// decimal with a trailing newline into a 16-byte stack buffer, then `SYS_WRITE0` it.
/// Cortex-M0 (ARMv6-M) has no divide, so each digit comes from a shift-only unsigned
/// divide-by-10 (Hacker's Delight). Saves/restores r4-r7; r0-r3 are scratch on this path.
fn emit_write_int(enc: &mut Encoder) -> Result<(), LowerError> {
    let oops = |_| LowerError::TooManyValues;
    enc.push_registers(0b1111_0000, false);
    enc.sub_sp(16).map_err(oops)?;
    enc.add_sp_imm(Reg::R6, 0).map_err(oops)?;
    enc.asrs_imm(Reg::R4, Reg::R0, 31).map_err(oops)?;
    enc.eors(Reg::R0, Reg::R4).map_err(oops)?;
    enc.subs(Reg::R0, Reg::R0, Reg::R4).map_err(oops)?;
    enc.movs_imm(Reg::R5, 15).map_err(oops)?;
    enc.movs_imm(Reg::R2, 0).map_err(oops)?;
    enc.strb_reg(Reg::R2, Reg::R6, Reg::R5).map_err(oops)?;
    enc.subs_imm8(Reg::R5, 1).map_err(oops)?;
    enc.movs_imm(Reg::R2, b'\n').map_err(oops)?;
    enc.strb_reg(Reg::R2, Reg::R6, Reg::R5).map_err(oops)?;
    let loop_top = enc.new_label();
    let skip_corr = enc.new_label();
    enc.bind_label(loop_top);
    enc.lsrs_imm(Reg::R1, Reg::R0, 1).map_err(oops)?;
    enc.lsrs_imm(Reg::R3, Reg::R0, 2).map_err(oops)?;
    enc.adds(Reg::R1, Reg::R1, Reg::R3).map_err(oops)?;
    enc.lsrs_imm(Reg::R3, Reg::R1, 4).map_err(oops)?;
    enc.adds(Reg::R1, Reg::R1, Reg::R3).map_err(oops)?;
    enc.lsrs_imm(Reg::R3, Reg::R1, 8).map_err(oops)?;
    enc.adds(Reg::R1, Reg::R1, Reg::R3).map_err(oops)?;
    enc.lsrs_imm(Reg::R3, Reg::R1, 16).map_err(oops)?;
    enc.adds(Reg::R1, Reg::R1, Reg::R3).map_err(oops)?;
    enc.lsrs_imm(Reg::R1, Reg::R1, 3).map_err(oops)?;
    enc.lsls_imm(Reg::R3, Reg::R1, 3).map_err(oops)?;
    enc.lsls_imm(Reg::R2, Reg::R1, 1).map_err(oops)?;
    enc.adds(Reg::R3, Reg::R3, Reg::R2).map_err(oops)?;
    enc.subs(Reg::R2, Reg::R0, Reg::R3).map_err(oops)?;
    enc.cmp_imm(Reg::R2, 10).map_err(oops)?;
    enc.b_cond(Cond::CarryClear, skip_corr);
    enc.adds_imm8(Reg::R1, 1).map_err(oops)?;
    enc.subs_imm8(Reg::R2, 10).map_err(oops)?;
    enc.bind_label(skip_corr);
    enc.adds_imm8(Reg::R2, b'0').map_err(oops)?;
    enc.subs_imm8(Reg::R5, 1).map_err(oops)?;
    enc.strb_reg(Reg::R2, Reg::R6, Reg::R5).map_err(oops)?;
    enc.movs_reg(Reg::R0, Reg::R1).map_err(oops)?;
    enc.cmp_imm(Reg::R0, 0).map_err(oops)?;
    enc.b_cond(Cond::Ne, loop_top);
    let skip_sign = enc.new_label();
    enc.cmp_imm(Reg::R4, 0).map_err(oops)?;
    enc.b_cond(Cond::Eq, skip_sign);
    enc.subs_imm8(Reg::R5, 1).map_err(oops)?;
    enc.movs_imm(Reg::R2, b'-').map_err(oops)?;
    enc.strb_reg(Reg::R2, Reg::R6, Reg::R5).map_err(oops)?;
    enc.bind_label(skip_sign);
    enc.adds(Reg::R1, Reg::R6, Reg::R5).map_err(oops)?;
    enc.movs_imm(Reg::R0, 4).map_err(oops)?;
    enc.bkpt(0xAB);
    enc.add_sp(16).map_err(oops)?;
    enc.pop_registers(0b1111_0000, false);
    Ok(())
}

/// Emits `System.String::op_Equality`: an ordinal equality of the two string pointers in r0 and
/// r1 (each an ObjectRef to the build's string blob, or null), leaving 0/1 in r0. Two nulls are
/// equal, null and non-null are not, otherwise length-then-content over the stored units/bytes.
/// Pure compares plus an element loop (no divide). Saves/restores r4-r7.
fn emit_string_equals(enc: &mut Encoder) -> Result<(), LowerError> {
    let oops = |_| LowerError::TooManyValues;
    enc.push_registers(0b1111_0000, false);
    let not_same = enc.new_label();
    let zero = enc.new_label();
    let equal = enc.new_label();
    let done = enc.new_label();
    let loop_top = enc.new_label();
    enc.cmp_reg(Reg::R0, Reg::R1).map_err(oops)?;
    enc.b_cond(Cond::Ne, not_same);
    enc.movs_imm(Reg::R0, 1).map_err(oops)?;
    enc.b(done);
    enc.bind_label(not_same);
    enc.cmp_imm(Reg::R0, 0).map_err(oops)?;
    enc.b_cond(Cond::Eq, zero);
    enc.cmp_imm(Reg::R1, 0).map_err(oops)?;
    enc.b_cond(Cond::Eq, zero);
    enc.ldr_imm(Reg::R4, Reg::R0, 0).map_err(oops)?;
    enc.ldr_imm(Reg::R5, Reg::R1, 0).map_err(oops)?;
    enc.cmp_reg(Reg::R4, Reg::R5).map_err(oops)?;
    enc.b_cond(Cond::Ne, zero);
    #[cfg(not(any(feature = "string-utf8", feature = "string-utf8-wtf8")))]
    {
        enc.adds_imm8(Reg::R0, 4).map_err(oops)?;
        enc.adds_imm8(Reg::R1, 4).map_err(oops)?;
        enc.lsls_imm(Reg::R4, Reg::R4, 1).map_err(oops)?;
        enc.movs_imm(Reg::R6, 0).map_err(oops)?;
        enc.bind_label(loop_top);
        enc.cmp_reg(Reg::R6, Reg::R4).map_err(oops)?;
        enc.b_cond(Cond::CarrySet, equal);
        enc.ldrh_reg(Reg::R7, Reg::R0, Reg::R6).map_err(oops)?;
        enc.ldrh_reg(Reg::R5, Reg::R1, Reg::R6).map_err(oops)?;
        enc.cmp_reg(Reg::R7, Reg::R5).map_err(oops)?;
        enc.b_cond(Cond::Ne, zero);
        enc.adds_imm8(Reg::R6, 2).map_err(oops)?;
        enc.b(loop_top);
    }
    #[cfg(any(feature = "string-utf8", feature = "string-utf8-wtf8"))]
    {
        enc.ldr_imm(Reg::R4, Reg::R0, 4).map_err(oops)?;
        enc.ldr_imm(Reg::R5, Reg::R1, 4).map_err(oops)?;
        enc.cmp_reg(Reg::R4, Reg::R5).map_err(oops)?;
        enc.b_cond(Cond::Ne, zero);
        enc.adds_imm8(Reg::R0, 8).map_err(oops)?;
        enc.adds_imm8(Reg::R1, 8).map_err(oops)?;
        enc.movs_imm(Reg::R6, 0).map_err(oops)?;
        enc.bind_label(loop_top);
        enc.cmp_reg(Reg::R6, Reg::R4).map_err(oops)?;
        enc.b_cond(Cond::CarrySet, equal);
        enc.ldrb_reg(Reg::R7, Reg::R0, Reg::R6).map_err(oops)?;
        enc.ldrb_reg(Reg::R5, Reg::R1, Reg::R6).map_err(oops)?;
        enc.cmp_reg(Reg::R7, Reg::R5).map_err(oops)?;
        enc.b_cond(Cond::Ne, zero);
        enc.adds_imm8(Reg::R6, 1).map_err(oops)?;
        enc.b(loop_top);
    }
    enc.bind_label(equal);
    enc.movs_imm(Reg::R0, 1).map_err(oops)?;
    enc.b(done);
    enc.bind_label(zero);
    enc.movs_imm(Reg::R0, 0).map_err(oops)?;
    enc.bind_label(done);
    enc.pop_registers(0b1111_0000, false);
    Ok(())
}

/// Lowers a function whose values do not fit in registers into a shared encoder.
/// Every value gets a stack slot; each instruction loads its operands into scratch
/// registers, computes, and stores the result. Control flow is handled: because a
/// block's parameter values are distinct from any argument value, the parameter
/// copies on a jump need no ordering. `func_labels` resolves calls.
#[allow(clippy::too_many_arguments)]
/// Each value's frame-slot byte offset on the fully-spilled path, plus the total slot bytes.
/// ONE home computation shared by the lowering ([`lower_spilled_into`]) and the per-method
/// stack-map record builder ([`method_record_roots`]), so a record's root offsets can never
/// drift from the offsets the emitted stores actually use.
fn spilled_slot_offsets(func: &Function) -> (Vec<u16>, u16) {
    let mut offsets: Vec<u16> = Vec::with_capacity(func.value_types.len());
    let out = out_args_bytes(func);
    let sret_bytes: u16 =
        if matches!(func.ret, Some(MirType::ValueType { size, .. }) if size > 4) { 4 } else { 0 };
    let mut used = out.saturating_add(sret_bytes);
    for ty in &func.value_types {
        offsets.push(used);
        used = used.saturating_add(ty.stack_slot_bytes() as u16);
    }
    (offsets, used)
}

fn lower_spilled_into(
    func: &Function,
    enc: &mut Encoder,
    func_labels: &[Label],
    alloc_addr: Option<u32>,
    py_support: PySupport,
    source_map: &[Vec<u32>],
    line_table: &mut Vec<(u32, u32)>,
    stack_maps: &mut Vec<StackMapEntry>,
    vtables: &[TypeMeta],
    relocate: bool,
    blob_table: Option<&[Box<[u16]>]>,
    console_symbol: Option<u32>,
    string_header: Option<(u32, i32)>,
) -> Result<(), LowerError> {
    let has_calls = func.blocks.iter().any(|b| {
        b.insts.iter().any(|(_, i)| {
            (console_symbol.is_some() && matches!(i, Inst::SemihostWrite { .. }))
                || matches!(
                i,
                Inst::Call { .. }
                    | Inst::CallVirtual { .. }
                    | Inst::CallInterface { .. }
                    | Inst::CallIndirect { .. }
                    | Inst::CallNative { .. }
                    | Inst::InvokeDelegate { .. }
                    | Inst::CastClassScan { .. }
                    | Inst::PyIntrinsic { .. }
                    | Inst::Alloc { .. }
                    | Inst::AllocLike { .. }
                    | Inst::AllocDescribed { .. }
                    | Inst::AllocArray { .. }
                    | Inst::AllocArray2D { .. }
                    | Inst::AllocArrayMD { .. }
            )
        })
    });
    let invokes_delegate = func.blocks.iter().any(|b| {
        b.insts
            .iter()
            .any(|(_, i)| matches!(i, Inst::InvokeDelegate { .. }))
    });
    let saved_mask: u8 = if invokes_delegate { 0x30 } else { 0 };
    let saved_bytes: u16 = (saved_mask.count_ones() as u16 + 1) * 4;
    let lr_bytes = if has_calls { 4 } else { 0 };
    let returns_big_struct = matches!(func.ret, Some(MirType::ValueType { size, .. }) if size > 4);
    let (offsets, mut used) = spilled_slot_offsets(func);
    let result_ptr_off = out_args_bytes(func);
    let max_call_argc = func
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .filter_map(|(_, i)| match i {
            Inst::PyIntrinsic {
                op: lamella_ir::PyOp::Call,
                args,
                ..
            } => Some(args.len().saturating_sub(1)),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let argv_scratch_off = used;
    used = used.saturating_add((max_call_argc as u16) * 4);
    let frame = ((used as usize + lr_bytes + 7) & !7usize) - lr_bytes;
    if frame > 65024 {
        return Err(LowerError::TooManyValues);
    }
    let frame = frame as u16;
    let slot = |v: ValueId| offsets[v.0 as usize];

    let safepoints = crate::regalloc::safepoint_roots(func, &func.value_types);
    let record_safepoint =
        |stack_maps: &mut Vec<StackMapEntry>, index: usize, inst_pos: usize, return_pc: u32| {
            let mut ref_offsets = Vec::new();
            let mut tagged_offsets = Vec::new();
            if let Some(roots) = safepoints
                .get(index)
                .and_then(|b| b.get(inst_pos))
                .and_then(Option::as_ref)
            {
                for &v in roots {
                    if func.value_type(v) == Some(MirType::PyValue) {
                        tagged_offsets.push(slot(v));
                    } else {
                        ref_offsets.push(slot(v));
                    }
                }
            }
            stack_maps.push(StackMapEntry {
                return_pc,
                frame_size: frame,
                saved_bytes,
                ref_offsets,
                tagged_offsets,
            });
        };

    let mut pool: Vec<(Label, u32)> = Vec::new();
    let mut sym_pool: Vec<(Label, u32, i32)> = Vec::new();
    let mut strings: Vec<(Label, Box<[u8]>)> = Vec::new();
    let mut string_blobs: Vec<(Label, Box<[u16]>)> = Vec::new();
    let mut type_descs: Vec<(Label, Box<[u32]>)> = Vec::new();
    let mut type_desc_labels: Vec<(lamella_ir::TypeHandle, Label)> = Vec::new();
    let mut element_edges: Vec<(Label, lamella_ir::TypeHandle)> = Vec::new();
    if has_calls {
        enc.push_registers(saved_mask, true);
    }
    enc.sub_sp_far(frame).map_err(|_| LowerError::TooManyValues)?;

    let entry_block = func
        .blocks
        .get(func.entry.index())
        .ok_or(LowerError::ControlFlowUnsupported)?;
    let mut reg = 0u8;
    if returns_big_struct {
        enc.str_sp(Reg::R0, result_ptr_off)
            .map_err(|_| LowerError::TooManyValues)?;
        reg = 1;
    }
    let mut stack_param_off = 0u16;
    for &param in &entry_block.params {
        let ty = func.value_type(param);
        let words = ty.map_or(1, |t| (t.stack_slot_bytes() / 4).max(1));
        if matches!(ty, Some(MirType::I64 | MirType::F64)) && reg % 2 == 1 {
            reg += 1;
        }
        for w in 0..words {
            let woff = (w as u16) * 4;
            if reg < 4 {
                let r = Reg::new(reg).unwrap_or(Reg::R0);
                enc.str_sp(r, slot(param) + woff)
                    .map_err(|_| LowerError::TooManyValues)?;
                reg += 1;
            } else {
                slot_load(enc, Reg::R0, frame + lr_bytes as u16 + stack_param_off)?;
                enc.str_sp(Reg::R0, slot(param) + woff)
                    .map_err(|_| LowerError::TooManyValues)?;
                stack_param_off += 4;
            }
        }
    }

    if has_calls {
        let entry_params = &entry_block.params;
        let ref_slots: Vec<u16> = func
            .value_types
            .iter()
            .enumerate()
            .filter(|(v, _)| !entry_params.contains(&ValueId(*v as u32)))
            .flat_map(|(v, ty)| {
                let base = offsets[v];
                crate::stackmaps::slot_roots(*ty, false)
                    .map(move |(offset, _)| base + offset as u16)
                    .collect::<Vec<_>>()
            })
            .collect();
        if !ref_slots.is_empty() {
            enc.movs_imm(Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            for off in ref_slots {
                slot_store(enc, Reg::R0, off, Reg::R2)?;
            }
        }
    }

    let block_labels: Vec<Label> = (0..func.blocks.len()).map(|_| enc.new_label()).collect();
    match block_labels.get(func.entry.index()) {
        Some(entry) if func.entry != BlockId(0) => enc.b(*entry),
        Some(_) => {}
        None => return Err(LowerError::ControlFlowUnsupported),
    }

    for (index, block) in func.blocks.iter().enumerate() {
        enc.bind_label(block_labels[index]);
        for (inst_pos, (result, inst)) in block.insts.iter().enumerate() {
            if let Some(&cil) = source_map.get(index).and_then(|b| b.get(inst_pos)) {
                line_table.push((enc.position(), cil));
            }
            if matches!(inst, Inst::InitStruct) {
                let bytes = func
                    .value_type(*result)
                    .map_or(0, MirType::stack_slot_bytes);
                enc.movs_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                for w in 0..(bytes / 4) {
                    slot_store(enc, Reg::R0, slot(*result) + (w as u16) * 4, Reg::R2)?;
                }
                continue;
            }
            if let Inst::CopyStruct { src } = inst {
                let bytes = func
                    .value_type(*result)
                    .map_or(0, MirType::stack_slot_bytes);
                for w in 0..(bytes / 4) {
                    let off = (w as u16) * 4;
                    slot_load(enc, Reg::R0, slot(*src) + off)?;
                    slot_store(enc, Reg::R0, slot(*result) + off, Reg::R2)?;
                }
                continue;
            }
            if let Inst::FieldStore {
                base,
                offset,
                value,
            } = inst
            {
                if let Some(MirType::ValueType { size, .. }) = func.value_type(*value) {
                    let full_words = (size / 4) as u16;
                    let rem = (size % 4) as u16;
                    let ptr = is_pointer_base(&func.value_types, *base);
                    if ptr {
                        slot_load(enc, Reg::R1, slot(*base))?;
                    }
                    for w in 0..full_words {
                        slot_load(enc, Reg::R0, slot(*value) + w * 4)?;
                        if ptr {
                            enc.str_imm(Reg::R0, Reg::R1, *offset as u16 + w * 4)
                                .map_err(|_| LowerError::TooManyValues)?;
                        } else {
                            slot_store(enc, Reg::R0, slot(*base) + *offset as u16 + w * 4, Reg::R2)?;
                        }
                    }
                    for k in 0..rem {
                        let at = full_words * 4 + k;
                        slot_addr(enc, Reg::R1, slot(*value))?;
                        narrow_load_at(enc, Reg::R0, Reg::R1, at as u32, 1, false)?;
                        if ptr {
                            slot_load(enc, Reg::R1, slot(*base))?;
                        } else {
                            slot_addr(enc, Reg::R1, slot(*base))?;
                        }
                        narrow_store_at(enc, Reg::R0, Reg::R1, *offset + at as u32, 1)?;
                    }
                    continue;
                }
            }
            if let Inst::FieldLoad { base, offset } = inst {
                if let Some(MirType::ValueType { size, .. }) = func.value_type(*result) {
                    let full_words = (size / 4) as u16;
                    let rem = (size % 4) as u16;
                    let ptr = is_pointer_base(&func.value_types, *base);
                    if ptr {
                        slot_load(enc, Reg::R1, slot(*base))?;
                    }
                    for w in 0..full_words {
                        if ptr {
                            enc.ldr_imm(Reg::R0, Reg::R1, *offset as u16 + w * 4)
                                .map_err(|_| LowerError::TooManyValues)?;
                        } else {
                            slot_load(enc, Reg::R0, slot(*base) + *offset as u16 + w * 4)?;
                        }
                        slot_store(enc, Reg::R0, slot(*result) + w * 4, Reg::R2)?;
                    }
                    for k in 0..rem {
                        let at = full_words * 4 + k;
                        if ptr {
                            slot_load(enc, Reg::R1, slot(*base))?;
                        } else {
                            slot_addr(enc, Reg::R1, slot(*base))?;
                        }
                        narrow_load_at(enc, Reg::R0, Reg::R1, *offset + at as u32, 1, false)?;
                        slot_addr(enc, Reg::R1, slot(*result))?;
                        narrow_store_at(enc, Reg::R0, Reg::R1, at as u32, 1)?;
                    }
                    continue;
                }
            }
            if let Inst::Call { callee, args } = inst {
                if matches!(func.value_type(*result), Some(MirType::ValueType { size, .. }) if size > 4)
                {
                    slot_addr(enc, Reg::R0, slot(*result))?;
                    load_call_args(enc, &func.value_types, &slot, args, 1)?;
                    if relocate {
                        enc.bl_symbol(*callee);
                    } else {
                        let target = *func_labels
                            .get(*callee as usize)
                            .ok_or(LowerError::CallUnsupported)?;
                        enc.bl(target);
                    }
                    record_safepoint(stack_maps, index, inst_pos, enc.safepoint_label());
                    continue;
                }
            }
            if let Inst::PyIntrinsic { op, args, cache } = inst {
                match op {
                    lamella_ir::PyOp::Getattr { name } => {
                        let support = py_support.getattr.ok_or(LowerError::CallUnsupported)?;
                        let receiver = *args.first().ok_or(LowerError::CallUnsupported)?;
                        slot_load(enc, Reg::R0, slot(receiver))?;
                        load_const_word(enc, &mut pool, Reg::R1, *name)?;
                        load_const_word(enc, &mut pool, Reg::R2, *cache)?;
                        load_const_word(enc, &mut pool, Reg::R3, support)?;
                        enc.blx(Reg::R3);
                    }
                    lamella_ir::PyOp::Len => {
                        let support = py_support.len.ok_or(LowerError::CallUnsupported)?;
                        let x = *args.first().ok_or(LowerError::CallUnsupported)?;
                        slot_load(enc, Reg::R0, slot(x))?;
                        load_const_word(enc, &mut pool, Reg::R1, support)?;
                        enc.blx(Reg::R1);
                    }
                    lamella_ir::PyOp::Call => {
                        let support = py_support.call.ok_or(LowerError::CallUnsupported)?;
                        let callee = *args.first().ok_or(LowerError::CallUnsupported)?;
                        for (i, &arg) in args[1..].iter().enumerate() {
                            slot_load(enc, Reg::R0, slot(arg))?;
                            slot_store(enc, Reg::R0, argv_scratch_off + (i as u16) * 4, Reg::R2)?;
                        }
                        slot_load(enc, Reg::R0, slot(callee))?;
                        slot_addr(enc, Reg::R1, argv_scratch_off)?;
                        load_const_word(enc, &mut pool, Reg::R2, (args.len() - 1) as u32)?;
                        load_const_word(enc, &mut pool, Reg::R3, support)?;
                        enc.blx(Reg::R3);
                    }
                    _ => return Err(LowerError::CallUnsupported),
                }
                record_safepoint(stack_maps, index, inst_pos, enc.safepoint_label());
                if op.result_type().is_some() {
                    slot_store(enc, Reg::R0, slot(*result), Reg::R2)?;
                }
                continue;
            }
            if let Inst::Alloc {
                handle,
                payload_size,
                ref_offsets,
            } = inst
            {
                let alloc = alloc_addr.ok_or(LowerError::CallUnsupported)?;
                let desc_label = match type_desc_labels.iter().find(|(h, _)| h == handle) {
                    Some((_, label)) => *label,
                    None => {
                        let label = enc.new_label();
                        let mut words: Vec<u32> = Vec::with_capacity(3 + ref_offsets.len());
                        words.push(*payload_size);
                        words.push(ref_offsets.len() as u32);
                        let type_tag = vtables
                            .iter()
                            .find(|m| m.handle == *handle)
                            .map_or(0, |m| m.type_tag);
                        words.push(type_tag);
                        words.extend_from_slice(ref_offsets);
                        type_descs.push((label, words.into_boxed_slice()));
                        type_desc_labels.push((*handle, label));
                        label
                    }
                };
                load_const_word(enc, &mut pool, Reg::R0, *payload_size)?;
                enc.adr(Reg::R1, desc_label)
                    .map_err(|_| LowerError::TooManyValues)?;
                load_const_word(enc, &mut pool, Reg::R2, alloc)?;
                enc.blx(Reg::R2);
                record_safepoint(stack_maps, index, inst_pos, enc.safepoint_label());
                let ok = enc.new_label();
                enc.cmp_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, ok);
                enc.udf(0);
                enc.bind_label(ok);
                slot_store(enc, Reg::R0, slot(*result), Reg::R2)?;
                continue;
            }
            if let Inst::AllocLike {
                proto,
                payload_size,
            } = inst
            {
                let alloc = alloc_addr.ok_or(LowerError::CallUnsupported)?;
                load_const_word(enc, &mut pool, Reg::R0, *payload_size)?;
                slot_load(enc, Reg::R1, slot(*proto))?;
                enc.subs_imm8(Reg::R1, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_imm(Reg::R1, Reg::R1, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                load_const_word(enc, &mut pool, Reg::R2, alloc)?;
                enc.blx(Reg::R2);
                record_safepoint(stack_maps, index, inst_pos, enc.safepoint_label());
                let ok = enc.new_label();
                enc.cmp_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, ok);
                enc.udf(0);
                enc.bind_label(ok);
                slot_store(enc, Reg::R0, slot(*result), Reg::R2)?;
                continue;
            }
            if let Inst::AllocDescribed {
                descriptor,
                payload_size,
            } = inst
            {
                let alloc = alloc_addr.ok_or(LowerError::CallUnsupported)?;
                slot_load(enc, Reg::R0, slot(*payload_size))?;
                slot_load(enc, Reg::R1, slot(*descriptor))?;
                load_const_word(enc, &mut pool, Reg::R2, alloc)?;
                enc.blx(Reg::R2);
                record_safepoint(stack_maps, index, inst_pos, enc.safepoint_label());
                let ok = enc.new_label();
                enc.cmp_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, ok);
                enc.udf(0);
                enc.bind_label(ok);
                slot_store(enc, Reg::R0, slot(*result), Reg::R2)?;
                continue;
            }
            if let Inst::TypeDescAddr { handle } = inst {
                let desc_label = match type_desc_labels.iter().find(|(h, _)| h == handle) {
                    Some((_, label)) => *label,
                    None => {
                        let label = enc.new_label();
                        type_descs.push((label, alloc::vec![0u32, 0u32, 0u32].into_boxed_slice()));
                        type_desc_labels.push((*handle, label));
                        label
                    }
                };
                enc.adr(Reg::R0, desc_label)
                    .map_err(|_| LowerError::TooManyValues)?;
                slot_store(enc, Reg::R0, slot(*result), Reg::R2)?;
                continue;
            }
            if let Inst::LoadTypeDesc { object } = inst {
                slot_load(enc, Reg::R0, slot(*object))?;
                let not_null = enc.new_label();
                enc.cmp_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Eq, not_null);
                enc.subs_imm8(Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_imm(Reg::R0, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.bind_label(not_null);
                slot_store(enc, Reg::R0, slot(*result), Reg::R2)?;
                continue;
            }
            if let Inst::AllocArray {
                handle,
                element,
                length,
                element_size,
                element_kind,
            } = inst
            {
                let alloc = alloc_addr.ok_or(LowerError::CallUnsupported)?;
                let desc_label = match type_desc_labels.iter().find(|(h, _)| h == handle) {
                    Some((_, label)) => *label,
                    None => {
                        let label = enc.new_label();
                        type_descs.push((
                            label,
                            alloc::vec![ARRAY_DESC_MARK | 1, *element_kind, 0u32]
                                .into_boxed_slice(),
                        ));
                        type_desc_labels.push((*handle, label));
                        if let Some(element) = element {
                            element_edges.push((label, *element));
                        }
                        label
                    }
                };
                slot_load(enc, Reg::R0, slot(*length))?;
                if *element_size != 1 {
                    if element_size.is_power_of_two() {
                        enc.lsls_imm(Reg::R0, Reg::R0, element_size.trailing_zeros() as u8)
                            .map_err(|_| LowerError::TooManyValues)?;
                    } else {
                        load_const_word(enc, &mut pool, Reg::R1, *element_size)?;
                        enc.muls(Reg::R0, Reg::R1)
                            .map_err(|_| LowerError::TooManyValues)?;
                    }
                }
                enc.adds_imm8(Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.adr(Reg::R1, desc_label)
                    .map_err(|_| LowerError::TooManyValues)?;
                load_const_word(enc, &mut pool, Reg::R2, alloc)?;
                enc.blx(Reg::R2);
                record_safepoint(stack_maps, index, inst_pos, enc.safepoint_label());
                let ok = enc.new_label();
                enc.cmp_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, ok);
                enc.udf(0);
                enc.bind_label(ok);
                slot_load(enc, Reg::R1, slot(*length))?;
                enc.str_imm(Reg::R1, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                slot_store(enc, Reg::R0, slot(*result), Reg::R2)?;
                continue;
            }
            if let Inst::AllocArray2D {
                handle,
                dim0,
                dim1,
                element_size,
            } = inst
            {
                let alloc = alloc_addr.ok_or(LowerError::CallUnsupported)?;
                let desc_label = match type_desc_labels.iter().find(|(h, _)| h == handle) {
                    Some((_, label)) => *label,
                    None => {
                        let label = enc.new_label();
                        type_descs.push((label, alloc::vec![0u32, 0u32, 0u32].into_boxed_slice()));
                        type_desc_labels.push((*handle, label));
                        label
                    }
                };
                slot_load(enc, Reg::R0, slot(*dim0))?;
                slot_load(enc, Reg::R1, slot(*dim1))?;
                enc.muls(Reg::R0, Reg::R1)
                    .map_err(|_| LowerError::TooManyValues)?;
                if *element_size != 1 {
                    if element_size.is_power_of_two() {
                        enc.lsls_imm(Reg::R0, Reg::R0, element_size.trailing_zeros() as u8)
                            .map_err(|_| LowerError::TooManyValues)?;
                    } else {
                        load_const_word(enc, &mut pool, Reg::R1, *element_size)?;
                        enc.muls(Reg::R0, Reg::R1)
                            .map_err(|_| LowerError::TooManyValues)?;
                    }
                }
                enc.adds_imm8(Reg::R0, 8)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.adr(Reg::R1, desc_label)
                    .map_err(|_| LowerError::TooManyValues)?;
                load_const_word(enc, &mut pool, Reg::R2, alloc)?;
                enc.blx(Reg::R2);
                record_safepoint(stack_maps, index, inst_pos, enc.safepoint_label());
                let ok = enc.new_label();
                enc.cmp_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, ok);
                enc.udf(0);
                enc.bind_label(ok);
                slot_load(enc, Reg::R1, slot(*dim0))?;
                enc.str_imm(Reg::R1, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                slot_load(enc, Reg::R1, slot(*dim1))?;
                enc.str_imm(Reg::R1, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                slot_store(enc, Reg::R0, slot(*result), Reg::R2)?;
                continue;
            }
            if let Inst::AllocArrayMD {
                handle,
                dims,
                element_size,
            } = inst
            {
                let alloc = alloc_addr.ok_or(LowerError::CallUnsupported)?;
                let desc_label = match type_desc_labels.iter().find(|(h, _)| h == handle) {
                    Some((_, label)) => *label,
                    None => {
                        let label = enc.new_label();
                        type_descs.push((label, alloc::vec![0u32, 0u32, 0u32].into_boxed_slice()));
                        type_desc_labels.push((*handle, label));
                        label
                    }
                };
                let n = dims.len();
                slot_load(enc, Reg::R0, slot(dims[0]))?;
                for d in &dims[1..] {
                    slot_load(enc, Reg::R1, slot(*d))?;
                    enc.muls(Reg::R0, Reg::R1)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
                if *element_size != 1 {
                    if element_size.is_power_of_two() {
                        enc.lsls_imm(Reg::R0, Reg::R0, element_size.trailing_zeros() as u8)
                            .map_err(|_| LowerError::TooManyValues)?;
                    } else {
                        load_const_word(enc, &mut pool, Reg::R1, *element_size)?;
                        enc.muls(Reg::R0, Reg::R1)
                            .map_err(|_| LowerError::TooManyValues)?;
                    }
                }
                enc.adds_imm8(Reg::R0, (4 * n) as u8)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.adr(Reg::R1, desc_label)
                    .map_err(|_| LowerError::TooManyValues)?;
                load_const_word(enc, &mut pool, Reg::R2, alloc)?;
                enc.blx(Reg::R2);
                record_safepoint(stack_maps, index, inst_pos, enc.safepoint_label());
                let ok = enc.new_label();
                enc.cmp_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, ok);
                enc.udf(0);
                enc.bind_label(ok);
                for (k, d) in dims.iter().enumerate() {
                    slot_load(enc, Reg::R1, slot(*d))?;
                    enc.str_imm(Reg::R1, Reg::R0, (4 * k) as u16)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
                slot_store(enc, Reg::R0, slot(*result), Reg::R2)?;
                continue;
            }
            let call_pc = lower_spilled_inst(
                enc,
                &mut pool,
                &mut sym_pool,
                &mut strings,
                &mut string_blobs,
                &func.value_types,
                &slot,
                inst,
                func.value_type(*result),
                func_labels,
                relocate,
                blob_table,
                console_symbol,
            )?;
            if let Some(return_pc) = call_pc {
                record_safepoint(stack_maps, index, inst_pos, return_pc);
            }
            slot_store(enc, Reg::R0, slot(*result), Reg::R2)?;
            if matches!(func.value_type(*result), Some(MirType::I64 | MirType::F64)) {
                slot_store(enc, Reg::R1, slot(*result) + 4, Reg::R2)?;
            }
        }
        if let Some(&cil) = source_map.get(index).and_then(|b| b.last()) {
            line_table.push((enc.position(), cil));
        }
        match &block.terminator {
            Some(Terminator::Return(value)) => {
                if returns_big_struct {
                    if let Some(v) = value {
                        let size = func.value_type(*v).map_or(0, MirType::stack_slot_bytes);
                        slot_load(enc, Reg::R1, result_ptr_off)?;
                        for w in 0..(size / 4) {
                            let off = (w as u16) * 4;
                            slot_load(enc, Reg::R0, slot(*v) + off)?;
                            enc.str_imm(Reg::R0, Reg::R1, off)
                                .map_err(|_| LowerError::TooManyValues)?;
                        }
                        slot_load(enc, Reg::R0, result_ptr_off)?;
                    }
                } else if let Some(v) = value {
                    slot_load(enc, Reg::R0, slot(*v))?;
                    if matches!(func.value_type(*v), Some(MirType::I64 | MirType::F64)) {
                        slot_load(enc, Reg::R1, slot(*v) + 4)?;
                    }
                }
                enc.add_sp_far(frame).map_err(|_| LowerError::TooManyValues)?;
                if has_calls {
                    enc.pop_registers(saved_mask, true);
                } else {
                    enc.bx(Reg::LR);
                }
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
                    let bytes = func.value_type(*a).map_or(4, |t| t.stack_slot_bytes() as u16);
                    let mut off = 0u16;
                    while off < bytes {
                        slot_load(enc, Reg::R0, slot(*a) + off)?;
                        slot_store(enc, Reg::R0, slot(*p) + off, Reg::R2)?;
                        off += 4;
                    }
                }
                let label = *block_labels
                    .get(target.index())
                    .ok_or(LowerError::ControlFlowUnsupported)?;
                enc.b(label);
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
                let true_label = *block_labels
                    .get(if_true.index())
                    .ok_or(LowerError::ControlFlowUnsupported)?;
                let false_label = *block_labels
                    .get(if_false.index())
                    .ok_or(LowerError::ControlFlowUnsupported)?;
                slot_load(enc, Reg::R0, slot(*cond))?;
                enc.cmp_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, true_label);
                enc.b(false_label);
            }
            Some(Terminator::Unreachable) => {
                enc.udf(0);
            }
            None => return Err(LowerError::ControlFlowUnsupported),
        }
    }

    if !pool.is_empty() {
        enc.align_to_word();
        for (entry, value) in pool {
            enc.pool_word(entry, value);
        }
    }
    if !sym_pool.is_empty() {
        enc.align_to_word();
        for (entry, symbol, addend) in sym_pool {
            enc.pool_word_symbol_addend(entry, symbol, addend);
        }
    }
    for (entry, text) in strings {
        enc.align_to_word();
        enc.bind_label(entry);
        let blob_start = enc.position();
        enc.emit_bytes(&text);
        enc.mark_blob(entry, enc.position() - blob_start);
    }
    for (entry, utf16) in string_blobs {
        enc.align_to_word();
        let header = if string_header.is_some() { 4 } else { 0 };
        if let Some((handle, vtable_bytes)) = string_header {
            enc.data_word_symbol_addend(DESC_SYMBOL_FLAG | handle, vtable_bytes);
        }
        enc.bind_label(entry);
        let blob_start = enc.position();
        enc.emit_bytes(&unencodable(crate::stringgen::string_blob_bytes(&utf16))?);
        enc.mark_blob_with_prefix(entry, header, enc.position() - blob_start);
    }
    let mut ancestor_i = 0;
    while ancestor_i < type_desc_labels.len() {
        let handle = type_desc_labels[ancestor_i].0;
        if let Some(base) = vtables
            .iter()
            .find(|m| m.handle == handle)
            .and_then(|m| m.base)
        {
            if !type_desc_labels.iter().any(|(h, _)| *h == base) {
                let label = enc.new_label();
                let type_tag = vtables
                    .iter()
                    .find(|m| m.handle == base)
                    .map_or(0, |m| m.type_tag);
                type_descs.push((label, alloc::vec![0u32, 0u32, type_tag].into_boxed_slice()));
                type_desc_labels.push((base, label));
            }
        }
        ancestor_i += 1;
    }
    let laid_descriptors: Vec<(Label, u32)> = type_descs
        .iter()
        .map(|(label, words)| (*label, words.first().copied().unwrap_or(0)))
        .collect();
    for (entry, words) in type_descs {
        enc.align_to_word();
        let meta = type_desc_labels
            .iter()
            .find(|(_, label)| *label == entry)
            .map(|(handle, _)| *handle)
            .and_then(|handle| vtables.iter().find(|m| m.handle == handle));
        if let Some(meta) = meta {
            for slot in meta.vtable.iter().rev() {
                let crate::resolver::VtableEntry::Func(func_index) = slot else {
                    continue;
                };
                if let Some(&label) = func_labels.get(*func_index as usize) {
                    enc.data_word_diff(entry, label);
                }
            }
        }
        enc.bind_label(entry);
        for &word in words.iter().take(3) {
            enc.emit_word(word);
        }
        match meta.and_then(|m| m.base).and_then(|base| {
            type_desc_labels
                .iter()
                .find(|(h, _)| *h == base)
                .map(|(_, l)| *l)
        }) {
            Some(base_label) => enc.data_word_diff(entry, base_label),
            None => enc.emit_word(0),
        }
        if words.first().is_some_and(|w| {
            w & crate::resolver::ARRAY_DESC_MARK_MASK == crate::resolver::ARRAY_DESC_MARK
        }) {
            match element_edges
                .iter()
                .find(|(label, _)| *label == entry)
                .and_then(|(_, element)| {
                    type_desc_labels
                        .iter()
                        .find(|(h, _)| h == element)
                        .map(|(_, l)| *l)
                })
                .filter(|element_label| {
                    words.get(1) == Some(&crate::resolver::ELEMENT_KIND_REFERENCE)
                        || laid_descriptors
                            .iter()
                            .find(|(l, _)| l == element_label)
                            .is_some_and(|(_, payload)| *payload != 0)
                }) {
                Some(element_label) => enc.data_word_diff(entry, element_label),
                None => enc.emit_word(0),
            }
        }
        for &word in words.iter().skip(3) {
            enc.emit_word(word);
        }
        if let Some(meta) = meta {
            if !meta.itable.is_empty() {
                enc.emit_word(meta.itable.len() as u32);
                for (tag, impl_) in &meta.itable {
                    enc.emit_word(*tag);
                    let crate::resolver::VtableEntry::Func(func_index) = impl_ else {
                        continue;
                    };
                    if let Some(&label) = func_labels.get(*func_index as usize) {
                        enc.data_word_diff(entry, label);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Lowers an integer [`Function`] -- straight-line or branching -- to ARMv6-M
/// Thumb machine code via the AAPCS convention. See the module documentation for
/// the supported slice.
/// Where a value lives in a register/spill mix: a machine register, or a spill slot at
/// a byte offset from the stack pointer.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Home {
    Reg(Reg),
    Spill(u16),
}

/// How a function's values are placed: all in registers (with the callee-saved set to
/// preserve); a register/spill mix (the rest reach the stack, reserving r0/r1 to shuttle
/// spilled operands); or every value spilled (the fully general path for the cases the
/// register path does not model -- i64, value types, semihosting, calls with live values).
enum Assignment {
    Registers {
        regs: Vec<Reg>,
        saved: u8,
    },
    Mixed {
        homes: Vec<Home>,
        saved: u8,
        frame: u16,
    },
    Spilled,
}

/// Verifies `func` and decides where its values live.
fn prepare(func: &Function) -> Result<Assignment, LowerError> {
    if lamella_ir::verify(func).is_err() {
        return Err(LowerError::NotWellFormed);
    }
    if func.value_types.iter().any(|ty| ty.is_float()) {
        return Ok(Assignment::Spilled);
    }
    if func.value_types.iter().any(|ty| {
        matches!(
            ty,
            MirType::I64 | MirType::ValueType { .. } | MirType::ManagedPtr
        )
    }) {
        return Ok(Assignment::Spilled);
    }
    if func.params.len() > 4
        || func.blocks.iter().any(|b| {
            b.insts
                .iter()
                .any(|(_, i)| matches!(i, Inst::Call { args, .. } if args.len() > 4))
        })
    {
        return Ok(Assignment::Spilled);
    }
    if func.blocks.iter().any(|b| {
        b.insts.iter().any(|(_, i)| {
            matches!(
                i,
                Inst::SemihostWrite { .. }
                    | Inst::WriteInt { .. }
                    | Inst::StringLiteral { .. }
                    | Inst::StringEquals { .. }
                    | Inst::StringConcat { .. }
                    | Inst::IntToString { .. }
                    | Inst::Binary {
                        op: BinOp::DivSigned
                            | BinOp::DivUnsigned
                            | BinOp::RemSigned
                            | BinOp::RemUnsigned,
                        ..
                    }
                    | Inst::Alloc { .. }
                    | Inst::AllocLike { .. }
                    | Inst::AllocDescribed { .. }
                    | Inst::AllocArray { .. }
                    | Inst::ArrayLoad { .. }
                    | Inst::ArrayStore { .. }
                    | Inst::ArrayElemAddr { .. }
                    | Inst::AllocArray2D { .. }
                    | Inst::Array2DLoad { .. }
                    | Inst::Array2DStore { .. }
                    | Inst::AllocArrayMD { .. }
                    | Inst::ArrayMDLoad { .. }
                    | Inst::ArrayMDStore { .. }
                    | Inst::StaticLoad { .. }
                    | Inst::StaticStore { .. }
                    | Inst::LoadTypeDesc { .. }
                    | Inst::TypeDescAddr { .. }
                    | Inst::CallVirtual { .. }
                    | Inst::CallInterface { .. }
                    | Inst::CastClassScan { .. }
                    | Inst::PyIntrinsic { .. }
                    | Inst::CallIndirect { .. }
                    | Inst::CallNative { .. }
                    | Inst::InvokeDelegate { .. }
                    | Inst::FuncAddr { .. }
                    | Inst::VirtualFuncAddr { .. }
                    | Inst::TypeDescLiteral { .. }
                    | Inst::CopyBlock { .. }
                    | Inst::FillBlock { .. }
            )
        })
    }) {
        return Ok(Assignment::Spilled);
    }
    if func.blocks.iter().any(|b| {
        b.insts.iter().any(|(_, i)| {
            matches!(
                i,
                Inst::FieldLoad { .. }
                | Inst::FieldStore { .. }
                | Inst::FieldLoadNarrow { .. }
                | Inst::FieldStoreNarrow { .. }
                | Inst::FieldAddr { .. }
            )
        })
    }) {
        return Ok(Assignment::Spilled);
    }
    let has_calls = func.blocks.iter().any(|b| {
        b.insts
            .iter()
            .any(|(_, i)| crate::regalloc::is_safepoint(i))
    });
    if has_calls && crate::regalloc::Liveness::analyze(func).any_value_live_across_call(func) {
        return Ok(Assignment::Spilled);
    }
    if func.value_types.len() <= 8 {
        let regs: Vec<Reg> = (0..func.value_types.len())
            .map(|i| Reg::new(i as u8).unwrap_or(Reg::R0))
            .collect();
        return Ok(Assignment::Registers {
            saved: contiguous_callee_saved(&regs),
            regs,
        });
    }
    let live = crate::regalloc::Liveness::analyze(func);
    let intervals = crate::regalloc::live_intervals(func, &live);
    let full = crate::regalloc::allocate(&intervals, 8);
    if full.spill_count == 0 {
        let regs: Vec<Reg> = full
            .locations
            .iter()
            .map(|loc| match loc {
                crate::regalloc::Location::Register(r) => Reg::new(*r as u8).unwrap_or(Reg::R0),
                crate::regalloc::Location::Spill(_) => Reg::R0,
            })
            .collect();
        return Ok(Assignment::Registers {
            saved: contiguous_callee_saved(&regs),
            regs,
        });
    }
    let allocatable = [Reg::R2, Reg::R3, Reg::R4, Reg::R5, Reg::R6, Reg::R7];
    let mixed = crate::regalloc::allocate(&intervals, allocatable.len());
    let homes: Vec<Home> = mixed
        .locations
        .iter()
        .map(|loc| match loc {
            crate::regalloc::Location::Register(r) => Home::Reg(allocatable[*r as usize]),
            crate::regalloc::Location::Spill(slot) => Home::Spill((*slot as u16) * 4),
        })
        .collect();
    let saved = sparse_callee_saved(&homes);
    let has_calls = func.blocks.iter().any(|b| {
        b.insts.iter().any(|(_, i)| {
            crate::regalloc::is_safepoint(i) || matches!(i, Inst::CastClassScan { .. })
        })
    });
    let lr_bytes = if has_calls { 4 } else { 0 };
    let pushed = saved.count_ones() as usize * 4 + lr_bytes;
    let frame = ((pushed + mixed.spill_count as usize * 4 + 7) & !7usize) - pushed;
    if frame > 1020 {
        return Ok(Assignment::Spilled);
    }
    Ok(Assignment::Mixed {
        homes,
        saved,
        frame: frame as u16,
    })
}

/// The callee-saved push mask (r4-r7) for a contiguous register assignment: every
/// preserved register up to the highest one used. Matches the trivial and no-spill
/// scans, which claim registers in a low-to-high prefix.
fn contiguous_callee_saved(regs: &[Reg]) -> u8 {
    let used = regs
        .iter()
        .map(|r| u32::from(r.number()) + 1)
        .max()
        .unwrap_or(0);
    if used > 4 {
        (((1u16 << used.min(8)) - (1u16 << 4)) & 0xF0) as u8
    } else {
        0
    }
}

/// The callee-saved push mask (r4-r7) for a register/spill mix: exactly the preserved
/// registers that hold a value, since the scan over r2-r7 may leave gaps.
fn sparse_callee_saved(homes: &[Home]) -> u8 {
    let mut mask = 0u8;
    for h in homes {
        if let Home::Reg(r) = h {
            if (4..=7).contains(&r.number()) {
                mask |= 1 << r.number();
            }
        }
    }
    mask
}

/// Lowers a `Call`: arguments into r0-r3, `BL` to the callee, result from r0. The
/// caller-saved registers (r0-r3, r12) do not survive the call -- correct as long
/// as the caller keeps no still-needed value parked in one across the call.
fn lower_call(
    enc: &mut Encoder,
    assign: &impl Fn(ValueId) -> Reg,
    result: ValueId,
    callee: u32,
    args: &[ValueId],
    func_labels: &[Label],
    relocate: bool,
) -> Result<(), LowerError> {
    if args.len() > 4 {
        return Err(LowerError::CallUnsupported);
    }
    let moves: Vec<(Reg, Reg)> = args
        .iter()
        .enumerate()
        .map(|(i, a)| (Reg::new(i as u8).unwrap_or(Reg::R0), assign(*a)))
        .collect();
    emit_parallel_move(enc, &moves);
    if relocate {
        enc.bl_symbol(callee);
    } else {
        let target = *func_labels
            .get(callee as usize)
            .ok_or(LowerError::CallUnsupported)?;
        enc.bl(target);
    }
    if assign(result) != Reg::R0 {
        enc.mov_reg(assign(result), Reg::R0);
    }
    Ok(())
}

/// Lowers one function's body into a shared encoder, given its register
/// assignment. `func_labels` resolves `Call` targets by program index; pass an
/// empty slice for a function that makes no calls. `relocate` makes each `Call` an
/// `R_ARM_THM_CALL` relocation (object emission) rather than a resolved branch.
#[allow(clippy::too_many_arguments)]
fn lower_into(
    func: &Function,
    enc: &mut Encoder,
    regs: &[Reg],
    saved: u8,
    func_labels: &[Label],
    source_map: &[Vec<u32>],
    line_table: &mut Vec<(u32, u32)>,
    stack_maps: &mut Vec<StackMapEntry>,
    relocate: bool,
) -> Result<(), LowerError> {
    let assign = |v: ValueId| regs.get(v.index()).copied().unwrap_or(Reg::R0);
    let has_calls = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| matches!(i, Inst::Call { .. })));
    let saved_bytes: u16 = (saved.count_ones() as u16 + 1) * 4;

    if has_calls || saved != 0 {
        enc.push_registers(saved, has_calls);
    }
    let mut pool: Vec<(Label, u32)> = Vec::new();
    let block_labels: Vec<Label> = (0..func.blocks.len()).map(|_| enc.new_label()).collect();
    match block_labels.get(func.entry.index()) {
        Some(entry) if func.entry != BlockId(0) => enc.b(*entry),
        Some(_) => {}
        None => return Err(LowerError::ControlFlowUnsupported),
    }

    for (index, block) in func.blocks.iter().enumerate() {
        enc.bind_label(block_labels[index]);

        for (inst_pos, (result, inst)) in block.insts.iter().enumerate() {
            if let Some(&cil) = source_map.get(index).and_then(|b| b.get(inst_pos)) {
                line_table.push((enc.position(), cil));
            }
            if let Inst::Call { callee, args } = inst {
                lower_call(enc, &assign, *result, *callee, args, func_labels, relocate)?;
                stack_maps.push(StackMapEntry {
                    return_pc: enc.safepoint_label(),
                    frame_size: 0,
                    saved_bytes,
                    ref_offsets: Vec::new(),
                    tagged_offsets: Vec::new(),
                });
            } else {
                lower_inst(enc, &mut pool, *result, inst, &assign)?;
            }
        }

        if let Some(&cil) = source_map.get(index).and_then(|b| b.last()) {
            line_table.push((enc.position(), cil));
        }
        match &block.terminator {
            Some(Terminator::Return(value)) => {
                if let Some(v) = value {
                    if assign(*v) != Reg::R0 {
                        enc.mov_reg(Reg::R0, assign(*v));
                    }
                }
                if has_calls {
                    enc.pop_registers(saved, true);
                } else if saved != 0 {
                    enc.pop_registers(saved, false);
                    enc.bx(Reg::LR);
                } else {
                    enc.bx(Reg::LR);
                }
            }
            Some(Terminator::Jump { target, args }) => {
                let params = &func
                    .block(*target)
                    .ok_or(LowerError::ControlFlowUnsupported)?
                    .params;
                if args.len() != params.len() {
                    return Err(LowerError::ControlFlowUnsupported);
                }
                let moves: Vec<(Reg, Reg)> = params
                    .iter()
                    .zip(args)
                    .map(|(p, a)| (assign(*p), assign(*a)))
                    .collect();
                emit_parallel_move(enc, &moves);
                let label = *block_labels
                    .get(target.index())
                    .ok_or(LowerError::ControlFlowUnsupported)?;
                enc.b(label);
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
                let true_label = *block_labels
                    .get(if_true.index())
                    .ok_or(LowerError::ControlFlowUnsupported)?;
                let false_label = *block_labels
                    .get(if_false.index())
                    .ok_or(LowerError::ControlFlowUnsupported)?;
                enc.cmp_imm(assign(*cond), 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, true_label);
                enc.b(false_label);
            }
            Some(Terminator::Unreachable) => {
                enc.udf(0);
            }
            None => {
                return Err(LowerError::ControlFlowUnsupported);
            }
        }
    }

    if !pool.is_empty() {
        enc.align_to_word();
        for (entry, value) in pool {
            enc.pool_word(entry, value);
        }
    }
    Ok(())
}

/// Lowers a function whose values do not all fit in registers into a shared encoder,
/// as a register/spill mix. Register-homed values stay in their register; a spilled
/// value lives in the stack frame and is loaded into a scratch register (r0/r1) around
/// each instruction that uses it, then stored back if it is the result. Control flow,
/// calls, and loop back-edges reuse the same per-instruction emitter as the all-register
/// path; only operand loads and result stores are added. `func_labels` resolves calls.
#[allow(clippy::too_many_arguments)]
fn lower_mixed_into(
    func: &Function,
    enc: &mut Encoder,
    homes: &[Home],
    saved: u8,
    frame: u16,
    func_labels: &[Label],
    source_map: &[Vec<u32>],
    line_table: &mut Vec<(u32, u32)>,
    stack_maps: &mut Vec<StackMapEntry>,
    relocate: bool,
) -> Result<(), LowerError> {
    let saved_bytes: u16 = (saved.count_ones() as u16 + 1) * 4;
    let has_calls = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| matches!(i, Inst::Call { .. })));
    let home = |v: ValueId| homes.get(v.index()).copied().unwrap_or(Home::Reg(Reg::R0));

    if has_calls || saved != 0 {
        enc.push_registers(saved, has_calls);
    }
    if frame > 0 {
        enc.sub_sp_far(frame).map_err(|_| LowerError::TooManyValues)?;
    }

    let mut pool: Vec<(Label, u32)> = Vec::new();

    let entry_block = func
        .blocks
        .get(func.entry.index())
        .ok_or(LowerError::ControlFlowUnsupported)?;
    let param_moves: Vec<(Home, Home)> = entry_block
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| (home(*p), Home::Reg(Reg::new(i as u8).unwrap_or(Reg::R0))))
        .collect();
    emit_home_moves(enc, &param_moves, Reg::R0)?;

    let block_labels: Vec<Label> = (0..func.blocks.len()).map(|_| enc.new_label()).collect();
    match block_labels.get(func.entry.index()) {
        Some(entry) if func.entry != BlockId(0) => enc.b(*entry),
        Some(_) => {}
        None => return Err(LowerError::ControlFlowUnsupported),
    }

    for (index, block) in func.blocks.iter().enumerate() {
        enc.bind_label(block_labels[index]);

        for (inst_pos, (result, inst)) in block.insts.iter().enumerate() {
            if let Some(&cil) = source_map.get(index).and_then(|b| b.get(inst_pos)) {
                line_table.push((enc.position(), cil));
            }
            if let Inst::Call { callee, args } = inst {
                lower_mixed_call(enc, &home, *result, *callee, args, func_labels, relocate)?;
                stack_maps.push(StackMapEntry {
                    return_pc: enc.safepoint_label(),
                    frame_size: frame,
                    saved_bytes,
                    ref_offsets: Vec::new(),
                    tagged_offsets: Vec::new(),
                });
            } else {
                lower_mixed_value(enc, &mut pool, &home, *result, inst)?;
            }
        }

        if let Some(&cil) = source_map.get(index).and_then(|b| b.last()) {
            line_table.push((enc.position(), cil));
        }
        match &block.terminator {
            Some(Terminator::Return(value)) => {
                if let Some(v) = value {
                    load_home(enc, home(*v), Reg::R0)?;
                }
                if frame > 0 {
                    enc.add_sp_far(frame).map_err(|_| LowerError::TooManyValues)?;
                }
                if has_calls {
                    enc.pop_registers(saved, true);
                } else if saved != 0 {
                    enc.pop_registers(saved, false);
                    enc.bx(Reg::LR);
                } else {
                    enc.bx(Reg::LR);
                }
            }
            Some(Terminator::Jump { target, args }) => {
                let params = &func
                    .block(*target)
                    .ok_or(LowerError::ControlFlowUnsupported)?
                    .params;
                if args.len() != params.len() {
                    return Err(LowerError::ControlFlowUnsupported);
                }
                let moves: Vec<(Home, Home)> = params
                    .iter()
                    .zip(args)
                    .map(|(p, a)| (home(*p), home(*a)))
                    .collect();
                emit_home_moves(enc, &moves, Reg::R0)?;
                let label = *block_labels
                    .get(target.index())
                    .ok_or(LowerError::ControlFlowUnsupported)?;
                enc.b(label);
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
                let true_label = *block_labels
                    .get(if_true.index())
                    .ok_or(LowerError::ControlFlowUnsupported)?;
                let false_label = *block_labels
                    .get(if_false.index())
                    .ok_or(LowerError::ControlFlowUnsupported)?;
                let c = read_to_scratch(enc, home(*cond), Reg::R0)?;
                enc.cmp_imm(c, 0).map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, true_label);
                enc.b(false_label);
            }
            Some(Terminator::Unreachable) => {
                enc.udf(0);
            }
            None => return Err(LowerError::ControlFlowUnsupported),
        }
    }

    if !pool.is_empty() {
        enc.align_to_word();
        for (entry, value) in pool {
            enc.pool_word(entry, value);
        }
    }
    Ok(())
}

/// Lowers one value-defining instruction of the mixed path: load each spilled operand
/// into a scratch register (r0 then r1 -- at most two distinct spilled operands appear),
/// emit through the shared [`lower_inst`], and store the result back if it is spilled.
/// A spilled result is computed in r0; the per-instruction emitter tolerates a result
/// register that reuses an operand's (the operand is consumed in the same instruction).
fn lower_mixed_value(
    enc: &mut Encoder,
    pool: &mut Vec<(Label, u32)>,
    home: &impl Fn(ValueId) -> Home,
    result: ValueId,
    inst: &Inst,
) -> Result<(), LowerError> {
    const SCRATCH: [Reg; 2] = [Reg::R0, Reg::R1];
    let mut uses: Vec<ValueId> = Vec::new();
    crate::regalloc::each_inst_use(inst, |v| {
        if !uses.contains(&v) {
            uses.push(v);
        }
    });
    let mut resolved: Vec<(ValueId, Reg)> = Vec::with_capacity(uses.len());
    let mut next_scratch = 0usize;
    for v in uses {
        let reg = match home(v) {
            Home::Reg(r) => r,
            Home::Spill(off) => {
                let s = *SCRATCH.get(next_scratch).ok_or(LowerError::TooManyValues)?;
                next_scratch += 1;
                enc.ldr_sp(s, off).map_err(|_| LowerError::TooManyValues)?;
                s
            }
        };
        resolved.push((v, reg));
    }
    let result_reg = match home(result) {
        Home::Reg(r) => r,
        Home::Spill(_) => Reg::R0,
    };
    let assign = |v: ValueId| -> Reg {
        if v == result {
            result_reg
        } else {
            resolved
                .iter()
                .find(|(u, _)| *u == v)
                .map(|(_, r)| *r)
                .unwrap_or(Reg::R0)
        }
    };
    lower_inst(enc, pool, result, inst, &assign)?;
    if !matches!(inst, Inst::Store { .. }) {
        if let Home::Spill(off) = home(result) {
            enc.str_sp(result_reg, off)
                .map_err(|_| LowerError::TooManyValues)?;
        }
    }
    Ok(())
}

/// Lowers a `Call` in the mixed path: move each argument into its AAPCS register (r0-r3),
/// `BL`, then move the result from r0 to its home. No value is live across the call on this
/// path (such a function is fully spilled), so clobbering the argument registers is safe.
fn lower_mixed_call(
    enc: &mut Encoder,
    home: &impl Fn(ValueId) -> Home,
    result: ValueId,
    callee: u32,
    args: &[ValueId],
    func_labels: &[Label],
    relocate: bool,
) -> Result<(), LowerError> {
    if args.len() > 4 {
        return Err(LowerError::CallUnsupported);
    }
    let moves: Vec<(Home, Home)> = args
        .iter()
        .enumerate()
        .map(|(i, a)| (Home::Reg(Reg::new(i as u8).unwrap_or(Reg::R0)), home(*a)))
        .collect();
    emit_home_moves(enc, &moves, Reg::R0)?;
    if relocate {
        enc.bl_symbol(callee);
    } else {
        let target = *func_labels
            .get(callee as usize)
            .ok_or(LowerError::CallUnsupported)?;
        enc.bl(target);
    }
    match home(result) {
        Home::Reg(r) => {
            if r != Reg::R0 {
                enc.mov_reg(r, Reg::R0);
            }
        }
        Home::Spill(off) => {
            enc.str_sp(Reg::R0, off)
                .map_err(|_| LowerError::TooManyValues)?;
        }
    }
    Ok(())
}

/// Reads a value into a register: a register-homed value is already there; a spilled one
/// is loaded into `scratch`. Returns the register the value is in.
fn read_to_scratch(enc: &mut Encoder, home: Home, scratch: Reg) -> Result<Reg, LowerError> {
    match home {
        Home::Reg(r) => Ok(r),
        Home::Spill(off) => {
            enc.ldr_sp(scratch, off)
                .map_err(|_| LowerError::TooManyValues)?;
            Ok(scratch)
        }
    }
}

/// Moves a value into `dst` -- a register move (skipped if already there) or a load.
fn load_home(enc: &mut Encoder, home: Home, dst: Reg) -> Result<(), LowerError> {
    match home {
        Home::Reg(r) => {
            if r != dst {
                enc.mov_reg(dst, r);
            }
        }
        Home::Spill(off) => {
            enc.ldr_sp(dst, off)
                .map_err(|_| LowerError::TooManyValues)?;
        }
    }
    Ok(())
}

/// Emits a set of moves between value homes so they take effect as if simultaneous, the
/// general form of [`emit_parallel_move`] over registers and spill slots. Distinct values
/// have distinct slots, so the only cross-move register hazards are register-to-register
/// (handled by the cycle-safe register move); the phases below order the rest so every
/// source is read in its original location. `mem_scratch` (r0/r1, never a value home)
/// shuttles a slot-to-slot move.
fn emit_home_moves(
    enc: &mut Encoder,
    moves: &[(Home, Home)],
    mem_scratch: Reg,
) -> Result<(), LowerError> {
    let oops = |_| LowerError::TooManyValues;
    let active: Vec<(Home, Home)> = moves
        .iter()
        .copied()
        .filter(|(d, s)| !same_home(*d, *s))
        .collect();
    for &(d, s) in &active {
        if let (Home::Spill(off), Home::Reg(r)) = (d, s) {
            enc.str_sp(r, off).map_err(oops)?;
        }
    }
    let reg_moves: Vec<(Reg, Reg)> = active
        .iter()
        .filter_map(|&(d, s)| match (d, s) {
            (Home::Reg(d), Home::Reg(s)) => Some((d, s)),
            _ => None,
        })
        .collect();
    emit_parallel_move(enc, &reg_moves);
    for &(d, s) in &active {
        if let (Home::Reg(r), Home::Spill(off)) = (d, s) {
            enc.ldr_sp(r, off).map_err(oops)?;
        }
    }
    for &(d, s) in &active {
        if let (Home::Spill(doff), Home::Spill(soff)) = (d, s) {
            enc.ldr_sp(mem_scratch, soff).map_err(oops)?;
            enc.str_sp(mem_scratch, doff).map_err(oops)?;
        }
    }
    Ok(())
}

/// Whether two homes are the same place (so a move between them is a no-op).
fn same_home(a: Home, b: Home) -> bool {
    match (a, b) {
        (Home::Reg(x), Home::Reg(y)) => x == y,
        (Home::Spill(x), Home::Spill(y)) => x == y,
        _ => false,
    }
}

/// Maps native code offsets to CIL byte offsets, ascending by offset, so a
/// debugger can take a native PC and recover the CIL instruction being executed. Built
/// by [`lower_debug`] from a `cil::CilSourceMap`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineTable(pub Vec<(u32, u32)>);

/// Per-method debug info from [`lower_module_debug`]: for each method, its function's image offset
/// paired with its [`LineTable`], so a native PC maps to a method, then a CIL offset, then source.
pub type MethodLineTables = Vec<(u32, LineTable)>;

pub use crate::resolver::{TypeMeta, VtableEntry};

/// One descriptor as the `TypeDescLiteral` scan finds it, borrowed from the lowered functions:
/// `(handle, words, vtable, element)` -- the element being the ELEMENT type's handle for an array
/// descriptor and `None` for a class's.
type ChosenDesc<'a> = (u32, &'a [u32], &'a [u32], Option<u32>);

/// One descriptor to LAY: the same four facts owned, with the itable joined in from the resolver's
/// [`TypeMeta`]. The RISC-V twin carries these as the named fields of its `DescEmit`.
type DescLayout = (u32, Vec<u32>, Vec<u32>, Vec<(u32, u32)>, Option<u32>);
use crate::resolver::{ARRAY_DESC_MARK, ARRAY_DESC_MARK_MASK};

impl LineTable {
    /// The CIL byte offset whose native code contains `offset` -- the last entry at or
    /// before it, or `None` if `offset` precedes all code.
    pub fn cil_offset_at(&self, offset: u32) -> Option<u32> {
        self.0
            .iter()
            .rev()
            .find(|&&(start, _)| start <= offset)
            .map(|&(_, cil)| cil)
    }
}

/// One GC safepoint's stack map: the roots live in the frame when a call or allocation returns,
/// for a relocating collector to find and update. `return_pc` is the native code offset of the
/// instruction after the call (add the method's load address for the device PC). `ref_offsets`
/// are byte offsets from SP-at-the-call of UNCONDITIONAL `ObjectRef` roots; `tagged_offsets` are
/// the `PyValue` roots (a tagged word the collector decodes -- traced only when the tag marks a
/// heap pointer). A C#-only image leaves `tagged_offsets` empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackMapEntry {
    /// The return address of the safepoint -- a native code offset, the collector's lookup key.
    pub return_pc: u32,
    /// The sub-SP slot area in bytes -- the roots' container. `ref_offsets`/`tagged_offsets` index it.
    pub frame_size: u16,
    /// The pushed callee-saved registers + LR in bytes, sitting just ABOVE the slots (the prologue
    /// pushes them, then sub-SPs the slots). So the caller's SP is `SP + frame_size + saved_bytes` and
    /// the saved LR (the caller's return address) is the word below it, at `caller_SP - 4`. The
    /// register path has `frame_size == 0` (no slots, just this push); a leaf gets no entry.
    pub saved_bytes: u16,
    /// Byte offsets from SP-at-the-call of the live unconditional `ObjectRef` roots in the frame.
    pub ref_offsets: Vec<u16>,
    /// Byte offsets from SP-at-the-call of the live `PyValue` roots -- tagged words the collector
    /// traces only when the tag marks a heap pointer (the scan-by-tag predicate). Empty for a C#
    /// image.
    pub tagged_offsets: Vec<u16>,
}

impl StackMapEntry {
    /// The entry's wire bytes AFTER its `return_pc` word: `u16 frame_size; u16 saved_bytes;
    /// u16 nrefs; u16 ref_offsets[nrefs]; u16 ntagged; u16 tagged_offsets[ntagged]`.
    ///
    /// Split out so the whole-map encoder and the per-function `.lamella_gcmap` fragment share ONE
    /// definition of an entry's shape. The fragment carries this as an opaque byte run, which is
    /// what keeps `lamella-link` from becoming a second reader of it: the linker rebases the
    /// `return_pc` word it prepends and copies these bytes through untouched.
    fn encode_tail(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.frame_size.to_le_bytes());
        out.extend_from_slice(&self.saved_bytes.to_le_bytes());
        out.extend_from_slice(&(self.ref_offsets.len() as u16).to_le_bytes());
        for &offset in &self.ref_offsets {
            out.extend_from_slice(&offset.to_le_bytes());
        }
        out.extend_from_slice(&(self.tagged_offsets.len() as u16).to_le_bytes());
        for &offset in &self.tagged_offsets {
            out.extend_from_slice(&offset.to_le_bytes());
        }
    }
}

/// The GC stack maps for a lowered program -- one entry per safepoint (every call + allocation, on
/// ALL register-allocation paths so the collector can step past any frame), sorted by `return_pc`
/// for its binary search. A frame walk reads the saved LR at `SP + frame_size + saved_bytes - 4` and
/// the caller's SP at `SP + frame_size + saved_bytes`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackMaps(pub Vec<StackMapEntry>);

impl StackMaps {
    /// The little-endian wire format the collector consumes: `u32 count`, then each entry as
    /// `u32 return_pc` followed by [`StackMapEntry::encode_tail`]. `ref_offsets` are unconditional
    /// `ObjectRef` roots; `tagged_offsets` are `PyValue` roots the collector traces by tag. The
    /// tagged fields are always present -- a C# image emits `ntagged = 0`.
    ///
    /// The OBJECT path no longer calls this. There, `return_pc` is only knowable after the linker
    /// has dead-stripped and laid out the text, so the map is synthesized by `lamella-link` from the
    /// [`lamella_elf::STACKMAP_GCMAP_SECTION`] fragments -- same bytes, same symbol, built where the
    /// addresses are true. This stays the encoder for the FLAT path, which has no linker and whose
    /// offsets are final at lowering time.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.0.len() as u32).to_le_bytes());
        for entry in &self.0 {
            out.extend_from_slice(&entry.return_pc.to_le_bytes());
            entry.encode_tail(&mut out);
        }
        out
    }
}

/// The `.lamella_gcmap` carried section for `data`, or nothing when the object contributes no
/// fragments. `flags: 0` is load-bearing: NOT `SHF_ALLOC`, so the fragments are an input the linker
/// consumes and never bytes the target flashes.
fn gcmap_carried_section(data: &[u8]) -> Vec<lamella_elf::Section<'_>> {
    (!data.is_empty())
        .then(|| lamella_elf::Section {
            name: lamella_elf::STACKMAP_GCMAP_SECTION,
            flags: 0,
            addralign: 4,
            data,
            relocations: &[],
        })
        .into_iter()
        .collect()
}

/// Appends one function's `.lamella_gcmap` fragment (the layout is on
/// [`lamella_elf::STACKMAP_GCMAP_SECTION`]): the owning function's symbol NAME, then each safepoint
/// as its return address RELATIVE TO `func_offset` plus the entry's opaque tail.
///
/// The name is what ties the fragment to its function without a relocation, and it is why the linker
/// can drop a fragment whose function did not survive: a dead function leaves no symbol to resolve.
fn encode_gcmap_fragment(out: &mut Vec<u8>, name: &str, func_offset: u32, entries: &[StackMapEntry]) {
    out.extend_from_slice(&(name.len() as u32).to_le_bytes());
    out.extend_from_slice(name.as_bytes());
    while out.len() % 4 != 0 {
        out.push(0);
    }
    out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    let mut tail = Vec::new();
    for entry in entries {
        tail.clear();
        entry.encode_tail(&mut tail);
        out.extend_from_slice(&(entry.return_pc - func_offset).to_le_bytes());
        out.extend_from_slice(&(tail.len() as u32).to_le_bytes());
        out.extend_from_slice(&tail);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
}

pub use crate::stackmaps::{
    AssemblyStatics, STACKMAP_KIND_MANAGED_PTR, STACKMAP_KIND_OBJECT_REF, STACKMAP_KIND_PINNED,
    STACKMAP_KIND_TAGGED, STACKMAP_MODE_METHOD_SLOTS, STACKMAP_MODE_STATICS,
};
use crate::stackmaps::{encode_stackmap_record, pinned_values};

/// The METHOD_SLOTS root list for one lowered function: on the fully-spilled path, EVERY ref-typed
/// value's slot (each value has its own slot there, so enumeration is complete by construction);
/// on the Registers/Mixed paths, EMPTY -- `prepare` forces the Spilled path whenever any value is
/// live across a safepoint (`any_value_live_across_call`), so a Registers/Mixed frame provably
/// holds no live reference at any PC a stack walk can observe it at. The record still matters for
/// those frames: its `frame_words`/`ret_lr_word` carry the walk PAST them.
fn method_record_roots(func: &Function, externs: &[alloc::string::String]) -> Vec<u16> {
    if !matches!(prepare(func), Ok(Assignment::Spilled)) {
        return Vec::new();
    }
    let (offsets, _) = spilled_slot_offsets(func);
    let pinned = pinned_values(func, externs);
    let mut roots = Vec::new();
    for (v, ty) in func.value_types.iter().enumerate() {
        for (offset, kind) in crate::stackmaps::slot_roots(*ty, pinned[v]) {
            roots.push(((u32::from(offsets[v]) + offset) / 4) | (u32::from(kind) << 14));
        }
    }
    roots.into_iter().map(|r| r as u16).collect()
}

/// Lowers a single function to ARM32 machine code. A function that calls another
/// must go through [`lower_module`], which resolves the call targets.
pub fn lower(func: &Function) -> Result<Vec<u8>, LowerError> {
    let mut enc = Encoder::new();
    let mut _lines = Vec::new();
    match prepare(func)? {
        Assignment::Registers { regs, saved } => lower_into(
            func,
            &mut enc,
            &regs,
            saved,
            &[],
            &[],
            &mut _lines,
            &mut Vec::new(),
            false,
        )?,
        Assignment::Mixed {
            homes,
            saved,
            frame,
        } => lower_mixed_into(
            func,
            &mut enc,
            &homes,
            saved,
            frame,
            &[],
            &[],
            &mut _lines,
            &mut Vec::new(),
            false,
        )?,
        Assignment::Spilled => lower_spilled_into(
            func,
            &mut enc,
            &[],
            None,
            PySupport::default(),
            &[],
            &mut _lines,
            &mut Vec::new(),
            &[],
            false,
            None,
            None,
            None,
        )?,
    }
    enc.finish()
        .map(|assembled| assembled.bytes)
        .map_err(reach_failure)
}

/// Lowers a function and also returns a [`LineTable`] mapping native code offsets to the
/// CIL byte offsets in `source_map` (from `cil::lower_method_debug`), so a native
/// PC recovers to a CIL position.
pub fn lower_debug(
    func: &Function,
    source_map: &[Vec<u32>],
) -> Result<(Vec<u8>, LineTable), LowerError> {
    let mut enc = Encoder::new();
    let mut lines = Vec::new();
    match prepare(func)? {
        Assignment::Registers { regs, saved } => lower_into(
            func,
            &mut enc,
            &regs,
            saved,
            &[],
            source_map,
            &mut lines,
            &mut Vec::new(),
            false,
        )?,
        Assignment::Mixed {
            homes,
            saved,
            frame,
        } => lower_mixed_into(
            func,
            &mut enc,
            &homes,
            saved,
            frame,
            &[],
            source_map,
            &mut lines,
            &mut Vec::new(),
            false,
        )?,
        Assignment::Spilled => lower_spilled_into(
            func,
            &mut enc,
            &[],
            None,
            PySupport::default(),
            source_map,
            &mut lines,
            &mut Vec::new(),
            &[],
            false,
            None,
            None,
            None,
        )?,
    }
    let bytes = enc
        .finish()
        .map(|assembled| assembled.bytes)
        .map_err(reach_failure)?;
    Ok((bytes, LineTable(lines)))
}

/// Lowers a whole program -- several functions concatenated into one image, the
/// direct calls between them resolved. `Call { callee }` names function index
/// `callee` in `funcs`.
pub fn lower_module(funcs: &[Function]) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(funcs, None, PySupport::default(), &[], &[]).map(|(bytes, _, _)| bytes)
}

/// Lowers a whole program whose reference-type allocations call the garbage-collected
/// runtime allocator at absolute address `alloc_addr` -- `lamella_gc_alloc(payload_size,
/// &TypeDesc) -> payload*`, AAPCS (size in r0, descriptor in r1, result in r0). Each `Alloc`
/// lowers to `blx` that address with a null-check; the type descriptors are emitted per type.
pub fn lower_module_gc(funcs: &[Function], alloc_addr: u32) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(funcs, Some(alloc_addr), PySupport::default(), &[], &[])
        .map(|(bytes, _, _)| bytes)
}

/// As [`lower_module_gc`], but with per-type VTABLES (`(type handle, function indices in slot order)`)
/// emitted before each TypeDesc, so `callvirt` dispatches through `obj-4`'s descriptor. The resolver
/// produces the table via `MetadataResolver::vtables`.
pub fn lower_module_gc_vtables(
    funcs: &[Function],
    alloc_addr: u32,
    vtables: &[TypeMeta],
) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(funcs, Some(alloc_addr), PySupport::default(), vtables, &[])
        .map(|(bytes, _, _)| bytes)
}

/// As [`lower_module_gc`], but also returns the GC [`StackMaps`] -- one entry per safepoint
/// (every call and allocation), naming the live `ObjectRef` roots for a relocating collector.
pub fn lower_module_gc_mapped(
    funcs: &[Function],
    alloc_addr: u32,
) -> Result<(Vec<u8>, StackMaps), LowerError> {
    lower_module_inner(funcs, Some(alloc_addr), PySupport::default(), &[], &[])
        .map(|(bytes, maps, _)| (bytes, maps))
}

/// Addresses of the Python runtime-support entry points a `PyIntrinsic` calls -- stand-ins for the
/// linker-resolved `py_*` symbols. Each is `None` until that op is wired, so emitting an un-wired op
/// errors ([`LowerError::CallUnsupported`]); only the wired ops are present.
#[derive(Debug, Clone, Copy, Default)]
pub struct PySupport {
    /// `py_getattr(receiver, name_id, cache_slot) -> PyValue` (r0, r1, r2 -> r0).
    pub getattr: Option<u32>,
    /// `py_len(x) -> PyValue` (r0 -> r0).
    pub len: Option<u32>,
    /// `py_call(callee, argv: *const PyValue, argc) -> PyValue` (r0, r1, r2 -> r0); the backend
    /// spills the positional `PyValue` args to a stack array and passes its pointer + count.
    pub call: Option<u32>,
}

/// Lowers a whole program with the Python runtime-support entries threaded: `alloc_addr` is the
/// `lamella_gc_alloc` address (or `None`), `py_support` the per-op entry addresses a `PyIntrinsic`
/// calls. Returns the image plus the GC [`StackMaps`] (carrying tagged `PyValue` roots).
pub fn lower_module_py(
    funcs: &[Function],
    alloc_addr: Option<u32>,
    py_support: PySupport,
) -> Result<(Vec<u8>, StackMaps), LowerError> {
    lower_module_inner(funcs, alloc_addr, py_support, &[], &[])
        .map(|(bytes, maps, _)| (bytes, maps))
}

/// The high bit of a relocation's symbol index flags it as an EXTERN symbol (a `CallNative` target --
/// `__aeabi_*`, a P/Invoke entry, a `py_*` helper) rather than an intra-module function index. The
/// backend ORs it in at the call site; `lower_object` decodes it to the extern symbol's ELF index.
const EXTERN_SYMBOL_FLAG: u32 = 0x8000_0000;
/// A backend symbol whose low bits are a TYPE HANDLE, not a function index -- an object-path reference to
/// the type's canonical descriptor symbol (`__lamella_typedesc_<handle>`). `lower_object_inner` resolves it
/// to the descriptor's symbol-table index. Bit 30, distinct from `EXTERN_SYMBOL_FLAG`; type tokens are well
/// under 2^30, so the handle never collides with the flag.
const DESC_SYMBOL_FLAG: u32 = 0x4000_0000;
/// A backend symbol whose low bits are a deduplicated STRING-LITERAL blob id (not a function index) -- a
/// program-object reference to the `__lamella_str_<id>` blob symbol `emit_object_pass` lays after the
/// descriptors. Bit 29, distinct from `EXTERN_SYMBOL_FLAG`/`DESC_SYMBOL_FLAG`; a blob id is a small
/// first-appearance index, well under 2^29, so it never collides with the flag. Loading a literal through
/// this ISLANDABLE pool word (rather than a direct `adr`) lets a literal in a >1020-byte function relax via
/// `finish`'s literal islanding instead of hard-erroring. Program objects only -- a library's blob symbols
/// would collide across a link (the assembly-unique-prefix fix descriptors also await is deferred).
const STRING_SYMBOL_FLAG: u32 = 0x2000_0000;
/// A backend symbol standing for a STATIC-REGION base (`__lamella_statics_<asmhash>`, see
/// `lamella_elf::STATICS_BASE_PREFIX`) -- an object-path `ldsfld`/`stsfld` pool word references
/// one with addend = the field's dense slot offset, and `lamella-link` places every referenced
/// region in RAM and defines the symbols. The low bits say WHOSE region: 0 = this assembly's own;
/// k+1 = the region of reference ordinal k (a cross-assembly static, resolved to the OWNER's
/// hash-qualified symbol via [`DescQualifiers::references`] -- the owner's own numbering, so both
/// sides address the same slots). Bit 28, distinct from the flags above; the payload stays under
/// bit 24, so the top-byte test in the relocation loop cannot alias a DESC value's TypeSpec bits.
const STATICS_BASE_SYMBOL_FLAG: u32 = 0x1000_0000;
/// A backend symbol standing for the ONE VES-global in-flight exception word
/// (`lamella_elf::EH_TAG_SYMBOL`). Every assembly's throw/catch lowering references the SAME symbol
/// -- the split that keeps a corlib `throw` visible to a program `catch` (a per-region row 0 would
/// silently break EH across assemblies). No payload bits. Bit 27, distinct from the flags above.
const EH_TAG_SYMBOL_FLAG: u32 = 0x0800_0000;
/// The name prefix of a deduplicated string-literal blob symbol. Backend-only (unlike `TYPE_DESC_PREFIX`
/// the linker never special-cases it: `trim_object` keeps a reached non-descriptor data symbol already).
const STR_BLOB_PREFIX: &str = "__lamella_str_";

pub use crate::resolver::DescQualifiers;
use crate::resolver::descriptor_symbol;

/// The mode an object build runs in -- program vs library, strict vs deferring, plus the
/// descriptor-identity qualifiers -- bundled so the emit pipeline's signatures stay flat.
pub struct ObjectBuildMode<'a> {
    /// Emit the `lamella_main` entry symbol (a program); `false` = a library object.
    pub emit_entry: bool,
    /// A program that DEFERS un-lowerable/un-encodable bodies to traps instead of failing.
    pub defer_encode: bool,
    /// The descriptor-identity qualifiers (see [`DescQualifiers`]).
    pub qualifiers: &'a DescQualifiers,
    /// Relax a far reference to its wide Thumb-2 form (`B.W`/`ADR.W`) rather than hard-erroring at
    /// the ARMv6-M reach -- set for a Mainline (M33) target object build (see
    /// [`Encoder::set_wide_thumb2`]). `false` (an ARMv6-M target) keeps every path byte-identical,
    /// since the widen fires only for an out-of-reach ref.
    pub wide: bool,
}

/// The `__aeabi_*` soft-float helper for a float arithmetic `Binary` op, keyed by the operand type;
/// `None` for an integer op (a different type) or a non-arithmetic op. ARM AAPCS soft-float passes
/// f32 in a core register and f64 in a register pair, which falls out of the C-ABI arg lowering.
fn aeabi_float_helper(op: BinOp, operand_ty: Option<MirType>) -> Option<&'static str> {
    match operand_ty {
        Some(MirType::F32) => Some(match op {
            BinOp::Add => "__aeabi_fadd",
            BinOp::Sub => "__aeabi_fsub",
            BinOp::Mul => "__aeabi_fmul",
            BinOp::DivSigned | BinOp::DivUnsigned => "__aeabi_fdiv",
            _ => return None,
        }),
        Some(MirType::F64) => Some(match op {
            BinOp::Add => "__aeabi_dadd",
            BinOp::Sub => "__aeabi_dsub",
            BinOp::Mul => "__aeabi_dmul",
            BinOp::DivSigned | BinOp::DivUnsigned => "__aeabi_ddiv",
            _ => return None,
        }),
        _ => None,
    }
}

/// The `__aeabi_*` soft-float helper for a float CONVERSION the no-FPU target does not do inline;
/// `None` for one lowered inline (`emit_f2i`) or an integer widen. A helper's single f64 argument
/// arrives in `r0:r1`, which the C-ABI arg lowering already forms for a `CallNative`.
///
/// `IntToFloat32` IS in this table even though [`emit_i2f`] can lower it inline, and that is the
/// point: the inline loop is exact only below 2^24 and TRUNCATES above it, so `(float)int.MaxValue`
/// came out 2147483520 where .NET rounds to nearest and answers 2147483648. The archive's
/// `__aeabi_i2f` rounds correctly. The flat path tests this kind BEFORE consulting this table, so it
/// keeps the inline form -- it has no linker and cannot call a helper at all.
fn aeabi_convert_helper(kind: ConvKind) -> Option<&'static str> {
    match kind {
        ConvKind::IntToFloat32 => Some("__aeabi_i2f"),
        ConvKind::Float64ToInt => Some("__aeabi_d2iz"),
        ConvKind::IntToFloat64 => Some("__aeabi_i2d"),
        ConvKind::LongToFloat64 => Some("__aeabi_l2d"),
        ConvKind::Float32ToFloat64 => Some("__aeabi_f2d"),
        ConvKind::Float64ToFloat32 => Some("__aeabi_d2f"),
        ConvKind::LongToFloat32 => Some("__aeabi_l2f"),
        ConvKind::UIntToFloat64 => Some("__aeabi_ui2d"),
        ConvKind::ULongToFloat64 => Some("__aeabi_ul2d"),
        _ => None,
    }
}

/// Interns `name` into the module's extern-symbol table, returning its index.
fn intern_extern(externs: &mut Vec<alloc::string::String>, name: &str) -> u32 {
    if let Some(i) = externs.iter().position(|s| s == name) {
        i as u32
    } else {
        externs.push(name.into());
        (externs.len() - 1) as u32
    }
}

/// A dispatch target as the function-index-or-extern-marker word a descriptor slot relocates
/// against: a this-module implementation is its function index, a referenced-assembly one is
/// `EXTERN_SYMBOL_FLAG | <interned extern>`. The extern must ALREADY be interned -- vtable slots
/// intern during `lower_runtime_calls`, itable entries in `lower_object_inner` -- because the
/// object pass that resolves them only reads the table.
fn encoded_impl(impl_: &crate::resolver::VtableEntry, externs: &[alloc::string::String]) -> u32 {
    match impl_ {
        crate::resolver::VtableEntry::Func(index) => *index,
        crate::resolver::VtableEntry::Extern(symbol) => {
            let index = externs
                .iter()
                .position(|s| s == symbol)
                .expect("a descriptor's extern implementation is interned before the object pass");
            EXTERN_SYMBOL_FLAG | index as u32
        }
    }
}

/// A type's vtable as the function-index-or-extern-marker words the object path lays before its
/// descriptor: a this-module slot is its function index, an inherited referenced-assembly slot is
/// `EXTERN_SYMBOL_FLAG | <interned extern>` (carried through `TypeDescLiteral` to the vtable relocation,
/// like a `CallNative` target). Empty for a type with no descriptor entry.
fn descriptor_vtable(
    meta: Option<&TypeMeta>,
    externs: &mut Vec<alloc::string::String>,
) -> Box<[u32]> {
    meta.map(|m| {
        m.vtable
            .iter()
            .map(|entry| match entry {
                crate::resolver::VtableEntry::Func(index) => *index,
                crate::resolver::VtableEntry::Extern(symbol) => {
                    EXTERN_SYMBOL_FLAG | intern_extern(externs, symbol)
                }
            })
            .collect()
    })
    .unwrap_or_default()
}

/// The `__aeabi_*cmp*` soft-float comparison helper for a float `Compare`, with whether its result
/// must be INVERTED, keyed by the operand type; `None` for an integer compare. The EABI helpers are
/// ORDERED (0 for NaN): `fcmplt/le/gt/ge/eq`. The CLI's unordered compares (`clt.un` etc.) and `!=`
/// are the negation of an ordered helper -- e.g. `clt.un` (a<b or unordered) = `!(a>=b)`, so
/// `UnsignedLt` is `fcmpge` inverted. f32 -> `__aeabi_fcmp*`, f64 -> `__aeabi_dcmp*`.
fn aeabi_float_compare(
    op: CmpOp,
    operand_ty: Option<MirType>,
) -> Option<(alloc::string::String, bool)> {
    let prefix = match operand_ty {
        Some(MirType::F32) => "__aeabi_fcmp",
        Some(MirType::F64) => "__aeabi_dcmp",
        _ => return None,
    };
    let (suffix, invert) = match op {
        CmpOp::Eq => ("eq", false),
        CmpOp::Ne => ("eq", true),
        CmpOp::SignedLt => ("lt", false),
        CmpOp::SignedLe => ("le", false),
        CmpOp::SignedGt => ("gt", false),
        CmpOp::SignedGe => ("ge", false),
        CmpOp::UnsignedLt => ("ge", true),
        CmpOp::UnsignedLe => ("gt", true),
        CmpOp::UnsignedGt => ("le", true),
        CmpOp::UnsignedGe => ("lt", true),
    };
    Some((alloc::format!("{prefix}{suffix}"), invert))
}

/// Rewrites a rectangular-array allocation (`AllocArray2D`/`AllocArrayMD`) for the OBJECT path into a
/// linker-resolved `lamella_gc_alloc` call, appending the resulting instructions to `insts` (fresh
/// `value_types` entries for each temporary). It allocates `4*N + product(dims)*element_size` bytes (the
/// `[dim0]..[dim(N-1)]` row-major header before the elements), tags the object with a minimal `[0,0,tag,0]`
/// descriptor (an array dispatches no virtuals through it), and writes each dimension length into the
/// header. The 1-D `AllocArray` has its own rewrite above; the monolithic path lowers all of these inline
/// against a fixed allocator address; the RISC-V object path does the same via `emit_alloc_call`.
#[allow(clippy::too_many_arguments)]
fn rewrite_md_alloc(
    value_types: &mut Vec<MirType>,
    insts: &mut Vec<(ValueId, Inst)>,
    externs: &mut Vec<alloc::string::String>,
    descriptors: &[TypeMeta],
    result: ValueId,
    handle: lamella_ir::TypeHandle,
    dims: &[ValueId],
    element_size: u32,
) {
    let symbol = intern_extern(externs, "lamella_gc_alloc");
    let type_tag = descriptors
        .iter()
        .find(|m| m.handle == handle)
        .map_or(0, |m| m.type_tag);
    let fresh = |value_types: &mut Vec<MirType>| {
        let v = ValueId(value_types.len() as u32);
        value_types.push(MirType::I32);
        v
    };
    let mut acc = dims[0];
    for &d in &dims[1..] {
        let p = fresh(value_types);
        insts.push((
            p,
            Inst::Binary {
                op: BinOp::Mul,
                lhs: acc,
                rhs: d,
            },
        ));
        acc = p;
    }
    let esize = fresh(value_types);
    insts.push((
        esize,
        Inst::ConstInt {
            ty: MirType::I32,
            value: i64::from(element_size),
        },
    ));
    let scaled = fresh(value_types);
    insts.push((
        scaled,
        Inst::Binary {
            op: BinOp::Mul,
            lhs: acc,
            rhs: esize,
        },
    ));
    let header = fresh(value_types);
    insts.push((
        header,
        Inst::ConstInt {
            ty: MirType::I32,
            value: 4 * dims.len() as i64,
        },
    ));
    let size = fresh(value_types);
    insts.push((
        size,
        Inst::Binary {
            op: BinOp::Add,
            lhs: scaled,
            rhs: header,
        },
    ));
    let typedesc = fresh(value_types);
    insts.push((
        typedesc,
        Inst::TypeDescLiteral {
            handle: handle.0,
            words: alloc::vec![0, 0, type_tag, 0].into_boxed_slice(),
            vtable: alloc::vec![].into_boxed_slice(),
            element: None,
        },
    ));
    insts.push((
        result,
        Inst::CallNative {
            symbol,
            args: alloc::vec![size, typedesc],
        },
    ));
    for (k, &d) in dims.iter().enumerate() {
        let st = fresh(value_types);
        insts.push((
            st,
            Inst::FieldStore {
                base: result,
                offset: (4 * k) as u32,
                value: d,
            },
        ));
    }
}

/// Rewrites the ops the object path resolves through the linker into `CallNative`s (interning each
/// extern name): soft-float `+ - * /` and the comparisons to `__aeabi_*` helpers (against `libgcc.a`),
/// and a heap allocation to `lamella_gc_alloc`. Float stays a target-independent typed MIR op; a
/// hard-float (VFP) target would lower it inline instead -- a later knob. A comparison whose CLI form
/// is a negation (the unordered compares, `!=`) expands to the ordered helper plus a logical-not
/// (`== 0`), which is why the instruction list is rebuilt rather than edited in place.
fn lower_runtime_calls(
    func: &Function,
    externs: &mut Vec<alloc::string::String>,
    descriptors: &[TypeMeta],
) -> Function {
    let mut func = func.clone();
    for bi in 0..func.blocks.len() {
        let old = core::mem::take(&mut func.blocks[bi].insts);
        let mut insts = Vec::with_capacity(old.len());
        for (result, inst) in old {
            if let Inst::PInvoke { import, args } = &inst {
                let symbol = intern_extern(externs, import);
                insts.push((
                    result,
                    Inst::CallNative {
                        symbol,
                        args: args.clone(),
                    },
                ));
                continue;
            }
            if let Inst::Alloc {
                handle,
                payload_size,
                ref_offsets,
            } = &inst
            {
                let symbol = intern_extern(externs, "lamella_gc_alloc");
                let meta = descriptors.iter().find(|m| m.handle == *handle);
                let type_tag = meta.map_or(0, |m| m.type_tag);
                let vtable = descriptor_vtable(meta, externs);
                let mut words: Vec<u32> =
                    alloc::vec![*payload_size, ref_offsets.len() as u32, type_tag, 0];
                words.extend(ref_offsets.iter().copied());
                let size = ValueId(func.value_types.len() as u32);
                func.value_types.push(MirType::I32);
                let typedesc = ValueId(func.value_types.len() as u32);
                func.value_types.push(MirType::I32);
                insts.push((
                    size,
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: i64::from(*payload_size),
                    },
                ));
                insts.push((
                    typedesc,
                    Inst::TypeDescLiteral {
                        handle: handle.0,
                        words: words.into_boxed_slice(),
                        vtable,
                        element: None,
                    },
                ));
                insts.push((
                    result,
                    Inst::CallNative {
                        symbol,
                        args: alloc::vec![size, typedesc],
                    },
                ));
                continue;
            }
            if let Inst::AllocLike {
                proto,
                payload_size,
            } = &inst
            {
                let symbol = intern_extern(externs, "lamella_gc_alloc");
                let size = ValueId(func.value_types.len() as u32);
                func.value_types.push(MirType::I32);
                let typedesc = ValueId(func.value_types.len() as u32);
                func.value_types.push(MirType::I32);
                insts.push((
                    size,
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: i64::from(*payload_size),
                    },
                ));
                insts.push((typedesc, Inst::LoadTypeDesc { object: *proto }));
                insts.push((
                    result,
                    Inst::CallNative {
                        symbol,
                        args: alloc::vec![size, typedesc],
                    },
                ));
                continue;
            }
            if let Inst::AllocDescribed {
                descriptor,
                payload_size,
            } = &inst
            {
                let symbol = intern_extern(externs, "lamella_gc_alloc");
                insts.push((
                    result,
                    Inst::CallNative {
                        symbol,
                        args: alloc::vec![*payload_size, *descriptor],
                    },
                ));
                continue;
            }
            if let Inst::AllocArray {
                handle,
                element,
                length,
                element_size,
                element_kind,
            } = &inst
            {
                let symbol = intern_extern(externs, "lamella_gc_alloc");
                let type_tag = element
                    .and_then(|e| descriptors.iter().find(|m| m.handle == e))
                    .map_or(0, |m| m.type_tag);
                let esize = ValueId(func.value_types.len() as u32);
                func.value_types.push(MirType::I32);
                let scaled = ValueId(func.value_types.len() as u32);
                func.value_types.push(MirType::I32);
                let four = ValueId(func.value_types.len() as u32);
                func.value_types.push(MirType::I32);
                let size = ValueId(func.value_types.len() as u32);
                func.value_types.push(MirType::I32);
                let typedesc = ValueId(func.value_types.len() as u32);
                func.value_types.push(MirType::I32);
                insts.push((
                    esize,
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: i64::from(*element_size),
                    },
                ));
                insts.push((
                    scaled,
                    Inst::Binary {
                        op: BinOp::Mul,
                        lhs: *length,
                        rhs: esize,
                    },
                ));
                insts.push((
                    four,
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: 4,
                    },
                ));
                insts.push((
                    size,
                    Inst::Binary {
                        op: BinOp::Add,
                        lhs: scaled,
                        rhs: four,
                    },
                ));
                insts.push((
                    typedesc,
                    Inst::TypeDescLiteral {
                        handle: handle.0,
                        words: alloc::vec![ARRAY_DESC_MARK | 1, *element_kind, type_tag, 0, 0]
                            .into_boxed_slice(),
                        vtable: descriptor_vtable(
                            descriptors.iter().find(|m| m.handle == *handle),
                            externs,
                        ),
                        element: element.map(|e| e.0),
                    },
                ));
                insts.push((
                    result,
                    Inst::CallNative {
                        symbol,
                        args: alloc::vec![size, typedesc],
                    },
                ));
                let lenstore = ValueId(func.value_types.len() as u32);
                func.value_types.push(MirType::I32);
                insts.push((
                    lenstore,
                    Inst::FieldStore {
                        base: result,
                        offset: 0,
                        value: *length,
                    },
                ));
                continue;
            }
            if let Inst::AllocArray2D {
                handle,
                dim0,
                dim1,
                element_size,
            } = &inst
            {
                rewrite_md_alloc(
                    &mut func.value_types,
                    &mut insts,
                    externs,
                    descriptors,
                    result,
                    *handle,
                    &[*dim0, *dim1],
                    *element_size,
                );
                continue;
            }
            if let Inst::AllocArrayMD {
                handle,
                dims,
                element_size,
            } = &inst
            {
                rewrite_md_alloc(
                    &mut func.value_types,
                    &mut insts,
                    externs,
                    descriptors,
                    result,
                    *handle,
                    dims,
                    *element_size,
                );
                continue;
            }
            if let Inst::TypeDescAddr { handle } = &inst {
                let meta = descriptors.iter().find(|m| m.handle == *handle);
                let type_tag = meta.map_or(0, |m| m.type_tag);
                let vtable = descriptor_vtable(meta, externs);
                insts.push((
                    result,
                    Inst::TypeDescLiteral {
                        handle: handle.0,
                        words: meta
                            .and_then(|m| m.words.clone())
                            .unwrap_or_else(|| alloc::vec![0, 0, type_tag, 0].into_boxed_slice()),
                        vtable,
                        element: None,
                    },
                ));
                continue;
            }
            if let Inst::Convert { value, kind } = &inst {
                if let Some(name) = aeabi_convert_helper(*kind) {
                    let symbol = intern_extern(externs, name);
                    insts.push((
                        result,
                        Inst::CallNative {
                            symbol,
                            args: alloc::vec![*value],
                        },
                    ));
                    continue;
                }
            }
            let plan = match &inst {
                Inst::Binary { op, lhs, rhs } => {
                    aeabi_float_helper(*op, func.value_types.get(lhs.index()).copied())
                        .map(|name| (intern_extern(externs, name), *lhs, *rhs, false))
                }
                Inst::Compare { op, lhs, rhs } => {
                    aeabi_float_compare(*op, func.value_types.get(lhs.index()).copied())
                        .map(|(name, invert)| (intern_extern(externs, &name), *lhs, *rhs, invert))
                }
                _ => None,
            };
            match plan {
                None => insts.push((result, inst)),
                Some((symbol, lhs, rhs, false)) => insts.push((
                    result,
                    Inst::CallNative {
                        symbol,
                        args: alloc::vec![lhs, rhs],
                    },
                )),
                Some((symbol, lhs, rhs, true)) => {
                    let tmp = ValueId(func.value_types.len() as u32);
                    func.value_types.push(MirType::I32);
                    let zero = ValueId(func.value_types.len() as u32);
                    func.value_types.push(MirType::I32);
                    insts.push((
                        tmp,
                        Inst::CallNative {
                            symbol,
                            args: alloc::vec![lhs, rhs],
                        },
                    ));
                    insts.push((
                        zero,
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 0,
                        },
                    ));
                    insts.push((
                        result,
                        Inst::Compare {
                            op: CmpOp::Eq,
                            lhs: tmp,
                            rhs: zero,
                        },
                    ));
                }
            }
        }
        func.blocks[bi].insts = insts;
    }
    func
}

/// Lowers a module into an ELF32 relocatable object -- the ARM/Thumb twin of
/// `riscv32::lower_object`. Each function becomes a global `STT_FUNC` symbol (named by `names[i]`,
/// the Thumb bit set in its value as an ARM toolchain marks a Thumb function) at its entry offset,
/// and every direct call becomes an `R_ARM_THM_CALL` relocation to the callee's symbol (its function
/// index), so a linker resolves them and sees the call graph. `names` must have one entry per
/// function. `extern_syms` names the module's external symbols (`CallNative` targets) -- undefined
/// globals the linker resolves (e.g. from `libgcc.a`); a `CallNative { symbol: i }` references
/// `extern_syms[i]`.
///
/// The register, mixed, and spilled paths all emit objects (so a function with a value live across a
/// call, or many values, qualifies). A function that needs a runtime address it has no symbol for
/// (an `Alloc` without an allocator, a `PyIntrinsic`), or whose branches relax (which would shift the
/// pre-finish symbol offsets), is rejected rather than mis-emitted -- the GC/dynamic objects await
/// emitting those helper references as relocations too.
pub fn lower_object(
    funcs: &[Function],
    names: &[&str],
    extern_syms: &[&str],
) -> Result<Vec<u8>, LowerError> {
    let mode =
        ObjectBuildMode { emit_entry: true, defer_encode: false, qualifiers: &DescQualifiers::default(), wide: false };
    lower_object_inner(funcs, names, extern_syms, &[], None, &mode, None).map(|(bytes, ..)| bytes)
}

/// As [`lower_object`], but also emitting per-type VTABLES/TypeDescs from `descriptors` (the resolver's
/// [`type_descriptors`](crate::resolver::MetadataResolver::type_descriptors)), so `callvirt`/`ldvirtftn`
/// dispatch through the object's descriptor on the linked device path (not only the flat GC path). Each
/// allocated type's descriptor gains its vtable, laid before it as `R_LAMELLA_REL_DESC` slots the linker
/// resolves -- so dispatch works AND the references survive `--gc-sections` re-layout.
pub fn lower_object_vtables(
    funcs: &[Function],
    names: &[&str],
    extern_syms: &[&str],
    descriptors: &[TypeMeta],
) -> Result<Vec<u8>, LowerError> {
    let mode =
        ObjectBuildMode { emit_entry: true, defer_encode: false, qualifiers: &DescQualifiers::default(), wide: false };
    lower_object_inner(funcs, names, extern_syms, descriptors, None, &mode, None).map(|(bytes, ..)| bytes)
}

/// As [`lower_object_vtables`], but also emitting the assembly's GLOBAL-roots statics record
/// (mode 2) into the object's `.lamella_stackmaps` records AND resolving the object's statics
/// accesses against its region symbol -- the build path passes the dense ref-bearing rows it read
/// from metadata, so the collector's root walk covers statics wherever the linker places them.
pub fn lower_object_vtables_statics(
    funcs: &[Function],
    names: &[&str],
    extern_syms: &[&str],
    descriptors: &[TypeMeta],
    statics: &AssemblyStatics,
    qualifiers: &DescQualifiers,
) -> Result<Vec<u8>, LowerError> {
    let mode = ObjectBuildMode { emit_entry: true, defer_encode: false, qualifiers, wide: false };
    lower_object_inner(funcs, names, extern_syms, descriptors, Some(statics), &mode, None)
        .map(|(bytes, ..)| bytes)
}

/// As [`lower_object_vtables_statics`], but threading `source_maps` (method `i`'s
/// [`CilSourceMap`](crate::cil::CilSourceMap)) and returning, per method, `(its code offset within
/// the object, its LineTable)`.
///
/// This is what a LINKED DEVICE image needs to carry DWARF: the flat path has produced line tables
/// for a long time, but the object path discarded them, so debug info stopped at the linker-less
/// build. The offsets are resolved AFTER `finish`, so Thumb-2 relaxation cannot leave them naming
/// the wrong bytes.
///
/// `debug` also carries the resolved source positions the object's `.debug_*` sections are built
/// from, so the returned object is directly linkable into a DEBUGGABLE device image. Passing a
/// `debug` whose `source_maps` and `methods` are empty produces byte-identical output to
/// [`lower_object_vtables_statics`] -- the rows are write-only with respect to the emitted code.
#[allow(clippy::too_many_arguments)]
pub fn lower_object_vtables_statics_debug(
    funcs: &[Function],
    names: &[&str],
    extern_syms: &[&str],
    descriptors: &[TypeMeta],
    statics: &AssemblyStatics,
    qualifiers: &DescQualifiers,
    debug: &crate::debugmap::ObjectDebug,
) -> Result<(Vec<u8>, MethodLineTables), LowerError> {
    let mode = ObjectBuildMode { emit_entry: true, defer_encode: false, qualifiers, wide: false };
    lower_object_inner(
        funcs,
        names,
        extern_syms,
        descriptors,
        Some(statics),
        &mode,
        Some(debug),
    )
    .map(|(bytes, _, lines)| (bytes, lines))
}

/// As [`lower_object_vtables_statics`], but DEFERRING instead of failing: a program method whose
/// body does not lower or whose inclusion overflows an object-scale encoding reach is emitted as
/// a `udf #0` trap (NEVER the library path's truthy `bx lr` -- a reached deferred method must
/// fail LOUD at its exact call site, and an unreached one is `--gc-sections` fodder), and the
/// stub report says which and why. The single-assembly device-demo bake (app + BSP +
/// System.Device sources in one program assembly) is the customer: library-grade surface rides
/// along that the program never calls.
pub fn lower_object_vtables_statics_report(
    funcs: &[Function],
    names: &[&str],
    extern_syms: &[&str],
    descriptors: &[TypeMeta],
    statics: &AssemblyStatics,
    qualifiers: &DescQualifiers,
    wide: bool,
) -> Result<(Vec<u8>, LibraryStubReport), LowerError> {
    let mode = ObjectBuildMode { emit_entry: true, defer_encode: true, qualifiers, wide };
    lower_object_inner(funcs, names, extern_syms, descriptors, Some(statics), &mode, None)
        .map(|(bytes, report, _)| (bytes, report))
}

/// As [`lower_object`], but for a LIBRARY object with no entry point: it omits the `lamella_main` entry
/// symbol, so several library objects (a corlib, helpers) link alongside one program without a
/// duplicate-symbol clash on the entry.
pub fn lower_object_library(
    funcs: &[Function],
    names: &[&str],
    extern_syms: &[&str],
) -> Result<Vec<u8>, LowerError> {
    let mode =
        ObjectBuildMode { emit_entry: false, defer_encode: false, qualifiers: &DescQualifiers::default(), wide: false };
    lower_object_inner(funcs, names, extern_syms, &[], None, &mode, None).map(|(bytes, ..)| bytes)
}

/// As [`lower_object_library_vtables`], but also returning the STUB REPORT: every function the
/// library build emitted as a bare `bx lr` instead of a body, as `(function index, why)` -- a
/// per-method dry-run lowering failure carries its [`LowerError`]; a fixpoint stub (the method
/// lowered but the whole object could not encode with it in) carries [`LowerError::CodeTooLarge`].
/// The object bytes are IDENTICAL to [`lower_object_library_vtables`]'s -- the report is
/// observation, not behavior. This is the (4b) tooling seam: a `bx lr` stub silently returns its
/// first argument, so knowing the stub set is what separates a stubbed method from a vtable slot
/// bug when a call misbehaves.
pub fn lower_object_library_vtables_report(
    funcs: &[Function],
    names: &[&str],
    extern_syms: &[&str],
    descriptors: &[TypeMeta],
    statics: Option<&AssemblyStatics>,
    qualifiers: &DescQualifiers,
    wide: bool,
) -> Result<(Vec<u8>, LibraryStubReport), LowerError> {
    let mode = ObjectBuildMode { emit_entry: false, defer_encode: false, qualifiers, wide };
    lower_object_inner(funcs, names, extern_syms, descriptors, statics, &mode, None)
        .map(|(bytes, report, _)| (bytes, report))
}

/// As [`lower_object_library`], but emitting per-type vtables/TypeDescs from `descriptors` -- so a corlib
/// (or helper) library object's allocating/virtual methods dispatch correctly once linked. The library
/// twin of [`lower_object_vtables`].
pub fn lower_object_library_vtables(
    funcs: &[Function],
    names: &[&str],
    extern_syms: &[&str],
    descriptors: &[TypeMeta],
) -> Result<Vec<u8>, LowerError> {
    let mode =
        ObjectBuildMode { emit_entry: false, defer_encode: false, qualifiers: &DescQualifiers::default(), wide: false };
    lower_object_inner(funcs, names, extern_syms, descriptors, None, &mode, None).map(|(bytes, ..)| bytes)
}

/// Verifies, prepares, and emits ONE function into `enc`. Extracted so a library object can DRY-RUN a
/// method into a scratch encoder (tolerating one that does not lower) before emitting it for real.
/// Lowers ONE function into an object-path encoder. `source_map` is the method's
/// [`CilSourceMap`](crate::cil::CilSourceMap) rows (empty for a build with no debug info) and
/// `lines` collects its native-offset -> CIL-offset pairs -- the same pair the flat path threads, so
/// an object can carry the line table DWARF is generated from. Both are write-only with respect to
/// the emitted code: passing an empty `source_map` leaves the emitted bytes unchanged.
#[allow(clippy::too_many_arguments)]
fn lower_one_func(
    func: &Function,
    enc: &mut Encoder,
    func_labels: &[Label],
    stack_maps: &mut Vec<StackMapEntry>,
    blob_table: Option<&[Box<[u16]>]>,
    console_symbol: Option<u32>,
    source_map: &[Vec<u32>],
    lines: &mut Vec<(u32, u32)>,
    string_header: Option<(u32, i32)>,
) -> Result<(), LowerError> {
    if lamella_ir::verify(func).is_err() {
        return Err(LowerError::NotWellFormed);
    }
    match prepare(func)? {
        Assignment::Registers { regs, saved } => lower_into(
            func,
            enc,
            &regs,
            saved,
            func_labels,
            source_map,
            lines,
            stack_maps,
            true,
        ),
        Assignment::Mixed {
            homes,
            saved,
            frame,
        } => lower_mixed_into(
            func,
            enc,
            &homes,
            saved,
            frame,
            func_labels,
            source_map,
            lines,
            stack_maps,
            true,
        ),
        Assignment::Spilled => lower_spilled_into(
            func,
            enc,
            func_labels,
            None,
            PySupport::default(),
            source_map,
            lines,
            stack_maps,
            &[],
            true,
            blob_table,
            console_symbol,
            string_header,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn lower_object_inner(
    funcs: &[Function],
    names: &[&str],
    extern_syms: &[&str],
    descriptors: &[TypeMeta],
    statics_record: Option<&AssemblyStatics>,
    mode: &ObjectBuildMode,
    debug: Option<&crate::debugmap::ObjectDebug>,
) -> Result<(Vec<u8>, LibraryStubReport, MethodLineTables), LowerError> {
    let mut externs: Vec<alloc::string::String> = extern_syms.iter().map(|s| (*s).into()).collect();
    let mut program = funcs.to_vec();
    crate::stringgen::lower_string_concat(&mut program, mode.qualifiers.string);
    crate::stringgen::lower_int_to_string(&mut program, mode.qualifiers.string);
    crate::stringgen::lower_write_int(&mut program);
    let owned_names: Vec<alloc::string::String> = names
        .iter()
        .map(|s| alloc::string::String::from(*s))
        .chain((names.len()..program.len()).map(|i| alloc::format!("__lamella_strgen_{i}")))
        .collect();
    let names: Vec<&str> = owned_names.iter().map(|s| s.as_str()).collect();
    let names = names.as_slice();
    let funcs: Vec<Function> = program
        .iter()
        .map(|f| lower_runtime_calls(f, &mut externs, descriptors))
        .collect();
    let funcs = funcs.as_slice();
    if funcs
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.insts)
        .any(|(_, i)| matches!(i, Inst::SemihostWrite { .. }))
    {
        intern_extern(&mut externs, crate::stringgen::CONSOLE_WRITE_BYTES);
    }
    for meta in descriptors {
        for (_, impl_) in &meta.itable {
            if let crate::resolver::VtableEntry::Extern(symbol) = impl_ {
                intern_extern(&mut externs, symbol);
            }
        }
    }
    let element_handles: Vec<u32> = funcs
        .iter()
        .flat_map(|f| &f.blocks)
        .flat_map(|b| &b.insts)
        .filter_map(|(_, inst)| match inst {
            Inst::TypeDescLiteral {
                element: Some(element),
                ..
            } => Some(*element),
            _ => None,
        })
        .collect();
    for meta in descriptors
        .iter()
        .filter(|meta| element_handles.contains(&meta.handle.0))
    {
        for entry in &meta.vtable {
            if let crate::resolver::VtableEntry::Extern(symbol) = entry {
                intern_extern(&mut externs, symbol);
            }
        }
    }
    if !mode.emit_entry {
        for meta in descriptors {
            for entry in &meta.vtable {
                if let crate::resolver::VtableEntry::Extern(symbol) = entry {
                    intern_extern(&mut externs, symbol);
                }
            }
        }
    }
    let mut stubbed: alloc::collections::BTreeSet<usize> = alloc::collections::BTreeSet::new();
    loop {
        match emit_object_pass(
            funcs,
            names,
            &externs,
            descriptors,
            statics_record,
            &stubbed,
            mode,
            debug,
        )? {
            PassOutcome::Object(bytes, mut stub_report, line_tables) => {
                stub_report.extend(stubbed.iter().map(|&i| (i, LowerError::CodeTooLarge { site: None })));
                stub_report.sort_by_key(|&(i, _)| i);
                return Ok((bytes, stub_report, line_tables));
            }
            PassOutcome::StubAndRetry(index) => {
                stubbed.insert(index);
            }
        }
    }
}
/// The descriptors this object will LAY, in emission order -- decided BEFORE any function is
/// lowered, because the question a string literal's object header has to answer ("is
/// `System.String`'s descriptor a LOCAL copy in this object, or a cross-assembly reference?") is
/// exactly membership in this set, and the per-function blob path emits its header WHILE lowering.
/// Deciding it once here leaves ONE source for that answer; a second derivation of the same
/// predicate is how the addend rule below comes to disagree with itself.
///
/// Pure computation -- it reads `funcs`, the resolver's `descriptors` and the already-interned
/// `externs`, and touches no encoder -- so hoisting it above the lowering loop moves no bytes.
fn descriptor_emit_set(
    funcs: &[Function],
    descriptors: &[TypeMeta],
    externs: &[alloc::string::String],
    emit_entry: bool,
) -> Vec<DescLayout> {
    let mut chosen: Vec<ChosenDesc> = Vec::new();
    for func in funcs {
        for block in &func.blocks {
            for (_, inst) in &block.insts {
                if let Inst::TypeDescLiteral {
                    handle,
                    words,
                    vtable,
                    element,
                } = inst
                {
                    let is_class = |w: &[u32]| {
                        w.first()
                            .is_none_or(|f| f & ARRAY_DESC_MARK_MASK != ARRAY_DESC_MARK)
                    };
                    let rank = |w: &[u32], v: &[u32]| {
                        (is_class(w), w.len(), w.first().copied().unwrap_or(0), v.len())
                    };
                    match chosen.iter_mut().find(|(h, ..)| h == handle) {
                        Some(slot) => {
                            if rank(words, vtable) > rank(slot.1, slot.2) {
                                slot.1 = words;
                                slot.2 = vtable;
                                slot.3 = *element;
                            }
                        }
                        None => chosen.push((*handle, words, vtable, *element)),
                    }
                }
            }
        }
    }
    let itable_of = |handle: u32| -> Vec<(u32, u32)> {
        descriptors
            .iter()
            .find(|m| m.handle.0 == handle)
            .map(|m| {
                m.itable
                    .iter()
                    .map(|(tag, impl_)| (*tag, encoded_impl(impl_, externs)))
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut emit: Vec<DescLayout> = chosen
        .iter()
        .map(|(h, w, v, e)| (*h, w.to_vec(), v.to_vec(), itable_of(*h), *e))
        .collect();
    if !emit_entry {
        for meta in descriptors {
            let Some(words) = meta.words.as_deref().filter(|_| meta.exported) else {
                continue;
            };
            if emit.iter().any(|(h, ..)| *h == meta.handle.0) {
                continue;
            }
            let vtable: Vec<u32> = meta.vtable.iter().map(|e| encoded_impl(e, externs)).collect();
            let itable: Vec<(u32, u32)> = meta
                .itable
                .iter()
                .map(|(tag, impl_)| (*tag, encoded_impl(impl_, externs)))
                .collect();
            emit.push((meta.handle.0, words.to_vec(), vtable, itable, None));
        }
    }
    let mut i = 0;
    while i < emit.len() {
        let handle = emit[i].0;
        if let Some(element) = emit[i].4 {
            let elements_are_references =
                emit[i].1.get(1) == Some(&crate::resolver::ELEMENT_KIND_REFERENCE);
            let cross_assembly =
                crate::resolver::reference_handle_parts(lamella_ir::TypeHandle(element))
                    .is_some();
            let laid = |emit: &[DescLayout]| {
                emit.iter().find(|(h, ..)| *h == element).map(|e| e.1.clone())
            };
            if laid(&emit).is_none() && !cross_assembly {
                let meta = descriptors.iter().find(|m| m.handle.0 == element);
                match meta.and_then(|m| m.words.as_deref()) {
                    Some(words) => {
                        let vtable = meta
                            .map(|m| m.vtable.iter().map(|e| encoded_impl(e, externs)).collect())
                            .unwrap_or_default();
                        emit.push((element, words.to_vec(), vtable, itable_of(element), None));
                    }
                    None if elements_are_references => {
                        let tag = meta.map_or(0, |m| m.type_tag);
                        emit.push((
                            element,
                            alloc::vec![0, 0, tag, 0],
                            Vec::new(),
                            Vec::new(),
                            None,
                        ));
                    }
                    None => emit[i].4 = None,
                }
            }
            if !elements_are_references
                && laid(&emit).is_some_and(|w| w.first().copied().unwrap_or(0) == 0)
            {
                emit[i].4 = None;
            }
        }
        if let Some(base) = descriptors
            .iter()
            .find(|m| m.handle.0 == handle)
            .and_then(|m| m.base)
        {
            if crate::resolver::reference_handle_parts(base).is_none()
                && !emit.iter().any(|(h, ..)| *h == base.0)
            {
                let tag = descriptors
                    .iter()
                    .find(|m| m.handle == base)
                    .map_or(0, |m| m.type_tag);
                emit.push((base.0, alloc::vec![0, 0, tag, 0], Vec::new(), Vec::new(), None));
            }
        }
        i += 1;
    }
    emit
}


/// One outcome of [`emit_object_pass`]: either the finished object bytes plus the pass's stub
/// report -- each `(function index, why)` the pass emitted as a bare `bx lr` because its body
/// failed the per-method dry run -- or, for a library, the index of a method that lowered but made
/// the whole object exceed a Thumb-1 encoding reach, so the driver stubs it and rebuilds.
enum PassOutcome {
    Object(Vec<u8>, LibraryStubReport, MethodLineTables),
    StubAndRetry(usize),
}

/// A library build's stub report: each function the build emitted as a bare `bx lr` instead of a
/// body, as `(function index, the lowering error that forced it)`.
pub type LibraryStubReport = Vec<(usize, LowerError)>;

/// Emits ONE object from the already-lowered `funcs`: every function, then the canonical per-type
/// descriptors, then `finish`. `stubbed` names functions to emit as a bare `bx lr` (a prior pass found
/// the object could not encode with them in). Returns [`PassOutcome::StubAndRetry`] if a LIBRARY object
/// still fails to encode -- mapping the overflow site back to its method; a program's encode failure is
/// fatal. Driven to a fixpoint by [`lower_object_inner`].
#[allow(clippy::too_many_arguments)]
fn emit_object_pass(
    funcs: &[Function],
    names: &[&str],
    externs: &[alloc::string::String],
    descriptors: &[TypeMeta],
    statics_record: Option<&AssemblyStatics>,
    stubbed: &alloc::collections::BTreeSet<usize>,
    mode: &ObjectBuildMode,
    debug: Option<&crate::debugmap::ObjectDebug>,
) -> Result<PassOutcome, LowerError> {
    let (emit_entry, defer_encode, qualifiers) = (mode.emit_entry, mode.defer_encode, mode.qualifiers);
    let mut enc = Encoder::new();
    enc.set_wide_thumb2(mode.wide);
    let console_symbol = externs
        .iter()
        .position(|s| s == crate::stringgen::CONSOLE_WRITE_BYTES)
        .map(|i| i as u32);
    let func_labels: Vec<Label> = funcs.iter().map(|_| enc.new_label()).collect();
    let mut stack_maps: Vec<StackMapEntry> = Vec::new();
    let string_table: Vec<Box<[u16]>> = if emit_entry {
        let mut table: Vec<Box<[u16]>> = Vec::new();
        for func in funcs {
            for block in &func.blocks {
                for (_, inst) in &block.insts {
                    if let Inst::StringLiteral { utf16 } = inst {
                        if !table.iter().any(|b| b.as_ref() == utf16.as_ref()) {
                            table.push(utf16.clone());
                        }
                    }
                }
            }
        }
        table
    } else {
        Vec::new()
    };
    let blob_table: Option<&[Box<[u16]>]> = emit_entry.then_some(string_table.as_slice());
    let emit = descriptor_emit_set(funcs, descriptors, externs, emit_entry);
    let string_header: Option<(u32, i32)> = mode.qualifiers.string.map(|handle| {
        let vtable_bytes = if emit.iter().any(|(h, ..)| *h == handle) {
            0
        } else {
            descriptors
                .iter()
                .find(|m| m.handle.0 == handle)
                .map_or(0, |m| m.vtable.len() as i32 * 4)
        };
        (handle, vtable_bytes)
    });
    let type_names = !cfg!(feature = "strip-type-names");
    let mut map_ranges: Vec<(usize, usize)> = Vec::with_capacity(funcs.len());
    let mut stub_report: LibraryStubReport = Vec::new();
    let mut method_lines: Vec<LineTable> = Vec::with_capacity(funcs.len());
    let mut func_starts: Vec<u32> = Vec::with_capacity(funcs.len());
    for (index, func) in funcs.iter().enumerate() {
        enc.align_to_word();
        enc.bind_label(func_labels[index]);
        let map_start = stack_maps.len();
        func_starts.push(enc.position());
        let source_map = debug
            .and_then(|d| d.source_maps.get(index))
            .map(|m| m.0.as_slice())
            .unwrap_or(&[]);
        let mut lines: Vec<(u32, u32)> = Vec::new();
        if emit_entry && !defer_encode {
            lower_one_func(
                func,
                &mut enc,
                &func_labels,
                &mut stack_maps,
                blob_table,
                console_symbol,
                source_map,
                &mut lines,
                string_header,
            )?;
        } else if stubbed.contains(&index) {
            if emit_entry {
                enc.udf(0);
            } else {
                enc.bx(Reg::LR);
            }
        } else {
            let mut scratch = Encoder::new();
            let scratch_labels: Vec<Label> =
                (0..funcs.len()).map(|_| scratch.new_label()).collect();
            let mut scratch_maps = Vec::new();
            match lower_one_func(
                func,
                &mut scratch,
                &scratch_labels,
                &mut scratch_maps,
                blob_table,
                console_symbol,
                source_map,
                &mut Vec::new(),
                string_header,
            ) {
                Ok(()) => {
                    lower_one_func(
                        func,
                        &mut enc,
                        &func_labels,
                        &mut stack_maps,
                        blob_table,
                        console_symbol,
                        source_map,
                        &mut lines,
                        string_header,
                    )
                    .expect("a method that lowered in the dry run lowers for real");
                }
                Err(error) => {
                    stub_report.push((index, error));
                    if emit_entry {
                        enc.udf(0);
                    } else {
                        enc.bx(Reg::LR);
                    }
                }
            }
        }
        map_ranges.push((map_start, stack_maps.len()));
        method_lines.push(LineTable(lines));
    }
    let code_end_label = enc.new_label();
    enc.bind_label(code_end_label);
    let mut desc_syms: Vec<(u32, Label, u32, u32, u32)> = Vec::new();
    {
        for (handle, words, vtable, itable, element) in &emit {
            enc.align_to_word();
            let vtable_label = enc.new_label();
            enc.bind_label(vtable_label);
            for (k, &func_index) in vtable.iter().enumerate().rev() {
                enc.data_word_symbol_reldesc(func_index, -(4 + 4 * k as i32));
            }
            let words_label = enc.new_label();
            enc.bind_label(words_label);
            let base = descriptors
                .iter()
                .find(|m| m.handle.0 == *handle)
                .and_then(|m| m.base);
            let laid_here = element.is_some_and(|e| emit.iter().any(|(h, ..)| *h == e));
            let element_word = |enc: &mut Encoder, element: u32, laid_here: bool| {
                let vtable_bytes = if laid_here {
                    0
                } else {
                    descriptors
                        .iter()
                        .find(|m| m.handle.0 == element)
                        .map_or(0, |m| m.vtable.len() as i32 * 4)
                };
                enc.data_word_symbol_reldesc(DESC_SYMBOL_FLAG | element, vtable_bytes + 16);
            };
            for (idx, &word) in words.iter().enumerate() {
                if idx == 4 {
                    match element {
                        Some(element) => element_word(&mut enc, *element, laid_here),
                        None => enc.emit_word(word),
                    }
                    continue;
                }
                match base {
                    Some(base_handle)
                        if idx == 3 && emit.iter().any(|(h, ..)| *h == base_handle.0) =>
                    {
                        enc.data_word_symbol_reldesc(DESC_SYMBOL_FLAG | base_handle.0, 12);
                    }
                    Some(base_handle)
                        if idx == 3
                            && crate::resolver::reference_handle_parts(base_handle).is_some() =>
                    {
                        let vtable_bytes = descriptors
                            .iter()
                            .find(|m| m.handle == base_handle)
                            .map_or(0, |m| m.vtable.len() as i32 * 4);
                        enc.data_word_symbol_reldesc(
                            DESC_SYMBOL_FLAG | base_handle.0,
                            vtable_bytes + 12,
                        );
                    }
                    _ => enc.emit_word(word),
                }
            }
            let mut itable_words = 1 + 2 * itable.len() as u32;
            enc.emit_word(itable.len() as u32);
            let words_bytes = words.len() as i32 * 4;
            for (i, &(tag, func_index)) in itable.iter().enumerate() {
                enc.emit_word(tag);
                enc.data_word_symbol_reldesc(func_index, words_bytes + 8 + 8 * i as i32);
            }
            let name: Option<Vec<u16>> = type_names
                .then(|| {
                    descriptors
                        .iter()
                        .find(|m| m.handle.0 == *handle)
                        .and_then(|m| m.full_name.as_deref())
                        .map(|n| n.encode_utf16().collect())
                })
                .flatten();
            match &name {
                Some(units) => {
                    let name_label = enc.new_label();
                    enc.data_word_diff(words_label, name_label);
                    if let Some((string_handle, vtable_bytes)) = string_header {
                        enc.data_word_symbol_addend(DESC_SYMBOL_FLAG | string_handle, vtable_bytes);
                    }
                    enc.bind_label(name_label);
                    let blob = unencodable(crate::stringgen::string_blob_bytes(units))?;
                    enc.emit_bytes(&blob);
                    let header_words = u32::from(string_header.is_some());
                    itable_words += 1 + header_words + (blob.len() as u32).div_ceil(4);
                    enc.align_to_word();
                }
                None => {
                    enc.emit_word(0);
                    itable_words += 1;
                }
            }
            desc_syms.push((
                *handle,
                vtable_label,
                vtable.len() as u32,
                words.len() as u32,
                itable_words,
            ));
        }
    }
    let mut str_syms: Vec<(Label, u32)> = Vec::with_capacity(string_table.len());
    for utf16 in &string_table {
        enc.align_to_word();
        let label = enc.new_label();
        enc.bind_label(label);
        let start = enc.position();
        if let Some((handle, vtable_bytes)) = string_header {
            enc.data_word_symbol_addend(DESC_SYMBOL_FLAG | handle, vtable_bytes);
        }
        enc.emit_bytes(&unencodable(crate::stringgen::string_blob_bytes(utf16))?);
        str_syms.push((label, enc.position() - start));
    }
    let assembled = if emit_entry && !defer_encode {
        enc.finish().map_err(reach_failure)?
    } else {
        let probe = enc.clone();
        match enc.finish() {
            Ok(assembled) => assembled,
            Err(AssembleError::BranchOutOfRange { at, kind }) => {
                let mut query = func_labels.clone();
                query.push(code_end_label);
                let positions = probe
                    .relaxed_positions(&query)
                    .map_err(reach_failure)?;
                let code_end_pos = positions[funcs.len()].unwrap_or(u32::MAX);
                if at >= code_end_pos {
                    return Err(LowerError::CodeTooLarge { site: Some((at, kind)) });
                }
                let failed = (0..funcs.len())
                    .rev()
                    .find(|&i| positions[i].is_some_and(|start| start <= at));
                return match failed {
                    Some(i) if !stubbed.contains(&i) => Ok(PassOutcome::StubAndRetry(i)),
                    _ => Err(LowerError::CodeTooLarge { site: Some((at, kind)) }),
                };
            }
            Err(e) => return Err(reach_failure(e)),
        }
    };
    let offsets: Vec<u32> = func_labels
        .iter()
        .map(|&l| assembled.label_position(l).unwrap_or(0))
        .collect();
    let code_end = assembled
        .label_position(code_end_label)
        .unwrap_or(assembled.bytes.len() as u32);
    let gcmap_section: Vec<u8> = if emit_entry {
        let mut data = Vec::new();
        for (i, &(start, end)) in map_ranges.iter().enumerate() {
            if start == end {
                continue;
            }
            let resolved: Vec<StackMapEntry> = stack_maps[start..end]
                .iter()
                .map(|entry| StackMapEntry {
                    return_pc: assembled
                        .label_position_by_id(entry.return_pc)
                        .unwrap_or(offsets[i]),
                    ..entry.clone()
                })
                .collect();
            encode_gcmap_fragment(&mut data, names[i], offsets[i], &resolved);
        }
        data
    } else {
        Vec::new()
    };
    let mut method_records: Vec<(usize, u32, u16, u16, Vec<u16>)> = Vec::new();
    for (i, &(start, end)) in map_ranges.iter().enumerate() {
        if start == end {
            continue;
        }
        let frame_size = stack_maps[start].frame_size;
        let saved_bytes = stack_maps[start].saved_bytes;
        debug_assert!(
            stack_maps[start..end]
                .iter()
                .all(|e| e.frame_size == frame_size && e.saved_bytes == saved_bytes),
            "a function's frame constants are fixed by its one prologue"
        );
        let frame_words = (frame_size + saved_bytes) / 4;
        let ret_lr_word = frame_words - 1;
        let end_off = offsets.get(i + 1).copied().unwrap_or(code_end);
        let code_size = end_off - offsets[i];
        let roots = method_record_roots(&funcs[i], externs);
        method_records.push((i, code_size, frame_words, ret_lr_word, roots));
    }
    let smrec_names: Vec<alloc::string::String> = method_records
        .iter()
        .map(|&(i, ..)| alloc::format!("{}{}", lamella_elf::STACKMAP_RECORD_PREFIX, names[i]))
        .collect();
    let smstat_name = statics_record.map(AssemblyStatics::record_symbol);
    let region_name = statics_record.map(AssemblyStatics::region_symbol);
    let mut ref_region_ordinals: Vec<u32> = assembled
        .relocs
        .iter()
        .filter(|r| r.symbol >> 24 == STATICS_BASE_SYMBOL_FLAG >> 24)
        .map(|r| r.symbol & 0x00ff_ffff)
        .filter(|&payload| payload != 0)
        .map(|payload| payload - 1)
        .collect();
    ref_region_ordinals.sort_unstable();
    ref_region_ordinals.dedup();
    let ref_region_names: Vec<alloc::string::String> = ref_region_ordinals
        .iter()
        .map(|&ordinal| {
            let hash = qualifiers
                .references
                .get(ordinal as usize)
                .unwrap_or_else(|| panic!("reference ordinal {ordinal} has no statics qualifier"));
            alloc::format!("{}{}", lamella_elf::STATICS_BASE_PREFIX, hash)
        })
        .collect();
    let desc_positions: Vec<u32> = desc_syms
        .iter()
        .map(|(_, label, ..)| assembled.label_position(*label).unwrap_or(0))
        .collect();
    let str_positions: Vec<u32> = str_syms
        .iter()
        .map(|(label, _)| assembled.label_position(*label).unwrap_or(0))
        .collect();
    let mut text = assembled.bytes;
    let mut symbols: Vec<lamella_elf::Symbol> = (0..funcs.len())
        .map(|i| {
            let end = offsets.get(i + 1).copied().unwrap_or(code_end);
            lamella_elf::Symbol {
                name: names[i],
                value: offsets[i] | 1,
                size: end - offsets[i],
                binding: lamella_elf::Binding::Global,
                kind: lamella_elf::SymbolType::Func,
                section: lamella_elf::SymbolSection::Text,
            }
        })
        .collect();
    for name in externs {
        symbols.push(lamella_elf::Symbol {
            name: name.as_str(),
            value: 0,
            size: 0,
            binding: lamella_elf::Binding::Global,
            kind: lamella_elf::SymbolType::NoType,
            section: lamella_elf::SymbolSection::Undefined,
        });
    }
    let statics_base_index = region_name.as_ref().map(|name| {
        let index = symbols.len() as u32;
        symbols.push(lamella_elf::Symbol {
            name: name.as_str(),
            value: 0,
            size: statics_record.map_or(0, |s| s.region_bytes),
            binding: lamella_elf::Binding::Global,
            kind: lamella_elf::SymbolType::NoType,
            section: lamella_elf::SymbolSection::Undefined,
        });
        index
    });
    let ref_region_index: alloc::collections::BTreeMap<u32, u32> = ref_region_ordinals
        .iter()
        .zip(&ref_region_names)
        .map(|(&ordinal, name)| {
            let index = symbols.len() as u32;
            symbols.push(lamella_elf::Symbol {
                name: name.as_str(),
                value: 0,
                size: 0,
                binding: lamella_elf::Binding::Global,
                kind: lamella_elf::SymbolType::NoType,
                section: lamella_elf::SymbolSection::Undefined,
            });
            (ordinal, index)
        })
        .collect();
    let eh_tag_index = assembled
        .relocs
        .iter()
        .any(|r| r.symbol == EH_TAG_SYMBOL_FLAG)
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
    if emit_entry {
        if let Some(&entry_off) = offsets.first() {
            symbols.push(lamella_elf::Symbol {
                name: "lamella_main",
                value: entry_off | 1,
                size: 0,
                binding: lamella_elf::Binding::Global,
                kind: lamella_elf::SymbolType::Func,
                section: lamella_elf::SymbolSection::Text,
            });
        }
    }
    let desc_names: Vec<alloc::string::String> = desc_syms
        .iter()
        .map(|(h, ..)| descriptor_symbol(*h, qualifiers))
        .collect();
    let mut desc_index: alloc::collections::BTreeMap<u32, (u32, i32)> =
        alloc::collections::BTreeMap::new();
    for (i, (handle, _vtable_label, vtable_len, words_len, itable_words)) in
        desc_syms.iter().enumerate()
    {
        let pos = desc_positions[i];
        desc_index.insert(*handle, (symbols.len() as u32, (*vtable_len as i32) * 4));
        symbols.push(lamella_elf::Symbol {
            name: desc_names[i].as_str(),
            value: pos,
            size: (vtable_len + words_len + itable_words) * 4,
            binding: if emit_entry {
                lamella_elf::Binding::Global
            } else {
                lamella_elf::Binding::Weak
            },
            kind: lamella_elf::SymbolType::NoType,
            section: lamella_elf::SymbolSection::Text,
        });
    }
    let str_names: Vec<alloc::string::String> = (0..str_syms.len())
        .map(|id| alloc::format!("{STR_BLOB_PREFIX}{id}"))
        .collect();
    let str_index: Vec<u32> = (0..str_syms.len())
        .map(|id| symbols.len() as u32 + id as u32)
        .collect();
    for (id, (_, size)) in str_syms.iter().enumerate() {
        symbols.push(lamella_elf::Symbol {
            name: str_names[id].as_str(),
            value: str_positions[id],
            size: *size,
            binding: lamella_elf::Binding::Global,
            kind: lamella_elf::SymbolType::NoType,
            section: lamella_elf::SymbolSection::Text,
        });
    }
    let is_desc_symbol = |sym: u32| {
        sym >> 24 != STATICS_BASE_SYMBOL_FLAG >> 24
            && sym != EH_TAG_SYMBOL_FLAG
            && sym & EXTERN_SYMBOL_FLAG == 0
            && sym & DESC_SYMBOL_FLAG != 0
    };
    let mut undef_desc_handles: Vec<u32> = assembled
        .relocs
        .iter()
        .filter(|r| is_desc_symbol(r.symbol))
        .map(|r| r.symbol & !DESC_SYMBOL_FLAG)
        .filter(|handle| !desc_index.contains_key(handle))
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
    let mut relocations: Vec<lamella_elf::Relocation> = Vec::with_capacity(assembled.relocs.len());
    for r in &assembled.relocs {
        let (kind, addend) = match r.kind {
            lamella_asm_arm32::RelocKind::ThumbCall => (lamella_elf::arm::R_ARM_THM_CALL, -4),
            lamella_asm_arm32::RelocKind::Abs32 => (lamella_elf::arm::R_ARM_ABS32, r.addend),
            lamella_asm_arm32::RelocKind::RelDesc32 => {
                (lamella_elf::arm::R_LAMELLA_REL_DESC, r.addend)
            }
            _ => return Err(LowerError::CallUnsupported),
        };
        let (symbol, final_addend) = if r.symbol >> 24 == STATICS_BASE_SYMBOL_FLAG >> 24 {
            let index = match r.symbol & 0x00ff_ffff {
                0 => statics_base_index.ok_or(LowerError::CallUnsupported)?,
                payload => *ref_region_index
                    .get(&(payload - 1))
                    .expect("the reference-region scan saw this relocation"),
            };
            (index, addend)
        } else if r.symbol == EH_TAG_SYMBOL_FLAG {
            let index = eh_tag_index.expect("the eh-tag scan saw this relocation");
            (index, addend)
        } else if r.symbol & EXTERN_SYMBOL_FLAG != 0 {
            (funcs.len() as u32 + (r.symbol & !EXTERN_SYMBOL_FLAG), addend)
        } else if r.symbol & DESC_SYMBOL_FLAG != 0 {
            let handle = r.symbol & !DESC_SYMBOL_FLAG;
            match desc_index.get(&handle) {
                Some(&(index, vtable_bytes)) => (index, vtable_bytes + addend),
                None => (undef_desc_index[&handle], addend),
            }
        } else if r.symbol & STRING_SYMBOL_FLAG != 0 {
            let header = if mode.qualifiers.string.is_some() { 4 } else { 0 };
            (
                str_index[(r.symbol & !STRING_SYMBOL_FLAG) as usize],
                addend + header,
            )
        } else {
            (r.symbol, addend)
        };
        relocations.push(lamella_elf::Relocation {
            offset: r.at,
            symbol,
            kind,
            addend: final_addend,
        });
    }
    for (rec_index, &(i, code_size, frame_words, ret_lr_word, ref roots)) in
        method_records.iter().enumerate()
    {
        while text.len() % 4 != 0 {
            text.push(0);
        }
        let rec_offset = text.len() as u32;
        encode_stackmap_record(
            &mut text,
            0,
            code_size,
            STACKMAP_MODE_METHOD_SLOTS,
            frame_words,
            ret_lr_word,
            roots,
        );
        symbols.push(lamella_elf::Symbol {
            name: smrec_names[rec_index].as_str(),
            value: rec_offset,
            size: text.len() as u32 - rec_offset,
            binding: lamella_elf::Binding::Weak,
            kind: lamella_elf::SymbolType::NoType,
            section: lamella_elf::SymbolSection::Text,
        });
        relocations.push(lamella_elf::Relocation {
            offset: rec_offset,
            symbol: i as u32,
            kind: lamella_elf::arm::R_ARM_ABS32,
            addend: 0,
        });
    }
    if let Some(statics) = statics_record {
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
            symbol: statics_base_index
                .expect("a statics record appends its region symbol"),
            kind: lamella_elf::arm::R_ARM_ABS32,
            addend: 0,
        });
    }
    let line_tables: MethodLineTables = method_lines
        .iter()
        .enumerate()
        .map(|(index, table)| (offsets.get(index).copied().unwrap_or(0), table.clone()))
        .collect();

    let object = match debug {
        Some(dbg) => {
            let described: Vec<(usize, Vec<crate::debugmap::SourceLine>)> = line_tables
                .iter()
                .enumerate()
                .filter_map(|(index, (_, table))| {
                    let source = dbg.methods.get(index)?;
                    let rows = crate::debugmap::function_rows(
                        &table.0,
                        source,
                        func_starts.get(index).copied().unwrap_or(0),
                    );
                    (!rows.is_empty()).then_some((index, rows))
                })
                .collect();
            let functions: Vec<crate::dwarf::FunctionLines> = described
                .iter()
                .map(|(index, rows)| crate::dwarf::FunctionLines {
                    name: match dbg.methods[*index].name {
                        "" => names[*index],
                        display => display,
                    },
                    file: dbg.methods[*index].file,
                    rows,
                    code_size: symbols[*index].size,
                })
                .collect();
            if functions.is_empty() {
                lamella_elf::write_relocatable_object(
                    lamella_elf::Machine::Arm,
                    &text,
                    &symbols,
                    &relocations,
                )
            } else {
                let first = described[0].0;
                let last = described[described.len() - 1].0;
                let span = Some(offsets[last] + symbols[last].size - offsets[first]);
                let line = crate::dwarf::line_program(&functions);
                let (info, abbrev) =
                    crate::dwarf::compilation_unit(dbg.unit_name, dbg.producer, span, &functions);
                let generated = [line, info, abbrev];
                let first_section_symbol = symbols.len() as u32;
                for i in 0..generated.len() {
                    symbols.push(lamella_elf::Symbol {
                        name: "",
                        value: 0,
                        size: 0,
                        binding: lamella_elf::Binding::Local,
                        kind: lamella_elf::SymbolType::Section,
                        section: lamella_elf::SymbolSection::InSection(i as u32),
                    });
                }
                let debug_relocs: Vec<Vec<lamella_elf::Relocation>> = generated
                    .iter()
                    .map(|section| {
                        let code =
                            section
                                .code_relocs
                                .iter()
                                .map(|(at, function)| lamella_elf::Relocation {
                                    offset: *at,
                                    symbol: described[*function].0 as u32,
                                    kind: lamella_elf::arm::R_ARM_ABS32,
                                    addend: 0,
                                });
                        let cross =
                            section
                                .section_relocs
                                .iter()
                                .map(|(at, target)| lamella_elf::Relocation {
                                    offset: *at,
                                    symbol: first_section_symbol
                                        + generated
                                            .iter()
                                            .position(|s| s.name == *target)
                                            .unwrap_or(0)
                                            as u32,
                                    kind: lamella_elf::arm::R_ARM_ABS32,
                                    addend: 0,
                                });
                        code.chain(cross).collect()
                    })
                    .collect();
                let mut sections: Vec<lamella_elf::Section> = generated
                    .iter()
                    .enumerate()
                    .map(|(i, section)| lamella_elf::Section {
                        name: section.name,
                        flags: 0,
                        addralign: 1,
                        data: &section.data,
                        relocations: &debug_relocs[i],
                    })
                    .collect();
                sections.extend(gcmap_carried_section(&gcmap_section));
                lamella_elf::write_relocatable_object_with_sections(
                    lamella_elf::Machine::Arm,
                    &text,
                    &symbols,
                    &relocations,
                    &sections,
                )
            }
        }
        None => lamella_elf::write_relocatable_object_with_sections(
            lamella_elf::Machine::Arm,
            &text,
            &symbols,
            &relocations,
            &gcmap_carried_section(&gcmap_section),
        ),
    };
    Ok(PassOutcome::Object(object, stub_report, line_tables))
}

/// Lowers a whole multi-method program WITH debug line tables -- the module variant of [`lower_debug`].
/// `source_maps[i]` is method `i`'s `CilSourceMap` (from `resolver::lower_methods_debug`); returns the
/// image bytes plus, per method, `(its function's image offset, its LineTable)` -- a native code offset
/// maps via the table to a CIL byte offset, then via the method's source map to source. `alloc_addr`
/// is `Some` for a program that allocates (the GC path), `None` otherwise. Unlike single-method
/// `cil::lower_method_debug`, cross-method calls resolve, so a real multi-method program is debuggable.
pub fn lower_module_debug(
    funcs: &[Function],
    alloc_addr: Option<u32>,
    source_maps: &[crate::cil::CilSourceMap],
) -> Result<(Vec<u8>, MethodLineTables), LowerError> {
    lower_module_inner(funcs, alloc_addr, PySupport::default(), &[], source_maps)
        .map(|(bytes, _, lines)| (bytes, lines))
}

fn lower_module_inner(
    funcs: &[Function],
    alloc_addr: Option<u32>,
    py_support: PySupport,
    vtables: &[TypeMeta],
    source_maps: &[crate::cil::CilSourceMap],
) -> Result<(Vec<u8>, StackMaps, MethodLineTables), LowerError> {
    let original_count = funcs.len();
    let mut program = funcs.to_vec();
    crate::stringgen::lower_string_concat(&mut program, None);
    crate::stringgen::lower_int_to_string(&mut program, None);
    let funcs = &program;
    let mut enc = Encoder::new();
    let func_labels: Vec<Label> = funcs.iter().map(|_| enc.new_label()).collect();
    let mut stack_maps: Vec<StackMapEntry> = Vec::new();
    let mut method_lines: Vec<(u32, LineTable)> = Vec::new();
    for (index, func) in funcs.iter().enumerate() {
        let func_offset = enc.position();
        enc.bind_label(func_labels[index]);
        let source_map = source_maps
            .get(index)
            .map(|m| m.0.as_slice())
            .unwrap_or(&[]);
        let mut lines = Vec::new();
        match prepare(func)? {
            Assignment::Registers { regs, saved } => {
                lower_into(
                    func,
                    &mut enc,
                    &regs,
                    saved,
                    &func_labels,
                    source_map,
                    &mut lines,
                    &mut stack_maps,
                    false,
                )?;
            }
            Assignment::Mixed {
                homes,
                saved,
                frame,
            } => {
                lower_mixed_into(
                    func,
                    &mut enc,
                    &homes,
                    saved,
                    frame,
                    &func_labels,
                    source_map,
                    &mut lines,
                    &mut stack_maps,
                    false,
                )?;
            }
            Assignment::Spilled => {
                lower_spilled_into(
                    func,
                    &mut enc,
                    &func_labels,
                    alloc_addr,
                    py_support,
                    source_map,
                    &mut lines,
                    &mut stack_maps,
                    vtables,
                    false,
                    None,
                    None,
                    None,
                )?;
            }
        }
        if index < original_count {
            method_lines.push((func_offset, LineTable(lines)));
        }
    }
    enc.finish()
        .map(|assembled| {
            for entry in &mut stack_maps {
                entry.return_pc = assembled.label_position_by_id(entry.return_pc).unwrap_or(0);
            }
            stack_maps.sort_by_key(|entry| entry.return_pc);
            (assembled.bytes, StackMaps(stack_maps), method_lines)
        })
        .map_err(reach_failure)
}

/// The ARMv6-M (Cortex-M) target code generator.
///
/// A unit type implementing the [`crate::target::TargetLowering`] seam by
/// delegating to [`lower`]; it will carry target options (the Cortex-M profile)
/// as the lowering grows.
#[derive(Debug, Clone, Copy, Default)]
pub struct Arm32;

impl TargetLowering for Arm32 {
    type Error = LowerError;

    fn lower(&self, func: &Function) -> Result<Vec<u8>, LowerError> {
        lower(func)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_ir::{BasicBlock, BlockId, MirType};

    #[test]
    fn lowers_constant_return() {
        let func = Function {
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
                        value: 42,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        assert_eq!(lower(&func).unwrap(), vec![0x2A, 0x20, 0x70, 0x47]);
    }

    #[test]
    fn descriptor_symbols_qualify_by_owner() {
        let plain = DescQualifiers::default();
        assert_eq!(
            descriptor_symbol(0x0200_0005, &plain),
            "__lamella_typedesc_33554437"
        );
        let lib = DescQualifiers {
            string: None,
            own: Some("0bd4d82a".into()),
            references: alloc::vec::Vec::new(),
        };
        assert_eq!(
            descriptor_symbol(0x0200_0005, &lib),
            "__lamella_typedesc_0bd4d82a_33554437"
        );
        let handle = (crate::resolver::REFERENCE_HANDLE_TABLE << 24) | (1 << 20) | 5;
        let refs = DescQualifiers {
            string: None,
            own: None,
            references: alloc::vec!["aaaaaaaa".into(), "0bd4d82a".into()],
        };
        assert_eq!(
            descriptor_symbol(handle, &refs),
            "__lamella_typedesc_0bd4d82a_33554437"
        );
        assert_eq!(
            crate::resolver::reference_handle_parts(lamella_ir::TypeHandle(handle)),
            Some((1, 0x0200_0005))
        );
        assert_eq!(
            crate::resolver::reference_handle_parts(lamella_ir::TypeHandle(handle)),
            Some((1, 0x0200_0005))
        );
        assert_eq!(
            crate::resolver::reference_handle_parts(lamella_ir::TypeHandle(0x0200_0005)),
            None
        );
    }

    /// **AN INSTANTIATION IS THE ONE HANDLE THAT MUST *NOT* QUALIFY BY ASSEMBLY, AND THE
    /// ORDINARY HANDLE BESIDE IT IS THE CONTROL.**
    ///
    /// The `own` hash keeps two libraries' row 5 APART -- two different types sharing a token
    /// number. `List<int>` named from a program and from a library is ONE type (assembly
    /// qualification NONE, `generics-identity-and-sharing` s6.1.0, forced by the loader's
    /// `(namespace, name)` interning), so it wants the reverse. Its handle comes from the canonical
    /// SPELLING rather than a row, which is what makes an unqualified symbol the same string in
    /// every build.
    ///
    /// **THE RULE IS ONLY SOUND BECAUSE AN INSTANTIATION HAS A TABLE BYTE OF ITS OWN**, `0x09` in
    /// `lamella-ir`. A branch keyed on a byte an instantiation SHARED with a front end's synthesized
    /// array would silently change every synthetic array's symbol in a library build as well. **The
    /// two bytes differing is asserted here, because it is the precondition the rule rests on rather
    /// than a fact about somewhere else.**
    ///
    /// A test that only checked the instantiation would pass under a `descriptor_symbol` that had
    /// stopped qualifying ANYTHING -- which is why the ordinary handle is asserted in the same test,
    /// under the same qualifiers.
    #[test]
    fn an_instantiations_descriptor_symbol_does_not_qualify_by_assembly() {
        assert_ne!(
            crate::generics::INSTANTIATION_HANDLE_TABLE,
            lamella_ir::SYNTHETIC_ARRAY_HANDLE_TABLE,
            "an instantiation's byte must not be a front end's synthesized-array byte"
        );
        let lib = DescQualifiers {
            string: None,
            own: Some("0bd4d82a".into()),
            references: alloc::vec::Vec::new(),
        };
        let instantiation = (crate::generics::INSTANTIATION_HANDLE_TABLE << 24) | 0x0061_73ac;
        assert_eq!(
            descriptor_symbol(instantiation, &lib),
            alloc::format!("__lamella_typedesc_{instantiation}"),
            "an instantiation is unqualified even in a build that qualifies its own types"
        );
        assert_eq!(
            descriptor_symbol(0x0200_0005, &lib),
            "__lamella_typedesc_0bd4d82a_33554437"
        );
        assert_eq!(
            descriptor_symbol(instantiation, &DescQualifiers::default()),
            descriptor_symbol(instantiation, &lib)
        );
    }

    #[test]
    fn lowers_an_mmio_store() {
        let func = Function {
            params: Vec::new(),
            ret: None,
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 0x5000_0508,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 0x2000,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Store {
                            address: ValueId(0),
                            value: ValueId(1),
                            width: 4,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(None)),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert!(bytes.windows(2).any(|w| w[1] == 0x60));
    }

    #[test]
    fn lowers_an_i64_add() {
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I64),
            value_types: vec![MirType::I64, MirType::I64, MirType::I64],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I64,
                            value: 5,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I64,
                            value: 3,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert!(
            bytes.windows(2).any(|w| w == [0x59, 0x41]),
            "ADCS (carry add) present"
        );
    }

    #[test]
    fn lowers_an_f64_constant_loading_both_words() {
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::F64),
            value_types: vec![MirType::F64],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::ConstInt {
                        ty: MirType::F64,
                        value: 0x4018_0000_0000_0000,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert!(
            bytes.windows(4).any(|w| w == [0x00, 0x00, 0x18, 0x40]),
            "the high word 0x40180000 of the f64 constant is materialized (not just the low word)"
        );
    }

    #[test]
    fn lowers_an_i64_mul() {
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I64),
            value_types: vec![MirType::I64, MirType::I64, MirType::I64],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I64,
                            value: 6,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I64,
                            value: 7,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Binary {
                            op: BinOp::Mul,
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert!(
            bytes.windows(2).any(|w| w == [0xF0, 0xB4]),
            "the 64-bit multiply's saved-scratch prologue is present"
        );
    }

    #[test]
    fn lowers_an_i64_shift() {
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I64),
            value_types: vec![MirType::I64, MirType::I32, MirType::I64],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I64,
                            value: 1,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 5,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Binary {
                            op: BinOp::Shl,
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert!(
            bytes.windows(2).any(|w| w == [0xF0, 0xB4]),
            "the 64-bit shift's saved-scratch prologue is present"
        );
    }

    #[test]
    fn lowers_an_i64_compare() {
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I64, MirType::I64, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I64,
                            value: 5,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I64,
                            value: 3,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Compare {
                            op: CmpOp::SignedLt,
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert!(
            bytes.windows(2).any(|w| w == [0x99, 0x41]),
            "SBCS (carry subtract) present"
        );
    }

    #[test]
    fn lowers_an_i64_widen() {
        let func = Function {
            params: vec![MirType::I32],
            ret: Some(MirType::I64),
            value_types: vec![MirType::I32, MirType::I64],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::Widen {
                        value: ValueId(0),
                        signed: true,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert!(
            bytes.windows(2).any(|w| w == [0xC1, 0x17]),
            "ASRS sign-extend present"
        );
    }

    #[test]
    fn lowers_a_blittable_struct() {
        let point = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 8,
            refs: lamella_ir::RefWords::NONE,
        };
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![point, MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (ValueId(0), Inst::InitStruct),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 7,
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
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 0,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(3)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert!(bytes.windows(2).any(|w| w == [0x00, 0x20]), "initobj zero");
    }

    #[test]
    fn lowers_a_struct_copy() {
        let point = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 8,
            refs: lamella_ir::RefWords::NONE,
        };
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![point, MirType::I32, MirType::I32, point, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (ValueId(0), Inst::InitStruct),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 9,
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
                    (ValueId(3), Inst::CopyStruct { src: ValueId(0) }),
                    (
                        ValueId(4),
                        Inst::FieldLoad {
                            base: ValueId(3),
                            offset: 0,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(4)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(lower(&func).is_ok());
    }

    #[test]
    fn a_sub_word_struct_field_copies_width_exact() {
        let flag = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 1,
            refs: lamella_ir::RefWords::NONE,
        };
        let func = Function {
            params: vec![MirType::ObjectRef],
            ret: Some(MirType::I32),
            value_types: vec![MirType::ObjectRef, flag, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![
                    (ValueId(1), Inst::FieldLoad { base: ValueId(0), offset: 0 }),
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
        let bytes = lower(&func).unwrap();
        assert!(
            bytes.windows(2).any(|w| w[1] & 0xF8 == 0x70),
            "the sub-word field store goes through STRB"
        );
        assert!(
            bytes.windows(2).any(|w| w[1] & 0xF8 == 0x78),
            "the sub-word field load goes through LDRB"
        );
    }

    #[test]
    fn lowers_a_struct_argument() {
        let point = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 8,
            refs: lamella_ir::RefWords::NONE,
        };
        let func = Function {
            params: vec![point],
            ret: Some(MirType::I32),
            value_types: vec![point, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::FieldLoad {
                        base: ValueId(0),
                        offset: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(lower(&func).is_ok());
    }

    #[test]
    fn passes_a_struct_argument_across_a_call() {
        let point = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 8,
            refs: lamella_ir::RefWords::NONE,
        };
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![point, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (ValueId(0), Inst::InitStruct),
                    (
                        ValueId(1),
                        Inst::Call {
                            callee: 1,
                            args: vec![ValueId(0)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let sum = Function {
            params: vec![point],
            ret: Some(MirType::I32),
            value_types: vec![point, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::FieldLoad {
                        base: ValueId(0),
                        offset: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert!(lower_module(&[main, sum]).is_ok());
    }

    #[test]
    fn returns_a_struct_by_value() {
        let point = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 8,
            refs: lamella_ir::RefWords::NONE,
        };
        let make = Function {
            params: Vec::new(),
            ret: Some(point),
            value_types: vec![point],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(ValueId(0), Inst::InitStruct)],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![point, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::Call {
                            callee: 1,
                            args: Vec::new(),
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 0,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert!(lower_module(&[main, make]).is_ok());
    }

    #[test]
    fn lowers_a_sub_word_conversion() {
        let func = Function {
            params: vec![MirType::I32],
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::Convert {
                        value: ValueId(0),
                        kind: ConvKind::SignExtend8,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert!(bytes.windows(2).any(|w| w[1] == 0xB2));
    }

    #[test]
    fn lowers_an_mmio_load() {
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 0x5000_0510,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::Load {
                            address: ValueId(0),
                            width: 4,
                            signed: false,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert!(bytes.windows(2).any(|w| w[1] == 0x68));
    }

    /// A one-instruction `Debug.WriteLine("Hi")` function, the fixture both console tests lower.
    fn semihost_write_func() -> Function {
        Function {
            params: Vec::new(),
            ret: None,
            value_types: vec![MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::SemihostWrite {
                        text: b"Hi\0".to_vec().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(None)),
            }],
        }
    }

    #[test]
    fn object_semihost_write_calls_the_console_seam_not_semihosting() {
        let bytes = lower_object(&[semihost_write_func()], &["main"], &[]).expect("lowers");
        let obj = lamella_elf::read_object(&bytes).expect("read the object back");
        let seam = obj
            .symbols
            .iter()
            .find(|s| s.name == "lamella_console_write_bytes")
            .expect("the console seam symbol is present");
        assert!(!seam.defined, "the seam is an undefined extern the linker resolves");
        assert!(
            obj.relocations
                .iter()
                .any(|r| obj.symbols.get(r.symbol as usize).map(|s| s.name.as_str())
                    == Some("lamella_console_write_bytes")),
            "a relocation names the console seam"
        );
        let halfwords: Vec<u16> = obj
            .text
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert!(
            !halfwords.contains(&0xBEAB),
            "no inline semihosting survives on the object path"
        );
    }

    /// A one-instruction `Console.WriteLine(42)` function.
    fn write_int_func() -> Function {
        Function {
            params: Vec::new(),
            ret: None,
            value_types: vec![MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 42,
                        },
                    ),
                    (ValueId(1), Inst::WriteInt { value: ValueId(0) }),
                ],
                terminator: Some(Terminator::Return(None)),
            }],
        }
    }

    #[test]
    fn object_write_int_uses_the_shared_helper_not_the_inline_itoa() {
        let bytes = lower_object(&[write_int_func()], &["main"], &[]).expect("lowers");
        let obj = lamella_elf::read_object(&bytes).expect("read the object back");
        assert!(
            obj.symbols
                .iter()
                .any(|s| s.name == "lamella_console_write_bytes" && !s.defined),
            "the console seam is an undefined extern"
        );
        let halfwords: Vec<u16> = obj
            .text
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert!(
            !halfwords.contains(&0xBEAB),
            "no inline semihosting survives -- the hand-encoded itoa is off the object path"
        );
    }

    #[test]
    fn flat_write_int_keeps_the_inline_itoa() {
        let bytes = lower(&write_int_func()).expect("lowers");
        let halfwords: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert!(
            halfwords.contains(&0xBEAB),
            "the flat path still emits the inline itoa's `bkpt 0xAB`"
        );
    }

    #[test]
    fn flat_semihost_write_keeps_inline_semihosting() {
        let bytes = lower(&semihost_write_func()).expect("lowers");
        let halfwords: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert!(
            halfwords.contains(&0xBEAB),
            "the flat path still emits `bkpt 0xAB` (SYS_WRITE0)"
        );
        assert!(
            bytes.windows(3).any(|w| w == b"Hi\0"),
            "and keeps the NUL terminator SYS_WRITE0 needs"
        );
    }

    #[test]
    fn lowers_a_semihost_write() {
        let func = Function {
            params: Vec::new(),
            ret: None,
            value_types: vec![MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::SemihostWrite {
                        text: b"Hi\0".to_vec().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(None)),
            }],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert!(
            bytes.windows(2).any(|w| w == [0xAB, 0xBE]),
            "BKPT 0xAB present"
        );
        assert!(bytes.windows(3).any(|w| w == b"Hi\0"), "string in the pool");
    }

    #[test]
    fn lower_debug_builds_a_line_table() {
        let func = Function {
            params: Vec::new(),
            ret: None,
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 0x5000_0508,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 0x2000,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Store {
                            address: ValueId(0),
                            value: ValueId(1),
                            width: 4,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(None)),
            }],
        };
        let source_map = vec![vec![2u32, 4, 6]];
        let (bytes, table) = lower_debug(&func, &source_map).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(table.0.first().map(|&(_, cil)| cil), Some(2));
        assert!(table.0.windows(2).all(|w| w[0].0 <= w[1].0));
        assert!(table.0.iter().all(|&(_, cil)| matches!(cil, 2 | 4 | 6)));
        let first = table.0.first().unwrap().0;
        assert_eq!(table.cil_offset_at(first), Some(2));
    }

    #[test]
    fn lowers_add_of_two_arguments() {
        let func = Function {
            params: vec![MirType::I32, MirType::I32],
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
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
        assert_eq!(
            lower(&func).unwrap(),
            vec![0x42, 0x18, 0x10, 0x46, 0x70, 0x47]
        );
    }

    #[test]
    fn lowers_a_six_parameter_function() {
        let func = Function {
            params: vec![MirType::I32; 6],
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32; 11],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: (0..6u32).map(ValueId).collect(),
                insts: vec![
                    (
                        ValueId(6),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                    (
                        ValueId(7),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(6),
                            rhs: ValueId(2),
                        },
                    ),
                    (
                        ValueId(8),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(7),
                            rhs: ValueId(3),
                        },
                    ),
                    (
                        ValueId(9),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(8),
                            rhs: ValueId(4),
                        },
                    ),
                    (
                        ValueId(10),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(9),
                            rhs: ValueId(5),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(10)))),
            }],
        };
        assert!(lower(&func).is_ok());
    }

    #[test]
    fn lowers_a_two_function_call() {
        let add = Function {
            params: vec![MirType::I32, MirType::I32],
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
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
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 40,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 2,
                        },
                    ),
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
        let bytes = lower_module(&[main, add]).unwrap();
        assert!(!bytes.is_empty());
        assert_eq!(&bytes[0..2], &[0x00, 0xB5]);
        assert_eq!(&bytes[bytes.len() - 2..], &[0x70, 0x47]);
    }

    fn spilled_branch_function() -> Function {
        let value_types = vec![MirType::I32; 20];
        let mut block0: Vec<(ValueId, Inst)> = (0..10)
            .map(|i| {
                (
                    ValueId(i),
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: i64::from(i) + 1,
                    },
                )
            })
            .collect();
        block0.push((
            ValueId(10),
            Inst::Compare {
                op: CmpOp::SignedLt,
                lhs: ValueId(0),
                rhs: ValueId(9),
            },
        ));
        let mut block1: Vec<(ValueId, Inst)> = vec![(
            ValueId(11),
            Inst::Binary {
                op: BinOp::Add,
                lhs: ValueId(0),
                rhs: ValueId(1),
            },
        )];
        for i in 0..8 {
            block1.push((
                ValueId(12 + i),
                Inst::Binary {
                    op: BinOp::Add,
                    lhs: ValueId(11 + i),
                    rhs: ValueId(2 + i),
                },
            ));
        }
        Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types,
            entry: BlockId(0),
            blocks: vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: block0,
                    terminator: Some(Terminator::Branch {
                        cond: ValueId(10),
                        if_true: BlockId(1),
                        true_args: Vec::new(),
                        if_false: BlockId(2),
                        false_args: Vec::new(),
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: block1,
                    terminator: Some(Terminator::Return(Some(ValueId(19)))),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Some(Terminator::Return(Some(ValueId(0)))),
                },
            ],
        }
    }

    #[test]
    fn lowers_a_spilled_branch_as_a_register_spill_mix() {
        let func = spilled_branch_function();
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(matches!(prepare(&func).unwrap(), Assignment::Mixed { .. }));
        let bytes = lower(&func).unwrap();
        assert!(
            bytes[1] == 0xB4 || bytes[1] == 0xB5,
            "opens by pushing the callee-saved registers"
        );
        assert_eq!(&bytes[bytes.len() - 2..], &[0x70, 0x47]);
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
        assert!(matches!(
            prepare(&func).unwrap(),
            Assignment::Registers { .. }
        ));
        let bytes = lower(&func).unwrap();
        let halfwords: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert!(
            halfwords.iter().any(|&h| h == 0x2201),
            "the compare materializes its 0/1 into r2"
        );
        let cmp_on_value = halfwords.iter().filter(|&&h| h == 0x2A00).count();
        assert!(
            cmp_on_value >= 2,
            "both branch sites test the materialized compare against zero (found {cmp_on_value})"
        );
    }

    #[test]
    fn a_compare_reused_by_a_later_block_branch_materializes_on_the_mixed_path() {
        let i32t = MirType::I32;
        let n = |v: u32| ValueId(v);
        let mut block0: Vec<(ValueId, Inst)> = (1..=9)
            .map(|i| {
                (
                    n(i),
                    Inst::ConstInt {
                        ty: i32t,
                        value: i64::from(i) + 9,
                    },
                )
            })
            .collect();
        block0.push((
            n(10),
            Inst::Compare {
                op: CmpOp::SignedLt,
                lhs: n(0),
                rhs: n(1),
            },
        ));
        let mut sums: Vec<(ValueId, Inst)> = Vec::new();
        for i in 0..8u32 {
            sums.push((
                n(11 + i),
                Inst::Binary {
                    op: BinOp::Add,
                    lhs: if i == 0 { n(1) } else { n(10 + i) },
                    rhs: n(2 + i),
                },
            ));
        }
        let func = Function {
            params: vec![i32t],
            ret: Some(i32t),
            value_types: vec![i32t; 19],
            entry: BlockId(0),
            blocks: vec![
                BasicBlock {
                    params: vec![n(0)],
                    insts: block0,
                    terminator: Some(Terminator::Branch {
                        cond: n(10),
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
                        cond: n(10),
                        if_true: BlockId(2),
                        true_args: Vec::new(),
                        if_false: BlockId(3),
                        false_args: Vec::new(),
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: sums,
                    terminator: Some(Terminator::Return(Some(n(18)))),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Some(Terminator::Return(Some(n(0)))),
                },
            ],
        };
        assert!(lamella_ir::verify(&func).is_ok());
        assert!(matches!(prepare(&func).unwrap(), Assignment::Mixed { .. }));
        let bytes = lower(&func).unwrap();
        let halfwords: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert!(
            halfwords.iter().any(|&h| h & 0xF8FF == 0x2001),
            "the compare materializes its 0/1 on the mixed path"
        );
        let cmp_on_value = halfwords.iter().filter(|&&h| h & 0xF8FF == 0x2800).count();
        assert!(
            cmp_on_value >= 2,
            "both branch sites test the materialized compare against zero (found {cmp_on_value})"
        );
    }

    #[test]
    fn a_nested_if_on_one_bool_local_reaches_the_register_path_materialized() {
        use crate::cil::{lower_method_typed, NoCalls};
        use lamella_cil::{Instruction, MethodBodyImage, Opcode, Operand};
        let body = MethodBodyImage {
            max_stack: 2,
            init_locals: true,
            local_var_sig: None,
            code: vec![
                Instruction::simple(Opcode::LdcI43),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(Opcode::Clt),
                Instruction::simple(Opcode::Stloc1),
                Instruction::simple(Opcode::Ldloc1),
                Instruction::new(Opcode::BrfalseS, Operand::Target(18)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(9)),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc1),
                Instruction::new(Opcode::BrfalseS, Operand::Target(18)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(30)),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        let (func, _) = lower_method_typed(
            &body,
            &NoCalls,
            &[MirType::I32, MirType::I32],
            &[MirType::I32, MirType::I32],
        )
        .unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        let compares: Vec<ValueId> = func
            .blocks
            .iter()
            .flat_map(|b| &b.insts)
            .filter(|(_, i)| matches!(i, Inst::Compare { .. }))
            .map(|(v, _)| *v)
            .collect();
        assert_eq!(compares.len(), 1, "the source has exactly one comparison");
        let branches_on_it = func
            .blocks
            .iter()
            .filter(
                |b| matches!(&b.terminator, Some(Terminator::Branch { cond, .. }) if *cond == compares[0]),
            )
            .count();
        assert_eq!(
            branches_on_it, 2,
            "both `if (t)` branches read the compare's own value (found {branches_on_it})"
        );
        assert!(
            matches!(prepare(&func).unwrap(), Assignment::Registers { .. }),
            "the pure-int no-call method takes the register path"
        );
        let bytes = lower(&func).unwrap();
        let halfwords: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert!(
            halfwords.iter().any(|&h| h & 0xF8FF == 0x2001),
            "the compare materializes its 0/1"
        );
        let cmp_on_value = halfwords.iter().filter(|&&h| h & 0xF8FF == 0x2800).count();
        assert!(
            cmp_on_value >= 2,
            "both `if (t)` sites test the materialized value against zero (found {cmp_on_value})"
        );
    }

    /// A straight-line pure-int function whose `n` constants are ALL live at once (each feeds the
    /// final left-associative sum), so an 8-register allocation must spill most of them -- a large
    /// register/spill MIXED frame. For n past ~135 that frame exceeds the single `SUB SP,#imm`
    /// reach (508) and must chunk via `sub_sp_far`.
    fn wide_live_mixed_function(n: u32) -> Function {
        let mut insts: Vec<(ValueId, Inst)> = (0..n)
            .map(|i| {
                (
                    ValueId(i),
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: 1,
                    },
                )
            })
            .collect();
        let mut acc = ValueId(0);
        for i in 1..n {
            let dst = ValueId(n - 1 + i);
            insts.push((
                dst,
                Inst::Binary {
                    op: BinOp::Add,
                    lhs: acc,
                    rhs: ValueId(i),
                },
            ));
            acc = dst;
        }
        Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32; (2 * n - 1) as usize],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts,
                terminator: Some(Terminator::Return(Some(acc))),
            }],
        }
    }

    #[test]
    fn lowers_a_mixed_frame_past_the_single_sub_sp_reach() {
        let func = wide_live_mixed_function(160);
        assert!(lamella_ir::verify(&func).is_ok());
        let frame = match prepare(&func).unwrap() {
            Assignment::Mixed { frame, .. } => frame,
            _ => panic!("a wide pure-int live set should take the register/spill Mixed path"),
        };
        assert!(
            frame > 508,
            "the spill frame {frame} exceeds the single SUB SP,#imm reach"
        );
        assert!(
            frame <= 1020,
            "and stays within the LDR/STR [SP,#imm] slot reach ({frame})"
        );
        let bytes = lower(&func).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], &[0x70, 0x47], "ends with bx lr");
    }

    #[test]
    fn lowers_a_fully_spilled_frame_past_the_single_sub_sp_reach() {
        const N: u32 = 100;
        let seed = Function {
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
                        value: 1,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let mut insts: Vec<(ValueId, Inst)> = (0..N)
            .map(|i| {
                (
                    ValueId(i),
                    Inst::Call {
                        callee: 1,
                        args: Vec::new(),
                    },
                )
            })
            .collect();
        let mut acc = ValueId(0);
        for i in 1..N {
            let dst = ValueId(N - 1 + i);
            insts.push((
                dst,
                Inst::Binary {
                    op: BinOp::Add,
                    lhs: acc,
                    rhs: ValueId(i),
                },
            ));
            acc = dst;
        }
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32; (2 * N - 1) as usize],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts,
                terminator: Some(Terminator::Return(Some(acc))),
            }],
        };
        assert!(matches!(prepare(&main), Ok(Assignment::Spilled)));
        assert!(
            lower_object(&[main, seed], &["main", "seed"], &[]).is_ok(),
            "a >508-byte fully-spilled frame lowers via sub_sp_far"
        );
    }

    fn cross_call_example() -> [Function; 2] {
        let g = Function {
            params: vec![MirType::I32],
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 1,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let f = Function {
            params: vec![MirType::I32],
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![
                    (
                        ValueId(1),
                        Inst::Call {
                            callee: 1,
                            args: vec![ValueId(0)],
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(1),
                            rhs: ValueId(0),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        [f, g]
    }

    #[test]
    fn a_value_live_across_a_call_spills() {
        let module = cross_call_example();
        assert!(
            crate::regalloc::Liveness::analyze(&module[0]).any_value_live_across_call(&module[0])
        );
        let bytes = lower_module(&module).unwrap();
        assert_eq!(&bytes[0..2], &[0x00, 0xB5]);
    }

    #[test]
    fn lowers_subtraction() {
        let func = Function {
            params: vec![MirType::I32, MirType::I32],
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![(
                    ValueId(2),
                    Inst::Binary {
                        op: BinOp::Sub,
                        lhs: ValueId(0),
                        rhs: ValueId(1),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        assert_eq!(
            lower(&func).unwrap(),
            vec![0x42, 0x1A, 0x10, 0x46, 0x70, 0x47]
        );
    }

    #[test]
    fn lowers_bitwise_and_via_move_then_operate() {
        let func = Function {
            params: vec![MirType::I32, MirType::I32],
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![(
                    ValueId(2),
                    Inst::Binary {
                        op: BinOp::And,
                        lhs: ValueId(0),
                        rhs: ValueId(1),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        assert_eq!(
            lower(&func).unwrap(),
            vec![0x02, 0x46, 0x0A, 0x40, 0x10, 0x46, 0x70, 0x47]
        );
    }

    #[test]
    #[ignore = "writes a micro:bit image for manual QEMU validation"]
    fn emit_qemu_microbit_image() {
        use lamella_asm_arm32::{Encoder, Reg};

        let add = Function {
            params: vec![MirType::I32, MirType::I32],
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
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
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 40,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 2,
                        },
                    ),
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
        let module = lower_module(&[main, add]).unwrap();

        let mut img = Encoder::new();
        img.emit_word(0x2000_4000);
        img.emit_word(0x0000_0009);
        let main_label = img.new_label();
        img.bl(main_label);
        img.movs_imm(Reg::R2, 0x20).unwrap();
        img.lsls_imm(Reg::R2, Reg::R2, 24).unwrap();
        img.movs_imm(Reg::R3, 0x80).unwrap();
        img.lsls_imm(Reg::R3, Reg::R3, 10).unwrap();
        img.adds_imm8(Reg::R3, 0x26).unwrap();
        img.str_imm(Reg::R3, Reg::R2, 0).unwrap();
        img.str_imm(Reg::R0, Reg::R2, 4).unwrap();
        img.mov_reg(Reg::R1, Reg::R2);
        img.movs_imm(Reg::R0, 0x20).unwrap();
        img.bkpt(0xAB);
        img.bind_label(main_label);
        img.emit_bytes(&module);
        let image = img.finish().unwrap().bytes;

        let path = std::env::temp_dir().join("lamella_microbit.bin");
        std::fs::write(&path, &image).unwrap();
        eprintln!("wrote {} bytes to {}", image.len(), path.display());
    }

    #[test]
    fn lowers_wide_constant_via_literal_pool() {
        let func = Function {
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
                        value: 0x1_2345,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let bytes = lower(&func).unwrap();
        assert_eq!(bytes[1], 0x48);
        assert_eq!(&bytes[bytes.len() - 4..], &0x0001_2345u32.to_le_bytes());
    }

    /// `fn() -> i32 { return (5 > 3) ? 7 : 9 }` as a four-block CFG: a comparison
    /// and conditional branch, two arms that each jump to a join block carrying
    /// their result, and a return of the join's parameter.
    fn if_else_function() -> Function {
        Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32; 6],
            entry: BlockId(0),
            blocks: vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![
                        (
                            ValueId(0),
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: 5,
                            },
                        ),
                        (
                            ValueId(1),
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: 3,
                            },
                        ),
                        (
                            ValueId(2),
                            Inst::Compare {
                                op: CmpOp::SignedGt,
                                lhs: ValueId(0),
                                rhs: ValueId(1),
                            },
                        ),
                    ],
                    terminator: Some(Terminator::Branch {
                        cond: ValueId(2),
                        if_true: BlockId(1),
                        true_args: Vec::new(),
                        if_false: BlockId(2),
                        false_args: Vec::new(),
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![(
                        ValueId(3),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 7,
                        },
                    )],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(3),
                        args: vec![ValueId(3)],
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![(
                        ValueId(4),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 9,
                        },
                    )],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(3),
                        args: vec![ValueId(4)],
                    }),
                },
                BasicBlock {
                    params: vec![ValueId(5)],
                    insts: Vec::new(),
                    terminator: Some(Terminator::Return(Some(ValueId(5)))),
                },
            ],
        }
    }

    #[test]
    fn lowers_if_else_control_flow() {
        let bytes = lower(&if_else_function()).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], &[0x70, 0x47]);
    }

    /// `fn() -> i32 { let mut s = 0; let mut i = 1; while i <= 5 { s += i; i += 1 } s }`
    /// as a counting loop: a header that compares and branches, a body that updates
    /// the accumulator and counter and jumps back, and a return of the sum (15).
    fn sum_loop_function() -> Function {
        Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32; 8],
            entry: BlockId(0),
            blocks: vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![
                        (
                            ValueId(0),
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: 0,
                            },
                        ),
                        (
                            ValueId(1),
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: 1,
                            },
                        ),
                        (
                            ValueId(2),
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: 5,
                            },
                        ),
                    ],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: vec![ValueId(0), ValueId(1)],
                    }),
                },
                BasicBlock {
                    params: vec![ValueId(3), ValueId(4)],
                    insts: vec![(
                        ValueId(5),
                        Inst::Compare {
                            op: CmpOp::SignedGt,
                            lhs: ValueId(4),
                            rhs: ValueId(2),
                        },
                    )],
                    terminator: Some(Terminator::Branch {
                        cond: ValueId(5),
                        if_true: BlockId(3),
                        true_args: Vec::new(),
                        if_false: BlockId(2),
                        false_args: Vec::new(),
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![
                        (
                            ValueId(6),
                            Inst::Binary {
                                op: BinOp::Add,
                                lhs: ValueId(3),
                                rhs: ValueId(4),
                            },
                        ),
                        (
                            ValueId(7),
                            Inst::Binary {
                                op: BinOp::Add,
                                lhs: ValueId(4),
                                rhs: ValueId(1),
                            },
                        ),
                    ],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: vec![ValueId(6), ValueId(7)],
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Some(Terminator::Return(Some(ValueId(3)))),
                },
            ],
        }
    }

    #[test]
    fn lowers_a_counting_loop() {
        let bytes = lower(&sum_loop_function()).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], &[0x70, 0x47]);
    }

    /// A straight-line running sum of 1..=6 over eleven values -- more than the
    /// eight registers -- forcing the stack-spilling path. The result is 21.
    fn spilled_sum_function() -> Function {
        let mut insts: Vec<(ValueId, Inst)> = (0..6)
            .map(|n| {
                (
                    ValueId(n),
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: i64::from(n) + 1,
                    },
                )
            })
            .collect();
        for k in 0..5u32 {
            let acc = if k == 0 { ValueId(0) } else { ValueId(5 + k) };
            insts.push((
                ValueId(6 + k),
                Inst::Binary {
                    op: BinOp::Add,
                    lhs: acc,
                    rhs: ValueId(1 + k),
                },
            ));
        }
        Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32; 11],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts,
                terminator: Some(Terminator::Return(Some(ValueId(10)))),
            }],
        }
    }

    #[test]
    fn lowers_spilled_straight_line() {
        let bytes = lower(&spilled_sum_function()).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], &[0x70, 0x47]);
    }

    #[test]
    fn lowers_a_block_parameter_swap_via_scratch() {
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32; 4],
            entry: BlockId(0),
            blocks: vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![
                        (
                            ValueId(0),
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: 1,
                            },
                        ),
                        (
                            ValueId(1),
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: 2,
                            },
                        ),
                    ],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: vec![ValueId(0), ValueId(1)],
                    }),
                },
                BasicBlock {
                    params: vec![ValueId(2), ValueId(3)],
                    insts: Vec::new(),
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: vec![ValueId(3), ValueId(2)],
                    }),
                },
            ],
        };
        assert!(lower(&func).is_ok());
    }

    #[test]
    fn lowers_unreachable_and_a_late_entry() {
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32],
            entry: BlockId(1),
            blocks: vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Some(Terminator::Unreachable),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![(
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 5,
                        },
                    )],
                    terminator: Some(Terminator::Return(Some(ValueId(0)))),
                },
            ],
        };
        assert!(lower(&func).is_ok());
    }

    #[test]
    fn branch_with_arguments_is_rejected_not_miscompiled() {
        let mut func = if_else_function();
        if let Some(Terminator::Branch { true_args, .. }) = func.blocks[0].terminator.as_mut() {
            *true_args = vec![ValueId(0)];
        }
        assert!(lower(&func).is_err());
    }

    /// A pure-integer loop with more values live in its body than there are registers:
    /// the loop-carried sum and counter, the limit and increment, and six invariants
    /// used only at the exit -- ten live at once. The linear scan spills some, so it
    /// lowers as a register/spill mix rather than falling to the all-spilled path.
    fn many_value_loop() -> Function {
        let n = |v: u32| ValueId(v);
        let constant = |v: u32, value: i64| {
            (
                n(v),
                Inst::ConstInt {
                    ty: MirType::I32,
                    value,
                },
            )
        };
        let add = |v: u32, lhs: u32, rhs: u32| {
            (
                n(v),
                Inst::Binary {
                    op: BinOp::Add,
                    lhs: n(lhs),
                    rhs: n(rhs),
                },
            )
        };
        Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32; 26],
            entry: BlockId(0),
            blocks: vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![
                        constant(0, 1),
                        constant(6, 1),
                        constant(7, 1),
                        constant(8, 1),
                        constant(9, 1),
                        constant(10, 1),
                        constant(11, 1),
                        constant(12, 0),
                        constant(13, 1),
                        constant(14, 8),
                    ],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: vec![n(12), n(13)],
                    }),
                },
                BasicBlock {
                    params: vec![n(15), n(16)],
                    insts: vec![(
                        n(17),
                        Inst::Compare {
                            op: CmpOp::SignedGt,
                            lhs: n(16),
                            rhs: n(14),
                        },
                    )],
                    terminator: Some(Terminator::Branch {
                        cond: n(17),
                        if_true: BlockId(3),
                        true_args: Vec::new(),
                        if_false: BlockId(2),
                        false_args: Vec::new(),
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![add(18, 15, 16), add(19, 16, 0)],
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: vec![n(18), n(19)],
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: vec![
                        add(20, 15, 6),
                        add(21, 20, 7),
                        add(22, 21, 8),
                        add(23, 22, 9),
                        add(24, 23, 10),
                        add(25, 24, 11),
                    ],
                    terminator: Some(Terminator::Return(Some(n(25)))),
                },
            ],
        }
    }

    #[test]
    fn a_spilling_loop_takes_the_register_spill_mix() {
        let func = many_value_loop();
        assert!(lamella_ir::verify(&func).is_ok());
        match prepare(&func).unwrap() {
            Assignment::Mixed { homes, frame, .. } => {
                assert!(frame > 0, "the mix needs a spill frame");
                assert!(
                    homes.iter().any(|h| matches!(h, Home::Reg(_))),
                    "some values stay in registers"
                );
                assert!(
                    homes.iter().any(|h| matches!(h, Home::Spill(_))),
                    "some values spill"
                );
                assert!(
                    homes
                        .iter()
                        .all(|h| !matches!(h, Home::Reg(r) if r.number() < 2)),
                    "the scratch registers are not allocated to values"
                );
            }
            _ => panic!("expected the register/spill mix"),
        }
        let bytes = lower(&func).expect("the mix lowers");
        assert!(!bytes.is_empty());
    }

    #[test]
    fn home_moves_break_a_register_cycle_and_shuttle_slots() {
        let mut enc = Encoder::new();
        emit_home_moves(
            &mut enc,
            &[
                (Home::Reg(Reg::R2), Home::Reg(Reg::R3)),
                (Home::Reg(Reg::R3), Home::Reg(Reg::R2)),
                (Home::Spill(0), Home::Spill(4)),
                (Home::Reg(Reg::R2), Home::Reg(Reg::R2)),
            ],
            Reg::R0,
        )
        .unwrap();
        assert!(!enc.finish().unwrap().bytes.is_empty());
    }

    #[test]
    fn lowers_a_reference_type_allocation() {
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: lamella_ir::TypeHandle(1),
                        payload_size: 12,
                        ref_offsets: vec![4u32, 8u32].into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let bytes = lower_module_gc(core::slice::from_ref(&func), 0x09)
            .expect("the GC entry lowers an alloc");
        assert!(
            bytes.windows(4).any(|w| w == [12, 0, 0, 0]),
            "payload_size word emitted"
        );
        assert!(
            bytes.windows(4).any(|w| w == [2, 0, 0, 0]),
            "nrefs word emitted"
        );
        assert!(lower_module(&[func]).is_err());
    }

    #[test]
    fn emits_a_vtable_before_the_type_descriptor() {
        let allocator = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: lamella_ir::TypeHandle(1),
                        payload_size: 4,
                        ref_offsets: Vec::new().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let method = Function {
            params: Vec::new(),
            ret: None,
            value_types: Vec::new(),
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: Vec::new(),
                terminator: Some(Terminator::Return(None)),
            }],
        };
        let module = [allocator, method];
        let tag: u32 = 0xDEAD_BEEF;
        let iface_tag: u32 = 0x0CAF_E001;
        let plain = lower_module_gc(&module, 0x09).expect("plain module lowers");
        let with_meta = lower_module_gc_vtables(
            &module,
            0x09,
            &[TypeMeta {
                handle: lamella_ir::TypeHandle(1),
                type_tag: tag,
                vtable: vec![VtableEntry::Func(1)],
                itable: vec![(iface_tag, VtableEntry::Func(1))],
                base: None,
                words: None,
                exported: true,
                full_name: None,
            }],
        )
        .expect("metadata module lowers");
        assert!(
            with_meta.len() > plain.len(),
            "the vtable word, the appended type_tag, and the itable grow the image"
        );
        let present = |image: &[u8], v: u32| image.windows(4).any(|w| w == v.to_le_bytes());
        assert!(
            present(&with_meta, tag),
            "type_tag emitted into the descriptor"
        );
        assert!(
            present(&with_meta, iface_tag),
            "itable interface-method tag emitted"
        );
        assert!(
            !present(&plain, tag),
            "no type_tag without per-type metadata"
        );
        assert!(
            !present(&plain, iface_tag),
            "no itable without per-type metadata"
        );
    }

    #[test]
    fn emits_a_safepoint_stack_map_for_a_live_root() {
        let alloc = || Inst::Alloc {
            handle: lamella_ir::TypeHandle(1),
            payload_size: 4,
            ref_offsets: Vec::new().into_boxed_slice(),
        };
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![
                MirType::ObjectRef,
                MirType::ObjectRef,
                MirType::I32,
                MirType::I32,
                MirType::I32,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (ValueId(0), alloc()),
                    (ValueId(1), alloc()),
                    (
                        ValueId(2),
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 0,
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::FieldLoad {
                            base: ValueId(1),
                            offset: 0,
                        },
                    ),
                    (
                        ValueId(4),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(2),
                            rhs: ValueId(3),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(4)))),
            }],
        };
        let (_code, maps) = lower_module_gc_mapped(&[func], 0x09).expect("lowers with stack maps");
        assert_eq!(maps.0.len(), 2);
        assert!(maps.0[0].return_pc <= maps.0[1].return_pc);
        let with_roots: Vec<_> = maps
            .0
            .iter()
            .filter(|e| !e.ref_offsets.is_empty())
            .collect();
        assert_eq!(with_roots.len(), 1);
        assert_eq!(with_roots[0].ref_offsets, vec![0]);
        assert_eq!(&maps.encode()[0..4], &2u32.to_le_bytes());
    }

    #[test]
    fn emits_a_stack_map_at_a_call_holding_a_root() {
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::ObjectRef, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::Alloc {
                            handle: lamella_ir::TypeHandle(1),
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
                        Inst::FieldLoad {
                            base: ValueId(0),
                            offset: 0,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let helper = Function {
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
                        value: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let (_code, maps) =
            lower_module_gc_mapped(&[main, helper], 0x09).expect("lowers with stack maps");
        assert_eq!(maps.0.len(), 2);
        assert!(
            maps.0.iter().any(|e| e.ref_offsets == vec![0]),
            "the call holding `a` names it as a root"
        );
    }

    #[test]
    fn a_py_value_root_goes_in_the_tagged_list_not_ref_offsets() {
        let main = Function {
            params: vec![MirType::PyValue],
            ret: Some(MirType::PyValue),
            value_types: vec![MirType::PyValue, MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![
                    (
                        ValueId(1),
                        Inst::Alloc {
                            handle: lamella_ir::TypeHandle(1),
                            payload_size: 4,
                            ref_offsets: Vec::new().into_boxed_slice(),
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Call {
                            callee: 1,
                            args: Vec::new(),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let helper = Function {
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
                        value: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let (_code, maps) =
            lower_module_gc_mapped(&[main, helper], 0x09).expect("lowers with stack maps");
        assert_eq!(maps.0.len(), 2);
        assert!(
            maps.0.iter().any(|e| e.tagged_offsets == vec![0]),
            "p is recorded as a tagged (scan-by-tag) root"
        );
        assert!(
            maps.0.iter().all(|e| e.ref_offsets.is_empty()),
            "no unconditional ObjectRef root is recorded"
        );
        assert_eq!(&maps.encode()[0..4], &2u32.to_le_bytes());
    }

    #[test]
    fn lowers_a_py_getattr_to_a_runtime_support_call() {
        let main = Function {
            params: vec![MirType::PyValue],
            ret: Some(MirType::PyValue),
            value_types: vec![MirType::PyValue, MirType::PyValue],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::PyIntrinsic {
                        op: lamella_ir::PyOp::Getattr { name: 5 },
                        args: vec![ValueId(0)],
                        cache: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let support = PySupport {
            getattr: Some(0x1234),
            ..Default::default()
        };
        let (code, maps) = lower_module_py(&[main.clone()], None, support).expect("getattr lowers");
        assert!(!code.is_empty(), "produced code");
        assert_eq!(maps.0.len(), 1, "the getattr call is one safepoint");
        assert!(matches!(
            lower_module_py(&[main], None, PySupport::default()),
            Err(LowerError::CallUnsupported)
        ));
    }

    #[test]
    fn lowers_a_py_len_to_a_runtime_support_call() {
        let main = Function {
            params: vec![MirType::PyValue],
            ret: Some(MirType::PyValue),
            value_types: vec![MirType::PyValue, MirType::PyValue],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::PyIntrinsic {
                        op: lamella_ir::PyOp::Len,
                        args: vec![ValueId(0)],
                        cache: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let support = PySupport {
            len: Some(0x2000),
            ..Default::default()
        };
        let (code, maps) = lower_module_py(&[main.clone()], None, support).expect("len lowers");
        assert!(!code.is_empty(), "produced code");
        assert_eq!(maps.0.len(), 1, "the len call is one safepoint");
        assert!(matches!(
            lower_module_py(
                &[main],
                None,
                PySupport {
                    getattr: Some(1),
                    ..Default::default()
                }
            ),
            Err(LowerError::CallUnsupported)
        ));
    }

    #[test]
    fn lowers_a_py_call_to_a_runtime_support_call() {
        let main = Function {
            params: vec![MirType::PyValue, MirType::PyValue, MirType::PyValue],
            ret: Some(MirType::PyValue),
            value_types: vec![
                MirType::PyValue,
                MirType::PyValue,
                MirType::PyValue,
                MirType::PyValue,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1), ValueId(2)],
                insts: vec![(
                    ValueId(3),
                    Inst::PyIntrinsic {
                        op: lamella_ir::PyOp::Call,
                        args: vec![ValueId(0), ValueId(1), ValueId(2)],
                        cache: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(3)))),
            }],
        };
        let support = PySupport {
            call: Some(0x3000),
            ..Default::default()
        };
        let (code, maps) = lower_module_py(&[main.clone()], None, support).expect("call lowers");
        assert!(!code.is_empty(), "produced code");
        assert_eq!(maps.0.len(), 1, "the py_call is one safepoint");
        assert!(matches!(
            lower_module_py(&[main], None, PySupport::default()),
            Err(LowerError::CallUnsupported)
        ));
    }

    #[test]
    fn flat_path_rejects_soft_float_ops_instead_of_miscompiling() {
        let single = |value_types: Vec<MirType>, result: ValueId, ops: Vec<(ValueId, Inst)>| Function {
            params: Vec::new(),
            ret: value_types.get(result.0 as usize).copied(),
            value_types,
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: ops,
                terminator: Some(Terminator::Return(Some(result))),
            }],
        };
        let f64c = |v: ValueId| {
            (
                v,
                Inst::ConstInt {
                    ty: MirType::F64,
                    value: 0,
                },
            )
        };

        let dbl_add = single(
            vec![MirType::F64, MirType::F64, MirType::F64],
            ValueId(2),
            vec![
                f64c(ValueId(0)),
                f64c(ValueId(1)),
                (
                    ValueId(2),
                    Inst::Binary {
                        op: BinOp::Add,
                        lhs: ValueId(0),
                        rhs: ValueId(1),
                    },
                ),
            ],
        );
        assert!(
            matches!(
                lower_module_py(&[dbl_add], None, PySupport::default()),
                Err(LowerError::CallUnsupported)
            ),
            "f64 add must not silently lower as an integer add of the low words"
        );

        let dbl_cmp = single(
            vec![MirType::F64, MirType::F64, MirType::I32],
            ValueId(2),
            vec![
                f64c(ValueId(0)),
                f64c(ValueId(1)),
                (
                    ValueId(2),
                    Inst::Compare {
                        op: CmpOp::SignedGt,
                        lhs: ValueId(0),
                        rhs: ValueId(1),
                    },
                ),
            ],
        );
        assert!(
            matches!(
                lower_module_py(&[dbl_cmp], None, PySupport::default()),
                Err(LowerError::CallUnsupported)
            ),
            "f64 compare must not silently lower as an integer compare of the low words"
        );

        let int_to_dbl = single(
            vec![MirType::I32, MirType::F64],
            ValueId(1),
            vec![
                (
                    ValueId(0),
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: 3,
                    },
                ),
                (
                    ValueId(1),
                    Inst::Convert {
                        value: ValueId(0),
                        kind: ConvKind::IntToFloat64,
                    },
                ),
            ],
        );
        assert!(
            matches!(
                lower_module_py(&[int_to_dbl], None, PySupport::default()),
                Err(LowerError::CallUnsupported)
            ),
            "int->f64 convert must not silently lower as an integer widen"
        );
    }

    #[test]
    fn lower_object_emits_an_arm_relocatable_object() {
        let answer = Function {
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
                        value: 42,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Call {
                        callee: 1,
                        args: Vec::new(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let obj_bytes =
            lower_object(&[main, answer], &["main", "answer"], &[]).expect("lower_object");
        let obj = lamella_elf::read_object(&obj_bytes).expect("a valid ELF object");
        assert_eq!(obj.machine, lamella_elf::Machine::Arm);
        let main_sym = obj.symbols.iter().find(|s| s.name == "main").unwrap();
        let answer_sym = obj.symbols.iter().find(|s| s.name == "answer").unwrap();
        assert_eq!(main_sym.value & 1, 1, "main is a Thumb function");
        assert_eq!(answer_sym.value & 1, 1, "answer is a Thumb function");
        let code = code_relocations(&obj);
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].kind, lamella_elf::arm::R_ARM_THM_CALL);
        assert_eq!(code[0].addend, -4);
    }

    #[test]
    fn lower_object_emits_a_spilled_function() {
        let answer = Function {
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
                        value: 42,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::Call {
                            callee: 1,
                            args: Vec::new(),
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
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        assert!(matches!(prepare(&main), Ok(Assignment::Spilled)));
        let obj_bytes =
            lower_object(&[main, answer], &["main", "answer"], &[]).expect("lower_object");
        let obj = lamella_elf::read_object(&obj_bytes).unwrap();
        let code = code_relocations(&obj);
        assert_eq!(code.len(), 2);
        assert!(
            code.iter()
                .all(|r| r.kind == lamella_elf::arm::R_ARM_THM_CALL && r.addend == -4)
        );
    }

    #[test]
    fn lower_object_emits_a_function_pointer_and_indirect_call() {
        let answer = Function {
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
                        value: 42,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (ValueId(0), Inst::FuncAddr { func: 1 }),
                    (
                        ValueId(1),
                        Inst::CallIndirect {
                            target: ValueId(0),
                            args: Vec::new(),
                            returns_value: true,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert!(matches!(prepare(&main), Ok(Assignment::Spilled)));
        let obj = lamella_elf::read_object(
            &lower_object(&[main, answer], &["main", "answer"], &[]).expect("lower_object"),
        )
        .unwrap();
        let code = code_relocations(&obj);
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].kind, lamella_elf::arm::R_ARM_ABS32);
    }

    #[test]
    fn lower_object_emits_a_callnative_to_an_extern_symbol() {
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 20,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 22,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::CallNative {
                            symbol: 0,
                            args: vec![ValueId(0), ValueId(1)],
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let obj = lamella_elf::read_object(&lower_object(&[main], &["main"], &["cadd"]).unwrap())
            .unwrap();
        let cadd = obj.symbols.iter().find(|s| s.name == "cadd").unwrap();
        assert!(!cadd.defined, "the extern symbol is undefined");
        let code = code_relocations(&obj);
        assert_eq!(code.len(), 1);
        assert_eq!(code[0].kind, lamella_elf::arm::R_ARM_THM_CALL);
        assert_eq!(obj.symbols[code[0].symbol as usize].name, "cadd");
    }

    #[test]
    fn lower_object_emits_a_far_islandable_string_blob() {
        let utf16: Box<[u16]> = alloc::vec![b'H' as u16, b'i' as u16].into_boxed_slice();
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef, MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (ValueId(0), Inst::StringLiteral { utf16: utf16.clone() }),
                    (ValueId(1), Inst::StringLiteral { utf16: utf16.clone() }),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let obj = lamella_elf::read_object(&lower_object(&[main], &["main"], &[]).unwrap()).unwrap();
        let blob = obj
            .symbols
            .iter()
            .find(|s| s.name == "__lamella_str_0")
            .expect("string blob symbol");
        assert!(blob.defined, "the blob is defined in this object");
        assert!(
            obj.symbols.iter().all(|s| s.name != "__lamella_str_1"),
            "identical literals share one blob"
        );
        let at = (blob.value & !1) as usize;
        assert_eq!(
            u32::from_le_bytes(obj.text[at..at + 4].try_into().unwrap()),
            2,
            "the blob leads with the char count"
        );
        let str_relocs: Vec<_> = obj
            .relocations
            .iter()
            .filter(|r| r.kind == lamella_elf::arm::R_ARM_ABS32)
            .collect();
        assert_eq!(str_relocs.len(), 2, "two ldstr pool words, one shared blob");
        for r in &str_relocs {
            assert_eq!(obj.symbols[r.symbol as usize].name, "__lamella_str_0");
        }
    }

    #[test]
    fn lower_object_lowers_float_add_to_aeabi_fadd() {
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::F32, MirType::F32, MirType::F32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::F32,
                            value: 0x41A0_0000,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::F32,
                            value: 0x41B0_0000,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::Binary {
                            op: BinOp::Add,
                            lhs: ValueId(0),
                            rhs: ValueId(1),
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::Convert {
                            value: ValueId(2),
                            kind: ConvKind::Float32ToInt,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(3)))),
            }],
        };
        let obj =
            lamella_elf::read_object(&lower_object(&[main], &["main"], &[]).unwrap()).unwrap();
        assert!(
            obj.symbols
                .iter()
                .any(|s| s.name == "__aeabi_fadd" && !s.defined)
        );
        let code = code_relocations(&obj);
        assert_eq!(code.len(), 1);
        assert_eq!(obj.symbols[code[0].symbol as usize].name, "__aeabi_fadd");
    }

    #[test]
    fn lower_object_lowers_double_to_int_via_aeabi_d2iz() {
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::F64, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::F64,
                            value: 0x4045_0000_0000_0000,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::Float64ToInt,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let obj =
            lamella_elf::read_object(&lower_object(&[main], &["main"], &[]).unwrap()).unwrap();
        assert!(
            obj.symbols
                .iter()
                .any(|s| s.name == "__aeabi_d2iz" && !s.defined),
            "double->int emits an undefined __aeabi_d2iz extern"
        );
        assert!(
            obj.relocations
                .iter()
                .any(|r| obj.symbols[r.symbol as usize].name == "__aeabi_d2iz"),
            "a relocation targets __aeabi_d2iz"
        );
    }

    #[test]
    fn lower_object_lowers_int_to_double_via_aeabi_i2d() {
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::F64),
            value_types: vec![MirType::I32, MirType::F64],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 42,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::Convert {
                            value: ValueId(0),
                            kind: ConvKind::IntToFloat64,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let obj =
            lamella_elf::read_object(&lower_object(&[main], &["main"], &[]).unwrap()).unwrap();
        assert!(
            obj.symbols
                .iter()
                .any(|s| s.name == "__aeabi_i2d" && !s.defined),
            "int->double emits an undefined __aeabi_i2d extern"
        );
    }

    #[test]
    fn lower_object_lowers_ldvirtftn_to_a_vtable_load() {
        let main = Function {
            params: vec![MirType::ObjectRef],
            ret: Some(MirType::I32),
            value_types: vec![MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::VirtualFuncAddr {
                        object: ValueId(0),
                        slot: 2,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert!(lamella_ir::verify(&main).is_ok());
        let obj =
            lamella_elf::read_object(&lower_object(&[main], &["main"], &[]).unwrap()).unwrap();
        assert!(
            obj.symbols.iter().any(|s| s.name == "main" && s.defined),
            "ldvirtftn lowers to a defined function"
        );
        assert!(
            obj.symbols.iter().all(|s| s.defined || s.name.is_empty()),
            "no undefined externs -- the fn-pointer is computed from the object's vtable at run time"
        );
    }

    #[test]
    fn lower_object_lowers_alloc_to_the_runtime_allocator() {
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: lamella_ir::TypeHandle(0),
                        payload_size: 8,
                        ref_offsets: Vec::new().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let obj =
            lamella_elf::read_object(&lower_object(&[main], &["main"], &[]).unwrap()).unwrap();
        assert!(
            obj.symbols
                .iter()
                .any(|s| s.name == "lamella_gc_alloc" && !s.defined),
            "Alloc lowers to a call to the undefined lamella_gc_alloc"
        );
    }

    #[test]
    fn lower_object_lowers_alloc_array_to_the_runtime_allocator() {
        let main = Function {
            params: vec![MirType::I32],
            ret: Some(MirType::F64),
            value_types: vec![MirType::I32, MirType::ObjectRef, MirType::F64],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![
                    (
                        ValueId(1),
                        Inst::AllocArray {
                            handle: lamella_ir::TypeHandle(8),
                            element: None,
                            length: ValueId(0),
                            element_size: 8,
                            element_kind: 8,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::ArrayLoad {
                            array: ValueId(1),
                            index: ValueId(0),
                            element_size: 8,
                            signed: false,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        assert!(lamella_ir::verify(&main).is_ok());
        let obj =
            lamella_elf::read_object(&lower_object(&[main], &["main"], &[]).unwrap()).unwrap();
        assert!(
            obj.symbols
                .iter()
                .any(|s| s.name == "lamella_gc_alloc" && !s.defined),
            "AllocArray lowers to a lamella_gc_alloc call (not the flat-path fixed address)"
        );
    }

    #[test]
    fn lower_object_lowers_alloc_array_md_to_the_runtime_allocator() {
        let n = ValueId;
        let i32c = |v: i64| Inst::ConstInt {
            ty: MirType::I32,
            value: v,
        };
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::ObjectRef,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::I32,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (n(0), i32c(2)),
                    (n(1), i32c(3)),
                    (n(2), i32c(4)),
                    (
                        n(3),
                        Inst::AllocArrayMD {
                            handle: lamella_ir::TypeHandle(1),
                            dims: alloc::vec![n(0), n(1), n(2)].into_boxed_slice(),
                            element_size: 4,
                        },
                    ),
                    (n(4), i32c(1)),
                    (n(5), i32c(2)),
                    (n(6), i32c(3)),
                    (n(7), i32c(42)),
                    (
                        n(8),
                        Inst::ArrayMDStore {
                            array: n(3),
                            indices: alloc::vec![n(4), n(5), n(6)].into_boxed_slice(),
                            value: n(7),
                            element_size: 4,
                        },
                    ),
                    (
                        n(9),
                        Inst::ArrayMDLoad {
                            array: n(3),
                            indices: alloc::vec![n(4), n(5), n(6)].into_boxed_slice(),
                            element_size: 4,
                            signed: false,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(n(9)))),
            }],
        };
        assert!(lamella_ir::verify(&main).is_ok());
        let obj =
            lamella_elf::read_object(&lower_object(&[main], &["main"], &[]).unwrap()).unwrap();
        assert!(
            obj.symbols
                .iter()
                .any(|s| s.name == "lamella_gc_alloc" && !s.defined),
            "AllocArrayMD lowers to a lamella_gc_alloc call on the object path"
        );
    }

    #[test]
    fn lower_object_lowers_array_elem_addr() {
        let main = Function {
            params: vec![MirType::ObjectRef, MirType::I32],
            ret: Some(MirType::ManagedPtr),
            value_types: vec![MirType::ObjectRef, MirType::I32, MirType::ManagedPtr],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![(
                    ValueId(2),
                    Inst::ArrayElemAddr {
                        array: ValueId(0),
                        index: ValueId(1),
                        element_size: 8,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        assert!(lamella_ir::verify(&main).is_ok());
        let obj =
            lamella_elf::read_object(&lower_object(&[main], &["main"], &[]).unwrap()).unwrap();
        assert!(
            obj.symbols.iter().all(|s| s.defined || s.name.is_empty()),
            "ldelema is pure address arithmetic -- no undefined externs"
        );
    }

    #[test]
    fn alloc_carries_a_per_type_descriptor() {
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: lamella_ir::TypeHandle(0),
                        payload_size: 12,
                        ref_offsets: vec![0, 4].into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let literal = |descriptors: &[TypeMeta]| {
            lower_runtime_calls(&main, &mut Vec::new(), descriptors)
                .blocks
                .iter()
                .flat_map(|b| b.insts.clone())
                .find_map(|(_, i)| match i {
                    Inst::TypeDescLiteral { words, vtable, .. } => Some((words, vtable)),
                    _ => None,
                })
                .expect("the alloc emits a TypeDescLiteral")
        };
        let (words, vtable) = literal(&[]);
        assert_eq!(&*words, &[12, 2, 0, 0, 0, 4]);
        assert!(vtable.is_empty());
        let descriptors = alloc::vec![TypeMeta {
            handle: lamella_ir::TypeHandle(0),
            type_tag: 0xABCD,
            vtable: alloc::vec![VtableEntry::Func(3), VtableEntry::Func(5)],
            itable: Vec::new(),
            base: None,
            words: None,
            exported: true,
            full_name: None,
        }];
        let (words, vtable) = literal(&descriptors);
        assert_eq!(&*words, &[12, 2, 0xABCD, 0, 0, 4]);
        assert_eq!(&*vtable, &[3, 5]);
    }

    #[test]
    fn an_array_descriptor_carries_a_vtable_and_the_object_header_still_points_at_word_0() {
        let main = Function {
            params: vec![MirType::I32],
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::I32, MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::AllocArray {
                        handle: lamella_ir::TypeHandle(0x0500_0001),
                        element: None,
                        length: ValueId(0),
                        element_size: 4,
                        element_kind: crate::resolver::ELEMENT_KIND_REFERENCE,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let descriptors = alloc::vec![TypeMeta {
            handle: lamella_ir::TypeHandle(0x0500_0001),
            type_tag: 0,
            vtable: alloc::vec![
                VtableEntry::Func(0),
                VtableEntry::Func(0),
                VtableEntry::Func(0)
            ],
            itable: Vec::new(),
            base: None,
            words: None,
            exported: false,
            full_name: None,
        }];
        let (words, vtable) =
            lower_runtime_calls(&main, &mut Vec::new(), &descriptors)
                .blocks
                .iter()
                .flat_map(|b| b.insts.clone())
                .find_map(|(_, i)| match i {
                    Inst::TypeDescLiteral { words, vtable, .. } => Some((words, vtable)),
                    _ => None,
                })
                .expect("the array allocation emits a TypeDescLiteral");
        assert_eq!(
            words[0],
            ARRAY_DESC_MARK | 1,
            "still the ratified array header"
        );
        assert_eq!(
            &*vtable,
            &[0, 0, 0],
            "an array descriptor must carry System.Array's slots, not an empty vtable"
        );

        let object = lower_object_vtables(&[main], &["main"], &[], &descriptors)
            .expect("the array object lowers");
        let parsed = lamella_elf::read_object(&object).expect("our own object parses");
        let desc = parsed
            .symbols
            .iter()
            .find(|s| s.name.starts_with("__lamella_typedesc_"))
            .expect("the array descriptor is a named symbol");
        let vtable_bytes = 3 * 4;
        let mark_at = desc.value as usize + vtable_bytes;
        assert_eq!(
            u32::from_le_bytes([
                parsed.text[mark_at],
                parsed.text[mark_at + 1],
                parsed.text[mark_at + 2],
                parsed.text[mark_at + 3],
            ]),
            ARRAY_DESC_MARK | 1,
            "the descriptor SYMBOL spans the vtable, so word 0 sits vtable_bytes past it"
        );
        let to_desc = parsed
            .relocations
            .iter()
            .find(|r| r.symbol as usize == parsed.symbols.iter().position(|s| s.name == desc.name).unwrap())
            .expect("the allocation references the descriptor");
        assert_eq!(
            to_desc.addend as usize, vtable_bytes,
            "the allocation must hand the allocator WORD 0 (symbol + vtable_bytes), never the \
             symbol -- a header pointing into the vtable reads a code pointer as a payload size and \
             desynchronizes the collector's walk over every object above it"
        );
    }

    #[test]
    fn lower_object_vtables_emits_a_reldesc_slot_per_virtual() {
        let allocates = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: lamella_ir::TypeHandle(1),
                        payload_size: 4,
                        ref_offsets: Vec::new().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let speak = Function {
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
                        value: 42,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let descriptors = alloc::vec![TypeMeta {
            handle: lamella_ir::TypeHandle(1),
            type_tag: 0x1234,
            vtable: alloc::vec![VtableEntry::Func(1)],
            itable: Vec::new(),
            base: None,
            words: None,
            exported: true,
            full_name: None,
        }];
        let bytes =
            lower_object_vtables(&[allocates, speak], &["f0", "f1"], &[], &descriptors).unwrap();
        let obj = lamella_elf::read_object(&bytes).unwrap();
        let reldesc: Vec<&lamella_elf::ParsedRelocation> = obj
            .relocations
            .iter()
            .filter(|r| r.kind == lamella_elf::arm::R_LAMELLA_REL_DESC)
            .collect();
        assert_eq!(
            reldesc.len(),
            1,
            "the type's one vtable slot -> one relative-descriptor relocation"
        );
        assert_eq!(reldesc[0].addend, -4);
        assert_eq!(
            obj.symbols[reldesc[0].symbol as usize].name, "f1",
            "the slot targets the virtual method's symbol"
        );
        let plain = lamella_elf::read_object(
            &lower_object(
                &[
                    Function {
                        params: Vec::new(),
                        ret: Some(MirType::ObjectRef),
                        value_types: vec![MirType::ObjectRef],
                        entry: BlockId(0),
                        blocks: vec![BasicBlock {
                            params: Vec::new(),
                            insts: vec![(
                                ValueId(0),
                                Inst::Alloc {
                                    handle: lamella_ir::TypeHandle(1),
                                    payload_size: 4,
                                    ref_offsets: Vec::new().into_boxed_slice(),
                                },
                            )],
                            terminator: Some(Terminator::Return(Some(ValueId(0)))),
                        }],
                    },
                    stub_returning_int(),
                ],
                &["f0", "f1"],
                &[],
            )
            .unwrap(),
        )
        .unwrap();
        assert!(
            !plain
                .relocations
                .iter()
                .any(|r| r.kind == lamella_elf::arm::R_LAMELLA_REL_DESC),
            "without descriptors, no vtable slots are emitted"
        );
    }

    #[test]
    fn lower_object_emits_an_extern_vtable_slot_for_an_inherited_base_virtual() {
        let allocates = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: lamella_ir::TypeHandle(1),
                        payload_size: 4,
                        ref_offsets: Vec::new().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let descriptors = alloc::vec![TypeMeta {
            handle: lamella_ir::TypeHandle(1),
            type_tag: 0x1234,
            vtable: alloc::vec![
                VtableEntry::Extern("System.Object.ToString.".into()),
                VtableEntry::Func(1),
            ],
            itable: Vec::new(),
            base: None,
            words: None,
            exported: true,
            full_name: None,
        }];
        let bytes = lower_object_vtables(
            &[allocates, stub_returning_int()],
            &["f0", "f1"],
            &[],
            &descriptors,
        )
        .unwrap();
        let obj = lamella_elf::read_object(&bytes).unwrap();
        let slot_target = |addend: i32| {
            obj.relocations
                .iter()
                .find(|r| r.kind == lamella_elf::arm::R_LAMELLA_REL_DESC && r.addend == addend)
                .map(|r| &obj.symbols[r.symbol as usize])
                .expect("the vtable slot's relocation exists")
        };
        let inherited = slot_target(-4);
        assert_eq!(inherited.name, "System.Object.ToString.");
        assert!(
            !inherited.defined,
            "the inherited slot's implementation is UNDEFINED here -- the linker resolves it \
             against the library object exporting the referenced method"
        );
        assert_eq!(slot_target(-8).name, "f1");
    }

    #[test]
    fn lower_object_typedescaddr_shares_the_alloc_canonical_descriptor() {
        let handle = lamella_ir::TypeHandle(0x0200_0005);
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::Alloc {
                            handle,
                            payload_size: 8,
                            ref_offsets: alloc::vec![4].into_boxed_slice(),
                        },
                    ),
                    (ValueId(1), Inst::TypeDescAddr { handle }),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        let descriptors = alloc::vec![TypeMeta {
            handle,
            type_tag: 0xAB,
            vtable: Vec::new(),
            itable: Vec::new(),
            base: None,
            words: None,
            exported: true,
            full_name: None,
        }];
        let bytes = lower_object_vtables(&[func], &["f0"], &[], &descriptors).unwrap();
        let obj = lamella_elf::read_object(&bytes).unwrap();
        let desc_name = alloc::format!("{}{}", lamella_elf::TYPE_DESC_PREFIX, handle.0);
        let descs: Vec<&lamella_elf::ParsedSymbol> = obj
            .symbols
            .iter()
            .filter(|s| s.name == desc_name && s.defined)
            .collect();
        assert_eq!(descs.len(), 1, "the alloc and the type-test share ONE descriptor");
        assert_eq!(descs[0].size, 28, "the alloc's richer header wins the dedup");
        let desc_index = obj.symbols.iter().position(|s| s.name == desc_name).unwrap() as u32;
        let refs = obj
            .relocations
            .iter()
            .filter(|r| r.kind == lamella_elf::arm::R_ARM_ABS32 && r.symbol == desc_index)
            .count();
        assert!(
            refs >= 2,
            "both the alloc and the TypeDescAddr reference the canonical descriptor (got {refs})"
        );
    }

    #[test]
    fn lower_object_lays_the_base_ptr_chain_and_synthesizes_ancestors() {
        let derived = lamella_ir::TypeHandle(0x0200_0004);
        let mid = lamella_ir::TypeHandle(0x0200_0003);
        let base = lamella_ir::TypeHandle(0x0200_0002);
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: derived,
                        payload_size: 4,
                        ref_offsets: Vec::new().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let descriptors = alloc::vec![
            TypeMeta { handle: derived, type_tag: 0xD1, vtable: Vec::new(), itable: Vec::new(), base: Some(mid), words: None,
                exported: true,
                full_name: None,
            },
            TypeMeta { handle: mid, type_tag: 0xD2, vtable: Vec::new(), itable: Vec::new(), base: Some(base), words: None,
                exported: true,
                full_name: None,
            },
            TypeMeta { handle: base, type_tag: 0xD3, vtable: Vec::new(), itable: Vec::new(), base: None, words: None,
                exported: true,
                full_name: None,
            },
        ];
        let bytes = lower_object_vtables(&[func], &["f0"], &[], &descriptors).unwrap();
        let obj = lamella_elf::read_object(&bytes).unwrap();
        let name = |h: lamella_ir::TypeHandle| alloc::format!("{}{}", lamella_elf::TYPE_DESC_PREFIX, h.0);
        for h in [derived, mid, base] {
            assert!(
                obj.symbols.iter().any(|s| s.name == name(h) && s.defined),
                "descriptor for {:#x} (an allocated type or its synthesized ancestor) is laid",
                h.0
            );
        }
        let chain: Vec<&str> = obj
            .relocations
            .iter()
            .filter(|r| r.kind == lamella_elf::arm::R_LAMELLA_REL_DESC)
            .map(|r| obj.symbols[r.symbol as usize].name.as_str())
            .collect();
        assert_eq!(chain.len(), 2, "Derived->Mid and Mid->Base, and Base->Object terminates at 0");
        assert!(chain.contains(&name(mid).as_str()), "Derived's base_ptr targets Mid");
        assert!(chain.contains(&name(base).as_str()), "Mid's base_ptr targets Base");
    }

    /// A descriptor carries its type's NAME after the itable, and `Object.ToString()` reads it there.
    ///
    /// Three things are pinned, because each fails silently on its own. The word is UNCONDITIONAL
    /// (a nameless type gets a zero), so a reader never has to know which way the knob was thrown.
    /// It is a DIFF from the descriptor's own address, so `--gc-sections` moving the descriptor
    /// leaves it correct -- an absolute address would be right until the first trim. And the name is
    /// laid INSIDE the descriptor's symbol, which is what makes that diff safe: the collector copies
    /// a symbol whole, so the two ends cannot be separated.
    #[test]
    fn a_descriptor_carries_its_type_name_after_the_itable() {
        let handle = lamella_ir::TypeHandle(0x0200_0007);
        let descriptors = alloc::vec![TypeMeta {
            handle,
            type_tag: 0xAB,
            vtable: Vec::new(),
            itable: Vec::new(),
            base: None,
            words: None,
            exported: true,
            full_name: Some(alloc::boxed::Box::from("Ns.Square")),
        }];
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle,
                        payload_size: 4,
                        ref_offsets: Vec::new().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let bytes = lower_object_vtables(&[func], &["f0"], &[], &descriptors).unwrap();
        let obj = lamella_elf::read_object(&bytes).unwrap();
        let desc = obj
            .symbols
            .iter()
            .find(|s| s.name == alloc::format!("{}{}", lamella_elf::TYPE_DESC_PREFIX, handle.0))
            .expect("the descriptor is laid");
        let name_word_at = desc.value as usize + 4 * 4 + 4;
        let word = u32::from_le_bytes(
            obj.text[name_word_at..name_word_at + 4]
                .try_into()
                .expect("four bytes"),
        );
        if cfg!(feature = "strip-type-names") {
            assert_eq!(word, 0, "a stripped build lays the name word as absent, not as nothing");
            assert_eq!(desc.size, 4 * 4 + 4 + 4, "vtable 0 + words 4 + itable 1 + name 1");
            return;
        }
        let blob = crate::stringgen::string_blob_bytes(
            &"Ns.Square".encode_utf16().collect::<Vec<u16>>(),
        )
        .expect("an ASCII name encodes in every tier");
        let at = obj
            .text
            .windows(blob.len())
            .position(|w| w == blob.as_slice())
            .expect("the name blob is laid in the object");
        assert!(
            (desc.value as usize) < at && at < (desc.value + desc.size) as usize,
            "the name blob must lie inside the descriptor's own symbol span"
        );
        assert_ne!(word, 0, "a named type's name word must not read as absent");
        assert_eq!(
            desc.value as usize + word as usize,
            at,
            "the name word is the blob's offset FROM the descriptor words, not its address"
        );
    }

    #[test]
    fn lower_object_emits_the_itable_for_interface_dispatch() {
        let handle = lamella_ir::TypeHandle(0x0200_0006);
        let func = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle,
                        payload_size: 4,
                        ref_offsets: Vec::new().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let descriptors = alloc::vec![TypeMeta {
            handle,
            type_tag: 0x55,
            vtable: Vec::new(),
            itable: alloc::vec![(0xCAFE, VtableEntry::Func(1))],
            base: None,
            words: None,
            exported: true,
            full_name: None,
        }];
        let bytes =
            lower_object_vtables(&[func, stub_returning_int()], &["f0", "f1"], &[], &descriptors)
                .unwrap();
        let obj = lamella_elf::read_object(&bytes).unwrap();
        let desc = obj
            .symbols
            .iter()
            .find(|s| s.name == alloc::format!("{}{}", lamella_elf::TYPE_DESC_PREFIX, handle.0))
            .expect("the descriptor is laid");
        assert_eq!(desc.size, 32, "the descriptor symbol spans vtable + words + itable + name");
        let f1 = obj.symbols.iter().position(|s| s.name == "f1").unwrap() as u32;
        let slot = obj
            .relocations
            .iter()
            .find(|r| r.kind == lamella_elf::arm::R_LAMELLA_REL_DESC && r.symbol == f1)
            .expect("the itable method slot's relocation targets the implementation");
        assert_eq!(slot.addend, 24);
    }

    fn stub_returning_int() -> Function {
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
                        value: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        }
    }

    #[test]
    fn lower_object_emits_the_gc_stackmap_symbol() {
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
                            handle: lamella_ir::TypeHandle(0),
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
        let g = Function {
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
                        value: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let (_, maps) = lower_module_gc_mapped(&[main.clone(), g.clone()], 0x1000).unwrap();
        assert!(
            maps.0.iter().any(|e| !e.ref_offsets.is_empty()),
            "the live ObjectRef `o` is a root across the call to g"
        );
        let obj = lamella_elf::read_object(&lower_object(&[main, g], &["main", "g"], &[]).unwrap())
            .unwrap();
        let section = obj
            .sections
            .iter()
            .find(|s| s.name == lamella_elf::STACKMAP_GCMAP_SECTION)
            .expect("the GC stack-map fragments are emitted");
        assert!(
            section.data.len() > 8,
            "the fragment carries a function name and at least one safepoint"
        );
        assert!(
            !obj.symbols.iter().any(|s| s.name == "__lamella_gc_stackmaps"),
            "the whole-program map is synthesized by lamella-link, not by the backend"
        );

        for (function, entries) in decode_gcmap_fragments(&section.data) {
            let size = obj
                .symbols
                .iter()
                .find(|s| s.name == function && s.kind == lamella_elf::SymbolType::Func)
                .map(|s| s.size)
                .unwrap_or_else(|| panic!("fragment names a function symbol: {function}"));
            for rel_pc in entries {
                assert!(
                    rel_pc < size,
                    "{function}: safepoint at +{rel_pc} is outside its {size}-byte extent"
                );
            }
        }
    }

    /// Checking "is the safepoint inside its function" against a two-function toy passes under BOTH
    /// the fix and the defect, because a small label id is also a plausible small offset. **A LABEL
    /// ID IS A COUNTER GLOBAL TO THE ENCODER; AN OFFSET IS FUNCTION-LOCAL.** So the place they must
    /// diverge is a SMALL function that lowers LATE: by then the label counter has run far past
    /// anything that could be an offset into it.
    ///
    /// `big` burns ~40 labels on a block chain; `small` then has one safepoint a few bytes in. Under
    /// the raw-label defect `small`'s "return address" is a label index in the forties and its extent
    /// is a couple of dozen bytes, so the range check fires. Under the fix it is the real offset.
    #[test]
    fn a_late_small_functions_safepoint_is_an_offset_and_not_a_label_id() {
        const CHAIN: usize = 40;
        let mut blocks: Vec<BasicBlock> = Vec::new();
        for i in 0..CHAIN {
            blocks.push(BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(i as u32),
                    Inst::ConstInt {
                        ty: MirType::I32,
                        value: i as i64,
                    },
                )],
                terminator: Some(Terminator::Jump {
                    target: BlockId(i as u32 + 1),
                    args: Vec::new(),
                }),
            });
        }
        blocks.push(BasicBlock {
            params: Vec::new(),
            insts: Vec::new(),
            terminator: Some(Terminator::Return(Some(ValueId(0)))),
        });
        let big = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32; CHAIN],
            entry: BlockId(0),
            blocks,
        };
        let small = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: lamella_ir::TypeHandle(0),
                        payload_size: 4,
                        ref_offsets: Vec::new().into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let obj = lamella_elf::read_object(
            &lower_object(&[big, small], &["big", "small"], &[]).unwrap(),
        )
        .unwrap();
        let section = obj
            .sections
            .iter()
            .find(|s| s.name == lamella_elf::STACKMAP_GCMAP_SECTION)
            .expect("the GC stack-map fragments are emitted");
        let fragments = decode_gcmap_fragments(&section.data);
        let (_, pcs) = fragments
            .iter()
            .find(|(name, _)| name == "small")
            .expect("the late function contributes a fragment");
        let size = obj
            .symbols
            .iter()
            .find(|s| s.name == "small" && s.kind == lamella_elf::SymbolType::Func)
            .map(|s| s.size)
            .expect("`small` is a function symbol");
        assert!(!pcs.is_empty(), "an allocation is a safepoint");
        for &rel_pc in pcs {
            assert!(
                rel_pc < size,
                "`small`: safepoint at +{rel_pc} is outside its {size}-byte extent -- \
                 that is a LABEL ID, not a return address"
            );
        }
    }

    /// `(function name, relative safepoint offsets)` per fragment in a `.lamella_gcmap` section --
    /// the test-side reader for the layout documented on `lamella_elf::STACKMAP_GCMAP_SECTION`.
    fn decode_gcmap_fragments(data: &[u8]) -> Vec<(String, Vec<u32>)> {
        let rd = |at: usize| u32::from_le_bytes(data[at..at + 4].try_into().unwrap());
        let mut out = Vec::new();
        let mut at = 0usize;
        while at < data.len() {
            let name_len = rd(at) as usize;
            let name = String::from_utf8(data[at + 4..at + 4 + name_len].to_vec()).unwrap();
            at = (at + 4 + name_len).next_multiple_of(4);
            let count = rd(at) as usize;
            at += 4;
            let mut pcs = Vec::with_capacity(count);
            for _ in 0..count {
                pcs.push(rd(at));
                let tail_len = rd(at + 4) as usize;
                at = (at + 8 + tail_len).next_multiple_of(4);
            }
            out.push((name, pcs));
        }
        out
    }

    #[test]
    fn register_path_call_gets_a_frame_walk_entry() {
        let h = Function {
            params: Vec::new(),
            ret: None,
            value_types: vec![MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Call {
                        callee: 1,
                        args: Vec::new(),
                    },
                )],
                terminator: Some(Terminator::Return(None)),
            }],
        };
        let g = Function {
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
                        value: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        assert!(
            matches!(prepare(&h).unwrap(), Assignment::Registers { saved: 0, .. }),
            "h takes the register path (nothing live across the call)"
        );
        let (_, maps) = lower_module_gc_mapped(&[h, g], 0x1000).unwrap();
        let entry = maps
            .0
            .iter()
            .find(|e| e.ref_offsets.is_empty() && e.frame_size == 0)
            .expect("the register-path call site has a frame-walk entry");
        assert_eq!(
            entry.saved_bytes, 4,
            "the pushed LR above the register frame, no callee-saved"
        );
    }

    #[test]
    fn invoke_delegate_roots_its_reloaded_inputs() {
        let f = Function {
            params: vec![MirType::ObjectRef, MirType::ObjectRef],
            ret: Some(MirType::I32),
            value_types: vec![MirType::ObjectRef, MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0), ValueId(1)],
                insts: vec![(
                    ValueId(2),
                    Inst::InvokeDelegate {
                        delegate: ValueId(0),
                        args: vec![ValueId(1)],
                        returns_value: true,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let (_, maps) = lower_module_gc_mapped(&[f], 0x1000).unwrap();
        let entry = maps
            .0
            .iter()
            .find(|e| !e.ref_offsets.is_empty())
            .expect("the InvokeDelegate safepoint records roots");
        assert_eq!(
            entry.ref_offsets.len(),
            2,
            "both the delegate and the ref arg are rooted (reloaded each iteration)"
        );
    }

    #[test]
    fn a_four_argument_delegate_invoke_reserves_an_outgoing_stack_word() {
        let delegate_taking = |n: usize| Function {
            params: core::iter::once(MirType::ObjectRef)
                .chain(core::iter::repeat_n(MirType::I32, n))
                .collect(),
            ret: Some(MirType::I32),
            value_types: core::iter::once(MirType::ObjectRef)
                .chain(core::iter::repeat_n(MirType::I32, n + 1))
                .collect(),
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: (0..=n as u32).map(ValueId).collect(),
                insts: vec![(
                    ValueId(n as u32 + 1),
                    Inst::InvokeDelegate {
                        delegate: ValueId(0),
                        args: (1..=n as u32).map(ValueId).collect(),
                        returns_value: true,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(n as u32 + 1)))),
            }],
        };
        assert_eq!(
            out_args_bytes(&delegate_taking(3)),
            0,
            "a 3-argument delegate spills nothing"
        );
        assert_eq!(
            out_args_bytes(&delegate_taking(4)),
            8,
            "the 4th argument needs a reserved outgoing word"
        );
        assert!(
            lower_module_gc_mapped(&[delegate_taking(4)], 0x1000).is_ok(),
            "a 4-argument delegate invoke lowers"
        );
    }

    #[test]
    fn lower_object_lowers_float_compares_to_aeabi_cmp_helpers() {
        let cmp = |op, lhs, rhs| Inst::Compare { op, lhs, rhs };
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![
                MirType::F32,
                MirType::F32,
                MirType::F64,
                MirType::F64,
                MirType::I32,
                MirType::I32,
                MirType::I32,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::F32,
                            value: 0,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::ConstInt {
                            ty: MirType::F32,
                            value: 0,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::ConstInt {
                            ty: MirType::F64,
                            value: 0,
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::ConstInt {
                            ty: MirType::F64,
                            value: 0,
                        },
                    ),
                    (ValueId(4), cmp(CmpOp::SignedLt, ValueId(0), ValueId(1))),
                    (ValueId(5), cmp(CmpOp::Ne, ValueId(0), ValueId(1))),
                    (ValueId(6), cmp(CmpOp::SignedLt, ValueId(2), ValueId(3))),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(4)))),
            }],
        };
        let obj =
            lamella_elf::read_object(&lower_object(&[main], &["main"], &[]).unwrap()).unwrap();
        let has = |n: &str| obj.symbols.iter().any(|s| s.name == n && !s.defined);
        assert!(has("__aeabi_fcmplt"), "f32 < -> fcmplt");
        assert!(has("__aeabi_fcmpeq"), "f32 != -> fcmpeq (inverted)");
        assert!(has("__aeabi_dcmplt"), "f64 < -> dcmplt");
        assert_eq!(code_relocations(&obj).len(), 3);
    }

    #[test]
    fn lower_object_handles_branch_relaxation() {
        let leaf = |v: i64| Function {
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
                        value: v,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let main = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Call {
                        callee: 2,
                        args: Vec::new(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let big = {
            const N: u32 = 200;
            let mut value_types = vec![MirType::I32];
            let mut adds = Vec::new();
            let mut prev = ValueId(0);
            for i in 1..=N {
                value_types.push(MirType::I32);
                adds.push((
                    ValueId(i),
                    Inst::Binary {
                        op: BinOp::Add,
                        lhs: prev,
                        rhs: ValueId(0),
                    },
                ));
                prev = ValueId(i);
            }
            Function {
                params: Vec::new(),
                ret: Some(MirType::I32),
                value_types,
                entry: BlockId(0),
                blocks: vec![
                    BasicBlock {
                        params: Vec::new(),
                        insts: vec![(
                            ValueId(0),
                            Inst::ConstInt {
                                ty: MirType::I32,
                                value: 1,
                            },
                        )],
                        terminator: Some(Terminator::Branch {
                            cond: ValueId(0),
                            if_true: BlockId(1),
                            true_args: Vec::new(),
                            if_false: BlockId(2),
                            false_args: Vec::new(),
                        }),
                    },
                    BasicBlock {
                        params: Vec::new(),
                        insts: adds,
                        terminator: Some(Terminator::Return(Some(prev))),
                    },
                    BasicBlock {
                        params: Vec::new(),
                        insts: Vec::new(),
                        terminator: Some(Terminator::Return(Some(ValueId(0)))),
                    },
                ],
            }
        };
        let obj = lamella_elf::read_object(
            &lower_object(&[main, big, leaf(42)], &["main", "big", "answer"], &[])
                .expect("a relaxing module lowers (no longer rejected)"),
        )
        .unwrap();
        let answer = obj.symbols.iter().find(|s| s.name == "answer").unwrap();
        let off = (answer.value & !1) as usize;
        assert_eq!(
            &obj.text[off..off + 2],
            &[0x2A, 0x20],
            "answer's post-relaxation offset must point to `movs r0, #42`"
        );
    }

    #[test]
    fn lower_object_islands_a_literal_heavy_function_past_the_pool_reach() {
        const N: i64 = 400;
        let mut value_types = vec![MirType::I32];
        let mut insts = vec![(
            ValueId(0),
            Inst::ConstInt {
                ty: MirType::I32,
                value: 0,
            },
        )];
        let mut next = 1u32;
        let mut acc = ValueId(0);
        for i in 0..N {
            let c = ValueId(next);
            next += 1;
            value_types.push(MirType::I32);
            insts.push((
                c,
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: 100_003 + i * 7919,
                },
            ));
            let sum = ValueId(next);
            next += 1;
            value_types.push(MirType::I32);
            insts.push((
                sum,
                Inst::Binary {
                    op: BinOp::Add,
                    lhs: acc,
                    rhs: c,
                },
            ));
            acc = sum;
        }
        let big = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types,
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts,
                terminator: Some(Terminator::Return(Some(acc))),
            }],
        };
        let bytes = lower_object(&[big], &["big"], &[])
            .expect("a literal-heavy body islands its pool instead of erroring CodeTooLarge");
        let obj = lamella_elf::read_object(&bytes).unwrap();
        assert!(
            obj.text.len() > 1020,
            "the body must overrun the ~1 KB literal reach for islanding to matter (text {})",
            obj.text.len()
        );
    }

    /// The object's relocations excluding the stack-map records' `func_addr` patches -- most
    /// object tests assert the CODE's relocations, and every safepoint-bearing function now adds
    /// one record ABS32 alongside them.
    fn code_relocations(obj: &lamella_elf::Object) -> Vec<lamella_elf::ParsedRelocation> {
        let record_spans: Vec<(u32, u32)> = obj
            .symbols
            .iter()
            .filter(|s| {
                s.defined
                    && (s.name.starts_with(lamella_elf::STACKMAP_RECORD_PREFIX)
                        || s.name.starts_with(lamella_elf::STACKMAP_STATICS_PREFIX))
            })
            .map(|s| (s.value, s.value + s.size))
            .collect();
        obj.relocations
            .iter()
            .filter(|r| !record_spans.iter().any(|&(a, b)| r.offset >= a && r.offset < b))
            .cloned()
            .collect()
    }

    /// Decodes one encoded stack-map record -- the test-side mirror of the walker's parse.
    fn decode_stackmap_record(bytes: &[u8]) -> (u32, u32, u16, u16, u16, Vec<u16>) {
        let word =
            |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
        let half = |o: usize| u16::from_le_bytes([bytes[o], bytes[o + 1]]);
        let count = half(14) as usize;
        let roots = (0..count).map(|i| half(16 + 2 * i)).collect();
        (word(0), word(4), half(8), half(10), half(12), roots)
    }

    #[test]
    fn stackmap_record_encoding_round_trips() {
        let roots = [
            5 | (STACKMAP_KIND_OBJECT_REF << 14),
            9 | (STACKMAP_KIND_MANAGED_PTR << 14),
            2 | (STACKMAP_KIND_PINNED << 14),
        ];
        let mut bytes = Vec::new();
        encode_stackmap_record(&mut bytes, 0, 0x84, STACKMAP_MODE_METHOD_SLOTS, 7, 6, &roots);
        assert_eq!(bytes.len() % 4, 0, "records stay word-aligned");
        let (func_addr, code_size, mode, frame_words, ret_lr_word, decoded) =
            decode_stackmap_record(&bytes);
        assert_eq!(func_addr, 0);
        assert_eq!(code_size, 0x84);
        assert_eq!(mode, STACKMAP_MODE_METHOD_SLOTS);
        assert_eq!(frame_words, 7);
        assert_eq!(ret_lr_word, 6);
        assert_eq!(decoded, roots);
    }

    /// The sec-2.3 memory-homing invariant, pinned: a function keeping an ObjectRef LIVE ACROSS a
    /// call must take the fully-spilled path (the METHOD_SLOTS record enumerates SLOTS, so a ref
    /// surviving a safepoint in a callee-saved register would be invisible to the collector). The
    /// `any_value_live_across_call` gate in `prepare` is what enforces it; this test is the
    /// tripwire should a register-path optimization ever loosen that gate.
    #[test]
    fn a_ref_live_across_a_call_forces_the_spilled_path() {
        let live_across = Function {
            params: vec![MirType::ObjectRef],
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![(
                    ValueId(1),
                    Inst::Call {
                        callee: 1,
                        args: Vec::new(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        assert!(matches!(prepare(&live_across), Ok(Assignment::Spilled)));
        let with_ptr = Function {
            params: vec![MirType::ManagedPtr],
            ret: None,
            value_types: vec![MirType::ManagedPtr],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: Vec::new(),
                terminator: Some(Terminator::Return(None)),
            }],
        };
        assert!(matches!(prepare(&with_ptr), Ok(Assignment::Spilled)));
    }

    /// WELD: every live root the per-safepoint analysis reports must appear among the METHOD_SLOTS
    /// record's slots at the same offset -- the record enumerates a superset (all ref slots), and
    /// this keeps the two computations (the lowering's `record_safepoint` and the record builder's
    /// `method_record_roots`) from drifting apart.
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
                            handle: lamella_ir::TypeHandle(0),
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
                            handle: lamella_ir::TypeHandle(0),
                            payload_size: 4,
                            ref_offsets: Vec::new().into_boxed_slice(),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let record_slots: Vec<u16> = method_record_roots(&func, &[])
            .iter()
            .map(|r| (r & 0x3FFF) * 4)
            .collect();
        let (offsets, _) = spilled_slot_offsets(&func);
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

    /// A big-struct (sret) return reserves the result-pointer word in the SHARED
    /// `spilled_slot_offsets`, below the first value slot -- so `method_record_roots` reports a ref
    /// root at the SAME offset `lower_spilled_into` stores it at. Before the fix the sret word was
    /// reserved only in the lowering (which remapped value offsets `+4`), while the record builder
    /// read the unshifted offsets, skewing a big-struct-return spilled function's stack-map roots by
    /// 4 bytes -- a latent moving-GC mis-walk. This pins the two back in lockstep.
    #[test]
    fn sret_return_shifts_value_slots_for_the_record_builder() {
        let big = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 8,
            refs: lamella_ir::RefWords::NONE,
        };
        let func = Function {
            params: Vec::new(),
            ret: Some(big),
            value_types: vec![MirType::ObjectRef, big],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::Alloc {
                            handle: lamella_ir::TypeHandle(0),
                            payload_size: 4,
                            ref_offsets: Vec::new().into_boxed_slice(),
                        },
                    ),
                    (ValueId(1), Inst::InitStruct),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        assert!(matches!(prepare(&func), Ok(Assignment::Spilled)));
        let (offsets, _) = spilled_slot_offsets(&func);
        assert_eq!(
            offsets[0],
            out_args_bytes(&func) + 4,
            "value 0 must sit above the reserved sret word"
        );
        let record_slots: Vec<u16> = method_record_roots(&func, &[])
            .iter()
            .map(|r| (r & 0x3FFF) * 4)
            .collect();
        assert!(
            record_slots.contains(&offsets[0]),
            "v0's ref root must be recorded at its real slot {} (record {record_slots:?})",
            offsets[0]
        );
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
                            handle: lamella_ir::TypeHandle(0),
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
        let g = Function {
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
                        value: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let obj =
            lamella_elf::read_object(&lower_object(&[main, g], &["main", "g"], &[]).unwrap())
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
        let (_, code_size, mode, frame_words, ret_lr_word, roots) = decode_stackmap_record(bytes);
        assert_eq!(mode, STACKMAP_MODE_METHOD_SLOTS);
        assert!(code_size > 0);
        assert_eq!(ret_lr_word, frame_words - 1, "LR is pushed first (highest)");
        assert_eq!(roots, vec![STACKMAP_KIND_OBJECT_REF << 14]);
        let main_index = obj
            .symbols
            .iter()
            .position(|s| s.name == "main")
            .expect("main symbol") as u32;
        assert!(
            obj.relocations.iter().any(|r| r.offset == rec.value
                && r.symbol == main_index
                && r.kind == lamella_elf::arm::R_ARM_ABS32),
            "the func_addr word carries an ABS32 to the function symbol"
        );
    }

    #[test]
    fn a_reference_cell_is_recorded_as_an_object_ref_root() {
        let main = Function {
            params: Vec::new(),
            ret: None,
            value_types: vec![
                MirType::ValueType {
                    handle: crate::stackmaps::REF_CELL_HANDLE,
                    size: 4,
                    refs: lamella_ir::RefWords::at_word(0),
                },
                MirType::I32,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (ValueId(0), Inst::InitStruct),
                    (
                        ValueId(1),
                        Inst::Call {
                            callee: 1,
                            args: Vec::new(),
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(None)),
            }],
        };
        let g = Function {
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
                        value: 0,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let obj = lamella_elf::read_object(&lower_object(&[main, g], &["main", "g"], &[]).unwrap())
            .unwrap();
        let rec = obj
            .symbols
            .iter()
            .find(|s| s.name == "__lamella_smrec_main")
            .expect("main gets a method record");
        let bytes = &obj.text[rec.value as usize..(rec.value + rec.size) as usize];
        let (_, _, mode, _, _, roots) = decode_stackmap_record(bytes);
        assert_eq!(mode, STACKMAP_MODE_METHOD_SLOTS);
        assert_eq!(roots.len(), 1, "the ref cell is the only reference-bearing slot");
        assert_eq!(
            roots[0] >> 14,
            STACKMAP_KIND_OBJECT_REF,
            "the REF_CELL_HANDLE cell must be enumerated as an ObjectRef root, not skipped"
        );
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
                            kind: lamella_ir::ConvKind::RefToInt,
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
        let externs = [alloc::string::String::from("lamella_thread_recv_poll")];
        let roots = method_record_roots(&seam, &externs);
        assert_eq!(roots, vec![STACKMAP_KIND_PINNED << 14]);
        let externs = [alloc::string::String::from("lamella_console_write")];
        let roots = method_record_roots(&seam, &externs);
        assert_eq!(roots, vec![STACKMAP_KIND_OBJECT_REF << 14]);
    }

    #[test]
    fn lower_object_vtables_statics_emits_the_mode2_record() {
        let f = Function {
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
                        value: 42,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let statics = AssemblyStatics {
            suffix: alloc::string::String::from("0badf00d"),
            region_bytes: 12,
            roots: vec![
                STACKMAP_KIND_MANAGED_PTR << 14,
                2 | (STACKMAP_KIND_OBJECT_REF << 14),
            ],
        };
        let obj = lamella_elf::read_object(
            &lower_object_vtables_statics(&[f], &["main"], &[], &[], &statics, &DescQualifiers::default())
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
        assert!(!target.defined, "the linker places the region");
        assert_eq!(target.size, 12, "st_size carries the region size");
    }

    #[test]
    fn a_64_bit_static_moves_both_words() {
        let f = Function {
            params: Vec::new(),
            ret: Some(MirType::I64),
            value_types: vec![MirType::I64, MirType::I32, MirType::I64],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I64,
                            value: 9_000_000_000,
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
                    (
                        ValueId(2),
                        Inst::StaticLoad {
                            owner: StaticOwner::Own,
                            offset: 4,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let bytes = lower(&f).expect("a 64-bit static lowers");
        let halfwords: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert!(
            halfwords.contains(&0x6041),
            "the store must write the high word at region+4"
        );
        assert!(
            halfwords.contains(&0x6841),
            "the load must read the high word from region+4"
        );
    }

    #[test]
    fn a_typespec_handle_never_aliases_a_bit_tested_symbol_flag() {
        const TYPE_SPEC_TABLE: u32 = 0x1B;
        let handles: [u32; 9] = [
            0x0000_0001,
            0x0100_0001,
            0x0200_0001,
            0x0300_0001,
            0x0400_0001,
            0x0500_0001,
            0x0600_0001,
            0x0700_0001,
            (TYPE_SPEC_TABLE << 24) | 1,
        ];
        for handle in handles {
            let sym = DESC_SYMBOL_FLAG | handle;
            assert_ne!(
                sym, EH_TAG_SYMBOL_FLAG,
                "a descriptor reference must never EQUAL the EH word's symbol (handle {handle:#x})"
            );
            assert_ne!(
                sym >> 24,
                STATICS_BASE_SYMBOL_FLAG >> 24,
                "a descriptor reference must differ from a statics base in its TOP BYTE, which is \
                 the only test that separates them (handle {handle:#x})"
            );
            assert_eq!(
                sym & EXTERN_SYMBOL_FLAG,
                0,
                "a type handle must not reach bit 31 (handle {handle:#x})"
            );
            assert_eq!(
                sym & STRING_SYMBOL_FLAG,
                0,
                "a type handle must not reach bit 29, or a descriptor reference would bit-test as a \
                 string blob (handle {handle:#x})"
            );
        }
        for payload in [0u32, 1, 16, 0x00ff_ffff] {
            let sym = STATICS_BASE_SYMBOL_FLAG | payload;
            assert_eq!(
                sym & DESC_SYMBOL_FLAG,
                0,
                "a statics base must not bit-test as a descriptor (payload {payload:#x})"
            );
            assert_ne!(sym, EH_TAG_SYMBOL_FLAG);
        }
    }

    #[test]
    fn object_statics_split_eh_word_from_the_assembly_region() {
        let f = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 7,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::StaticStore {
                            owner: StaticOwner::Own,
                            offset: 0,
                            value: ValueId(0),
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::StaticLoad {
                            owner: StaticOwner::Own,
                            offset: 8,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let statics = AssemblyStatics {
            suffix: alloc::string::String::from("11223344"),
            region_bytes: 12,
            roots: Vec::new(),
        };
        let obj = lamella_elf::read_object(
            &lower_object_vtables_statics(&[f], &["main"], &[], &[], &statics, &DescQualifiers::default())
                .unwrap(),
        )
        .unwrap();
        let target = |r: &lamella_elf::ParsedRelocation| obj.symbols[r.symbol as usize].name.clone();
        assert!(
            obj.relocations.iter().any(|r| {
                r.kind == lamella_elf::arm::R_ARM_ABS32
                    && target(r) == lamella_elf::EH_TAG_SYMBOL
                    && r.addend == 0
            }),
            "the offset-0 store references the shared EH word"
        );
        assert!(
            obj.relocations.iter().any(|r| {
                r.kind == lamella_elf::arm::R_ARM_ABS32
                    && target(r) == "__lamella_statics_11223344"
                    && r.addend == 8
            }),
            "the field access references the assembly's own region + slot addend"
        );
        let words: Vec<u32> = obj
            .text
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert!(
            !words.contains(&(STATIC_FIELD_BASE + 8)),
            "the linked path retired the fixed-address statics idiom"
        );
    }

    #[test]
    fn cross_assembly_statics_reference_the_owners_region() {
        let f = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32, MirType::I32, MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::StaticLoad {
                            owner: StaticOwner::Reference(0),
                            offset: 4,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::StaticStore {
                            owner: StaticOwner::Reference(1),
                            offset: 8,
                            value: ValueId(0),
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::StaticLoad {
                            owner: StaticOwner::Own,
                            offset: 4,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(2)))),
            }],
        };
        let statics = AssemblyStatics {
            suffix: alloc::string::String::from("11223344"),
            region_bytes: 8,
            roots: Vec::new(),
        };
        let qualifiers = DescQualifiers {
            string: None,
            own: None,
            references: alloc::vec![
                alloc::string::String::from("aaaa0001"),
                alloc::string::String::from("bbbb0002"),
            ],
        };
        let obj = lamella_elf::read_object(
            &lower_object_vtables_statics(&[f], &["main"], &[], &[], &statics, &qualifiers)
                .unwrap(),
        )
        .unwrap();
        let lands = |name: &str, addend: i32| {
            obj.relocations.iter().any(|r| {
                r.kind == lamella_elf::arm::R_ARM_ABS32
                    && obj.symbols[r.symbol as usize].name == name
                    && r.addend == addend
            })
        };
        assert!(
            lands("__lamella_statics_aaaa0001", 4),
            "the reference-0 load lands on ITS owner's region + slot addend"
        );
        assert!(
            lands("__lamella_statics_bbbb0002", 8),
            "the reference-1 store lands on ITS owner's region + slot addend"
        );
        assert!(
            lands("__lamella_statics_11223344", 4),
            "an own access still lands on this assembly's region"
        );
        for name in ["__lamella_statics_aaaa0001", "__lamella_statics_bbbb0002"] {
            let sym = obj.symbols.iter().find(|s| s.name == name).unwrap();
            assert!(!sym.defined, "the linker places the owner's region");
            assert_eq!(sym.size, 0, "the owner's own object carries the st_size channel");
        }
    }

    #[test]
    fn flat_path_rejects_a_cross_assembly_static() {
        let f = Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::StaticLoad {
                        owner: StaticOwner::Reference(0),
                        offset: 4,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        assert!(matches!(
            lower_module(&[f]),
            Err(LowerError::CallUnsupported)
        ));
    }

    /// A non-leaf function with `n` spilled I32 values (a call keeps it walkable and needs LR).
    fn many_values(n: u32) -> Function {
        let mut insts = Vec::new();
        for i in 0..n {
            insts.push((
                ValueId(i),
                Inst::ConstInt {
                    ty: MirType::I32,
                    value: i64::from(i),
                },
            ));
        }
        insts.push((
            ValueId(n),
            Inst::Call {
                callee: 0,
                args: Vec::new(),
            },
        ));
        Function {
            params: Vec::new(),
            ret: Some(MirType::I32),
            value_types: vec![MirType::I32; n as usize + 1],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts,
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        }
    }

    #[test]
    fn library_report_names_the_dry_run_stubs_and_leaves_bytes_identical() {
        let good = Function {
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
        };
        let bad = many_values(17_000);
        let funcs = [good, bad];
        let names = ["good", "bad"];
        let (bytes, report) =
            lower_object_library_vtables_report(&funcs, &names, &[], &[], None, &DescQualifiers::default(), false)
                .unwrap();
        assert_eq!(
            report,
            vec![(1, LowerError::TooManyValues)],
            "the oversized frame is reported by index with its lowering error"
        );
        let obj = lamella_elf::read_object(&bytes).unwrap();
        let bad_sym = obj.symbols.iter().find(|s| s.name == "bad").unwrap();
        let start = (bad_sym.value & !1) as usize;
        assert_eq!(
            &obj.text[start..start + 2],
            &[0x70, 0x47],
            "the reported method is a bare `bx lr` stub"
        );
        let plain = lower_object_library_vtables(&funcs, &names, &[], &[]).unwrap();
        assert_eq!(plain, bytes);
    }

    #[test]
    fn narrow_field_access_is_width_exact() {
        let f = Function {
            params: vec![MirType::ObjectRef],
            ret: Some(MirType::I32),
            value_types: vec![
                MirType::ObjectRef,
                MirType::I32,
                MirType::I32,
                MirType::I32,
                MirType::I32,
            ],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: vec![ValueId(0)],
                insts: vec![
                    (
                        ValueId(1),
                        Inst::FieldLoadNarrow {
                            base: ValueId(0),
                            offset: 1,
                            size: 1,
                            signed: false,
                        },
                    ),
                    (
                        ValueId(2),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 0,
                        },
                    ),
                    (
                        ValueId(3),
                        Inst::FieldStoreNarrow {
                            base: ValueId(0),
                            offset: 0,
                            value: ValueId(2),
                            size: 1,
                        },
                    ),
                    (
                        ValueId(4),
                        Inst::FieldLoadNarrow {
                            base: ValueId(0),
                            offset: 2,
                            size: 1,
                            signed: true,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(4)))),
            }],
        };
        let obj =
            lamella_elf::read_object(&lower_object(&[f], &["main"], &[]).unwrap()).unwrap();
        let halfwords: Vec<u16> = obj
            .text
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert!(
            halfwords.iter().any(|&h| h & 0xF800 == 0x7800),
            "the loads are LDRB"
        );
        assert!(
            halfwords.iter().any(|&h| h & 0xF800 == 0x7000),
            "the store is STRB -- never a word-wide stomp"
        );
        assert!(
            halfwords.iter().any(|&h| h & 0xFFC0 == 0xB240),
            "the signed byte load sign-extends (SXTB)"
        );
    }

    #[test]
    fn a_reach_failure_keeps_the_site_the_encoder_named() {
        assert_eq!(
            reach_failure(AssembleError::BranchOutOfRange {
                at: 1106,
                kind: RelocKind::ThumbLdrLit8,
            }),
            LowerError::CodeTooLarge {
                site: Some((1106, RelocKind::ThumbLdrLit8)),
            }
        );
        assert_eq!(
            reach_failure(AssembleError::UnencodableOperand),
            LowerError::CodeTooLarge { site: None }
        );
    }

    #[test]
    fn big_frame_past_the_thumb1_reach_lowers() {
        let f = many_values(600);
        let obj =
            lamella_elf::read_object(&lower_object(&[f], &["big"], &[]).unwrap()).unwrap();
        let big = obj.symbols.iter().find(|s| s.name == "big").unwrap();
        assert!(
            big.size > 600 * 2,
            "a real body, not a stub (got {} bytes)",
            big.size
        );
        let rec = obj
            .symbols
            .iter()
            .find(|s| s.name == "__lamella_smrec_big")
            .expect("a safepoint-bearing function gets a record");
        let bytes = &obj.text[rec.value as usize..(rec.value + rec.size) as usize];
        let (_, _, mode, frame_words, _, _) = decode_stackmap_record(bytes);
        assert_eq!(mode, STACKMAP_MODE_METHOD_SLOTS);
        assert!(
            u32::from(frame_words) * 4 > 1020,
            "the record spans the big frame ({frame_words} words)"
        );
    }

    #[test]
    fn the_object_path_emits_dwarf_without_moving_a_byte_of_code() {
        let body = |v: u32| Function {
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
                        value: v as i64,
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let funcs = [body(1), body(2)];
        let names = ["first", "second"];
        let statics = AssemblyStatics {
            suffix: alloc::string::String::from("0badf00d"),
            region_bytes: 0,
            roots: Vec::new(),
        };
        let maps = [
            crate::cil::CilSourceMap(vec![vec![7]]),
            crate::cil::CilSourceMap(vec![vec![7]]),
        ];
        let points = [(7u32, 11u32, 5u32)];
        let sources = [
            crate::debugmap::MethodSource { name: "T.first", file: "t.cs", points: &points },
            crate::debugmap::MethodSource { name: "T.second", file: "t.cs", points: &points },
        ];
        let debug = crate::debugmap::ObjectDebug {
            source_maps: &maps,
            methods: &sources,
            unit_name: "t.cs",
            producer: "test",
        };
        let (with_debug, lines) = lower_object_vtables_statics_debug(
            &funcs,
            &names,
            &[],
            &[],
            &statics,
            &DescQualifiers::default(),
            &debug,
        )
        .unwrap();

        let plain =
            lower_object_vtables_statics(&funcs, &names, &[], &[], &statics, &DescQualifiers::default())
                .unwrap();
        let with_obj = lamella_elf::read_object(&with_debug).unwrap();
        let plain_obj = lamella_elf::read_object(&plain).unwrap();
        assert_eq!(with_obj.text, plain_obj.text, "debug info must not move a byte of code");
        assert!(plain_obj.sections.is_empty(), "the plain build carries no debug sections");
        let names_out: Vec<&str> = with_obj.sections.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names_out, [".debug_line", ".debug_info", ".debug_abbrev"]);
        assert!(
            with_obj
                .sections
                .iter()
                .any(|s| s.name == ".debug_line" && !s.relocations.is_empty()),
            "the line program's addresses come from relocations"
        );

        assert_eq!(lines.len(), 2);
        let obj = &with_obj;
        for (index, name) in names.iter().enumerate() {
            let (offset, table) = &lines[index];
            assert!(!table.0.is_empty(), "{name} has no rows");
            assert!(
                table.0.iter().all(|&(_, cil)| cil == 7),
                "{name}'s rows name the CIL offset they lowered from"
            );
            let sym = obj
                .symbols
                .iter()
                .find(|s| s.name == *name)
                .unwrap_or_else(|| panic!("no {name} symbol"));
            assert_eq!(
                *offset,
                sym.value & !1,
                "{name}'s line-table offset is where the symbol says its code is"
            );
        }
        assert_ne!(lines[0].0, lines[1].0, "the two functions sit at different offsets");
    }

    #[test]
    fn a_flat_path_descriptor_lays_the_ratified_four_word_header() {
        let func = Function {
            params: Vec::new(),
            ret: None,
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle: lamella_ir::TypeHandle(0x0200_0001),
                        payload_size: 12,
                        ref_offsets: alloc::vec![4u32, 8].into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(None)),
            }],
        };
        let bytes = lower_module_gc(&[func], 0x2000_0000).expect("the flat GC path lowers an Alloc");
        let at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let words: Vec<u32> = (0..bytes.len() / 4).map(|i| at(i * 4)).collect();
        let ratified = words
            .windows(6)
            .any(|w| w[0] == 12 && w[1] == 2 && w[3] == 0 && w[4] == 4 && w[5] == 8);
        assert!(
            ratified,
            "no descriptor with the ratified header              [payload@0][nrefs@4][tag@8][base_ptr@12][ref_offsets@16..]"
        );
        let three_word = words
            .windows(5)
            .any(|w| w[0] == 12 && w[1] == 2 && w[3] == 4 && w[4] == 8);
        assert!(
            !three_word,
            "the pre-fix three-word header is still being emitted"
        );
    }

    #[test]
    fn a_flat_path_array_descriptor_lays_the_ratified_array_header() {
        let func = Function {
            params: Vec::new(),
            ret: None,
            value_types: vec![MirType::I32, MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 3,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::AllocArray {
                            handle: lamella_ir::synthetic_array_handle(8),
                            element: None,
                            length: ValueId(0),
                            element_size: 8,
                            element_kind: 8,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(None)),
            }],
        };
        let bytes =
            lower_module_gc(&[func], 0x2000_0000).expect("the flat GC path lowers an AllocArray");
        let at = |i: usize| u32::from_le_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let words: Vec<u32> = (0..bytes.len() / 4).map(|i| at(i * 4)).collect();
        assert!(
            words
                .windows(4)
                .any(|w| w[0] == ARRAY_DESC_MARK | 1 && w[1] == 8 && w[2] == 0 && w[3] == 0),
            "no array descriptor with the ratified header [MARK|rank@0][element_kind@4][tag@8][base_ptr@12]"
        );
        assert!(
            words.windows(4).all(|w| w != [0, 0, 0, 0]),
            "an all-zero (unmarked) array descriptor is still being emitted"
        );
        let marks = words.iter().filter(|&&w| w == ARRAY_DESC_MARK | 1).count();
        assert_eq!(marks, 1, "expected exactly one array descriptor");
    }

    #[test]
    fn an_array_does_not_displace_the_class_descriptor_sharing_its_handle() {
        let handle = lamella_ir::TypeHandle(0x0200_0004);
        let boxed = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![(
                    ValueId(0),
                    Inst::Alloc {
                        handle,
                        payload_size: 4,
                        ref_offsets: alloc::vec![].into_boxed_slice(),
                    },
                )],
                terminator: Some(Terminator::Return(Some(ValueId(0)))),
            }],
        };
        let array = Function {
            params: Vec::new(),
            ret: Some(MirType::ObjectRef),
            value_types: vec![MirType::I32, MirType::ObjectRef],
            entry: BlockId(0),
            blocks: vec![BasicBlock {
                params: Vec::new(),
                insts: vec![
                    (
                        ValueId(0),
                        Inst::ConstInt {
                            ty: MirType::I32,
                            value: 3,
                        },
                    ),
                    (
                        ValueId(1),
                        Inst::AllocArray {
                            handle,
                            element: None,
                            length: ValueId(0),
                            element_size: 4,
                            element_kind: 5,
                        },
                    ),
                ],
                terminator: Some(Terminator::Return(Some(ValueId(1)))),
            }],
        };
        for (funcs, names, order) in [
            (alloc::vec![boxed.clone(), array.clone()], ["f0", "f1"], "class first"),
            (alloc::vec![array.clone(), boxed.clone()], ["f0", "f1"], "array first"),
        ] {
            let obj = lower_object_vtables(&funcs, &names, &[], &[])
                .expect("the object path lowers an Alloc beside an AllocArray");
            let words: Vec<u32> = (0..obj.len() / 4)
                .map(|i| {
                    u32::from_le_bytes([obj[i * 4], obj[i * 4 + 1], obj[i * 4 + 2], obj[i * 4 + 3]])
                })
                .collect();
            assert!(
                words
                    .windows(4)
                    .any(|w| w[0] == 4 && w[1] == 0 && w[2] == 0 && w[3] == 0),
                "{order}: the CLASS descriptor (payload 4, nrefs 0) must survive the shared handle"
            );
            assert!(
                !words
                    .iter()
                    .any(|&w| w & ARRAY_DESC_MARK_MASK == ARRAY_DESC_MARK),
                "{order}: the array must not take over the shared handle's canonical descriptor"
            );
        }
    }

    #[test]
    fn a_synthesized_array_handle_is_distinct_per_element_kind() {
        let kinds = [0u32, 1, 2, 3, 4, 5, 6, 7, 8, 0xFF];
        for (i, &a) in kinds.iter().enumerate() {
            for &b in &kinds[i + 1..] {
                assert_ne!(
                    lamella_ir::synthetic_array_handle(a),
                    lamella_ir::synthetic_array_handle(b),
                    "kinds {a} and {b} collide on one handle"
                );
            }
        }
        for &k in &kinds {
            let handle = lamella_ir::synthetic_array_handle(k).0;
            assert_eq!(handle >> 24, lamella_ir::SYNTHETIC_ARRAY_HANDLE_TABLE);
            assert!(handle < 1 << 27, "handle {handle:#x} reaches the flag bits");
        }
    }
}
