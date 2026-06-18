//! Lowering CIL method bodies to the middle IR by abstract interpretation.

use alloc::vec;
use alloc::vec::Vec;

use lamella_cil::{MethodBodyImage, Opcode, Operand};
use lamella_ir::{BasicBlock, BinOp, BlockId, CmpOp, Function, Inst, MirType, Terminator, ValueId};

/// Why a method body could not be lowered to MIR.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CilError {
    /// An opcode needed more operands than the evaluation stack held.
    StackUnderflow,
    /// The method body did not end in `ret` (control flow is not lowered yet).
    NoReturn,
    /// An opcode's decoded operand was not the shape the opcode requires.
    BadOperand,
    /// A CIL opcode this spike does not lower yet.
    Unsupported(Opcode),
}

/// Lowers a straight-line integer [`MethodBodyImage`] to a MIR [`Function`] by
/// abstract interpretation of the evaluation stack.
pub fn lower_method(body: &MethodBodyImage) -> Result<Function, CilError> {
    let mut value_types: Vec<MirType> = Vec::new();
    let mut stack: Vec<ValueId> = Vec::new();
    let mut insts: Vec<(ValueId, Inst)> = Vec::new();
    let mut terminator: Option<Terminator> = None;

    for instruction in body.code.iter() {
        match instruction.opcode {
            Opcode::Nop => {}
            Opcode::LdcI4M1 => push_const(&mut value_types, &mut stack, &mut insts, -1),
            Opcode::LdcI40 => push_const(&mut value_types, &mut stack, &mut insts, 0),
            Opcode::LdcI41 => push_const(&mut value_types, &mut stack, &mut insts, 1),
            Opcode::LdcI42 => push_const(&mut value_types, &mut stack, &mut insts, 2),
            Opcode::LdcI43 => push_const(&mut value_types, &mut stack, &mut insts, 3),
            Opcode::LdcI44 => push_const(&mut value_types, &mut stack, &mut insts, 4),
            Opcode::LdcI45 => push_const(&mut value_types, &mut stack, &mut insts, 5),
            Opcode::LdcI46 => push_const(&mut value_types, &mut stack, &mut insts, 6),
            Opcode::LdcI47 => push_const(&mut value_types, &mut stack, &mut insts, 7),
            Opcode::LdcI48 => push_const(&mut value_types, &mut stack, &mut insts, 8),
            Opcode::LdcI4S => {
                let Operand::Int8(v) = &instruction.operand else {
                    return Err(CilError::BadOperand);
                };
                push_const(&mut value_types, &mut stack, &mut insts, i64::from(*v));
            }
            Opcode::LdcI4 => {
                let Operand::Int32(v) = &instruction.operand else {
                    return Err(CilError::BadOperand);
                };
                push_const(&mut value_types, &mut stack, &mut insts, i64::from(*v));
            }
            Opcode::Add => binary(&mut value_types, &mut stack, &mut insts, BinOp::Add)?,
            Opcode::Sub => binary(&mut value_types, &mut stack, &mut insts, BinOp::Sub)?,
            Opcode::Mul => binary(&mut value_types, &mut stack, &mut insts, BinOp::Mul)?,
            Opcode::And => binary(&mut value_types, &mut stack, &mut insts, BinOp::And)?,
            Opcode::Or => binary(&mut value_types, &mut stack, &mut insts, BinOp::Or)?,
            Opcode::Xor => binary(&mut value_types, &mut stack, &mut insts, BinOp::Xor)?,
            Opcode::Shl => binary(&mut value_types, &mut stack, &mut insts, BinOp::Shl)?,
            Opcode::Shr => binary(&mut value_types, &mut stack, &mut insts, BinOp::ShrSigned)?,
            Opcode::ShrUn => binary(&mut value_types, &mut stack, &mut insts, BinOp::ShrUnsigned)?,
            Opcode::Ceq => compare(&mut value_types, &mut stack, &mut insts, CmpOp::Eq)?,
            Opcode::Cgt => compare(&mut value_types, &mut stack, &mut insts, CmpOp::SignedGt)?,
            Opcode::CgtUn => compare(&mut value_types, &mut stack, &mut insts, CmpOp::UnsignedGt)?,
            Opcode::Clt => compare(&mut value_types, &mut stack, &mut insts, CmpOp::SignedLt)?,
            Opcode::CltUn => compare(&mut value_types, &mut stack, &mut insts, CmpOp::UnsignedLt)?,
            Opcode::Pop => {
                stack.pop().ok_or(CilError::StackUnderflow)?;
            }
            Opcode::Dup => {
                let top = *stack.last().ok_or(CilError::StackUnderflow)?;
                stack.push(top);
            }
            Opcode::Ret => {
                terminator = Some(Terminator::Return(stack.pop()));
                break;
            }
            other => return Err(CilError::Unsupported(other)),
        }
    }

    let terminator = terminator.ok_or(CilError::NoReturn)?;
    let ret = match &terminator {
        Terminator::Return(Some(v)) => value_types.get(v.index()).copied(),
        _ => None,
    };

    Ok(Function {
        params: Vec::new(),
        ret,
        value_types,
        entry: BlockId(0),
        blocks: vec![BasicBlock {
            params: Vec::new(),
            insts,
            terminator: Some(terminator),
        }],
    })
}

/// Defines a fresh MIR value of `ty` and returns its id.
fn new_value(value_types: &mut Vec<MirType>, ty: MirType) -> ValueId {
    let id = ValueId(value_types.len() as u32);
    value_types.push(ty);
    id
}

/// Pushes an integer constant: a new value defined by a `ConstInt`.
fn push_const(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    value: i64,
) {
    let result = new_value(value_types, MirType::I32);
    insts.push((
        result,
        Inst::ConstInt {
            ty: MirType::I32,
            value,
        },
    ));
    stack.push(result);
}

/// Pops two operands (CIL order: the top is the right operand) and pushes a
/// binary operation over them.
fn binary(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    op: BinOp,
) -> Result<(), CilError> {
    let rhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let lhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let result = new_value(value_types, MirType::I32);
    insts.push((result, Inst::Binary { op, lhs, rhs }));
    stack.push(result);
    Ok(())
}

/// Pops two operands and pushes a comparison yielding 0 or 1.
fn compare(
    value_types: &mut Vec<MirType>,
    stack: &mut Vec<ValueId>,
    insts: &mut Vec<(ValueId, Inst)>,
    op: CmpOp,
) -> Result<(), CilError> {
    let rhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let lhs = stack.pop().ok_or(CilError::StackUnderflow)?;
    let result = new_value(value_types, MirType::I32);
    insts.push((result, Inst::Compare { op, lhs, rhs }));
    stack.push(result);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cil::Instruction;

    /// `ldc.i4.s 40 ; ldc.i4.s 2 ; add ; ret`, the CIL of `fn() -> i32 { 40 + 2 }`.
    fn forty_plus_two() -> MethodBodyImage {
        MethodBodyImage {
            max_stack: 2,
            init_locals: false,
            local_var_sig: None,
            code: vec![
                Instruction::new(Opcode::LdcI4S, Operand::Int8(40)),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(2)),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]
            .into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        }
    }

    #[test]
    fn lowers_ldc_add_ret_to_a_returning_function() {
        let func = lower_method(&forty_plus_two()).unwrap();
        assert!(lamella_ir::verify(&func).is_ok());
        assert_eq!(func.value_types.len(), 3);
        assert_eq!(func.ret, Some(MirType::I32));
    }

    #[test]
    fn rejects_a_body_with_no_return() {
        let body = MethodBodyImage {
            max_stack: 1,
            init_locals: false,
            local_var_sig: None,
            code: vec![Instruction::simple(Opcode::Nop)].into_boxed_slice(),
            handlers: Vec::new().into_boxed_slice(),
        };
        assert_eq!(lower_method(&body), Err(CilError::NoReturn));
    }

    #[cfg(feature = "arm32")]
    #[test]
    fn ldc_add_ret_lowers_through_to_arm32() {
        let func = lower_method(&forty_plus_two()).unwrap();
        let bytes = crate::arm32::lower(&func).unwrap();
        assert_eq!(&bytes[bytes.len() - 2..], &[0x70, 0x47]);
    }
}
