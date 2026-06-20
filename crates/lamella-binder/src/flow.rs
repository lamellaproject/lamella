//! Flow analysis (ECMA-334 1st ed, clause 12).

use crate::bound::{BoundExpr, BoundExprKind};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::statement::{BoundStmt, BoundStmtKind, BoundSwitchLabel};
use crate::symbols::{Model, TypeKind};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use lamella_syntax::ast::{AssignmentOperator, Literal, UnaryOperator};
use lamella_syntax::span::Span;

/// Whether executing `stmt` always transfers control away rather than reaching
/// its endpoint -- the basis for `CS0161`.
#[must_use]
pub fn always_exits(stmt: &BoundStmt) -> bool {
    use BoundStmtKind as Kind;
    match &stmt.kind {
        Kind::Return(_) | Kind::Throw(_) => true,
        Kind::Block(statements) => statements.iter().any(always_exits),
        Kind::If {
            then_branch,
            else_branch: Some(else_branch),
            ..
        } => always_exits(then_branch) && always_exits(else_branch),
        Kind::While { condition, .. } => is_const_true(condition),
        Kind::For { condition, .. } => condition.as_ref().is_none_or(is_const_true),
        Kind::DoWhile { body, condition } => always_exits(body) || is_const_true(condition),
        Kind::Lock { body, .. } | Kind::Using { body, .. } => always_exits(body),
        Kind::Checked(inner) | Kind::Unchecked(inner) => always_exits(inner),
        Kind::Labeled { body, .. } => always_exits(body),
        Kind::Try {
            body,
            catches,
            finally,
        } => {
            finally.as_ref().is_some_and(|block| always_exits(block))
                || (always_exits(body) && catches.iter().all(|catch| always_exits(&catch.body)))
        }
        Kind::Switch { sections, .. } => {
            let has_default = sections
                .iter()
                .any(|section| section.labels.contains(&BoundSwitchLabel::Default));
            has_default
                && sections
                    .iter()
                    .all(|section| section.statements.iter().any(always_exits))
        }
        _ => false,
    }
}

/// Whether an expression is the constant `true`.
fn is_const_true(expr: &BoundExpr) -> bool {
    matches!(&expr.kind, BoundExprKind::Literal(Literal::Boolean(true)))
}

/// Whether an expression is the constant `false`.
fn is_const_false(expr: &BoundExpr) -> bool {
    matches!(&expr.kind, BoundExprKind::Literal(Literal::Boolean(false)))
}

/// The set of locals definitely assigned at a program point.
type Assigned = BTreeSet<Box<str>>;

/// The flow that leaves a statement: it either reaches its endpoint with a given
/// definitely-assigned set, or transfers control away (and the endpoint is
/// unreachable).
enum Flow {
    Reaches(Assigned),
    Exits,
}

/// Reports `CS0168`/`CS0219` for a local whose name never appears anywhere in the
/// body. Counting *every* occurrence -- even an assignment target -- as a use makes
/// this a safe subset of csc (it under-reports rather than risk a false warning): it
/// fires only when a declared local is truly never referenced again. `CS0219` when
/// the local has an initializer (its assigned value is unused), `CS0168` otherwise.
#[must_use]
pub fn check_unused_locals(body: &BoundStmt, also_used: &BTreeSet<Box<str>>) -> Vec<Diagnostic> {
    let mut used: BTreeSet<Box<str>> = also_used.clone();
    let mut declared: Vec<(Box<str>, Span, bool)> = Vec::new();
    collect_locals(body, &mut used, &mut declared);
    declared
        .into_iter()
        .filter(|(name, _, _)| !used.contains(name))
        .map(|(name, span, has_initializer)| {
            let kind = if has_initializer {
                DiagnosticKind::UnusedLocalValue { name }
            } else {
                DiagnosticKind::UnusedLocal { name }
            };
            Diagnostic::new(kind, span)
        })
        .collect()
}

/// Records every declared local (with its span and whether it has an initializer)
/// and gathers every local name that appears anywhere in `stmt`.
fn collect_locals(
    stmt: &BoundStmt,
    used: &mut BTreeSet<Box<str>>,
    declared: &mut Vec<(Box<str>, Span, bool)>,
) {
    match &stmt.kind {
        BoundStmtKind::Local { ty, declarators } => {
            let report = !ty.is_error();
            for declarator in declarators {
                if report {
                    declared.push((
                        declarator.name.clone(),
                        stmt.span,
                        declarator.initializer.is_some(),
                    ));
                }
                if let Some(initializer) = &declarator.initializer {
                    collect_uses(initializer, used);
                }
            }
        }
        BoundStmtKind::Empty
        | BoundStmtKind::Error
        | BoundStmtKind::Break
        | BoundStmtKind::Continue
        | BoundStmtKind::Goto => {}
        BoundStmtKind::Block(statements) => {
            for statement in statements {
                collect_locals(statement, used, declared);
            }
        }
        BoundStmtKind::Expression(expr) => collect_uses(expr, used),
        BoundStmtKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_uses(condition, used);
            collect_locals(then_branch, used, declared);
            if let Some(else_branch) = else_branch {
                collect_locals(else_branch, used, declared);
            }
        }
        BoundStmtKind::While { condition, body } | BoundStmtKind::DoWhile { condition, body } => {
            collect_uses(condition, used);
            collect_locals(body, used, declared);
        }
        BoundStmtKind::For {
            initializer,
            condition,
            iterators,
            body,
        } => {
            for statement in initializer {
                collect_locals(statement, used, declared);
            }
            if let Some(condition) = condition {
                collect_uses(condition, used);
            }
            for iterator in iterators {
                collect_uses(iterator, used);
            }
            collect_locals(body, used, declared);
        }
        BoundStmtKind::ForEach {
            collection, body, ..
        } => {
            collect_uses(collection, used);
            collect_locals(body, used, declared);
        }
        BoundStmtKind::Return(value) | BoundStmtKind::Throw(value) => {
            if let Some(value) = value {
                collect_uses(value, used);
            }
        }
        BoundStmtKind::Try {
            body,
            catches,
            finally,
        } => {
            collect_locals(body, used, declared);
            for catch in catches {
                collect_locals(&catch.body, used, declared);
            }
            if let Some(finally) = finally {
                collect_locals(finally, used, declared);
            }
        }
        BoundStmtKind::Switch {
            expression,
            sections,
        } => {
            collect_uses(expression, used);
            for section in sections {
                for statement in &section.statements {
                    collect_locals(statement, used, declared);
                }
            }
        }
        BoundStmtKind::Lock { expression, body } => {
            collect_uses(expression, used);
            collect_locals(body, used, declared);
        }
        BoundStmtKind::Using { resource, body } => {
            for statement in resource {
                collect_locals(statement, used, declared);
            }
            collect_locals(body, used, declared);
        }
        BoundStmtKind::Checked(inner)
        | BoundStmtKind::Unchecked(inner)
        | BoundStmtKind::Labeled { body: inner, .. } => collect_locals(inner, used, declared),
    }
}

/// Gathers every local name that appears anywhere in `expr` (an assignment target is
/// counted too, which only makes the unused check more conservative). Also used by
/// the binder to record locals referenced in `switch` case-label expressions.
pub(crate) fn collect_uses(expr: &BoundExpr, used: &mut BTreeSet<Box<str>>) {
    match &expr.kind {
        BoundExprKind::Local(name) => {
            used.insert(name.clone());
        }
        BoundExprKind::Literal(_)
        | BoundExprKind::This
        | BoundExprKind::Base
        | BoundExprKind::TypeReference(_)
        | BoundExprKind::NamespaceReference(_)
        | BoundExprKind::TypeOf
        | BoundExprKind::Error => {}
        BoundExprKind::FieldAccess { receiver, .. }
        | BoundExprKind::PropertyAccess { receiver, .. }
        | BoundExprKind::MethodGroup { receiver, .. } => collect_uses(receiver, used),
        BoundExprKind::Call {
            callee, arguments, ..
        } => {
            collect_uses(callee, used);
            for argument in arguments {
                collect_uses(argument, used);
            }
        }
        BoundExprKind::ElementAccess { receiver, indices } => {
            collect_uses(receiver, used);
            for index in indices {
                collect_uses(index, used);
            }
        }
        BoundExprKind::ArrayCreation { lengths } => {
            for length in lengths {
                collect_uses(length, used);
            }
        }
        BoundExprKind::ObjectCreation { arguments, .. } => {
            for argument in arguments {
                collect_uses(argument, used);
            }
        }
        BoundExprKind::DelegateCreation { receiver, .. } => {
            if let Some(receiver) = receiver {
                collect_uses(receiver, used);
            }
        }
        BoundExprKind::Binary { left, right, .. } => {
            collect_uses(left, used);
            collect_uses(right, used);
        }
        BoundExprKind::Unary { operand, .. } | BoundExprKind::Postfix { operand, .. } => {
            collect_uses(operand, used);
        }
        BoundExprKind::Cast { operand, .. }
        | BoundExprKind::TypeTest { operand, .. }
        | BoundExprKind::Conversion { operand, .. } => collect_uses(operand, used),
        BoundExprKind::Checked(inner) | BoundExprKind::Unchecked(inner) => {
            collect_uses(inner, used);
        }
        BoundExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => {
            collect_uses(condition, used);
            collect_uses(when_true, used);
            collect_uses(when_false, used);
        }
        BoundExprKind::Assignment { target, value, .. } => {
            collect_uses(target, used);
            collect_uses(value, used);
        }
    }
}

/// Reports `CS0162` for the first statement in each block whose start point cannot be
/// reached (8.1). This is deliberately conservative: a statement is flagged only when
/// control *definitely* cannot reach it -- after a `return`/`throw`/`break`/
/// `continue`/`goto`, after a constant-true loop with no `break` that targets it, or
/// after an `if` all of whose branches leave. Constant `if` conditions, `switch`
/// ends, and `try` ends are treated as reachable, so the analysis under-reports
/// rather than risk flagging reachable code.
#[must_use]
pub fn check_unreachable(body: &BoundStmt) -> Vec<Diagnostic> {
    let mut check = Unreachable {
        diagnostics: Vec::new(),
    };
    check.statement(body);
    check.diagnostics
}

struct Unreachable {
    diagnostics: Vec<Diagnostic>,
}

impl Unreachable {
    /// Processes a statement list, flagging only the first unreachable statement (as
    /// csc does). Returns whether control can reach the end of the list.
    fn block(&mut self, statements: &[BoundStmt]) -> bool {
        let mut reachable = true;
        for statement in statements {
            if !reachable {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::UnreachableCode,
                    statement.span,
                ));
                return false;
            }
            reachable = self.statement(statement);
        }
        reachable
    }

    /// Whether control can reach the end point of `stmt`, given its start is reached.
    fn statement(&mut self, stmt: &BoundStmt) -> bool {
        use BoundStmtKind as Kind;
        match &stmt.kind {
            Kind::Return(_) | Kind::Throw(_) | Kind::Break | Kind::Continue | Kind::Goto => false,
            Kind::Expression(_) | Kind::Local { .. } | Kind::Empty | Kind::Error => true,
            Kind::Block(statements) => self.block(statements),
            Kind::If {
                then_branch,
                else_branch,
                ..
            } => {
                let then_reaches = self.statement(then_branch);
                let else_reaches = match else_branch {
                    Some(else_branch) => self.statement(else_branch),
                    None => true,
                };
                then_reaches || else_reaches
            }
            Kind::While { condition, body } => {
                self.statement(body);
                !is_const_true(condition) || loop_breaks(body)
            }
            Kind::For {
                condition, body, ..
            } => {
                self.statement(body);
                let endless = condition.as_ref().is_none_or(is_const_true);
                !endless || loop_breaks(body)
            }
            Kind::ForEach { body, .. } | Kind::DoWhile { body, .. } => {
                self.statement(body);
                true
            }
            Kind::Switch { sections, .. } => {
                for section in sections {
                    self.block(&section.statements);
                }
                true
            }
            Kind::Try {
                body,
                catches,
                finally,
            } => {
                self.statement(body);
                for catch in catches {
                    self.statement(&catch.body);
                }
                if let Some(finally) = finally {
                    self.statement(finally);
                }
                true
            }
            Kind::Lock { body, .. } | Kind::Using { body, .. } => self.statement(body),
            Kind::Checked(inner) | Kind::Unchecked(inner) | Kind::Labeled { body: inner, .. } => {
                self.statement(inner)
            }
        }
    }
}

/// Whether `stmt` contains a `break` that targets the immediately enclosing loop --
/// that is, one not captured by a nested loop or `switch`. Over-approximates (treats
/// anything it is unsure about as not-a-break by structure, but never misses a break
/// reachable through `if`/`try`/`block`), so an endless loop is only declared endless
/// when it truly has no escaping `break`.
fn loop_breaks(stmt: &BoundStmt) -> bool {
    use BoundStmtKind as Kind;
    match &stmt.kind {
        Kind::Break => true,
        Kind::While { .. }
        | Kind::DoWhile { .. }
        | Kind::For { .. }
        | Kind::ForEach { .. }
        | Kind::Switch { .. } => false,
        Kind::Block(statements) => statements.iter().any(loop_breaks),
        Kind::If {
            then_branch,
            else_branch,
            ..
        } => {
            loop_breaks(then_branch)
                || else_branch
                    .as_ref()
                    .is_some_and(|branch| loop_breaks(branch))
        }
        Kind::Try {
            body,
            catches,
            finally,
        } => {
            loop_breaks(body)
                || catches.iter().any(|catch| loop_breaks(&catch.body))
                || finally.as_ref().is_some_and(|finally| loop_breaks(finally))
        }
        Kind::Lock { body, .. } | Kind::Using { body, .. } => loop_breaks(body),
        Kind::Checked(inner) | Kind::Unchecked(inner) | Kind::Labeled { body: inner, .. } => {
            loop_breaks(inner)
        }
        Kind::Return(_)
        | Kind::Throw(_)
        | Kind::Continue
        | Kind::Goto
        | Kind::Expression(_)
        | Kind::Local { .. }
        | Kind::Empty
        | Kind::Error => false,
    }
}

/// Reports `CS0165` for every read of a local that is not definitely assigned on
/// all paths to it (clause 12, Annex A). `parameters` start definitely assigned.
/// `model` distinguishes a struct (whose field assignment assigns the local) from a
/// reference type (whose field assignment reads the local).
#[must_use]
pub fn check_definite_assignment(
    body: &BoundStmt,
    parameters: &[Box<str>],
    model: &Model,
) -> Vec<Diagnostic> {
    let mut analyzer = Analyzer {
        diagnostics: Vec::new(),
        model,
    };
    let assigned: Assigned = parameters.iter().cloned().collect();
    analyzer.statement(body, assigned);
    analyzer.diagnostics
}

struct Analyzer<'a> {
    diagnostics: Vec<Diagnostic>,
    model: &'a Model,
}

impl Analyzer<'_> {
    /// Whether `ty` is a struct, whose fields are assigned in place (12.x).
    fn is_struct(&self, ty: &TypeSymbol) -> bool {
        self.model
            .get_by_symbol(ty)
            .is_some_and(|info| info.kind == TypeKind::Struct)
    }

    fn statement(&mut self, stmt: &BoundStmt, assigned: Assigned) -> Flow {
        let span = stmt.span;
        match &stmt.kind {
            BoundStmtKind::Empty | BoundStmtKind::Error => Flow::Reaches(assigned),
            BoundStmtKind::Block(statements) => self.block(statements, assigned),
            BoundStmtKind::Local { declarators, .. } => {
                let mut assigned = assigned;
                for declarator in declarators {
                    if let Some(initializer) = &declarator.initializer {
                        self.expression(initializer, &mut assigned, span);
                        assigned.insert(declarator.name.clone());
                    }
                }
                Flow::Reaches(assigned)
            }
            BoundStmtKind::Expression(expr) => {
                let mut assigned = assigned;
                self.expression(expr, &mut assigned, span);
                Flow::Reaches(assigned)
            }
            BoundStmtKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let mut assigned = assigned;
                self.expression(condition, &mut assigned, span);
                let then_flow = self.statement(then_branch, assigned.clone());
                let else_flow = match else_branch {
                    Some(branch) => self.statement(branch, assigned.clone()),
                    None => Flow::Reaches(assigned),
                };
                if is_const_true(condition) {
                    then_flow
                } else if is_const_false(condition) {
                    else_flow
                } else {
                    merge(then_flow, else_flow)
                }
            }
            BoundStmtKind::While { condition, body } => {
                let mut assigned = assigned;
                self.expression(condition, &mut assigned, span);
                self.statement(body, assigned.clone());
                if is_const_true(condition) {
                    Flow::Exits
                } else {
                    Flow::Reaches(assigned)
                }
            }
            BoundStmtKind::DoWhile { body, condition } => match self.statement(body, assigned) {
                Flow::Exits => Flow::Exits,
                Flow::Reaches(mut assigned) => {
                    self.expression(condition, &mut assigned, span);
                    if is_const_true(condition) {
                        Flow::Exits
                    } else {
                        Flow::Reaches(assigned)
                    }
                }
            },
            BoundStmtKind::For {
                initializer,
                condition,
                iterators,
                body,
            } => {
                let mut assigned = assigned;
                for init in initializer {
                    match self.statement(init, assigned) {
                        Flow::Reaches(set) => assigned = set,
                        Flow::Exits => return Flow::Exits,
                    }
                }
                let infinite = match condition {
                    Some(condition) => {
                        self.expression(condition, &mut assigned, span);
                        is_const_true(condition)
                    }
                    None => true,
                };
                self.statement(body, assigned.clone());
                for iterator in iterators {
                    let mut iterator_set = assigned.clone();
                    self.expression(iterator, &mut iterator_set, span);
                }
                if infinite {
                    Flow::Exits
                } else {
                    Flow::Reaches(assigned)
                }
            }
            BoundStmtKind::ForEach {
                name,
                collection,
                body,
                ..
            } => {
                let mut assigned = assigned;
                self.expression(collection, &mut assigned, span);
                let mut body_set = assigned.clone();
                body_set.insert(name.clone());
                self.statement(body, body_set);
                Flow::Reaches(assigned)
            }
            BoundStmtKind::Return(value) | BoundStmtKind::Throw(value) => {
                if let Some(value) = value {
                    let mut assigned = assigned;
                    self.expression(value, &mut assigned, span);
                }
                Flow::Exits
            }
            BoundStmtKind::Break | BoundStmtKind::Continue | BoundStmtKind::Goto => Flow::Exits,
            BoundStmtKind::Switch {
                expression,
                sections,
            } => {
                let mut assigned = assigned;
                self.expression(expression, &mut assigned, span);
                for section in sections {
                    self.block(&section.statements, assigned.clone());
                }
                Flow::Reaches(assigned)
            }
            BoundStmtKind::Try {
                body,
                catches,
                finally,
            } => {
                self.statement(body, assigned.clone());
                for catch in catches {
                    let mut catch_set = assigned.clone();
                    if let Some(name) = &catch.name {
                        catch_set.insert(name.clone());
                    }
                    self.statement(&catch.body, catch_set);
                }
                match finally {
                    Some(finally) => self.statement(finally, assigned),
                    None => Flow::Reaches(assigned),
                }
            }
            BoundStmtKind::Lock { expression, body } => {
                let mut assigned = assigned;
                self.expression(expression, &mut assigned, span);
                self.statement(body, assigned)
            }
            BoundStmtKind::Using { resource, body } => {
                let mut assigned = assigned;
                for statement in resource {
                    match self.statement(statement, assigned) {
                        Flow::Reaches(set) => assigned = set,
                        Flow::Exits => return Flow::Exits,
                    }
                }
                self.statement(body, assigned)
            }
            BoundStmtKind::Checked(inner) | BoundStmtKind::Unchecked(inner) => {
                self.statement(inner, assigned)
            }
            BoundStmtKind::Labeled { body, .. } => self.statement(body, assigned),
        }
    }

    fn block(&mut self, statements: &[BoundStmt], assigned: Assigned) -> Flow {
        let mut assigned = assigned;
        for statement in statements {
            match self.statement(statement, assigned) {
                Flow::Reaches(set) => assigned = set,
                Flow::Exits => return Flow::Exits,
            }
        }
        Flow::Reaches(assigned)
    }

    /// Walks an expression left to right, reporting a read of an unassigned local
    /// and threading the assignments it makes.
    fn expression(&mut self, expr: &BoundExpr, assigned: &mut Assigned, span: Span) {
        match &expr.kind {
            BoundExprKind::Local(name) => {
                if !assigned.contains(name) {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::UseOfUnassignedLocal { name: name.clone() },
                        span,
                    ));
                }
            }
            BoundExprKind::Literal(_)
            | BoundExprKind::This
            | BoundExprKind::Base
            | BoundExprKind::TypeReference(_)
            | BoundExprKind::NamespaceReference(_)
            | BoundExprKind::TypeOf
            | BoundExprKind::Error => {}
            BoundExprKind::FieldAccess { receiver, .. }
            | BoundExprKind::PropertyAccess { receiver, .. }
            | BoundExprKind::MethodGroup { receiver, .. } => {
                self.expression(receiver, assigned, span);
            }
            BoundExprKind::Call {
                callee, arguments, ..
            } => {
                self.expression(callee, assigned, span);
                for argument in arguments {
                    self.expression(argument, assigned, span);
                }
            }
            BoundExprKind::ElementAccess { receiver, indices } => {
                self.expression(receiver, assigned, span);
                for index in indices {
                    self.expression(index, assigned, span);
                }
            }
            BoundExprKind::ArrayCreation { lengths } => {
                for length in lengths {
                    self.expression(length, assigned, span);
                }
            }
            BoundExprKind::ObjectCreation { arguments, .. } => {
                for argument in arguments {
                    self.expression(argument, assigned, span);
                }
            }
            BoundExprKind::DelegateCreation { receiver, .. } => {
                if let Some(receiver) = receiver {
                    self.expression(receiver, assigned, span);
                }
            }
            BoundExprKind::Binary { left, right, .. } => {
                self.expression(left, assigned, span);
                self.expression(right, assigned, span);
            }
            BoundExprKind::Unary { operator, operand } => {
                self.expression(operand, assigned, span);
                if matches!(
                    operator,
                    UnaryOperator::PreIncrement | UnaryOperator::PreDecrement
                ) {
                    if let BoundExprKind::Local(name) = &operand.kind {
                        assigned.insert(name.clone());
                    }
                }
            }
            BoundExprKind::Postfix { operand, .. } => {
                self.expression(operand, assigned, span);
                if let BoundExprKind::Local(name) = &operand.kind {
                    assigned.insert(name.clone());
                }
            }
            BoundExprKind::Cast { operand, .. }
            | BoundExprKind::TypeTest { operand, .. }
            | BoundExprKind::Conversion { operand, .. } => {
                self.expression(operand, assigned, span);
            }
            BoundExprKind::Checked(inner) | BoundExprKind::Unchecked(inner) => {
                self.expression(inner, assigned, span);
            }
            BoundExprKind::Conditional {
                condition,
                when_true,
                when_false,
            } => {
                self.expression(condition, assigned, span);
                self.expression(when_true, assigned, span);
                self.expression(when_false, assigned, span);
            }
            BoundExprKind::Assignment {
                operator,
                target,
                value,
            } => self.assignment(*operator, target, value, assigned, span),
        }
    }

    fn assignment(
        &mut self,
        operator: AssignmentOperator,
        target: &BoundExpr,
        value: &BoundExpr,
        assigned: &mut Assigned,
        span: Span,
    ) {
        if matches!(operator, AssignmentOperator::Assign) {
            self.expression(value, assigned, span);
            match &target.kind {
                BoundExprKind::Local(name) => {
                    assigned.insert(name.clone());
                }
                BoundExprKind::FieldAccess { receiver, .. } => match &receiver.kind {
                    BoundExprKind::Local(name) if self.is_struct(&receiver.ty) => {
                        assigned.insert(name.clone());
                    }
                    _ => self.expression(target, assigned, span),
                },
                _ => self.expression(target, assigned, span),
            }
        } else {
            match &target.kind {
                BoundExprKind::Local(name) => {
                    if !assigned.contains(name) {
                        self.diagnostics.push(Diagnostic::new(
                            DiagnosticKind::UseOfUnassignedLocal { name: name.clone() },
                            span,
                        ));
                    }
                    self.expression(value, assigned, span);
                    assigned.insert(name.clone());
                }
                _ => {
                    self.expression(target, assigned, span);
                    self.expression(value, assigned, span);
                }
            }
        }
    }
}

/// Merges the flow of two branches: the endpoint is reachable if either branch
/// reaches it, with only the locals both branches assign.
fn merge(left: Flow, right: Flow) -> Flow {
    match (left, right) {
        (Flow::Exits, other) | (other, Flow::Exits) => other,
        (Flow::Reaches(left), Flow::Reaches(right)) => {
            Flow::Reaches(left.intersection(&right).cloned().collect())
        }
    }
}
