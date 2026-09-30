//! Spilling for a `stackalloc` nested in an expression (C# 8.0) that is lowered to `localloc`.
//!
//! `localloc` requires an evaluation stack empty apart from its size (ECMA-335 III.3.47), and a
//! nested `stackalloc` is reached with whatever the enclosing expression evaluated before it
//! still pushed: `_device.Write(stackalloc byte[2])` has the receiver there. So, as csc does, a
//! statement holding one is rewritten: each operand evaluated before the `stackalloc` is stored in
//! a temporary first, the span is built into a temporary of its own, and the statement then reads
//! both. An operand that is an address -- a value type's receiver variable, a `ref` argument --
//! or `this`, a type or a constant is left in place, since reading it later reads the same thing.
//!
//! Only the `localloc` lowering needs this; the array one (a span type with no `(void*, int)`
//! constructor) has no such constraint, and a body with no `localloc` is left untouched.

use crate::expr::EmitError;
use crate::tokens::Tokens;
use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use lamella_binder::statement::BoundDeclarator;
use lamella_binder::{BoundExpr, BoundExprKind, BoundStmt, BoundStmtKind, TypeSymbol};
use lamella_syntax::ast::{AssignmentOperator, BinaryOperator};
use lamella_syntax::span::Span;

/// `body` with every nested `localloc` spilled, or `None` when it holds no `localloc` at all.
pub(crate) fn spill_stackallocs(
    body: &BoundStmt,
    tokens: &Tokens,
) -> Result<Option<BoundStmt>, EmitError> {
    let mut any = false;
    crate::awaitlower::visit_stmt_exprs(body, &mut |expr| any |= is_localloc(expr));
    if !any {
        return Ok(None);
    }
    let mut spiller = Spiller { next: 0, tokens };
    let mut out = Vec::new();
    spiller.statement(body.clone(), &mut out)?;
    Ok(Some(match out.len() {
        1 => out.pop().expect("one statement"),
        _ => BoundStmt {
            span: body.span,
            kind: BoundStmtKind::Block(out),
        },
    }))
}

struct Spiller<'t> {
    next: u32,
    tokens: &'t Tokens,
}

impl Spiller<'_> {
    /// Lowers `stmt` into `out`, preceded by whatever its expressions need spilled.
    fn statement(&mut self, stmt: BoundStmt, out: &mut Vec<BoundStmt>) -> Result<(), EmitError> {
        let span = stmt.span;
        let mut kind = stmt.kind;
        match &mut kind {
            BoundStmtKind::Block(statements) => {
                *statements = self.statements(core::mem::take(statements))?;
            }
            BoundStmtKind::Expression(expr)
            | BoundStmtKind::Return(Some(expr))
            | BoundStmtKind::Throw(Some(expr))
            | BoundStmtKind::Lock {
                expression: expr, ..
            }
            | BoundStmtKind::ForEach {
                collection: expr, ..
            } => self.root(expr, span, out)?,
            BoundStmtKind::Local { ty, declarators } if declarators.len() > 1 => {
                for declarator in core::mem::take(declarators) {
                    self.statement(
                        BoundStmt {
                            span,
                            kind: BoundStmtKind::Local {
                                ty: ty.clone(),
                                declarators: alloc::vec![declarator],
                            },
                        },
                        out,
                    )?;
                }
                return Ok(());
            }
            BoundStmtKind::Local { declarators, .. } => {
                for declarator in declarators.iter_mut() {
                    if let Some(initializer) = &mut declarator.initializer {
                        self.root(initializer, span, out)?;
                    }
                }
            }
            BoundStmtKind::If { condition, .. } => self.root(condition, span, out)?,
            BoundStmtKind::Switch { expression, .. } => self.root(expression, span, out)?,
            BoundStmtKind::While { condition, .. } | BoundStmtKind::DoWhile { condition, .. } => {
                refuse_nested(condition)?;
            }
            BoundStmtKind::For {
                condition,
                iterators,
                ..
            } => {
                if let Some(condition) = condition {
                    refuse_nested(condition)?;
                }
                for iterator in iterators.iter() {
                    refuse_nested(iterator)?;
                }
            }
            _ => {}
        }
        match &mut kind {
            BoundStmtKind::If {
                then_branch,
                else_branch,
                ..
            } => {
                self.nested(then_branch)?;
                if let Some(branch) = else_branch {
                    self.nested(branch)?;
                }
            }
            BoundStmtKind::Switch { sections, .. } => {
                for section in sections.iter_mut() {
                    section.statements = self.statements(core::mem::take(&mut section.statements))?;
                }
            }
            BoundStmtKind::For {
                initializer, body, ..
            } => {
                *initializer = self.statements(core::mem::take(initializer))?;
                self.nested(body)?;
            }
            BoundStmtKind::Using { resource, body } => {
                *resource = self.statements(core::mem::take(resource))?;
                self.nested(body)?;
            }
            BoundStmtKind::While { body, .. }
            | BoundStmtKind::DoWhile { body, .. }
            | BoundStmtKind::ForEach { body, .. }
            | BoundStmtKind::Lock { body, .. }
            | BoundStmtKind::Try { body, .. }
            | BoundStmtKind::Labeled { body, .. }
            | BoundStmtKind::Checked(body)
            | BoundStmtKind::Unchecked(body) => self.nested(body)?,
            _ => {}
        }
        out.push(BoundStmt { span, kind });
        Ok(())
    }

    fn statements(&mut self, statements: Vec<BoundStmt>) -> Result<Vec<BoundStmt>, EmitError> {
        let mut out = Vec::with_capacity(statements.len());
        for stmt in statements {
            self.statement(stmt, &mut out)?;
        }
        Ok(out)
    }

    /// A statement in a position that holds exactly one -- a branch, a loop body -- lowered, and
    /// wrapped in a block when its spills made it several.
    fn nested(&mut self, stmt: &mut Box<BoundStmt>) -> Result<(), EmitError> {
        let span = stmt.span;
        let taken = core::mem::replace(
            &mut **stmt,
            BoundStmt {
                span,
                kind: BoundStmtKind::Empty,
            },
        );
        let mut out = Vec::new();
        self.statement(taken, &mut out)?;
        **stmt = match out.len() {
            1 => out.pop().expect("one statement"),
            _ => BoundStmt {
                span,
                kind: BoundStmtKind::Block(out),
            },
        };
        Ok(())
    }

    /// A statement's expression, which starts on an empty stack: left alone when every `localloc`
    /// in it is reached with nothing pushed before it, else spilled into `out`.
    fn root(&mut self, expr: &mut BoundExpr, span: Span, out: &mut Vec<BoundStmt>) -> Result<(), EmitError> {
        if holds(expr) && !reached_empty(expr) {
            self.hoist(expr, span, out)?;
        }
        Ok(())
    }

    /// Rewrites `expr` so that each `localloc` in it is built into a temporary by a statement in
    /// `out`, after every operand evaluated before it has been spilled there too, in order.
    fn hoist(&mut self, expr: &mut BoundExpr, span: Span, out: &mut Vec<BoundStmt>) -> Result<(), EmitError> {
        if !holds(expr) {
            return Ok(());
        }
        if is_localloc(expr) {
            self.spill(expr, span, out);
            return Ok(());
        }
        if let BoundExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } = &mut expr.kind
        {
            if !(arm_starts_empty(when_true) && arm_starts_empty(when_false)) {
                return Err(unsupported());
            }
            self.hoist(condition, span, out)?;
            self.spill(expr, span, out);
            return Ok(());
        }
        let is_value_type = |ty: &TypeSymbol| crate::expr::is_value_type(ty, self.tokens);
        let Some(mut operands) = ordered_operands(&mut expr.kind, &is_value_type) else {
            return Err(unsupported());
        };
        let Some(last) = operands.iter().rposition(|operand| holds(operand.expr)) else {
            return Ok(());
        };
        for operand in operands.iter_mut().take(last + 1) {
            if holds(operand.expr) {
                self.hoist(operand.expr, span, out)?;
            } else if operand.spills {
                self.spill(operand.expr, span, out);
            }
        }
        Ok(())
    }

    /// Replaces `expr` with a read of a fresh temporary local, declared and initialized from it by
    /// a statement in `out`.
    fn spill(&mut self, expr: &mut BoundExpr, span: Span, out: &mut Vec<BoundStmt>) {
        let name: Box<str> = format!("<stackalloc>{}", self.next).into();
        self.next += 1;
        let ty = expr.ty.clone();
        let read = BoundExpr {
            ty: ty.clone(),
            kind: BoundExprKind::Local(name.clone()),
        };
        let value = core::mem::replace(expr, read);
        out.push(BoundStmt {
            span,
            kind: BoundStmtKind::Local {
                ty,
                declarators: alloc::vec![BoundDeclarator {
                    name,
                    initializer: Some(value),
                }],
            },
        });
    }
}

/// One operand of an expression, in the order it is evaluated, with whether evaluating it before
/// a `localloc` means storing it first.
struct Operand<'e> {
    expr: &'e mut BoundExpr,
    spills: bool,
}

/// The operands of `kind` in the order the emitter evaluates them, or `None` for a form this pass
/// does not spill.
fn ordered_operands<'e>(
    kind: &'e mut BoundExprKind,
    is_value_type: &dyn Fn(&TypeSymbol) -> bool,
) -> Option<Vec<Operand<'e>>> {
    let value = |expr: &'e mut BoundExpr| {
        let spills = spills_as_value(expr);
        Operand { expr, spills }
    };
    let receiver = |expr: &'e mut BoundExpr| {
        let spills = spills_as_value(expr) && !(is_value_type(&expr.ty) && is_variable(expr));
        Operand { expr, spills }
    };
    Some(match kind {
        BoundExprKind::Conversion { operand, .. }
        | BoundExprKind::Cast { operand, .. }
        | BoundExprKind::Unary { operand, .. } => alloc::vec![value(operand)],
        BoundExprKind::Call {
            callee,
            arguments,
            method,
        } => {
            let mut operands = Vec::with_capacity(arguments.len() + 1);
            if !method.as_ref().is_some_and(|method| method.is_static) {
                match &mut callee.kind {
                    BoundExprKind::MethodGroup { receiver: target, .. } => {
                        operands.push(receiver(target));
                    }
                    _ => return None,
                }
            }
            operands.extend(arguments.iter_mut().map(value));
            operands
        }
        BoundExprKind::ObjectCreation {
            arguments,
            initializer: None,
            ..
        } => arguments.iter_mut().map(value).collect(),
        BoundExprKind::Binary {
            operator,
            left,
            right,
            ..
        } if !matches!(operator, BinaryOperator::LogicalAnd | BinaryOperator::LogicalOr) => {
            alloc::vec![value(left), value(right)]
        }
        BoundExprKind::FieldAccess { receiver: target, .. }
        | BoundExprKind::PropertyAccess { receiver: target, .. } => {
            alloc::vec![receiver(target)]
        }
        BoundExprKind::IndexerAccess {
            receiver: target,
            indices,
            ..
        }
        | BoundExprKind::ElementAccess {
            receiver: target,
            indices,
        } => {
            let mut operands = alloc::vec![receiver(target)];
            operands.extend(indices.iter_mut().map(value));
            operands
        }
        BoundExprKind::Assignment {
            operator: AssignmentOperator::Assign,
            target,
            value: assigned,
            ..
        } => {
            let mut operands = match &mut target.kind {
                BoundExprKind::Local(_) => Vec::new(),
                BoundExprKind::FieldAccess { receiver: target, .. }
                | BoundExprKind::PropertyAccess { receiver: target, .. } => {
                    alloc::vec![receiver(target)]
                }
                BoundExprKind::IndexerAccess {
                    receiver: target,
                    indices,
                    ..
                }
                | BoundExprKind::ElementAccess {
                    receiver: target,
                    indices,
                } => {
                    let mut operands = alloc::vec![receiver(target)];
                    operands.extend(indices.iter_mut().map(value));
                    operands
                }
                _ => return None,
            };
            operands.push(value(assigned));
            operands
        }
        _ => return None,
    })
}

/// Whether `expr`, evaluated before a `localloc`, has to be stored first: anything but a constant,
/// `this`, `base`, a type or an address (a `ref` argument).
fn spills_as_value(expr: &BoundExpr) -> bool {
    !matches!(
        expr.kind,
        BoundExprKind::Literal(_)
            | BoundExprKind::This
            | BoundExprKind::Base
            | BoundExprKind::TypeReference(_)
            | BoundExprKind::Ref { .. }
    )
}

/// Whether `expr` names storage, whose address a value-type receiver takes.
fn is_variable(expr: &BoundExpr) -> bool {
    matches!(
        expr.kind,
        BoundExprKind::Local(_) | BoundExprKind::FieldAccess { .. } | BoundExprKind::ElementAccess { .. }
    )
}

fn unsupported() -> EmitError {
    EmitError::Unsupported("a stackalloc nested in an expression form that is not spilled yet")
}

/// A refusal when a nested `localloc` is in a loop's condition or iterator: the loop evaluates it
/// more than once, so its spills cannot run ahead of the statement.
fn refuse_nested(expr: &BoundExpr) -> Result<(), EmitError> {
    if holds(expr) && !reached_empty(expr) {
        return Err(EmitError::Unsupported(
            "a stackalloc nested in a loop's condition or iterator is not spilled yet",
        ));
    }
    Ok(())
}

/// Whether `expr` is a `stackalloc` lowered to `localloc`: its span constructor takes a pointer.
fn is_localloc(expr: &BoundExpr) -> bool {
    matches!(&expr.kind, BoundExprKind::StackAlloc { span_constructor: Some(ctor), .. }
        if matches!(ctor.parameters.first(), Some(TypeSymbol::Pointer(_))))
}

/// Whether a `localloc` is anywhere in `expr`.
fn holds(expr: &BoundExpr) -> bool {
    let mut found = false;
    crate::awaitlower::visit_expr(expr, &mut |node| found |= is_localloc(node));
    found
}

/// Whether every `localloc` in `expr` is reached with nothing pushed before it, when `expr` itself
/// starts on an empty stack: it is one, or converts one, or is a static call or an object creation
/// whose first argument is the only operand holding one and reaches it so.
fn reached_empty(expr: &BoundExpr) -> bool {
    match &expr.kind {
        _ if is_localloc(expr) => true,
        BoundExprKind::Conversion { operand, .. }
        | BoundExprKind::Cast { operand, .. }
        | BoundExprKind::Unary { operand, .. } => reached_empty(operand),
        BoundExprKind::Call {
            arguments, method, ..
        } if method.as_ref().is_some_and(|method| method.is_static) => first_only(arguments),
        BoundExprKind::ObjectCreation {
            arguments,
            initializer: None,
            ..
        } => first_only(arguments),
        _ => false,
    }
}

fn first_only(arguments: &[BoundExpr]) -> bool {
    arguments.first().is_some_and(reached_empty) && arguments.iter().skip(1).all(|a| !holds(a))
}

/// Whether a conditional's arm starts its `localloc`, if it has one, on the arm's empty stack.
fn arm_starts_empty(arm: &BoundExpr) -> bool {
    !holds(arm) || reached_empty(arm)
}
