//! Lowering a bound method body to a CIL instruction stream (ECMA-335 1st ed,
//! Partition III).

use crate::expr::{EmitError, emit_expression, emit_local, emit_ref_argument};
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
/// material the debug-info writer turns into a source-line mapping. `None` for the
/// span marks a HIDDEN point: synthesized IL with no source, which a debugger steps
/// over (a `using`/`lock` disposal, other compiler-generated code).
pub type SequencePoint = (u32, Option<Span>);

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
    /// The protected-region depth where this loop was entered, so a `break` or `continue` from
    /// deeper inside knows it is LEAVING one. See [`Labels::statement_branch`].
    region_depth: usize,
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
struct Labels<'a> {
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
    /// How many protected regions -- `try` bodies and their `catch`/`finally` handlers -- enclose
    /// the instruction being emitted. [`emit_try`] is the only thing that opens one.
    region_depth: usize,
    /// True while emitting a synthesized `finally` (a using/lock disposal): its
    /// statements become hidden sequence points a debugger steps over.
    hidden_region: bool,
    /// The UTF-8 source bytes in a debug build, else `None` for a release build. A
    /// block's `{`/`}` step points emit only when its span starts at a real `{` here,
    /// which tells a user-written block apart from a compiler desugaring (a `using`/
    /// `lock`/`foreach` wrapper block carries the whole statement's span, so it starts
    /// at the keyword, not a brace).
    source: Option<&'a [u8]>,
}

/// A `switch` being emitted: each case value's section label (integral and string),
/// and the default's, so a `goto case`/`goto default` in any section can branch to it.
struct SwitchContext {
    cases: Vec<(i64, usize)>,
    string_cases: Vec<(Box<[u16]>, usize)>,
    default: Option<usize>,
    /// The protected-region depth where the switch was entered, for the same reason
    /// [`LoopContext`] carries one: a `goto case` from inside a `try` leaves it.
    region_depth: usize,
}

impl Labels<'_> {
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

    /// An unconditional branch at a STATEMENT boundary to `label`, whose target sits
    /// `target_depth` protected regions deep -- picking the opcode the CLI requires.
    ///
    /// `br` may not cross the boundary of a `try` block or of a handler; `leave` may, and it runs
    /// any intervening `finally` on the way out (ECMA-335 III.1.7.5). A `leave` where a `br` would
    /// also do is harmless -- it empties an evaluation stack that a statement boundary has already
    /// left empty -- so the test only has to be conservative in one direction.
    ///
    /// **ONE FUNCTION FOR ALL SEVEN STATEMENT BRANCHES, BECAUSE THE RULE IS THE SAME AT EVERY
    /// ONE.** `return`, `goto`, `break`, `continue`, `goto case`, `goto case <string>` and
    /// `goto default` all leave a protected region the same way, and any of them emitting a bare
    /// `br` is rejected by ILVerify as `BranchOutOfTry` (or `BranchOutOfHandler`), with the runtime
    /// refusing the image outright as `InvalidProgramException`.
    ///
    /// **THE DEPTHS ARE COMPARED RATHER THAN TESTED AGAINST ZERO**, and the difference is two
    /// programs: a `break` whose loop is ITSELF inside a `try` leaves nothing and wants `br`, and
    /// so does one inside a `finally` whose loop is also inside it. Asking only *"does this method
    /// contain a try"* answers both of those the same way as the case that must be `leave`.
    fn statement_branch(&mut self, label: usize, target_depth: usize, out: &mut Vec<Instruction>) {
        let opcode = if self.region_depth > target_depth {
            Opcode::Leave
        } else {
            Opcode::Br
        };
        self.branch(opcode, label, out);
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
/// from each instruction's net stack effect; this emitter keeps the stack balanced
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
        | Opcode::Ldsflda
        | Opcode::Ldstr
        | Opcode::Ldtoken
        | Opcode::Sizeof
        | Opcode::Dup
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
    let mut frame = Frame::build(parameters, &[], &[], body, 0);
    Ok(lower(
        &mut frame,
        &Tokens::new(),
        body,
        &TypeSymbol::Special(SpecialType::Void),
        None,
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
    /// The `: base(...)` / `: this(...)` initializer's span, for a debug build's
    /// sequence point on it (a breakpoint on the chain call). `None` for an implicit or
    /// synthesized base call, which carries no source.
    pub span: Option<Span>,
    /// The bound chain arguments, in order.
    pub arguments: Vec<BoundExpr>,
    /// How many leading statements of the body -- the instance field initializers
    /// (17.11) -- to emit BEFORE this chain call, so a virtual method the base
    /// constructor invokes observes the derived fields already assigned. Zero when the
    /// constructor chains to `this(...)`, since the initializers run in the constructor
    /// ultimately invoked, not here.
    pub leading_body: usize,
    /// `: this()` ON A STRUCT, which chains to NO constructor: holds the struct's own type token
    /// and the prologue becomes `ldarg.0; initobj S` instead of a call. `None` for every ordinary
    /// chain, which is every class and every `: this(args)`.
    ///
    /// **A VALUE TYPE HAS NO IL DEFAULT CONSTRUCTOR TO CALL.** The initializer means "every field
    /// starts at its zero" (17.4.4), and csc lowers it exactly this way -- measured. Calling
    /// `object::.ctor()` on the `this` byref instead produces IL that RUNS and that `ilverify`
    /// refuses, which is why this is a field rather than a fallback.
    pub zero_initialize: Option<Token>,
}

/// Lowers a method body and reports its local types (for the local signature) and
/// sequence points (for debug info). `tokens` resolves members; `arg_base` is 1 for
/// an instance method (argument 0 is `this`), else 0; `return_type` is the method's
/// return type, for the epilogue a `try` needs. `prologue` is the constructor chain
/// call, if any. `source` is the UTF-8 source bytes in a debug build (else `None`),
/// which drives the `{`/`}` brace step points a debugger stops on for every real user
/// block; a release build passes `None` and stays lean.
pub fn emit_body(
    parameters: &[Box<str>],
    byref_params: &[(Box<str>, TypeSymbol)],
    type_parameters: &[Box<str>],
    body: &BoundStmt,
    tokens: &Tokens,
    arg_base: u16,
    return_type: &TypeSymbol,
    prologue: Option<&ConstructorPrologue>,
    source: Option<&[u8]>,
) -> Result<EmittedBody, EmitError> {
    let mut frame = Frame::build(parameters, byref_params, type_parameters, body, arg_base);
    let lowered = lower(&mut frame, tokens, body, return_type, prologue, source)?;
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
    source: Option<&[u8]>,
) -> Result<Lowered, EmitError> {
    let mut labels = Labels {
        source,
        ..Labels::default()
    };
    let method_braces = source.is_some_and(|s| s.get(body.span.start as usize) == Some(&b'{'));
    let reaches_epilogue =
        method_braces && (completes_normally(body) || contains_return(body));
    if contains_try(body) || method_braces {
        let return_slot = if matches!(return_type, TypeSymbol::Special(SpecialType::Void)) {
            None
        } else {
            Some(frame.reserve_local(return_type))
        };
        let label = labels.label();
        labels.epilogue = Some(Epilogue { label, return_slot });
    }

    let mut out = Vec::new();
    let push_open_brace = |labels: &mut Labels<'_>, out: &mut Vec<Instruction>| {
        if method_braces {
            labels.points.push((out.len() as u32, Some(open_brace_span(body))));
            out.push(Instruction::simple(Opcode::Nop));
        }
    };
    match prologue {
        Some(prologue) if prologue.leading_body > 0 => {
            if let BoundStmtKind::Block(statements) = &body.kind {
                let split = prologue.leading_body.min(statements.len());
                for statement in &statements[..split] {
                    emit_statement(statement, frame, tokens, &mut labels, &mut out)?;
                }
                emit_prologue(prologue, frame, tokens, &mut labels, &mut out)?;
                push_open_brace(&mut labels, &mut out);
                for statement in &statements[split..] {
                    emit_statement(statement, frame, tokens, &mut labels, &mut out)?;
                }
            } else {
                emit_prologue(prologue, frame, tokens, &mut labels, &mut out)?;
                push_open_brace(&mut labels, &mut out);
                emit_statement(body, frame, tokens, &mut labels, &mut out)?;
            }
        }
        Some(prologue) => {
            emit_prologue(prologue, frame, tokens, &mut labels, &mut out)?;
            push_open_brace(&mut labels, &mut out);
            emit_body_statements(body, frame, tokens, &mut labels, &mut out)?;
        }
        None => {
            push_open_brace(&mut labels, &mut out);
            emit_body_statements(body, frame, tokens, &mut labels, &mut out)?;
        }
    }

    if let Some(Epilogue { label, return_slot }) = labels.epilogue {
        labels.place(label, &out);
        if reaches_epilogue {
            labels.points.push((out.len() as u32, Some(close_brace_span(body))));
            out.push(Instruction::simple(Opcode::Nop));
        }
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

/// Emits a method body's statements directly, WITHOUT the block-brace points the
/// [`emit_statement`] `Block` arm adds: `lower` emits the method's own `{`/`}` around
/// the prologue and epilogue, so the top-level body block must not be braced a second
/// time. Its children emit one by one (a lone braceless statement emits as itself), so
/// only NESTED blocks reach the brace-emitting arm.
fn emit_body_statements(
    body: &BoundStmt,
    frame: &mut Frame,
    tokens: &Tokens,
    labels: &mut Labels<'_>,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if let BoundStmtKind::Block(statements) = &body.kind {
        for statement in statements {
            emit_statement(statement, frame, tokens, labels, out)?;
        }
    } else {
        emit_statement(body, frame, tokens, labels, out)?;
    }
    Ok(())
}

/// Emits a constructor's chain call `ldarg.0; <arguments>; call ctor` -- the invocation of
/// the base or sibling constructor a prologue records (17.11).
fn emit_prologue(
    prologue: &ConstructorPrologue,
    frame: &Frame,
    tokens: &Tokens,
    labels: &mut Labels<'_>,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if let Some(span) = prologue.span {
        labels.points.push((out.len() as u32, Some(span)));
    }
    out.push(Instruction::new(Opcode::Ldarg, Operand::Variable(0)));
    if let Some(ty) = prologue.zero_initialize {
        out.push(Instruction::new(Opcode::Initobj, Operand::Token(ty)));
        return Ok(());
    }
    for argument in &prologue.arguments {
        crate::expr::emit_argument(argument, frame, tokens, out)?;
    }
    out.push(Instruction::new(Opcode::Call, Operand::Token(prologue.ctor)));
    Ok(())
}

/// The single-character span of a block's opening brace `{` -- a block span starts
/// there (`parse_block`) -- for a debug build's brace sequence point.
fn open_brace_span(block: &BoundStmt) -> Span {
    Span::new(block.span.start, block.span.start + 1)
}

/// The single-character span of a block's closing brace `}` -- a block span ends just
/// past it -- for a debug build's brace sequence point.
fn close_brace_span(block: &BoundStmt) -> Span {
    Span::new(block.span.end.saturating_sub(1), block.span.end)
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

/// Whether `stmt` contains a `return` anywhere. A method can reach its epilogue ret (and
/// so its closing brace) by routing a `return` there even when its body never falls
/// through, so a method that returns somewhere is braced while one that only throws is not.
fn contains_return(stmt: &BoundStmt) -> bool {
    use BoundStmtKind as Kind;
    match &stmt.kind {
        Kind::Return(_) => true,
        Kind::Block(statements) => statements.iter().any(contains_return),
        Kind::If {
            then_branch,
            else_branch,
            ..
        } => contains_return(then_branch) || else_branch.as_deref().is_some_and(contains_return),
        Kind::While { body, .. }
        | Kind::DoWhile { body, .. }
        | Kind::For { body, .. }
        | Kind::ForEach { body, .. }
        | Kind::Lock { body, .. }
        | Kind::Using { body, .. }
        | Kind::Fixed { body, .. }
        | Kind::Labeled { body, .. } => contains_return(body),
        Kind::Checked(inner) | Kind::Unchecked(inner) => contains_return(inner),
        Kind::Try {
            body,
            catches,
            finally,
        } => {
            contains_return(body)
                || catches.iter().any(|catch| contains_return(&catch.body))
                || finally.as_deref().is_some_and(contains_return)
        }
        Kind::Switch { sections, .. } => sections
            .iter()
            .any(|section| section.statements.iter().any(contains_return)),
        _ => false,
    }
}

/// Whether control can fall through the end of `stmt` -- C#'s reachable-end-point
/// analysis (ECMA-334 8.1). A block's closing-brace step point is emitted only when its
/// end is reachable this way: a block that ends by transferring control (a `break`,
/// `continue`, `return`, `throw`, `goto`, an exhaustive `if`, or a `try`/`switch` whose
/// every path diverts) has an unreachable `}`, which csc -- and so lcsc -- leaves
/// unbraced. A loop is taken to complete, since its usual shape has a reachable exit (a
/// never-completing `while (true)` without a `break` is a rare case this does not model,
/// and at worst brackets a `}` csc omits -- it never drops one csc keeps).
fn completes_normally(stmt: &BoundStmt) -> bool {
    use BoundStmtKind as Kind;
    match &stmt.kind {
        Kind::Return(_)
        | Kind::Throw(_)
        | Kind::Break
        | Kind::Continue
        | Kind::Goto(_)
        | Kind::GotoCase(_)
        | Kind::GotoCaseString(_)
        | Kind::GotoDefault => false,
        Kind::Block(statements) => statements.iter().all(completes_normally),
        Kind::If {
            then_branch,
            else_branch,
            ..
        } => else_branch.as_deref().is_none_or(|else_branch| {
            completes_normally(then_branch) || completes_normally(else_branch)
        }),
        Kind::Checked(inner) | Kind::Unchecked(inner) => completes_normally(inner),
        Kind::Labeled { body, .. }
        | Kind::Lock { body, .. }
        | Kind::Using { body, .. }
        | Kind::Fixed { body, .. } => completes_normally(body),
        Kind::Try {
            body,
            catches,
            finally,
        } => {
            finally.as_deref().is_none_or(completes_normally)
                && (completes_normally(body)
                    || catches.iter().any(|catch| completes_normally(&catch.body)))
        }
        Kind::Switch { .. } => !always_exits(stmt),
        _ => true,
    }
}

fn emit_statement(
    stmt: &BoundStmt,
    frame: &mut Frame,
    tokens: &Tokens,
    labels: &mut Labels<'_>,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if !matches!(stmt.kind, BoundStmtKind::Block(_) | BoundStmtKind::Empty) {
        let point = if labels.hidden_region || stmt.span.is_hidden() {
            None
        } else {
            Some(stmt.span)
        };
        labels.points.push((out.len() as u32, point));
    }
    match &stmt.kind {
        BoundStmtKind::Empty => {}
        BoundStmtKind::Block(statements) => {
            let braced = labels
                .source
                .is_some_and(|s| s.get(stmt.span.start as usize) == Some(&b'{'));
            if braced {
                let point = (!labels.hidden_region).then(|| open_brace_span(stmt));
                labels.points.push((out.len() as u32, point));
                out.push(Instruction::simple(Opcode::Nop));
            }
            for statement in statements {
                emit_statement(statement, frame, tokens, labels, out)?;
            }
            if braced && statements.iter().all(completes_normally) {
                let point = (!labels.hidden_region).then(|| close_brace_span(stmt));
                labels.points.push((out.len() as u32, point));
                out.push(Instruction::simple(Opcode::Nop));
            }
        }
        BoundStmtKind::Local { declarators, .. } => {
            for declarator in declarators {
                frame.rebind_decl(stmt.span, &declarator.name);
                if let Some(initializer) = &declarator.initializer {
                    let value_type_new = matches!(
                        &initializer.kind,
                        BoundExprKind::ObjectCreation { arguments, .. } if arguments.is_empty()
                    ) && tokens.is_struct(&initializer.ty);
                    if value_type_new {
                        let token =
                            tokens
                                .instruction_type_token(&initializer.ty)
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
                    } else if let BoundExprKind::Ref { operand, .. } = &initializer.kind {
                        crate::expr::emit_ref_argument(operand, frame, tokens, out)?;
                        store_to(frame, &declarator.name, out)?;
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
                if matches!(value.kind, BoundExprKind::Ref { .. }) {
                    let BoundExprKind::Ref { operand, .. } = &value.kind else {
                        unreachable!("just matched")
                    };
                    emit_ref_argument(operand, frame, tokens, out)?;
                } else {
                    emit_expression(value, frame, tokens, out)?;
                }
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
                region_depth: labels.region_depth,
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
                region_depth: labels.region_depth,
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
            let (target, depth) = loop_target(labels, false, |context| context.break_label)?;
            labels.statement_branch(target, depth, out);
        }
        BoundStmtKind::Continue => {
            let (target, depth) = loop_target(labels, true, |context| context.continue_label)?;
            labels.statement_branch(target, depth, out);
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
            frame.rebind_decl(stmt.span, name);
            if matches!(&init.ty, TypeSymbol::Pointer(_)) {
                let slot =
                    frame.reserve_pinned_local(&TypeSymbol::ByRef(Box::new(element.clone())));
                emit_expression(init, frame, tokens, out)?;
                out.push(Instruction::new(Opcode::Stloc, Operand::Variable(slot)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
                out.push(Instruction::simple(Opcode::ConvU));
                store_to(frame, name, out)?;
            } else if matches!(&init.ty, TypeSymbol::Special(SpecialType::String)) {
                let slot = frame.reserve_pinned_local(&init.ty);
                emit_expression(init, frame, tokens, out)?;
                out.push(Instruction::new(Opcode::Stloc, Operand::Variable(slot)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
                out.push(Instruction::simple(Opcode::ConvI));
                out.push(Instruction::simple(Opcode::Dup));
                let to_nonnull = out.len();
                out.push(Instruction::new(Opcode::Brtrue, Operand::Target(0)));
                let to_done = out.len();
                out.push(Instruction::new(Opcode::Br, Operand::Target(0)));
                out[to_nonnull].operand = Operand::Target(out.len() as u32);
                let reference = crate::compile::offset_to_string_data_reference();
                let offset_token = tokens
                    .method(&reference.declaring_type, &reference.name, &reference.parameters)
                    .ok_or(EmitError::Unsupported(
                        "RuntimeHelpers.OffsetToStringData was not minted",
                    ))?;
                out.push(Instruction::new(Opcode::Call, Operand::Token(offset_token)));
                out.push(Instruction::simple(Opcode::Add));
                out[to_done].operand = Operand::Target(out.len() as u32);
                store_to(frame, name, out)?;
            } else {
                let array_slot = frame.reserve_pinned_local(&init.ty);
                emit_expression(init, frame, tokens, out)?;
                out.push(Instruction::new(Opcode::Stloc, Operand::Variable(array_slot)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(array_slot)));
                let rank = match &init.ty {
                    TypeSymbol::Array { rank, .. } => *rank,
                    _ => 1,
                };
                if rank <= 1 {
                    out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(0)));
                    let element_token = tokens
                        .instruction_type_token(element)
                        .ok_or(EmitError::Unsupported("fixed element type has no token"))?;
                    out.push(Instruction::new(Opcode::Ldelema, Operand::Token(element_token)));
                } else {
                    for _ in 0..rank {
                        out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(0)));
                    }
                    let token = tokens
                        .method(&init.ty, "Address", &crate::expr::array_int_params(rank as usize))
                        .ok_or(EmitError::Unsupported("fixed rectangular-array Address method"))?;
                    out.push(Instruction::new(Opcode::Call, Operand::Token(token)));
                }
                store_to(frame, name, out)?;
            }
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
                        BoundSwitchLabel::CaseNull => {
                            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(temp)));
                            labels.branch(Opcode::Brfalse, section_labels[index], out);
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
                        BoundSwitchLabel::CaseNull => {}
                        BoundSwitchLabel::Default => {}
                    }
                }
            }
            labels.switches.push(SwitchContext {
                cases: switch_cases,
                string_cases: switch_string_cases,
                default: default_label,
                region_depth: labels.region_depth,
            });
            labels.loops.push(LoopContext {
                continue_label: end,
                break_label: end,
                is_switch: true,
                region_depth: labels.region_depth,
            });
            let reachability = lamella_binder::switch_section_reachability(expression, sections);
            for (index, section) in sections.iter().enumerate() {
                labels.place(section_labels[index], out);
                let unreachable = reachability
                    .as_ref()
                    .is_some_and(|reachable| !reachable[index]);
                let saved_hidden = labels.hidden_region;
                labels.hidden_region |= unreachable;
                for statement in &section.statements {
                    emit_statement(statement, frame, tokens, labels, out)?;
                }
                labels.hidden_region = saved_hidden;
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
            let TypeSymbol::Array { element, rank } = &collection.ty else {
                return Err(EmitError::Unsupported(
                    "foreach over a non-array collection is not lowered yet",
                ));
            };
            if *rank != 1 {
                return Err(EmitError::Unsupported(
                    "foreach over a rectangular array must be desugared before emission",
                ));
            }
            let array = frame.reserve_local(&collection.ty);
            let index = frame.reserve_local(&TypeSymbol::Special(SpecialType::Int32));
            frame.rebind_decl(stmt.span, name);

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
                let token = tokens.instruction_type_token(element).ok_or(EmitError::Unsupported(
                    "foreach element type has no token",
                ))?;
                out.push(Instruction::new(Opcode::Ldelema, Operand::Token(token)));
                out.push(Instruction::new(Opcode::Ldobj, Operand::Token(token)));
            } else {
                out.push(crate::expr::ldelem_instruction(element, tokens)?);
            }
            store_to(frame, name, out)?;

            labels.loops.push(LoopContext {
                continue_label: step,
                break_label: end,
                is_switch: false,
                region_depth: labels.region_depth,
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
            out.push(Instruction::simple(Opcode::Nop));
            emit_statement(body, frame, tokens, labels, out)?;
        }
        BoundStmtKind::Goto(label) => {
            let id = labels.named_label(label);
            labels.statement_branch(id, 0, out);
        }
        BoundStmtKind::GotoCase(value) => {
            let target = labels.switches.last().and_then(|switch| {
                switch
                    .cases
                    .iter()
                    .find(|(case, _)| case == value)
                    .map(|(_, label)| (*label, switch.region_depth))
            });
            match target {
                Some((label, depth)) => labels.statement_branch(label, depth, out),
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
                    .map(|(_, label)| (*label, switch.region_depth))
            });
            match target {
                Some((label, depth)) => labels.statement_branch(label, depth, out),
                None => {
                    return Err(EmitError::Unsupported(
                        "goto case with no matching string case in the enclosing switch",
                    ));
                }
            }
        }
        BoundStmtKind::GotoDefault => match labels
            .switches
            .last()
            .and_then(|switch| switch.default.map(|label| (label, switch.region_depth)))
        {
            Some((label, depth)) => labels.statement_branch(label, depth, out),
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
    labels: &mut Labels<'_>,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    let end = labels.label();

    let try_start = out.len() as u32;
    labels.region_depth += 1;
    let emitted = emit_statement(body, frame, tokens, labels, out);
    labels.region_depth -= 1;
    emitted?;
    if !always_exits(body) {
        labels.branch(Opcode::Leave, end, out);
    }
    let try_range = InstructionRange {
        start: try_start,
        end: out.len() as u32,
    };

    for catch in catches {
        let filter_start = catch.filter.as_ref().map(|condition| {
            let start = out.len() as u32;
            let done = labels.label();
            if let Some(ty) = &catch.exception_type {
                let bind = labels.label();
                let token = tokens
                    .instruction_type_token(ty)
                    .ok_or(EmitError::Unsupported("a catch filter's type has no token"));
                let token = match token {
                    Ok(token) => token,
                    Err(error) => return Err(error),
                };
                out.push(Instruction::new(Opcode::Isinst, Operand::Token(token)));
                out.push(Instruction::simple(Opcode::Dup));
                labels.branch(Opcode::Brtrue, bind, out);
                out.push(Instruction::simple(Opcode::Pop));
                out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(0)));
                labels.branch(Opcode::Br, done, out);
                labels.place(bind, out);
            }
            if let Some(name) = catch.name.as_deref() {
                frame.rebind_decl(catch.span, name);
            }
            match catch.name.as_deref().and_then(|name| frame.slot(name)) {
                Some(Slot::Local(slot)) => {
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(slot)));
                }
                _ => out.push(Instruction::simple(Opcode::Pop)),
            }
            emit_expression(condition, frame, tokens, out)?;
            out.push(Instruction::new(Opcode::LdcI4, Operand::Int32(0)));
            out.push(Instruction::simple(Opcode::CgtUn));
            labels.place(done, out);
            out.push(Instruction::simple(Opcode::Endfilter));
            Ok(start)
        });
        let filter_start = match filter_start {
            Some(Ok(start)) => Some(start),
            Some(Err(error)) => return Err(error),
            None => None,
        };
        let handler_start = out.len() as u32;
        labels.points.push((handler_start, Some(catch.span)));
        if filter_start.is_none() {
            if let Some(name) = catch.name.as_deref() {
                frame.rebind_decl(catch.span, name);
            }
            match catch.name.as_deref().and_then(|name| frame.slot(name)) {
                Some(Slot::Local(slot)) => {
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(slot)));
                }
                _ => out.push(Instruction::simple(Opcode::Pop)),
            }
        } else {
            out.push(Instruction::simple(Opcode::Pop));
        }
        labels.region_depth += 1;
        let emitted = emit_statement(&catch.body, frame, tokens, labels, out);
        labels.region_depth -= 1;
        emitted?;
        if !always_exits(&catch.body) {
            labels.branch(Opcode::Leave, end, out);
        }
        let kind = match filter_start {
            Some(filter_start) => EhKind::Filter { filter_start },
            None => {
                let ty = catch
                    .exception_type
                    .clone()
                    .unwrap_or(TypeSymbol::Special(SpecialType::Object));
                let token = tokens
                    .instruction_type_token(&ty)
                    .ok_or(EmitError::Unsupported("a catch clause's type has no token"))?;
                EhKind::Catch(token)
            }
        };
        labels.handlers.push(EhClause {
            try_range,
            handler_range: InstructionRange {
                start: handler_start,
                end: out.len() as u32,
            },
            kind,
        });
    }

    if let Some(finally) = finally {
        let handler_start = out.len() as u32;
        let saved_hidden = labels.hidden_region;
        labels.hidden_region |= finally.span.is_hidden();
        labels.region_depth += 1;
        let emitted = emit_statement(finally, frame, tokens, labels, out);
        labels.region_depth -= 1;
        labels.hidden_region = saved_hidden;
        emitted?;
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
    labels: &mut Labels<'_>,
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
            if !always_exits(then_branch) {
                labels.branch(Opcode::Br, end, out);
            }
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
    labels: &mut Labels<'_>,
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
        region_depth: labels.region_depth,
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
) -> Result<(usize, usize), EmitError> {
    labels
        .loops
        .iter()
        .rev()
        .find(|context| !(skip_switch && context.is_switch))
        .map(|context| (select(context), context.region_depth))
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
            ..
        } => {
            if let BoundExprKind::Local(name) = &target.kind {
                if let BoundExprKind::Ref { operand, .. } = &value.kind {
                    crate::expr::emit_ref_argument(operand, frame, tokens, out)?;
                    return store_to(frame, name, out);
                }
                if let Some((slot, element)) = frame.byref(name) {
                    out.push(slot.load());
                    emit_expression(value, frame, tokens, out)?;
                    crate::expr::emit_byref_store(&element, tokens, out)?;
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
                    false,
                    frame,
                    tokens,
                    out,
                );
            }
            if let BoundExprKind::ElementAccess { receiver, indices } = &target.kind {
                return crate::expr::emit_element_store(
                    &target.ty, receiver, indices, value, false, frame, tokens, out,
                );
            }
            if let BoundExprKind::IndexerAccess {
                receiver,
                indices,
                setter,
            } = &target.kind
            {
                return crate::expr::emit_indexer_store(
                    receiver, indices, setter, value, false, frame, tokens, out,
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
                let token = tokens.instruction_type_token(referent).ok_or(EmitError::Unsupported(
                    "__refvalue type has no token",
                ))?;
                out.push(Instruction::new(Opcode::Refanyval, Operand::Token(token)));
                emit_expression(value, frame, tokens, out)?;
                crate::expr::emit_byref_store(referent, tokens, out)?;
                return Ok(());
            }
            if let BoundExprKind::PropertyAccess {
                receiver,
                setter_declaring_type,
                name,
                ..
            } = &target.kind
            {
                return crate::expr::emit_property_store(
                    &target.ty,
                    receiver,
                    setter_declaring_type,
                    name,
                    value,
                    false,
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
            checked,
        } => {
            if let Some(binary) = compound_binary_operator(*operator) {
                return emit_compound(target, binary, Some(value), None, None, *checked, frame, tokens, out, Leave::Discard);
            }
        }
        BoundExprKind::Postfix { operator, operand, step } => {
            let increment = *operator == PostfixOperator::Increment;
            let (user_step, result_conversion) = step_tokens(step.as_deref(), operand, increment, tokens);
            return emit_compound(operand, step_operator(increment), None, user_step, result_conversion, false, frame, tokens, out, Leave::Discard);
        }
        BoundExprKind::Unary {
            operator: operator @ (UnaryOperator::PreIncrement | UnaryOperator::PreDecrement),
            operand,
        } => {
            let increment = *operator == UnaryOperator::PreIncrement;
            let user_step = user_step_method(operand, increment, tokens);
            return emit_compound(operand, step_operator(increment), None, user_step, None, false, frame, tokens, out, Leave::Discard);
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

/// The (operator, result-conversion) tokens for a `++`/`--`. For a converting user operator (14.14.2)
/// -- the [`ConvertingStep`] the binder resolved -- the operator's own token and its result
/// conversion; otherwise the type's exact same-type op_Increment/op_Decrement, if any, and no
/// conversion.
pub(crate) fn step_tokens(
    step: Option<&lamella_binder::ConvertingStep>,
    operand: &BoundExpr,
    increment: bool,
    tokens: &Tokens,
) -> (Option<Token>, Option<Token>) {
    match step {
        Some(step) => {
            let operator = tokens.method(
                &step.operator.declaring_type,
                &step.operator.name,
                &step.operator.parameters,
            );
            let conversion = step
                .result_conversion
                .as_ref()
                .and_then(|conversion| conversion_token(conversion, tokens));
            (operator, conversion)
        }
        None => (user_step_method(operand, increment, tokens), None),
    }
}

/// Resolves a conversion operator reference (`op_Implicit`/`op_Explicit`, keyed by return type to
/// disambiguate return-overloaded operators) to its token, falling back to its plain name.
fn conversion_token(method: &lamella_binder::MethodReference, tokens: &Tokens) -> Option<Token> {
    tokens
        .method(
            &method.declaring_type,
            &crate::tokens::conversion_key_name(&method.name, &method.return_type),
            &method.parameters,
        )
        .or_else(|| tokens.method(&method.declaring_type, &method.name, &method.parameters))
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
    result_conversion: Option<Token>,
    checked: bool,
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
                out.push(slot.load());
                emit_local(name, frame, tokens, out)?;
                emit_modify(user_step, result_conversion, binary, &target.ty, rhs, checked, frame, tokens, out)?;
                crate::expr::emit_byref_store(&element, tokens, out)?;
                return Ok(());
            }
            emit_local(name, frame, tokens, out)?;
            emit_modify(user_step, result_conversion, binary, &target.ty, rhs, checked, frame, tokens, out)?;
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
                emit_modify(user_step, result_conversion, binary, &target.ty, rhs, checked, frame, tokens, out)?;
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
                emit_modify(user_step, result_conversion, binary, &target.ty, rhs, checked, frame, tokens, out)?;
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
            setter_declaring_type,
            name,
            ..
        } => {
            let is_static = matches!(receiver.kind, BoundExprKind::TypeReference(_));
            let value_type_receiver =
                !is_static && (tokens.is_struct(&receiver.ty) || tokens.is_enum(&receiver.ty));
            let getter = tokens
                .method(declaring_type, &crate::expr::accessor_name("get_", name), &[])
                .ok_or(EmitError::Unsupported("property getter outside this module"))?;
            let setter = tokens
                .method(
                    setter_declaring_type,
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
            emit_modify(user_step, result_conversion, binary, &target.ty, rhs, checked, frame, tokens, out)?;
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
            if matches!(receiver.ty, TypeSymbol::Pointer(_)) {
                emit_expression(receiver, frame, tokens, out)?;
                emit_expression(&indices[0], frame, tokens, out)?;
                crate::expr::emit_sizeof(&target.ty, tokens, out)?;
                out.push(Instruction::simple(Opcode::Mul));
                out.push(Instruction::simple(Opcode::Add));
                let address = frame.reserve_local(&receiver.ty);
                out.push(Instruction::new(Opcode::Stloc, Operand::Variable(address)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(address)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(address)));
                out.push(Instruction::simple(crate::expr::ldind_opcode(&target.ty)));
                if leave == Leave::Old {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                emit_modify(user_step, result_conversion, binary, &target.ty, rhs, checked, frame, tokens, out)?;
                if leave == Leave::New {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                out.push(Instruction::simple(crate::expr::stind_opcode(&target.ty)));
                if let Some(slot) = kept {
                    out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
                }
                return Ok(());
            }
            emit_expression(receiver, frame, tokens, out)?;
            let array = frame.reserve_local(&receiver.ty);
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(array)));
            emit_expression(&indices[0], frame, tokens, out)?;
            let index = frame.reserve_local(&TypeSymbol::Special(SpecialType::Int32));
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(index)));
            if tokens.is_struct(&target.ty) || tokens.is_enum(&target.ty) {
                let token = tokens
                    .instruction_type_token(&target.ty)
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
                emit_modify(user_step, result_conversion, binary, &target.ty, rhs, checked, frame, tokens, out)?;
                if leave == Leave::New {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                out.push(Instruction::new(Opcode::Stobj, Operand::Token(token)));
            } else {
                let load = crate::expr::ldelem_instruction(&target.ty, tokens)?;
                let store = crate::expr::stelem_instruction(&target.ty, tokens)?;
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(array)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(index)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(array)));
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(index)));
                out.push(load);
                if leave == Leave::Old {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                emit_modify(user_step, result_conversion, binary, &target.ty, rhs, checked, frame, tokens, out)?;
                if leave == Leave::New {
                    out.push(Instruction::simple(Opcode::Dup));
                    out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
                }
                out.push(store);
            }
            if let Some(slot) = kept {
                out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(slot)));
            }
            Ok(())
        }
        BoundExprKind::Dereference { operand } => {
            emit_expression(operand, frame, tokens, out)?;
            let address = frame.reserve_local(&operand.ty);
            out.push(Instruction::new(Opcode::Stloc, Operand::Variable(address)));
            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(address)));
            out.push(Instruction::new(Opcode::Ldloc, Operand::Variable(address)));
            out.push(Instruction::simple(crate::expr::ldind_opcode(&target.ty)));
            if leave == Leave::Old {
                out.push(Instruction::simple(Opcode::Dup));
                out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
            }
            emit_modify(user_step, result_conversion, binary, &target.ty, rhs, checked, frame, tokens, out)?;
            if leave == Leave::New {
                out.push(Instruction::simple(Opcode::Dup));
                out.push(Instruction::new(Opcode::Stloc, Operand::Variable(kept.unwrap())));
            }
            out.push(Instruction::simple(crate::expr::stind_opcode(&target.ty)));
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
    result_conversion: Option<Token>,
    binary: BinaryOperator,
    operand_ty: &TypeSymbol,
    rhs: Option<&BoundExpr>,
    checked: bool,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    match user_step {
        Some(token) => {
            out.push(Instruction::new(Opcode::Call, Operand::Token(token)));
            if let Some(conversion) = result_conversion {
                out.push(Instruction::new(Opcode::Call, Operand::Token(conversion)));
            }
            Ok(())
        }
        None => emit_combine(binary, operand_ty, rhs, checked, frame, tokens, out),
    }
}

/// Pushes the right-hand value (the `op=` value, or the implicit `1` of `++`/`--` in the
/// target's type) and applies `binary` -- string `+` is `String.Concat`, not `add`.
fn emit_combine(
    binary: BinaryOperator,
    operand_ty: &TypeSymbol,
    rhs: Option<&BoundExpr>,
    checked: bool,
    frame: &Frame,
    tokens: &Tokens,
    out: &mut Vec<Instruction>,
) -> Result<(), EmitError> {
    if binary == BinaryOperator::Add
        && rhs.is_some()
        && matches!(
            operand_ty,
            TypeSymbol::Special(SpecialType::String | SpecialType::Object)
        )
    {
        return match crate::expr::compound_concat(operand_ty, rhs) {
            crate::expr::CompoundConcat::Spliced {
                reference,
                arguments,
            } => {
                for argument in &arguments {
                    emit_expression(argument, frame, tokens, out)?;
                }
                crate::expr::emit_concat_call(&reference, tokens, out)
            }
            crate::expr::CompoundConcat::Pairwise(reference) => {
                if let Some(value) = rhs {
                    emit_expression(value, frame, tokens, out)?;
                }
                crate::expr::emit_concat_call(&reference, tokens, out)
            }
        };
    }
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
    crate::expr::emit_binary(binary, operand_ty, checked, out)?;
    narrow_compound_result(operand_ty, checked, out);
    Ok(())
}

/// Narrows a compound-assignment result back to a sub-int target: `conv.*` in an unchecked
/// context, `conv.ovf.*` (throwing on overflow) in a checked one (14.14.2 / 14.5.12). Targets of
/// int width or wider share int's stack representation and need no narrowing.
fn narrow_compound_result(operand_ty: &TypeSymbol, checked: bool, out: &mut Vec<Instruction>) {
    let TypeSymbol::Special(special) = operand_ty else {
        return;
    };
    if !matches!(
        special,
        SpecialType::SByte
            | SpecialType::Byte
            | SpecialType::Int16
            | SpecialType::UInt16
            | SpecialType::Char
    ) {
        return;
    }
    let op = if checked {
        crate::expr::checked_overflow_conversion(*special, false)
    } else {
        crate::expr::numeric_conversion(operand_ty).ok()
    };
    if let Some(op) = op {
        out.push(Instruction::simple(op));
    }
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
        let bound = Binder::new().bind_method(None, "M", int(), &params, &[], false, false, &body);
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
                Opcode::Ldarg,
                Opcode::Ret,
                Opcode::Ret,
            ]
        );
        assert_eq!(target(&body[3]), 6);
    }

    #[test]
    fn emission_records_a_sequence_point_per_statement() {
        let body = parse_statement("{ int x = 1; return x; }").statement;
        let bound = Binder::new().bind_method(None, "M", int(), &[], &[], false, false, &body);
        let emitted = emit_body(&[], &[], &[], &bound, &Tokens::new(), 0, &int(), None, None)
            .expect("should lower");

        let offsets: Vec<u32> = emitted
            .sequence_points
            .iter()
            .map(|(offset, _)| *offset)
            .collect();
        assert_eq!(offsets, [0, 2]);
        assert!(
            emitted.sequence_points[0].1.unwrap().start
                < emitted.sequence_points[1].1.unwrap().start
        );
    }

    #[test]
    fn a_debug_build_brackets_the_method_with_brace_points() {
        let source = "{ int x = 1; return x; }";
        let body = parse_statement(source).statement;
        let block_span = body.span;
        let bound = Binder::new().bind_method(None, "M", int(), &[], &[], false, false, &body);
        let emitted =
            emit_body(&[], &[], &[], &bound, &Tokens::new(), 0, &int(), None, Some(source.as_bytes()))
                .expect("should lower");

        assert_eq!(emitted.code[0].opcode, Opcode::Nop);
        let first = emitted.sequence_points.first().expect("an opening point");
        assert_eq!(first.0, 0);
        assert_eq!(first.1, Some(Span::new(block_span.start, block_span.start + 1)));
        let last = emitted.sequence_points.last().expect("a closing point");
        assert_eq!(last.1, Some(Span::new(block_span.end - 1, block_span.end)));
        assert_eq!(emitted.code.last().unwrap().opcode, Opcode::Ret);
    }

    #[test]
    fn an_always_throwing_method_omits_its_closing_brace() {
        let source = "{ throw null; }";
        let body = parse_statement(source).statement;
        let bound = Binder::new().bind_method(None, "M", int(), &[], &[], false, false, &body);
        let emitted =
            emit_body(&[], &[], &[], &bound, &Tokens::new(), 0, &int(), None, Some(source.as_bytes()))
                .expect("should lower");
        let braced = |offset: u32| {
            emitted
                .sequence_points
                .iter()
                .any(|(_, span)| *span == Some(Span::new(offset, offset + 1)))
        };
        assert!(braced(0), "method `{{` should be a point");
        assert!(
            !braced(source.len() as u32 - 1),
            "method `}}` must be omitted -- the epilogue ret is unreachable"
        );
    }

    #[test]
    fn nested_block_closes_its_brace_only_when_reachable() {
        let source = "{ int a = 0; { int reachable = 1; } if (a > 0) { return 2; } return 0; }";
        let body = parse_statement(source).statement;
        let bound = Binder::new().bind_method(None, "M", int(), &[], &[], false, false, &body);
        let emitted =
            emit_body(&[], &[], &[], &bound, &Tokens::new(), 0, &int(), None, Some(source.as_bytes()))
                .expect("should lower");
        let braced = |offset: u32| {
            emitted
                .sequence_points
                .iter()
                .any(|(_, span)| *span == Some(Span::new(offset, offset + 1)))
        };
        let bare_open = source.find("{ int reachable").unwrap() as u32;
        let bare_close = source[..source.find(" if").unwrap()].rfind('}').unwrap() as u32;
        assert!(braced(bare_open), "bare block `{{` should be a point");
        assert!(braced(bare_close), "bare block `}}` should be a point");
        let then_open = source.find("{ return").unwrap() as u32;
        let then_close = source[..source.rfind("return 0").unwrap()].rfind('}').unwrap() as u32;
        assert!(braced(then_open), "if-then block `{{` should be a point");
        assert!(
            !braced(then_close),
            "if-then block `}}` is unreachable after `return` and must not be a point"
        );
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
