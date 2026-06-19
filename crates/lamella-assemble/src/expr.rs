//! Lowering a bound expression to CIL (ECMA-335 1st ed, Partition III).

use crate::frame::{Frame, Slot};
use alloc::vec::Vec;
use lamella_binder::{BoundExpr, BoundExprKind, ConversionKind, SpecialType, TypeSymbol};
use lamella_cil::{Instruction, Opcode, Operand};
use lamella_syntax::ast::{BinaryOperator, Literal, UnaryOperator};

/// Why an expression could not be lowered to CIL yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitError {
    /// A construct the emitter does not handle yet, with a short reason.
    Unsupported(&'static str),
}

/// Lowers `expr` to CIL, appending the instructions that leave its value on the
/// evaluation stack. `frame` resolves local and argument names to slots.
pub fn emit_expression(
    expr: &BoundExpr,
    frame: &Frame,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match &expr.kind {
        BoundExprKind::Literal(literal) => emit_literal(literal, &expr.ty, out),
        BoundExprKind::Local(name) => emit_local(name, frame, out),
        BoundExprKind::Binary {
            operator,
            left,
            right,
        } => {
            emit_expression(left, frame, out)?;
            emit_expression(right, frame, out)?;
            emit_binary(*operator, out)
        }
        BoundExprKind::Unary { operator, operand } => {
            emit_expression(operand, frame, out)?;
            emit_unary(*operator, out)
        }
        BoundExprKind::Checked(inner) | BoundExprKind::Unchecked(inner) => {
            emit_expression(inner, frame, out)
        }
        BoundExprKind::Conversion {
            operand,
            conversion,
        } => {
            emit_expression(operand, frame, out)?;
            emit_conversion(*conversion, &expr.ty, out)
        }
        _ => Err(EmitError::Unsupported(
            "this expression form is not lowered yet",
        )),
    }
}

/// Emits the instruction (if any) for a conversion whose target is `target`.
fn emit_conversion(
    conversion: ConversionKind,
    target: &TypeSymbol,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match conversion {
        ConversionKind::ImplicitNumeric => {
            out.push(Instruction::simple(numeric_conversion(target)?));
            Ok(())
        }
        ConversionKind::ImplicitReference => Ok(()),
        ConversionKind::Boxing => Err(EmitError::Unsupported("boxing (needs a metadata token)")),
    }
}

/// The `conv.*` opcode that produces a value of the numeric `target` type.
fn numeric_conversion(target: &TypeSymbol) -> Result<Opcode, EmitError> {
    let TypeSymbol::Special(special) = target else {
        return Err(EmitError::Unsupported(
            "numeric conversion to a non-primitive",
        ));
    };
    Ok(match special {
        SpecialType::SByte => Opcode::ConvI1,
        SpecialType::Byte => Opcode::ConvU1,
        SpecialType::Int16 => Opcode::ConvI2,
        SpecialType::UInt16 | SpecialType::Char => Opcode::ConvU2,
        SpecialType::Int32 => Opcode::ConvI4,
        SpecialType::UInt32 => Opcode::ConvU4,
        SpecialType::Int64 => Opcode::ConvI8,
        SpecialType::UInt64 => Opcode::ConvU8,
        SpecialType::Single => Opcode::ConvR4,
        SpecialType::Double => Opcode::ConvR8,
        _ => {
            return Err(EmitError::Unsupported(
                "numeric conversion to a non-numeric type",
            ));
        }
    })
}

pub(crate) fn emit_local(
    name: &str,
    frame: &Frame,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match frame.slot(name) {
        Some(Slot::Argument(slot)) => {
            out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(slot)));
        }
        Some(Slot::Local(slot)) => {
            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
        }
        None => return Err(EmitError::Unsupported("read of a name with no frame slot")),
    }
    Ok(())
}

fn emit_literal(
    literal: &Literal,
    ty: &TypeSymbol,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match literal {
        Literal::Integer { value, .. } => {
            if matches!(
                ty,
                TypeSymbol::Special(SpecialType::Int64 | SpecialType::UInt64)
            ) {
                out.push(Instruction::new(
                    Opcode::LdcI8,
                    Operand::Int64(*value as i64),
                ));
            } else {
                out.push(load_i4(*value as i32));
            }
        }
        Literal::Boolean(value) => out.push(load_i4(i32::from(*value))),
        Literal::Character(value) => out.push(load_i4(i32::from(*value))),
        Literal::Null => out.push(Instruction::simple(Opcode::Ldnull)),
        Literal::String(_) => {
            return Err(EmitError::Unsupported(
                "string literal (needs the user-string heap)",
            ));
        }
        Literal::Real { .. } => {
            return Err(EmitError::Unsupported(
                "real literal (value not retained by the parser)",
            ));
        }
    }
    Ok(())
}

fn emit_binary(operator: BinaryOperator, out: &mut Vec<Instruction>) -> Result<(), EmitError> {
    use BinaryOperator as Op;
    let opcode = match operator {
        Op::Add => Opcode::Add,
        Op::Subtract => Opcode::Sub,
        Op::Multiply => Opcode::Mul,
        Op::Divide => Opcode::Div,
        Op::Modulo => Opcode::Rem,
        Op::BitwiseAnd => Opcode::And,
        Op::BitwiseOr => Opcode::Or,
        Op::BitwiseXor => Opcode::Xor,
        Op::LeftShift => Opcode::Shl,
        Op::RightShift => Opcode::Shr,
        Op::Equal => Opcode::Ceq,
        Op::GreaterThan => Opcode::Cgt,
        Op::LessThan => Opcode::Clt,
        Op::NotEqual => return emit_negated(Opcode::Ceq, out),
        Op::LessThanOrEqual => return emit_negated(Opcode::Cgt, out),
        Op::GreaterThanOrEqual => return emit_negated(Opcode::Clt, out),
        Op::LogicalAnd | Op::LogicalOr => {
            return Err(EmitError::Unsupported(
                "short-circuit && / || (needs branches)",
            ));
        }
    };
    out.push(Instruction::simple(opcode));
    Ok(())
}

fn emit_unary(operator: UnaryOperator, out: &mut Vec<Instruction>) -> Result<(), EmitError> {
    match operator {
        UnaryOperator::Minus => out.push(Instruction::simple(Opcode::Neg)),
        UnaryOperator::Complement => out.push(Instruction::simple(Opcode::Not)),
        UnaryOperator::Not => push_logical_negation(out),
        UnaryOperator::Plus => {}
        UnaryOperator::PreIncrement | UnaryOperator::PreDecrement => {
            return Err(EmitError::Unsupported("++/-- (needs an lvalue store)"));
        }
    }
    Ok(())
}

/// Emits a comparison and then negates its boolean result.
fn emit_negated(comparison: Opcode, out: &mut Vec<Instruction>) -> Result<(), EmitError> {
    out.push(Instruction::simple(comparison));
    push_logical_negation(out);
    Ok(())
}

/// Negates the boolean on the stack: `value == 0`.
fn push_logical_negation(out: &mut Vec<Instruction>) {
    out.push(load_i4(0));
    out.push(Instruction::simple(Opcode::Ceq));
}

fn load_i4(value: i32) -> Instruction {
    Instruction::new(Opcode::LdcI4, Operand::Int32(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_binder::bind_expression;
    use lamella_syntax::parser::parse_expression;

    fn emit(source: &str) -> Vec<Instruction> {
        let expr = bind_expression(&parse_expression(source).expr);
        let mut out = Vec::new();
        emit_expression(&expr, &Frame::empty(), &mut out).expect("should lower");
        out
    }

    fn i4(value: i32) -> Instruction {
        Instruction::new(Opcode::LdcI4, Operand::Int32(value))
    }
    fn op(opcode: Opcode) -> Instruction {
        Instruction::simple(opcode)
    }

    #[test]
    fn integer_arithmetic_lowers_left_right_operator() {
        assert_eq!(emit("7"), [i4(7)]);
        assert_eq!(emit("1 + 2"), [i4(1), i4(2), op(Opcode::Add)]);
        assert_eq!(
            emit("1 + 2 * 3"),
            [i4(1), i4(2), i4(3), op(Opcode::Mul), op(Opcode::Add)]
        );
        assert_eq!(
            emit("10L"),
            [Instruction::new(Opcode::LdcI8, Operand::Int64(10))]
        );
    }

    #[test]
    fn comparisons_use_ceq_cgt_clt_and_negation() {
        assert_eq!(emit("1 == 2"), [i4(1), i4(2), op(Opcode::Ceq)]);
        assert_eq!(emit("1 < 2"), [i4(1), i4(2), op(Opcode::Clt)]);
        assert_eq!(
            emit("1 != 2"),
            [i4(1), i4(2), op(Opcode::Ceq), i4(0), op(Opcode::Ceq)]
        );
        assert_eq!(
            emit("1 <= 2"),
            [i4(1), i4(2), op(Opcode::Cgt), i4(0), op(Opcode::Ceq)]
        );
    }

    #[test]
    fn unary_and_bitwise() {
        assert_eq!(emit("-5"), [i4(5), op(Opcode::Neg)]);
        assert_eq!(emit("~3"), [i4(3), op(Opcode::Not)]);
        assert_eq!(emit("true"), [i4(1)]);
        assert_eq!(emit("!true"), [i4(1), i4(0), op(Opcode::Ceq)]);
        assert_eq!(emit("5 & 3"), [i4(5), i4(3), op(Opcode::And)]);
    }
}
