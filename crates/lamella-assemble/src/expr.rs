//! Lowering a bound expression to CIL (ECMA-335 1st ed, Partition III).

use crate::frame::{Frame, Slot};
use crate::tokens::Tokens;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_binder::{
    BoundExpr, BoundExprKind, BoundInitializer, BoundInitializerTarget, BoundMemberInitializer,
    BoundMemberInitializerValue, ConversionKind, FieldReference, MethodReference, SpecialType,
    TypeSymbol,
};
use lamella_cil::{Instruction, Opcode, Operand};
use lamella_syntax::ast::{BinaryOperator, Literal, PostfixOperator, UnaryOperator};

/// Why an expression could not be lowered to CIL yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitError {
    /// A construct the emitter does not handle yet, with a short reason.
    Unsupported(&'static str),
    /// [`EmitError::Unsupported`] with the containing method named, so a compilation
    /// large enough that the reason alone cannot locate the construct still points at it.
    UnsupportedIn {
        /// The unhandled construct.
        reason: &'static str,
        /// The method whose body carries it.
        method: alloc::string::String,
    },
}

impl core::fmt::Display for EmitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EmitError::Unsupported(reason) => f.write_str(reason),
            EmitError::UnsupportedIn { reason, method } => {
                write!(f, "{reason} (in method '{method}')")
            }
        }
    }
}

impl EmitError {
    /// This error carrying the containing method's name; an error already attributed
    /// to a method keeps its original site.
    #[must_use]
    pub fn in_method(self, method: &str) -> EmitError {
        match self {
            EmitError::Unsupported(reason) => EmitError::UnsupportedIn {
                reason,
                method: alloc::string::String::from(method),
            },
            other => other,
        }
    }
}

/// The `__arglist` marker pseudo-parameter that KEYS a vararg member in the token
/// table: appended after the fixed parameters on the DEF key, and followed by the
/// extra argument types on a non-empty call-site key. It mirrors the binder's marker
/// type; user code cannot spell it (`__arglist` lexes as a keyword under the knob).
pub(crate) fn arglist_marker_symbol() -> TypeSymbol {
    TypeSymbol::Named([Box::from("__arglist")].into())
}

/// Splits a bound argument list at its trailing `__arglist(...)` pack: `(fixed,
/// Some(extras))` for a vararg call site, `(all, None)` otherwise.
pub(crate) fn split_vararg_arguments(
    arguments: &[BoundExpr],
) -> (&[BoundExpr], Option<&[BoundExpr]>) {
    if let Some((last, fixed)) = arguments.split_last() {
        if let BoundExprKind::ArgListLiteral(extras) = &last.kind {
            return (fixed, Some(extras));
        }
    }
    (arguments, None)
}

/// A variable argument's type as the call-site signature records it: its static type,
/// with the null literal folded to `object` (`ELEMENT_TYPE_OBJECT`, `0x1C`) -- the literal has
/// no type of its own and the signature must record one a callee can bind against -- and a
/// `ref`/`out` element as a byref of its referent.
pub(crate) fn vararg_extra_symbol(extra: &BoundExpr) -> TypeSymbol {
    if matches!(extra.kind, BoundExprKind::Ref { .. }) {
        return TypeSymbol::ByRef(Box::new(extra.ty.clone()));
    }
    if matches!(extra.ty, TypeSymbol::Special(SpecialType::Null)) {
        return TypeSymbol::Special(SpecialType::Object);
    }
    extra.ty.clone()
}

/// The token-table key for a vararg CALL SITE: the fixed parameters, the `__arglist`
/// marker, then each extra argument's signature type. An EMPTY `__arglist()` yields
/// exactly the DEF key (fixed + marker), so it resolves to the MethodDef token -- the
/// same lowering csc uses.
pub(crate) fn vararg_lookup_params(
    fixed: &[TypeSymbol],
    extras: &[BoundExpr],
) -> Vec<TypeSymbol> {
    let mut params = fixed.to_vec();
    params.push(arglist_marker_symbol());
    params.extend(extras.iter().map(vararg_extra_symbol));
    params
}

/// The marker pseudo-parameter that separates a generic CALL SITE's key from its definition's,
/// exactly as [`arglist_marker_symbol`] does for a vararg site. Not a spellable type: a C#
/// identifier cannot contain `<`.
fn method_instantiation_marker_symbol() -> TypeSymbol {
    TypeSymbol::Named([Box::from("<instantiation>")].into())
}

/// The token-table key for a generic CALL SITE: the definition's OPEN parameters, the marker,
/// then the type arguments the site named.
///
/// **THE TYPE ARGUMENTS MUST BE IN THE KEY, AND THE SUBSTITUTED PARAMETERS ARE NOT A
/// SUBSTITUTE FOR THEM.** A generic method need not mention its own type parameter in its
/// signature -- `T Make<T>(int seed)` closes to `Make(int)` for EVERY `T` -- so keying a site by
/// what it resolved to would give `Make<int>(0)` and `Make<string>(0)` one row, one `MethodSpec`,
/// and whichever argument was minted first. That is the same collapse a shared `TypeSpec` would
/// be, reached through the method table instead of the type one.
///
/// The OPEN parameters lead rather than the closed ones so that a site key can never coincide
/// with another method's DEF key (which has no marker) or a vararg site's (whose marker differs).
pub(crate) fn generic_site_lookup_params(
    open_parameters: &[TypeSymbol],
    type_arguments: &[TypeSymbol],
) -> Vec<TypeSymbol> {
    let mut params = open_parameters.to_vec();
    params.push(method_instantiation_marker_symbol());
    params.extend(type_arguments.iter().cloned());
    params
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
                if emit_pointer_arithmetic(*operator, left, right, *checked, frame, tokens, out)? {
                    return Ok(());
                }
                let reference_equality =
                    matches!(operator, BinaryOperator::Equal | BinaryOperator::NotEqual);
                emit_expression(left, frame, tokens, out)?;
                if reference_equality {
                    box_type_parameter(&left.ty, tokens, out)?;
                }
                emit_expression(right, frame, tokens, out)?;
                if reference_equality {
                    box_type_parameter(&right.ty, tokens, out)?;
                }
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
            emit_unary(*operator, out)?;
            if *operator == UnaryOperator::Complement {
                if let Some(underlying) = tokens.enum_underlying(&operand.ty) {
                    narrow_subint(&TypeSymbol::Special(underlying), out);
                }
            }
            Ok(())
        }
        BoundExprKind::Postfix {
            operator,
            operand,
            step,
        } => {
            let increment = *operator == PostfixOperator::Increment;
            if let Some(step) = step {
                let (user_step, result_conversion) =
                    crate::method::step_tokens(Some(step), operand, increment, tokens);
                crate::method::emit_compound(
                    operand,
                    crate::method::step_operator(increment),
                    None,
                    user_step,
                    result_conversion,
                    false,
                    frame,
                    tokens,
                    out,
                    crate::method::Leave::Old,
                )
            } else {
                emit_step_expression(operand, true, increment, frame, tokens, out)
            }
        }
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
                    .instruction_type_token(&operand.ty)
                    .ok_or(EmitError::Unsupported(
                        "boxing a value type with no metadata token",
                    ))?;
                out.push(Instruction::new(Opcode::Box, Operand::Token(token)));
                Ok(())
            } else {
                emit_conversion(*conversion, &operand.ty, &expr.ty, out)
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
            ..
        } => emit_property_load(receiver, declaring_type, name, frame, tokens, out),
        BoundExprKind::This | BoundExprKind::Base => {
            out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(0)));
            if is_value_type(&expr.ty, tokens) {
                if tokens.is_struct(&expr.ty) || tokens.is_enum(&expr.ty) {
                    let token = tokens
                        .instruction_type_token(&expr.ty)
                        .ok_or(EmitError::Unsupported("a value-type `this` with no token"))?;
                    out.push(Instruction::new(Opcode::Ldobj, Operand::Token(token)));
                } else {
                    out.push(Instruction::simple(ldind_opcode(&expr.ty)));
                }
            }
            Ok(())
        }
        BoundExprKind::ObjectCreation {
            arguments,
            constructor,
            initializer,
        } => {
            emit_new(constructor.as_ref(), arguments, frame, tokens, out)?;
            match initializer {
                Some(initializer) => emit_initializer(initializer, &expr.ty, frame, tokens, out),
                None => Ok(()),
            }
        }
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
        BoundExprKind::NullCoalescing { left, right } => {
            emit_null_coalescing(left, right, frame, tokens, out)
        }
        BoundExprKind::TypeOf(target) => emit_typeof(target, tokens, out),
        BoundExprKind::SizeOf(target) => emit_sizeof(target, tokens, out),
        BoundExprKind::DefaultValue(target) => emit_default_value(target, frame, tokens, out),
        BoundExprKind::MakeRef(operand) => emit_makeref(operand, frame, tokens, out),
        BoundExprKind::ArgListValue => {
            out.push(Instruction::simple(Opcode::Arglist));
            Ok(())
        }
        BoundExprKind::ArgListLiteral(_) => Err(EmitError::Unsupported(
            "an __arglist expression outside a call or new expression",
        )),
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
        BoundExprKind::AddressOf { operand } => match &operand.kind {
            BoundExprKind::Dereference { operand: pointer } => {
                emit_expression(pointer, frame, tokens, out)
            }
            BoundExprKind::Local(_)
            | BoundExprKind::FieldAccess { .. }
            | BoundExprKind::ElementAccess { .. } => {
                emit_value_type_receiver(operand, frame, tokens, out)
            }
            _ => Err(EmitError::Unsupported(
                "address-of a non-addressable expression",
            )),
        },
        BoundExprKind::TypeTest {
            operation,
            operand,
            target,
        } => {
            emit_expression(operand, frame, tokens, out)?;
            if operand.ty.is_void() {
                out.push(load_i4(0));
                return Ok(());
            }
            if is_value_type(&operand.ty, tokens) {
                let box_token = tokens.instruction_type_token(&operand.ty).ok_or(EmitError::Unsupported(
                    "boxing a value type for a type test with no metadata token",
                ))?;
                out.push(Instruction::new(Opcode::Box, Operand::Token(box_token)));
            }
            let token = tokens.instruction_type_token(target).ok_or(EmitError::Unsupported(
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
            ..
        } => match &target.kind {
            BoundExprKind::Local(name) if frame.byref(name).is_none() => {
                emit_expression(value, frame, tokens, out)?;
                out.push(Instruction::simple(Opcode::Dup));
                crate::method::store_to(frame, name, out)
            }
            BoundExprKind::Local(name) => {
                let (slot, element) = frame.byref(name).expect("byref checked above");
                out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(slot)));
                emit_expression(value, frame, tokens, out)?;
                let kept = keep_assigned(true, &value.ty, frame, out);
                emit_byref_store(element, tokens, out)?;
                load_kept(kept, out);
                Ok(())
            }
            BoundExprKind::FieldAccess {
                receiver, field, ..
            } => emit_field_store(field.as_ref(), receiver, value, true, frame, tokens, out),
            BoundExprKind::ElementAccess { receiver, indices } => {
                emit_element_store(&target.ty, receiver, indices, value, true, frame, tokens, out)
            }
            BoundExprKind::IndexerAccess {
                receiver,
                indices,
                setter,
            } => emit_indexer_store(receiver, indices, setter, value, true, frame, tokens, out),
            BoundExprKind::PropertyAccess {
                receiver,
                setter_declaring_type,
                name,
                ..
            } => emit_property_store(
                &target.ty,
                receiver,
                setter_declaring_type,
                name,
                value,
                true,
                frame,
                tokens,
                out,
            ),
            BoundExprKind::Dereference { operand } => {
                emit_expression(operand, frame, tokens, out)?;
                emit_expression(value, frame, tokens, out)?;
                let kept = keep_assigned(true, &value.ty, frame, out);
                out.push(Instruction::simple(stind_opcode(&target.ty)));
                load_kept(kept, out);
                Ok(())
            }
            BoundExprKind::This => {
                out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(0)));
                emit_expression(value, frame, tokens, out)?;
                let kept = keep_assigned(true, &value.ty, frame, out);
                let token = tokens.instruction_type_token(&target.ty).ok_or(EmitError::Unsupported(
                    "`this =` on a value type with no token",
                ))?;
                out.push(Instruction::new(Opcode::Stobj, Operand::Token(token)));
                load_kept(kept, out);
                Ok(())
            }
            _ => Err(EmitError::Unsupported(
                "this assignment target is not lowered as an expression yet",
            )),
        },
        BoundExprKind::Await { .. } => Err(EmitError::Unsupported(
            "an await expression is not lowered yet (the async state machine is in flight)",
        )),
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

/// Lowers `left ?? right` (14.13) as csc does: the left value is evaluated ONCE, duplicated, and
/// kept when it is not null.
///
/// ```text
/// <left>          the value under test
/// dup             a copy for the test, so the value survives it
/// brtrue  end     not null: the copy on the stack IS the result
/// pop             null: drop it
/// <right>
/// end:
/// ```
///
/// **THE `dup` IS WHAT MAKES THIS ONE EVALUATION**, which is the whole difference from the
/// desugaring `left != null ? left : right`: that one emits `left` twice, so any side effect in it
/// happens twice. The binder converted both operands to the result type already, and for a
/// reference type that conversion emits nothing -- which is why the duplicated value needs no
/// further work on the non-null path.
fn emit_null_coalescing(
    left: &BoundExpr,
    right: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    emit_expression(left, frame, tokens, out)?;
    out.push(Instruction::simple(Opcode::Dup));
    let to_end = out.len();
    out.push(Instruction::new(Opcode::Brtrue, Operand::Target(0)));
    out.push(Instruction::simple(Opcode::Pop));
    emit_expression(right, frame, tokens, out)?;
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
        if !elements.is_empty() {
            emit_rectangular_initializer(array_ty, element, lengths, elements, frame, tokens, out)?;
        }
        return Ok(());
    }
    let element_token = tokens
        .instruction_type_token(element)
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
                out.push(stelem_instruction(element, tokens)?);
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

/// Stores a rectangular array's initializer elements (19.6) with the new array already on
/// the stack: each flattened element, in row-major order, is written through the array
/// type's `Set` method at the multi-dimensional index its flat position decodes to. The
/// array is left on the stack (each `Set` consumes a duplicate). The dimension lengths are
/// read from the constant length expressions the constructor received.
fn emit_rectangular_initializer(
    array_ty: &TypeSymbol,
    element: &TypeSymbol,
    lengths: &[BoundExpr],
    elements: &[BoundExpr],
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let dimensions = lengths
        .iter()
        .map(constant_length)
        .collect::<Option<Vec<i32>>>()
        .ok_or(EmitError::Unsupported(
            "a rectangular array initializer needs constant dimension lengths",
        ))?;
    let mut set_params = array_int_params(lengths.len());
    set_params.push(element.clone());
    let set = tokens
        .method(array_ty, "Set", &set_params)
        .ok_or(EmitError::Unsupported("array Set was not minted"))?;
    for (position, value) in elements.iter().enumerate() {
        out.push(Instruction::simple(Opcode::Dup));
        for index in row_major_indices(position, &dimensions) {
            out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(index)));
        }
        emit_expression(value, frame, tokens, out)?;
        out.push(Instruction::new(Opcode::Call, Operand::Token(set)));
    }
    Ok(())
}

/// The multi-dimensional index a flat position decodes to in row-major order (last axis
/// varying fastest, II.14.2), given each dimension's length.
fn row_major_indices(position: usize, dimensions: &[i32]) -> Vec<i32> {
    let mut indices = alloc::vec![0i32; dimensions.len()];
    let mut remainder = position;
    for axis in (0..dimensions.len()).rev() {
        let size = (dimensions[axis].max(1)) as usize;
        indices[axis] = (remainder % size) as i32;
        remainder /= size;
    }
    indices
}

/// The constant `int32` value of an array-dimension length, when it is an integer literal
/// (a rectangular initializer's inferred or constant lengths always are).
fn constant_length(length: &BoundExpr) -> Option<i32> {
    match &length.kind {
        BoundExprKind::Literal(Literal::Integer { value, .. }) => i32::try_from(*value).ok(),
        _ => None,
    }
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
        widen_pointer_offset(&indices[0].ty, out);
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
            .instruction_type_token(element_ty)
            .ok_or(EmitError::Unsupported("array element type has no token"))?;
        out.push(Instruction::new(Opcode::Ldelema, Operand::Token(token)));
        out.push(Instruction::new(Opcode::Ldobj, Operand::Token(token)));
    } else {
        out.push(ldelem_instruction(element_ty, tokens)?);
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
    leave: bool,
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
        let kept = keep_assigned(leave, &value.ty, frame, out);
        let mut set_params = array_int_params(indices.len());
        set_params.push(element_ty.clone());
        let set = tokens
            .method(&receiver.ty, "Set", &set_params)
            .ok_or(EmitError::Unsupported("array Set was not minted"))?;
        out.push(Instruction::new(Opcode::Call, Operand::Token(set)));
        load_kept(kept, out);
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
        let kept = keep_assigned(leave, &value.ty, frame, out);
        out.push(Instruction::simple(stind_opcode(element_ty)));
        load_kept(kept, out);
        return Ok(());
    }
    emit_expression(receiver, frame, tokens, out)?;
    emit_expression(&indices[0], frame, tokens, out)?;
    let kept = if tokens.is_struct(element_ty)
        || tokens.is_enum(element_ty)
        || matches!(element_ty, TypeSymbol::Special(SpecialType::Decimal))
    {
        let token = tokens
            .instruction_type_token(element_ty)
            .ok_or(EmitError::Unsupported("array element type has no token"))?;
        out.push(Instruction::new(Opcode::Ldelema, Operand::Token(token)));
        emit_expression(value, frame, tokens, out)?;
        let kept = keep_assigned(leave, &value.ty, frame, out);
        out.push(Instruction::new(Opcode::Stobj, Operand::Token(token)));
        kept
    } else {
        emit_expression(value, frame, tokens, out)?;
        let kept = keep_assigned(leave, &value.ty, frame, out);
        out.push(stelem_instruction(element_ty, tokens)?);
        kept
    };
    load_kept(kept, out);
    Ok(())
}

/// The `ldelem.*` opcode for reading an element of the given type.
/// The instruction that READS `array[index]` for an element of this type.
///
/// **A TYPE PARAMETER TAKES THE TOKEN-CARRYING `ldelem` (III.4.7), NOT ONE OF THE WIDTH-SPECIFIC
/// FORMS.** `T[]` is `int[]` under one instantiation and `string[]` under another, so no opcode
/// chosen at compile time is right for both -- `ldelem.ref` on a `T` closed over `int` reads the
/// value as a reference. The token form defers the decision to the instantiation in hand, which is
/// what csc emits and the only lowering that is correct for either.
pub(crate) fn ldelem_instruction(
    element_ty: &TypeSymbol,
    tokens: &Tokens,
) -> Result<Instruction, EmitError> {
    if let Some(spec) = tokens.type_parameter_spec(element_ty) {
        return Ok(Instruction::new(Opcode::Ldelem, Operand::Token(spec)));
    }
    Ok(Instruction::simple(ldelem_opcode(element_ty)?))
}

/// The instruction that WRITES `array[index]` for an element of this type. The `stelem` twin of
/// [`ldelem_instruction`], and the same rule for the same reason -- `stelem.ref` storing a `T`
/// closed over `int` writes an integer where a reference is expected, which faults at the next read
/// rather than at the store.
pub(crate) fn stelem_instruction(
    element_ty: &TypeSymbol,
    tokens: &Tokens,
) -> Result<Instruction, EmitError> {
    if let Some(spec) = tokens.type_parameter_spec(element_ty) {
        return Ok(Instruction::new(Opcode::Stelem, Operand::Token(spec)));
    }
    Ok(Instruction::simple(stelem_opcode(element_ty)?))
}

fn ldelem_opcode(element_ty: &TypeSymbol) -> Result<Opcode, EmitError> {
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
        TypeSymbol::Instantiation { .. } => Opcode::LdelemRef,
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
        TypeSymbol::Pointer(_) => return Err(EmitError::Unsupported("stelem on a pointer")),
        TypeSymbol::ByRef(_) => return Err(EmitError::Unsupported("stelem on a byref")),
        TypeSymbol::Instantiation { .. } => Opcode::StelemRef,
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
    if arguments.is_empty() && is_value_type(&constructor.declaring_type, tokens) {
        let type_token = tokens
            .instruction_type_token(&constructor.declaring_type)
            .ok_or(EmitError::Unsupported(
                "a value type with no metadata token for initobj",
            ))?;
        let slot = frame.reserve_local(&constructor.declaring_type);
        out.push(Instruction::new(Opcode::Ldloca, Operand::Variable(slot)));
        out.push(Instruction::new(Opcode::Initobj, Operand::Token(type_token)));
        out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
        return Ok(());
    }
    let (fixed_args, extras) = split_vararg_arguments(arguments);
    for argument in fixed_args {
        emit_argument(argument, frame, tokens, out)?;
    }
    if let Some(extras) = extras {
        for element in extras {
            emit_argument(element, frame, tokens, out)?;
        }
    }
    let lookup_params = match extras {
        Some(extras) => vararg_lookup_params(&constructor.parameters, extras),
        None => constructor.parameters.clone(),
    };
    let token = tokens
        .method(&constructor.declaring_type, &constructor.name, &lookup_params)
        .ok_or(EmitError::Unsupported("constructor outside this module"))?;
    out.push(Instruction::new(Opcode::Newobj, Operand::Token(token)));
    Ok(())
}

/// Pushes one call/creation argument: a `ref`/`out` argument pushes the variable's
/// address (17.5.1), any other argument pushes its value.
pub(crate) fn emit_argument(
    argument: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if let BoundExprKind::Ref { operand, .. } = &argument.kind {
        emit_ref_argument(operand, frame, tokens, out)
    } else {
        emit_expression(argument, frame, tokens, out)
    }
}

/// Lowers `new D(method)`: push the target object (`ldnull` for a static target, else the
/// receiver), the function pointer, then `newobj D::.ctor`. A virtual instance target reached
/// through an object loads the pointer from the object's runtime type (`dup; ldvirtftn`,
/// III.4.18), so an override is honored; a static, `base`-qualified, value-type, or non-virtual
/// target binds the exact method named by the token (`ldftn`).
fn emit_delegate_creation(
    delegate_type: &TypeSymbol,
    target: &lamella_binder::MethodReference,
    receiver: Option<&BoundExpr>,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let target_token = tokens
        .method(&target.declaring_type, &target.name, &target.parameters)
        .ok_or(EmitError::Unsupported(
            "delegate target outside this module",
        ))?;
    let through_object = matches!(receiver, Some(r)
        if !matches!(r.kind, BoundExprKind::Base) && !is_value_type(&r.ty, tokens));
    let virtual_target = !target.is_static
        && through_object
        && tokens.is_virtual_method(&target.declaring_type, &target.name, &target.parameters);
    match receiver {
        Some(receiver) => {
            emit_expression(receiver, frame, tokens, out)?;
            if !target.is_static
                && !matches!(receiver.kind, BoundExprKind::Base)
                && is_value_type(&receiver.ty, tokens)
            {
                let box_token = tokens.instruction_type_token(&receiver.ty).ok_or(EmitError::Unsupported(
                    "boxing a delegate receiver with no type token",
                ))?;
                out.push(Instruction::new(Opcode::Box, Operand::Token(box_token)));
            }
        }
        None => out.push(Instruction::simple(Opcode::Ldnull)),
    }
    if virtual_target {
        out.push(Instruction::simple(Opcode::Dup));
        out.push(Instruction::new(
            Opcode::Ldvirtftn,
            Operand::Token(target_token),
        ));
    } else {
        out.push(Instruction::new(
            Opcode::Ldftn,
            Operand::Token(target_token),
        ));
    }
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
    let type_parameter_receiver = match receiver {
        Some(r) if tokens.body_type_parameter(&r.ty).is_some() => Some(&r.ty),
        _ => None,
    };
    let is_base_call = matches!(receiver, Some(r) if matches!(r.kind, BoundExprKind::Base));
    let inherited_value_call =
        matches!(value_type_receiver, Some(ty) if !same_type(&method.declaring_type, ty));
    let mut constrained_token = None;
    if !method.is_static {
        match &callee.kind {
            BoundExprKind::MethodGroup { receiver, .. } => {
                if type_parameter_receiver.is_some() {
                    emit_value_type_receiver(receiver, frame, tokens, out)?;
                    constrained_token = Some(tokens.instruction_type_token(&receiver.ty).ok_or(
                        EmitError::Unsupported(
                            "a call on a type parameter with no metadata token",
                        ),
                    )?);
                } else if inherited_value_call {
                    emit_value_type_receiver(receiver, frame, tokens, out)?;
                    constrained_token = Some(tokens.instruction_type_token(&receiver.ty).ok_or(
                        EmitError::Unsupported(
                            "a virtual call on a value type with no metadata token",
                        ),
                    )?);
                } else if value_type_receiver.is_some() {
                    emit_value_type_receiver(receiver, frame, tokens, out)?;
                } else {
                    emit_expression(receiver, frame, tokens, out)?;
                }
            }
            _ => emit_expression(callee, frame, tokens, out)?,
        }
    }
    let (fixed_args, extras) = split_vararg_arguments(arguments);
    for argument in fixed_args {
        emit_argument(argument, frame, tokens, out)?;
    }
    if let Some(extras) = extras {
        for element in extras {
            emit_argument(element, frame, tokens, out)?;
        }
    }
    let token = match method.instantiation.as_deref() {
        Some(instantiation) => tokens
            .method(
                &method.declaring_type,
                &method.name,
                &generic_site_lookup_params(&instantiation.parameters, &instantiation.arguments),
            )
            .ok_or(EmitError::Unsupported(
                "a generic call whose instantiation could not be minted",
            ))?,
        None => {
            let lookup_params = match extras {
                Some(extras) => vararg_lookup_params(&method.parameters, extras),
                None => method.parameters.clone(),
            };
            tokens
                .method(
                    &method.declaring_type,
                    &crate::tokens::conversion_key_name(&method.name, &method.return_type),
                    &lookup_params,
                )
                .or_else(|| tokens.method(&method.declaring_type, &method.name, &lookup_params))
                .ok_or(EmitError::Unsupported(
                    "call to a method outside this module",
                ))?
        }
    };
    let opcode = if inherited_value_call || type_parameter_receiver.is_some() {
        Opcode::Callvirt
    } else if method.is_static || value_type_receiver.is_some() || is_base_call {
        Opcode::Call
    } else {
        Opcode::Callvirt
    };
    if let Some(constrained) = constrained_token {
        out.push(Instruction::new(
            Opcode::Constrained,
            Operand::Token(constrained),
        ));
    }
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
        if field.is_volatile {
            out.push(Instruction::simple(Opcode::Volatile));
        }
        out.push(Instruction::new(Opcode::Ldsfld, Operand::Token(token)));
    } else {
        emit_field_receiver(field, receiver, frame, tokens, out)?;
        if field.is_volatile {
            out.push(Instruction::simple(Opcode::Volatile));
        }
        out.push(Instruction::new(Opcode::Ldfld, Operand::Token(token)));
    }
    Ok(())
}

/// After an assignment's value is on the stack and before the store consumes it, saves a copy to
/// a fresh temp when the assignment is used as an EXPRESSION (`leave`), so its value survives the
/// store; [`load_kept`] reloads it afterward. A statement-position store passes `leave = false`
/// and this emits nothing. (14.14: an assignment expression's value is the value assigned.)
fn keep_assigned(
    leave: bool,
    value_ty: &TypeSymbol,
    frame: &Frame,
    out: &mut Vec<Instruction>,
) -> Option<u16> {
    if !leave {
        return None;
    }
    out.push(Instruction::simple(Opcode::Dup));
    let temp = frame.reserve_local(value_ty);
    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(temp)));
    Some(temp)
}

/// Reloads the value [`keep_assigned`] saved, leaving it as the assignment expression's result.
fn load_kept(kept: Option<u16>, out: &mut Vec<Instruction>) {
    if let Some(temp) = kept {
        out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(temp)));
    }
}

/// Lowers a field write: the value (and receiver, if an instance field) are on the
/// stack, then `stsfld`/`stfld` stores. Shared by assignment emission. `leave` keeps the
/// assigned value on the stack (the assignment used as an expression, 14.14).
pub(crate) fn emit_field_store(
    field: Option<&FieldReference>,
    receiver: &BoundExpr,
    value: &BoundExpr,
    leave: bool,
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
    let kept = if field.is_static {
        emit_expression(value, frame, tokens, out)?;
        let kept = keep_assigned(leave, &value.ty, frame, out);
        if field.is_volatile {
            out.push(Instruction::simple(Opcode::Volatile));
        }
        out.push(Instruction::new(Opcode::Stsfld, Operand::Token(token)));
        kept
    } else {
        emit_field_receiver(field, receiver, frame, tokens, out)?;
        emit_expression(value, frame, tokens, out)?;
        let kept = keep_assigned(leave, &value.ty, frame, out);
        if field.is_volatile {
            out.push(Instruction::simple(Opcode::Volatile));
        }
        out.push(Instruction::new(Opcode::Stfld, Operand::Token(token)));
        kept
    };
    load_kept(kept, out);
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
    if name == "Length" && matches!(&receiver.ty, TypeSymbol::Array { rank: 1, .. }) {
        emit_expression(receiver, frame, tokens, out)?;
        out.push(Instruction::simple(Opcode::Ldlen));
        out.push(Instruction::simple(Opcode::ConvI4));
        return Ok(());
    }
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
    let is_base = matches!(receiver.kind, BoundExprKind::Base);
    let opcode = if is_static || value_type_receiver || is_base {
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
    leave: bool,
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
    let kept = keep_assigned(leave, &value.ty, frame, out);
    let token = tokens
        .method(
            declaring_type,
            &accessor_name("set_", name),
            core::slice::from_ref(property_ty),
        )
        .ok_or(EmitError::Unsupported(
            "property setter outside this module",
        ))?;
    let is_base = matches!(receiver.kind, BoundExprKind::Base);
    let opcode = if is_static || value_type_receiver || is_base {
        Opcode::Call
    } else {
        Opcode::Callvirt
    };
    out.push(Instruction::new(opcode, Operand::Token(token)));
    load_kept(kept, out);
    Ok(())
}

/// Lowers an indexer write `obj[indices] = value`: the receiver, the indices, then the value,
/// then a call to the resolved `set_` accessor. Mirrors [`emit_property_store`] with the index
/// arguments pushed between the receiver and the value. `leave` keeps the assigned value on the
/// stack (the assignment used as an expression, 14.14) via the same dup-to-temp/reload as the
/// other stores -- exactly csc's `dup; stloc; callvirt set_Item; ldloc`.
pub(crate) fn emit_indexer_store(
    receiver: &BoundExpr,
    indices: &[BoundExpr],
    setter: &MethodReference,
    value: &BoundExpr,
    leave: bool,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let value_type_receiver = is_value_type(&receiver.ty, tokens);
    if value_type_receiver {
        emit_value_type_receiver(receiver, frame, tokens, out)?;
    } else {
        emit_expression(receiver, frame, tokens, out)?;
    }
    for index in indices {
        emit_expression(index, frame, tokens, out)?;
    }
    emit_expression(value, frame, tokens, out)?;
    let kept = keep_assigned(leave, &value.ty, frame, out);
    let token = tokens
        .method(&setter.declaring_type, &setter.name, &setter.parameters)
        .ok_or(EmitError::Unsupported("indexer setter outside this module"))?;
    let is_base = matches!(receiver.kind, BoundExprKind::Base);
    let opcode = if value_type_receiver || is_base {
        Opcode::Call
    } else {
        Opcode::Callvirt
    };
    out.push(Instruction::new(opcode, Operand::Token(token)));
    load_kept(kept, out);
    Ok(())
}

/// The `get_`/`set_` accessor method name for a property.
/// Lowers an object or collection initializer, with the object it initializes ALREADY ON THE STACK,
/// and leaves it there.
///
/// **Every member `dup`s the object rather than storing it to a local**, which is what csc emits and
/// what makes the whole initializer one expression: the value of `new C { F = 1 }` is the
/// initialized `C`, so it can be returned, passed or assigned without a temp.
///
/// ```text
/// newobj C::.ctor
/// dup ; <value> ; stfld C::F              a field
/// dup ; <value> ; callvirt C::set_P       a property
/// dup ; <value> ; callvirt C::Add         a collection element
/// dup ; ldfld C::F ; <nested...> ; pop    a NESTED initializer
/// ```
///
/// **The nested form loads the member and assigns INTO it -- it constructs nothing.** `ldfld` then
/// the nested members then `pop`: the `pop` discards the member value the nested stores were made
/// against, leaving the outer object. Emitting a `newobj` there instead would be the natural
/// misreading and would silently replace whatever `F` already referred to.
fn emit_initializer(
    initializer: &BoundInitializer,
    target_ty: &TypeSymbol,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match initializer {
        BoundInitializer::Collection(elements) => {
            for element in elements {
                out.push(Instruction::simple(Opcode::Dup));
                emit_expression(element, frame, tokens, out)?;
                let token = tokens
                    .method(target_ty, "Add", core::slice::from_ref(&element.ty))
                    .ok_or(EmitError::Unsupported("collection Add outside this module"))?;
                out.push(Instruction::new(Opcode::Callvirt, Operand::Token(token)));
            }
            Ok(())
        }
        BoundInitializer::Object(members) => {
            for member in members {
                emit_member_initializer(member, frame, tokens, out)?;
            }
            Ok(())
        }
    }
}

/// Lowers one `name = value`, with the object on the stack, leaving it there.
fn emit_member_initializer(
    member: &BoundMemberInitializer,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match (&member.target, &member.value) {
        (BoundInitializerTarget::Field(field), BoundMemberInitializerValue::Expression(value)) => {
            let token = tokens
                .field(&field.declaring_type, &field.name)
                .ok_or(EmitError::Unsupported("initializer field outside this module"))?;
            out.push(Instruction::simple(Opcode::Dup));
            emit_expression(value, frame, tokens, out)?;
            if field.is_volatile {
                out.push(Instruction::simple(Opcode::Volatile));
            }
            out.push(Instruction::new(Opcode::Stfld, Operand::Token(token)));
            Ok(())
        }
        (
            BoundInitializerTarget::Property {
                setter_declaring_type,
                ty,
            },
            BoundMemberInitializerValue::Expression(value),
        ) => {
            let token = tokens
                .method(
                    setter_declaring_type,
                    &accessor_name("set_", &member.name),
                    core::slice::from_ref(ty),
                )
                .ok_or(EmitError::Unsupported(
                    "initializer property setter outside this module",
                ))?;
            out.push(Instruction::simple(Opcode::Dup));
            emit_expression(value, frame, tokens, out)?;
            out.push(Instruction::new(Opcode::Callvirt, Operand::Token(token)));
            Ok(())
        }
        (BoundInitializerTarget::Field(field), BoundMemberInitializerValue::Nested(nested)) => {
            let token = tokens
                .field(&field.declaring_type, &field.name)
                .ok_or(EmitError::Unsupported("initializer field outside this module"))?;
            out.push(Instruction::simple(Opcode::Dup));
            out.push(Instruction::new(Opcode::Ldfld, Operand::Token(token)));
            emit_initializer(nested, &field.ty, frame, tokens, out)?;
            out.push(Instruction::simple(Opcode::Pop));
            Ok(())
        }
        (
            BoundInitializerTarget::Property {
                setter_declaring_type,
                ty,
            },
            BoundMemberInitializerValue::Nested(nested),
        ) => {
            let token = tokens
                .method(setter_declaring_type, &accessor_name("get_", &member.name), &[])
                .ok_or(EmitError::Unsupported(
                    "initializer property getter outside this module",
                ))?;
            out.push(Instruction::simple(Opcode::Dup));
            out.push(Instruction::new(Opcode::Callvirt, Operand::Token(token)));
            emit_initializer(nested, ty, frame, tokens, out)?;
            out.push(Instruction::simple(Opcode::Pop));
            Ok(())
        }
        (BoundInitializerTarget::Unresolved, _) => Err(EmitError::Unsupported(
            "an initializer member that did not resolve",
        )),
    }
}

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
        match from {
            TypeSymbol::Pointer(_) => {}
            TypeSymbol::Special(
                SpecialType::UInt32 | SpecialType::UInt64 | SpecialType::UInt16 | SpecialType::Byte,
            ) => out.push(Instruction::simple(Opcode::ConvU)),
            _ => out.push(Instruction::simple(Opcode::ConvI)),
        }
        return Ok(());
    }
    if matches!(from, TypeSymbol::Pointer(_)) {
        if let TypeSymbol::Special(target) = to {
            let opcode = match target {
                SpecialType::UInt64 | SpecialType::Int64 => Opcode::ConvU8,
                SpecialType::UInt32 => Opcode::ConvU4,
                SpecialType::Int32 => Opcode::ConvI4,
                SpecialType::SByte => Opcode::ConvI1,
                SpecialType::Byte => Opcode::ConvU1,
                SpecialType::Int16 => Opcode::ConvI2,
                SpecialType::UInt16 => Opcode::ConvU2,
                _ => return Err(EmitError::Unsupported("this pointer cast is not lowered")),
            };
            out.push(Instruction::simple(opcode));
            return Ok(());
        }
        return Err(EmitError::Unsupported("this pointer cast is not lowered"));
    }
    if is_value_type(to, tokens) && !is_value_type(from, tokens) {
        let token = tokens.instruction_type_token(to).ok_or(EmitError::Unsupported(
            "unboxing to a value type with no metadata token",
        ))?;
        out.push(Instruction::new(Opcode::Unbox, Operand::Token(token)));
        out.push(Instruction::new(Opcode::Ldobj, Operand::Token(token)));
        return Ok(());
    }
    if matches!(to, TypeSymbol::Special(SpecialType::String)) {
        let token = tokens.instruction_type_token(to).ok_or(EmitError::Unsupported(
            "a cast to string with no metadata token",
        ))?;
        out.push(Instruction::new(Opcode::Castclass, Operand::Token(token)));
        return Ok(());
    }
    if matches!(to, TypeSymbol::Special(SpecialType::Object)) {
        if boxes_to_a_reference(from, tokens) {
            let token = tokens.instruction_type_token(from).ok_or(EmitError::Unsupported(
                "boxing to object with no metadata token",
            ))?;
            out.push(Instruction::new(Opcode::Box, Operand::Token(token)));
        }
        return Ok(());
    }
    let target_special = match to {
        TypeSymbol::Special(special) => Some(*special),
        _ if tokens.is_enum(to) => Some(tokens.enum_underlying(to).unwrap_or(SpecialType::Int32)),
        _ => None,
    };
    if let Some(target_special) = target_special {
        let source = conversion_operand_type(from, tokens);
        let target = TypeSymbol::Special(target_special);
        if checked {
            let unsigned_source =
                matches!(&source, TypeSymbol::Special(special) if special.is_unsigned());
            if let Some(ovf) = checked_overflow_conversion(target_special, unsigned_source) {
                out.push(Instruction::simple(ovf));
                return Ok(());
            }
        }
        return emit_numeric_conversion(&source, &target, out);
    }
    if boxes_to_a_reference(from, tokens) {
        let token = tokens.instruction_type_token(from).ok_or(EmitError::Unsupported(
            "boxing to an interface with no metadata token",
        ))?;
        out.push(Instruction::new(Opcode::Box, Operand::Token(token)));
        return Ok(());
    }
    let to_reference = matches!(to, TypeSymbol::Array { .. })
        || (matches!(to, TypeSymbol::Named(_) | TypeSymbol::Instantiation { .. })
            && !is_value_type(to, tokens));
    if to_reference {
        let token = tokens.instruction_type_token(to).ok_or(EmitError::Unsupported(
            "a cast to a reference type with no metadata token",
        ))?;
        out.push(Instruction::new(Opcode::Castclass, Operand::Token(token)));
        return Ok(());
    }
    Err(EmitError::Unsupported("this cast is not lowered yet"))
}

/// Emits `box !T` when `ty` is a bare type parameter of the body being emitted, and nothing
/// otherwise. A reference comparison converts its operand to `object`, and for a type parameter
/// that conversion is a box.
///
/// It is keyed on [`Tokens::body_type_parameter`] rather than on [`is_value_type`], which answers
/// NO for a type parameter: a `T` is neither a struct nor an enum in the token tables, so a box
/// gated on that predicate is skipped for exactly the types that need one.
fn box_type_parameter(
    ty: &TypeSymbol,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if tokens.body_type_parameter(ty).is_none() {
        return Ok(());
    }
    let token = tokens
        .instruction_type_token(ty)
        .ok_or(EmitError::Unsupported(
            "boxing a type parameter for a reference comparison with no metadata token",
        ))?;
    out.push(Instruction::new(Opcode::Box, Operand::Token(token)));
    Ok(())
}

/// Whether converting `ty` to `object` or to an interface must emit `box`.
///
/// **A TYPE PARAMETER BOXES HERE EVEN THOUGH IT IS NOT A VALUE TYPE**, and that is the difference
/// between a program that runs and one the runtime refuses to load. `T` is decided per
/// instantiation, so the compiler cannot know which conversion it is -- and `box !n` is the answer
/// for BOTH: it copies a value type onto the heap and is a no-op returning the same reference for a
/// reference type (III.4.1). Omitting it leaves a `!0` on the stack where the callee's signature
/// says `object`, which is not a type error the emitter can see and IS one the verifier reports as
/// `InvalidProgramException` at the first call.
///
/// Stated once and used at every boxing site, because the value-type test and the parameter test
/// answer the same question and a site that asks only the first one silently emits the shorter,
/// wrong sequence.
fn boxes_to_a_reference(ty: &TypeSymbol, tokens: &Tokens) -> bool {
    is_value_type(ty, tokens) || tokens.body_type_parameter(ty).is_some()
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

/// Emits the instruction (if any) for a conversion from `from` to `target`.
fn emit_conversion(
    conversion: ConversionKind,
    from: &TypeSymbol,
    target: &TypeSymbol,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match conversion {
        ConversionKind::ImplicitNumeric => emit_numeric_conversion(from, target, out),
        ConversionKind::ImplicitReference => Ok(()),
        ConversionKind::Boxing => Err(EmitError::Unsupported("boxing (needs a metadata token)")),
    }
}

/// Emits the `conv.*` that produces numeric `target` from a value of numeric `source` on the
/// stack. An unsigned source widens to a wider integer without sign extension (`uint` to `long`
/// is `conv.u8`, III.3.19) and reaches a floating-point type through `conv.r.un` -- which reads
/// the source integer as unsigned (III.3.31) -- before narrowing; a signed source, and any
/// same-or-narrowing integral target, use the plain width-keyed `conv.*` (III.3.17).
/// The type a numeric conversion actually operates on: an enum's UNDERLYING integral type, and
/// `ty` unchanged otherwise.
///
/// **ONE IMPLEMENTATION, CALLED FOR BOTH ENDS OF A CONVERSION.** An enum is its underlying type on
/// the source side exactly as it is on the target side (ECMA-335 II.14.3), and the two ends used to
/// be decided separately -- the target by an arm that hardcoded `conv.i4`, the source by a
/// signedness test that only recognized a primitive. A `uint`-backed enum widening to `long`
/// therefore sign-extended.
fn conversion_operand_type(ty: &TypeSymbol, tokens: &Tokens) -> TypeSymbol {
    match tokens.enum_underlying(ty) {
        Some(underlying) => TypeSymbol::Special(underlying),
        None => ty.clone(),
    }
}

fn emit_numeric_conversion(
    source: &TypeSymbol,
    target: &TypeSymbol,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let unsigned_source = matches!(source, TypeSymbol::Special(s) if s.is_unsigned());
    if unsigned_source {
        if let TypeSymbol::Special(target) = target {
            match target {
                SpecialType::Int64 => {
                    out.push(Instruction::simple(Opcode::ConvU8));
                    return Ok(());
                }
                SpecialType::Single => {
                    out.push(Instruction::simple(Opcode::ConvRUn));
                    out.push(Instruction::simple(Opcode::ConvR4));
                    return Ok(());
                }
                SpecialType::Double => {
                    out.push(Instruction::simple(Opcode::ConvRUn));
                    out.push(Instruction::simple(Opcode::ConvR8));
                    return Ok(());
                }
                _ => {}
            }
        }
    }
    out.push(Instruction::simple(numeric_conversion(target)?));
    Ok(())
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

/// Narrows an int-width stack value back to a sub-int type's width (`conv.u1`/`conv.i2`/...),
/// so an operation defined in the narrower type wraps correctly (e.g. a `byte`-backed enum at
/// 256). A no-op for int/uint/long/ulong (already the right width) and any non-sub-int type.
fn narrow_subint(ty: &TypeSymbol, out: &mut Vec<Instruction>) {
    if matches!(
        ty,
        TypeSymbol::Special(
            SpecialType::SByte
                | SpecialType::Byte
                | SpecialType::Int16
                | SpecialType::UInt16
                | SpecialType::Char
        )
    ) {
        if let Ok(op) = numeric_conversion(ty) {
            out.push(Instruction::simple(op));
        }
    }
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
            out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(0)));
            Ok(())
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
                    .instruction_type_token(&receiver.ty)
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
        BoundExprKind::Dereference { operand } => emit_expression(operand, frame, tokens, out),
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
                .instruction_type_token(element)
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
            None,
            false,
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
    if let TypeSymbol::Pointer(element) = &operand.ty {
        emit_sizeof(element, tokens, out)?;
    } else {
        out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(1)));
    }
    let enum_underlying = tokens.enum_underlying(&operand.ty);
    let step_ty = match enum_underlying {
        Some(special) => TypeSymbol::Special(special),
        None => operand.ty.clone(),
    };
    if matches!(
        step_ty,
        TypeSymbol::Special(SpecialType::Int64 | SpecialType::UInt64)
    ) {
        out.push(Instruction::simple(Opcode::ConvI8));
    }
    out.push(Instruction::simple(if increment {
        Opcode::Add
    } else {
        Opcode::Sub
    }));
    if enum_underlying.is_some() {
        narrow_subint(&step_ty, out);
    }
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
            .instruction_type_token(element)
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
            .instruction_type_token(element)
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
            let wide = matches!(
                ty,
                TypeSymbol::Special(SpecialType::Int64 | SpecialType::UInt64)
            ) || matches!(
                tokens.enum_underlying(ty),
                Some(SpecialType::Int64 | SpecialType::UInt64)
            );
            if wide {
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
        .instruction_type_token(target)
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
/// Lowers `default(T)` (14.5.13): the target type's zero.
///
/// Three shapes, chosen by what the type IS rather than by what it is called:
///
/// - a REFERENCE type -- `ldnull`;
/// - a PRIMITIVE -- its zero literal, at the right width (`ldc.i4.0`, `ldc.i8 0`, `ldc.r4 0`);
/// - a VALUE type OR a TYPE PARAMETER -- a temporary, `ldloca; initobj; ldloc`.
///
/// **THE TYPE PARAMETER IS THE CASE THE OPERATOR EXISTS FOR, AND IT IS WHY THE VALUE CANNOT BE
/// FOLDED EARLIER.** `T` may close over a reference type, where the answer is `null`, or a struct,
/// where it is an all-zero value -- and one `default(T)` is both, decided per instantiation. So the
/// lowering is the one form that is correct for either: `initobj` over a token naming `!n`, which
/// the runtime resolves against the instantiation in hand.
///
/// **A BARE `T` HAS NO ORDINARY TOKEN, DELIBERATELY** -- minting one would invent a `TypeRef` to
/// a type called `T` that no assembly declares, which is a defect this compiler had and removed. It
/// is named instead by a `TypeSpec` whose blob is `ELEMENT_TYPE_VAR n`, pre-minted per POSITION
/// (see `Tokens::var_spec`) because emission cannot mint.
fn emit_default_value(
    target: &TypeSymbol,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if let TypeSymbol::Named(parts) = target
        && let [only] = &parts[..]
        && let Some(index) = frame.type_parameter_index(only)
    {
        let spec = tokens.var_spec(index).ok_or(EmitError::Unsupported(
            "default of a type parameter with no TypeSpec minted for its position",
        ))?;
        let slot = frame.reserve_local(target);
        out.push(Instruction::new(Opcode::Ldloca, Operand::Variable(slot)));
        out.push(Instruction::new(Opcode::Initobj, Operand::Token(spec)));
        out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
        return Ok(());
    }
    if let TypeSymbol::Special(special) = target {
        match special {
            SpecialType::Boolean
            | SpecialType::Char
            | SpecialType::SByte
            | SpecialType::Byte
            | SpecialType::Int16
            | SpecialType::UInt16
            | SpecialType::Int32
            | SpecialType::UInt32 => {
                out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(0)));
                return Ok(());
            }
            SpecialType::Int64 | SpecialType::UInt64 => {
                out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(0)));
                out.push(Instruction::new(Opcode::ConvI8, Operand::None));
                return Ok(());
            }
            SpecialType::Single => {
                out.push(Instruction::new(Opcode::LdcR4, Operand::Float32(0.0)));
                return Ok(());
            }
            SpecialType::Double => {
                out.push(Instruction::new(Opcode::LdcR8, Operand::Float64(0.0)));
                return Ok(());
            }
            _ => {}
        }
    }
    if is_value_type(target, tokens) {
        let type_token = tokens.instruction_type_token(target).ok_or(EmitError::Unsupported(
            "default of a value type with no metadata token",
        ))?;
        let slot = frame.reserve_local(target);
        out.push(Instruction::new(Opcode::Ldloca, Operand::Variable(slot)));
        out.push(Instruction::new(Opcode::Initobj, Operand::Token(type_token)));
        out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
        return Ok(());
    }
    out.push(Instruction::new(Opcode::Ldnull, Operand::None));
    Ok(())
}

pub(crate) fn emit_sizeof(
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
        .instruction_type_token(target)
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

/// Whether `ty` is `System.IntPtr`. In a signature it is the `native int` (ELEMENT_TYPE_I)
/// primitive -- the encoding the BCL uses -- so a MemberRef to a method taking or returning it
/// (e.g. `Marshal.AllocHGlobal(int) -> IntPtr`, `IntPtr.Zero`) resolves. It is the same type
/// either way (II.14.4.3 deems `System.IntPtr` and `native int` interchangeable), but only the
/// primitive form matches the BCL's own signatures.
pub(crate) fn is_native_int(ty: &TypeSymbol) -> bool {
    matches!(ty, TypeSymbol::Named(parts)
        if parts.len() == 2 && &*parts[0] == "System" && &*parts[1] == "IntPtr")
}

/// Whether `ty` is `System.UIntPtr` -- the `native uint` (ELEMENT_TYPE_U) primitive, the unsigned
/// companion to [`is_native_int`].
pub(crate) fn is_native_uint(ty: &TypeSymbol) -> bool {
    matches!(ty, TypeSymbol::Named(parts)
        if parts.len() == 2 && &*parts[0] == "System" && &*parts[1] == "UIntPtr")
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
        .instruction_type_token(&operand.ty)
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
        .instruction_type_token(target)
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
/// Widens a pointer-arithmetic offset to native width before it joins the address math
/// (18.5.6): an unsigned offset zero-extends (`conv.u`), a signed one sign-extends
/// (`conv.i`). Without this a 32-bit unsigned offset >= 2^31 would ride the i4 add
/// sign-extended and land 4 GB away on a 64-bit host.
fn widen_pointer_offset(offset_ty: &TypeSymbol, out: &mut Vec<Instruction>) {
    let opcode = match offset_ty {
        TypeSymbol::Special(SpecialType::UInt32 | SpecialType::UInt64) => Opcode::ConvU,
        _ => Opcode::ConvI,
    };
    out.push(Instruction::simple(opcode));
}

fn emit_pointer_arithmetic(
    operator: BinaryOperator,
    left: &BoundExpr,
    right: &BoundExpr,
    checked: bool,
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
            widen_pointer_offset(&offset.ty, out);
            emit_sizeof(&element, tokens, out)?;
            out.push(Instruction::simple(Opcode::Mul));
            out.push(Instruction::simple(if checked {
                Opcode::AddOvfUn
            } else {
                Opcode::Add
            }));
            Ok(true)
        }
        BinaryOperator::Subtract => {
            if let (Some(element), Some(_)) =
                (pointer_element(&left.ty), pointer_element(&right.ty))
            {
                emit_expression(left, frame, tokens, out)?;
                emit_expression(right, frame, tokens, out)?;
                out.push(Instruction::simple(Opcode::Sub));
                out.push(Instruction::simple(Opcode::ConvI8));
                emit_sizeof(&element, tokens, out)?;
                out.push(Instruction::simple(Opcode::ConvI8));
                out.push(Instruction::simple(Opcode::Div));
                return Ok(true);
            }
            if let Some(element) = pointer_element(&left.ty) {
                emit_expression(left, frame, tokens, out)?;
                emit_expression(right, frame, tokens, out)?;
                widen_pointer_offset(&right.ty, out);
                emit_sizeof(&element, tokens, out)?;
                out.push(Instruction::simple(Opcode::Mul));
                out.push(Instruction::simple(if checked {
                    Opcode::SubOvfUn
                } else {
                    Opcode::Sub
                }));
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
    let unsigned = matches!(operand_ty, TypeSymbol::Special(special) if special.is_unsigned())
        || matches!(operand_ty, TypeSymbol::Pointer(_));
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
pub(crate) fn checked_overflow_conversion(target: SpecialType, unsigned_source: bool) -> Option<Opcode> {
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
