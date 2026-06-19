//! Lowering a bound expression to CIL (ECMA-335 1st ed, Partition III).

use crate::frame::{Frame, Slot};
use crate::tokens::Tokens;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_binder::{
    BoundExpr, BoundExprKind, ConversionKind, FieldReference, SpecialType, TypeSymbol,
};
use lamella_cil::{Instruction, Opcode, Operand};
use lamella_syntax::ast::{BinaryOperator, Literal, UnaryOperator};

/// Why an expression could not be lowered to CIL yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitError {
    /// A construct the emitter does not handle yet, with a short reason.
    Unsupported(&'static str),
}

/// Lowers `expr` to CIL, appending the instructions that leave its value on the
/// evaluation stack. `frame` resolves variable names to slots, `tokens` resolves a
/// called method or accessed field to its token.
pub fn emit_expression(
    expr: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match &expr.kind {
        BoundExprKind::Literal(literal) => emit_literal(literal, &expr.ty, tokens, out),
        BoundExprKind::Local(name) => emit_local(name, frame, out),
        BoundExprKind::Binary {
            operator,
            left,
            right,
        } => match operator {
            BinaryOperator::LogicalAnd => {
                emit_short_circuit(left, right, false, frame, tokens, out)
            }
            BinaryOperator::LogicalOr => emit_short_circuit(left, right, true, frame, tokens, out),
            _ => {
                emit_expression(left, frame, tokens, out)?;
                emit_expression(right, frame, tokens, out)?;
                emit_binary(*operator, out)
            }
        },
        BoundExprKind::Unary { operator, operand } => {
            emit_expression(operand, frame, tokens, out)?;
            emit_unary(*operator, out)
        }
        BoundExprKind::Checked(inner) | BoundExprKind::Unchecked(inner) => {
            emit_expression(inner, frame, tokens, out)
        }
        BoundExprKind::Conversion {
            operand,
            conversion,
        } => {
            emit_expression(operand, frame, tokens, out)?;
            emit_conversion(*conversion, &expr.ty, out)
        }
        BoundExprKind::Cast { operand } => {
            emit_expression(operand, frame, tokens, out)?;
            emit_cast(&operand.ty, &expr.ty, tokens, out)
        }
        BoundExprKind::Call {
            callee,
            arguments,
            method,
        } => emit_call(method.as_ref(), callee, arguments, frame, tokens, out),
        BoundExprKind::FieldAccess {
            receiver, field, ..
        } => emit_field_load(field.as_ref(), receiver, frame, tokens, out),
        BoundExprKind::PropertyAccess { receiver, name } => {
            emit_property_load(receiver, name, frame, tokens, out)
        }
        BoundExprKind::This | BoundExprKind::Base => {
            out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(0)));
            Ok(())
        }
        BoundExprKind::ObjectCreation {
            arguments,
            constructor,
        } => emit_new(constructor.as_ref(), arguments, frame, tokens, out),
        BoundExprKind::ArrayCreation { lengths } => {
            emit_array_creation(&expr.ty, lengths, frame, tokens, out)
        }
        BoundExprKind::ElementAccess { receiver, indices } => {
            emit_element_load(&expr.ty, receiver, indices, frame, tokens, out)
        }
        BoundExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => emit_conditional(condition, when_true, when_false, frame, tokens, out),
        _ => Err(EmitError::Unsupported(
            "this expression form is not lowered yet",
        )),
    }
}

/// Lowers `a && b` (or `a || b`): evaluate `a`, and short-circuit to the constant
/// result (`false` for `&&`, `true` for `||`) when `a` already decides it.
fn emit_short_circuit(
    left: &BoundExpr,
    right: &BoundExpr,
    is_or: bool,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    emit_expression(left, frame, tokens, out)?;
    let short = out.len();
    let branch = if is_or {
        Opcode::Brtrue
    } else {
        Opcode::Brfalse
    };
    out.push(Instruction::new(branch, Operand::Target(0)));
    emit_expression(right, frame, tokens, out)?;
    let to_end = out.len();
    out.push(Instruction::new(Opcode::Br, Operand::Target(0)));
    out[short].operand = Operand::Target(out.len() as u32);
    out.push(load_i4(i32::from(is_or)));
    out[to_end].operand = Operand::Target(out.len() as u32);
    Ok(())
}

/// Lowers `c ? a : b`: evaluate `c`, branch to `b` when false, else `a` then jump
/// past `b`. Both arms leave their value on the stack.
fn emit_conditional(
    condition: &BoundExpr,
    when_true: &BoundExpr,
    when_false: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    emit_expression(condition, frame, tokens, out)?;
    let to_else = out.len();
    out.push(Instruction::new(Opcode::Brfalse, Operand::Target(0)));
    emit_expression(when_true, frame, tokens, out)?;
    let to_end = out.len();
    out.push(Instruction::new(Opcode::Br, Operand::Target(0)));
    out[to_else].operand = Operand::Target(out.len() as u32);
    emit_expression(when_false, frame, tokens, out)?;
    out[to_end].operand = Operand::Target(out.len() as u32);
    Ok(())
}

/// Lowers `new T[n]`: the length is pushed, then `newarr` names the element type.
fn emit_array_creation(
    array_ty: &TypeSymbol,
    lengths: &[BoundExpr],
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let TypeSymbol::Array { element, rank } = array_ty else {
        return Err(EmitError::Unsupported("array creation of a non-array type"));
    };
    if *rank != 1 || lengths.len() != 1 {
        return Err(EmitError::Unsupported(
            "only single-dimension arrays are lowered",
        ));
    }
    emit_expression(&lengths[0], frame, tokens, out)?;
    let token = tokens
        .type_token(element)
        .ok_or(EmitError::Unsupported("array element type has no token"))?;
    out.push(Instruction::new(Opcode::Newarr, Operand::Token(token)));
    Ok(())
}

/// Lowers `a[i]`: the array and index are pushed, then `ldelem.*` for the element.
fn emit_element_load(
    element_ty: &TypeSymbol,
    receiver: &BoundExpr,
    indices: &[BoundExpr],
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if indices.len() != 1 {
        return Err(EmitError::Unsupported(
            "only single-dimension element access is lowered",
        ));
    }
    emit_expression(receiver, frame, tokens, out)?;
    emit_expression(&indices[0], frame, tokens, out)?;
    out.push(Instruction::simple(ldelem_opcode(element_ty)?));
    Ok(())
}

/// Lowers `a[i] = v`: array, index, and value are pushed, then `stelem.*`. Shared
/// by assignment emission.
pub(crate) fn emit_element_store(
    element_ty: &TypeSymbol,
    receiver: &BoundExpr,
    indices: &[BoundExpr],
    value: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if indices.len() != 1 {
        return Err(EmitError::Unsupported(
            "only single-dimension element access is lowered",
        ));
    }
    emit_expression(receiver, frame, tokens, out)?;
    emit_expression(&indices[0], frame, tokens, out)?;
    emit_expression(value, frame, tokens, out)?;
    out.push(Instruction::simple(stelem_opcode(element_ty)?));
    Ok(())
}

/// The `ldelem.*` opcode for reading an element of the given type.
pub(crate) fn ldelem_opcode(element_ty: &TypeSymbol) -> Result<Opcode, EmitError> {
    Ok(match element_ty {
        TypeSymbol::Special(special) => match special {
            SpecialType::SByte => Opcode::LdelemI1,
            SpecialType::Byte | SpecialType::Boolean => Opcode::LdelemU1,
            SpecialType::Int16 => Opcode::LdelemI2,
            SpecialType::UInt16 | SpecialType::Char => Opcode::LdelemU2,
            SpecialType::Int32 => Opcode::LdelemI4,
            SpecialType::UInt32 => Opcode::LdelemU4,
            SpecialType::Int64 | SpecialType::UInt64 => Opcode::LdelemI8,
            SpecialType::Single => Opcode::LdelemR4,
            SpecialType::Double => Opcode::LdelemR8,
            SpecialType::String | SpecialType::Object => Opcode::LdelemRef,
            _ => {
                return Err(EmitError::Unsupported(
                    "element type not lowered for ldelem",
                ));
            }
        },
        TypeSymbol::Named(_) | TypeSymbol::Array { .. } => Opcode::LdelemRef,
        TypeSymbol::Error => return Err(EmitError::Unsupported("element access of an error type")),
    })
}

/// The `stelem.*` opcode for writing an element of the given type.
fn stelem_opcode(element_ty: &TypeSymbol) -> Result<Opcode, EmitError> {
    Ok(match element_ty {
        TypeSymbol::Special(special) => match special {
            SpecialType::SByte | SpecialType::Byte | SpecialType::Boolean => Opcode::StelemI1,
            SpecialType::Int16 | SpecialType::UInt16 | SpecialType::Char => Opcode::StelemI2,
            SpecialType::Int32 | SpecialType::UInt32 => Opcode::StelemI4,
            SpecialType::Int64 | SpecialType::UInt64 => Opcode::StelemI8,
            SpecialType::Single => Opcode::StelemR4,
            SpecialType::Double => Opcode::StelemR8,
            SpecialType::String | SpecialType::Object => Opcode::StelemRef,
            _ => {
                return Err(EmitError::Unsupported(
                    "element type not lowered for stelem",
                ));
            }
        },
        TypeSymbol::Named(_) | TypeSymbol::Array { .. } => Opcode::StelemRef,
        TypeSymbol::Error => return Err(EmitError::Unsupported("element store of an error type")),
    })
}

/// Lowers object creation: each constructor argument is pushed, then `newobj`
/// names the constructor by token and leaves the new instance on the stack.
fn emit_new(
    constructor: Option<&lamella_binder::MethodReference>,
    arguments: &[BoundExpr],
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let Some(constructor) = constructor else {
        return Err(EmitError::Unsupported(
            "an object creation that did not resolve",
        ));
    };
    for argument in arguments {
        emit_expression(argument, frame, tokens, out)?;
    }
    let token = tokens
        .method(
            &constructor.declaring_type,
            &constructor.name,
            &constructor.parameters,
        )
        .ok_or(EmitError::Unsupported("constructor outside this module"))?;
    out.push(Instruction::new(Opcode::Newobj, Operand::Token(token)));
    Ok(())
}

/// Lowers a call. An instance call pushes the receiver first and dispatches with
/// `callvirt`; a static call uses `call`. Then the arguments are pushed and the
/// target named by token. Same-module targets only for now; external calls follow.
fn emit_call(
    method: Option<&lamella_binder::MethodReference>,
    callee: &BoundExpr,
    arguments: &[BoundExpr],
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let Some(method) = method else {
        return Err(EmitError::Unsupported("a call that did not resolve"));
    };
    if !method.is_static {
        match &callee.kind {
            BoundExprKind::MethodGroup { receiver, .. } => {
                emit_expression(receiver, frame, tokens, out)?;
            }
            _ => return Err(EmitError::Unsupported("an instance call with no receiver")),
        }
    }
    for argument in arguments {
        emit_expression(argument, frame, tokens, out)?;
    }
    let token = tokens
        .method(&method.declaring_type, &method.name, &method.parameters)
        .ok_or(EmitError::Unsupported(
            "call to a method outside this module",
        ))?;
    let opcode = if method.is_static {
        Opcode::Call
    } else {
        Opcode::Callvirt
    };
    out.push(Instruction::new(opcode, Operand::Token(token)));
    Ok(())
}

/// Lowers a field read: `ldsfld` for a static field, the receiver then `ldfld`
/// for an instance field.
fn emit_field_load(
    field: Option<&FieldReference>,
    receiver: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let Some(field) = field else {
        return Err(EmitError::Unsupported(
            "a field access that did not resolve",
        ));
    };
    if let Some(value) = field.constant {
        out.push(load_i4(value as i32));
        return Ok(());
    }
    let token = tokens
        .field(&field.declaring_type, &field.name)
        .ok_or(EmitError::Unsupported("field outside this module"))?;
    if field.is_static {
        out.push(Instruction::new(Opcode::Ldsfld, Operand::Token(token)));
    } else {
        emit_expression(receiver, frame, tokens, out)?;
        out.push(Instruction::new(Opcode::Ldfld, Operand::Token(token)));
    }
    Ok(())
}

/// Lowers a field write: the value (and receiver, if an instance field) are on the
/// stack, then `stsfld`/`stfld` stores. Shared by assignment emission.
pub(crate) fn emit_field_store(
    field: Option<&FieldReference>,
    receiver: &BoundExpr,
    value: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let Some(field) = field else {
        return Err(EmitError::Unsupported("a field store that did not resolve"));
    };
    let token = tokens
        .field(&field.declaring_type, &field.name)
        .ok_or(EmitError::Unsupported("field outside this module"))?;
    if field.is_static {
        emit_expression(value, frame, tokens, out)?;
        out.push(Instruction::new(Opcode::Stsfld, Operand::Token(token)));
    } else {
        emit_expression(receiver, frame, tokens, out)?;
        emit_expression(value, frame, tokens, out)?;
        out.push(Instruction::new(Opcode::Stfld, Operand::Token(token)));
    }
    Ok(())
}

/// Lowers a property read: the receiver (for an instance property) then a call to
/// the `get_Name` accessor. A static property is accessed through its type.
fn emit_property_load(
    receiver: &BoundExpr,
    name: &str,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let is_static = matches!(receiver.kind, BoundExprKind::TypeReference(_));
    if !is_static {
        emit_expression(receiver, frame, tokens, out)?;
    }
    let token = tokens
        .method(&receiver.ty, &accessor_name("get_", name), &[])
        .ok_or(EmitError::Unsupported(
            "property getter outside this module",
        ))?;
    let opcode = if is_static {
        Opcode::Call
    } else {
        Opcode::Callvirt
    };
    out.push(Instruction::new(opcode, Operand::Token(token)));
    Ok(())
}

/// Lowers a property write: the receiver (for an instance property) and value,
/// then a call to the `set_Name` accessor. Shared by assignment emission.
pub(crate) fn emit_property_store(
    property_ty: &TypeSymbol,
    receiver: &BoundExpr,
    name: &str,
    value: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let is_static = matches!(receiver.kind, BoundExprKind::TypeReference(_));
    if !is_static {
        emit_expression(receiver, frame, tokens, out)?;
    }
    emit_expression(value, frame, tokens, out)?;
    let token = tokens
        .method(
            &receiver.ty,
            &accessor_name("set_", name),
            core::slice::from_ref(property_ty),
        )
        .ok_or(EmitError::Unsupported(
            "property setter outside this module",
        ))?;
    let opcode = if is_static {
        Opcode::Call
    } else {
        Opcode::Callvirt
    };
    out.push(Instruction::new(opcode, Operand::Token(token)));
    Ok(())
}

/// The `get_`/`set_` accessor method name for a property.
fn accessor_name(prefix: &str, property: &str) -> String {
    let mut name = String::from(prefix);
    name.push_str(property);
    name
}

/// Emits the instruction (if any) for an explicit cast from `from` to `to`. An
/// identity cast is a no-op; a cast to an enum or a numeric/char type is the
/// corresponding `conv.*` (an enum's operand is already its underlying integer, so
/// `conv.i4` is the v1 underlying conversion). A reference downcast (`castclass`)
/// and unboxing arrive with the reference-type work.
fn emit_cast(
    from: &TypeSymbol,
    to: &TypeSymbol,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if from == to {
        return Ok(());
    }
    if tokens.is_enum(to) {
        out.push(Instruction::simple(Opcode::ConvI4));
        return Ok(());
    }
    if matches!(to, TypeSymbol::Special(_)) {
        out.push(Instruction::simple(numeric_conversion(to)?));
        return Ok(());
    }
    Err(EmitError::Unsupported("this cast is not lowered yet"))
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
    tokens: &Tokens,
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
        Literal::String(text) => {
            let token = tokens.string(text).ok_or(EmitError::Unsupported(
                "string literal was not interned before emission",
            ))?;
            out.push(Instruction::new(Opcode::Ldstr, Operand::Token(token)));
        }
        Literal::Real { bits, .. } => {
            let value = f64::from_bits(*bits);
            match ty {
                TypeSymbol::Special(SpecialType::Single) => {
                    out.push(Instruction::new(
                        Opcode::LdcR4,
                        Operand::Float32(value as f32),
                    ));
                }
                TypeSymbol::Special(SpecialType::Double) => {
                    out.push(Instruction::new(Opcode::LdcR8, Operand::Float64(value)));
                }
                _ => {
                    return Err(EmitError::Unsupported(
                        "decimal literals are not lowered yet",
                    ));
                }
            }
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
        emit_expression(&expr, &Frame::empty(), &Tokens::new(), &mut out).expect("should lower");
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
