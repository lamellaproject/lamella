//! Rewrites a generator body into a state machine, before the tree reaches the encoder.

use crate::ast::{Expression, Function, MemberProperty, Pattern, Statement, SwitchCase};
use crate::diagnostic::{DiagnosticKind, Diagnostics, Phase};
use crate::source::Span;
use crate::{Box, ToString, Vec};

/// The binding the desugared body reaches its frame through.
///
/// **IT CONTAINS A SPACE, SO NO PROGRAM CAN NAME IT.** The lexer cannot produce this as an
/// identifier, and the encoder's string pool takes any bytes -- so the desugared tree can refer to
/// it while a source file, however adversarial, cannot shadow, read or assign it. A name that is
/// merely unlikely (`__frame`, `$state`) is a name some program is entitled to use, and a
/// conformance corpus is exactly the collection of programs that use the unlikely ones.
pub(crate) const FRAME: &str = " generator frame";

/// The frame property holding which case runs next.
pub(crate) const STATE: &str = "state";

/// The frame property saying whether the step that just ran reached a `yield`.
///
/// # THE SUSPENSION SAYS SO ITSELF RATHER THAN BEING DEDUCED FROM THE STATE
///
/// The transform sets this immediately before each synthesized `return`, and those are the only
/// places that set it; the runtime clears it before each step and reads it after. **A state number
/// cannot answer the same question**, because a loop's back edge suspends at the state it resumed
/// from: `while (c) { yield 1; }` jumps to the loop head and stops again exactly where it started,
/// so a rule comparing the state across a step reports a running generator as finished.
///
/// **It needs no walk over the body's own `return`s** -- an original `return` simply never sets
/// this, so every exit a body has is answered without finding any of them.
pub(crate) const SUSPENDED: &str = "suspended";

/// The frame property holding the value a resumed `yield` evaluates to.
///
/// `next(v)` writes `v` here before the body runs, and a case that resumes reads it wherever the
/// original `yield` stood.
///
/// **CASE 0 CANNOT READ IT, WHICH IS WHY `next` MAY WRITE IT UNCONDITIONALLY.** A read is only
/// ever emitted at a resume point, and a resume point by construction follows a suspension -- so
/// the first resumption, which `GeneratorStart` specifies as discarding its argument, discards it
/// here by having nowhere to observe it rather than by a check that could drift.
pub(crate) const SENT: &str = "sent";

/// The frame property holding the exception a resumption is delivering, or the one being handled.
///
/// It carries two things that are never live at once: what `throw(v)` is injecting on its way in,
/// and what the dispatch's `catch` is handing to a handler block on its way round. A `catch`
/// clause's parameter is bound from it at the top of the handler.
pub(crate) const THROWN: &str = "thrown";

/// The frame property saying WHICH of the three resumptions this is.
///
/// ```text
///     0   next(v)     the value is in `sent`
///     1   throw(v)    the value is in `thrown`, and the resume block raises it
///     2   return(v)   the value is in `thrown`, and the resume block returns it
/// ```
///
/// # ONE SLOT RATHER THAN A FLAG PER KIND, BECAUSE THE KINDS ARE EXCLUSIVE
///
/// The standard calls these a completion, and it branches on which one it is -- `GeneratorResume`
/// for the first and `GeneratorResumeAbrupt` for the other two. Two booleans would let a caller
/// set both, which is a state the standard has no name for; a number cannot express it.
///
/// # WHY A RESUMPTION HAS TO CARRY ITS KIND AT ALL
///
/// `throw()` and `return()` deliver AT the suspension point, which is inside the body. While a
/// `try` around a `yield` was refused there was nothing between that point and the caller, so
/// completing the generator was the same program -- and that is what both did. A body that can
/// CATCH makes throw different, and a `yield*` makes return different too: 15.5.5 step 8.c hands a
/// return completion to the DELEGATE's `return` method rather than finishing anything.
///
/// So a resume block begins by asking. The slot is cleared as it is read, because a resumption that
/// left it set would act again at the next suspension.
pub(crate) const KIND: &str = "kind";

/// [`KIND`] for a `throw()` resumption.
pub(crate) const KIND_THROW: f64 = 1.0;

/// [`KIND`] for a `return()` resumption.
pub(crate) const KIND_RETURN: f64 = 2.0;

/// Control fell off the end of the `try`, so the `finally` runs and then nothing else happens.
const REASON_NORMAL: f64 = 0.0;
/// Control threw, so the `finally` runs and then the exception is raised again.
const REASON_THROW: f64 = 1.0;
/// Control returned -- a `return` written in the body, or a `return()` delivered at a suspension
/// inside it -- so the `finally` runs and then the return happens.
const REASON_RETURN: f64 = 2.0;

/// The frame slot saying WHY control reached a `finally`, one per `try` that has one.
///
/// # A SLOT PER `try`, NOT A STACK ON THE FRAME
///
/// `try`/`finally` nests statically, so which slot is live is a property of where the code was
/// written -- the same reason [`Machine::handlers`] is a table rather than a run-time stack. A
/// stack would have to be pushed and popped on every path INCLUDING the abrupt ones, which is
/// exactly the bookkeeping a `finally` exists to spare the programmer.
fn reason_slot(index: usize) -> crate::String {
    crate::format!("reason{index}")
}

/// The frame slot holding what that reason carries: the exception for a throw, the operand for a
/// return, and an unread `undefined` for a normal completion.
///
/// **WRITTEN EVEN WHEN IT IS NOT READ.** A `try`/`finally` inside a loop reaches the same slot on
/// every pass, so a normal completion that left the previous pass's exception in place would hand
/// it back the next time the reason said to read it.
fn value_slot(index: usize) -> crate::String {
    crate::format!("value{index}")
}

/// `GetIterator(source)`, parking the record in this iteration's frame slots.
pub(crate) const ITER_OPEN: &str = "iterOpen";
/// `IteratorStep`, answering `false` once the iterator reports completion.
pub(crate) const ITER_STEP: &str = "iterStep";
/// `IteratorValue` on a step's result object.
pub(crate) const ITER_VALUE: &str = "iterValue";
/// `IteratorClose`, with a flag saying whether the loop is leaving under a throw.
pub(crate) const ITER_CLOSE: &str = "iterClose";

/// The frame slot holding one iteration's iterator object.
pub(crate) fn iterator_slot(index: usize) -> crate::String {
    crate::format!("iterator{index}")
}

/// The frame slot holding the `next` method that iteration captured.
///
/// **CAPTURED ONCE, AT `GetIterator`, AND READ FROM HERE EVERY STEP.** 7.4.3 reads `next` when the
/// record is made and never again, so an accessor on `next` runs once however long the loop runs --
/// and a program can count that. Re-reading it per step would be a different observable program.
pub(crate) fn next_method_slot(index: usize) -> crate::String {
    crate::format!("iteratorNext{index}")
}

/// The frame slot holding whether that iteration's record is done.
///
/// A record marked done is never closed, and it is marked done by an iterator reporting completion
/// AND by one that threw out of its own `next` -- neither is asked to clean up after itself.
pub(crate) fn iteration_done_slot(index: usize) -> crate::String {
    crate::format!("iteratorDone{index}")
}

/// The frame slot holding one iteration's current step result.
fn iteration_result_slot(index: usize) -> crate::String {
    crate::format!("iteratorResult{index}")
}

/// A fresh `{}` for an object literal whose members are written one at a time.
pub(crate) const OBJECT_OPEN: &str = "objectOpen";

/// `CreateDataPropertyOrThrow` on that object -- what a literal's `k: v` member does, and NOT what
/// an assignment does: a literal defines, so no inherited setter runs.
pub(crate) const OBJECT_ADD: &str = "objectAdd";

/// `CopyDataProperties` on that object -- what a literal's `...source` member does.
pub(crate) const OBJECT_SPREAD: &str = "objectSpread";

/// One element of an array pattern: the next value, or `undefined` once the iterator is spent.
///
/// **A PATTERN AND A LOOP DIFFER IN WHAT EXHAUSTION MEANS, AND IN NOTHING ELSE.** `[a, b] = [1]`
/// leaves `b` undefined where a `for`-`of` would have stopped, so this answers a VALUE where
/// [`ITER_STEP`] answers `false` -- and both open their iterator with [`ITER_OPEN`] and park the
/// record in the same slots, because that half is identical.
pub(crate) const PATTERN_STEP: &str = "patternStep";

/// The remainder of an array pattern's iterator, as a fresh array -- `[a, ...rest]`.
pub(crate) const PATTERN_REST: &str = "patternRest";

/// `IteratorClose`, but only when the pattern stopped before its iterator did.
pub(crate) const PATTERN_CLOSE: &str = "patternClose";

/// `RequireObjectCoercible`, answering the value so it can be parked in the same statement.
pub(crate) const REQUIRE_OBJECT: &str = "requireObject";

/// One turn of the 15.5.5 loop: takes the resumption the frame is carrying, does whatever that
/// resumption means to the delegate, and answers what the outer generator should do next.
///
/// **A DELEGATION OPENS ITS ITERATOR WITH [`ITER_OPEN`] AND NUMBERS IT FROM THE SAME COUNTER AS A
/// `for`-`of`.** 15.5.5 step 4 and 14.7.5.6 both say `GetIterator`, and the record is parked the
/// same way, so a second operation here would be one algorithm with two implementations -- the
/// mistake this file has already paid for twice. Sharing the counter is what keeps the slots
/// distinct, and a `yield*` inside a `for`-`of` is an ordinary shape that needs them to be.
pub(crate) const DELEGATE_STEP: &str = "delegateStep";

/// The frame slot holding which resumption a delegation is carrying into its next step.
///
/// It takes the same three values as [`KIND`], and it is a SECOND slot rather than a reader of that
/// one because the two are live at different times: `kind` is cleared as the resume block reads it,
/// and this survives from there to the loop head, which is a different block and, after the first
/// pass, a different call to `next()`.
pub(crate) fn delegate_mode_slot(index: usize) -> crate::String {
    crate::format!("delegateMode{index}")
}

/// The frame slot holding the value that resumption carried -- `sent` for a `next`, `thrown` for
/// the other two, chosen where the resumption is captured rather than where it is used.
pub(crate) fn delegate_sent_slot(index: usize) -> crate::String {
    crate::format!("delegateSent{index}")
}

/// The frame slot holding what a step answered. What it MEANS is [`delegate_disposition_slot`].
pub(crate) fn delegate_payload_slot(index: usize) -> crate::String {
    crate::format!("delegatePayload{index}")
}

/// The frame slot saying which of the three things the payload is.
///
/// # A SECOND SLOT BECAUSE ONE CALL ANSWERS ONE VALUE, AND THE STEP HAS THREE ANSWERS
///
/// 15.5.5 folds three outcomes into one loop body -- forward this result object, finish with this
/// value, leave the body with this value -- and they are told apart by WHERE the algorithm is, not
/// by anything about the value. A payload of `undefined` is legal in all three. So the step writes
/// which one it took, exactly as a `finally` writes [`reason_slot`], and for the same reason.
///
/// A fourth outcome needs no disposition: an exception is an abrupt completion out of the step
/// itself, and the dispatch's own `catch` routes it by state like any other.
pub(crate) fn delegate_disposition_slot(index: usize) -> crate::String {
    crate::format!("delegateDisposition{index}")
}

/// [`delegate_disposition_slot`]: the payload is a result object to hand the consumer AS IT STANDS,
/// and the loop goes round.
pub(crate) const DISPOSITION_YIELD: f64 = 0.0;

/// [`delegate_disposition_slot`]: the payload is the `yield*` expression's own value, and the
/// delegation is over.
pub(crate) const DISPOSITION_VALUE: f64 = 1.0;

/// [`delegate_disposition_slot`]: the payload is a return completion's operand, and the outer body
/// leaves through every iteration close and finalizer standing between it and the caller.
pub(crate) const DISPOSITION_RETURN: f64 = 2.0;

/// The frame property saying the suspension is forwarding a result object as it stands.
///
/// # THE OBJECT'S IDENTITY IS OBSERVABLE, SO IT CANNOT BE REBUILT
///
/// 15.5.5 step 7.a.vii is `GeneratorYield(innerResult)` -- the object the DELEGATE returned, not a
/// fresh `{value, done}` carrying the same two fields. `g.next()` on a generator delegating to an
/// iterator whose `next` answers a known object must answer that very object, and a corpus tests it
/// with `SameValue`. Every other suspension builds its result, so the runtime's default is to
/// build one and this says not to.
///
/// Cleared before each step beside [`SUSPENDED`], for the same reason: what is read afterwards has
/// to describe the step that just ran.
pub(crate) const VERBATIM: &str = "verbatim";

/// The frame slot holding one partly-evaluated operand across a suspension.
///
/// # WHY AN OPERAND NEEDS A SLOT AT ALL
///
/// `a + (yield b)` evaluates `a` first and the addition last, so `a`'s VALUE at the moment it was
/// read has to survive a suspension that happens between the two. Nothing in the desugared body can
/// hold it: a JavaScript local would be a name a program is entitled to shadow, and the dispatch
/// re-enters its `switch` on every step, so anything declared inside a case is gone by the next
/// one. The frame is the one thing that survives, exactly as it is for an iterator record.
pub(crate) fn temporary_slot(index: usize) -> crate::String {
    crate::format!("temporary{index}")
}

/// Whether a name is BOUND or ASSIGNED anywhere in these statements, at any depth.
///
/// # WHY A `catch` PARAMETER THAT IS ONLY READ CAN BE RE-BOUND PER BLOCK, AND ONE THAT IS NOT CANNOT
///
/// A suspending handler is flattened into the graph, and every block of it re-establishes the
/// parameter with `let e = FRAME.catchParamN;` -- a real lexical binding, in a real block, which is
/// what keeps shadowing and closure capture correct without renaming anything. That is exact while
/// the parameter's value never changes: each re-binding reads the same slot, so every block and
/// every closure sees the same value.
///
/// **AN ASSIGNMENT BREAKS IT, AND SILENTLY.** `catch (e) { e = 1; yield; use(e); }` would write to
/// one block's binding and read a fresh one from the slot in the next, answering the ORIGINAL
/// exception. So a handler that rebinds the name is refused instead.
///
/// The walk is deliberately conservative and does NOT stop at a nested function: a parameter or
/// declaration there SHADOWS rather than assigns, so treating it as a rebind over-refuses and never
/// under-refuses. It is a check that a value is constant, not a scope resolver.
fn name_is_rebound(statements: &[Statement], name: &str) -> bool {
    fn in_pattern(pattern: &Pattern, name: &str) -> bool {
        match pattern {
            Pattern::Identifier { name: bound, .. } => bound == name,
            Pattern::Member { object, property, .. } => {
                in_expression(object, name)
                    || matches!(&**property,
                        MemberProperty::Computed { expression, .. } if in_expression(expression, name))
            }
            Pattern::Array { elements, rest, .. } => {
                elements.iter().flatten().any(|p| in_pattern(p, name))
                    || rest.as_ref().is_some_and(|p| in_pattern(p, name))
            }
            Pattern::Object { properties, rest, .. } => {
                properties.iter().any(|p| in_pattern(&p.value, name))
                    || rest.as_ref().is_some_and(|p| in_pattern(p, name))
            }
            Pattern::Default { target, .. } => in_pattern(target, name),
            Pattern::Rest { argument, .. } => in_pattern(argument, name),
        }
    }
    fn in_function(function: &crate::ast::Function, name: &str) -> bool {
        function.params.iter().any(|p| in_pattern(p, name))
            || function.name.as_deref() == Some(name)
            || in_statements(&function.body, name)
    }
    fn in_expression(expression: &Expression, name: &str) -> bool {
        let go = |e: &Expression| in_expression(e, name);
        match expression {
            Expression::Assignment { target, value, .. } => {
                let assigns = match &**target {
                    crate::ast::AssignmentTarget::Pattern { pattern, .. } => {
                        in_pattern(pattern, name)
                    }
                    _ => false,
                };
                assigns || go(value)
            }
            Expression::Update { argument, .. } => {
                matches!(&**argument, Expression::Identifier { name: n, .. } if n == name)
                    || go(argument)
            }
            Expression::Function(function) => in_function(function, name),
            Expression::Arrow(arrow) => {
                arrow.params.iter().any(|p| in_pattern(p, name))
                    || match &arrow.body {
                        crate::ast::ArrowBody::Expression(e) => go(e),
                        crate::ast::ArrowBody::Block(body) => in_statements(body, name),
                    }
            }
            Expression::Parenthesized { expression, .. }
            | Expression::Unary { argument: expression, .. }
            | Expression::Yield { argument: Some(expression), .. }
            | Expression::Member { object: expression, .. } => go(expression),
            Expression::Binary { left, right, .. } | Expression::Logical { left, right, .. } => {
                go(left) || go(right)
            }
            Expression::Conditional { test, consequent, alternate, .. } => {
                go(test) || go(consequent) || go(alternate)
            }
            Expression::Sequence { expressions, .. } => expressions.iter().any(go),
            Expression::Call { callee, arguments, .. }
            | Expression::New { callee, arguments, .. } => {
                go(callee)
                    || arguments.iter().any(|argument| match argument {
                        crate::ast::Argument::Expression(e)
                        | crate::ast::Argument::Spread { argument: e, .. } => go(e),
                    })
            }
            Expression::Array { elements, .. } => elements.iter().any(|element| match element {
                crate::ast::ArrayElement::Hole => false,
                crate::ast::ArrayElement::Expression(e)
                | crate::ast::ArrayElement::Spread { argument: e, .. } => go(e),
            }),
            Expression::Object { properties, .. } => properties.iter().any(|property| {
                use crate::ast::ObjectProperty as P;
                match property {
                    P::Property { value, .. } => go(value),
                    P::CoverInitializedName { name: bound, value, .. } => bound == name || go(value),
                    P::Spread { argument, .. } => go(argument),
                    P::Method { .. } => true,
                }
            }),
            Expression::Template { expressions, .. } => expressions.iter().any(go),
            Expression::Class(_) => true,
            _ => false,
        }
    }
    fn in_statements(statements: &[Statement], name: &str) -> bool {
        statements.iter().any(|statement| in_statement(statement, name))
    }
    fn in_statement(statement: &Statement, name: &str) -> bool {
        let go = |s: &Statement| in_statement(s, name);
        match statement {
            Statement::Declaration { declarations, .. } => declarations.iter().any(|d| {
                in_pattern(&d.target, name)
                    || d.init.as_ref().is_some_and(|e| in_expression(e, name))
            }),
            Statement::Expression { expression, .. }
            | Statement::Throw { argument: expression, .. } => in_expression(expression, name),
            Statement::Return { argument: Some(expression), .. } => in_expression(expression, name),
            Statement::Block { body, .. } => in_statements(body, name),
            Statement::If { test, consequent, alternate, .. } => {
                in_expression(test, name)
                    || go(consequent)
                    || alternate.as_ref().is_some_and(|s| go(s))
            }
            Statement::While { test, body, .. } | Statement::DoWhile { test, body, .. } => {
                in_expression(test, name) || go(body)
            }
            Statement::For { init, test, update, body, .. } => {
                init.as_ref().is_some_and(|i| match &**i {
                    crate::ast::ForInit::Declaration { declarations, .. } => declarations
                        .iter()
                        .any(|d| in_pattern(&d.target, name)),
                    crate::ast::ForInit::Pattern(p) => in_pattern(p, name),
                    crate::ast::ForInit::Expression(e) => in_expression(e, name),
                }) || test.as_ref().is_some_and(|e| in_expression(e, name))
                    || update.as_ref().is_some_and(|e| in_expression(e, name))
                    || go(body)
            }
            Statement::ForIn { left, right, body, .. }
            | Statement::ForOf { left, right, body, .. } => {
                (match &**left {
                    crate::ast::ForInit::Declaration { declarations, .. } => {
                        declarations.iter().any(|d| in_pattern(&d.target, name))
                    }
                    crate::ast::ForInit::Pattern(p) => in_pattern(p, name),
                    crate::ast::ForInit::Expression(e) => in_expression(e, name),
                }) || in_expression(right, name)
                    || go(body)
            }
            Statement::Switch { discriminant, cases, .. } => {
                in_expression(discriminant, name)
                    || cases.iter().any(|case| {
                        case.test.as_ref().is_some_and(|e| in_expression(e, name))
                            || in_statements(&case.body, name)
                    })
            }
            Statement::Try { block, handler, finalizer, .. } => {
                in_statements(block, name)
                    || handler.as_ref().is_some_and(|c| {
                        c.param.as_ref().is_some_and(|p| in_pattern(p, name))
                            || in_statements(&c.body, name)
                    })
                    || finalizer.as_ref().is_some_and(|f| in_statements(f, name))
            }
            Statement::Labeled { body, .. } | Statement::With { body, .. } => go(body),
            Statement::Function(function) => in_function(function, name),
            Statement::Class(_) => true,
            _ => false,
        }
    }
    in_statements(statements, name)
}

/// The frame slot holding one `catch` clause's exception while its handler is flattened.
fn catch_param_slot(index: usize) -> crate::String {
    crate::format!("catchParam{index}")
}

/// For each item, whether any item AFTER it suspends -- which is exactly when its own value has to
/// be parked in a frame slot.
fn later_flags<T>(items: &[T], contains: impl Fn(&T) -> bool) -> Vec<bool> {
    let mut flags = crate::vec![false; items.len()];
    let mut seen = false;
    for i in (0..items.len()).rev() {
        flags[i] = seen;
        seen = seen || contains(&items[i]);
    }
    flags
}

/// Whether an expression is a bare read of one of the frame's own slots.
///
/// That is what a suspension whose value nobody wants leaves behind, and evaluating it as a
/// statement does nothing -- the frame's slots are data properties this transform writes itself,
/// with no accessor a read could trigger.
fn is_frame_read(expression: &Expression) -> bool {
    match expression {
        Expression::Member { object, property, .. } => {
            matches!(&**object, Expression::Identifier { name, .. } if name == FRAME)
                && matches!(&**property, MemberProperty::Identifier { .. })
        }
        _ => false,
    }
}

/// Whether re-reading this expression after a suspension is the same program as reading it before.
///
/// **ONLY A LITERAL QUALIFIES.** An identifier does not: its binding can be assigned between the
/// two reads, by the very `next(v)` that resumed the generator.
fn is_stable(expression: &Expression) -> bool {
    matches!(
        expression,
        Expression::Number { .. }
            | Expression::String { .. }
            | Expression::Boolean { .. }
            | Expression::Null { .. }
    )
}

fn array_element_contains_yield(element: &crate::ast::ArrayElement) -> bool {
    match element {
        crate::ast::ArrayElement::Hole => false,
        crate::ast::ArrayElement::Expression(e)
        | crate::ast::ArrayElement::Spread { argument: e, .. } => expression_contains_yield(e),
    }
}

fn argument_contains_yield(argument: &crate::ast::Argument) -> bool {
    match argument {
        crate::ast::Argument::Expression(e) | crate::ast::Argument::Spread { argument: e, .. } => {
            expression_contains_yield(e)
        }
    }
}

fn object_property_contains_yield(property: &crate::ast::ObjectProperty) -> bool {
    use crate::ast::ObjectProperty as P;
    match property {
        P::Property { key, value, .. } => {
            expression_contains_yield(value)
                || match key {
                    crate::ast::PropertyKey::Computed { expression, .. } => {
                        expression_contains_yield(expression)
                    }
                    _ => false,
                }
        }
        P::CoverInitializedName { value, .. } => expression_contains_yield(value),
        P::Spread { argument, .. } => expression_contains_yield(argument),
        P::Method { .. } => false,
    }
}

/// What a compound assignment is called in a refusal, named by the target it writes through.
///
/// **THE SENTENCE THIS REPLACES NAMED THE WRONG SHAPE.** Every assignment the spiller could not
/// linearize reported "in the value of an assignment to a member", and the corpus says the great
/// majority of them are destructuring assignments -- which contain no member at all. A reader was
/// handed a description of a different program, and a count grouped on that message was grouping
/// two shapes under one name.
fn compound_assignment_position(pattern: &Pattern) -> &'static str {
    match pattern {
        Pattern::Array { .. } | Pattern::Object { .. } => {
            "in the value of a compound destructuring assignment"
        }
        _ => "in the value of a compound assignment to a member",
    }
}

/// What an expression kind is called in a refusal, for the kinds the spiller cannot linearize.
fn spill_position(expression: &Expression) -> &'static str {
    match expression {
        Expression::New { .. } => "in the arguments of a `new`",
        Expression::Tagged { .. } => "in a tagged template's substitution",
        Expression::Update { .. } => "in the operand of an increment",
        Expression::Class(_) => "in a class's computed member name",
        Expression::Arrow(_) => "in an arrow's parameter default",
        _ => "nested inside a larger expression",
    }
}

/// Whether an expression contains a `yield` that belongs to THIS generator.
///
/// The walk stops where the body's scope does -- at a nested function, and at an arrow's or a
/// class's body -- so a `yield` written in one of those is not this generator's and is not found.
///
/// **IT SEES THROUGH PARENTHESES, WHICH A MATCH ON THE NODE DOES NOT.** `(yield 1)` keeps a
/// `Parenthesized` wrapper, because other rules turn on whether one was written -- `(a) = b` and
/// `delete (x)` both do -- so the five statement positions that once asked
/// `matches!(.., Expression::Yield { .. })` refused `(yield 1);`, `var a = (yield 1);`,
/// `a = (yield 1);` and `return (yield 1);` as shapes this profile lacked. Asking what an
/// expression CONTAINS rather than what it IS retires that whole class of question.
fn expression_contains_yield(expression: &Expression) -> bool {
    let mut found = false;
    visit_yields_in_expression(expression, &mut |_| found = true);
    found
}

/// `void 0`, which is `undefined` and cannot be shadowed the way the identifier can.
fn undefined(span: Span) -> Expression {
    Expression::Unary {
        operator: crate::ast::UnaryOperator::Void,
        argument: Box::new(Expression::Number { value: 0.0, span }),
        span,
    }
}

/// The one sentence every refusal of an unsupported `yield` position reports, with the position
/// that refused it named.
///
/// **WRITTEN ONCE BECAUSE IT WAS WRITTEN FOUR TIMES.** These positions are found at four sites -- a
/// statement the graph cannot take apart, a `yield` a split left behind, one in a loop header, and
/// one nested in an expression -- and when loops left the unsupported list, three of the four
/// sentences were updated. The corpus then reported both texts at once, which is a rule with
/// several implementations gaining its new case in only some of them, caught by the population
/// rather than by a reader.
///
/// # AND ONE SENTENCE WAS NAMING FIVE ABSENCES
///
/// It read "a `yield` inside an expression, a labelled statement, a `for-in`, a `for-of` or a
/// `finally`", so every site said all five things. A user who hit one was handed the list and left
/// to work out which; the compiler cannot separate them either, and the published absence list is
/// a deliverable rather than a note. The positions are different features with different costs, so
/// each site now names its own -- through one template, because the reason above has not changed.
fn unsupported_yield(position: &str) -> crate::String {
    crate::format!("a `yield` {position} is not in this profile")
}

/// What a statement kind is called in a refusal, for the kinds the graph cannot take apart.
///
/// A kind that IS flattened never reaches here, so the fall-through names what it actually found
/// rather than listing everything it might have.
fn statement_position(statement: &Statement) -> &'static str {
    match statement {
        Statement::ForIn { .. } => "in a `for`-`in`",
        Statement::ForOf { .. } => "in a `for`-`of`",
        Statement::Labeled { .. } => "inside a labelled statement",
        Statement::Switch { .. } => "inside a `switch`",
        Statement::With { .. } => "inside a `with`",
        Statement::Throw { .. } => "in the operand of a `throw`",
        Statement::Try { .. } => "in a `try`",
        Statement::Break { .. } | Statement::Continue { .. } => "in a jump to a label",
        Statement::Expression { .. } => "nested inside a larger expression",
        Statement::Declaration { .. } => "nested inside a declaration's initializer",
        Statement::Return { .. } => "nested inside a `return` operand",
        Statement::Class(_) => "in a class's computed member name",
        _ => "in this position",
    }
}

/// Rewrites `function` in place when it is a generator whose body this profile can transform, and
/// reports one diagnostic per shape it cannot.
///
/// **THE REFUSAL AND THE CAPABILITY ARE THE SAME FUNCTION, DELIBERATELY.** A separate positional
/// check would be a second implementation of one rule -- "which `yield` shapes are supported" --
/// and this engine's most expensive recurring defect is a rule with several implementations that
/// gains a new case in only one of them. Here a shape is supported exactly when this code rewrites
/// it, so the published absence cannot drift from the behaviour.
pub(crate) fn rewrite(function: &mut Function, diagnostics: &mut Diagnostics) {
    if !function.is_generator {
        return;
    }
    let mut refused = false;
    for parameter in &function.params {
        let mut found: Option<Span> = None;
        visit_yields_in_pattern(parameter, &mut |span| found = found.or(Some(span)));
        if let Some(span) = found {
            diagnostics.error(
                Phase::Syntactic,
                DiagnosticKind::EarlyError,
                span,
                "a generator's parameter list may not contain `yield`",
            );
            refused = true;
        }
    }
    if !function.body.iter().any(statement_contains_yield) {
        return;
    }
    for statement in &function.body {
        refuse_lexical_declarations(statement, diagnostics, &mut refused);
    }
    let mut machine = Machine::new(function.span);
    machine.lower_block(function.body.clone(), diagnostics, &mut refused);
    if refused {
        return;
    }
    function.body = machine.finish();
}

/// The body, flattened into numbered blocks that jump to one another.
///
/// # WHY A FLAT GRAPH AND NOT A LIST OF STEPS
///
/// The first slice of this transform split the top-level statement list at each `yield` and emitted
/// one switch case per piece. That is exactly right while control flows forwards through the list
/// once, and it cannot express a loop at all: resuming inside a loop body means re-entering the
/// middle of a construct the dispatch has already left.
///
/// So the body becomes a graph. Every block is a switch case, every edge is `FRAME.state = N;
/// continue;`, and the switch sits inside a `for (;;)` so that taking an edge re-dispatches without
/// returning to the caller. A `yield` is the one edge that leaves: it sets the state, marks the
/// frame suspended, and returns.
///
/// **A BLOCK IS ALLOCATED BEFORE IT IS FILLED**, because a loop head has to name its exit before
/// the body between them exists. Blocks are therefore emitted in ALLOCATION order rather than
/// build order -- a switch does not care, and the alternative is a patch-up pass over numbers
/// already written into the tree.
struct Machine {
    blocks: Vec<Vec<Statement>>,
    current: usize,
    /// The jump targets of each enclosing loop this transform flattened, innermost last.
    ///
    /// **AN UNLABELLED `break` OR `continue` INSIDE A FLATTENED LOOP IS AN EDGE, NOT A `break`.**
    /// The loop it named no longer exists as a construct; what encloses it now is the dispatch
    /// `for (;;)`, so leaving it alone would break out of the state machine and finish the
    /// generator. Each one becomes the edge it meant.
    loops: Vec<LoopTargets>,
    /// The block a `throw` from the block being built lands in, innermost first as it changes.
    ///
    /// **A HANDLER IS A PROPERTY OF THE BLOCK, NOT A STACK KEPT AT RUN TIME.** Which `catch`
    /// covers a piece of the body is decided when the body is read, so the machine needs no
    /// handler stack on the frame: the dispatch's own `catch` looks the thrown-from state up in a
    /// table the transform already knows. And it can, because a block only assigns `FRAME.state`
    /// at its edges -- so while a block's statements are running, `FRAME.state` still holds the
    /// number that was dispatched on, which is that block's.
    handler: Option<usize>,
    /// The handler covering each block, parallel to `blocks`.
    handlers: Vec<Option<usize>>,
    /// The `finally` blocks enclosing the block being built, outermost first.
    ///
    /// **A `return` INSIDE A `try` THAT HAS A `finally` IS NOT A `return`**, and neither is the one
    /// a `return()` resumption delivers at a suspension in there. Both have to reach the finalizer
    /// first and happen again after it, so each is routed through [`Machine::leave_with_return`]
    /// rather than emitted where it was written.
    finalizers: Vec<Finally>,
    /// The `for`-`of` loops enclosing the block being built, outermost first.
    ///
    /// **AN OPEN ITERATION IS A DEBT, AND EVERY WAY OUT OF THE LOOP PAYS IT.** 14.7.5.6 closes the
    /// iterator on `break`, on `return` and on a throw, and NOT when the iterator itself reported
    /// completion -- so the close cannot be attached to the loop's exit, which all four reach.
    iterations: Vec<usize>,
    /// How many `for`-`of` loops have been lowered, which is where the next one's slot number comes
    /// from -- and not the depth, for the reason given on `finalizer_count`.
    iteration_count: usize,
    /// How many `finally` blocks have been lowered, which is where the next one's slot number comes
    /// from.
    ///
    /// **NOT THE DEPTH.** Two `try`/`finally`s at the same depth are never live at once and could
    /// share a slot -- except that one of them may be written INSIDE the other's finalizer, where
    /// the outer slot is still live and about to be read. A counter cannot express that mistake.
    finalizer_count: usize,
    /// How many expression temporaries have been allocated, which is where the next slot number
    /// comes from.
    ///
    /// **NOT A DEPTH AND NOT REUSED.** Two temporaries at the same nesting depth can both be live
    /// across one suspension -- `f(yield 1, yield 2)` parks the callee and the first argument and
    /// reads both after the second `yield` -- so a counter that went back down would hand the same
    /// slot to two values the same statement still needs.
    temporary_count: usize,
    /// How many `catch` clauses have been flattened, which is where the next one's slot
    /// number comes from. Not the depth, for the reason `finalizer_count` gives.
    catch_count: usize,
    span: Span,
}

/// One `try`'s `finally`: where its statements begin, and which pair of frame slots it owns.
#[derive(Clone, Copy)]
struct Finally {
    run: usize,
    index: usize,
    /// How many `for`-`of` iterations were open when this `try` was entered.
    ///
    /// **IT SEPARATES THE ITERATIONS THIS FINALIZER IS INSIDE FROM THE ONES IT IS OUTSIDE**, which
    /// is the whole of the ordering question in [`Machine::closes_down_to`]. The mirror of
    /// [`LoopTargets::finalizer_depth`], and needed for the same reason: a count on its own says
    /// nothing, and the DIFFERENCE between two of them says everything.
    iteration_depth: usize,
}

#[derive(Clone, Copy)]
struct LoopTargets {
    /// Where `continue` goes: the update block of a `for`, the test of a `while`.
    next: usize,
    /// Where `break` goes.
    exit: usize,
    /// How many `finally` blocks were open when this loop started.
    ///
    /// **IT IS THE DIFFERENCE, NOT THE COUNT, THAT DECIDES.** A `break` inside a `try` whose loop
    /// is ALSO inside that `try` crosses nothing and is an ordinary edge; the same `break` with the
    /// loop OUTSIDE the `try` has to run the finalizer on its way. Both look identical at the
    /// `break` -- one finalizer is open in each -- and only the loop's own depth separates them.
    finalizer_depth: usize,
}

/// What the resume block of a suspension does with the kind of resumption it woke up on.
///
/// **THE SUSPENSION KNOWS AND THE RESUME BLOCK DOES NOT.** Whether a `throw()` should be raised
/// here or handed to a delegate is decided by which construct emitted the suspension, and that is
/// gone by the time the resume block is built -- so the answer travels with the suspension instead
/// of being inferred from the machine, where a later construct would have to remember to update it.
#[derive(Clone, Copy)]
enum Resumption {
    /// Act on it where it lands: raise a `throw()`, leave the body on a `return()`.
    Acts,
    /// Record it for the delegation numbered `index` and re-enter its loop at `head`.
    Delegates { index: usize, head: usize },
}

impl Machine {
    fn new(span: Span) -> Self {
        Machine {
            blocks: crate::vec![Vec::new()],
            current: 0,
            loops: Vec::new(),
            handler: None,
            handlers: crate::vec![None],
            finalizers: Vec::new(),
            iterations: Vec::new(),
            iteration_count: 0,
            finalizer_count: 0,
            temporary_count: 0,
            catch_count: 0,
            span,
        }
    }

    /// Reserves a block and answers its number. It is empty until something opens it.
    fn alloc(&mut self) -> usize {
        self.blocks.push(Vec::new());
        self.handlers.push(None);
        self.blocks.len() - 1
    }

    /// **THE HANDLER IS RECORDED WHERE THE BLOCK IS OPENED, NOT WHERE IT IS ALLOCATED.** A loop's
    /// exit block is allocated inside the loop and opened after it, and a `throw` from the exit
    /// belongs to whatever encloses the loop rather than to the loop's own `try`.
    fn open(&mut self, block: usize) {
        self.current = block;
        self.handlers[block] = self.handler;
    }

    fn emit(&mut self, statement: Statement) {
        self.blocks[self.current].push(statement);
    }

    /// `FRAME.state = target; continue;` -- an edge that does NOT return to the caller.
    fn jump(&mut self, target: usize) {
        let span = self.span;
        self.emit(assign_state(target, span));
        self.emit(Statement::Continue { label: None, span });
    }

    /// The one edge that leaves: hand `argument` to the caller and resume at `resume`.
    ///
    /// The order is load-bearing in one direction only -- the state and the marker must both be
    /// written before the `return` -- but they are written in this order so that a reader meets the
    /// resume point first, which is the half that says where control comes back.
    fn suspend(&mut self, argument: Option<Expression>, resume: usize, resumption: Resumption) {
        let span = self.span;
        self.emit(assign_state(resume, span));
        self.emit(assign_suspended(span));
        if matches!(resumption, Resumption::Delegates { .. }) {
            self.emit(assign_frame(VERBATIM, Expression::Boolean { value: true, span }, span));
        }
        self.emit(Statement::Return { argument, span });
        self.open(resume);
        for statement in self.resumption_prologue(resumption) {
            self.emit(statement);
        }
    }

    /// What a resume block does before anything else, and the ONE place that decides which.
    ///
    /// **THERE ARE TWO ANSWERS AND A THIRD SITE MUST NOT INVENT A THIRD.** Ordinarily a resumption
    /// is acted on where it lands; inside a delegation it is captured and handed to the delegate.
    /// The choice belongs to the suspension, so it travels with it as [`Resumption`] rather than
    /// being re-derived from the machine's state -- which is the same rule that put
    /// [`Machine::leaving_with_return`] in one place after its second implementation went wrong.
    fn resumption_prologue(&self, resumption: Resumption) -> Vec<Statement> {
        match resumption {
            Resumption::Acts => self.resumption_guards(),
            Resumption::Delegates { index, head } => self.resumption_capture(index, head),
        }
    }

    /// A resumption arriving at a `yield*`: recorded for the delegate, and acted on by nobody here.
    ///
    /// ```text
    ///     FRAME.delegateModeN = 0;
    ///     FRAME.delegateSentN = FRAME.sent;
    ///     if (FRAME.kind === 1) { FRAME.delegateModeN = 1; FRAME.delegateSentN = FRAME.thrown; }
    ///     if (FRAME.kind === 2) { FRAME.delegateModeN = 2; FRAME.delegateSentN = FRAME.thrown; }
    ///     FRAME.kind = 0;
    ///     FRAME.state = head; continue;
    /// ```
    ///
    /// # EVERY TEST ON `kind` IS POSITIVE, AND WRITING ONE THE OTHER WAY ROUND IS A WRONG ANSWER
    ///
    /// **[`KIND`] IS ABSENT UNTIL AN ABRUPT RESUMPTION IS DELIVERED.** The frame is built with a
    /// state and a `sent` and nothing else, and only `throw()` and `return()` ever write this slot
    /// -- so on an ordinary `next()` it reads `undefined`, not `0`. A guard asking `!== 0` is
    /// therefore TRUE on the commonest path in the language, and one written that way sent the
    /// delegate `thrown` instead of `sent` on the first resumption after a delegation began. Both
    /// values are `undefined` at that moment for most programs, which is why it survived ten
    /// hand-written probes and was caught by a corpus file that passed `next(5555)`.
    ///
    /// [`Machine::resumption_guards`] asks `=== 1` and `=== 2` and is immune for the same reason.
    ///
    /// **BOTH GUARDS OF [`Machine::resumption_guards`] ARE EXACTLY WRONG HERE, WHICH IS THE WHOLE
    /// REASON THIS EXISTS.** A `throw()` delivered at a `yield*` is not raised in the outer body:
    /// 15.5.5 step 7.b hands it to the delegate's `throw` method, and the outer body only sees an
    /// exception if the delegate has no such method or its own `throw` raises. A `return()` is not
    /// a return either: step 7.c hands it to the delegate's `return` method, which may answer
    /// `done: false` and keep the loop running -- so a generator can legally refuse to finish.
    ///
    /// The kind is cleared as it is read, exactly as the acting guards clear it, because a
    /// resumption left set would be captured a second time at the next suspension.
    fn resumption_capture(&self, index: usize, head: usize) -> Vec<Statement> {
        let span = self.span;
        let mode = delegate_mode_slot(index);
        let sent = delegate_sent_slot(index);
        let abrupt = |kind: f64| Statement::If {
            test: Expression::Binary {
                operator: crate::ast::BinaryOperator::StrictEqual,
                left: Box::new(read_frame(KIND, span)),
                right: Box::new(Expression::Number { value: kind, span }),
                span,
            },
            consequent: Box::new(Statement::Block {
                body: crate::vec![
                    assign_frame(&mode, Expression::Number { value: kind, span }, span),
                    assign_frame(&sent, read_frame(THROWN, span), span),
                ],
                span,
            }),
            alternate: None,
            span,
        };
        crate::vec![
            assign_frame(&mode, Expression::Number { value: 0.0, span }, span),
            assign_frame(&sent, read_sent(span), span),
            abrupt(KIND_THROW),
            abrupt(KIND_RETURN),
            assign_frame(KIND, Expression::Number { value: 0.0, span }, span),
            assign_state(head, span),
            Statement::Continue { label: None, span },
        ]
    }

    /// What a resume block does before anything else: act on the kind of resumption this is.
    ///
    /// ```text
    ///     if (FRAME.kind === 1) { FRAME.kind = 0; throw FRAME.thrown; }
    ///     if (FRAME.kind === 2) { FRAME.kind = 0; return FRAME.thrown; }
    /// ```
    ///
    /// **THE `throw` NEEDS NO SPECIAL CASE FOR A `finally` AND THE `return` DOES.** A throw goes to
    /// the dispatch's own `catch`, which routes it by state through the handler table -- and a
    /// `try`/`finally` registers its finalizer in that table exactly as a `catch` does, so the
    /// first guard is right wherever it stands. A return completion is not an exception: 27.5.3.2
    /// requires the finalizer to run, and the dispatch's `catch` must not see it, so it cannot
    /// travel the same road and is re-routed here instead.
    ///
    /// Neither guard sets `suspended`, so a body leaving through the second one reports `done` with
    /// the returned value by the ordinary rule.
    fn resumption_guards(&self) -> Vec<Statement> {
        let span = self.span;
        let guard = |kind: f64, body: Vec<Statement>| Statement::If {
            test: Expression::Binary {
                operator: crate::ast::BinaryOperator::StrictEqual,
                left: Box::new(read_frame(KIND, span)),
                right: Box::new(Expression::Number { value: kind, span }),
                span,
            },
            consequent: Box::new(Statement::Block { body, span }),
            alternate: None,
            span,
        };
        let clear = || assign_frame(KIND, Expression::Number { value: 0.0, span }, span);
        let mut throwing = crate::vec![clear()];
        throwing.push(Statement::Throw { argument: read_frame(THROWN, span), span });
        let mut resuming = crate::vec![clear()];
        resuming.extend(self.leaving_with_return(read_frame(THROWN, span)));
        crate::vec![guard(KIND_THROW, throwing), guard(KIND_RETURN, resuming)]
    }

    /// Everything a return completion has to do on its way out of the body, in order.
    ///
    /// # TWO CALLERS, AND THE SECOND ONE IS WHY THIS IS A FUNCTION
    ///
    /// A return leaves the body from a `return` written in it and from a `return()` the consumer
    /// delivers at a suspension, and the two are built in different places -- one emits into the
    /// current block, the other assembles a list for a resume block. When `for`-`of` added "close
    /// every open iteration" to the sequence, **only the written `return` got it**: `return()` on a
    /// generator suspended inside a `for`-`of` left the iterator open, which the crate suite could
    /// not see because nothing about the returned value is wrong.
    ///
    /// So the sequence lives here once. Its order is load-bearing, and it is decided by STATIC
    /// NESTING rather than by kind: the closes and the finalizers interleave, innermost first, and
    /// [`Machine::closes_down_to`] is the one place that works out which closes come before the
    /// next finalizer outward.
    /// Whether a `return` written in the block being built is more than a `return`.
    ///
    /// A finalizer is due, an iterator close is due, or both -- and the fast path in
    /// [`Machine::lower`] has to ask before it emits one whole.
    /// [`Machine::leaving_with_return`] is the sequence itself.
    fn a_return_here_has_work_to_do(&self) -> bool {
        !self.finalizers.is_empty() || !self.iterations.is_empty()
    }

    /// The iteration closes a return performs before it reaches `outer`, innermost first.
    ///
    /// # AN ITERATOR IS CLOSED BY WHATEVER LEAVES ITS LOOP, AND A `finally` INSIDE THAT LOOP IS NOT
    ///
    /// Both orders are right somewhere, and only the nesting says which:
    ///
    /// ```text
    ///     try { for (x of it) { yield 1; } } finally { f(); }   close it, THEN f
    ///     for (x of it) { try { yield 1; } finally { f(); } }   f, THEN close it
    /// ```
    ///
    /// Closing every open iteration first is exact for the first and inverted for the second, where
    /// the finalizer belongs to a `try` the loop has not been left yet -- `f` must run while the
    /// iteration is still open, because leaving the loop is what closes it. So each `Finally`
    /// records how many iterations were open when it was pushed, and a return closes only the ones
    /// INSIDE it before handing control over. The finalizer's own tail then asks the same question
    /// about the next finalizer outward, which is why this is a function rather than a loop here.
    fn closes_down_to(&self, outer: Option<Finally>, span: Span) -> Vec<Statement> {
        let floor = outer.map_or(0, |finally| finally.iteration_depth);
        self.iterations[floor.min(self.iterations.len())..]
            .iter()
            .rev()
            .map(|index| close_iteration(*index, false, span))
            .collect()
    }

    fn leaving_with_return(&self, value: Expression) -> Vec<Statement> {
        let span = self.span;
        let active = self.finalizers.last().copied();
        let mut out = self.closes_down_to(active, span);
        match active {
            Some(active) => out.extend(routed_return(active, value, span)),
            None => out.push(Statement::Return { argument: Some(value), span }),
        }
        out
    }

    /// A `break` or `continue` taking its edge, unless that edge would leave a `finally` behind.
    ///
    /// # A JUMP OUT OF A `finally` IS NOT A JUMP, AND EMITTING ONE IS A MISCOMPILE
    ///
    /// `for (;;) { try { yield 1; break; } finally { f(); } }` must run `f` before it leaves. The
    /// edge is a plain `FRAME.state = exit; continue;`, which reaches the exit having run nothing
    /// -- silently, with the right value and the wrong effects. It shipped that way: measured
    /// `f0,end` where the standard says `f0,f1,end`, and every `continue` skipped its finalizer
    /// entirely.
    ///
    /// **A `return` in the same position is handled and a jump is not, and the difference is that a
    /// return has ONE destination.** A jump has as many as there are enclosing loops, so parking a
    /// target for the finalizer to resume needs the finalizer's tail to know how far the jump was
    /// going -- which is not a property of the finalizer. Until that is built the edge is refused
    /// where it is written, which is a published absence rather than a wrong answer.
    fn leave_to(
        &mut self,
        target: usize,
        targets: LoopTargets,
        keyword: &str,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
    ) {
        if self.finalizers.len() > targets.finalizer_depth {
            refuse(
                diagnostics,
                self.span,
                &crate::format!(
                    "a `{keyword}` that leaves a `finally` behind is not in this profile"
                ),
            );
            *refused = true;
            return;
        }
        self.jump(target);
    }

    /// A `return` that must run an enclosing `finally` first, or an ordinary one where there is
    /// none.
    fn leave_with_return(&mut self, argument: Option<Expression>) {
        let span = self.span;
        let value = argument.unwrap_or_else(|| undefined(span));
        for statement in self.leaving_with_return(value) {
            self.emit(statement);
        }
    }

    /// Runs a `yield` or a `yield*` standing in VALUE POSITION and answers the expression its
    /// result is read from. On return the machine's current block is the one control reaches with
    /// that value available, so a caller emits its own statement straight into it.
    ///
    /// # ONE PLACE DECIDES WHICH OF THE TWO THIS IS
    ///
    /// Four statement positions accept a suspension -- `yield e;`, `var a = yield e;`,
    /// `a = yield e;` and `return yield e;` -- and they differ only in what they do with the value.
    /// A `yield*` differs only in how the value is REACHED. Asking at each of the four would be one
    /// rule with four implementations and four chances to add the delegating case to three of them,
    /// which is the exact shape of the last two defects this file shipped.
    fn suspend_for_value(
        &mut self,
        expression: Expression,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
        span: Span,
    ) -> Expression {
        match split_value(expression, diagnostics, refused, span) {
            Suspension::Plain(argument) => {
                let argument =
                    argument.map(|a| self.spill(a, diagnostics, refused, span));
                let resume = self.alloc();
                self.suspend(argument, resume, Resumption::Acts);
                read_sent(span)
            }
            Suspension::Delegated(subject) => {
                let subject = self.spill(subject, diagnostics, refused, span);
                self.delegate(subject, span)
            }
            Suspension::None(expression) => self.spill(expression, diagnostics, refused, span),
        }
    }

    /// A `catch` whose handler suspends: flattened into the graph, with its parameter re-established
    /// in every block the handler owns.
    ///
    /// # THE PARAMETER IS THE WHOLE DIFFICULTY, AND IT IS A SCOPE RATHER THAN A VALUE
    ///
    /// Everywhere else the handler is emitted as ONE statement -- `{ let e = FRAME.thrown; ... }` --
    /// so `e` is a real lexical binding and shadowing, capture and the temporal dead zone are the
    /// language's business rather than this transform's. That is exactly what a suspension cannot
    /// have: the dispatch re-enters its `switch` on every `next()`, so a binding made in one block
    /// is gone by the next, and the cases share one scope so a second `let e` would not even parse.
    ///
    /// **SO THE VALUE GOES ON THE FRAME AND THE BINDING IS REMADE PER BLOCK.** Each block the
    /// handler owns becomes `{ let e = FRAME.catchParamN; ... }`, which is a real binding again --
    /// no renaming, no walk into nested functions, and shadowing inside the handler keeps working
    /// because nothing was rewritten. A nested `try` inside this one wraps its own blocks first, so
    /// the two bindings nest in the order the source wrote them.
    ///
    /// # WHAT IT COSTS, AND WHY THAT IS A REFUSAL RATHER THAN A CLEVERER SCHEME
    ///
    /// Re-reading the slot per block is only faithful while the value never changes. An assignment
    /// to the parameter would write one block's binding and be lost at the next, so a handler that
    /// rebinds the name is refused by [`name_is_rebound`] -- conservatively, since it does not
    /// resolve scopes. A DESTRUCTURING parameter is refused too: re-running it per block would
    /// re-iterate the exception, which a program can watch.
    fn lower_suspending_catch(
        &mut self,
        catch: crate::ast::CatchClause,
        first: usize,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
        span: Span,
    ) {
        let mut in_param = false;
        if let Some(param) = &catch.param {
            visit_yields_in_pattern(param, &mut |_| in_param = true);
        }
        if in_param {
            refuse(diagnostics, catch.span, &unsupported_yield("in a `catch` parameter"));
            *refused = true;
        }
        let binding = match &catch.param {
            None => None,
            Some(Pattern::Identifier { name, .. }) => Some(name.clone()),
            Some(_) => {
                refuse(
                    diagnostics,
                    catch.span,
                    &unsupported_yield("inside a `catch` that destructures its parameter"),
                );
                *refused = true;
                return;
            }
        };
        if let Some(name) = &binding {
            if name_is_rebound(&catch.body, name) {
                refuse(
                    diagnostics,
                    catch.span,
                    &unsupported_yield("inside a `catch` whose parameter is assigned"),
                );
                *refused = true;
                return;
            }
        }
        let index = self.catch_count;
        self.catch_count += 1;
        let slot = catch_param_slot(index);
        let _ = first;
        let body_start = self.alloc();
        if binding.is_some() {
            self.emit(assign_frame(&slot, read_frame(THROWN, span), span));
        }
        self.jump(body_start);
        self.open(body_start);
        self.lower_block(catch.body, diagnostics, refused);
        let Some(name) = binding else { return };
        for block in body_start..self.blocks.len() {
            if self.blocks[block].is_empty() {
                continue;
            }
            let body = core::mem::take(&mut self.blocks[block]);
            let mut rebound = crate::vec![Statement::Declaration {
                kind: crate::ast::DeclarationKind::Let,
                declarations: crate::vec![crate::ast::Declarator {
                    target: Pattern::Identifier { name: name.clone(), span },
                    init: Some(read_frame(&slot, span)),
                    span,
                }],
                span,
            }];
            rebound.extend(body);
            self.blocks[block] = crate::vec![Statement::Block { body: rebound, span }];
        }
    }

    /// A frame slot number for one partly-evaluated operand.
    fn next_temporary(&mut self) -> usize {
        let index = self.temporary_count;
        self.temporary_count += 1;
        index
    }

    /// Evaluates one operand of a larger expression and answers how to read its value afterwards.
    ///
    /// `later_suspends` says whether anything evaluated AFTER this operand contains a `yield`. When
    /// it does the value has to outlive that suspension, so it is parked in a frame slot and the
    /// rebuilt expression reads the slot instead.
    ///
    /// **AN IDENTIFIER IS NOT SAFE TO RE-READ, AND THAT IS THE WHOLE REASON FOR THE SLOT.**
    /// `a + (yield b)` reads `a` BEFORE the suspension, and the consumer may assign a different `a`
    /// before resuming; an engine that rebuilt the addition from the name would use the new one.
    /// Only a literal is the same program read twice.
    fn spill_part(
        &mut self,
        expression: Expression,
        later_suspends: bool,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
        span: Span,
    ) -> Expression {
        let value = self.spill(expression, diagnostics, refused, span);
        if !later_suspends || is_stable(&value) {
            return value;
        }
        let slot = temporary_slot(self.next_temporary());
        self.emit(assign_frame(&slot, value, span));
        read_frame(&slot, span)
    }

    /// An assignment that carries a suspension, decided on its TARGET.
    ///
    /// # THE THREE TARGETS ARE THREE ORDERS, WHICH IS WHY THEY ARE NOT ONE CASE
    ///
    /// 13.15.2 evaluates a SIMPLE target's reference BEFORE the right-hand side and a
    /// DESTRUCTURING one's AFTER it, and the two answer with different values besides -- the
    /// assigned value for a simple target, the right-hand side itself for a pattern. A generator
    /// can suspend on either side of that order, so both halves are observable.
    ///
    /// - **An identifier** names a binding. Nothing runs before the value and nothing is parked.
    /// - **A member** carries its own expressions -- the object, and a computed key -- and both run
    ///   before the value. [`Machine::park_target`] parks them, so the assignment that lands after
    ///   the suspension writes through the reference the program evaluated rather than deriving a
    ///   second one from operands the suspension may have changed.
    /// - **A pattern** is `DestructuringAssignmentEvaluation`, which is [`Machine::destructure`].
    ///
    /// # PLAIN `=` ONLY, AND THE COMPOUND FORMS ARE A DIFFERENT MECHANISM
    ///
    /// `o[k] += yield 1` READS the target through its reference before the value runs, so
    /// rebuilding it after the suspension calls a getter a second time and reads a property the
    /// suspension may have moved. That needs the OLD VALUE parked as well as the reference, and the
    /// logical forms (`&&=`, `||=`, `??=`) short-circuit besides -- a branch in the graph rather
    /// than a slot, which is the same machinery a loop condition wants.
    fn spill_assignment(
        &mut self,
        operator: crate::ast::AssignmentOperator,
        target: crate::ast::AssignmentTarget,
        value: Expression,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
        span: Span,
    ) -> Expression {
        use crate::ast::{AssignmentOperator, AssignmentTarget};
        let (pattern, parenthesized) = match target {
            AssignmentTarget::Pattern { pattern, parenthesized } => (pattern, parenthesized),
            AssignmentTarget::Invalid(inner) => {
                *refused = true;
                return Expression::Assignment {
                    operator,
                    target: Box::new(AssignmentTarget::Invalid(inner)),
                    value: Box::new(value),
                    span,
                };
            }
        };
        let rebuild = |pattern, value| Expression::Assignment {
            operator,
            target: Box::new(AssignmentTarget::Pattern { pattern, parenthesized }),
            value: Box::new(value),
            span,
        };
        match pattern {
            Pattern::Identifier { .. } => {
                let value = self.spill(value, diagnostics, refused, span);
                rebuild(pattern, value)
            }
            Pattern::Member { .. } if operator == AssignmentOperator::Assign => {
                let parked = self.park_target(pattern, diagnostics, refused, span);
                let value = self.spill(value, diagnostics, refused, span);
                rebuild(parked, value)
            }
            Pattern::Array { .. } | Pattern::Object { .. }
                if operator == AssignmentOperator::Assign =>
            {
                let value = self.spill_part(value, true, diagnostics, refused, span);
                self.destructure(pattern, value.clone(), diagnostics, refused, span);
                value
            }
            other => {
                refuse(
                    diagnostics,
                    span,
                    &unsupported_yield(compound_assignment_position(&other)),
                );
                *refused = true;
                rebuild(other, value)
            }
        }
    }

    /// Evaluates an expression that contains a suspension somewhere other than at its root, and
    /// answers an expression that reads its result.
    ///
    /// # THE RULE IS ONE SENTENCE, AND EVERY ARM BELOW IS THAT SENTENCE
    ///
    /// Walk the operands in EVALUATION ORDER; park any whose value must outlive a suspension that
    /// comes later in the same expression; rebuild the node from what was parked. `a + (yield b)`
    /// becomes `FRAME.temporary0 = a; <suspend on b>; FRAME.temporary0 + FRAME.sent`.
    ///
    /// # WHAT IS REFUSED, AND WHY EACH ONE IS A DIFFERENT MECHANISM RATHER THAN MORE OF THIS ONE
    ///
    /// - **A SPREAD FOLLOWED BY A SUSPENSION.** `[...a, yield 1]` ITERATES `a` where it stands, and
    ///   parking `a` would move that iteration to after the suspension -- observable from the
    ///   iterator's own methods. Parking the ITERATED-OUT elements is a different transform.
    /// - **A SUBSTITUTION THAT NEEDS PARKING AND IS NOT ALREADY A STRING.** A template runs
    ///   `ToString` on each substitution BEFORE evaluating the next one (13.2.8.6), so parking the
    ///   value defers a call a program can observe on a `toString`.
    /// - **THE CONDITIONALLY-EVALUATED HALF OF `?:`, `&&`, `||` and `??`.** Those operands run only
    ///   sometimes, so a suspension in one is a BRANCH in the graph rather than a slot -- the same
    ///   machinery a loop condition needs, and not built here.
    /// - **A CALL WHOSE CALLEE IS A MEMBER.** `o.m(yield 1)` reads `o.m` before the argument and
    ///   calls it with `o` as `this`. Rebuilding it after the suspension would re-read the
    ///   property, and re-applying `this` needs a `call` this transform cannot name.
    fn spill(
        &mut self,
        expression: Expression,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
        span: Span,
    ) -> Expression {
        if !expression_contains_yield(&expression) {
            return expression;
        }
        match expression {
            Expression::Parenthesized { expression, .. } => {
                self.spill(*expression, diagnostics, refused, span)
            }
            Expression::Yield { .. } => {
                self.suspend_for_value(expression, diagnostics, refused, span)
            }
            Expression::Binary { operator, left, right, span } => {
                let later = expression_contains_yield(&right);
                let left = self.spill_part(*left, later, diagnostics, refused, span);
                let right = self.spill(*right, diagnostics, refused, span);
                Expression::Binary {
                    operator,
                    left: Box::new(left),
                    right: Box::new(right),
                    span,
                }
            }
            Expression::Sequence { expressions, span } => {
                let later = later_flags(&expressions, expression_contains_yield);
                let expressions = expressions
                    .into_iter()
                    .enumerate()
                    .map(|(i, e)| self.spill_part(e, later[i], diagnostics, refused, span))
                    .collect();
                Expression::Sequence { expressions, span }
            }
            Expression::Unary { operator, argument, span } => Expression::Unary {
                operator,
                argument: Box::new(self.spill(*argument, diagnostics, refused, span)),
                span,
            },
            Expression::Array { elements, span } => {
                let later = later_flags(&elements, array_element_contains_yield);
                let mut out = Vec::with_capacity(elements.len());
                for (i, element) in elements.into_iter().enumerate() {
                    out.push(match element {
                        crate::ast::ArrayElement::Hole => crate::ast::ArrayElement::Hole,
                        crate::ast::ArrayElement::Expression(e) => {
                            crate::ast::ArrayElement::Expression(
                                self.spill_part(e, later[i], diagnostics, refused, span),
                            )
                        }
                        crate::ast::ArrayElement::Spread { argument, span, comma_after } => {
                            let argument = if later[i] {
                                self.refuse_spill(
                                    "in an array spread that a later suspension follows",
                                    span,
                                    diagnostics,
                                    refused,
                                );
                                argument
                            } else {
                                self.spill(argument, diagnostics, refused, span)
                            };
                            crate::ast::ArrayElement::Spread { argument, span, comma_after }
                        }
                    });
                }
                Expression::Array { elements: out, span }
            }
            Expression::Object { properties, span } => {
                let later = later_flags(&properties, object_property_contains_yield);
                let piecewise = properties
                    .iter()
                    .zip(&later)
                    .any(|(property, later)| {
                        *later && matches!(property, crate::ast::ObjectProperty::Spread { .. })
                    });
                if !piecewise {
                    let mut out = Vec::with_capacity(properties.len());
                    for (i, property) in properties.into_iter().enumerate() {
                        out.push(self.spill_property(
                            property,
                            later[i],
                            diagnostics,
                            refused,
                            span,
                        ));
                    }
                    return Expression::Object { properties: out, span };
                }
                let slot = temporary_slot(self.next_temporary());
                self.emit(assign_frame(
                    &slot,
                    frame_call(OBJECT_OPEN, Vec::new(), span),
                    span,
                ));
                for property in properties {
                    self.build_object_member(&slot, property, diagnostics, refused, span);
                }
                read_frame(&slot, span)
            }
            Expression::Template { quasis, expressions, span } => {
                let later = later_flags(&expressions, expression_contains_yield);
                let mut out = Vec::with_capacity(expressions.len());
                for (i, e) in expressions.into_iter().enumerate() {
                    if later[i] && !is_stable(&e) {
                        self.refuse_spill(
                            "in a template substitution that a later suspension follows",
                            span,
                            diagnostics,
                            refused,
                        );
                        out.push(e);
                        continue;
                    }
                    out.push(self.spill_part(e, later[i], diagnostics, refused, span));
                }
                Expression::Template { quasis, expressions: out, span }
            }
            Expression::Call { callee, arguments, optional, span } => {
                let later = later_flags(&arguments, argument_contains_yield);
                let arguments_suspend = later.first().copied().unwrap_or(false)
                    || arguments.iter().any(argument_contains_yield);
                if arguments_suspend && matches!(peel_ref(&callee), Expression::Member { .. }) {
                    self.refuse_spill(
                        "in an argument of a method call",
                        span,
                        diagnostics,
                        refused,
                    );
                    return Expression::Call { callee, arguments, optional, span };
                }
                let callee = self.spill_part(*callee, arguments_suspend, diagnostics, refused, span);
                let mut out = Vec::with_capacity(arguments.len());
                for (i, argument) in arguments.into_iter().enumerate() {
                    out.push(match argument {
                        crate::ast::Argument::Expression(e) => crate::ast::Argument::Expression(
                            self.spill_part(e, later[i], diagnostics, refused, span),
                        ),
                        crate::ast::Argument::Spread { argument, span } => {
                            let argument = if later[i] {
                                self.refuse_spill(
                                    "in an array spread that a later suspension follows",
                                    span,
                                    diagnostics,
                                    refused,
                                );
                                argument
                            } else {
                                self.spill(argument, diagnostics, refused, span)
                            };
                            crate::ast::Argument::Spread { argument, span }
                        }
                    });
                }
                Expression::Call { callee: Box::new(callee), arguments: out, optional, span }
            }
            Expression::Member { object, property, optional, span } => {
                let later = match &*property {
                    MemberProperty::Computed { expression, .. } => {
                        expression_contains_yield(expression)
                    }
                    _ => false,
                };
                let object = self.spill_part(*object, later, diagnostics, refused, span);
                let property = match *property {
                    MemberProperty::Computed { expression, span: key } => {
                        MemberProperty::Computed {
                            expression: self.spill(expression, diagnostics, refused, span),
                            span: key,
                        }
                    }
                    other => other,
                };
                Expression::Member {
                    object: Box::new(object),
                    property: Box::new(property),
                    optional,
                    span,
                }
            }
            Expression::Conditional { test, consequent, alternate, span } => {
                let test = self.spill(*test, diagnostics, refused, span);
                if !expression_contains_yield(&consequent)
                    && !expression_contains_yield(&alternate)
                {
                    return Expression::Conditional {
                        test: Box::new(test),
                        consequent,
                        alternate,
                        span,
                    };
                }
                let slot = temporary_slot(self.next_temporary());
                let otherwise = self.alloc();
                let join = self.alloc();
                self.emit(Statement::If {
                    test: negate(test, span),
                    consequent: Box::new(self.edge(otherwise)),
                    alternate: None,
                    span,
                });
                let taken = self.spill(*consequent, diagnostics, refused, span);
                self.emit(assign_frame(&slot, taken, span));
                self.jump(join);
                self.open(otherwise);
                let taken = self.spill(*alternate, diagnostics, refused, span);
                self.emit(assign_frame(&slot, taken, span));
                self.jump(join);
                self.open(join);
                read_frame(&slot, span)
            }
            Expression::Logical { operator, left, right, span } => {
                let left = self.spill(*left, diagnostics, refused, span);
                if !expression_contains_yield(&right) {
                    return Expression::Logical {
                        operator,
                        left: Box::new(left),
                        right,
                        span,
                    };
                }
                let slot = temporary_slot(self.next_temporary());
                self.emit(assign_frame(&slot, left, span));
                let join = self.alloc();
                self.emit(Statement::If {
                    test: short_circuits(operator, read_frame(&slot, span), span),
                    consequent: Box::new(self.edge(join)),
                    alternate: None,
                    span,
                });
                let taken = self.spill(*right, diagnostics, refused, span);
                self.emit(assign_frame(&slot, taken, span));
                self.jump(join);
                self.open(join);
                read_frame(&slot, span)
            }
            Expression::Assignment { operator, target, value, span } => {
                self.spill_assignment(operator, *target, *value, diagnostics, refused, span)
            }
            other => {
                refuse(diagnostics, other.span(), &unsupported_yield(spill_position(&other)));
                *refused = true;
                other
            }
        }
    }

    /// One member of an object literal being built member by member, written where it stands.
    ///
    /// The key comes before the value, which is the order 13.2.5.5 gives them and the order that
    /// matters as soon as either can suspend.
    fn build_object_member(
        &mut self,
        target: &str,
        property: crate::ast::ObjectProperty,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
        span: Span,
    ) {
        use crate::ast::ObjectProperty as P;
        let call = |operation: &str, arguments: Vec<Expression>, machine: &mut Self| {
            machine.emit(Statement::Expression {
                expression: frame_call(operation, arguments, span),
                span,
            });
        };
        match property {
            P::Spread { argument, .. } => {
                let source = self.spill(argument, diagnostics, refused, span);
                call(OBJECT_SPREAD, crate::vec![read_frame(target, span), source], self);
            }
            P::Property { key, value, .. } => {
                let key = match key {
                    crate::ast::PropertyKey::Identifier { name, span: own } => {
                        Expression::String { value: crate::string_value::JsString::from(name.as_str()), span: own }
                    }
                    crate::ast::PropertyKey::String { value, span: own } => {
                        Expression::String { value, span: own }
                    }
                    crate::ast::PropertyKey::Number { value, span: own } => {
                        Expression::Number { value, span: own }
                    }
                    crate::ast::PropertyKey::Computed { expression, span: own } => {
                        let _ = own;
                        let computed = self.spill(*expression, diagnostics, refused, span);
                        self.spill_part(
                            computed,
                            expression_contains_yield(&value),
                            diagnostics,
                            refused,
                            span,
                        )
                    }
                    crate::ast::PropertyKey::Private { span: own, .. } => {
                        refuse(diagnostics, own, &unsupported_yield("beside a private name"));
                        *refused = true;
                        return;
                    }
                };
                let value = self.spill(value, diagnostics, refused, span);
                call(OBJECT_ADD, crate::vec![read_frame(target, span), key, value], self);
            }
            other => {
                let _ = other;
                refuse(
                    diagnostics,
                    span,
                    &unsupported_yield("beside a method in an object literal"),
                );
                *refused = true;
            }
        }
    }

    /// One object literal member, with its operands taken in evaluation order.
    fn spill_property(
        &mut self,
        property: crate::ast::ObjectProperty,
        later_suspends: bool,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
        span: Span,
    ) -> crate::ast::ObjectProperty {
        use crate::ast::ObjectProperty as P;
        match property {
            P::Property { key, value, computed, shorthand, span: own } => {
                let key = match key {
                    crate::ast::PropertyKey::Computed { expression, span: key_span }
                        if expression_contains_yield(&expression)
                            || expression_contains_yield(&value) =>
                    {
                        let later = expression_contains_yield(&value) || later_suspends;
                        crate::ast::PropertyKey::Computed {
                            expression: Box::new(self.spill_part(
                                *expression,
                                later,
                                diagnostics,
                                refused,
                                span,
                            )),
                            span: key_span,
                        }
                    }
                    other => other,
                };
                let value = self.spill_part(value, later_suspends, diagnostics, refused, span);
                P::Property { key, value, computed, shorthand, span: own }
            }
            P::Spread { argument, span: own } => {
                let argument = if later_suspends {
                    self.refuse_spill(
                        "in an object spread this profile could not build",
                        own,
                        diagnostics,
                        refused,
                    );
                    argument
                } else {
                    self.spill(argument, diagnostics, refused, span)
                };
                P::Spread { argument, span: own }
            }
            other => {
                refuse(
                    diagnostics,
                    span,
                    &unsupported_yield("in an object literal's member definition"),
                );
                *refused = true;
                other
            }
        }
    }

    /// A shape the spiller cannot linearize, named where it was written.
    fn refuse_spill(
        &self,
        position: &str,
        span: Span,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
    ) {
        refuse(diagnostics, span, &unsupported_yield(position));
        *refused = true;
    }

    /// Assigns `value` into an assignment pattern that contains a suspension, as explicit steps.
    ///
    /// # A PATTERN IS AN ALGORITHM, AND ONLY A SUSPENSION MAKES THAT VISIBLE
    ///
    /// `[a, b] = xs` is handed to the encoder whole everywhere else in this transform, and that is
    /// right: it is one statement whose insides nothing can re-enter. A `yield` written inside one
    /// -- `for ([a = yield] of xs)` -- makes the pattern a sequence of steps that control leaves and
    /// comes back to, so the steps have to BE steps: an iterator opened on the value and parked in
    /// the frame, one `patternStep` per element, a branch for each default, and a close at the end.
    ///
    /// # THE ORDER IS THE FEATURE, AND IT IS NOT THE ORDER THE PATTERN IS WRITTEN IN
    ///
    /// 8.6.2 evaluates each element's TARGET REFERENCE before stepping the iterator for that
    /// element -- so in `[x[yield]] = xs` the key is computed, and the suspension taken, BEFORE
    /// `xs`'s iterator is asked for anything. An implementation that stepped first would call
    /// `next` before the `yield`, which a program can see from the iterator. That is why
    /// [`Machine::park_target`] runs where it does and not later.
    ///
    /// Only patterns that CONTAIN a suspension come here; every other one is still emitted whole.
    fn destructure(
        &mut self,
        pattern: Pattern,
        value: Expression,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
        span: Span,
    ) {
        match pattern {
            Pattern::Identifier { .. } | Pattern::Member { .. } => {
                self.emit(assign_to(pattern, value, span));
            }
            Pattern::Array { elements, rest, .. } => {
                let index = self.iteration_count;
                self.iteration_count += 1;
                self.emit(Statement::Expression {
                    expression: frame_call(
                        ITER_OPEN,
                        crate::vec![number(index, span), value],
                        span,
                    ),
                    span,
                });
                for element in elements {
                    let step = frame_call(PATTERN_STEP, crate::vec![number(index, span)], span);
                    match element {
                        None => self.emit(Statement::Expression { expression: step, span }),
                        Some(element) => {
                            self.destructure_element(element, step, diagnostics, refused, span)
                        }
                    }
                }
                if let Some(rest) = rest {
                    let collected =
                        frame_call(PATTERN_REST, crate::vec![number(index, span)], span);
                    let target = match *rest {
                        Pattern::Rest { argument, .. } => *argument,
                        other => other,
                    };
                    self.destructure_element(target, collected, diagnostics, refused, span);
                }
                self.emit(Statement::Expression {
                    expression: frame_call(PATTERN_CLOSE, crate::vec![number(index, span)], span),
                    span,
                });
            }
            Pattern::Object { properties, rest, span: own } => {
                if rest.is_some() {
                    refuse(
                        diagnostics,
                        own,
                        &unsupported_yield("in an object pattern that has a rest element"),
                    );
                    *refused = true;
                    return;
                }
                let source = self.spill_part(
                    frame_call(REQUIRE_OBJECT, crate::vec![value], span),
                    true,
                    diagnostics,
                    refused,
                    span,
                );
                for property in properties {
                    let key = match property.key {
                        crate::ast::PropertyKey::Identifier { name, span: key } => {
                            MemberProperty::Identifier { name, span: key }
                        }
                        crate::ast::PropertyKey::Computed { expression, span: key } => {
                            let computed = self.spill(*expression, diagnostics, refused, span);
                            MemberProperty::Computed {
                                expression: self
                                    .spill_part(computed, true, diagnostics, refused, span),
                                span: key,
                            }
                        }
                        crate::ast::PropertyKey::String { value, span: key } => {
                            MemberProperty::Computed {
                                expression: Expression::String { value, span: key },
                                span: key,
                            }
                        }
                        crate::ast::PropertyKey::Number { value, span: key } => {
                            MemberProperty::Computed {
                                expression: Expression::Number { value, span: key },
                                span: key,
                            }
                        }
                        crate::ast::PropertyKey::Private { .. } => {
                            refuse(
                                diagnostics,
                                property.span,
                                &unsupported_yield("beside a private name in a pattern"),
                            );
                            *refused = true;
                            return;
                        }
                    };
                    let read = Expression::Member {
                        object: Box::new(source.clone()),
                        property: Box::new(key),
                        optional: false,
                        span,
                    };
                    self.destructure_element(property.value, read, diagnostics, refused, span);
                }
            }
            Pattern::Default { span: own, .. } | Pattern::Rest { span: own, .. } => {
                refuse(diagnostics, own, &unsupported_yield("in a pattern this profile cannot rebuild"));
                *refused = true;
            }
        }
    }

    /// One element or property of a pattern: its reference, then its value, then its default.
    ///
    /// The three happen in that order because 8.6.2 says so, and a suspension in any of them is
    /// what makes the order observable. A default is a BRANCH rather than a conditional expression:
    /// it runs only when the value is `undefined`, and it may suspend, so it needs its own block.
    fn destructure_element(
        &mut self,
        element: Pattern,
        value: Expression,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
        span: Span,
    ) {
        let (target, default) = match element {
            Pattern::Default { target, value: default, .. } => (*target, Some(*default)),
            other => (other, None),
        };
        let target = self.park_target(target, diagnostics, refused, span);
        let slot = temporary_slot(self.next_temporary());
        self.emit(assign_frame(&slot, value, span));
        if let Some(default) = default {
            let is_absent = Expression::Binary {
                operator: crate::ast::BinaryOperator::StrictEqual,
                left: Box::new(read_frame(&slot, span)),
                right: Box::new(undefined(span)),
                span,
            };
            if expression_contains_yield(&default) {
                let apply = self.alloc();
                let join = self.alloc();
                self.emit(Statement::If {
                    test: negate(is_absent, span),
                    consequent: Box::new(self.edge(join)),
                    alternate: None,
                    span,
                });
                self.jump(apply);
                self.open(apply);
                let value = self.spill(default, diagnostics, refused, span);
                self.emit(assign_frame(&slot, value, span));
                self.jump(join);
                self.open(join);
            } else {
                self.emit(Statement::If {
                    test: is_absent,
                    consequent: Box::new(Statement::Block {
                        body: crate::vec![assign_frame(&slot, default, span)],
                        span,
                    }),
                    alternate: None,
                    span,
                });
            }
        }
        self.destructure(target, read_frame(&slot, span), diagnostics, refused, span);
    }

    /// The parts of an assignment target that are expressions, evaluated and parked where 8.6.2
    /// puts them: BEFORE the element's value is obtained.
    ///
    /// A nested array or object pattern has no reference of its own -- 8.6.2 step 1 excludes it --
    /// so it comes back untouched and is destructured from the value instead.
    fn park_target(
        &mut self,
        pattern: Pattern,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
        span: Span,
    ) -> Pattern {
        let Pattern::Member { object, property, optional, span: own } = pattern else {
            return pattern;
        };
        let object = self.spill(*object, diagnostics, refused, span);
        let object = self.spill_part(object, true, diagnostics, refused, span);
        let property = match *property {
            MemberProperty::Computed { expression, span: key } => {
                let computed = self.spill(expression, diagnostics, refused, span);
                MemberProperty::Computed {
                    expression: self.spill_part(computed, true, diagnostics, refused, span),
                    span: key,
                }
            }
            other => other,
        };
        Pattern::Member {
            object: Box::new(object),
            property: Box::new(property),
            optional,
            span: own,
        }
    }

    /// `yield* e` -- 15.5.5, as a loop in the graph.
    ///
    /// ```text
    ///     FRAME.iterOpen(N, e)                              GetIterator, record parked
    ///     FRAME.delegateModeN = 0; FRAME.delegateSentN = void 0;
    ///   head:
    ///     FRAME.delegatePayloadN = FRAME.delegateStep(N)    one turn; writes the disposition
    ///     if (FRAME.delegateDispositionN === 1) -> finish
    ///     if (FRAME.delegateDispositionN === 2) -> leave
    ///     <suspend, forwarding FRAME.delegatePayloadN VERBATIM, resuming into a capture>
    ///   leave:   the return sequence, carrying FRAME.delegatePayloadN
    ///   finish:  the expression's value is FRAME.delegatePayloadN
    /// ```
    ///
    /// # THE THREE DISPOSITIONS ARE ONE CALL BECAUSE THE SPEC'S THREE BRANCHES SHARE THEIR TAIL
    ///
    /// 15.5.5 branches on the resumption and then does the same four things in each arm -- call,
    /// check the answer is an Object, read `done`, and either finish with `IteratorValue` or
    /// forward the result. Splitting that across three frame operations would put the shared tail
    /// in three places; splitting it across three blocks of desugared JavaScript would put the
    /// `Get`s there, where their ORDER is observable and the transform cannot see it. So the step
    /// is one native operation and the desugared body only routes what it answered.
    ///
    /// # A DELEGATION IS NOT PUSHED ONTO `iterations`, AND THAT IS THE SPEC'S SHAPE
    ///
    /// An open `for`-`of` closes its iterator on every abrupt way out. A `yield*` must not: an
    /// exception travelling out of it leaves the delegate open
    /// (each call is `?`, with no cleanup around it), and a `return()` is not an early exit at all
    /// -- step 7.c hands it to the delegate's own `return` method, which is what `leave` is for.
    /// The one close 15.5.5 does perform is step 7.b.iii, inside the step, where the delegate has
    /// no `throw` method. Pushing the delegation here would close the iterator a second time.
    fn delegate(&mut self, subject: Expression, span: Span) -> Expression {
        let index = self.iteration_count;
        self.iteration_count += 1;
        self.emit(Statement::Expression {
            expression: frame_call(
                ITER_OPEN,
                crate::vec![Expression::Number { value: index as f64, span }, subject],
                span,
            ),
            span,
        });
        self.emit(assign_frame(
            &delegate_mode_slot(index),
            Expression::Number { value: 0.0, span },
            span,
        ));
        self.emit(assign_frame(&delegate_sent_slot(index), undefined(span), span));

        let head = self.alloc();
        let leave = self.alloc();
        let finish = self.alloc();
        self.jump(head);
        self.open(head);
        let payload = delegate_payload_slot(index);
        self.emit(assign_frame(
            &payload,
            frame_call(
                DELEGATE_STEP,
                crate::vec![Expression::Number { value: index as f64, span }],
                span,
            ),
            span,
        ));
        let disposition = delegate_disposition_slot(index);
        let took = |value: f64| Expression::Binary {
            operator: crate::ast::BinaryOperator::StrictEqual,
            left: Box::new(read_frame(&disposition, span)),
            right: Box::new(Expression::Number { value, span }),
            span,
        };
        self.emit(Statement::If {
            test: took(DISPOSITION_VALUE),
            consequent: Box::new(self.edge(finish)),
            alternate: None,
            span,
        });
        self.emit(Statement::If {
            test: took(DISPOSITION_RETURN),
            consequent: Box::new(self.edge(leave)),
            alternate: None,
            span,
        });
        let resume = self.alloc();
        self.suspend(
            Some(read_frame(&payload, span)),
            resume,
            Resumption::Delegates { index, head },
        );

        self.open(leave);
        self.leave_with_return(Some(read_frame(&payload, span)));

        self.open(finish);
        read_frame(&payload, span)
    }

    /// Wraps the blocks in the dispatch.
    ///
    /// `for (;;) { switch (FRAME.state) { ... } break; }` -- a case that falls out of the switch
    /// reaches the `break` and leaves the loop, which is how the body's own end is reached. A case
    /// that jumps `continue`s past it, and a case that suspends has already returned.
    ///
    /// # EVERY CASE IS TERMINATED, BECAUSE A SWITCH CASE FALLS THROUGH
    ///
    /// **This is not tidiness and it is not free.** In the first slice every case ended in a
    /// `return`, so nothing fell anywhere; a graph has blocks that end in ordinary statements -- a
    /// loop's exit, a branch's join -- and in a switch those run straight into the NEXT block's
    /// code, which is some other part of the program entirely. Left out, the first `if` inside a
    /// loop spins forever.
    ///
    /// The terminator is `break`, which inside a switch leaves the SWITCH, and control then meets
    /// the dispatch's own `break` and leaves the `for` -- so a block that simply ran out is the
    /// body reaching its end, and the function falls off into `undefined`. It is appended to every
    /// block rather than to the ones that need it: after a `return` or a `continue` it is
    /// unreachable, and deciding which blocks those are is a second implementation of a rule the
    /// emitter already has.
    fn finish(self) -> Vec<Statement> {
        let span = self.span;
        let handlers = self.handlers.clone();
        let cases = self
            .blocks
            .into_iter()
            .enumerate()
            .map(|(index, mut body)| {
                body.push(Statement::Break { label: None, span });
                SwitchCase { test: Some(number(index, span)), body, span }
            })
            .collect();
        let dispatch = Statement::Switch { discriminant: read_state(span), cases, span };
        let guarded = match handler_dispatch(&handlers, span) {
            Some(catch) => Statement::Try {
                block: crate::vec![dispatch],
                handler: Some(catch),
                finalizer: None,
                span,
            },
            None => dispatch,
        };
        crate::vec![Statement::For {
            init: None,
            test: None,
            update: None,
            body: Box::new(Statement::Block {
                body: crate::vec![guarded, Statement::Break { label: None, span }],
                span,
            }),
            span,
        }]
    }

    /// Lowers a statement list into the current block, opening new ones as suspensions require.
    fn lower_block(
        &mut self,
        body: Vec<Statement>,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
    ) {
        for statement in body {
            self.lower(statement, diagnostics, refused);
        }
    }

    /// Lowers one statement.
    ///
    /// **THE REFUSAL AND THE CAPABILITY ARE THE SAME FUNCTION, DELIBERATELY.** A separate
    /// positional check would be a second implementation of one rule -- "which `yield` shapes are
    /// supported" -- and this engine's most expensive recurring defect is a rule with several
    /// implementations that gains a new case in only one of them. Here a shape is supported exactly
    /// when this code rewrites it, so the published absence cannot drift from the behaviour.
    fn lower(&mut self, statement: Statement, diagnostics: &mut Diagnostics, refused: &mut bool) {
        if !statement_contains_yield(&statement) {
            let escapes = !self.loops.is_empty() && escapes_to_flattened_loop(&statement);
            let returns = self.a_return_here_has_work_to_do() && contains_return(&statement);
            if !escapes && !returns {
                self.emit(statement);
                return;
            }
        }
        let span = statement.span();
        match statement {
            Statement::Expression { expression, .. } if expression_contains_yield(&expression) => {
                let value = self.suspend_for_value(expression, diagnostics, refused, span);
                if !is_frame_read(&value) {
                    self.emit(Statement::Expression { expression: value, span });
                }
            }
            Statement::Declaration { kind, mut declarations, span: statement_span }
                if declarations.len() == 1
                    && declarations[0]
                        .init
                        .as_ref()
                        .is_some_and(|init| expression_contains_yield(init)) =>
            {
                let mut declarator = declarations.remove(0);
                let Some(init) = declarator.init.take() else {
                    declarations.insert(0, declarator);
                    self.emit(Statement::Declaration { kind, declarations, span: statement_span });
                    return;
                };
                let resumed_value = self.suspend_for_value(init, diagnostics, refused, span);
                declarator.init = Some(resumed_value);
                let resumed = Statement::Declaration {
                    kind,
                    declarations: crate::vec![declarator],
                    span: statement_span,
                };
                self.no_leftover(&resumed, diagnostics, refused, span);
                self.emit(resumed);
            }
            Statement::Return { argument: Some(value), span: statement_span }
                if expression_contains_yield(&value) =>
            {
                let _ = statement_span;
                let resumed_value = self.suspend_for_value(value, diagnostics, refused, span);
                self.leave_with_return(Some(resumed_value));
            }
            Statement::Return { argument, .. } => {
                if let Some(value) = &argument {
                    if refuse_nested_yields(value, "nested inside a `return` operand", diagnostics) {
                        *refused = true;
                    }
                }
                self.leave_with_return(argument);
            }
            Statement::Block { body, .. } => self.lower_block(body, diagnostics, refused),
            Statement::If { test, consequent, alternate, .. } => {
                if refuse_nested_yields(&test, "in an `if` condition", diagnostics) {
                    *refused = true;
                }
                let otherwise = self.alloc();
                let join = self.alloc();
                let guard = Statement::If {
                    test: negate(test, span),
                    consequent: Box::new(self.edge(otherwise)),
                    alternate: None,
                    span,
                };
                self.emit(guard);
                self.lower(*consequent, diagnostics, refused);
                self.jump(join);
                self.open(otherwise);
                if let Some(alternate) = alternate {
                    self.lower(*alternate, diagnostics, refused);
                }
                self.jump(join);
                self.open(join);
            }
            Statement::While { test, body, .. } => {
                let head = self.alloc();
                let exit = self.alloc();
                self.jump(head);
                self.open(head);
                if refuse_nested_yields(&test, "in a `while` condition", diagnostics) {
                    *refused = true;
                }
                let guard = Statement::If {
                    test: negate(test, span),
                    consequent: Box::new(self.edge(exit)),
                    alternate: None,
                    span,
                };
                self.emit(guard);
                self.loops.push(LoopTargets { next: head, exit , finalizer_depth: self.finalizers.len() });
                self.lower(*body, diagnostics, refused);
                self.loops.pop();
                self.jump(head);
                self.open(exit);
            }
            Statement::DoWhile { body, test, .. } => {
                let entry = self.alloc();
                let next = self.alloc();
                let exit = self.alloc();
                self.jump(entry);
                self.open(entry);
                self.loops.push(LoopTargets { next, exit , finalizer_depth: self.finalizers.len() });
                self.lower(*body, diagnostics, refused);
                self.loops.pop();
                self.jump(next);
                self.open(next);
                if refuse_nested_yields(&test, "in a `do`-`while` condition", diagnostics) {
                    *refused = true;
                }
                let guard = Statement::If {
                    test: negate(test, span),
                    consequent: Box::new(self.edge(exit)),
                    alternate: None,
                    span,
                };
                self.emit(guard);
                self.jump(entry);
                self.open(exit);
            }
            Statement::For { init, test, update, body, .. } => {
                let head = self.alloc();
                let next = self.alloc();
                let exit = self.alloc();
                if let Some(init) = init {
                    let statement = for_init_statement(*init, span);
                    if refuse_nested_yields_in_statement(&statement, "in a `for` initializer", diagnostics) {
                        *refused = true;
                    }
                    self.emit(statement);
                }
                self.jump(head);
                self.open(head);
                if let Some(test) = test {
                    if refuse_nested_yields(&test, "in a `for` condition", diagnostics) {
                        *refused = true;
                    }
                    let guard = Statement::If {
                        test: negate(test, span),
                        consequent: Box::new(self.edge(exit)),
                        alternate: None,
                        span,
                    };
                    self.emit(guard);
                }
                self.loops.push(LoopTargets { next, exit , finalizer_depth: self.finalizers.len() });
                self.lower(*body, diagnostics, refused);
                self.loops.pop();
                self.jump(next);
                self.open(next);
                if let Some(update) = update {
                    if refuse_nested_yields(&update, "in a `for` update", diagnostics) {
                        *refused = true;
                    }
                    self.emit(Statement::Expression { expression: update, span });
                }
                self.jump(head);
                self.open(exit);
            }
            Statement::Try { block, handler: Some(catch), finalizer: None, .. } => {
                let join = self.alloc();
                let guarded = self.alloc();
                let handler_block = self.alloc();
                let outer = self.handler;
                self.handler = Some(handler_block);
                self.jump(guarded);
                self.open(guarded);
                self.lower_block(block, diagnostics, refused);
                self.handler = outer;
                self.jump(join);
                self.open(handler_block);
                if catch.body.iter().any(statement_contains_yield) {
                    self.lower_suspending_catch(catch, handler_block, diagnostics, refused, span);
                    self.jump(join);
                    self.open(join);
                    return;
                }
                let body = catch_body(catch, diagnostics, refused, span);
                if let Some(body) = body {
                    if !self.finalizers.is_empty() && contains_return(&body) {
                        refuse(
                            diagnostics,
                            span,
                            "a `return` inside a `catch` that a `finally` encloses is not in this \
                             profile",
                        );
                        *refused = true;
                    }
                    self.emit(body);
                }
                self.jump(join);
                self.open(join);
            }
            Statement::Try { block, handler, finalizer: Some(finalizer), .. } => {
                let index = self.finalizer_count;
                self.finalizer_count += 1;
                let raise = self.alloc();
                let run = self.alloc();
                let join = self.alloc();

                let outer_handler = self.handler;
                self.handler = Some(raise);
                self.finalizers.push(Finally {
                    run,
                    index,
                    iteration_depth: self.iterations.len(),
                });
                match handler {
                    Some(catch) => self.lower(
                        Statement::Try { block, handler: Some(catch), finalizer: None, span },
                        diagnostics,
                        refused,
                    ),
                    None => self.lower_block(block, diagnostics, refused),
                }
                self.finalizers.pop();
                self.handler = outer_handler;
                self.emit(assign_frame(
                    &reason_slot(index),
                    Expression::Number { value: REASON_NORMAL, span },
                    span,
                ));
                self.jump(run);

                self.open(raise);
                self.emit(assign_frame(
                    &reason_slot(index),
                    Expression::Number { value: REASON_THROW, span },
                    span,
                ));
                self.emit(assign_frame(&value_slot(index), read_frame(THROWN, span), span));
                self.jump(run);

                self.open(run);
                self.lower(Statement::Block { body: finalizer, span }, diagnostics, refused);
                let equals = |reason: f64| Expression::Binary {
                    operator: crate::ast::BinaryOperator::StrictEqual,
                    left: Box::new(read_frame(&reason_slot(index), span)),
                    right: Box::new(Expression::Number { value: reason, span }),
                    span,
                };
                self.emit(Statement::If {
                    test: equals(REASON_THROW),
                    consequent: Box::new(Statement::Throw {
                        argument: read_frame(&value_slot(index), span),
                        span,
                    }),
                    alternate: None,
                    span,
                });
                let value = read_frame(&value_slot(index), span);
                let outer = self.finalizers.last().copied();
                let mut body = self.closes_down_to(outer, span);
                match outer {
                    Some(outer) => body.extend(routed_return(outer, value, span)),
                    None => body.push(Statement::Return { argument: Some(value), span }),
                }
                self.emit(Statement::If {
                    test: equals(REASON_RETURN),
                    consequent: Box::new(Statement::Block { body, span }),
                    alternate: None,
                    span,
                });
                self.jump(join);
                self.open(join);
            }
            Statement::ForOf { left, right, body, .. } => {
                let index = self.iteration_count;
                self.iteration_count += 1;
                if refuse_nested_yields(&right, "in a `for`-`of` subject", diagnostics) {
                    *refused = true;
                }
                let mut in_head = None;
                visit_yields_in_for_head(&left, &mut |span| in_head = in_head.or(Some(span)));
                let suspending_pattern = match (&in_head, &*left) {
                    (Some(_), crate::ast::ForInit::Declaration { .. }) => {
                        refuse(
                            diagnostics,
                            in_head.unwrap_or(span),
                            &unsupported_yield("in a `for`-`of` binding declaration"),
                        );
                        *refused = true;
                        false
                    }
                    (Some(_), _) => true,
                    (None, _) => false,
                };
                self.emit(Statement::Expression {
                    expression: frame_call(
                        ITER_OPEN,
                        crate::vec![Expression::Number { value: index as f64, span }, right],
                        span,
                    ),
                    span,
                });

                let head = self.alloc();
                let next = self.alloc();
                let leave = self.alloc();
                let exit = self.alloc();
                let raise = self.alloc();

                let outer_handler = self.handler;
                self.handler = Some(raise);
                self.iterations.push(index);
                self.jump(head);
                self.open(head);
                let result = iteration_result_slot(index);
                self.emit(assign_frame(
                    &result,
                    frame_call(
                        ITER_STEP,
                        crate::vec![Expression::Number { value: index as f64, span }],
                        span,
                    ),
                    span,
                ));
                self.emit(Statement::If {
                    test: Expression::Binary {
                        operator: crate::ast::BinaryOperator::StrictEqual,
                        left: Box::new(read_frame(&result, span)),
                        right: Box::new(Expression::Boolean { value: false, span }),
                        span,
                    },
                    consequent: Box::new(self.edge(exit)),
                    alternate: None,
                    span,
                });
                let value = frame_call(
                    ITER_VALUE,
                    crate::vec![read_frame(&result, span)],
                    span,
                );
                if suspending_pattern {
                    match head_pattern(*left, span) {
                        Some(pattern) => {
                            self.destructure(pattern, value, diagnostics, refused, span)
                        }
                        None => self.emit(Statement::Expression { expression: value, span }),
                    }
                } else {
                    self.emit(bind_iteration_value(left, value, span));
                }

                self.loops.push(LoopTargets {
                    next,
                    exit: leave,
                    finalizer_depth: self.finalizers.len(),
                });
                self.lower(*body, diagnostics, refused);
                self.loops.pop();
                self.jump(next);
                self.open(next);
                self.jump(head);

                self.iterations.pop();
                self.handler = outer_handler;

                self.open(raise);
                self.emit(close_iteration(index, true, span));
                self.emit(Statement::Throw { argument: read_frame(THROWN, span), span });

                self.open(leave);
                self.emit(close_iteration(index, false, span));
                self.jump(exit);

                self.open(exit);
            }
            Statement::Break { label: None, .. } => match self.loops.last().copied() {
                Some(targets) => self.leave_to(targets.exit, targets, "break", diagnostics, refused),
                None => self.emit(Statement::Break { label: None, span }),
            },
            Statement::Continue { label: None, .. } => match self.loops.last().copied() {
                Some(targets) => {
                    self.leave_to(targets.next, targets, "continue", diagnostics, refused)
                }
                None => self.emit(Statement::Continue { label: None, span }),
            },
            other => {
                refuse(
                    diagnostics,
                    other.span(),
                    &unsupported_yield(statement_position(&other)),
                );
                *refused = true;
                self.emit(other);
            }
        }
    }

    /// `{ FRAME.state = target; continue; }` as one statement, for an `if`'s consequent.
    fn edge(&self, target: usize) -> Statement {
        let span = self.span;
        Statement::Block {
            body: crate::vec![assign_state(target, span), Statement::Continue { label: None, span }],
            span,
        }
    }

    /// **THE SPLIT MUST HAVE CONSUMED EVERY `yield` IN THE STATEMENT, AND THIS ASSERTS IT RATHER
    /// THAN DESCRIBING IT.** A binding target carries expressions of its own -- `var [a = yield 1]
    /// = yield 2` splits on the initializer and leaves one behind -- and a comment claiming
    /// otherwise cannot fail.
    fn no_leftover(
        &self,
        resumed: &Statement,
        diagnostics: &mut Diagnostics,
        refused: &mut bool,
        span: Span,
    ) {
        let mut leftover = false;
        visit_yields_in_statement(resumed, &mut |_| leftover = true);
        if leftover {
            refuse(
                diagnostics,
                span,
                &unsupported_yield("in a binding target's own expression"),
            );
            *refused = true;
        }
    }
}

/// Whether an unlabelled `break` or `continue` inside this statement would reach the loop the
/// transform has flattened, rather than a construct that still exists inside it.
///
/// # THE STATEMENT NEED NOT CONTAIN A `yield` FOR THIS TO MATTER
///
/// `while (c) { yield 1; if (d) break; }` has an `if` with no suspension in it, so the obvious
/// thing is to emit it whole -- and then its `break` breaks out of the dispatch `for (;;)` and
/// finishes the generator. **The edge has to be taken apart even though nothing around it
/// suspends**, which is the one case where "contains no `yield`" is not enough to emit a statement
/// unchanged.
///
/// A `break` inside a nested loop or `switch`, or one carrying a label, belongs to that construct
/// and is left alone -- the construct is still there after the flattening, because only the
/// generator's own control flow was taken apart.
/// Whether a `return` inside this statement belongs to the generator rather than to a function
/// written inside it.
///
/// It exists for one caller: a statement emitted WHOLE under an active `finally` would perform its
/// `return` where it stands and skip the finalizer. Every arm mirrors
/// [`escapes_to_flattened_loop`] -- and mirrors what it does NOT descend into, which is the whole
/// point. A nested `function` or arrow has its own `return`, belonging to itself.
fn contains_return(statement: &Statement) -> bool {
    fn any(body: &[Statement]) -> bool {
        body.iter().any(contains_return)
    }
    match statement {
        Statement::Return { .. } => true,
        Statement::While { body, .. }
        | Statement::DoWhile { body, .. }
        | Statement::For { body, .. }
        | Statement::ForIn { body, .. }
        | Statement::ForOf { body, .. }
        | Statement::Labeled { body, .. }
        | Statement::With { body, .. } => contains_return(body),
        Statement::Switch { cases, .. } => cases.iter().any(|case| any(&case.body)),
        Statement::Block { body, .. } => any(body),
        Statement::If { consequent, alternate, .. } => {
            contains_return(consequent)
                || alternate.as_ref().is_some_and(|s| contains_return(s))
        }
        Statement::Try { block, handler, finalizer, .. } => {
            any(block)
                || handler.as_ref().is_some_and(|c| any(&c.body))
                || finalizer.as_ref().is_some_and(|f| any(f))
        }
        _ => false,
    }
}

fn escapes_to_flattened_loop(statement: &Statement) -> bool {
    fn walk(statement: &Statement, breakable: bool, continuable: bool) -> bool {
        match statement {
            Statement::Break { label: None, .. } => !breakable,
            Statement::Continue { label: None, .. } => !continuable,
            Statement::While { body, .. } | Statement::DoWhile { body, .. } => {
                walk(body, true, true)
            }
            Statement::For { body, .. }
            | Statement::ForIn { body, .. }
            | Statement::ForOf { body, .. } => walk(body, true, true),
            Statement::Switch { cases, .. } => cases
                .iter()
                .any(|case| case.body.iter().any(|s| walk(s, true, continuable))),
            Statement::Block { body, .. } => {
                body.iter().any(|s| walk(s, breakable, continuable))
            }
            Statement::If { consequent, alternate, .. } => {
                walk(consequent, breakable, continuable)
                    || alternate.as_ref().is_some_and(|s| walk(s, breakable, continuable))
            }
            Statement::Labeled { body, .. } => walk(body, breakable, continuable),
            Statement::Try { block, handler, finalizer, .. } => {
                block.iter().any(|s| walk(s, breakable, continuable))
                    || handler
                        .as_ref()
                        .is_some_and(|c| c.body.iter().any(|s| walk(s, breakable, continuable)))
                    || finalizer
                        .as_ref()
                        .is_some_and(|f| f.iter().any(|s| walk(s, breakable, continuable)))
            }
            _ => false,
        }
    }
    walk(statement, false, false)
}

/// Reports every `let` and `const` the flattening would move into a switch case.
///
/// The walk stops where the flattening does: at a nested function, which keeps its own body, and at
/// a construct this transform emits whole. **A statement with no `yield` anywhere inside it is
/// emitted unchanged and keeps its scope**, so its declarations are not this rule's business --
/// which is why an ordinary `if (a) { let x = 1; }` beside a `yield` is still accepted.
fn refuse_lexical_declarations(
    statement: &Statement,
    diagnostics: &mut Diagnostics,
    refused: &mut bool,
) {
    if let Statement::Declaration { kind, span, .. } = statement {
        if !matches!(kind, crate::ast::DeclarationKind::Var) {
            refuse(
                diagnostics,
                *span,
                "a `let` or `const` in a generator body that suspends is not in this profile",
            );
            *refused = true;
        }
    }
    if !statement_contains_yield(statement) {
        return;
    }
    let mut nested = |body: &[Statement]| {
        for statement in body {
            refuse_lexical_declarations(statement, diagnostics, refused);
        }
    };
    match statement {
        Statement::Block { body, .. } => nested(body),
        Statement::If { consequent, alternate, .. } => {
            refuse_lexical_declarations(consequent, diagnostics, refused);
            if let Some(alternate) = alternate {
                refuse_lexical_declarations(alternate, diagnostics, refused);
            }
        }
        Statement::While { body, .. }
        | Statement::DoWhile { body, .. }
        | Statement::For { body, .. } => refuse_lexical_declarations(body, diagnostics, refused),
        _ => {}
    }
    if let Statement::For { init: Some(init), span, .. } = statement {
        if let crate::ast::ForInit::Declaration { kind, .. } = &**init {
            if !matches!(kind, crate::ast::DeclarationKind::Var) {
                refuse(
                    diagnostics,
                    *span,
                    "a `let` or `const` in a generator body that suspends is not in this profile",
                );
                *refused = true;
            }
        }
    }
}

/// The `catch` that routes a thrown exception to the block that handles it, or `None` when no
/// block has one.
///
/// # THE ROUTE IS A TABLE, NOT A STACK
///
/// ```text
///     catch (ERROR) {
///       switch (FRAME.state) {
///         case 3: case 4: FRAME.thrown = ERROR; FRAME.state = 7; continue;
///         case 7:         FRAME.thrown = ERROR; FRAME.state = 9; continue;
///       }
///       throw ERROR;
///     }
/// ```
///
/// Which handler covers a block is settled when the body is read, so nothing has to be pushed or
/// popped while it runs. **It works because `FRAME.state` still holds the number that was
/// dispatched on**: a block assigns the state only at its edges, immediately before leaving, so
/// during the statements that can throw it has not been touched.
///
/// A state with no row falls out of the inner switch and reaches the `throw`, which propagates out
/// of the generator -- and that is also what an exception thrown inside a `catch` block does, since
/// a handler block's own handler is the one enclosing its `try` rather than itself.
///
/// **THE PARAMETER IS UNNAMEABLE for the same reason the frame is.** Nothing in the generated
/// `catch` reads a name from the body, but a program is entitled to a variable called anything, and
/// a shadowed one here would be a program nobody wrote.
fn handler_dispatch(
    handlers: &[Option<usize>],
    span: Span,
) -> Option<crate::ast::CatchClause> {
    if handlers.iter().all(Option::is_none) {
        return None;
    }
    let error = " generator error";
    let mut cases: Vec<SwitchCase> = Vec::new();
    for (block, handler) in handlers.iter().enumerate() {
        let Some(handler) = handler else { continue };
        cases.push(SwitchCase {
            test: Some(number(block, span)),
            body: crate::vec![
                assign_frame(
                    THROWN,
                    Expression::Identifier { name: error.to_string(), span },
                    span,
                ),
                assign_state(*handler, span),
                Statement::Continue { label: None, span },
            ],
            span,
        });
    }
    Some(crate::ast::CatchClause {
        param: Some(Pattern::Identifier { name: error.to_string(), span }),
        body: crate::vec![
            Statement::Switch { discriminant: read_state(span), cases, span },
            Statement::Throw {
                argument: Expression::Identifier { name: error.to_string(), span },
                span,
            },
        ],
        span,
    })
}

/// The `catch` clause as ONE statement: its parameter bound from the frame, then its body verbatim.
///
/// Refused rather than flattened when it suspends. A handler spread across blocks would put its
/// parameter in the switch's shared scope, where `typeof e` after the `try` would find it and a
/// second binding of the name would collide with it -- both observable, and neither is what a
/// `catch` parameter is.
fn catch_body(
    catch: crate::ast::CatchClause,
    diagnostics: &mut Diagnostics,
    refused: &mut bool,
    span: Span,
) -> Option<Statement> {
    let mut body = Vec::new();
    let mut in_param = false;
    if let Some(param) = &catch.param {
        visit_yields_in_pattern(param, &mut |_| in_param = true);
    }
    if in_param {
        refuse(diagnostics, catch.span, &unsupported_yield("in a `catch` parameter"));
        *refused = true;
        return None;
    }
    if let Some(param) = catch.param {
        body.push(Statement::Declaration {
            kind: crate::ast::DeclarationKind::Let,
            declarations: crate::vec![crate::ast::Declarator {
                target: param,
                init: Some(read_frame(THROWN, span)),
                span,
            }],
            span,
        });
    }
    body.extend(catch.body);
    Some(Statement::Block { body, span })
}

/// The statements that hand a return completion to a `finally` instead of performing it: park the
/// value, say why, and jump to the finalizer.
///
/// Free-standing because its two callers are both inside `Machine` and one holds `&self` while the
/// other holds `&mut self` -- and because there is exactly one right order, which a reader should
/// meet once rather than twice.
/// `FRAME.<operation>(<arguments>)` -- a member call on the frame, so the operation reaches it as
/// `this` without the transform having to name it twice.
fn frame_call(operation: &str, arguments: Vec<Expression>, span: Span) -> Expression {
    Expression::Call {
        callee: Box::new(Expression::Member {
            object: Box::new(Expression::Identifier { name: FRAME.to_string(), span }),
            property: Box::new(MemberProperty::Identifier { name: operation.to_string(), span }),
            optional: false,
            span,
        }),
        arguments: arguments
            .into_iter()
            .map(crate::ast::Argument::Expression)
            .collect(),
        optional: false,
        span,
    }
}

/// `FRAME.iterClose(index, quietly);`
fn close_iteration(index: usize, quietly: bool, span: Span) -> Statement {
    Statement::Expression {
        expression: frame_call(
            ITER_CLOSE,
            crate::vec![
                Expression::Number { value: index as f64, span },
                Expression::Boolean { value: quietly, span },
            ],
            span,
        ),
        span,
    }
}

/// What a `for`-`of` head does with each value: declare it, or assign to what was already there.
///
/// A declaration is REBUILT rather than reused, because the head carries a declarator with no
/// initializer and each pass needs the value as one -- which is also what keeps a destructuring
/// pattern working, since the pattern is the declarator's target either way.
fn bind_iteration_value(
    left: Box<crate::ast::ForInit>,
    value: Expression,
    span: Span,
) -> Statement {
    match *left {
        crate::ast::ForInit::Declaration { kind, declarations, span: declaration_span } => {
            let target = declarations
                .into_iter()
                .next()
                .map_or(Pattern::Identifier { name: "undefined".to_string(), span }, |d| d.target);
            Statement::Declaration {
                kind,
                declarations: crate::vec![crate::ast::Declarator {
                    target,
                    init: Some(value),
                    span: declaration_span,
                }],
                span: declaration_span,
            }
        }
        crate::ast::ForInit::Pattern(pattern) => assign_to(pattern, value, span),
        crate::ast::ForInit::Expression(expression) => match refine_target(expression, span) {
            Some(pattern) => assign_to(pattern, value, span),
            None => Statement::Expression { expression: value, span },
        },
    }
}

/// `<pattern> = <value>;`
/// A `for`-`of` head as the assignment pattern it assigns to, for the one case that takes it apart.
///
/// A declaration head never reaches here -- it is refused before the loop is built -- so the two
/// arms left are a head that was already refined into a pattern and one still wearing its
/// expression spelling, which is the same cover node [`bind_iteration_value`] refines.
fn head_pattern(left: crate::ast::ForInit, span: Span) -> Option<Pattern> {
    match left {
        crate::ast::ForInit::Pattern(pattern) => Some(pattern),
        crate::ast::ForInit::Expression(expression) => refine_target(expression, span),
        crate::ast::ForInit::Declaration { .. } => None,
    }
}

fn assign_to(pattern: Pattern, value: Expression, span: Span) -> Statement {
    Statement::Expression {
        expression: Expression::Assignment {
            operator: crate::ast::AssignmentOperator::Assign,
            target: Box::new(crate::ast::AssignmentTarget::Pattern {
                pattern,
                parenthesized: false,
            }),
            value: Box::new(value),
            span,
        },
        span,
    }
}

/// The assignment target an expression head denotes, for the shapes a `for`-`of` head can take.
fn refine_target(expression: Expression, _span: Span) -> Option<Pattern> {
    match expression {
        Expression::Identifier { name, span } => Some(Pattern::Identifier { name, span }),
        Expression::Member { object, property, optional, span } => {
            Some(Pattern::Member { object, property, optional, span })
        }
        _ => None,
    }
}

fn routed_return(active: Finally, value: Expression, span: Span) -> Vec<Statement> {
    crate::vec![
        assign_frame(&value_slot(active.index), value, span),
        assign_frame(
            &reason_slot(active.index),
            Expression::Number { value: REASON_RETURN, span },
            span,
        ),
        assign_state(active.run, span),
        Statement::Continue { label: None, span },
    ]
}

/// `FRAME.<name>`
fn read_frame(name: &str, span: Span) -> Expression {
    Expression::Member {
        object: Box::new(Expression::Identifier { name: FRAME.to_string(), span }),
        property: Box::new(MemberProperty::Identifier { name: name.to_string(), span }),
        optional: false,
        span,
    }
}

/// `FRAME.<name> = <value>;`
fn assign_frame(name: &str, value: Expression, span: Span) -> Statement {
    Statement::Expression {
        expression: Expression::Assignment {
            operator: crate::ast::AssignmentOperator::Assign,
            target: Box::new(crate::ast::AssignmentTarget::Pattern {
                pattern: Pattern::Member {
                    object: Box::new(Expression::Identifier { name: FRAME.to_string(), span }),
                    property: Box::new(MemberProperty::Identifier {
                        name: name.to_string(),
                        span,
                    }),
                    optional: false,
                    span,
                },
                parenthesized: false,
            }),
            value: Box::new(value),
            span,
        },
        span,
    }
}

/// When a logical operator answers its LEFT operand and never evaluates the right.
///
/// **THE THREE DIFFER ONLY HERE, AND `??` IS NOT `||` WITH A DIFFERENT NAME.** `a || b` skips `b`
/// when `a` is truthy; `a ?? b` skips it for everything except `null` and `undefined`, so `0 ?? b`
/// is `0` where `0 || b` is `b`.
fn short_circuits(operator: crate::ast::LogicalOperator, left: Expression, span: Span) -> Expression {
    use crate::ast::LogicalOperator as L;
    let is = |value: Expression, left: Expression| Expression::Binary {
        operator: crate::ast::BinaryOperator::StrictNotEqual,
        left: Box::new(left),
        right: Box::new(value),
        span,
    };
    match operator {
        L::And => negate(left, span),
        L::Or => left,
        L::NullishCoalescing => Expression::Logical {
            operator: L::And,
            left: Box::new(is(Expression::Null { span }, left.clone())),
            right: Box::new(is(undefined(span), left)),
            span,
        },
    }
}

/// `!(test)` -- the guard is written inverted so both arms of a branch become forward edges.
fn negate(test: Expression, span: Span) -> Expression {
    Expression::Unary {
        operator: crate::ast::UnaryOperator::Not,
        argument: Box::new(Expression::Parenthesized { expression: Box::new(test), span }),
        span,
    }
}

/// A `for`'s initializer as a statement, so it can be emitted into a block like any other.
fn for_init_statement(init: crate::ast::ForInit, span: Span) -> Statement {
    match init {
        crate::ast::ForInit::Declaration { kind, declarations, span: declaration_span } => {
            Statement::Declaration { kind, declarations, span: declaration_span }
        }
        crate::ast::ForInit::Expression(expression) => Statement::Expression { expression, span },
        crate::ast::ForInit::Pattern(pattern) => Statement::Expression {
            expression: Expression::Identifier { name: pattern_name(&pattern), span },
            span,
        },
    }
}

fn pattern_name(pattern: &Pattern) -> crate::String {
    match pattern {
        Pattern::Identifier { name, .. } => name.clone(),
        _ => "undefined".to_string(),
    }
}

fn refuse_nested_yields_in_statement(
    statement: &Statement,
    position: &str,
    diagnostics: &mut Diagnostics,
) -> bool {
    let mut found = false;
    visit_yields_in_statement(statement, &mut |span| {
        refuse(diagnostics, span, &unsupported_yield(position));
        found = true;
    });
    found
}

/// `FRAME.suspended = true;`
fn assign_suspended(span: Span) -> Statement {
    Statement::Expression {
        expression: Expression::Assignment {
            operator: crate::ast::AssignmentOperator::Assign,
            target: Box::new(crate::ast::AssignmentTarget::Pattern {
                pattern: Pattern::Member {
                    object: Box::new(Expression::Identifier { name: FRAME.to_string(), span }),
                    property: Box::new(MemberProperty::Identifier {
                        name: SUSPENDED.to_string(),
                        span,
                    }),
                    optional: false,
                    span,
                },
                parenthesized: false,
            }),
            value: Box::new(Expression::Boolean { value: true, span }),
            span,
        },
        span,
    }
}

/// Drops any parentheses wrapping an expression, however many.
fn peel(expression: Expression) -> Expression {
    match expression {
        Expression::Parenthesized { expression, .. } => peel(*expression),
        other => other,
    }
}

fn peel_ref(expression: &Expression) -> &Expression {
    match expression {
        Expression::Parenthesized { expression, .. } => peel_ref(expression),
        other => other,
    }
}

/// Splits an expression standing in VALUE POSITION -- an initializer, a `return` operand, the
/// right-hand side of an assignment -- into the operand to suspend on and what to read on resume.
///
/// **THE EXPRESSION MUST BE EXACTLY A `yield`, NOT MERELY CONTAIN ONE.** `yield x` splits;
/// `(yield x) + 1` does not, because the addition's right operand is evaluated after the
/// suspension while its left is the suspension's own result -- which needs a temporary this slice
/// does not allocate. A caller that reaches here with anything else gets it back unchanged, and
/// the leftover check in [`classify`] then refuses the statement.
fn split_value(
    expression: Expression,
    diagnostics: &mut Diagnostics,
    refused: &mut bool,
    span: Span,
) -> Suspension {
    let _ = span;
    let peeled = peel(expression);
    let Expression::Yield { argument, delegate, span: yield_span } = peeled else {
        return Suspension::None(peeled);
    };
    let argument = argument.map(|argument| *argument);
    if !delegate {
        return Suspension::Plain(argument);
    }
    match argument {
        Some(subject) => Suspension::Delegated(subject),
        None => {
            refuse(diagnostics, yield_span, &unsupported_yield("with no operand after its `*`"));
            *refused = true;
            Suspension::None(undefined(yield_span))
        }
    }
}

/// Which of the three things an expression standing in value position is.
enum Suspension {
    /// Not a suspension at all. The expression comes back peeled and otherwise unchanged, and the
    /// leftover check at the call site refuses it if it merely CONTAINED a `yield`.
    None(Expression),
    /// `yield e` -- suspend on the operand, and the value is whatever the resumption carried in.
    Plain(Option<Expression>),
    /// `yield* e` -- the operand is the thing to iterate, and the value is the delegation's own.
    Delegated(Expression),
}

fn refuse(diagnostics: &mut Diagnostics, span: Span, message: &str) {
    diagnostics.error(Phase::Syntactic, DiagnosticKind::NotInProfile, span, message);
}

/// Reports every `yield` inside an expression, which is a position this slice cannot rewrite.
fn refuse_nested_yields(
    expression: &Expression,
    position: &str,
    diagnostics: &mut Diagnostics,
) -> bool {
    let mut found = false;
    visit_yields_in_expression(expression, &mut |span| {
        refuse(diagnostics, span, &unsupported_yield(position));
        found = true;
    });
    found
}
fn number(value: usize, span: Span) -> Expression {
    Expression::Number { value: value as f64, span }
}

/// `FRAME.state`
fn read_state(span: Span) -> Expression {
    Expression::Member {
        object: Box::new(Expression::Identifier { name: FRAME.to_string(), span }),
        property: Box::new(MemberProperty::Identifier { name: STATE.to_string(), span }),
        optional: false,
        span,
    }
}

/// `FRAME.sent` -- the value the resumption carried in.
fn read_sent(span: Span) -> Expression {
    Expression::Member {
        object: Box::new(Expression::Identifier { name: FRAME.to_string(), span }),
        property: Box::new(MemberProperty::Identifier { name: SENT.to_string(), span }),
        optional: false,
        span,
    }
}

/// `FRAME.state = value;`
///
/// The target is a `Pattern::Member` and not an expression: this engine's assignment targets are
/// REFINED patterns, and a member expression is the one pattern that is an assignment target
/// without ever being a binding.
fn assign_state(value: usize, span: Span) -> Statement {
    Statement::Expression {
        expression: Expression::Assignment {
            operator: crate::ast::AssignmentOperator::Assign,
            target: Box::new(crate::ast::AssignmentTarget::Pattern {
                pattern: Pattern::Member {
                    object: Box::new(Expression::Identifier { name: FRAME.to_string(), span }),
                    property: Box::new(MemberProperty::Identifier {
                        name: STATE.to_string(),
                        span,
                    }),
                    optional: false,
                    span,
                },
                parenthesized: false,
            }),
            value: Box::new(number(value, span)),
            span,
        },
        span,
    }
}


/// Whether a statement contains a `yield` at any depth, NOT counting nested functions.
///
/// A nested function has its own `yield` rules -- an ordinary one may not contain a `yield` at all,
/// and a nested generator's belongs to that generator -- so the walk stops at every function
/// boundary. Descending into one would refuse a legal program and, worse, would make an inner
/// generator's suspension look like an outer one's.
fn statement_contains_yield(statement: &Statement) -> bool {
    let mut found = false;
    visit_yields_in_statement(statement, &mut |_| found = true);
    found
}

fn visit_yields_in_statement(statement: &Statement, report: &mut impl FnMut(Span)) {
    fn expression(e: &Expression, report: &mut impl FnMut(Span)) {
        visit_yields_in_expression(e, report);
    }
    fn block(body: &[Statement], report: &mut impl FnMut(Span)) {
        for statement in body {
            visit_yields_in_statement(statement, report);
        }
    }
    match statement {
        Statement::Expression { expression: e, .. } => expression(e, report),
        Statement::Block { body, .. } => block(body, report),
        Statement::If { test, consequent, alternate, .. } => {
            expression(test, report);
            visit_yields_in_statement(consequent, report);
            if let Some(alternate) = alternate {
                visit_yields_in_statement(alternate, report);
            }
        }
        Statement::While { test, body, .. } => {
            expression(test, report);
            visit_yields_in_statement(body, report);
        }
        Statement::DoWhile { body, test, .. } => {
            visit_yields_in_statement(body, report);
            expression(test, report);
        }
        Statement::For { init, test, update, body, .. } => {
            if let Some(init) = init {
                visit_yields_in_for_init(init, report);
            }
            if let Some(test) = test {
                expression(test, report);
            }
            if let Some(update) = update {
                expression(update, report);
            }
            visit_yields_in_statement(body, report);
        }
        Statement::ForIn { left, right, body, .. }
        | Statement::ForOf { left, right, body, .. } => {
            visit_yields_in_for_init(left, report);
            expression(right, report);
            visit_yields_in_statement(body, report);
        }
        Statement::Return { argument, .. } => {
            if let Some(argument) = argument {
                expression(argument, report);
            }
        }
        Statement::Throw { argument, .. } => expression(argument, report),
        Statement::Try { block: b, handler, finalizer, .. } => {
            block(b, report);
            if let Some(handler) = handler {
                if let Some(param) = &handler.param {
                    visit_yields_in_pattern(param, report);
                }
                block(&handler.body, report);
            }
            if let Some(finalizer) = finalizer {
                block(finalizer, report);
            }
        }
        Statement::Labeled { body, .. } => visit_yields_in_statement(body, report),
        Statement::Switch { discriminant, cases, .. } => {
            expression(discriminant, report);
            for case in cases {
                if let Some(test) = &case.test {
                    expression(test, report);
                }
                block(&case.body, report);
            }
        }
        Statement::Declaration { declarations, .. } => {
            for declarator in declarations {
                visit_yields_in_pattern(&declarator.target, report);
                if let Some(init) = &declarator.init {
                    expression(init, report);
                }
            }
        }
        Statement::With { object, body, .. } => {
            expression(object, report);
            visit_yields_in_statement(body, report);
        }
        Statement::Function(_) => {}
        Statement::Class(class) => visit_yields_in_class(class, report),
        Statement::Empty { .. } | Statement::Break { .. } | Statement::Continue { .. } => {}
        Statement::Debugger { .. } => {}
    }
}

fn visit_yields_in_for_init(init: &crate::ast::ForInit, report: &mut impl FnMut(Span)) {
    match init {
        crate::ast::ForInit::Expression(e) => visit_yields_in_expression(e, report),
        crate::ast::ForInit::Declaration { declarations, .. } => {
            for declarator in declarations {
                visit_yields_in_pattern(&declarator.target, report);
                if let Some(init) = &declarator.init {
                    visit_yields_in_expression(init, report);
                }
            }
        }
        crate::ast::ForInit::Pattern(pattern) => visit_yields_in_pattern(pattern, report),
    }
}

/// A refined assignment target can hold ordinary expressions: a member target's object and its
/// computed key, and a default's value. A `yield` in any of them is a `yield` in an operand.
/// Every `yield` a `for` head's left-hand side contains, whichever of the three shapes it is.
fn visit_yields_in_for_head(left: &crate::ast::ForInit, report: &mut impl FnMut(Span)) {
    match left {
        crate::ast::ForInit::Declaration { declarations, .. } => {
            for declarator in declarations {
                visit_yields_in_pattern(&declarator.target, report);
                if let Some(init) = &declarator.init {
                    visit_yields_in_expression(init, report);
                }
            }
        }
        crate::ast::ForInit::Pattern(pattern) => visit_yields_in_pattern(pattern, report),
        crate::ast::ForInit::Expression(expression) => {
            visit_yields_in_expression(expression, report)
        }
    }
}

fn visit_yields_in_pattern(pattern: &Pattern, report: &mut impl FnMut(Span)) {
    match pattern {
        Pattern::Member { object, property, .. } => {
            visit_yields_in_expression(object, report);
            if let MemberProperty::Computed { expression, .. } = property.as_ref() {
                visit_yields_in_expression(expression, report);
            }
        }
        Pattern::Array { elements, rest, .. } => {
            for element in elements.iter().flatten() {
                visit_yields_in_pattern(element, report);
            }
            if let Some(rest) = rest {
                visit_yields_in_pattern(rest, report);
            }
        }
        Pattern::Object { properties, rest, .. } => {
            for property in properties {
                visit_yields_in_key(&property.key, report);
                visit_yields_in_pattern(&property.value, report);
            }
            if let Some(rest) = rest {
                visit_yields_in_pattern(rest, report);
            }
        }
        Pattern::Default { target, value, .. } => {
            visit_yields_in_pattern(target, report);
            visit_yields_in_expression(value, report);
        }
        Pattern::Rest { argument, .. } => visit_yields_in_pattern(argument, report),
        Pattern::Identifier { .. } => {}
    }
}

/// A COMPUTED key is ordinary code and can hold a `yield`; the other three spellings cannot.
fn visit_yields_in_key(key: &crate::ast::PropertyKey, report: &mut impl FnMut(Span)) {
    if let crate::ast::PropertyKey::Computed { expression, .. } = key {
        visit_yields_in_expression(expression, report);
    }
}

fn visit_yields_in_expression(expression: &Expression, report: &mut impl FnMut(Span)) {
    fn go(e: &Expression, report: &mut impl FnMut(Span)) {
        visit_yields_in_expression(e, report);
    }
    match expression {
        Expression::Yield { argument, span, .. } => {
            report(*span);
            if let Some(argument) = argument {
                visit_yields_in_expression(argument, report);
            }
        }
        Expression::Unary { argument, .. } | Expression::Update { argument, .. } => go(argument, report),
        Expression::Binary { left, right, .. } | Expression::Logical { left, right, .. } => {
            go(left, report);
            go(right, report);
        }
        Expression::Assignment { target, value, .. } => {
            match target.as_ref() {
                crate::ast::AssignmentTarget::Invalid(e) => go(e, report),
                crate::ast::AssignmentTarget::Pattern { pattern, .. } => {
                    visit_yields_in_pattern(pattern, report);
                }
            }
            go(value, report);
        }
        Expression::Conditional { test, consequent, alternate, .. } => {
            go(test, report);
            go(consequent, report);
            go(alternate, report);
        }
        Expression::Call { callee, arguments, .. } | Expression::New { callee, arguments, .. } => {
            go(callee, report);
            for argument in arguments {
                match argument {
                    crate::ast::Argument::Expression(e)
                    | crate::ast::Argument::Spread { argument: e, .. } => go(e, report),
                }
            }
        }
        Expression::Member { object, property, .. } => {
            go(object, report);
            if let MemberProperty::Computed { expression, .. } = property.as_ref() {
                go(expression, report);
            }
        }
        Expression::Sequence { expressions, .. } => {
            for e in expressions {
                go(e, report);
            }
        }
        Expression::Parenthesized { expression, .. } => go(expression, report),
        Expression::Array { elements, .. } => {
            for element in elements {
                match element {
                    crate::ast::ArrayElement::Expression(e)
                    | crate::ast::ArrayElement::Spread { argument: e, .. } => go(e, report),
                    crate::ast::ArrayElement::Hole => {}
                }
            }
        }
        Expression::Object { properties, .. } => {
            for property in properties {
                match property {
                    crate::ast::ObjectProperty::Property { key, value, .. } => {
                        visit_yields_in_key(key, report);
                        go(value, report);
                    }
                    crate::ast::ObjectProperty::Spread { argument, .. } => go(argument, report),
                    crate::ast::ObjectProperty::CoverInitializedName { value, .. } => go(value, report),
                    crate::ast::ObjectProperty::Method { key, .. } => {
                        visit_yields_in_key(key, report);
                    }
                }
            }
        }
        Expression::Template { expressions, .. } => {
            for e in expressions {
                go(e, report);
            }
        }
        Expression::Tagged { tag, quasi, .. } => {
            go(tag, report);
            go(quasi, report);
        }
        Expression::Identifier { .. }
        | Expression::Number { .. }
        | Expression::String { .. }
        | Expression::Boolean { .. }
        | Expression::Null { .. }
        | Expression::This { .. }
        | Expression::RegExp { .. }
        | Expression::Super { .. }
        | Expression::NewTarget { .. }
        | Expression::Function(_) => {}
        Expression::Arrow(arrow) => {
            for parameter in &arrow.params {
                visit_yields_in_pattern(parameter, report);
            }
        }
        Expression::Class(class) => visit_yields_in_class(class, report),
    }
}

/// The parts of a class that are evaluated in the ENCLOSING context: its heritage and every
/// computed member key. Member bodies are function boundaries and are not walked.
fn visit_yields_in_class(class: &crate::ast::Class, report: &mut impl FnMut(Span)) {
    if let Some(heritage) = &class.heritage {
        visit_yields_in_expression(heritage, report);
    }
    for member in &class.members {
        visit_yields_in_key(&member.key, report);
    }
}
