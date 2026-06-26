//! Lowering a bound expression to CIL (ECMA-335 1st ed, Partition III).

use crate::frame::{Frame, Slot};
use crate::tokens::Tokens;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_binder::{
    BoundExpr, BoundExprKind, ConversionKind, FieldReference, SpecialType, TypeSymbol,
};
use lamella_cil::{Instruction, Opcode, Operand};
use lamella_syntax::ast::{BinaryOperator, Literal, PostfixOperator, UnaryOperator};

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
        BoundExprKind::Local(name) => emit_local(name, frame, tokens, out),
        BoundExprKind::Binary {
            operator,
            left,
            right,
            checked,
        } => match operator {
            BinaryOperator::LogicalAnd => {
                emit_short_circuit(left, right, false, frame, tokens, out)
            }
            BinaryOperator::LogicalOr => emit_short_circuit(left, right, true, frame, tokens, out),
            _ => {
                if emit_pointer_arithmetic(*operator, left, right, frame, tokens, out)? {
                    return Ok(());
                }
                emit_expression(left, frame, tokens, out)?;
                emit_expression(right, frame, tokens, out)?;
                let is_string =
                    |ty: &TypeSymbol| matches!(ty, TypeSymbol::Special(SpecialType::String));
                if matches!(operator, BinaryOperator::Add) && is_string(&expr.ty) {
                    let arg = if is_string(&left.ty) && is_string(&right.ty) {
                        SpecialType::String
                    } else {
                        SpecialType::Object
                    };
                    let arg = TypeSymbol::Special(arg);
                    let string = TypeSymbol::Special(SpecialType::String);
                    let token = tokens
                        .method(&string, "Concat", &[arg.clone(), arg])
                        .ok_or(EmitError::Unsupported("String.Concat was not minted"))?;
                    out.push(Instruction::new(Opcode::Call, Operand::Token(token)));
                    Ok(())
                } else if matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual)
                    && is_string(&left.ty)
                    && is_string(&right.ty)
                {
                    emit_string_equality(*operator == BinaryOperator::NotEqual, tokens, out)
                } else {
                    emit_binary(*operator, &left.ty, *checked, out)
                }
            }
        },
        BoundExprKind::Unary {
            operator: operator @ (UnaryOperator::PreIncrement | UnaryOperator::PreDecrement),
            operand,
        } => emit_step_expression(
            operand,
            false,
            *operator == UnaryOperator::PreIncrement,
            frame,
            tokens,
            out,
        ),
        BoundExprKind::Unary { operator, operand } => {
            emit_expression(operand, frame, tokens, out)?;
            emit_unary(*operator, out)
        }
        BoundExprKind::Postfix { operator, operand } => emit_step_expression(
            operand,
            true,
            *operator == PostfixOperator::Increment,
            frame,
            tokens,
            out,
        ),
        BoundExprKind::Checked(inner) | BoundExprKind::Unchecked(inner) => {
            emit_expression(inner, frame, tokens, out)
        }
        BoundExprKind::Conversion {
            operand,
            conversion,
        } => {
            emit_expression(operand, frame, tokens, out)?;
            if matches!(conversion, ConversionKind::Boxing) {
                let token = tokens
                    .type_token(&operand.ty)
                    .ok_or(EmitError::Unsupported(
                        "boxing a value type with no metadata token",
                    ))?;
                out.push(Instruction::new(Opcode::Box, Operand::Token(token)));
                Ok(())
            } else {
                emit_conversion(*conversion, &expr.ty, out)
            }
        }
        BoundExprKind::Cast { operand, checked } => {
            emit_expression(operand, frame, tokens, out)?;
            emit_cast(&operand.ty, &expr.ty, *checked, tokens, out)
        }
        BoundExprKind::Call {
            callee,
            arguments,
            method,
        } => emit_call(method.as_ref(), callee, arguments, frame, tokens, out),
        BoundExprKind::FieldAccess {
            receiver, field, ..
        } => emit_field_load(field.as_ref(), receiver, frame, tokens, out),
        BoundExprKind::PropertyAccess {
            receiver,
            declaring_type,
            name,
        } => emit_property_load(receiver, declaring_type, name, frame, tokens, out),
        BoundExprKind::This | BoundExprKind::Base => {
            out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(0)));
            Ok(())
        }
        BoundExprKind::ObjectCreation {
            arguments,
            constructor,
        } => emit_new(constructor.as_ref(), arguments, frame, tokens, out),
        BoundExprKind::DelegateCreation {
            delegate_type,
            target,
            receiver,
        } => emit_delegate_creation(
            delegate_type,
            target,
            receiver.as_deref(),
            frame,
            tokens,
            out,
        ),
        BoundExprKind::ArrayCreation { lengths, elements } => {
            emit_array_creation(&expr.ty, lengths, elements, frame, tokens, out)
        }
        BoundExprKind::ElementAccess { receiver, indices } => {
            emit_element_load(&expr.ty, receiver, indices, frame, tokens, out)
        }
        BoundExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => emit_conditional(condition, when_true, when_false, frame, tokens, out),
        BoundExprKind::TypeOf(target) => emit_typeof(target, tokens, out),
        BoundExprKind::SizeOf(target) => emit_sizeof(target, tokens, out),
        BoundExprKind::MakeRef(operand) => emit_makeref(operand, frame, tokens, out),
        BoundExprKind::RefType(reference) => emit_reftype(reference, frame, tokens, out),
        BoundExprKind::RefValue { reference, target } => {
            emit_refvalue(reference, target, frame, tokens, out)
        }
        BoundExprKind::StackAlloc { element, count } => {
            emit_expression(count, frame, tokens, out)?;
            emit_sizeof(element, tokens, out)?;
            out.push(Instruction::simple(Opcode::Mul));
            out.push(Instruction::simple(Opcode::Localloc));
            Ok(())
        }
        BoundExprKind::Dereference { operand } => {
            emit_expression(operand, frame, tokens, out)?;
            let TypeSymbol::Pointer(element) = &operand.ty else {
                return Err(EmitError::Unsupported("dereference of a non-pointer"));
            };
            out.push(Instruction::simple(ldind_opcode(element)));
            Ok(())
        }
        BoundExprKind::TypeTest {
            operation,
            operand,
            target,
        } => {
            emit_expression(operand, frame, tokens, out)?;
            let token = tokens.type_token(target).ok_or(EmitError::Unsupported(
                "a type test against a type with no metadata token",
            ))?;
            out.push(Instruction::new(Opcode::Isinst, Operand::Token(token)));
            if matches!(operation, lamella_syntax::ast::TypeTestOperation::Is) {
                out.push(Instruction::simple(Opcode::Ldnull));
                out.push(Instruction::simple(Opcode::CgtUn));
            }
            Ok(())
        }
        BoundExprKind::Assignment {
            operator: lamella_syntax::ast::AssignmentOperator::Assign,
            target,
            value,
        } => match &target.kind {
            BoundExprKind::Local(name) if frame.byref(name).is_none() => {
                emit_expression(value, frame, tokens, out)?;
                out.push(Instruction::simple(Opcode::Dup));
                crate::method::store_to(frame, name, out)
            }
            _ => Err(EmitError::Unsupported(
                "assignment as an expression is lowered only to a local",
            )),
        },
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
    elements: &[BoundExpr],
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let TypeSymbol::Array { element, rank } = array_ty else {
        return Err(EmitError::Unsupported("array creation of a non-array type"));
    };
    if *rank >= 2 {
        for length in lengths {
            emit_expression(length, frame, tokens, out)?;
        }
        let ctor = tokens
            .method(array_ty, ".ctor", &array_int_params(lengths.len()))
            .ok_or(EmitError::Unsupported("array .ctor was not minted"))?;
        out.push(Instruction::new(Opcode::Newobj, Operand::Token(ctor)));
        return Ok(());
    }
    let element_token = tokens
        .type_token(element)
        .ok_or(EmitError::Unsupported("array element type has no token"))?;
    if !elements.is_empty() || lengths.is_empty() {
        if let Some(length) = lengths.first() {
            emit_expression(length, frame, tokens, out)?;
        } else {
            out.push(Instruction::new(
                Opcode::LdcI4,
                Operand::Int32(elements.len() as i32),
            ));
        }
        out.push(Instruction::new(Opcode::Newarr, Operand::Token(element_token)));
        let by_address = tokens.is_struct(element)
            || tokens.is_enum(element)
            || matches!(&**element, TypeSymbol::Special(SpecialType::Decimal));
        for (index, value) in elements.iter().enumerate() {
            out.push(Instruction::simple(Opcode::Dup));
            out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(index as i32)));
            if by_address {
                out.push(Instruction::new(Opcode::Ldelema, Operand::Token(element_token)));
                emit_expression(value, frame, tokens, out)?;
                out.push(Instruction::new(Opcode::Stobj, Operand::Token(element_token)));
            } else {
                emit_expression(value, frame, tokens, out)?;
                out.push(Instruction::simple(stelem_opcode(element)?));
            }
        }
        return Ok(());
    }
    if lengths.len() != 1 {
        return Err(EmitError::Unsupported(
            "a single-dimension array takes one length",
        ));
    }
    emit_expression(&lengths[0], frame, tokens, out)?;
    out.push(Instruction::new(Opcode::Newarr, Operand::Token(element_token)));
    Ok(())
}

/// The `int32` parameter-key types of an array's `.ctor`/`Get`/`Set` (one per
/// dimension), matching how the member tokens are recorded in the pre-pass.
pub(crate) fn array_int_params(rank: usize) -> Vec<TypeSymbol> {
    (0..rank)
        .map(|_| TypeSymbol::Special(SpecialType::Int32))
        .collect()
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
    if indices.len() >= 2 {
        emit_expression(receiver, frame, tokens, out)?;
        for index in indices {
            emit_expression(index, frame, tokens, out)?;
        }
        let get = tokens
            .method(&receiver.ty, "Get", &array_int_params(indices.len()))
            .ok_or(EmitError::Unsupported("array Get was not minted"))?;
        out.push(Instruction::new(Opcode::Call, Operand::Token(get)));
        return Ok(());
    }
    if indices.len() != 1 {
        return Err(EmitError::Unsupported("element access needs an index"));
    }
    if matches!(receiver.ty, TypeSymbol::Pointer(_)) {
        emit_expression(receiver, frame, tokens, out)?;
        emit_expression(&indices[0], frame, tokens, out)?;
        emit_sizeof(element_ty, tokens, out)?;
        out.push(Instruction::simple(Opcode::Mul));
        out.push(Instruction::simple(Opcode::Add));
        out.push(Instruction::simple(ldind_opcode(element_ty)));
        return Ok(());
    }
    emit_expression(receiver, frame, tokens, out)?;
    emit_expression(&indices[0], frame, tokens, out)?;
    if matches!(receiver.ty, TypeSymbol::Special(SpecialType::String)) {
        let token = tokens
            .method(
                &receiver.ty,
                "get_Chars",
                &[TypeSymbol::Special(SpecialType::Int32)],
            )
            .ok_or(EmitError::Unsupported("String::get_Chars was not minted"))?;
        out.push(Instruction::new(Opcode::Callvirt, Operand::Token(token)));
        return Ok(());
    }
    if tokens.is_struct(element_ty)
        || tokens.is_enum(element_ty)
        || matches!(element_ty, TypeSymbol::Special(SpecialType::Decimal))
    {
        let token = tokens
            .type_token(element_ty)
            .ok_or(EmitError::Unsupported("array element type has no token"))?;
        out.push(Instruction::new(Opcode::Ldelema, Operand::Token(token)));
        out.push(Instruction::new(Opcode::Ldobj, Operand::Token(token)));
    } else {
        out.push(Instruction::simple(ldelem_opcode(element_ty)?));
    }
    Ok(())
}

/// Lowers `a[i] = v`: array, index, and value are pushed, then `stelem.*` (a value-type
/// element stores through its address). Shared by assignment emission.
pub(crate) fn emit_element_store(
    element_ty: &TypeSymbol,
    receiver: &BoundExpr,
    indices: &[BoundExpr],
    value: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if indices.len() >= 2 {
        emit_expression(receiver, frame, tokens, out)?;
        for index in indices {
            emit_expression(index, frame, tokens, out)?;
        }
        emit_expression(value, frame, tokens, out)?;
        let mut set_params = array_int_params(indices.len());
        set_params.push(element_ty.clone());
        let set = tokens
            .method(&receiver.ty, "Set", &set_params)
            .ok_or(EmitError::Unsupported("array Set was not minted"))?;
        out.push(Instruction::new(Opcode::Call, Operand::Token(set)));
        return Ok(());
    }
    if indices.len() != 1 {
        return Err(EmitError::Unsupported("element access needs an index"));
    }
    if matches!(receiver.ty, TypeSymbol::Pointer(_)) {
        emit_expression(receiver, frame, tokens, out)?;
        emit_expression(&indices[0], frame, tokens, out)?;
        emit_sizeof(element_ty, tokens, out)?;
        out.push(Instruction::simple(Opcode::Mul));
        out.push(Instruction::simple(Opcode::Add));
        emit_expression(value, frame, tokens, out)?;
        out.push(Instruction::simple(stind_opcode(element_ty)));
        return Ok(());
    }
    emit_expression(receiver, frame, tokens, out)?;
    emit_expression(&indices[0], frame, tokens, out)?;
    if tokens.is_struct(element_ty)
        || tokens.is_enum(element_ty)
        || matches!(element_ty, TypeSymbol::Special(SpecialType::Decimal))
    {
        let token = tokens
            .type_token(element_ty)
            .ok_or(EmitError::Unsupported("array element type has no token"))?;
        out.push(Instruction::new(Opcode::Ldelema, Operand::Token(token)));
        emit_expression(value, frame, tokens, out)?;
        out.push(Instruction::new(Opcode::Stobj, Operand::Token(token)));
    } else {
        emit_expression(value, frame, tokens, out)?;
        out.push(Instruction::simple(stelem_opcode(element_ty)?));
    }
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
        TypeSymbol::Pointer(_) => return Err(EmitError::Unsupported("ldelem on a pointer")),
        TypeSymbol::ByRef(_) => return Err(EmitError::Unsupported("ldelem on a byref")),
        TypeSymbol::Error => return Err(EmitError::Unsupported("element access of an error type")),
    })
}

/// The `stelem.*` opcode for writing an element of the given type.
pub(crate) fn stelem_opcode(element_ty: &TypeSymbol) -> Result<Opcode, EmitError> {
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
        TypeSymbol::Pointer(_) => return Err(EmitError::Unsupported("stelem on a pointer")),
        TypeSymbol::ByRef(_) => return Err(EmitError::Unsupported("stelem on a byref")),
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
    if arguments.is_empty() && tokens.is_struct(&constructor.declaring_type) {
        let type_token = tokens
            .type_token(&constructor.declaring_type)
            .ok_or(EmitError::Unsupported(
                "a value type with no metadata token for initobj",
            ))?;
        let slot = frame.reserve_local(&constructor.declaring_type);
        out.push(Instruction::new(Opcode::Ldloca, Operand::Variable(slot)));
        out.push(Instruction::new(Opcode::Initobj, Operand::Token(type_token)));
        out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
        return Ok(());
    }
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

/// Lowers `new D(method)`: push the target object (`ldnull` for a static target, else
/// the receiver), the function pointer (`ldftn target`), then `newobj D::.ctor`.
fn emit_delegate_creation(
    delegate_type: &TypeSymbol,
    target: &lamella_binder::MethodReference,
    receiver: Option<&BoundExpr>,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match receiver {
        Some(receiver) => emit_expression(receiver, frame, tokens, out)?,
        None => out.push(Instruction::simple(Opcode::Ldnull)),
    }
    let target_token = tokens
        .method(&target.declaring_type, &target.name, &target.parameters)
        .ok_or(EmitError::Unsupported(
            "delegate target outside this module",
        ))?;
    out.push(Instruction::new(
        Opcode::Ldftn,
        Operand::Token(target_token),
    ));
    let ctor_token = tokens
        .method(delegate_type, ".ctor", &[])
        .ok_or(EmitError::Unsupported(
            "delegate constructor was not emitted",
        ))?;
    out.push(Instruction::new(Opcode::Newobj, Operand::Token(ctor_token)));
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
    let receiver = match &callee.kind {
        BoundExprKind::MethodGroup { receiver, .. } => Some(&**receiver),
        _ => None,
    };
    let value_type_receiver = match receiver {
        Some(r) if is_value_type(&r.ty, tokens) => Some(&r.ty),
        _ => None,
    };
    let is_base_call = matches!(receiver, Some(r) if matches!(r.kind, BoundExprKind::Base));
    let inherited_value_call =
        matches!(value_type_receiver, Some(ty) if !same_type(&method.declaring_type, ty));
    if !method.is_static {
        match &callee.kind {
            BoundExprKind::MethodGroup { receiver, .. } => {
                if inherited_value_call {
                    emit_expression(receiver, frame, tokens, out)?;
                    let box_token =
                        tokens.type_token(&receiver.ty).ok_or(EmitError::Unsupported(
                            "a virtual call on a value type with no metadata token",
                        ))?;
                    out.push(Instruction::new(Opcode::Box, Operand::Token(box_token)));
                } else if value_type_receiver.is_some() {
                    emit_value_type_receiver(receiver, frame, tokens, out)?;
                } else {
                    emit_expression(receiver, frame, tokens, out)?;
                }
            }
            _ => emit_expression(callee, frame, tokens, out)?,
        }
    }
    for argument in arguments {
        if let BoundExprKind::Ref { operand, .. } = &argument.kind {
            emit_ref_argument(operand, frame, tokens, out)?;
        } else {
            emit_expression(argument, frame, tokens, out)?;
        }
    }
    let token = tokens
        .method(
            &method.declaring_type,
            &crate::tokens::conversion_key_name(&method.name, &method.return_type),
            &method.parameters,
        )
        .or_else(|| {
            tokens.method(&method.declaring_type, &method.name, &method.parameters)
        })
        .ok_or(EmitError::Unsupported(
            "call to a method outside this module",
        ))?;
    let opcode = if inherited_value_call {
        Opcode::Callvirt
    } else if method.is_static || value_type_receiver.is_some() || is_base_call {
        Opcode::Call
    } else {
        Opcode::Callvirt
    };
    out.push(Instruction::new(opcode, Operand::Token(token)));
    Ok(())
}

/// Pushes the address of a `ref`/`out` argument variable: a byref parameter's slot
/// already holds the address (`ldarg`), otherwise it is the variable's address
/// (`ldloca`/`ldarga`/`ldflda`).
fn emit_ref_argument(
    operand: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if let BoundExprKind::Local(name) = &operand.kind {
        if let Some((slot, _)) = frame.byref(name) {
            out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(slot)));
            return Ok(());
        }
    }
    emit_value_type_receiver(operand, frame, tokens, out)
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
    if let Some(value) = &field.constant {
        emit_literal(value, &field.ty, tokens, out)?;
        return Ok(());
    }
    let token = tokens
        .field(&field.declaring_type, &field.name)
        .ok_or(EmitError::Unsupported("field outside this module"))?;
    if field.is_static {
        out.push(Instruction::new(Opcode::Ldsfld, Operand::Token(token)));
    } else {
        emit_field_receiver(field, receiver, frame, tokens, out)?;
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
        emit_field_receiver(field, receiver, frame, tokens, out)?;
        emit_expression(value, frame, tokens, out)?;
        out.push(Instruction::new(Opcode::Stfld, Operand::Token(token)));
    }
    Ok(())
}

/// Lowers a property read: the receiver (for an instance property) then a call to
/// the `get_Name` accessor. A static property is accessed through its type.
fn emit_property_load(
    receiver: &BoundExpr,
    declaring_type: &TypeSymbol,
    name: &str,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let is_static = matches!(receiver.kind, BoundExprKind::TypeReference(_));
    let value_type_receiver = !is_static && is_value_type(&receiver.ty, tokens);
    if !is_static {
        if value_type_receiver {
            emit_value_type_receiver(receiver, frame, tokens, out)?;
        } else {
            emit_expression(receiver, frame, tokens, out)?;
        }
    }
    let token = tokens
        .method(declaring_type, &accessor_name("get_", name), &[])
        .ok_or(EmitError::Unsupported(
            "property getter outside this module",
        ))?;
    let opcode = if is_static || value_type_receiver {
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
    declaring_type: &TypeSymbol,
    name: &str,
    value: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let is_static = matches!(receiver.kind, BoundExprKind::TypeReference(_));
    let value_type_receiver = !is_static && is_value_type(&receiver.ty, tokens);
    if !is_static {
        if value_type_receiver {
            emit_value_type_receiver(receiver, frame, tokens, out)?;
        } else {
            emit_expression(receiver, frame, tokens, out)?;
        }
    }
    emit_expression(value, frame, tokens, out)?;
    let token = tokens
        .method(
            declaring_type,
            &accessor_name("set_", name),
            core::slice::from_ref(property_ty),
        )
        .ok_or(EmitError::Unsupported(
            "property setter outside this module",
        ))?;
    let opcode = if is_static || value_type_receiver {
        Opcode::Call
    } else {
        Opcode::Callvirt
    };
    out.push(Instruction::new(opcode, Operand::Token(token)));
    Ok(())
}

/// The `get_`/`set_` accessor method name for a property.
pub(crate) fn accessor_name(prefix: &str, property: &str) -> String {
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
    checked: bool,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if from == to {
        return Ok(());
    }
    if matches!(to, TypeSymbol::Pointer(_)) {
        return Ok(());
    }
    if matches!(from, TypeSymbol::Special(SpecialType::Object)) && is_value_type(to, tokens) {
        let token = tokens.type_token(to).ok_or(EmitError::Unsupported(
            "unboxing to a value type with no metadata token",
        ))?;
        out.push(Instruction::new(Opcode::Unbox, Operand::Token(token)));
        out.push(Instruction::new(Opcode::Ldobj, Operand::Token(token)));
        return Ok(());
    }
    if tokens.is_enum(to) {
        out.push(Instruction::simple(Opcode::ConvI4));
        return Ok(());
    }
    if matches!(to, TypeSymbol::Special(SpecialType::String)) {
        let token = tokens.type_token(to).ok_or(EmitError::Unsupported(
            "a cast to string with no metadata token",
        ))?;
        out.push(Instruction::new(Opcode::Castclass, Operand::Token(token)));
        return Ok(());
    }
    if matches!(to, TypeSymbol::Special(SpecialType::Object)) {
        if is_value_type(from, tokens) {
            let token = tokens.type_token(from).ok_or(EmitError::Unsupported(
                "boxing to object with no metadata token",
            ))?;
            out.push(Instruction::new(Opcode::Box, Operand::Token(token)));
        }
        return Ok(());
    }
    if let TypeSymbol::Special(target) = to {
        let unsigned_source = matches!(from, TypeSymbol::Special(source) if source.is_unsigned());
        let opcode = match (checked, checked_overflow_conversion(*target, unsigned_source)) {
            (true, Some(ovf)) => ovf,
            _ => numeric_conversion(to)?,
        };
        out.push(Instruction::simple(opcode));
        return Ok(());
    }
    let to_reference = matches!(to, TypeSymbol::Array { .. })
        || (matches!(to, TypeSymbol::Named(_)) && !is_value_type(to, tokens));
    if to_reference {
        let token = tokens.type_token(to).ok_or(EmitError::Unsupported(
            "a cast to a reference type with no metadata token",
        ))?;
        out.push(Instruction::new(Opcode::Castclass, Operand::Token(token)));
        return Ok(());
    }
    Err(EmitError::Unsupported("this cast is not lowered yet"))
}

/// Whether `ty` is a value type that boxes/unboxes by token: a numeric/`bool`/`char`
/// primitive, or a module struct or enum (an enum boxes/unboxes as its own type, so
/// `(Color)someObject` is `unbox.any Color`).
pub(crate) fn is_value_type(ty: &TypeSymbol, tokens: &Tokens) -> bool {
    match ty {
        TypeSymbol::Special(special) => !matches!(
            special,
            SpecialType::Object | SpecialType::String | SpecialType::Void | SpecialType::Null
        ),
        _ => tokens.is_struct(ty) || tokens.is_enum(ty),
    }
}

/// Whether two type symbols denote the same type, treating a predefined `Special`
/// type and its `System.<Name>` spelling as equal. The binder names a method's
/// declaring type by its model identity (`System.Int32`) while a value's static type
/// is a `Special` (`Int32`); a value-type call must see those as one type to pick a
/// direct `call` on the value's address over a needless box.
fn same_type(a: &TypeSymbol, b: &TypeSymbol) -> bool {
    if a == b {
        return true;
    }
    match (canonical_name(a), canonical_name(b)) {
        (Some(left), Some(right)) => left == right,
        _ => false,
    }
}

/// A type's `(namespace, name)`, mapping a `Special` to its `System.<Name>` identity
/// and a simple `Named` to its parts. `None` for arrays, byrefs, and the error type.
fn canonical_name(ty: &TypeSymbol) -> Option<(String, String)> {
    match ty {
        TypeSymbol::Special(special) => {
            let (namespace, name) = special.full_name();
            Some((namespace.into(), name.into()))
        }
        TypeSymbol::Named(parts) => {
            let (name, namespace_parts) = parts.split_last()?;
            let mut namespace = String::new();
            for part in namespace_parts {
                if !namespace.is_empty() {
                    namespace.push('.');
                }
                namespace.push_str(part);
            }
            Some((namespace, String::from(&**name)))
        }
        _ => None,
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
pub(crate) fn numeric_conversion(target: &TypeSymbol) -> Result<Opcode, EmitError> {
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

/// Emits the address of a local or parameter (`ldloca`/`ldarga`), for accessing a
/// field of a value type in place.
fn emit_local_address(
    name: &str,
    frame: &Frame,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if let Some((slot, _)) = frame.byref(name) {
        out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(slot)));
        return Ok(());
    }
    match frame.slot(name) {
        Some(Slot::Argument(slot)) => {
            out.push(Instruction::new(Opcode::Ldarga, Operand::Variable(slot)));
        }
        Some(Slot::Local(slot)) => {
            out.push(Instruction::new(Opcode::Ldloca, Operand::Variable(slot)));
        }
        None => {
            return Err(EmitError::Unsupported(
                "address of a name with no frame slot",
            ));
        }
    }
    Ok(())
}

/// Emits a field-access receiver. A field of a value type (a struct) held in a local
/// or parameter is reached through its address (`ldloca`/`ldarga`), so a read avoids a
/// copy and a write stores back in place; every other receiver is emitted as a value.
pub(crate) fn emit_field_receiver(
    field: &FieldReference,
    receiver: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if tokens.is_struct(&field.declaring_type) {
        return emit_value_type_receiver(receiver, frame, tokens, out);
    }
    emit_expression(receiver, frame, tokens, out)
}

/// Emits the receiver of a value-type member (a field or method) as an address: a
/// local or parameter is taken by `ldloca`/`ldarga`; a nested value-type field is the
/// address of its container then `ldflda`, so a write stores in place; `this`/`base`
/// is already a managed pointer (`ldarg.0`), so it is emitted as a value.
pub(crate) fn emit_value_type_receiver(
    receiver: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match &receiver.kind {
        BoundExprKind::Local(name) => emit_local_address(name, frame, out),
        BoundExprKind::FieldAccess {
            receiver: container,
            field: Some(field),
            ..
        } if field.constant.is_none() => {
            let token =
                tokens
                    .field(&field.declaring_type, &field.name)
                    .ok_or(EmitError::Unsupported(
                        "address of a field outside this module",
                    ))?;
            if field.is_static {
                if field.is_readonly {
                    out.push(Instruction::new(Opcode::Ldsfld, Operand::Token(token)));
                    let slot = frame.reserve_local(&receiver.ty);
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(slot)));
                    out.push(Instruction::new(Opcode::Ldloca, Operand::Variable(slot)));
                } else {
                    out.push(Instruction::new(Opcode::Ldsflda, Operand::Token(token)));
                }
            } else {
                if tokens.is_struct(&container.ty) {
                    emit_value_type_receiver(container, frame, tokens, out)?;
                } else {
                    emit_expression(container, frame, tokens, out)?;
                }
                out.push(Instruction::new(Opcode::Ldflda, Operand::Token(token)));
            }
            Ok(())
        }
        BoundExprKind::This | BoundExprKind::Base => {
            emit_expression(receiver, frame, tokens, out)
        }
        BoundExprKind::ElementAccess {
            receiver: array,
            indices,
        } => {
            emit_expression(array, frame, tokens, out)?;
            for index in indices {
                emit_expression(index, frame, tokens, out)?;
            }
            if indices.len() == 1 {
                let element = tokens
                    .type_token(&receiver.ty)
                    .ok_or(EmitError::Unsupported("ldelema element type has no token"))?;
                out.push(Instruction::new(Opcode::Ldelema, Operand::Token(element)));
            } else {
                let token = tokens
                    .method(&array.ty, "Address", &array_int_params(indices.len()))
                    .ok_or(EmitError::Unsupported(
                        "rectangular-array Address method (ref a[i,j])",
                    ))?;
                out.push(Instruction::new(Opcode::Call, Operand::Token(token)));
            }
            Ok(())
        }
        _ => {
            emit_expression(receiver, frame, tokens, out)?;
            let slot = frame.reserve_local(&receiver.ty);
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(slot)));
            out.push(Instruction::new(Opcode::Ldloca, Operand::Variable(slot)));
            Ok(())
        }
    }
}

pub(crate) fn emit_local(
    name: &str,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if let Some((slot, element)) = frame.byref(name) {
        out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(slot)));
        if tokens.is_struct(element) || tokens.is_enum(element) {
            let token = tokens
                .type_token(element)
                .ok_or(EmitError::Unsupported("byref referent type has no token"))?;
            out.push(Instruction::new(Opcode::Ldobj, Operand::Token(token)));
        } else {
            out.push(Instruction::simple(ldind_opcode(element)));
        }
        return Ok(());
    }
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

/// Lowers `x++`/`x--`/`++x`/`--x` used in EXPRESSION position (leaving its value) for a
/// non-byref local: load; (postfix) dup; +/-1; (prefix) dup; store. Postfix leaves the
/// old value on the stack, prefix the new.
fn emit_step_expression(
    operand: &BoundExpr,
    postfix: bool,
    increment: bool,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let BoundExprKind::Local(name) = &operand.kind else {
        let user_step = crate::method::user_step_method(operand, increment, tokens);
        let leave = if postfix {
            crate::method::Leave::Old
        } else {
            crate::method::Leave::New
        };
        return crate::method::emit_compound(
            operand,
            crate::method::step_operator(increment),
            None,
            user_step,
            frame,
            tokens,
            out,
            leave,
        );
    };
    if frame.byref(name).is_some() {
        return Err(EmitError::Unsupported(
            "++/-- of a byref parameter in expression position",
        ));
    }
    let store = match frame.slot(name) {
        Some(Slot::Local(slot)) => Instruction::new(Opcode::Stloc, Operand::Variable(slot)),
        Some(Slot::Argument(slot)) => Instruction::new(Opcode::Starg, Operand::Variable(slot)),
        None => return Err(EmitError::Unsupported("++/-- of a name with no frame slot")),
    };
    if let Some(token) = crate::method::user_step_method(operand, increment, tokens) {
        if postfix {
            emit_local(name, frame, tokens, out)?;
        }
        emit_local(name, frame, tokens, out)?;
        out.push(Instruction::new(Opcode::Call, Operand::Token(token)));
        out.push(store);
        if !postfix {
            emit_local(name, frame, tokens, out)?;
        }
        return Ok(());
    }
    emit_local(name, frame, tokens, out)?;
    if postfix {
        out.push(Instruction::simple(Opcode::Dup));
    }
    out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(1)));
    if matches!(
        operand.ty,
        TypeSymbol::Special(SpecialType::Int64 | SpecialType::UInt64)
    ) {
        out.push(Instruction::simple(Opcode::ConvI8));
    }
    out.push(Instruction::simple(if increment {
        Opcode::Add
    } else {
        Opcode::Sub
    }));
    if !postfix {
        out.push(Instruction::simple(Opcode::Dup));
    }
    out.push(store);
    Ok(())
}

/// The `ldind.*` opcode that loads a value of `ty` through a managed pointer (the
/// signed/unsigned width follows the type, as csc emits for a byref read).
pub(crate) fn ldind_opcode(ty: &TypeSymbol) -> Opcode {
    match ty {
        TypeSymbol::Special(SpecialType::Boolean | SpecialType::Byte) => Opcode::LdindU1,
        TypeSymbol::Special(SpecialType::SByte) => Opcode::LdindI1,
        TypeSymbol::Special(SpecialType::Int16) => Opcode::LdindI2,
        TypeSymbol::Special(SpecialType::UInt16 | SpecialType::Char) => Opcode::LdindU2,
        TypeSymbol::Special(SpecialType::Int32) => Opcode::LdindI4,
        TypeSymbol::Special(SpecialType::UInt32) => Opcode::LdindU4,
        TypeSymbol::Special(SpecialType::Int64 | SpecialType::UInt64) => Opcode::LdindI8,
        TypeSymbol::Special(SpecialType::Single) => Opcode::LdindR4,
        TypeSymbol::Special(SpecialType::Double) => Opcode::LdindR8,
        _ => Opcode::LdindRef,
    }
}

/// The `stind.*` opcode that stores a value of `ty` through a managed pointer (a
/// size-keyed store, sign-agnostic).
pub(crate) fn stind_opcode(ty: &TypeSymbol) -> Opcode {
    match ty {
        TypeSymbol::Special(
            SpecialType::Boolean | SpecialType::Byte | SpecialType::SByte,
        ) => Opcode::StindI1,
        TypeSymbol::Special(SpecialType::Int16 | SpecialType::UInt16 | SpecialType::Char) => {
            Opcode::StindI2
        }
        TypeSymbol::Special(SpecialType::Int32 | SpecialType::UInt32) => Opcode::StindI4,
        TypeSymbol::Special(SpecialType::Int64 | SpecialType::UInt64) => Opcode::StindI8,
        TypeSymbol::Special(SpecialType::Single) => Opcode::StindR4,
        TypeSymbol::Special(SpecialType::Double) => Opcode::StindR8,
        _ => Opcode::StindRef,
    }
}

/// Emits the load-through-a-byref instruction for a referent of type `element` (the managed
/// pointer is already on the stack): `ldobj <token>` for a value type (struct/enum -- there is
/// no `ldind` for one), else the width-appropriate `ldind`. The mirror of [`emit_byref_store`].
pub(crate) fn emit_byref_load(
    element: &TypeSymbol,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if tokens.is_struct(element) || tokens.is_enum(element) {
        let token = tokens
            .type_token(element)
            .ok_or(EmitError::Unsupported("byref referent type has no token"))?;
        out.push(Instruction::new(Opcode::Ldobj, Operand::Token(token)));
    } else {
        out.push(Instruction::simple(ldind_opcode(element)));
    }
    Ok(())
}

/// Emits the store-through-a-byref instruction for a referent of type `element` (the
/// address and value are already on the stack): `stobj <token>` for a value type
/// (struct/enum -- there is no `stind` for one), else the width-appropriate `stind`.
pub(crate) fn emit_byref_store(
    element: &TypeSymbol,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if tokens.is_struct(element) || tokens.is_enum(element) {
        let token = tokens
            .type_token(element)
            .ok_or(EmitError::Unsupported("byref referent type has no token"))?;
        out.push(Instruction::new(Opcode::Stobj, Operand::Token(token)));
    } else {
        out.push(Instruction::simple(stind_opcode(element)));
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
                        "a real literal of a non-float type",
                    ));
                }
            }
        }
        Literal::Decimal {
            lo,
            mid,
            hi,
            scale,
            negative,
        } => {
            let decimal_ty = TypeSymbol::Special(SpecialType::Decimal);
            let ctor_params = [
                TypeSymbol::Special(SpecialType::Int32),
                TypeSymbol::Special(SpecialType::Int32),
                TypeSymbol::Special(SpecialType::Int32),
                TypeSymbol::Special(SpecialType::Boolean),
                TypeSymbol::Special(SpecialType::Byte),
            ];
            let token = tokens.method(&decimal_ty, ".ctor", &ctor_params).ok_or(
                EmitError::Unsupported("the System.Decimal constructor was not minted"),
            )?;
            out.push(load_i4(*lo as i32));
            out.push(load_i4(*mid as i32));
            out.push(load_i4(*hi as i32));
            out.push(load_i4(i32::from(*negative)));
            out.push(load_i4(i32::from(*scale)));
            out.push(Instruction::new(Opcode::Newobj, Operand::Token(token)));
        }
    }
    Ok(())
}

/// Lowers `typeof(T)`: `ldtoken T` pushes a RuntimeTypeHandle, then
/// `System.Type::GetTypeFromHandle` turns it into the `System.Type`.
fn emit_typeof(
    target: &TypeSymbol,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let type_token = tokens
        .type_token(target)
        .ok_or(EmitError::Unsupported("typeof of a type with no token"))?;
    out.push(Instruction::new(Opcode::Ldtoken, Operand::Token(type_token)));
    let method = tokens
        .method(
            &system_type_symbol(),
            "GetTypeFromHandle",
            &[runtime_type_handle_symbol()],
        )
        .ok_or(EmitError::Unsupported("Type::GetTypeFromHandle was not minted"))?;
    out.push(Instruction::new(Opcode::Call, Operand::Token(method)));
    Ok(())
}

/// Lowers `sizeof(T)`: a struct/enum emits the `sizeof` opcode over its token (the runtime
/// computes the size from the shared value-type layout); a primitive is its constant byte
/// size (csc likewise folds `sizeof(primitive)`).
fn emit_sizeof(
    target: &TypeSymbol,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if let TypeSymbol::Special(special) = target {
        let size = primitive_byte_size(*special)
            .ok_or(EmitError::Unsupported("sizeof of this primitive type"))?;
        out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(size)));
        return Ok(());
    }
    let token = tokens
        .type_token(target)
        .ok_or(EmitError::Unsupported("sizeof of a type with no token"))?;
    out.push(Instruction::new(Opcode::Sizeof, Operand::Token(token)));
    Ok(())
}

/// The constant byte size of a fixed-width primitive, or `None` for one whose size is not a
/// compile-time constant here (`IntPtr`/`UIntPtr`, `object`/`string`/`void`, `decimal`).
fn primitive_byte_size(special: SpecialType) -> Option<i32> {
    use SpecialType as S;
    Some(match special {
        S::Boolean | S::SByte | S::Byte => 1,
        S::Int16 | S::UInt16 | S::Char => 2,
        S::Int32 | S::UInt32 | S::Single => 4,
        S::Int64 | S::UInt64 | S::Double => 8,
        _ => return None,
    })
}

/// `System.Type` -- the result of `typeof` and the receiver of `GetTypeFromHandle`.
pub(crate) fn system_type_symbol() -> TypeSymbol {
    TypeSymbol::Named([Box::from("System"), Box::from("Type")].into())
}

/// `System.RuntimeTypeHandle` -- the value `ldtoken` pushes for a type.
pub(crate) fn runtime_type_handle_symbol() -> TypeSymbol {
    TypeSymbol::Named([Box::from("System"), Box::from("RuntimeTypeHandle")].into())
}

/// Whether `ty` is `System.TypedReference`, the special byref-like type whose signature element
/// is `TYPEDBYREF` (it is not a value type named by a token).
pub(crate) fn is_typed_reference(ty: &TypeSymbol) -> bool {
    matches!(ty, TypeSymbol::Named(parts)
        if parts.len() == 2 && &*parts[0] == "System" && &*parts[1] == "TypedReference")
}

/// Lowers `__makeref(variable)`: take the variable's address (a managed pointer), then
/// `mkrefany <variable type>` pairs it with the type into a `TypedReference`.
fn emit_makeref(
    operand: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    emit_value_type_receiver(operand, frame, tokens, out)?;
    let token = tokens
        .type_token(&operand.ty)
        .ok_or(EmitError::Unsupported("__makeref operand type has no token"))?;
    out.push(Instruction::new(Opcode::Mkrefany, Operand::Token(token)));
    Ok(())
}

/// Lowers `__refvalue(reference, T)` in value position: `refanyval <T>` recovers the managed
/// pointer (trapping if the reference was not made over `T`), then it is loaded through.
fn emit_refvalue(
    reference: &BoundExpr,
    target: &TypeSymbol,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    emit_expression(reference, frame, tokens, out)?;
    let token = tokens
        .type_token(target)
        .ok_or(EmitError::Unsupported("__refvalue type has no token"))?;
    out.push(Instruction::new(Opcode::Refanyval, Operand::Token(token)));
    emit_byref_load(target, tokens, out)
}

/// Lowers `__reftype(reference)`: `refanytype` recovers the referent's type as a
/// RuntimeTypeHandle, then `System.Type::GetTypeFromHandle` turns it into a `System.Type`.
fn emit_reftype(
    reference: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    emit_expression(reference, frame, tokens, out)?;
    out.push(Instruction::simple(Opcode::Refanytype));
    let method = tokens
        .method(
            &system_type_symbol(),
            "GetTypeFromHandle",
            &[runtime_type_handle_symbol()],
        )
        .ok_or(EmitError::Unsupported("Type::GetTypeFromHandle was not minted"))?;
    out.push(Instruction::new(Opcode::Call, Operand::Token(method)));
    Ok(())
}

/// Emits string value comparison: `call bool String::op_Equality(string, string)`,
/// negated (`ldc.i4.0; ceq`) for `!=`. The operands are already on the stack.
pub(crate) fn emit_string_equality(
    negate: bool,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let string = TypeSymbol::Special(SpecialType::String);
    let token = tokens
        .method(&string, "op_Equality", &[string.clone(), string.clone()])
        .ok_or(EmitError::Unsupported("String.op_Equality was not minted"))?;
    out.push(Instruction::new(Opcode::Call, Operand::Token(token)));
    if negate {
        out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(0)));
        out.push(Instruction::simple(Opcode::Ceq));
    }
    Ok(())
}

/// Lowers pointer arithmetic (18.5.6, unsafe), returning whether it handled the operator:
/// `p + n` / `n + p` push the pointer then `n * sizeof(T)` and `add`; `p - n` does the same
/// with `sub`; `p - q` subtracts the pointers and divides by `sizeof(T)` (the element
/// count). The integer is scaled by the element size, exactly as `p[i]` is.
fn emit_pointer_arithmetic(
    operator: BinaryOperator,
    left: &BoundExpr,
    right: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<bool, EmitError> {
    let pointer_element = |ty: &TypeSymbol| match ty {
        TypeSymbol::Pointer(element) => Some((**element).clone()),
        _ => None,
    };
    match operator {
        BinaryOperator::Add => {
            let (pointer, offset, element) = if let Some(element) = pointer_element(&left.ty) {
                (left, right, element)
            } else if let Some(element) = pointer_element(&right.ty) {
                (right, left, element)
            } else {
                return Ok(false);
            };
            emit_expression(pointer, frame, tokens, out)?;
            emit_expression(offset, frame, tokens, out)?;
            emit_sizeof(&element, tokens, out)?;
            out.push(Instruction::simple(Opcode::Mul));
            out.push(Instruction::simple(Opcode::Add));
            Ok(true)
        }
        BinaryOperator::Subtract => {
            if let (Some(element), Some(_)) =
                (pointer_element(&left.ty), pointer_element(&right.ty))
            {
                emit_expression(left, frame, tokens, out)?;
                emit_expression(right, frame, tokens, out)?;
                out.push(Instruction::simple(Opcode::Sub));
                emit_sizeof(&element, tokens, out)?;
                out.push(Instruction::simple(Opcode::Div));
                return Ok(true);
            }
            if let Some(element) = pointer_element(&left.ty) {
                emit_expression(left, frame, tokens, out)?;
                emit_expression(right, frame, tokens, out)?;
                emit_sizeof(&element, tokens, out)?;
                out.push(Instruction::simple(Opcode::Mul));
                out.push(Instruction::simple(Opcode::Sub));
                return Ok(true);
            }
            Ok(false)
        }
        _ => Ok(false),
    }
}

pub(crate) fn emit_binary(
    operator: BinaryOperator,
    operand_ty: &TypeSymbol,
    checked: bool,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    use BinaryOperator as Op;
    let unsigned = matches!(operand_ty, TypeSymbol::Special(special) if special.is_unsigned());
    let opcode = match operator {
        Op::Add => checked_or(checked, unsigned, Opcode::AddOvfUn, Opcode::AddOvf, Opcode::Add),
        Op::Subtract => checked_or(checked, unsigned, Opcode::SubOvfUn, Opcode::SubOvf, Opcode::Sub),
        Op::Multiply => checked_or(checked, unsigned, Opcode::MulOvfUn, Opcode::MulOvf, Opcode::Mul),
        Op::Divide => unsigned_or(unsigned, Opcode::DivUn, Opcode::Div),
        Op::Modulo => unsigned_or(unsigned, Opcode::RemUn, Opcode::Rem),
        Op::BitwiseAnd => Opcode::And,
        Op::BitwiseOr => Opcode::Or,
        Op::BitwiseXor => Opcode::Xor,
        Op::LeftShift => Opcode::Shl,
        Op::RightShift => unsigned_or(unsigned, Opcode::ShrUn, Opcode::Shr),
        Op::Equal => Opcode::Ceq,
        Op::GreaterThan => unsigned_or(unsigned, Opcode::CgtUn, Opcode::Cgt),
        Op::LessThan => unsigned_or(unsigned, Opcode::CltUn, Opcode::Clt),
        Op::NotEqual => return emit_negated(Opcode::Ceq, out),
        Op::LessThanOrEqual => {
            return emit_negated(unsigned_or(unsigned, Opcode::CgtUn, Opcode::Cgt), out);
        }
        Op::GreaterThanOrEqual => {
            return emit_negated(unsigned_or(unsigned, Opcode::CltUn, Opcode::Clt), out);
        }
        Op::LogicalAnd | Op::LogicalOr => {
            return Err(EmitError::Unsupported(
                "short-circuit && / || (needs branches)",
            ));
        }
    };
    out.push(Instruction::simple(opcode));
    Ok(())
}

/// Picks the unsigned opcode when the operands are unsigned, else the signed one.
fn unsigned_or(unsigned: bool, when_unsigned: Opcode, when_signed: Opcode) -> Opcode {
    if unsigned { when_unsigned } else { when_signed }
}

/// Picks the overflow-throwing opcode in a `checked` context (its `.un` variant for
/// unsigned operands), else the plain form.
fn checked_or(checked: bool, unsigned: bool, ovf_un: Opcode, ovf: Opcode, plain: Opcode) -> Opcode {
    if checked {
        if unsigned { ovf_un } else { ovf }
    } else {
        plain
    }
}

/// The `conv.ovf.*` opcode for a checked conversion to integral `target` (the `.un`
/// form for an unsigned source). `None` for a non-integral target (float/decimal),
/// which cannot overflow and uses the plain `conv.*`.
fn checked_overflow_conversion(target: SpecialType, unsigned_source: bool) -> Option<Opcode> {
    use SpecialType as S;
    Some(match (target, unsigned_source) {
        (S::SByte, false) => Opcode::ConvOvfI1,
        (S::SByte, true) => Opcode::ConvOvfI1Un,
        (S::Byte, false) => Opcode::ConvOvfU1,
        (S::Byte, true) => Opcode::ConvOvfU1Un,
        (S::Int16, false) => Opcode::ConvOvfI2,
        (S::Int16, true) => Opcode::ConvOvfI2Un,
        (S::UInt16 | S::Char, false) => Opcode::ConvOvfU2,
        (S::UInt16 | S::Char, true) => Opcode::ConvOvfU2Un,
        (S::Int32, false) => Opcode::ConvOvfI4,
        (S::Int32, true) => Opcode::ConvOvfI4Un,
        (S::UInt32, false) => Opcode::ConvOvfU4,
        (S::UInt32, true) => Opcode::ConvOvfU4Un,
        (S::Int64, false) => Opcode::ConvOvfI8,
        (S::Int64, true) => Opcode::ConvOvfI8Un,
        (S::UInt64, false) => Opcode::ConvOvfU8,
        (S::UInt64, true) => Opcode::ConvOvfU8Un,
        _ => return None,
    })
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
    fn checked_context_uses_overflow_opcodes() {
        assert_eq!(emit("checked(1 + 2)"), [i4(1), i4(2), op(Opcode::AddOvf)]);
        assert_eq!(emit("checked(5 * 6)"), [i4(5), i4(6), op(Opcode::MulOvf)]);
        assert_eq!(*emit("checked((int)5L)").last().unwrap(), op(Opcode::ConvOvfI4));
        assert_eq!(emit("unchecked(1 + 2)"), [i4(1), i4(2), op(Opcode::Add)]);
        assert_eq!(emit("1 + 2"), [i4(1), i4(2), op(Opcode::Add)]);
        assert_eq!(*emit("(int)5L").last().unwrap(), op(Opcode::ConvI4));
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
