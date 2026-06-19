//! Flow analysis (ECMA-334 1st ed, clause 12).

use crate::bound::{BoundExpr, BoundExprKind};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::statement::{BoundStmt, BoundStmtKind, BoundSwitchLabel};
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

/// Reports `CS0165` for every read of a local that is not definitely assigned on
/// all paths to it (clause 12, Annex A). `parameters` start definitely assigned.
#[must_use]
pub fn check_definite_assignment(body: &BoundStmt, parameters: &[Box<str>]) -> Vec<Diagnostic> {
    let mut analyzer = Analyzer {
        diagnostics: Vec::new(),
    };
    let assigned: Assigned = parameters.iter().cloned().collect();
    analyzer.statement(body, assigned);
    analyzer.diagnostics
}

struct Analyzer {
    diagnostics: Vec<Diagnostic>,
}

impl Analyzer {
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
            BoundExprKind::Cast { operand }
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
