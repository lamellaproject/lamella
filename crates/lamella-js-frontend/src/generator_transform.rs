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
    for statement in &function.body {
        if let Some(yield_expression) = bare_yield(statement) {
            if let Expression::Yield { delegate: true, span, .. } = yield_expression {
                refuse(diagnostics, *span, "a `yield*` expression is not in this profile");
                refused = true;
            }
            if let Expression::Yield { argument: Some(argument), .. } = yield_expression {
                refused |= refuse_nested_yields(argument, diagnostics);
            }
        } else if statement_contains_yield(statement) {
            refuse(
                diagnostics,
                statement.span(),
                "a `yield` inside an expression, a loop or a `try` is not in this profile",
            );
            refused = true;
        }
    }
    if refused {
        return;
    }
    let yields = function.body.iter().filter(|s| bare_yield(s).is_some()).count();
    if yields == 0 {
        return;
    }
    function.body = build_machine(core::mem::take(&mut function.body), function.span);
}

/// The `yield` a statement IS, when the statement is nothing but one.
///
/// `yield x;` is an expression statement whose whole expression is a `Yield`. `f(yield x);` and
/// `var a = yield x;` are not -- the yield is an OPERAND there, which needs the spill that this
/// slice does not have.
fn bare_yield(statement: &Statement) -> Option<&Expression> {
    match statement {
        Statement::Expression { expression: expression @ Expression::Yield { .. }, .. } => {
            Some(expression)
        }
        _ => None,
    }
}

fn refuse(diagnostics: &mut Diagnostics, span: Span, message: &str) {
    diagnostics.error(Phase::Syntactic, DiagnosticKind::NotInProfile, span, message);
}

/// Reports every `yield` inside an expression, which is a position this slice cannot rewrite.
fn refuse_nested_yields(expression: &Expression, diagnostics: &mut Diagnostics) -> bool {
    let mut found = false;
    visit_yields_in_expression(expression, &mut |span| {
        refuse(
            diagnostics,
            span,
            "a `yield` inside an expression, a loop or a `try` is not in this profile",
        );
        found = true;
    });
    found
}

/// Builds the dispatch. See the module note for why an original `return` is left untouched.
fn build_machine(body: Vec<Statement>, span: Span) -> Vec<Statement> {
    let mut cases: Vec<SwitchCase> = Vec::new();
    let mut current: Vec<Statement> = Vec::new();
    let mut index: usize = 0;
    for statement in body {
        match bare_yield(&statement) {
            None => current.push(statement),
            Some(_) => {
                let Statement::Expression { expression: Expression::Yield { argument, .. }, .. } =
                    statement
                else {
                    continue;
                };
                current.push(assign_state(index + 1, span));
                current.push(Statement::Return {
                    argument: argument.map(|argument| *argument),
                    span,
                });
                cases.push(SwitchCase {
                    test: Some(number(index, span)),
                    body: core::mem::take(&mut current),
                    span,
                });
                index += 1;
            }
        }
    }
    cases.push(SwitchCase { test: Some(number(index, span)), body: current, span });
    crate::vec![Statement::Switch {
        discriminant: read_state(span),
        cases,
        span,
    }]
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
