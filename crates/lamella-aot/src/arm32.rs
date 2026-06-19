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
            extend_for(enc, assign(result), assign(*value), *kind)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::Widen { .. }
        | Inst::Truncate { .. }
        | Inst::InitStruct
        | Inst::FieldLoad { .. }
        | Inst::FieldStore { .. }
        | Inst::CopyStruct { .. } => return Err(LowerError::CallUnsupported),
        Inst::Call { .. } => return Err(LowerError::CallUnsupported),
        Inst::SemihostWrite { .. } => return Err(LowerError::CallUnsupported),
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

/// Lowers a straight-line integer [`Function`] whose value count exceeds the
/// eight registers, by giving each value a stack slot at `[sp, #value*4]` and
/// shuttling operands through scratch registers r0 and r1. The frame is bounded
/// by the `SUB SP` reach (508 bytes, so up to 127 values); spilling is not yet
/// combined with control flow.
/// Lowers one instruction of a spilled function: load its operands from their
/// stack slots into scratch registers (r0-r3), compute, and leave the result in
/// r0 for the caller to store.
fn lower_spilled_inst(
    enc: &mut Encoder,
    pool: &mut Vec<(Label, u32)>,
    strings: &mut Vec<(Label, Box<[u8]>)>,
    value_types: &[MirType],
    slot: &impl Fn(ValueId) -> u16,
    inst: &Inst,
    func_labels: &[Label],
) -> Result<(), LowerError> {
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
                BinOp::Mul | BinOp::Shl | BinOp::ShrSigned | BinOp::ShrUnsigned => {
                    return Err(LowerError::TooManyValues);
                }
            }
        }
        Inst::Binary { op, lhs, rhs } => {
            enc.ldr_sp(Reg::R0, slot(*lhs))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.ldr_sp(Reg::R1, slot(*rhs))
                .map_err(|_| LowerError::TooManyValues)?;
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
            };
            emitted.map_err(|_| LowerError::TooManyValues)?;
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
            if args.len() > 4 {
                return Err(LowerError::CallUnsupported);
            }
            for (i, a) in args.iter().enumerate() {
                let r = Reg::new(i as u8).ok_or(LowerError::TooManyValues)?;
                enc.ldr_sp(r, slot(*a))
                    .map_err(|_| LowerError::TooManyValues)?;
            }
            let target = *func_labels
                .get(*callee as usize)
                .ok_or(LowerError::CallUnsupported)?;
            enc.bl(target);
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
            enc.ldr_sp(Reg::R0, slot(*base) + *offset as u16)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::FieldStore {
            base,
            offset,
            value,
        } => {
            enc.ldr_sp(Reg::R0, slot(*value))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.str_sp(Reg::R0, slot(*base) + *offset as u16)
                .map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::InitStruct | Inst::CopyStruct { .. } => {}
        Inst::Convert { value, kind } => {
            enc.ldr_sp(Reg::R0, slot(*value))
                .map_err(|_| LowerError::TooManyValues)?;
            extend_for(enc, Reg::R0, Reg::R0, *kind).map_err(|_| LowerError::TooManyValues)?;
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
    }
    Ok(())
}

/// Lowers a function whose values do not fit in registers into a shared encoder.
/// Every value gets a stack slot; each instruction loads its operands into scratch
/// registers, computes, and stores the result. Control flow is handled: because a
/// block's parameter values are distinct from any argument value, the parameter
/// copies on a jump need no ordering. `func_labels` resolves calls.
fn lower_spilled_into(
    func: &Function,
    enc: &mut Encoder,
    func_labels: &[Label],
    source_map: &[Vec<u32>],
    line_table: &mut Vec<(u32, u32)>,
) -> Result<(), LowerError> {
    let has_calls = func
        .blocks
        .iter()
        .any(|b| b.insts.iter().any(|(_, i)| matches!(i, Inst::Call { .. })));
    let lr_bytes = if has_calls { 4 } else { 0 };
    let mut offsets: Vec<u16> = Vec::with_capacity(func.value_types.len());
    let mut used = 0u16;
    for ty in &func.value_types {
        offsets.push(used);
        used += ty.stack_slot_bytes() as u16;
    }
    let frame = ((used as usize + lr_bytes + 7) & !7usize) - lr_bytes;
    if frame > 508 {
        return Err(LowerError::TooManyValues);
    }
    let frame = frame as u16;
    let slot = |v: ValueId| offsets[v.0 as usize];

    let mut pool: Vec<(Label, u32)> = Vec::new();
    let mut strings: Vec<(Label, Box<[u8]>)> = Vec::new();
    if has_calls {
        enc.push_registers(0, true);
    }
    enc.sub_sp(frame).map_err(|_| LowerError::TooManyValues)?;

    let entry_block = func
        .blocks
        .get(func.entry.index())
        .ok_or(LowerError::ControlFlowUnsupported)?;
    for (i, &param) in entry_block.params.iter().enumerate() {
        let arg = Reg::new(i as u8)
            .filter(|_| i < 4)
            .ok_or(LowerError::TooManyValues)?;
        enc.str_sp(arg, slot(param))
            .map_err(|_| LowerError::TooManyValues)?;
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
            lower_spilled_inst(
                enc,
                &mut pool,
                &mut strings,
                &func.value_types,
                &slot,
                inst,
                func_labels,
            )?;
            enc.str_sp(Reg::R0, slot(*result))
                .map_err(|_| LowerError::TooManyValues)?;
            if func.value_type(*result) == Some(MirType::I64) {
                enc.str_sp(Reg::R1, slot(*result) + 4)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
        }
        if let Some(&cil) = source_map.get(index).and_then(|b| b.last()) {
            line_table.push((enc.position(), cil));
        }
        match &block.terminator {
            Some(Terminator::Return(value)) => {
                if let Some(v) = value {
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
    Ok(())
}

/// Lowers an integer [`Function`] -- straight-line or branching -- to ARMv6-M
/// Thumb machine code via the AAPCS convention. See the module documentation for
/// the supported slice.
/// How a function's values are placed: in registers (with the callee-saved set to
/// preserve), or spilled to the stack when more values are live at once than there
/// are registers.
enum Assignment {
    Registers { regs: Vec<Reg>, saved: u8 },
    Spilled,
}

/// Verifies `func` and decides where its values live.
fn prepare(func: &Function) -> Result<Assignment, LowerError> {
    if lamella_ir::verify(func).is_err() {
        return Err(LowerError::NotWellFormed);
    }
    if func
        .value_types
        .iter()
        .any(|ty| ty.is_float() || ty.is_gc_reference())
    {
        return Err(LowerError::NonIntegerValue);
    }
    if func
        .value_types
        .iter()
        .any(|ty| matches!(ty, MirType::I64 | MirType::ValueType { .. }))
    {
        return Ok(Assignment::Spilled);
    }
    if func.blocks.iter().any(|b| {
        b.insts
            .iter()
            .any(|(_, i)| matches!(i, Inst::SemihostWrite { .. }))
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
    let regs: Vec<Reg> = if func.value_types.len() <= 8 {
        (0..func.value_types.len())
            .map(|i| Reg::new(i as u8).unwrap_or(Reg::R0))
            .collect()
    } else {
        let live = crate::regalloc::Liveness::analyze(func);
        let intervals = crate::regalloc::live_intervals(func, &live);
        let allocation = crate::regalloc::allocate(&intervals, 8);
        if allocation.spill_count > 0 {
            return Ok(Assignment::Spilled);
        }
        allocation
            .locations
            .iter()
            .map(|loc| match loc {
                crate::regalloc::Location::Register(r) => Reg::new(*r as u8).unwrap_or(Reg::R0),
                crate::regalloc::Location::Spill(_) => Reg::R0,
            })
            .collect()
    };
    let registers_used = regs
        .iter()
        .map(|r| u32::from(r.number()) + 1)
        .max()
        .unwrap_or(0);
    let saved: u8 = if registers_used > 4 {
        (((1u16 << registers_used.min(8)) - (1u16 << 4)) & 0xF0) as u8
    } else {
        0
    };
    Ok(Assignment::Registers { regs, saved })
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

/// Maps native code offsets to CIL byte offsets, ascending by offset, so a
/// debugger can take a native PC and recover the CIL instruction being executed. Built
/// by [`lower_debug`] from a `cil::CilSourceMap`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineTable(pub Vec<(u32, u32)>);

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

/// Lowers a single function to ARM32 machine code. A function that calls another
/// must go through [`lower_module`], which resolves the call targets.
pub fn lower(func: &Function) -> Result<Vec<u8>, LowerError> {
    let mut enc = Encoder::new();
    let mut _lines = Vec::new();
    match prepare(func)? {
        Assignment::Registers { regs, saved } => {
            lower_into(func, &mut enc, &regs, saved, &[], &[], &mut _lines)?
        }
        Assignment::Spilled => lower_spilled_into(func, &mut enc, &[], &[], &mut _lines)?,
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
        Assignment::Spilled => lower_spilled_into(func, &mut enc, &[], source_map, &mut lines)?,
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
    let mut enc = Encoder::new();
    let func_labels: Vec<Label> = funcs.iter().map(|_| enc.new_label()).collect();
    let mut _lines = Vec::new();
    for (index, func) in funcs.iter().enumerate() {
        enc.bind_label(func_labels[index]);
        match prepare(func)? {
            Assignment::Registers { regs, saved } => {
                lower_into(func, &mut enc, &regs, saved, &func_labels, &[], &mut _lines)?;
            }
            Assignment::Spilled => {
                lower_spilled_into(func, &mut enc, &func_labels, &[], &mut _lines)?;
            }
        }
    }
    enc.finish()
        .map(|assembled| assembled.bytes)
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
    fn lowers_a_spilled_branch() {
        let func = spilled_branch_function();
        assert!(lamella_ir::verify(&func).is_ok());
        let bytes = lower(&func).unwrap();
        assert_eq!(bytes[1], 0xB0);
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
}
