//! Lowering a bound method body to a CIL instruction stream (ECMA-335 1st ed,
//! Partition III).

use crate::expr::{EmitError, emit_expression, emit_local};
use crate::frame::{Frame, Slot};
use crate::tokens::Tokens;
use alloc::boxed::Box;
use alloc::vec::Vec;
use lamella_binder::{
    BoundCatch, BoundExpr, BoundExprKind, BoundStmt, BoundStmtKind, SpecialType, TypeSymbol,
    always_exits,
};
use lamella_cil::{EhClause, EhKind, Instruction, InstructionRange, Opcode, Operand};
use lamella_syntax::ast::{AssignmentOperator, PostfixOperator, UnaryOperator};
use lamella_syntax::span::Span;

/// A statement's first instruction index paired with its source span -- the raw
/// material the debug-info writer turns into a source-line mapping.
pub type SequencePoint = (u32, Span);

/// A method body's emitted CIL plus what later stages need from it: the local
/// types (for the local-variable signature) and the sequence points, which the
/// debug-info writer turns into source-line mappings.
pub struct EmittedBody {
    /// The lowered instruction stream.
    pub code: Vec<Instruction>,
    /// The local-variable types in slot order.
    pub local_types: Vec<TypeSymbol>,
    /// The local-variable names in slot order (parallel to `local_types`).
    pub local_names: Vec<Box<str>>,
    /// One sequence point per statement, in emission order.
    pub sequence_points: Vec<SequencePoint>,
    /// The exception-handling clauses for the method body's try statements.
    pub handlers: Vec<EhClause>,
}

/// The enclosing loop's branch targets, for `break` and `continue`.
struct LoopContext {
    continue_label: usize,
    break_label: usize,
}

/// A method's return epilogue, used when the body has a `try`: a `return` cannot
/// `ret` from inside a protected region, so it parks its value in `return_slot`
/// (when non-void) and `leave`s to `label`, where the single `ret` lives.
#[derive(Clone, Copy)]
struct Epilogue {
    label: usize,
    return_slot: Option<u16>,
}

/// Tracks branch labels (backpatched once known) and the sequence points recorded
/// at statement boundaries during emission.
#[derive(Default)]
struct Labels {
    positions: Vec<Option<u32>>,
    pending: Vec<(usize, usize)>,
    loops: Vec<LoopContext>,
    points: Vec<SequencePoint>,
    handlers: Vec<EhClause>,
    epilogue: Option<Epilogue>,
}

impl Labels {
    fn label(&mut self) -> usize {
        self.positions.push(None);
        self.positions.len() - 1
    }

    fn place(&mut self, label: usize, out: &[Instruction]) {
        self.positions[label] = Some(out.len() as u32);
    }

    fn branch(&mut self, opcode: Opcode, label: usize, out: &mut Vec<Instruction>) {
        out.push(Instruction::new(opcode, Operand::Target(0)));
        self.pending.push((out.len() - 1, label));
    }

    fn backpatch(&self, out: &mut [Instruction]) {
        for &(index, label) in &self.pending {
            if let Some(position) = self.positions[label] {
                out[index].operand = Operand::Target(position);
            }
        }
    }
}

/// The maximum evaluation-stack depth a straight-line/structured body reaches --
/// the method's `.maxstack` (II.25.4.3). Computed by tracking the running depth
/// from each instruction's net stack effect; our emitter keeps the stack balanced
/// at statement boundaries, so a single forward pass suffices.
#[must_use]
pub fn max_stack(code: &[Instruction]) -> u16 {
    let mut depth: i32 = 0;
    let mut high: i32 = 0;
    for instruction in code {
        depth += stack_effect(instruction.opcode);
        high = high.max(depth);
        depth = depth.max(0);
    }
    u16::try_from(high).unwrap_or(u16::MAX)
}

/// The net change an opcode makes to the evaluation-stack depth, for the opcodes
/// the emitter produces.
fn stack_effect(opcode: Opcode) -> i32 {
    match opcode {
        Opcode::LdcI4
        | Opcode::LdcI8
        | Opcode::Ldnull
        | Opcode::Ldarg
        | Opcode::Ldloc
        | Opcode::Ldsfld
        | Opcode::Newobj
        | Opcode::Call
        | Opcode::Callvirt => 1,
        Opcode::Add
        | Opcode::Sub
        | Opcode::Mul
        | Opcode::Div
        | Opcode::Rem
        | Opcode::And
        | Opcode::Or
        | Opcode::Xor
        | Opcode::Shl
        | Opcode::Shr
        | Opcode::Ceq
        | Opcode::Cgt
        | Opcode::Clt => -1,
        Opcode::Stloc
        | Opcode::Starg
        | Opcode::Stsfld
        | Opcode::Pop
        | Opcode::Brfalse
        | Opcode::Brtrue
        | Opcode::Throw
        | Opcode::Ret => -1,
        _ => 0,
    }
}

/// Lowers a bound method body to CIL. `parameters` are the argument names in
/// source order; the body's locals take the slots after them.
pub fn emit_method(
    parameters: &[Box<str>],
    body: &BoundStmt,
) -> Result<Vec<Instruction>, EmitError> {
    let mut frame = Frame::build(parameters, body, 0);
    Ok(lower(
        &mut frame,
        &Tokens::new(),
        body,
        &TypeSymbol::Special(SpecialType::Void),
    )?
    .0)
}

/// Lowers a method body and reports its local types (for the local signature) and
/// sequence points (for debug info). `tokens` resolves members; `arg_base` is 1 for
/// an instance method (argument 0 is `this`), else 0; `return_type` is the method's
/// return type, for the epilogue a `try` needs.
pub fn emit_body(
    parameters: &[Box<str>],
    body: &BoundStmt,
    tokens: &Tokens,
    arg_base: u16,
    return_type: &TypeSymbol,
) -> Result<EmittedBody, EmitError> {
    let mut frame = Frame::build(parameters, body, arg_base);
    let lowered = lower(&mut frame, tokens, body, return_type)?;
    Ok(EmittedBody {
        code: lowered.0,
        local_types: frame.local_types().to_vec(),
        local_names: frame.local_names(),
        sequence_points: lowered.1,
        handlers: lowered.2,
    })
}

/// A lowered body: the instruction stream, its sequence points, and its
/// exception-handling clauses.
type Lowered = (Vec<Instruction>, Vec<SequencePoint>, Vec<EhClause>);

fn lower(
    frame: &mut Frame,
    tokens: &Tokens,
    body: &BoundStmt,
    return_type: &TypeSymbol,
) -> Result<Lowered, EmitError> {
    let mut labels = Labels::default();
    if contains_try(body) {
        let return_slot = if matches!(return_type, TypeSymbol::Special(SpecialType::Void)) {
            None
        } else {
            Some(frame.reserve_local(return_type))
        };
        let label = labels.label();
        labels.epilogue = Some(Epilogue { label, return_slot });
    }

    let mut out = Vec::new();
    emit_statement(body, frame, tokens, &mut labels, &mut out)?;

    if let Some(Epilogue { label, return_slot }) = labels.epilogue {
        labels.place(label, &out);
        if let Some(slot) = return_slot {
            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
        }
        out.push(Instruction::simple(Opcode::Ret));
    } else {
        let end = out.len() as u32;
        let branch_to_end = labels.positions.contains(&Some(end));
        if branch_to_end || out.last().map(|instruction| instruction.opcode) != Some(Opcode::Ret) {
            out.push(Instruction::simple(Opcode::Ret));
        }
    }
    labels.backpatch(&mut out);
    Ok((out, labels.points, labels.handlers))
}

/// Whether `stmt` contains a `try` anywhere, so the body needs a return epilogue.
fn contains_try(stmt: &BoundStmt) -> bool {
    use BoundStmtKind as Kind;
    match &stmt.kind {
        Kind::Try { .. } => true,
        Kind::Block(statements) => statements.iter().any(contains_try),
        Kind::If {
            then_branch,
            else_branch,
            ..
        } => contains_try(then_branch) || else_branch.as_deref().is_some_and(contains_try),
        Kind::While { body, .. }
        | Kind::DoWhile { body, .. }
        | Kind::For { body, .. }
        | Kind::ForEach { body, .. }
        | Kind::Lock { body, .. }
        | Kind::Using { body, .. }
        | Kind::Labeled { body, .. } => contains_try(body),
        Kind::Checked(inner) | Kind::Unchecked(inner) => contains_try(inner),
        Kind::Switch { sections, .. } => sections.iter().flatten().any(contains_try),
        _ => false,
    }
}

fn emit_statement(
    stmt: &BoundStmt,
    frame: &Frame,
    tokens: &Tokens,
    labels: &mut Labels,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if !matches!(stmt.kind, BoundStmtKind::Block(_) | BoundStmtKind::Empty) {
        labels.points.push((out.len() as u32, stmt.span));
    }
    match &stmt.kind {
        BoundStmtKind::Empty => {}
        BoundStmtKind::Block(statements) => {
            for statement in statements {
                emit_statement(statement, frame, tokens, labels, out)?;
            }
        }
        BoundStmtKind::Local { declarators, .. } => {
            for declarator in declarators {
                if let Some(initializer) = &declarator.initializer {
                    emit_expression(initializer, frame, tokens, out)?;
                    store_to(frame, &declarator.name, out)?;
                }
            }
        }
        BoundStmtKind::Expression(expr) => emit_statement_expression(expr, frame, tokens, out)?,
        BoundStmtKind::Return(value) => {
            if let Some(value) = value {
                emit_expression(value, frame, tokens, out)?;
            }
            match labels.epilogue {
                Some(Epilogue { label, return_slot }) => {
                    if let Some(slot) = return_slot {
                        out.push(Instruction::new(Opcode::Stloc, Operand::Variable(slot)));
                    }
                    labels.branch(Opcode::Leave, label, out);
                }
                None => out.push(Instruction::simple(Opcode::Ret)),
            }
        }
        BoundStmtKind::If {
            condition,
            then_branch,
            else_branch,
        } => emit_if(
            condition,
            then_branch,
            else_branch.as_deref(),
            frame,
            tokens,
            labels,
            out,
        )?,
        BoundStmtKind::While { condition, body } => {
            let start = labels.label();
            labels.place(start, out);
            emit_expression(condition, frame, tokens, out)?;
            let end = labels.label();
            labels.branch(Opcode::Brfalse, end, out);
            labels.loops.push(LoopContext {
                continue_label: start,
                break_label: end,
            });
            emit_statement(body, frame, tokens, labels, out)?;
            labels.loops.pop();
            labels.branch(Opcode::Br, start, out);
            labels.place(end, out);
        }
        BoundStmtKind::DoWhile { body, condition } => {
            let start = labels.label();
            labels.place(start, out);
            let test = labels.label();
            let end = labels.label();
            labels.loops.push(LoopContext {
                continue_label: test,
                break_label: end,
            });
            emit_statement(body, frame, tokens, labels, out)?;
            labels.loops.pop();
            labels.place(test, out);
            emit_expression(condition, frame, tokens, out)?;
            labels.branch(Opcode::Brtrue, start, out);
            labels.place(end, out);
        }
        BoundStmtKind::For {
            initializer,
            condition,
            iterators,
            body,
        } => emit_for(
            initializer,
            condition.as_ref(),
            iterators,
            body,
            frame,
            tokens,
            labels,
            out,
        )?,
        BoundStmtKind::Break => {
            let target = loop_target(labels, |context| context.break_label)?;
            labels.branch(Opcode::Br, target, out);
        }
        BoundStmtKind::Continue => {
            let target = loop_target(labels, |context| context.continue_label)?;
            labels.branch(Opcode::Br, target, out);
        }
        BoundStmtKind::Checked(inner) | BoundStmtKind::Unchecked(inner) => {
            emit_statement(inner, frame, tokens, labels, out)?;
        }
        BoundStmtKind::Throw(value) => match value {
            Some(expr) => {
                emit_expression(expr, frame, tokens, out)?;
                out.push(Instruction::simple(Opcode::Throw));
            }
            None => out.push(Instruction::simple(Opcode::Rethrow)),
        },
        BoundStmtKind::Try {
            body,
            catches,
            finally,
        } => emit_try(
            body,
            catches,
            finally.as_deref(),
            frame,
            tokens,
            labels,
            out,
        )?,
        _ => {
            return Err(EmitError::Unsupported(
                "this statement form is not lowered yet",
            ));
        }
    }
    Ok(())
}

/// Lowers a `try` statement (15.10) to a protected region with catch and/or
/// finally handlers, recorded as exception-handling clauses. Each region exits with
/// `leave` to the instruction past the whole statement (the runtime runs any
/// intervening `finally` on the way); a `finally` handler ends with `endfinally`.
fn emit_try(
    body: &BoundStmt,
    catches: &[BoundCatch],
    finally: Option<&BoundStmt>,
    frame: &Frame,
    tokens: &Tokens,
    labels: &mut Labels,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let end = labels.label();

    let try_start = out.len() as u32;
    emit_statement(body, frame, tokens, labels, out)?;
    if !always_exits(body) {
        labels.branch(Opcode::Leave, end, out);
    }
    let try_range = InstructionRange {
        start: try_start,
        end: out.len() as u32,
    };

    for catch in catches {
        let handler_start = out.len() as u32;
        match catch.name.as_deref().and_then(|name| frame.slot(name)) {
            Some(Slot::Local(slot)) => {
                out.push(Instruction::new(Opcode::Stloc, Operand::Variable(slot)));
            }
            _ => out.push(Instruction::simple(Opcode::Pop)),
        }
        emit_statement(&catch.body, frame, tokens, labels, out)?;
        if !always_exits(&catch.body) {
            labels.branch(Opcode::Leave, end, out);
        }
        let ty = catch
            .exception_type
            .clone()
            .unwrap_or(TypeSymbol::Special(SpecialType::Object));
        let token = tokens
            .type_token(&ty)
            .ok_or(EmitError::Unsupported("a catch clause's type has no token"))?;
        labels.handlers.push(EhClause {
            try_range,
            handler_range: InstructionRange {
                start: handler_start,
                end: out.len() as u32,
            },
            kind: EhKind::Catch(token),
        });
    }

    if let Some(finally) = finally {
        let handler_start = out.len() as u32;
        emit_statement(finally, frame, tokens, labels, out)?;
        out.push(Instruction::simple(Opcode::Endfinally));
        labels.handlers.push(EhClause {
            try_range: InstructionRange {
                start: try_start,
                end: handler_start,
            },
            handler_range: InstructionRange {
                start: handler_start,
                end: out.len() as u32,
            },
            kind: EhKind::Finally,
        });
    }

    labels.place(end, out);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_if(
    condition: &BoundExpr,
    then_branch: &BoundStmt,
    else_branch: Option<&BoundStmt>,
    frame: &Frame,
    tokens: &Tokens,
    labels: &mut Labels,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    emit_expression(condition, frame, tokens, out)?;
    match else_branch {
        None => {
            let end = labels.label();
            labels.branch(Opcode::Brfalse, end, out);
            emit_statement(then_branch, frame, tokens, labels, out)?;
            labels.place(end, out);
        }
        Some(else_branch) => {
            let else_label = labels.label();
            labels.branch(Opcode::Brfalse, else_label, out);
            emit_statement(then_branch, frame, tokens, labels, out)?;
            let end = labels.label();
            labels.branch(Opcode::Br, end, out);
            labels.place(else_label, out);
            emit_statement(else_branch, frame, tokens, labels, out)?;
            labels.place(end, out);
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn emit_for(
    initializer: &[BoundStmt],
    condition: Option<&BoundExpr>,
    iterators: &[BoundExpr],
    body: &BoundStmt,
    frame: &Frame,
    tokens: &Tokens,
    labels: &mut Labels,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    for statement in initializer {
        emit_statement(statement, frame, tokens, labels, out)?;
    }
    let start = labels.label();
    labels.place(start, out);
    let end = labels.label();
    if let Some(condition) = condition {
        emit_expression(condition, frame, tokens, out)?;
        labels.branch(Opcode::Brfalse, end, out);
    }
    let step = labels.label();
    labels.loops.push(LoopContext {
        continue_label: step,
        break_label: end,
    });
    emit_statement(body, frame, tokens, labels, out)?;
    labels.loops.pop();
    labels.place(step, out);
    for iterator in iterators {
        emit_statement_expression(iterator, frame, tokens, out)?;
    }
    labels.branch(Opcode::Br, start, out);
    labels.place(end, out);
    Ok(())
}

fn loop_target(
    labels: &Labels,
    select: impl Fn(&LoopContext) -> usize,
) -> Result<usize, EmitError> {
    labels
        .loops
        .last()
        .map(select)
        .ok_or(EmitError::Unsupported("break/continue outside a loop"))
}

/// Lowers an expression used as a statement: an assignment or `++`/`--` to a local
/// stores in place; any other value is computed and discarded.
fn emit_statement_expression(
    expr: &BoundExpr,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match &expr.kind {
        BoundExprKind::Assignment {
            operator: AssignmentOperator::Assign,
            target,
            value,
        } => {
            if let BoundExprKind::Local(name) = &target.kind {
                emit_expression(value, frame, tokens, out)?;
                return store_to(frame, name, out);
            }
            if let BoundExprKind::FieldAccess {
                receiver, field, ..
            } = &target.kind
            {
                return crate::expr::emit_field_store(
                    field.as_ref(),
                    receiver,
                    value,
                    frame,
                    tokens,
                    out,
                );
            }
            if let BoundExprKind::ElementAccess { receiver, indices } = &target.kind {
                return crate::expr::emit_element_store(
                    &target.ty, receiver, indices, value, frame, tokens, out,
                );
            }
            if let BoundExprKind::PropertyAccess { receiver, name } = &target.kind {
                return crate::expr::emit_property_store(
                    &target.ty, receiver, name, value, frame, tokens, out,
                );
            }
        }
        BoundExprKind::Postfix { operator, operand } => {
            if let BoundExprKind::Local(name) = &operand.kind {
                return emit_step(frame, name, *operator == PostfixOperator::Increment, out);
            }
        }
        BoundExprKind::Unary {
            operator: operator @ (UnaryOperator::PreIncrement | UnaryOperator::PreDecrement),
            operand,
        } => {
            if let BoundExprKind::Local(name) = &operand.kind {
                return emit_step(frame, name, *operator == UnaryOperator::PreIncrement, out);
            }
        }
        _ => {}
    }
    emit_expression(expr, frame, tokens, out)?;
    if !matches!(expr.ty, TypeSymbol::Special(SpecialType::Void)) {
        out.push(Instruction::simple(Opcode::Pop));
    }
    Ok(())
}

/// Increments or decrements a local in place: load, +/- 1, store.
fn emit_step(
    frame: &Frame,
    name: &str,
    increment: bool,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    emit_local(name, frame, out)?;
    out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(1)));
    out.push(Instruction::simple(if increment {
        Opcode::Add
    } else {
        Opcode::Sub
    }));
    store_to(frame, name, out)
}

fn store_to(frame: &Frame, name: &str, out: &mut Vec<Instruction>) -> Result<(), EmitError> {
    match frame.slot(name) {
        Some(Slot::Local(slot)) => {
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(slot)))
        }
        Some(Slot::Argument(slot)) => {
            out.push(Instruction::new(Opcode::Starg, Operand::Variable(slot)));
        }
        None => return Err(EmitError::Unsupported("store to a name with no frame slot")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_binder::{Binder, SpecialType, TypeSymbol};
    use lamella_syntax::parser::parse_statement;

    fn int() -> TypeSymbol {
        TypeSymbol::Special(SpecialType::Int32)
    }

    fn emit(parameter_names: &[&str], body_source: &str) -> Vec<Instruction> {
        let body = parse_statement(body_source).statement;
        let params: Vec<(Box<str>, TypeSymbol)> = parameter_names
            .iter()
            .map(|name| ((*name).into(), int()))
            .collect();
        let bound = Binder::new().bind_method(None, "M", int(), &params, &body);
        let names: Vec<Box<str>> = parameter_names.iter().map(|name| (*name).into()).collect();
        emit_method(&names, &bound).expect("should lower")
    }

    fn opcodes(instructions: &[Instruction]) -> Vec<Opcode> {
        instructions.iter().map(|i| i.opcode).collect()
    }

    fn target(instruction: &Instruction) -> u32 {
        match instruction.operand {
            Operand::Target(index) => index,
            _ => panic!("expected a branch target"),
        }
    }

    #[test]
    fn arguments_load_and_a_method_returns() {
        assert_eq!(
            opcodes(&emit(&["a", "b"], "{ return a + b; }")),
            [Opcode::Ldarg, Opcode::Ldarg, Opcode::Add, Opcode::Ret]
        );
    }

    #[test]
    fn if_else_lowers_to_brfalse_and_br() {
        let body = emit(&["a", "b"], "{ if (a > b) return a; else return b; }");
        assert_eq!(
            opcodes(&body),
            [
                Opcode::Ldarg,
                Opcode::Ldarg,
                Opcode::Cgt,
                Opcode::Brfalse,
                Opcode::Ldarg,
                Opcode::Ret,
                Opcode::Br,
                Opcode::Ldarg,
                Opcode::Ret,
                Opcode::Ret,
            ]
        );
        assert_eq!(target(&body[3]), 7);
    }

    #[test]
    fn emission_records_a_sequence_point_per_statement() {
        let body = parse_statement("{ int x = 1; return x; }").statement;
        let bound = Binder::new().bind_method(None, "M", int(), &[], &body);
        let emitted = emit_body(&[], &bound, &Tokens::new(), 0, &int()).expect("should lower");

        let offsets: Vec<u32> = emitted
            .sequence_points
            .iter()
            .map(|(offset, _)| *offset)
            .collect();
        assert_eq!(offsets, [0, 2]);
        assert!(emitted.sequence_points[0].1.start < emitted.sequence_points[1].1.start);
    }

    #[test]
    fn max_stack_tracks_the_deepest_expression() {
        assert_eq!(max_stack(&emit(&["a", "b"], "{ return a + b; }")), 2);
        assert_eq!(max_stack(&emit(&[], "{ int x = 1 + 2 * 3; }")), 3);
        assert_eq!(max_stack(&emit(&[], "{ }")), 0);
    }

    #[test]
    fn widening_initializer_emits_conv() {
        assert_eq!(
            opcodes(&emit(&[], "{ long x = 1; }")),
            [Opcode::LdcI4, Opcode::ConvI8, Opcode::Stloc, Opcode::Ret]
        );
    }

    #[test]
    fn while_loops_back_to_the_condition() {
        let body = emit(&[], "{ int i = 0; while (i < 10) { i = i + 1; } }");
        assert_eq!(
            opcodes(&body),
            [
                Opcode::LdcI4,
                Opcode::Stloc,
                Opcode::Ldloc,
                Opcode::LdcI4,
                Opcode::Clt,
                Opcode::Brfalse,
                Opcode::Ldloc,
                Opcode::LdcI4,
                Opcode::Add,
                Opcode::Stloc,
                Opcode::Br,
                Opcode::Ret,
            ]
        );
        assert_eq!(target(&body[10]), 2);
        assert_eq!(target(&body[5]), 11);
    }

    #[test]
    fn for_loop_with_increment_and_break() {
        let body = emit(
            &["n"],
            "{ for (int i = 0; i < n; i++) { if (i > 3) break; } }",
        );
        let codes = opcodes(&body);
        assert!(codes.contains(&Opcode::Clt));
        assert!(codes.contains(&Opcode::Brfalse));
        assert!(codes.iter().filter(|&&c| c == Opcode::Br).count() >= 2);
        assert_eq!(*codes.last().unwrap(), Opcode::Ret);
    }
}
