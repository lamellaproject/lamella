//! Lowering the middle IR to ARMv6-M Thumb machine code.

use alloc::boxed::Box;
use alloc::vec::Vec;

use lamella_asm_arm32::{AssembleError, Cond, Encoder, Label, Reg};
use lamella_ir::{BinOp, BlockId, CmpOp, ConvKind, Function, Inst, MirType, Terminator, ValueId};

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
    /// The function plus its literal pool is too large for a literal load to
    /// reach (a constant's pool entry sits more than ~1 KB past the load).
    CodeTooLarge,
    /// The function contains a call, which the single-function lowering cannot
    /// resolve; calls are lowered by the program (module) lowering.
    CallUnsupported,
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
        Inst::Store { address, value } => {
            enc.str_imm(assign(*value), assign(*address), 0)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::Load { address } => {
            enc.ldr_imm(assign(result), assign(*address), 0)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::Convert { value, kind } => {
            if matches!(kind, ConvKind::Float32ToInt | ConvKind::IntToFloat32) {
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
        | Inst::FieldAddr { .. }
        | Inst::CopyStruct { .. } => return Err(LowerError::CallUnsupported),
        Inst::Call { .. }
        | Inst::CallVirtual { .. }
        | Inst::CallInterface { .. }
        | Inst::CastClassScan { .. } => {
            return Err(LowerError::CallUnsupported);
        }
        Inst::SemihostWrite { .. }
        | Inst::WriteInt { .. }
        | Inst::StringLiteral { .. }
        | Inst::StringEquals { .. }
        | Inst::StringConcat { .. }
        | Inst::IntToString { .. }
        | Inst::Alloc { .. }
        | Inst::AllocArray { .. }
        | Inst::ArrayLoad { .. }
        | Inst::ArrayStore { .. }
        | Inst::AllocArray2D { .. }
        | Inst::Array2DLoad { .. }
        | Inst::Array2DStore { .. }
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
        ConvKind::Float32ToInt | ConvKind::IntToFloat32 => Ok(()),
    }
}

/// Loads a 32-bit constant into `reg` -- inline if it fits a `MOVS #imm8`, else from the
/// literal pool.
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

/// Loads call arguments per the AAPCS: each word into a register (`start_reg`..r3, a
/// doubleword even-aligned), then the remainder into an 8-byte-aligned outgoing stack area.
/// Returns the stack bytes reserved, which the caller reclaims (`add sp`) after the `BL`.
fn load_call_args(
    enc: &mut Encoder,
    value_types: &[MirType],
    slot: &impl Fn(ValueId) -> u16,
    args: &[ValueId],
    start_reg: u8,
) -> Result<u16, LowerError> {
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
    let stack_bytes = (stack_used + 7) & !7;
    if stack_bytes > 0 && start_reg != 0 {
        return Err(LowerError::CallUnsupported);
    }
    if stack_bytes > 0 {
        enc.sub_sp(stack_bytes)
            .map_err(|_| LowerError::TooManyValues)?;
    }
    for &(stack_off, a, woff) in &stack_plan {
        enc.ldr_sp(Reg::R3, slot(a) + stack_bytes + woff)
            .map_err(|_| LowerError::TooManyValues)?;
        enc.str_sp(Reg::R3, stack_off)
            .map_err(|_| LowerError::TooManyValues)?;
    }
    for &(r, a, woff) in &reg_plan {
        let dst = Reg::new(r).ok_or(LowerError::CallUnsupported)?;
        enc.ldr_sp(dst, slot(a) + stack_bytes + woff)
            .map_err(|_| LowerError::TooManyValues)?;
    }
    Ok(stack_bytes)
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
    strings: &mut Vec<(Label, Box<[u8]>)>,
    string_blobs: &mut Vec<(Label, Box<[u16]>)>,
    value_types: &[MirType],
    slot: &impl Fn(ValueId) -> u16,
    inst: &Inst,
    result_ty: Option<MirType>,
    func_labels: &[Label],
) -> Result<Option<u32>, LowerError> {
    match inst {
        Inst::ConstInt {
            ty: MirType::I64,
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
            enc.ldr_sp(Reg::R0, a)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R1, a + 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R2, b)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R3, b + 4)
                .map_err(|_| LowerError::TooManyValues)?;
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
            enc.ldr_sp(Reg::R0, slot(*lhs))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R1, slot(*rhs))
                .map_err(|_| LowerError::TooManyValues)?;
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
                enc.ldr_sp(Reg::R0, a)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R1, a + 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R2, b)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R3, b + 4)
                    .map_err(|_| LowerError::TooManyValues)?;
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
                enc.ldr_sp(Reg::R0, m)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R1, m + 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R2, s)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R3, s + 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.subs(Reg::R0, Reg::R0, Reg::R2)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.sbcs(Reg::R1, Reg::R3)
                    .map_err(|_| LowerError::TooManyValues)?;
                materialize_from_flags(enc, Reg::R0, cond)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::Compare { op, lhs, rhs } => {
            enc.ldr_sp(Reg::R0, slot(*lhs))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R1, slot(*rhs))
                .map_err(|_| LowerError::TooManyValues)?;
            materialize_compare(enc, Reg::R0, Reg::R0, Reg::R1, *op)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::Call { callee, args } => {
            let stack_bytes = load_call_args(enc, value_types, slot, args, 0)?;
            let target = *func_labels
                .get(*callee as usize)
                .ok_or(LowerError::CallUnsupported)?;
            enc.bl(target);
            let return_pc = enc.position();
            if stack_bytes > 0 {
                enc.add_sp(stack_bytes)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
            return Ok(Some(return_pc));
        }
        Inst::CallVirtual {
            slot: vtable_slot,
            args,
        } => {
            let receiver = *args.first().ok_or(LowerError::CallUnsupported)?;
            let entry_offset = vtable_slot
                .checked_mul(4)
                .and_then(|x| x.checked_add(4))
                .filter(|&offset| offset <= 255)
                .ok_or(LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R0, slot(receiver))
                .map_err(|_| LowerError::TooManyValues)?;
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
            let stack_bytes = load_call_args(enc, value_types, slot, args, 0)?;
            enc.blx(Reg::R12);
            let return_pc = enc.position();
            if stack_bytes > 0 {
                enc.add_sp(stack_bytes)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
            return Ok(Some(return_pc));
        }
        Inst::CallInterface { tag, args } => {
            let receiver = *args.first().ok_or(LowerError::CallUnsupported)?;
            enc.ldr_sp(Reg::R0, slot(receiver))
                .map_err(|_| LowerError::TooManyValues)?;
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
            enc.ldr_sp(Reg::R3, slot(receiver))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.subs_imm8(Reg::R3, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R3, Reg::R3, 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds(Reg::R0, Reg::R3, Reg::R0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds_imm8(Reg::R0, 1)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.mov_reg(Reg::R12, Reg::R0);
            let stack_bytes = load_call_args(enc, value_types, slot, args, 0)?;
            enc.blx(Reg::R12);
            let return_pc = enc.position();
            if stack_bytes > 0 {
                enc.add_sp(stack_bytes)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
            return Ok(Some(return_pc));
        }
        Inst::CastClassScan { args } => {
            let start = *args.first().ok_or(LowerError::CallUnsupported)?;
            let target = *args.get(1).ok_or(LowerError::CallUnsupported)?;
            enc.ldr_sp(Reg::R0, slot(start))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R2, slot(target))
                .map_err(|_| LowerError::TooManyValues)?;
            let search = enc.new_label();
            let found = enc.new_label();
            let miss = enc.new_label();
            let done = enc.new_label();
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
        Inst::Store { address, value } => {
            enc.ldr_sp(Reg::R0, slot(*address))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R1, slot(*value))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.str_imm(Reg::R1, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::Load { address } => {
            enc.ldr_sp(Reg::R0, slot(*address))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R0, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::FieldLoad { base, offset } => {
            let two_words = matches!(result_ty, Some(MirType::I64 | MirType::F64));
            if is_pointer_base(value_types, *base) {
                enc.ldr_sp(Reg::R2, slot(*base))
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_imm(Reg::R0, Reg::R2, *offset as u16)
                    .map_err(|_| LowerError::TooManyValues)?;
                if two_words {
                    enc.ldr_imm(Reg::R1, Reg::R2, *offset as u16 + 4)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
            } else {
                enc.ldr_sp(Reg::R0, slot(*base) + *offset as u16)
                    .map_err(|_| LowerError::TooManyValues)?;
                if two_words {
                    enc.ldr_sp(Reg::R1, slot(*base) + *offset as u16 + 4)
                        .map_err(|_| LowerError::TooManyValues)?;
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
                enc.ldr_sp(Reg::R1, slot(*base))
                    .map_err(|_| LowerError::TooManyValues)?;
            }
            enc.ldr_sp(Reg::R0, slot(*value))
                .map_err(|_| LowerError::TooManyValues)?;
            if base_ptr {
                enc.str_imm(Reg::R0, Reg::R1, *offset as u16)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                enc.str_sp(Reg::R0, slot(*base) + *offset as u16)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
            if two_words {
                enc.ldr_sp(Reg::R0, slot(*value) + 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                if base_ptr {
                    enc.str_imm(Reg::R0, Reg::R1, *offset as u16 + 4)
                        .map_err(|_| LowerError::TooManyValues)?;
                } else {
                    enc.str_sp(Reg::R0, slot(*base) + *offset as u16 + 4)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
            }
        }
        Inst::FieldAddr { base, offset } => {
            if is_pointer_base(value_types, *base) {
                enc.ldr_sp(Reg::R0, slot(*base))
                    .map_err(|_| LowerError::TooManyValues)?;
                if *offset != 0 {
                    enc.adds_imm8(Reg::R0, *offset as u8)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
            } else {
                enc.add_sp_imm(Reg::R0, slot(*base) + *offset as u16)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::InitStruct | Inst::CopyStruct { .. } => {}
        Inst::Convert { value, kind } => {
            enc.ldr_sp(Reg::R0, slot(*value))
                .map_err(|_| LowerError::TooManyValues)?;
            if matches!(kind, ConvKind::Float32ToInt) {
                emit_f2i(enc)?;
            } else if matches!(kind, ConvKind::IntToFloat32) {
                emit_i2f(enc)?;
            } else {
                extend_for(enc, Reg::R0, Reg::R0, *kind).map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::Widen { value, signed } => {
            enc.ldr_sp(Reg::R0, slot(*value))
                .map_err(|_| LowerError::TooManyValues)?;
            if *signed {
                enc.asrs_imm(Reg::R1, Reg::R0, 31)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                enc.movs_imm(Reg::R1, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::Truncate { value } => {
            enc.ldr_sp(Reg::R0, slot(*value))
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::SemihostWrite { text } => {
            let entry = enc.new_label();
            strings.push((entry, text.clone()));
            enc.adr(Reg::R1, entry)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.movs_imm(Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.bkpt(0xAB);
        }
        Inst::WriteInt { value } => {
            enc.ldr_sp(Reg::R0, slot(*value))
                .map_err(|_| LowerError::TooManyValues)?;
            emit_write_int(enc)?;
        }
        Inst::StringLiteral { utf16 } => {
            let entry = enc.new_label();
            string_blobs.push((entry, utf16.clone()));
            enc.adr(Reg::R0, entry)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::StringEquals { lhs, rhs } => {
            enc.ldr_sp(Reg::R0, slot(*lhs))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R1, slot(*rhs))
                .map_err(|_| LowerError::TooManyValues)?;
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
            enc.ldr_sp(Reg::R0, slot(*array))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R1, slot(*index))
                .map_err(|_| LowerError::TooManyValues)?;
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
            enc.ldr_sp(Reg::R0, slot(*array))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R1, slot(*index))
                .map_err(|_| LowerError::TooManyValues)?;
            emit_array_bounds_check(enc)?;
            scale_index(enc, pool, *element_size)?;
            enc.adds_imm3(Reg::R0, Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            if *element_size == 8 {
                enc.adds(Reg::R0, Reg::R0, Reg::R1)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R2, slot(*value))
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R3, slot(*value) + 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_imm(Reg::R2, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_imm(Reg::R3, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                enc.ldr_sp(Reg::R2, slot(*value))
                    .map_err(|_| LowerError::TooManyValues)?;
                match *element_size {
                    1 => enc.strb_reg(Reg::R2, Reg::R0, Reg::R1),
                    2 => enc.strh_reg(Reg::R2, Reg::R0, Reg::R1),
                    _ => enc.str_reg(Reg::R2, Reg::R0, Reg::R1),
                }
                .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        Inst::StaticLoad { offset } => {
            load_const_word(enc, pool, Reg::R0, STATIC_FIELD_BASE + *offset)?;
            enc.ldr_imm(Reg::R0, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::StaticStore { offset, value } => {
            load_const_word(enc, pool, Reg::R0, STATIC_FIELD_BASE + *offset)?;
            enc.ldr_sp(Reg::R1, slot(*value))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.str_imm(Reg::R1, Reg::R0, 0)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::Alloc { .. }
        | Inst::AllocArray { .. }
        | Inst::AllocArray2D { .. }
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
            enc.ldr_sp(Reg::R0, slot(*array))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R1, slot(*index0))
                .map_err(|_| LowerError::TooManyValues)?;
            emit_dim_bounds_check(enc, 0)?;
            enc.ldr_sp(Reg::R1, slot(*index1))
                .map_err(|_| LowerError::TooManyValues)?;
            emit_dim_bounds_check(enc, 4)?;
            enc.ldr_sp(Reg::R1, slot(*index0))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R2, Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.muls(Reg::R1, Reg::R2)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R2, slot(*index1))
                .map_err(|_| LowerError::TooManyValues)?;
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
            enc.ldr_sp(Reg::R0, slot(*array))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R1, slot(*index0))
                .map_err(|_| LowerError::TooManyValues)?;
            emit_dim_bounds_check(enc, 0)?;
            enc.ldr_sp(Reg::R1, slot(*index1))
                .map_err(|_| LowerError::TooManyValues)?;
            emit_dim_bounds_check(enc, 4)?;
            enc.ldr_sp(Reg::R1, slot(*index0))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_imm(Reg::R2, Reg::R0, 4)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.muls(Reg::R1, Reg::R2)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R2, slot(*index1))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.adds(Reg::R1, Reg::R1, Reg::R2)
                .map_err(|_| LowerError::TooManyValues)?;
            scale_index(enc, pool, *element_size)?;
            enc.adds_imm8(Reg::R0, 8)
                .map_err(|_| LowerError::TooManyValues)?;
            if *element_size == 8 {
                enc.adds(Reg::R0, Reg::R0, Reg::R1)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R2, slot(*value))
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R3, slot(*value) + 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_imm(Reg::R2, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_imm(Reg::R3, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                enc.ldr_sp(Reg::R2, slot(*value))
                    .map_err(|_| LowerError::TooManyValues)?;
                match *element_size {
                    1 => enc.strb_reg(Reg::R2, Reg::R0, Reg::R1),
                    2 => enc.strh_reg(Reg::R2, Reg::R0, Reg::R1),
                    _ => enc.str_reg(Reg::R2, Reg::R0, Reg::R1),
                }
                .map_err(|_| LowerError::TooManyValues)?;
            }
        }
    }
    Ok(None)
}

/// The absolute base of the module's static-field storage in RAM. A convention for now (a real
/// target would place this region and pass its base like the GC allocator address); scalar statics
/// live here at their byte offsets, between the semihosting output word and the GC heap.
const STATIC_FIELD_BASE: u32 = 0x2000_1000;

/// Emits the array bounds check: with `r0` = the array and `r1` = the index, traps (`udf`) unless
/// `index < length` (the length at `[array+0]`), compared UNSIGNED so a negative index -- a huge
/// unsigned value -- traps too, matching `IndexOutOfRangeException`'s effect. Until the exception
/// model lands, an out-of-range access aborts rather than throwing a catchable exception.
fn emit_array_bounds_check(enc: &mut Encoder) -> Result<(), LowerError> {
    emit_dim_bounds_check(enc, 0)
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
/// a follow-on). r1-r3 are scratch.
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

/// Encodes a string's UTF-16 units to the build's UTF-8 storage bytes: standard UTF-8 (lossy on
/// lone surrogates) for `string-utf8`, surrogate-preserving WTF-8 for `string-utf8-wtf8`.
#[cfg(all(feature = "string-utf8", not(feature = "string-utf8-wtf8")))]
fn encode_string_bytes(units: &[u16]) -> Vec<u8> {
    alloc::string::String::from_utf16_lossy(units).into_bytes()
}

/// WTF-8: a surrogate pair combines to its code point; a lone surrogate is encoded as its own
/// (3-byte) code point, preserving the interpreter's surrogate parity.
#[cfg(feature = "string-utf8-wtf8")]
fn encode_string_bytes(units: &[u16]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < units.len() {
        let u = units[i] as u32;
        let code = if (0xD800..=0xDBFF).contains(&u)
            && i + 1 < units.len()
            && (0xDC00..=0xDFFF).contains(&(units[i + 1] as u32))
        {
            let lo = units[i + 1] as u32;
            i += 2;
            0x1_0000 + ((u - 0xD800) << 10) + (lo - 0xDC00)
        } else {
            i += 1;
            u
        };
        if code < 0x80 {
            out.push(code as u8);
        } else if code < 0x800 {
            out.push(0xC0 | (code >> 6) as u8);
            out.push(0x80 | (code & 0x3F) as u8);
        } else if code < 0x1_0000 {
            out.push(0xE0 | (code >> 12) as u8);
            out.push(0x80 | ((code >> 6) & 0x3F) as u8);
            out.push(0x80 | (code & 0x3F) as u8);
        } else {
            out.push(0xF0 | (code >> 18) as u8);
            out.push(0x80 | ((code >> 12) & 0x3F) as u8);
            out.push(0x80 | ((code >> 6) & 0x3F) as u8);
            out.push(0x80 | (code & 0x3F) as u8);
        }
    }
    out
}

/// Lowers a function whose values do not fit in registers into a shared encoder.
/// Every value gets a stack slot; each instruction loads its operands into scratch
/// registers, computes, and stores the result. Control flow is handled: because a
/// block's parameter values are distinct from any argument value, the parameter
/// copies on a jump need no ordering. `func_labels` resolves calls.
#[allow(clippy::too_many_arguments)]
fn lower_spilled_into(
    func: &Function,
    enc: &mut Encoder,
    func_labels: &[Label],
    alloc_addr: Option<u32>,
    source_map: &[Vec<u32>],
    line_table: &mut Vec<(u32, u32)>,
    stack_maps: &mut Vec<StackMapEntry>,
    vtables: &[TypeMeta],
) -> Result<(), LowerError> {
    let has_calls = func.blocks.iter().any(|b| {
        b.insts.iter().any(|(_, i)| {
            matches!(
                i,
                Inst::Call { .. }
                    | Inst::CallVirtual { .. }
                    | Inst::CallInterface { .. }
                    | Inst::CastClassScan { .. }
                    | Inst::Alloc { .. }
                    | Inst::AllocArray { .. }
                    | Inst::AllocArray2D { .. }
            )
        })
    });
    let lr_bytes = if has_calls { 4 } else { 0 };
    let mut offsets: Vec<u16> = Vec::with_capacity(func.value_types.len());
    let mut used = 0u16;
    for ty in &func.value_types {
        offsets.push(used);
        used += ty.stack_slot_bytes() as u16;
    }
    let returns_big_struct = matches!(func.ret, Some(MirType::ValueType { size, .. }) if size > 4);
    let result_ptr_off = used;
    if returns_big_struct {
        used += 4;
    }
    let frame = ((used as usize + lr_bytes + 7) & !7usize) - lr_bytes;
    if frame > 508 {
        return Err(LowerError::TooManyValues);
    }
    let frame = frame as u16;
    let slot = |v: ValueId| offsets[v.0 as usize];

    let safepoints = crate::regalloc::safepoint_roots(func, &func.value_types);
    let record_safepoint =
        |stack_maps: &mut Vec<StackMapEntry>, index: usize, inst_pos: usize, return_pc: u32| {
            if let Some(roots) = safepoints
                .get(index)
                .and_then(|b| b.get(inst_pos))
                .and_then(Option::as_ref)
            {
                stack_maps.push(StackMapEntry {
                    return_pc,
                    frame_size: frame,
                    ref_offsets: roots.iter().map(|v| slot(*v)).collect(),
                });
            }
        };

    let mut pool: Vec<(Label, u32)> = Vec::new();
    let mut strings: Vec<(Label, Box<[u8]>)> = Vec::new();
    let mut string_blobs: Vec<(Label, Box<[u16]>)> = Vec::new();
    let mut type_descs: Vec<(Label, Box<[u32]>)> = Vec::new();
    let mut type_desc_labels: Vec<(lamella_ir::TypeHandle, Label)> = Vec::new();
    if has_calls {
        enc.push_registers(0, true);
    }
    enc.sub_sp(frame).map_err(|_| LowerError::TooManyValues)?;

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
                enc.ldr_sp(Reg::R0, frame + lr_bytes as u16 + stack_param_off)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_sp(Reg::R0, slot(param) + woff)
                    .map_err(|_| LowerError::TooManyValues)?;
                stack_param_off += 4;
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
                    enc.str_sp(Reg::R0, slot(*result) + (w as u16) * 4)
                        .map_err(|_| LowerError::TooManyValues)?;
                }
                continue;
            }
            if let Inst::CopyStruct { src } = inst {
                let bytes = func
                    .value_type(*result)
                    .map_or(0, MirType::stack_slot_bytes);
                for w in 0..(bytes / 4) {
                    let off = (w as u16) * 4;
                    enc.ldr_sp(Reg::R0, slot(*src) + off)
                        .map_err(|_| LowerError::TooManyValues)?;
                    enc.str_sp(Reg::R0, slot(*result) + off)
                        .map_err(|_| LowerError::TooManyValues)?;
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
                    let words = (size / 4) as u16;
                    let ptr = is_pointer_base(&func.value_types, *base);
                    if ptr {
                        enc.ldr_sp(Reg::R1, slot(*base))
                            .map_err(|_| LowerError::TooManyValues)?;
                    }
                    for w in 0..words {
                        enc.ldr_sp(Reg::R0, slot(*value) + w * 4)
                            .map_err(|_| LowerError::TooManyValues)?;
                        if ptr {
                            enc.str_imm(Reg::R0, Reg::R1, *offset as u16 + w * 4)
                                .map_err(|_| LowerError::TooManyValues)?;
                        } else {
                            enc.str_sp(Reg::R0, slot(*base) + *offset as u16 + w * 4)
                                .map_err(|_| LowerError::TooManyValues)?;
                        }
                    }
                    continue;
                }
            }
            if let Inst::FieldLoad { base, offset } = inst {
                if let Some(MirType::ValueType { size, .. }) = func.value_type(*result) {
                    let words = (size / 4) as u16;
                    let ptr = is_pointer_base(&func.value_types, *base);
                    if ptr {
                        enc.ldr_sp(Reg::R1, slot(*base))
                            .map_err(|_| LowerError::TooManyValues)?;
                    }
                    for w in 0..words {
                        if ptr {
                            enc.ldr_imm(Reg::R0, Reg::R1, *offset as u16 + w * 4)
                                .map_err(|_| LowerError::TooManyValues)?;
                        } else {
                            enc.ldr_sp(Reg::R0, slot(*base) + *offset as u16 + w * 4)
                                .map_err(|_| LowerError::TooManyValues)?;
                        }
                        enc.str_sp(Reg::R0, slot(*result) + w * 4)
                            .map_err(|_| LowerError::TooManyValues)?;
                    }
                    continue;
                }
            }
            if let Inst::Call { callee, args } = inst {
                if matches!(func.value_type(*result), Some(MirType::ValueType { size, .. }) if size > 4)
                {
                    enc.add_sp_imm(Reg::R0, slot(*result))
                        .map_err(|_| LowerError::TooManyValues)?;
                    load_call_args(enc, &func.value_types, &slot, args, 1)?;
                    let target = *func_labels
                        .get(*callee as usize)
                        .ok_or(LowerError::CallUnsupported)?;
                    enc.bl(target);
                    record_safepoint(stack_maps, index, inst_pos, enc.position());
                    continue;
                }
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
                record_safepoint(stack_maps, index, inst_pos, enc.position());
                let ok = enc.new_label();
                enc.cmp_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, ok);
                enc.udf(0);
                enc.bind_label(ok);
                enc.str_sp(Reg::R0, slot(*result))
                    .map_err(|_| LowerError::TooManyValues)?;
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
                enc.str_sp(Reg::R0, slot(*result))
                    .map_err(|_| LowerError::TooManyValues)?;
                continue;
            }
            if let Inst::LoadTypeDesc { object } = inst {
                enc.ldr_sp(Reg::R0, slot(*object))
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.subs_imm8(Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_imm(Reg::R0, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_sp(Reg::R0, slot(*result))
                    .map_err(|_| LowerError::TooManyValues)?;
                continue;
            }
            if let Inst::AllocArray {
                handle,
                length,
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
                enc.ldr_sp(Reg::R0, slot(*length))
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
                enc.adds_imm8(Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.adr(Reg::R1, desc_label)
                    .map_err(|_| LowerError::TooManyValues)?;
                load_const_word(enc, &mut pool, Reg::R2, alloc)?;
                enc.blx(Reg::R2);
                record_safepoint(stack_maps, index, inst_pos, enc.position());
                let ok = enc.new_label();
                enc.cmp_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, ok);
                enc.udf(0);
                enc.bind_label(ok);
                enc.ldr_sp(Reg::R1, slot(*length))
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_imm(Reg::R1, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_sp(Reg::R0, slot(*result))
                    .map_err(|_| LowerError::TooManyValues)?;
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
                enc.ldr_sp(Reg::R0, slot(*dim0))
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R1, slot(*dim1))
                    .map_err(|_| LowerError::TooManyValues)?;
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
                record_safepoint(stack_maps, index, inst_pos, enc.position());
                let ok = enc.new_label();
                enc.cmp_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.b_cond(Cond::Ne, ok);
                enc.udf(0);
                enc.bind_label(ok);
                enc.ldr_sp(Reg::R1, slot(*dim0))
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_imm(Reg::R1, Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R1, slot(*dim1))
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_imm(Reg::R1, Reg::R0, 4)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.str_sp(Reg::R0, slot(*result))
                    .map_err(|_| LowerError::TooManyValues)?;
                continue;
            }
            let call_pc = lower_spilled_inst(
                enc,
                &mut pool,
                &mut strings,
                &mut string_blobs,
                &func.value_types,
                &slot,
                inst,
                func.value_type(*result),
                func_labels,
            )?;
            if let Some(return_pc) = call_pc {
                record_safepoint(stack_maps, index, inst_pos, return_pc);
            }
            enc.str_sp(Reg::R0, slot(*result))
                .map_err(|_| LowerError::TooManyValues)?;
            if matches!(func.value_type(*result), Some(MirType::I64 | MirType::F64)) {
                enc.str_sp(Reg::R1, slot(*result) + 4)
                    .map_err(|_| LowerError::TooManyValues)?;
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
                        enc.ldr_sp(Reg::R1, result_ptr_off)
                            .map_err(|_| LowerError::TooManyValues)?;
                        for w in 0..(size / 4) {
                            let off = (w as u16) * 4;
                            enc.ldr_sp(Reg::R0, slot(*v) + off)
                                .map_err(|_| LowerError::TooManyValues)?;
                            enc.str_imm(Reg::R0, Reg::R1, off)
                                .map_err(|_| LowerError::TooManyValues)?;
                        }
                        enc.ldr_sp(Reg::R0, result_ptr_off)
                            .map_err(|_| LowerError::TooManyValues)?;
                    }
                } else if let Some(v) = value {
                    enc.ldr_sp(Reg::R0, slot(*v))
                        .map_err(|_| LowerError::TooManyValues)?;
                    if func.value_type(*v) == Some(MirType::I64) {
                        enc.ldr_sp(Reg::R1, slot(*v) + 4)
                            .map_err(|_| LowerError::TooManyValues)?;
                    }
                }
                enc.add_sp(frame).map_err(|_| LowerError::TooManyValues)?;
                if has_calls {
                    enc.pop_registers(0, true);
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
                    enc.ldr_sp(Reg::R0, slot(*a))
                        .map_err(|_| LowerError::TooManyValues)?;
                    enc.str_sp(Reg::R0, slot(*p))
                        .map_err(|_| LowerError::TooManyValues)?;
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
                enc.ldr_sp(Reg::R0, slot(*cond))
                    .map_err(|_| LowerError::TooManyValues)?;
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
            enc.bind_label(entry);
            enc.emit_word(value);
        }
    }
    for (entry, text) in strings {
        enc.align_to_word();
        enc.bind_label(entry);
        enc.emit_bytes(&text);
    }
    for (entry, utf16) in string_blobs {
        enc.align_to_word();
        enc.bind_label(entry);
        #[cfg(not(any(feature = "string-utf8", feature = "string-utf8-wtf8")))]
        {
            enc.emit_word(utf16.len() as u32);
            for &unit in utf16.iter() {
                enc.emit_u16(unit);
            }
        }
        #[cfg(any(feature = "string-utf8", feature = "string-utf8-wtf8"))]
        {
            let bytes = encode_string_bytes(&utf16);
            enc.emit_word(utf16.len() as u32);
            enc.emit_word(bytes.len() as u32);
            enc.emit_bytes(&bytes);
        }
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
    for (entry, words) in type_descs {
        enc.align_to_word();
        let meta = type_desc_labels
            .iter()
            .find(|(_, label)| *label == entry)
            .map(|(handle, _)| *handle)
            .and_then(|handle| vtables.iter().find(|m| m.handle == handle));
        if let Some(meta) = meta {
            for &func_index in meta.vtable.iter().rev() {
                if let Some(&label) = func_labels.get(func_index as usize) {
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
        for &word in words.iter().skip(3) {
            enc.emit_word(word);
        }
        if let Some(meta) = meta {
            if !meta.itable.is_empty() {
                enc.emit_word(meta.itable.len() as u32);
                for &(tag, func_index) in &meta.itable {
                    enc.emit_word(tag);
                    if let Some(&label) = func_labels.get(func_index as usize) {
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
                    | Inst::AllocArray { .. }
                    | Inst::ArrayLoad { .. }
                    | Inst::ArrayStore { .. }
                    | Inst::AllocArray2D { .. }
                    | Inst::Array2DLoad { .. }
                    | Inst::Array2DStore { .. }
                    | Inst::StaticLoad { .. }
                    | Inst::StaticStore { .. }
                    | Inst::LoadTypeDesc { .. }
                    | Inst::TypeDescAddr { .. }
                    | Inst::CallVirtual { .. }
                    | Inst::CallInterface { .. }
                    | Inst::CastClassScan { .. }
            )
        })
    }) {
        return Ok(Assignment::Spilled);
    }
    if func.blocks.iter().any(|b| {
        b.insts.iter().any(|(_, i)| {
            matches!(
                i,
                Inst::FieldLoad { .. } | Inst::FieldStore { .. } | Inst::FieldAddr { .. }
            )
        })
    }) {
        return Ok(Assignment::Spilled);
    }
    let has_calls = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| matches!(i, Inst::Call { .. })));
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
    let has_calls = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| matches!(i, Inst::Call { .. })));
    let lr_bytes = if has_calls { 4 } else { 0 };
    let pushed = saved.count_ones() as usize * 4 + lr_bytes;
    let frame = ((pushed + mixed.spill_count as usize * 4 + 7) & !7usize) - pushed;
    if frame > 508 {
        return Err(LowerError::TooManyValues);
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
    let target = *func_labels
        .get(callee as usize)
        .ok_or(LowerError::CallUnsupported)?;
    enc.bl(target);
    if assign(result) != Reg::R0 {
        enc.mov_reg(assign(result), Reg::R0);
    }
    Ok(())
}

/// Lowers one function's body into a shared encoder, given its register
/// assignment. `func_labels` resolves `Call` targets by program index; pass an
/// empty slice for a function that makes no calls.
fn lower_into(
    func: &Function,
    enc: &mut Encoder,
    regs: &[Reg],
    saved: u8,
    func_labels: &[Label],
    source_map: &[Vec<u32>],
    line_table: &mut Vec<(u32, u32)>,
) -> Result<(), LowerError> {
    let assign = |v: ValueId| regs.get(v.index()).copied().unwrap_or(Reg::R0);
    let has_calls = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| matches!(i, Inst::Call { .. })));

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
        for (inst_pos, (result, inst)) in body.iter().enumerate() {
            if let Some(&cil) = source_map.get(index).and_then(|b| b.get(inst_pos)) {
                line_table.push((enc.position(), cil));
            }
            if let Inst::Call { callee, args } = inst {
                lower_call(enc, &assign, *result, *callee, args, func_labels)?;
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
                let condition = match fused {
                    Some((op, lhs, rhs)) => {
                        enc.cmp_reg(assign(lhs), assign(rhs))
                            .map_err(|_| LowerError::TooManyValues)?;
                        cmpop_to_cond(op)
                    }
                    None => {
                        enc.cmp_imm(assign(*cond), 0)
                            .map_err(|_| LowerError::TooManyValues)?;
                        Cond::Ne
                    }
                };
                enc.b_cond(condition, true_label);
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
            enc.bind_label(entry);
            enc.emit_word(value);
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
) -> Result<(), LowerError> {
    let has_calls = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| matches!(i, Inst::Call { .. })));
    let home = |v: ValueId| homes.get(v.index()).copied().unwrap_or(Home::Reg(Reg::R0));

    if has_calls || saved != 0 {
        enc.push_registers(saved, has_calls);
    }
    if frame > 0 {
        enc.sub_sp(frame).map_err(|_| LowerError::TooManyValues)?;
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

        for (inst_pos, (result, inst)) in body.iter().enumerate() {
            if let Some(&cil) = source_map.get(index).and_then(|b| b.get(inst_pos)) {
                line_table.push((enc.position(), cil));
            }
            if let Inst::Call { callee, args } = inst {
                lower_mixed_call(enc, &home, *result, *callee, args, func_labels)?;
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
                    enc.add_sp(frame).map_err(|_| LowerError::TooManyValues)?;
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
                let condition = match fused {
                    Some((op, lhs, rhs)) => {
                        let a = read_to_scratch(enc, home(lhs), Reg::R0)?;
                        let b = read_to_scratch(enc, home(rhs), Reg::R1)?;
                        enc.cmp_reg(a, b).map_err(|_| LowerError::TooManyValues)?;
                        cmpop_to_cond(op)
                    }
                    None => {
                        let c = read_to_scratch(enc, home(*cond), Reg::R0)?;
                        enc.cmp_imm(c, 0).map_err(|_| LowerError::TooManyValues)?;
                        Cond::Ne
                    }
                };
                enc.b_cond(condition, true_label);
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
            enc.bind_label(entry);
            enc.emit_word(value);
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
    let target = *func_labels
        .get(callee as usize)
        .ok_or(LowerError::CallUnsupported)?;
    enc.bl(target);
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

pub use crate::resolver::TypeMeta;

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

/// One GC safepoint's stack map: the `ObjectRef` roots live in the frame when a call or
/// allocation returns, for a relocating collector to find and update. `return_pc` is the native
/// code offset of the instruction after the call (add the method's load address for the device
/// PC); each `ref_offsets` entry is a byte offset from SP-at-the-call of a spilled root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackMapEntry {
    /// The return address of the safepoint -- a native code offset, the collector's lookup key.
    pub return_pc: u32,
    /// The frame the safepoint opened, in bytes; the collector steps past it to the caller.
    pub frame_size: u16,
    /// Byte offsets from SP-at-the-call of the live `ObjectRef` roots in the frame.
    pub ref_offsets: Vec<u16>,
}

/// The GC stack maps for a lowered program -- one entry per safepoint, sorted by `return_pc` for
/// the collector's binary search. The all-spilled path keeps every root in a frame slot, so the
/// map names slot offsets only; on this path no callee-saved register is used (`saved == 0`), so
/// the saved LR a frame walk reads sits at `SP-at-the-call + frame_size`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StackMaps(pub Vec<StackMapEntry>);

impl StackMaps {
    /// The little-endian wire format the collector consumes: `u32 count`, then each entry as
    /// `u32 return_pc; u16 frame_size; u16 nrefs; u16 ref_offsets[nrefs]`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(self.0.len() as u32).to_le_bytes());
        for entry in &self.0 {
            out.extend_from_slice(&entry.return_pc.to_le_bytes());
            out.extend_from_slice(&entry.frame_size.to_le_bytes());
            out.extend_from_slice(&(entry.ref_offsets.len() as u16).to_le_bytes());
            for &offset in &entry.ref_offsets {
                out.extend_from_slice(&offset.to_le_bytes());
            }
        }
        out
    }
}

/// Lowers a single function to ARM32 machine code. A function that calls another
/// must go through [`lower_module`], which resolves the call targets.
pub fn lower(func: &Function) -> Result<Vec<u8>, LowerError> {
    let mut enc = Encoder::new();
    let mut _lines = Vec::new();
    match prepare(func)? {
        Assignment::Registers { regs, saved } => {
            lower_into(func, &mut enc, &regs, saved, &[], &[], &mut _lines)?
        }
        Assignment::Mixed {
            homes,
            saved,
            frame,
        } => lower_mixed_into(func, &mut enc, &homes, saved, frame, &[], &[], &mut _lines)?,
        Assignment::Spilled => lower_spilled_into(
            func,
            &mut enc,
            &[],
            None,
            &[],
            &mut _lines,
            &mut Vec::new(),
            &[],
        )?,
    }
    enc.finish()
        .map(|assembled| assembled.bytes)
        .map_err(|_| LowerError::CodeTooLarge)
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
        Assignment::Registers { regs, saved } => {
            lower_into(func, &mut enc, &regs, saved, &[], source_map, &mut lines)?
        }
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
        )?,
        Assignment::Spilled => lower_spilled_into(
            func,
            &mut enc,
            &[],
            None,
            source_map,
            &mut lines,
            &mut Vec::new(),
            &[],
        )?,
    }
    let bytes = enc
        .finish()
        .map(|assembled| assembled.bytes)
        .map_err(|_| LowerError::CodeTooLarge)?;
    Ok((bytes, LineTable(lines)))
}

/// Lowers a whole program -- several functions concatenated into one image, the
/// direct calls between them resolved. `Call { callee }` names function index
/// `callee` in `funcs`.
pub fn lower_module(funcs: &[Function]) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(funcs, None, &[], &[]).map(|(bytes, _, _)| bytes)
}

/// Lowers a whole program whose reference-type allocations call the garbage-collected
/// runtime allocator at absolute address `alloc_addr` -- `lamella_gc_alloc(payload_size,
/// &TypeDesc) -> payload*`, AAPCS (size in r0, descriptor in r1, result in r0). Each `Alloc`
/// lowers to `blx` that address with a null-check; the type descriptors are emitted per type.
pub fn lower_module_gc(funcs: &[Function], alloc_addr: u32) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(funcs, Some(alloc_addr), &[], &[]).map(|(bytes, _, _)| bytes)
}

/// As [`lower_module_gc`], but with per-type VTABLES (`(type handle, function indices in slot order)`)
/// emitted before each TypeDesc, so `callvirt` dispatches through `obj-4`'s descriptor. The resolver
/// produces the table via `MetadataResolver::vtables`.
pub fn lower_module_gc_vtables(
    funcs: &[Function],
    alloc_addr: u32,
    vtables: &[TypeMeta],
) -> Result<Vec<u8>, LowerError> {
    lower_module_inner(funcs, Some(alloc_addr), vtables, &[]).map(|(bytes, _, _)| bytes)
}

/// As [`lower_module_gc`], but also returns the GC [`StackMaps`] -- one entry per safepoint
/// (every call and allocation), naming the live `ObjectRef` roots for a relocating collector.
pub fn lower_module_gc_mapped(
    funcs: &[Function],
    alloc_addr: u32,
) -> Result<(Vec<u8>, StackMaps), LowerError> {
    lower_module_inner(funcs, Some(alloc_addr), &[], &[]).map(|(bytes, maps, _)| (bytes, maps))
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
    lower_module_inner(funcs, alloc_addr, &[], source_maps).map(|(bytes, _, lines)| (bytes, lines))
}

fn lower_module_inner(
    funcs: &[Function],
    alloc_addr: Option<u32>,
    vtables: &[TypeMeta],
    source_maps: &[crate::cil::CilSourceMap],
) -> Result<(Vec<u8>, StackMaps, MethodLineTables), LowerError> {
    let original_count = funcs.len();
    let mut program = funcs.to_vec();
    crate::stringgen::lower_string_concat(&mut program);
    crate::stringgen::lower_int_to_string(&mut program);
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
                )?;
            }
            Assignment::Spilled => {
                lower_spilled_into(
                    func,
                    &mut enc,
                    &func_labels,
                    alloc_addr,
                    source_map,
                    &mut lines,
                    &mut stack_maps,
                    vtables,
                )?;
            }
        }
        if index < original_count {
            method_lines.push((func_offset, LineTable(lines)));
        }
    }
    stack_maps.sort_by_key(|entry| entry.return_pc);
    enc.finish()
        .map(|assembled| (assembled.bytes, StackMaps(stack_maps), method_lines))
        .map_err(|_| LowerError::CodeTooLarge)
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

    #[cfg(feature = "string-utf8-wtf8")]
    #[test]
    fn wtf8_encodes_surrogate_pairs_and_lone_surrogates() {
        assert_eq!(
            encode_string_bytes(&[0xD834, 0xDD1E]),
            [0xF0, 0x9D, 0x84, 0x9E]
        );
        assert_eq!(encode_string_bytes(&[0xD800]), [0xED, 0xA0, 0x80]);
        assert_eq!(encode_string_bytes(&[0x61, 0x62]), [0x61, 0x62]);
    }

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
    fn lowers_a_struct_argument() {
        let point = MirType::ValueType {
            handle: lamella_ir::TypeHandle(0),
            size: 8,
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
                vtable: vec![1],
                itable: vec![(iface_tag, 1)],
                base: None,
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
}
