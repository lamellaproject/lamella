//! Lowering a bound method body to a CIL instruction stream (ECMA-335 1st ed,
//! Partition III).

use crate::expr::{EmitError, emit_expression, emit_local};
use crate::frame::{Frame, Slot};
use alloc::boxed::Box;
use alloc::vec::Vec;
use lamella_binder::{BoundExpr, BoundExprKind, BoundStmt, BoundStmtKind, TypeSymbol};
use lamella_cil::{Instruction, Opcode, Operand};
use lamella_syntax::ast::{AssignmentOperator, PostfixOperator, UnaryOperator};

/// The enclosing loop's branch targets, for `break` and `continue`.
struct LoopContext {
    continue_label: usize,
    break_label: usize,
}

/// Tracks branch labels and backpatches their targets. A label is a reserved slot
/// whose instruction index is filled in by [`Labels::place`]; a branch records
/// itself for backpatching once that index is known.
#[derive(Default)]
struct Labels {
    positions: Vec<Option<u32>>,
    pending: Vec<(usize, usize)>,
    loops: Vec<LoopContext>,
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

/// Lowers a bound method body to CIL. `parameters` are the argument names in
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
        Opcode::LdcI4 | Opcode::LdcI8 | Opcode::Ldnull | Opcode::Ldarg | Opcode::Ldloc => 1,
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
        | Opcode::Pop
        | Opcode::Brfalse
        | Opcode::Brtrue
        | Opcode::Ret => -1,
        _ => 0,
    }
}

/// source order; the body's locals take the slots after them.
pub fn emit_method(
    parameters: &[Box<str>],
    body: &BoundStmt,
) -> Result<Vec<Instruction>, EmitError> {
    lower(&Frame::build(parameters, body), body)
}

/// Lowers a method body and also reports its local types in slot order, so the
/// caller can build the local-variable signature.
pub fn emit_body(
    parameters: &[Box<str>],
    body: &BoundStmt,
) -> Result<(Vec<Instruction>, Vec<TypeSymbol>), EmitError> {
    let frame = Frame::build(parameters, body);
    let code = lower(&frame, body)?;
    Ok((code, frame.local_types().to_vec()))
}

fn lower(frame: &Frame, body: &BoundStmt) -> Result<Vec<Instruction>, EmitError> {
    let mut labels = Labels::default();
    let mut out = Vec::new();
    emit_statement(body, frame, &mut labels, &mut out)?;
    let end = out.len() as u32;
    let branch_to_end = labels.positions.contains(&Some(end));
    if branch_to_end || out.last().map(|instruction| instruction.opcode) != Some(Opcode::Ret) {
        out.push(Instruction::simple(Opcode::Ret));
    }
    labels.backpatch(&mut out);
    Ok(out)
}

fn emit_statement(
    stmt: &BoundStmt,
    frame: &Frame,
    labels: &mut Labels,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match &stmt.kind {
        BoundStmtKind::Empty => {}
        BoundStmtKind::Block(statements) => {
            for statement in statements {
                emit_statement(statement, frame, labels, out)?;
            }
        }
        BoundStmtKind::Local { declarators, .. } => {
            for declarator in declarators {
                if let Some(initializer) = &declarator.initializer {
                    emit_expression(initializer, frame, out)?;
                    store_to(frame, &declarator.name, out)?;
                }
            }
        }
        BoundStmtKind::Expression(expr) => emit_statement_expression(expr, frame, out)?,
        BoundStmtKind::Return(value) => {
            if let Some(value) = value {
                emit_expression(value, frame, out)?;
            }
            out.push(Instruction::simple(Opcode::Ret));
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
            labels,
            out,
        )?,
        BoundStmtKind::While { condition, body } => {
            let start = labels.label();
            labels.place(start, out);
            emit_expression(condition, frame, out)?;
            let end = labels.label();
            labels.branch(Opcode::Brfalse, end, out);
            labels.loops.push(LoopContext {
                continue_label: start,
                break_label: end,
            });
            emit_statement(body, frame, labels, out)?;
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
            emit_statement(body, frame, labels, out)?;
            labels.loops.pop();
            labels.place(test, out);
            emit_expression(condition, frame, out)?;
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
            emit_statement(inner, frame, labels, out)?;
        }
        _ => {
            return Err(EmitError::Unsupported(
                "this statement form is not lowered yet",
            ));
        }
    }
    Ok(())
}

fn emit_if(
    condition: &BoundExpr,
    then_branch: &BoundStmt,
    else_branch: Option<&BoundStmt>,
    frame: &Frame,
    labels: &mut Labels,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    emit_expression(condition, frame, out)?;
    match else_branch {
        None => {
            let end = labels.label();
            labels.branch(Opcode::Brfalse, end, out);
            emit_statement(then_branch, frame, labels, out)?;
            labels.place(end, out);
        }
        Some(else_branch) => {
            let else_label = labels.label();
            labels.branch(Opcode::Brfalse, else_label, out);
            emit_statement(then_branch, frame, labels, out)?;
            let end = labels.label();
            labels.branch(Opcode::Br, end, out);
            labels.place(else_label, out);
            emit_statement(else_branch, frame, labels, out)?;
            labels.place(end, out);
        }
    }
    Ok(())
}

fn emit_for(
    initializer: &[BoundStmt],
    condition: Option<&BoundExpr>,
    iterators: &[BoundExpr],
    body: &BoundStmt,
    frame: &Frame,
    labels: &mut Labels,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    for statement in initializer {
        emit_statement(statement, frame, labels, out)?;
    }
    let start = labels.label();
    labels.place(start, out);
    let end = labels.label();
    if let Some(condition) = condition {
        emit_expression(condition, frame, out)?;
        labels.branch(Opcode::Brfalse, end, out);
    }
    let step = labels.label();
    labels.loops.push(LoopContext {
        continue_label: step,
        break_label: end,
    });
    emit_statement(body, frame, labels, out)?;
    labels.loops.pop();
    labels.place(step, out);
    for iterator in iterators {
        emit_statement_expression(iterator, frame, out)?;
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
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match &expr.kind {
        BoundExprKind::Assignment {
            operator: AssignmentOperator::Assign,
            target,
            value,
        } => {
            if let BoundExprKind::Local(name) = &target.kind {
                emit_expression(value, frame, out)?;
                return store_to(frame, name, out);
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
    emit_expression(expr, frame, out)?;
    out.push(Instruction::simple(Opcode::Pop));
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
