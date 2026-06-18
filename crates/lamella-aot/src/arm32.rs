//! Lowering the middle IR to ARMv6-M Thumb machine code.

use alloc::vec::Vec;

use lamella_asm_arm32::{Cond, Encoder, Label, Reg};
use lamella_ir::{BinOp, BlockId, CmpOp, Function, Inst, Terminator, ValueId};

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
}

/// Value *i* lives in register *i* under the trivial allocation (the value count
/// is bounded to eight, r0-r7, before lowering begins).
fn reg(value: ValueId) -> Reg {
    Reg::new(value.0 as u8).unwrap_or(Reg::R0)
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
) -> Result<(), LowerError> {
    match inst {
        Inst::ConstInt { value, .. } => {
            if let Ok(imm) = u8::try_from(*value) {
                enc.movs_imm(reg(result), imm)
                    .map_err(|_| LowerError::TooManyValues)?;
            } else {
                let entry = enc.new_label();
                enc.ldr_literal(reg(result), entry)
                    .map_err(|_| LowerError::TooManyValues)?;
                pool.push((entry, *value as u32));
            }
        }
        Inst::Binary { op, lhs, rhs } => {
            let (d, a, b) = (reg(result), reg(*lhs), reg(*rhs));
            let emitted = match op {
                BinOp::Add => enc.adds(d, a, b),
                BinOp::Sub => enc.subs(d, a, b),
                BinOp::And => {
                    enc.mov_reg(d, a);
                    enc.ands(d, b)
                }
                BinOp::Or => {
                    enc.mov_reg(d, a);
                    enc.orrs(d, b)
                }
                BinOp::Xor => {
                    enc.mov_reg(d, a);
                    enc.eors(d, b)
                }
                BinOp::Mul => {
                    enc.mov_reg(d, a);
                    enc.muls(d, b)
                }
                BinOp::Shl => {
                    enc.mov_reg(d, a);
                    enc.lsls_reg(d, b)
                }
                BinOp::ShrSigned => {
                    enc.mov_reg(d, a);
                    enc.asrs_reg(d, b)
                }
                BinOp::ShrUnsigned => {
                    enc.mov_reg(d, a);
                    enc.lsrs_reg(d, b)
                }
            };
            emitted.map_err(|_| LowerError::TooManyValues)?;
        }
        Inst::Compare { op, lhs, rhs } => {
            enc.cmp_reg(reg(*lhs), reg(*rhs))
                .map_err(|_| LowerError::TooManyValues)?;
            enc.movs_imm(reg(result), 1)
                .map_err(|_| LowerError::TooManyValues)?;
            let done = enc.new_label();
            enc.b_cond(cmpop_to_cond(*op), done);
            enc.movs_imm(reg(result), 0)
                .map_err(|_| LowerError::TooManyValues)?;
            enc.bind_label(done);
        }
    }
    Ok(())
}

/// Lowers a straight-line integer [`Function`] whose value count exceeds the
/// eight registers, by giving each value a stack slot at `[sp, #value*4]` and
/// shuttling operands through scratch registers r0 and r1. The frame is bounded
/// by the `SUB SP` reach (508 bytes, so up to 127 values); spilling is not yet
/// combined with control flow.
fn lower_spilled(func: &Function) -> Result<Vec<u8>, LowerError> {
    let block = &func.blocks[0];
    let frame = (func.value_types.len() * 4 + 7) & !7usize;
    if frame > 508 {
        return Err(LowerError::TooManyValues);
    }
    let frame = frame as u16;
    let slot = |v: ValueId| (v.0 as u16) * 4;

    let mut enc = Encoder::new();
    let mut pool: Vec<(Label, u32)> = Vec::new();
    enc.sub_sp(frame).map_err(|_| LowerError::TooManyValues)?;

    for (i, &param) in block.params.iter().enumerate() {
        if i >= 4 {
            return Err(LowerError::TooManyValues);
        }
        let arg = Reg::new(i as u8).ok_or(LowerError::TooManyValues)?;
        enc.str_sp(arg, slot(param))
            .map_err(|_| LowerError::TooManyValues)?;
    }

    for (result, inst) in &block.insts {
        match inst {
            Inst::ConstInt { value, .. } => {
                if let Ok(imm) = u8::try_from(*value) {
                    enc.movs_imm(Reg::R0, imm)
                        .map_err(|_| LowerError::TooManyValues)?;
                } else {
                    let entry = enc.new_label();
                    enc.ldr_literal(Reg::R0, entry)
                        .map_err(|_| LowerError::TooManyValues)?;
                    pool.push((entry, *value as u32));
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
            Inst::Compare { op, lhs, rhs } => {
                enc.ldr_sp(Reg::R0, slot(*lhs))
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.ldr_sp(Reg::R1, slot(*rhs))
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.cmp_reg(Reg::R0, Reg::R1)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.movs_imm(Reg::R0, 1)
                    .map_err(|_| LowerError::TooManyValues)?;
                let done = enc.new_label();
                enc.b_cond(cmpop_to_cond(*op), done);
                enc.movs_imm(Reg::R0, 0)
                    .map_err(|_| LowerError::TooManyValues)?;
                enc.bind_label(done);
            }
        }
        enc.str_sp(Reg::R0, slot(*result))
            .map_err(|_| LowerError::TooManyValues)?;
    }

    match &block.terminator {
        Some(Terminator::Return(value)) => {
            if let Some(v) = value {
                enc.ldr_sp(Reg::R0, slot(*v))
                    .map_err(|_| LowerError::TooManyValues)?;
            }
            enc.add_sp(frame).map_err(|_| LowerError::TooManyValues)?;
            enc.bx(Reg::LR);
        }
        _ => return Err(LowerError::ControlFlowUnsupported),
    }

    if !pool.is_empty() {
        enc.align_to_word();
        for (entry, value) in pool {
            enc.bind_label(entry);
            enc.emit_word(value);
        }
    }

    enc.finish()
        .map(|assembled| assembled.bytes)
        .map_err(|_| LowerError::CodeTooLarge)
}

/// Lowers an integer [`Function`] -- straight-line or branching -- to ARMv6-M
/// Thumb machine code via the AAPCS convention. See the module documentation for
/// the supported slice.
pub fn lower(func: &Function) -> Result<Vec<u8>, LowerError> {
    if lamella_ir::verify(func).is_err() {
        return Err(LowerError::NotWellFormed);
    }
    if func.value_types.iter().any(|ty| !ty.is_integer()) {
        return Err(LowerError::NonIntegerValue);
    }
    if func.value_types.len() > 8 {
        return if func.blocks.len() == 1 {
            lower_spilled(func)
        } else {
            Err(LowerError::TooManyValues)
        };
    }
    let mut enc = Encoder::new();
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
        for (result, inst) in body {
            lower_inst(&mut enc, &mut pool, *result, inst)?;
        }

        match &block.terminator {
            Some(Terminator::Return(value)) => {
                if let Some(v) = value {
                    if reg(*v) != Reg::R0 {
                        enc.mov_reg(Reg::R0, reg(*v));
                    }
                }
                enc.bx(Reg::LR);
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
                    .map(|(p, a)| (reg(*p), reg(*a)))
                    .collect();
                emit_parallel_move(&mut enc, &moves);
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
                        enc.cmp_reg(reg(lhs), reg(rhs))
                            .map_err(|_| LowerError::TooManyValues)?;
                        cmpop_to_cond(op)
                    }
                    None => {
                        enc.cmp_imm(reg(*cond), 0)
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

        let func = spilled_sum_function();

        let mut body = lower(&func).unwrap();
        assert_eq!(&body[body.len() - 2..], &[0x70, 0x47]);
        body.truncate(body.len() - 2);

        let mut img = Encoder::new();
        img.emit_word(0x2000_4000);
        img.emit_word(0x0000_0009);
        img.emit_bytes(&body);
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
