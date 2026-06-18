//! Lowering the middle IR to ARMv6-M Thumb machine code.

use alloc::vec::Vec;

use lamella_asm_arm32::{Encoder, Reg};
use lamella_ir::{BinOp, Function, Inst, Terminator, ValueId};

use crate::target::TargetLowering;

/// Why a [`Function`] could not be lowered by this first ARMv6-M tracer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LowerError {
    /// The function did not pass [`lamella_ir::verify`].
    NotWellFormed,
    /// The function has control flow; only a single straight-line block is
    /// handled so far.
    ControlFlowUnsupported,
    /// The function uses more values than the trivial allocator has registers
    /// (eight: r0-r7).
    TooManyValues,
    /// A value's type is not an integer; floats and references are not lowered yet.
    NonIntegerValue,
    /// An instruction is outside the supported subset (so far only `ConstInt` and
    /// `Binary` addition).
    UnsupportedInstruction,
    /// A constant does not fit the 8-bit immediate this tracer can materialize.
    ConstantTooWide,
}

/// Lowers a straight-line, integer [`Function`] to ARMv6-M Thumb machine code via
/// the AAPCS convention. See the module documentation for the supported slice.
pub fn lower(func: &Function) -> Result<Vec<u8>, LowerError> {
    if lamella_ir::verify(func).is_err() {
        return Err(LowerError::NotWellFormed);
    }
    if func.blocks.len() != 1 {
        return Err(LowerError::ControlFlowUnsupported);
    }
    if func.value_types.len() > 8 {
        return Err(LowerError::TooManyValues);
    }
    if func.value_types.iter().any(|ty| !ty.is_integer()) {
        return Err(LowerError::NonIntegerValue);
    }

    let reg = |value: ValueId| Reg::new(value.0 as u8).unwrap_or(Reg::R0);

    let block = &func.blocks[0];
    let mut enc = Encoder::new();

    for (result, inst) in &block.insts {
        match inst {
            Inst::ConstInt { value, .. } => {
                let imm = u8::try_from(*value).map_err(|_| LowerError::ConstantTooWide)?;
                enc.movs_imm(reg(*result), imm)
                    .map_err(|_| LowerError::TooManyValues)?;
            }
            Inst::Binary { op, lhs, rhs } => {
                let (d, a, b) = (reg(*result), reg(*lhs), reg(*rhs));
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
            _ => return Err(LowerError::UnsupportedInstruction),
        }
    }

    match &block.terminator {
        Some(Terminator::Return(value)) => {
            if let Some(v) = value {
                let src = reg(*v);
                if src != Reg::R0 {
                    enc.mov_reg(Reg::R0, src);
                }
            }
            enc.bx(Reg::LR);
        }
        _ => return Err(LowerError::ControlFlowUnsupported),
    }

    enc.finish()
        .map(|assembled| assembled.bytes)
        .map_err(|_| LowerError::UnsupportedInstruction)
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
    fn control_flow_is_rejected_not_miscompiled() {
        let func = Function {
            params: Vec::new(),
            ret: None,
            value_types: Vec::new(),
            entry: BlockId(0),
            blocks: vec![
                BasicBlock {
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Some(Terminator::Jump {
                        target: BlockId(1),
                        args: Vec::new(),
                    }),
                },
                BasicBlock {
                    params: Vec::new(),
                    insts: Vec::new(),
                    terminator: Some(Terminator::Return(None)),
                },
            ],
        };
        assert_eq!(lower(&func), Err(LowerError::ControlFlowUnsupported));
    }
}
