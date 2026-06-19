//! Statement binding (ECMA-334 1st ed, clause 15).

use crate::bind::bind_type;
use crate::bound::{Binder, BoundExpr, BoundExprKind};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::special::SpecialType;
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use lamella_syntax::ast::{
    CatchClause, Expr, ExprKind, ForInitializer, Literal, Stmt, StmtKind, SwitchLabel,
    SwitchSection, TypeRef, UnaryOperator, UsingResource, VariableDeclarator,
};
use lamella_syntax::span::Span;

/// A bound statement (15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundStmt {
    /// What the statement is, after binding.
    pub kind: BoundStmtKind,
    /// The source range the statement came from, retained so code emission can
    /// attach sequence points (CIL offset to source line) for the debugger.
    pub span: Span,
}

/// The kind of a [`BoundStmt`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundStmtKind {
    /// A block; its own scope has already been applied (15.2).
    Block(Vec<BoundStmt>),
    /// The empty statement (15.3).
    Empty,
    /// A local-variable declaration (15.5.1).
    Local {
        /// The declared type, shared by every declarator.
        ty: TypeSymbol,
        /// The declared variables, with bound initializers.
        declarators: Vec<BoundDeclarator>,
    },
    /// An expression statement (15.6).
    Expression(BoundExpr),
    /// An `if` statement (15.7.1).
    If {
        /// The (boolean) condition.
        condition: BoundExpr,
        /// The then branch.
        then_branch: Box<BoundStmt>,
        /// The else branch, if any.
        else_branch: Option<Box<BoundStmt>>,
    },
    /// A `while` statement (15.8.1).
    While {
        /// The (boolean) condition.
        condition: BoundExpr,
        /// The loop body.
        body: Box<BoundStmt>,
    },
    /// A `return` statement (15.9.4).
    Return(Option<BoundExpr>),
    /// A `do ... while` statement (15.8.2).
    DoWhile {
        /// The loop body.
        body: Box<BoundStmt>,
        /// The (boolean) condition tested after each iteration.
        condition: BoundExpr,
    },
    /// A `for` statement (15.8.3). The initializer is a local declaration or a
    /// list of expression statements, already in the loop's scope.
    For {
        /// The initializer statements.
        initializer: Vec<BoundStmt>,
        /// The (boolean) loop condition, if any.
        condition: Option<BoundExpr>,
        /// The iterator expressions.
        iterators: Vec<BoundExpr>,
        /// The loop body.
        body: Box<BoundStmt>,
    },
    /// A `foreach` statement (15.8.4); the iteration variable is in the body's
    /// scope. The element-type check against the collection is deferred.
    ForEach {
        /// The iteration variable's name.
        name: Box<str>,
        /// The iteration variable's declared type.
        element_type: TypeSymbol,
        /// The collection iterated over.
        collection: BoundExpr,
        /// The loop body.
        body: Box<BoundStmt>,
    },
    /// A `break` statement (15.9.1).
    Break,
    /// A `continue` statement (15.9.2).
    Continue,
    /// A `throw` statement (15.9.5), with the thrown expression if any.
    Throw(Option<BoundExpr>),
    /// A `switch` statement (15.7.2): the governing expression and the sections,
    /// each carrying its bound `case`/`default` labels and statements.
    Switch {
        /// The governing expression.
        expression: BoundExpr,
        /// The sections, in order.
        sections: Vec<BoundSwitchSection>,
    },
    /// A `try` statement (15.10).
    Try {
        /// The protected block.
        body: Box<BoundStmt>,
        /// The catch clauses.
        catches: Vec<BoundCatch>,
        /// The finally block, if any.
        finally: Option<Box<BoundStmt>>,
    },
    /// A `lock` statement (15.12).
    Lock {
        /// The object locked on.
        expression: BoundExpr,
        /// The guarded statement.
        body: Box<BoundStmt>,
    },
    /// A `using` statement (15.13); the resource declaration/expression is bound
    /// in the body's scope.
    Using {
        /// The resource acquisition, as bound statements.
        resource: Vec<BoundStmt>,
        /// The guarded statement.
        body: Box<BoundStmt>,
    },
    /// A `checked` block (15.11).
    Checked(Box<BoundStmt>),
    /// An `unchecked` block (15.11).
    Unchecked(Box<BoundStmt>),
    /// A labeled statement (15.4).
    Labeled {
        /// The label.
        label: Box<str>,
        /// The labeled statement.
        body: Box<BoundStmt>,
    },
    /// A `goto` statement (15.9.3); label resolution is deferred.
    Goto,
    /// A statement form not yet bound, for recovery.
    Error,
}

/// A bound `switch` section (15.7.2): its labels and statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundSwitchSection {
    /// The `case`/`default` labels introducing the section.
    pub labels: Vec<BoundSwitchLabel>,
    /// The statements run when a label matches.
    pub statements: Vec<BoundStmt>,
}

/// A bound `switch` label (15.7.2): a case constant (an integral/char value as
/// `i64`) or the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundSwitchLabel {
    /// `case constant:` -- the constant's value.
    Case(i64),
    /// `default:`.
    Default,
}

/// A bound `catch` clause (15.10): the caught type, the bound exception variable
/// (in the handler's scope), and the handler body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundCatch {
    /// The caught exception type, or `None` for a general `catch`.
    pub exception_type: Option<TypeSymbol>,
    /// The exception variable's name, if any.
    pub name: Option<Box<str>>,
    /// The handler body.
    pub body: Box<BoundStmt>,
}

/// One bound variable declarator (15.5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundDeclarator {
    /// The variable's name.
    pub name: Box<str>,
    /// The bound initializer, if present.
    pub initializer: Option<BoundExpr>,
}

impl Binder {
    /// Binds a statement (15).
    pub fn bind_statement(&mut self, stmt: &Stmt) -> BoundStmt {
        let kind = match &stmt.kind {
            StmtKind::Block(statements) => {
                self.enter_scope();
                let bound = statements.iter().map(|s| self.bind_statement(s)).collect();
                self.exit_scope();
                BoundStmtKind::Block(bound)
            }
            StmtKind::Empty => BoundStmtKind::Empty,
            StmtKind::Expression(expr) => {
                let bound = self.bind_expression(expr);
                if !is_statement_expression(&bound.kind) {
                    self.report(Diagnostic::new(
                        DiagnosticKind::IllegalStatementExpression,
                        expr.span,
                    ));
                }
                BoundStmtKind::Expression(bound)
            }
            StmtKind::LocalDeclaration { ty, declarators } => self.bind_local(ty, declarators),
            StmtKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let condition = self.bind_condition(condition);
                let then_branch = Box::new(self.bind_statement(then_branch));
                let else_branch = else_branch
                    .as_ref()
                    .map(|branch| Box::new(self.bind_statement(branch)));
                BoundStmtKind::If {
                    condition,
                    then_branch,
                    else_branch,
                }
            }
            StmtKind::While { condition, body } => {
                let condition = self.bind_condition(condition);
                let body = Box::new(self.bind_statement(body));
                BoundStmtKind::While { condition, body }
            }
            StmtKind::Return(value) => {
                let value = value.as_ref().map(|expr| self.bind_expression(expr));
                self.check_return(value.as_ref(), stmt.span);
                let value = value.map(|v| self.convert_to_return_type(v));
                BoundStmtKind::Return(value)
            }
            StmtKind::DoWhile { body, condition } => {
                let body = Box::new(self.bind_statement(body));
                let condition = self.bind_condition(condition);
                BoundStmtKind::DoWhile { body, condition }
            }
            StmtKind::For {
                initializer,
                condition,
                iterators,
                body,
            } => self.bind_for(initializer.as_ref(), condition.as_ref(), iterators, body),
            StmtKind::ForEach {
                ty,
                name,
                collection,
                body,
            } => {
                let collection = self.bind_expression(collection);
                let element_type = self.resolve_named_type(&bind_type(ty), ty.span);
                self.enter_scope();
                self.declare_local(name, element_type.clone());
                let body = Box::new(self.bind_statement(body));
                self.exit_scope();
                BoundStmtKind::ForEach {
                    name: name.clone(),
                    element_type,
                    collection,
                    body,
                }
            }
            StmtKind::Break => BoundStmtKind::Break,
            StmtKind::Continue => BoundStmtKind::Continue,
            StmtKind::Throw(value) => {
                BoundStmtKind::Throw(value.as_ref().map(|expr| self.bind_expression(expr)))
            }
            StmtKind::Switch {
                expression,
                sections,
            } => {
                let switch_span = expression.span;
                let expression = self.bind_expression(expression);
                self.enter_scope();
                let mut seen_values: Vec<i64> = Vec::new();
                let mut seen_default = false;
                let mut bound_sections = Vec::with_capacity(sections.len());
                for section in sections {
                    let mut labels = Vec::with_capacity(section.labels.len());
                    for label in &section.labels {
                        let bound = self.bind_switch_label(label);
                        let duplicate = match bound {
                            BoundSwitchLabel::Case(value) if seen_values.contains(&value) => {
                                Some(format!("case {value}").into())
                            }
                            BoundSwitchLabel::Case(value) => {
                                seen_values.push(value);
                                None
                            }
                            BoundSwitchLabel::Default if seen_default => {
                                Some(Box::<str>::from("default"))
                            }
                            BoundSwitchLabel::Default => {
                                seen_default = true;
                                None
                            }
                        };
                        if let Some(text) = duplicate {
                            let span = match label {
                                SwitchLabel::Case(expr) => expr.span,
                                SwitchLabel::Default => section_anchor(section, switch_span),
                            };
                            self.report(Diagnostic::new(
                                DiagnosticKind::DuplicateCaseLabel { label: text },
                                span,
                            ));
                        }
                        labels.push(bound);
                    }
                    let statements: Vec<BoundStmt> = section
                        .statements
                        .iter()
                        .map(|statement| self.bind_statement(statement))
                        .collect();
                    if !statements.is_empty() && statements.iter().all(is_straight_line) {
                        self.report(Diagnostic::new(
                            DiagnosticKind::SwitchFallThrough,
                            section_anchor(section, switch_span),
                        ));
                    }
                    bound_sections.push(BoundSwitchSection { labels, statements });
                }
                self.exit_scope();
                BoundStmtKind::Switch {
                    expression,
                    sections: bound_sections,
                }
            }
            StmtKind::Try {
                body,
                catches,
                finally_block,
            } => BoundStmtKind::Try {
                body: Box::new(self.bind_statement(body)),
                catches: catches.iter().map(|catch| self.bind_catch(catch)).collect(),
                finally: finally_block
                    .as_ref()
                    .map(|block| Box::new(self.bind_statement(block))),
            },
            StmtKind::Lock { expression, body } => BoundStmtKind::Lock {
                expression: self.bind_expression(expression),
                body: Box::new(self.bind_statement(body)),
            },
            StmtKind::Using { resource, body } => self.bind_using(resource, body),
            StmtKind::Checked(inner) => {
                BoundStmtKind::Checked(Box::new(self.bind_statement(inner)))
            }
            StmtKind::Unchecked(inner) => {
                BoundStmtKind::Unchecked(Box::new(self.bind_statement(inner)))
            }
            StmtKind::Labeled { label, statement } => BoundStmtKind::Labeled {
                label: label.clone(),
                body: Box::new(self.bind_statement(statement)),
            },
            StmtKind::Goto(_) => BoundStmtKind::Goto,
            StmtKind::Error => BoundStmtKind::Error,
        };
        BoundStmt {
            kind,
            span: stmt.span,
        }
    }

    /// Binds a `switch` label: a `case` constant to its value, or `default`. A
    /// non-constant case is `CS0150`, recovered as `case 0`.
    fn bind_switch_label(&mut self, label: &SwitchLabel) -> BoundSwitchLabel {
        match label {
            SwitchLabel::Default => BoundSwitchLabel::Default,
            SwitchLabel::Case(expr) => {
                match case_constant(expr).or_else(|| self.enum_case_value(expr)) {
                    Some(value) => BoundSwitchLabel::Case(value),
                    None => {
                        self.report(Diagnostic::new(DiagnosticKind::ConstantExpected, expr.span));
                        BoundSwitchLabel::Case(0)
                    }
                }
            }
        }
    }

    /// The underlying value of a case label that names an enum member (or any
    /// constant field), by binding it and reading the field's constant. `None` when
    /// the expression is not a constant member access.
    fn enum_case_value(&mut self, expr: &Expr) -> Option<i64> {
        let bound = self.bind_expression(expr);
        self.record_case_label_uses(&bound);
        match &bound.kind {
            BoundExprKind::FieldAccess {
                field: Some(field), ..
            } => field.constant,
            _ => None,
        }
    }

    fn bind_catch(&mut self, catch: &CatchClause) -> BoundCatch {
        let exception_type = catch
            .exception_type
            .as_ref()
            .map(|ty| self.resolve_named_type(&bind_type(ty), ty.span));
        self.enter_scope();
        if let Some(name) = &catch.name {
            let ty = exception_type.clone().unwrap_or(TypeSymbol::Error);
            self.declare_local(name, ty);
        }
        let body = Box::new(self.bind_statement(&catch.body));
        self.exit_scope();
        BoundCatch {
            exception_type,
            name: catch.name.clone(),
            body,
        }
    }

    fn bind_using(&mut self, resource: &UsingResource, body: &Stmt) -> BoundStmtKind {
        self.enter_scope();
        let resource = match resource {
            UsingResource::Declaration { ty, declarators } => {
                let kind = self.bind_local(ty, declarators);
                alloc::vec![BoundStmt {
                    kind,
                    span: ty.span,
                }]
            }
            UsingResource::Expression(expression) => alloc::vec![BoundStmt {
                kind: BoundStmtKind::Expression(self.bind_expression(expression)),
                span: expression.span,
            }],
        };
        let body = Box::new(self.bind_statement(body));
        self.exit_scope();
        BoundStmtKind::Using { resource, body }
    }

    fn bind_for(
        &mut self,
        initializer: Option<&ForInitializer>,
        condition: Option<&Expr>,
        iterators: &[Expr],
        body: &Stmt,
    ) -> BoundStmtKind {
        self.enter_scope();
        let initializer = match initializer {
            None => Vec::new(),
            Some(ForInitializer::Declaration { ty, declarators }) => {
                let kind = self.bind_local(ty, declarators);
                alloc::vec![BoundStmt {
                    kind,
                    span: ty.span,
                }]
            }
            Some(ForInitializer::Expressions(expressions)) => expressions
                .iter()
                .map(|expression| BoundStmt {
                    kind: BoundStmtKind::Expression(self.bind_expression(expression)),
                    span: expression.span,
                })
                .collect(),
        };
        let condition = condition.map(|condition| self.bind_condition(condition));
        let iterators = iterators
            .iter()
            .map(|iterator| self.bind_expression(iterator))
            .collect();
        let body = Box::new(self.bind_statement(body));
        self.exit_scope();
        BoundStmtKind::For {
            initializer,
            condition,
            iterators,
            body,
        }
    }

    fn bind_local(&mut self, ty: &TypeRef, declarators: &[VariableDeclarator]) -> BoundStmtKind {
        let declared = self.resolve_named_type(&bind_type(ty), ty.span);
        let mut bound = Vec::with_capacity(declarators.len());
        for declarator in declarators {
            if self.local_in_current_scope(&declarator.name) {
                self.report(Diagnostic::new(
                    DiagnosticKind::DuplicateLocal {
                        name: declarator.name.clone(),
                    },
                    declarator.span,
                ));
            } else if self.local_in_enclosing_scope(&declarator.name) {
                self.report(Diagnostic::new(
                    DiagnosticKind::LocalShadowsEnclosing {
                        name: declarator.name.clone(),
                    },
                    declarator.span,
                ));
            }
            let initializer = declarator.initializer.as_ref().map(|expr| {
                let value = self.bind_expression(expr);
                self.check_convertible(&value.ty, &declared, declarator.span);
                self.convert(value, &declared)
            });
            self.declare_local(&declarator.name, declared.clone());
            bound.push(BoundDeclarator {
                name: declarator.name.clone(),
                initializer,
            });
        }
        BoundStmtKind::Local {
            ty: declared,
            declarators: bound,
        }
    }

    fn bind_condition(&mut self, condition: &Expr) -> BoundExpr {
        let bound = self.bind_expression(condition);
        self.check_convertible(
            &bound.ty,
            &TypeSymbol::Special(SpecialType::Boolean),
            condition.span,
        );
        bound
    }

    /// Reports `CS0029` if `source` has no implicit conversion to `target`. Error
    /// types are skipped so a failure does not cascade.
    fn check_convertible(&mut self, source: &TypeSymbol, target: &TypeSymbol, span: Span) {
        if source.is_error() || target.is_error() {
            return;
        }
        if !self.converts(source, target) {
            self.report_no_implicit_conversion(source, target, span);
        }
    }
}

/// Evaluates a `case` label's constant expression to an integral/char value. Only
/// the v1 constant forms are recognized: integer and character literals, and a
/// negated one. Anything else (a non-constant, or an unsupported form) is `None`.
fn case_constant(expr: &Expr) -> Option<i64> {
    match &expr.kind {
        ExprKind::Literal(Literal::Integer { value, .. }) => i64::try_from(*value).ok(),
        ExprKind::Literal(Literal::Character(unit)) => Some(i64::from(*unit)),
        ExprKind::Unary {
            operator: UnaryOperator::Minus,
            operand,
        } => case_constant(operand).map(|value| -value),
        _ => None,
    }
}

/// A span to anchor a section-level diagnostic on: its first `case` constant, else
/// its first statement, else the switch's governing expression.
fn section_anchor(section: &SwitchSection, fallback: Span) -> Span {
    section
        .labels
        .iter()
        .find_map(|label| match label {
            SwitchLabel::Case(expr) => Some(expr.span),
            SwitchLabel::Default => None,
        })
        .or_else(|| section.statements.first().map(|statement| statement.span))
        .unwrap_or(fallback)
}

/// Whether a bound expression is one C# allows to stand alone as a statement
/// (15.6): assignment, invocation, object/array creation, or pre/post
/// increment/decrement. `checked`/`unchecked` wrappers and a binding error are
/// admitted conservatively, so an odd-but-legal form is a gap, not a false CS0201.
fn is_statement_expression(kind: &BoundExprKind) -> bool {
    matches!(
        kind,
        BoundExprKind::Assignment { .. }
            | BoundExprKind::Call { .. }
            | BoundExprKind::ObjectCreation { .. }
            | BoundExprKind::ArrayCreation { .. }
            | BoundExprKind::Postfix { .. }
            | BoundExprKind::Unary {
                operator: UnaryOperator::PreIncrement | UnaryOperator::PreDecrement,
                ..
            }
            | BoundExprKind::Checked(_)
            | BoundExprKind::Unchecked(_)
            | BoundExprKind::Error
    )
}

/// Whether a statement passes control straight through to the next (no jump, no
/// branching). A section built only of these reaches its endpoint, so it falls
/// through (CS0163); anything else is left uncertain to avoid a false positive.
fn is_straight_line(stmt: &BoundStmt) -> bool {
    match &stmt.kind {
        BoundStmtKind::Local { .. } | BoundStmtKind::Expression(_) | BoundStmtKind::Empty => true,
        BoundStmtKind::Block(statements) => statements.iter().all(is_straight_line),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resolve::TypeTable;
    use lamella_syntax::parser::parse_statement;

    fn codes(source: &str) -> Vec<u16> {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.bind_statement(&parse_statement(source).statement);
        binder
            .into_diagnostics()
            .iter()
            .map(Diagnostic::code)
            .collect()
    }

    #[test]
    fn well_typed_locals_and_conditions_are_clean() {
        assert_eq!(codes("int x = 1;"), []);
        assert_eq!(codes("long n = 1;"), []);
        assert_eq!(codes("while (true) ;"), []);
        assert_eq!(codes("{ int x = 1; int y = x + 2; }"), []);
    }

    #[test]
    fn a_widening_initializer_gets_a_conversion_node() {
        use crate::bound::{BoundExprKind, ConversionKind};

        let mut binder = Binder::new();
        binder.enter_scope();
        let stmt = binder.bind_statement(&parse_statement("long x = 1;").statement);
        let BoundStmtKind::Local { declarators, .. } = &stmt.kind else {
            panic!("expected a local declaration");
        };
        let init = declarators[0].initializer.as_ref().expect("initializer");
        assert_eq!(init.ty, TypeSymbol::Special(SpecialType::Int64));
        assert!(matches!(
            init.kind,
            BoundExprKind::Conversion {
                conversion: ConversionKind::ImplicitNumeric,
                ..
            }
        ));

        let mut binder = Binder::new();
        binder.enter_scope();
        let stmt = binder.bind_statement(&parse_statement("int y = 1;").statement);
        let BoundStmtKind::Local { declarators, .. } = &stmt.kind else {
            panic!("expected a local declaration");
        };
        let init = declarators[0].initializer.as_ref().expect("initializer");
        assert!(matches!(init.kind, BoundExprKind::Literal(_)));
    }

    #[test]
    fn bad_initializer_conversion_is_cs0029() {
        assert_eq!(codes("int x = true;"), [29]);
        assert_eq!(codes("bool b = 1;"), [29]);
    }

    #[test]
    fn a_non_bool_condition_is_cs0029() {
        assert_eq!(codes("if (1) ;"), [29]);
        assert_eq!(codes("while (\"x\") ;"), [29]);
    }

    #[test]
    fn a_local_goes_out_of_scope_after_its_block() {
        assert_eq!(codes("{ { int x = 1; } int y = x + 0; }"), [103]);
    }

    #[test]
    fn switch_try_using_lock_bind_their_parts() {
        assert_eq!(
            codes("switch (1) { case 1: int a = 2; break; default: break; }"),
            []
        );
        assert_eq!(codes("try { } catch { }"), []);
        assert_eq!(codes("{ int x = true; }"), [29]);
        assert_eq!(codes("{ int n = 1; lock (n) { int m = n; } }"), []);
        assert_eq!(codes("using (int r = 5) { int s = r; }"), []);
        assert_eq!(codes("checked { int v = 1; }"), []);
        assert_eq!(codes("done: ;"), []);
    }

    #[test]
    fn bound_statements_retain_their_source_span() {
        let parsed = parse_statement("int x = 1;");
        let mut binder = Binder::new();
        binder.enter_scope();
        let bound = binder.bind_statement(&parsed.statement);
        assert_eq!(bound.span, parsed.statement.span);
    }

    #[test]
    fn loops_and_jumps_check_conditions_and_scope() {
        assert_eq!(codes("for (int i = 0; i < 10; i = i + 1) ;"), []);
        assert_eq!(codes("for (int i = 0; i; i = i + 1) ;"), [29]);
        assert_eq!(codes("do ; while (1);"), [29]);
        assert_eq!(codes("while (true) break;"), []);
        assert_eq!(codes("throw;"), []);
        assert_eq!(
            codes("for (int i = 0; i < 3; i = i + 1) { int j = i; }"),
            []
        );
    }

    #[test]
    fn local_declaration_types_resolve_against_the_world() {
        let mut world = TypeTable::new();
        world.insert("", "Widget");
        let mut binder = Binder::with_world(world);
        binder.enter_scope();
        binder.bind_statement(&parse_statement("Widget w;").statement);
        assert!(binder.diagnostics().is_empty());
        binder.bind_statement(&parse_statement("Gadget g;").statement);
        assert_eq!(
            binder
                .diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>(),
            [246]
        );
    }
}
