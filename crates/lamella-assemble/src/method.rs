//! Lowering a bound method body to a CIL instruction stream (ECMA-335 1st ed,
//! Partition III).

use crate::expr::{EmitError, emit_expression, emit_local, ldelem_opcode};
use crate::frame::{Frame, Slot};
use crate::tokens::Tokens;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use lamella_binder::{
    BoundCatch, BoundExpr, BoundExprKind, BoundStmt, BoundStmtKind, BoundSwitchLabel, SpecialType,
    TypeSymbol, always_exits,
};
use lamella_cil::{EhClause, EhKind, Instruction, InstructionRange, Opcode, Operand};
use lamella_syntax::ast::{AssignmentOperator, BinaryOperator, PostfixOperator, UnaryOperator};
use lamella_syntax::span::Span;
use lamella_token::Token;

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
    /// Local slots that must be `pinned` in the signature (a `fixed` array holder).
    pub pinned_slots: alloc::collections::BTreeSet<u16>,
}

/// The enclosing loop's (or switch's) branch targets, for `break` and `continue`.
/// `break` leaves the innermost loop or switch; `continue` targets the innermost
/// loop, skipping any switch in between.
struct LoopContext {
    continue_label: usize,
    break_label: usize,
    is_switch: bool,
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
    /// Source label names mapped to label ids, so a forward `goto` and its labeled
    /// statement share one id whichever is emitted first.
    named: BTreeMap<Box<str>, usize>,
    /// The enclosing `switch` statements (innermost last), so `goto case`/`goto default`
    /// can branch to a sibling section's label.
    switches: Vec<SwitchContext>,
}

/// A `switch` being emitted: each case value's section label (integral and string),
/// and the default's, so a `goto case`/`goto default` in any section can branch to it.
struct SwitchContext {
    cases: Vec<(i64, usize)>,
    string_cases: Vec<(Box<[u16]>, usize)>,
    default: Option<usize>,
}

impl Labels {
    fn label(&mut self) -> usize {
        self.positions.push(None);
        self.positions.len() - 1
    }

    /// The label id for a source label `name`, allocated on first reference.
    fn named_label(&mut self, name: &str) -> usize {
        if let Some(&id) = self.named.get(name) {
            return id;
        }
        let id = self.label();
        self.named.insert(name.into(), id);
        id
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
        | Opcode::LdcR4
        | Opcode::LdcR8
        | Opcode::Ldnull
        | Opcode::Ldarg
        | Opcode::Ldarga
        | Opcode::LdargaS
        | Opcode::Ldloc
        | Opcode::Ldloca
        | Opcode::LdlocaS
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
    let mut frame = Frame::build(parameters, &[], body, 0);
    Ok(lower(
        &mut frame,
        &Tokens::new(),
        body,
        &TypeSymbol::Special(SpecialType::Void),
        None,
    )?
    .0)
}

/// A constructor's prologue: `ldarg.0; <arguments>; call ctor`. Models both the implicit
/// parameterless base call (empty `arguments`) and an explicit `this(args)`/`base(args)`
/// chain.
pub struct ConstructorPrologue {
    /// The target `.ctor` token (a sibling, the base, or System.Object's).
    pub ctor: Token,
    /// The bound chain arguments, in order.
    pub arguments: Vec<BoundExpr>,
}

/// Lowers a method body and reports its local types (for the local signature) and
/// sequence points (for debug info). `tokens` resolves members; `arg_base` is 1 for
/// an instance method (argument 0 is `this`), else 0; `return_type` is the method's
/// return type, for the epilogue a `try` needs. `prologue` is the constructor chain
/// call, if any.
pub fn emit_body(
    parameters: &[Box<str>],
    byref_params: &[(Box<str>, TypeSymbol)],
    body: &BoundStmt,
    tokens: &Tokens,
    arg_base: u16,
    return_type: &TypeSymbol,
    prologue: Option<&ConstructorPrologue>,
) -> Result<EmittedBody, EmitError> {
    let mut frame = Frame::build(parameters, byref_params, body, arg_base);
    let lowered = lower(&mut frame, tokens, body, return_type, prologue)?;
    Ok(EmittedBody {
        code: lowered.0,
        local_types: frame.local_types(),
        local_names: frame.local_names(),
        sequence_points: lowered.1,
        handlers: lowered.2,
        pinned_slots: frame.pinned_slots(),
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
    prologue: Option<&ConstructorPrologue>,
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
    if let Some(prologue) = prologue {
        out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(0)));
        for argument in &prologue.arguments {
            emit_expression(argument, frame, tokens, &mut out)?;
        }
        out.push(Instruction::new(Opcode::Call, Operand::Token(prologue.ctor)));
    }
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
        Kind::Switch { sections, .. } => sections
            .iter()
            .any(|section| section.statements.iter().any(contains_try)),
        _ => false,
    }
}

fn emit_statement(
    stmt: &BoundStmt,
    frame: &mut Frame,
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
                    let value_type_new = matches!(
                        &initializer.kind,
                        BoundExprKind::ObjectCreation { arguments, .. } if arguments.is_empty()
                    ) && tokens.is_struct(&initializer.ty);
                    if value_type_new {
                        let token =
                            tokens
                                .type_token(&initializer.ty)
                                .ok_or(EmitError::Unsupported(
                                    "struct type has no token for initobj",
                                ))?;
                        match frame.slot(&declarator.name) {
                            Some(Slot::Local(slot)) => {
                                out.push(Instruction::new(Opcode::Ldloca, Operand::Variable(slot)));
                                out.push(Instruction::new(Opcode::Initobj, Operand::Token(token)));
                            }
                            _ => {
                                return Err(EmitError::Unsupported(
                                    "initobj target is not a local",
                                ));
                            }
                        }
                    } else {
                        emit_expression(initializer, frame, tokens, out)?;
                        store_to(frame, &declarator.name, out)?;
                    }
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
                is_switch: false,
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
                is_switch: false,
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
            let target = loop_target(labels, false, |context| context.break_label)?;
            labels.branch(Opcode::Br, target, out);
        }
        BoundStmtKind::Continue => {
            let target = loop_target(labels, true, |context| context.continue_label)?;
            labels.branch(Opcode::Br, target, out);
        }
        BoundStmtKind::Checked(inner) | BoundStmtKind::Unchecked(inner) => {
            emit_statement(inner, frame, tokens, labels, out)?;
        }
        BoundStmtKind::Fixed {
            name,
            element,
            init,
            body,
        } => {
            let array_slot = frame.reserve_pinned_local(&init.ty);
            emit_expression(init, frame, tokens, out)?;
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(array_slot)));
            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(array_slot)));
            out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(0)));
            let element_token = tokens
                .type_token(element)
                .ok_or(EmitError::Unsupported("fixed element type has no token"))?;
            out.push(Instruction::new(Opcode::Ldelema, Operand::Token(element_token)));
            store_to(frame, name, out)?;
            emit_statement(body, frame, tokens, labels, out)?;
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
        BoundStmtKind::Switch {
            expression,
            sections,
        } => {
            let temp = frame.reserve_local(&expression.ty);
            emit_expression(expression, frame, tokens, out)?;
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(temp)));

            let long = matches!(
                expression.ty,
                TypeSymbol::Special(SpecialType::Int64 | SpecialType::UInt64)
            );
            let section_labels: Vec<usize> = sections.iter().map(|_| labels.label()).collect();
            let end = labels.label();
            let mut default_label = None;

            for (index, section) in sections.iter().enumerate() {
                for label in &section.labels {
                    match label {
                        BoundSwitchLabel::Case(value) => {
                            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(temp)));
                            let constant = if long {
                                Instruction::new(Opcode::LdcI8, Operand::Int64(*value))
                            } else {
                                Instruction::new(Opcode::LdcI4, Operand::Int32(*value as i32))
                            };
                            out.push(constant);
                            out.push(Instruction::simple(Opcode::Ceq));
                            labels.branch(Opcode::Brtrue, section_labels[index], out);
                        }
                        BoundSwitchLabel::CaseString(text) => {
                            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(temp)));
                            let token = tokens.string(text).ok_or(EmitError::Unsupported(
                                "a switch case string was not minted",
                            ))?;
                            out.push(Instruction::new(Opcode::Ldstr, Operand::Token(token)));
                            crate::expr::emit_string_equality(false, tokens, out)?;
                            labels.branch(Opcode::Brtrue, section_labels[index], out);
                        }
                        BoundSwitchLabel::Default => default_label = Some(section_labels[index]),
                    }
                }
            }
            labels.branch(Opcode::Br, default_label.unwrap_or(end), out);

            let mut switch_cases: Vec<(i64, usize)> = Vec::new();
            let mut switch_string_cases: Vec<(Box<[u16]>, usize)> = Vec::new();
            for (index, section) in sections.iter().enumerate() {
                for label in &section.labels {
                    match label {
                        BoundSwitchLabel::Case(value) => {
                            switch_cases.push((*value, section_labels[index]));
                        }
                        BoundSwitchLabel::CaseString(text) => {
                            switch_string_cases.push((text.clone(), section_labels[index]));
                        }
                        BoundSwitchLabel::Default => {}
                    }
                }
            }
            labels.switches.push(SwitchContext {
                cases: switch_cases,
                string_cases: switch_string_cases,
                default: default_label,
            });
            labels.loops.push(LoopContext {
                continue_label: end,
                break_label: end,
                is_switch: true,
            });
            for (index, section) in sections.iter().enumerate() {
                labels.place(section_labels[index], out);
                for statement in &section.statements {
                    emit_statement(statement, frame, tokens, labels, out)?;
                }
            }
            labels.loops.pop();
            labels.switches.pop();
            labels.place(end, out);
        }
        BoundStmtKind::ForEach {
            name,
            collection,
            body,
            ..
        } => {
            let TypeSymbol::Array { element, .. } = &collection.ty else {
                return Err(EmitError::Unsupported(
                    "foreach over a non-array collection is not lowered yet",
                ));
            };
            let array = frame.reserve_local(&collection.ty);
            let index = frame.reserve_local(&TypeSymbol::Special(SpecialType::Int32));

            emit_expression(collection, frame, tokens, out)?;
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(array)));
            out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(0)));
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(index)));

            let test = labels.label();
            let step = labels.label();
            let end = labels.label();

            labels.place(test, out);
            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(index)));
            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(array)));
            out.push(Instruction::simple(Opcode::Ldlen));
            out.push(Instruction::simple(Opcode::ConvI4));
            out.push(Instruction::simple(Opcode::Clt));
            labels.branch(Opcode::Brfalse, end, out);

            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(array)));
            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(index)));
            if tokens.is_struct(element)
                || tokens.is_enum(element)
                || matches!(&**element, TypeSymbol::Special(SpecialType::Decimal))
            {
                let token = tokens.type_token(element).ok_or(EmitError::Unsupported(
                    "foreach element type has no token",
                ))?;
                out.push(Instruction::new(Opcode::Ldelema, Operand::Token(token)));
                out.push(Instruction::new(Opcode::Ldobj, Operand::Token(token)));
            } else {
                out.push(Instruction::simple(ldelem_opcode(element)?));
            }
            store_to(frame, name, out)?;

            labels.loops.push(LoopContext {
                continue_label: step,
                break_label: end,
                is_switch: false,
            });
            emit_statement(body, frame, tokens, labels, out)?;
            labels.loops.pop();

            labels.place(step, out);
            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(index)));
            out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(1)));
            out.push(Instruction::simple(Opcode::Add));
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(index)));
            labels.branch(Opcode::Br, test, out);
            labels.place(end, out);
        }
        BoundStmtKind::Labeled { label, body } => {
            let id = labels.named_label(label);
            labels.place(id, out);
            emit_statement(body, frame, tokens, labels, out)?;
        }
        BoundStmtKind::Goto(label) => {
            let id = labels.named_label(label);
            labels.branch(Opcode::Br, id, out);
        }
        BoundStmtKind::GotoCase(value) => {
            let target = labels.switches.last().and_then(|switch| {
                switch
                    .cases
                    .iter()
                    .find(|(case, _)| case == value)
                    .map(|(_, label)| *label)
            });
            match target {
                Some(label) => labels.branch(Opcode::Br, label, out),
                None => {
                    return Err(EmitError::Unsupported(
                        "goto case with no matching case in the enclosing switch",
                    ));
                }
            }
        }
        BoundStmtKind::GotoCaseString(text) => {
            let target = labels.switches.last().and_then(|switch| {
                switch
                    .string_cases
                    .iter()
                    .find(|(case, _)| case == text)
                    .map(|(_, label)| *label)
            });
            match target {
                Some(label) => labels.branch(Opcode::Br, label, out),
                None => {
                    return Err(EmitError::Unsupported(
                        "goto case with no matching string case in the enclosing switch",
                    ));
                }
            }
        }
        BoundStmtKind::GotoDefault => match labels.switches.last().and_then(|s| s.default) {
            Some(label) => labels.branch(Opcode::Br, label, out),
            None => {
                return Err(EmitError::Unsupported(
                    "goto default with no default section in the enclosing switch",
                ));
            }
        },
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
    frame: &mut Frame,
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
    frame: &mut Frame,
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
    frame: &mut Frame,
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
        is_switch: false,
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
    skip_switch: bool,
    select: impl Fn(&LoopContext) -> usize,
) -> Result<usize, EmitError> {
    labels
        .loops
        .iter()
        .rev()
        .find(|context| !(skip_switch && context.is_switch))
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
                if let Some((slot, element)) = frame.byref(name) {
                    out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(slot)));
                    emit_expression(value, frame, tokens, out)?;
                    crate::expr::emit_byref_store(element, tokens, out)?;
                    return Ok(());
                }
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
            if let BoundExprKind::Dereference { operand } = &target.kind {
                emit_expression(operand, frame, tokens, out)?;
                emit_expression(value, frame, tokens, out)?;
                out.push(Instruction::simple(crate::expr::stind_opcode(&target.ty)));
                return Ok(());
            }
            if let BoundExprKind::RefValue { reference, target: referent } = &target.kind {
                emit_expression(reference, frame, tokens, out)?;
                let token = tokens.type_token(referent).ok_or(EmitError::Unsupported(
                    "__refvalue type has no token",
                ))?;
                out.push(Instruction::new(Opcode::Refanyval, Operand::Token(token)));
                emit_expression(value, frame, tokens, out)?;
                crate::expr::emit_byref_store(referent, tokens, out)?;
                return Ok(());
            }
            if let BoundExprKind::PropertyAccess {
                receiver,
                declaring_type,
                name,
            } = &target.kind
            {
                return crate::expr::emit_property_store(
                    &target.ty,
                    receiver,
                    declaring_type,
                    name,
                    value,
                    frame,
                    tokens,
                    out,
                );
            }
        }
        BoundExprKind::Assignment {
            operator,
            target,
            value,
        } => {
            if let Some(binary) = compound_binary_operator(*operator) {
                return emit_compound(target, binary, Some(value), None, frame, tokens, out, Leave::Discard);
            }
        }
        BoundExprKind::Postfix { operator, operand } => {
            let increment = *operator == PostfixOperator::Increment;
            let user_step = user_step_method(operand, increment, tokens);
            return emit_compound(operand, step_operator(increment), None, user_step, frame, tokens, out, Leave::Discard);
        }
        BoundExprKind::Unary {
            operator: operator @ (UnaryOperator::PreIncrement | UnaryOperator::PreDecrement),
            operand,
        } => {
            let increment = *operator == UnaryOperator::PreIncrement;
            let user_step = user_step_method(operand, increment, tokens);
            return emit_compound(operand, step_operator(increment), None, user_step, frame, tokens, out, Leave::Discard);
        }
        _ => {}
    }
    emit_expression(expr, frame, tokens, out)?;
    if !matches!(expr.ty, TypeSymbol::Special(SpecialType::Void)) {
        out.push(Instruction::simple(Opcode::Pop));
    }
    Ok(())
}

/// The binary operator of `++` (Add) or `--` (Subtract).
pub(crate) fn step_operator(increment: bool) -> BinaryOperator {
    if increment {
        BinaryOperator::Add
    } else {
        BinaryOperator::Subtract
    }
}

/// The `op_Increment`/`op_Decrement` method token for a `++`/`--` on `operand`'s type, when
/// the type defines one (a user-defined stepper); `None` for a numeric `++`/`--`, which steps
/// by the implicit `1`. Shared by statement- and expression-position `++`/`--` emission, which
/// route through [`emit_compound`] (so any lvalue -- local, field, element, property -- works).
pub(crate) fn user_step_method(
    operand: &BoundExpr,
    increment: bool,
    tokens: &Tokens,
) -> Option<Token> {
    let name = if increment { "op_Increment" } else { "op_Decrement" };
    tokens.method(&operand.ty, name, core::slice::from_ref(&operand.ty))
}

/// Whether a read-modify-write leaves its value on the stack (an expression `++`/`--`) and,
/// if so, which: the value BEFORE the step (postfix) or AFTER it (prefix).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Leave {
    /// Statement position: store and leave nothing.
    Discard,
    /// Postfix `x++`/`x--`: leave the pre-step value.
    Old,
    /// Prefix `++x`/`--x`: leave the post-step value.
    New,
}

/// Emits a read-modify-write to `target` (an `op=` or, with `rhs` = `None`, a `++`/`--`):
/// read the target, apply the modification, and store it back. The modification is a user
/// `op_Increment`/`op_Decrement` call when `user_step` is `Some` (a `++`/`--` on a user
/// type), else combining the right-hand value via `binary`. The receiver/index is evaluated
/// once. `leave` keeps the expression value on the stack for a non-local `++`/`--` -- through
/// a temp local, since 1st-edition CIL has no `dup_x1` to reorder it past the store's
/// receiver/index operands. Lowers to 1st-edition CIL only.
pub(crate) fn emit_compound(
    target: &BoundExpr,
    binary: BinaryOperator,
    rhs: Option<&BoundExpr>,
    user_step: Option<Token>,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
    leave: Leave,
) -> Result<(), EmitError> {
    let kept = if leave == Leave::Discard {
        None
    } else {
        Some(frame.reserve_local(&target.ty))
    };
    match &target.kind {
        BoundExprKind::Local(name) => {
            if let Some((slot, element)) = frame.byref(name) {
                out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(slot)));
                emit_local(name, frame, tokens, out)?;
                emit_modify(user_step, binary, &target.ty, rhs, frame, tokens, out)?;
                crate::expr::emit_byref_store(element, tokens, out)?;
                return Ok(());
            }
            emit_local(name, frame, tokens, out)?;
            emit_modify(user_step, binary, &target.ty, rhs, frame, tokens, out)?;
            store_to(frame, name, out)
        }
        BoundExprKind::FieldAccess {
            receiver,
            field: Some(field),
            ..
        } => {
            let token = tokens
                .field(&field.declaring_type, &field.name)
                .ok_or(EmitError::Unsupported("field outside this module"))?;
            if field.is_static {
                out.push(Instruction::new(Opcode::Ldsfld, Operand::Token(token)));
                if leave == Leave::Old {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                emit_modify(user_step, binary, &target.ty, rhs, frame, tokens, out)?;
                if leave == Leave::New {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                out.push(Instruction::new(Opcode::Stsfld, Operand::Token(token)));
            } else {
                crate::expr::emit_field_receiver(field, receiver, frame, tokens, out)?;
                out.push(Instruction::simple(Opcode::Dup));
                out.push(Instruction::new(Opcode::Ldfld, Operand::Token(token)));
                if leave == Leave::Old {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                emit_modify(user_step, binary, &target.ty, rhs, frame, tokens, out)?;
                if leave == Leave::New {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                out.push(Instruction::new(Opcode::Stfld, Operand::Token(token)));
            }
            if let Some(slot) = kept {
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
            }
            Ok(())
        }
        BoundExprKind::PropertyAccess {
            receiver,
            declaring_type,
            name,
        } => {
            let is_static = matches!(receiver.kind, BoundExprKind::TypeReference(_));
            let value_type_receiver =
                !is_static && (tokens.is_struct(&receiver.ty) || tokens.is_enum(&receiver.ty));
            let getter = tokens
                .method(declaring_type, &crate::expr::accessor_name("get_", name), &[])
                .ok_or(EmitError::Unsupported("property getter outside this module"))?;
            let setter = tokens
                .method(
                    declaring_type,
                    &crate::expr::accessor_name("set_", name),
                    core::slice::from_ref(&target.ty),
                )
                .ok_or(EmitError::Unsupported("property setter outside this module"))?;
            let opcode = if is_static || value_type_receiver {
                Opcode::Call
            } else {
                Opcode::Callvirt
            };
            if !is_static {
                if value_type_receiver {
                    crate::expr::emit_value_type_receiver(receiver, frame, tokens, out)?;
                } else {
                    emit_expression(receiver, frame, tokens, out)?;
                }
                out.push(Instruction::simple(Opcode::Dup));
            }
            out.push(Instruction::new(opcode, Operand::Token(getter)));
            if leave == Leave::Old {
                out.push(Instruction::simple(Opcode::Dup));
                out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
            }
            emit_modify(user_step, binary, &target.ty, rhs, frame, tokens, out)?;
            if leave == Leave::New {
                out.push(Instruction::simple(Opcode::Dup));
                out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
            }
            out.push(Instruction::new(opcode, Operand::Token(setter)));
            if let Some(slot) = kept {
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
            }
            Ok(())
        }
        BoundExprKind::ElementAccess { receiver, indices } if indices.len() == 1 => {
            emit_expression(receiver, frame, tokens, out)?;
            let array = frame.reserve_local(&receiver.ty);
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(array)));
            emit_expression(&indices[0], frame, tokens, out)?;
            let index = frame.reserve_local(&TypeSymbol::Special(SpecialType::Int32));
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(index)));
            if tokens.is_struct(&target.ty) || tokens.is_enum(&target.ty) {
                let token = tokens
                    .type_token(&target.ty)
                    .ok_or(EmitError::Unsupported("array element type has no token"))?;
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(array)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(index)));
                out.push(Instruction::new(Opcode::Ldelema, Operand::Token(token)));
                out.push(Instruction::simple(Opcode::Dup));
                out.push(Instruction::new(Opcode::Ldobj, Operand::Token(token)));
                if leave == Leave::Old {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                emit_modify(user_step, binary, &target.ty, rhs, frame, tokens, out)?;
                if leave == Leave::New {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                out.push(Instruction::new(Opcode::Stobj, Operand::Token(token)));
            } else {
                let load = crate::expr::ldelem_opcode(&target.ty)?;
                let store = crate::expr::stelem_opcode(&target.ty)?;
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(array)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(index)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(array)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(index)));
                out.push(Instruction::simple(load));
                if leave == Leave::Old {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                emit_modify(user_step, binary, &target.ty, rhs, frame, tokens, out)?;
                if leave == Leave::New {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                out.push(Instruction::simple(store));
            }
            if let Some(slot) = kept {
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
            }
            Ok(())
        }
        _ => Err(EmitError::Unsupported("compound assignment to this target")),
    }
}

/// Applies the modification of a read-modify-write to the value already on the stack: a
/// user `op_Increment`/`op_Decrement` (`user_step`, for a `++`/`--` on a user type) is a
/// static call that consumes the value and pushes the stepped one; otherwise the numeric
/// `++`/`--` or `op=` combine pushes the right-hand value (or the implicit `1`) and applies
/// `binary`.
fn emit_modify(
    user_step: Option<Token>,
    binary: BinaryOperator,
    operand_ty: &TypeSymbol,
    rhs: Option<&BoundExpr>,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match user_step {
        Some(token) => {
            out.push(Instruction::new(Opcode::Call, Operand::Token(token)));
            Ok(())
        }
        None => emit_combine(binary, operand_ty, rhs, frame, tokens, out),
    }
}

/// Pushes the right-hand value (the `op=` value, or the implicit `1` of `++`/`--` in the
/// target's type) and applies `binary` -- string `+` is `String.Concat`, not `add`.
fn emit_combine(
    binary: BinaryOperator,
    operand_ty: &TypeSymbol,
    rhs: Option<&BoundExpr>,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match rhs {
        Some(value) => {
            emit_expression(value, frame, tokens, out)?;
            if value.ty != *operand_ty
                && matches!(
                    operand_ty,
                    TypeSymbol::Special(
                        SpecialType::Int64
                            | SpecialType::UInt64
                            | SpecialType::Single
                            | SpecialType::Double
                    )
                )
            {
                out.push(Instruction::simple(crate::expr::numeric_conversion(operand_ty)?));
            }
        }
        None => push_one(operand_ty, out),
    }
    if binary == BinaryOperator::Add
        && matches!(operand_ty, TypeSymbol::Special(SpecialType::String))
    {
        let value_is_string =
            rhs.is_some_and(|value| matches!(value.ty, TypeSymbol::Special(SpecialType::String)));
        let arg = TypeSymbol::Special(if value_is_string {
            SpecialType::String
        } else {
            SpecialType::Object
        });
        let string = TypeSymbol::Special(SpecialType::String);
        let token = tokens
            .method(&string, "Concat", &[arg.clone(), arg])
            .ok_or(EmitError::Unsupported("String.Concat was not minted"))?;
        out.push(Instruction::new(Opcode::Call, Operand::Token(token)));
        return Ok(());
    }
    crate::expr::emit_binary(binary, operand_ty, false, out)
}

/// Pushes the constant `1` in `ty` (the step of `++`/`--`): `ldc.i4.1`, widened for a
/// 64-bit target.
fn push_one(ty: &TypeSymbol, out: &mut Vec<Instruction>) {
    out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(1)));
    if matches!(
        ty,
        TypeSymbol::Special(SpecialType::Int64 | SpecialType::UInt64)
    ) {
        out.push(Instruction::simple(Opcode::ConvI8));
    }
}

/// The binary operator a compound assignment (`op=`) applies, or `None` for simple
/// `=` (which the dedicated branch handles).
fn compound_binary_operator(operator: AssignmentOperator) -> Option<BinaryOperator> {
    use AssignmentOperator as A;
    use BinaryOperator as B;
    Some(match operator {
        A::Assign => return None,
        A::Add => B::Add,
        A::Subtract => B::Subtract,
        A::Multiply => B::Multiply,
        A::Divide => B::Divide,
        A::Modulo => B::Modulo,
        A::And => B::BitwiseAnd,
        A::Or => B::BitwiseOr,
        A::Xor => B::BitwiseXor,
        A::LeftShift => B::LeftShift,
        A::RightShift => B::RightShift,
    })
}

pub(crate) fn store_to(
    frame: &Frame,
    name: &str,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
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
        let emitted =
            emit_body(&[], &[], &bound, &Tokens::new(), 0, &int(), None).expect("should lower");

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
