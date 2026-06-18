//! Statement binding (ECMA-334 1st ed, clause 15).

use crate::bind::bind_type;
use crate::bound::{Binder, BoundExpr};
use crate::conversion::has_implicit_conversion;
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::special::SpecialType;
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::string::ToString;
use alloc::vec::Vec;
use lamella_syntax::ast::{Expr, Stmt, StmtKind, TypeRef, VariableDeclarator};
use lamella_syntax::span::Span;

/// A bound statement (15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundStmt {
    /// What the statement is, after binding.
    pub kind: BoundStmtKind,
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
    /// A statement form not yet bound, for recovery.
    Error,
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
            StmtKind::Expression(expr) => BoundStmtKind::Expression(self.bind_expression(expr)),
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
                BoundStmtKind::Return(value.as_ref().map(|expr| self.bind_expression(expr)))
            }
            _ => BoundStmtKind::Error,
        };
        BoundStmt { kind }
    }

    fn bind_local(&mut self, ty: &TypeRef, declarators: &[VariableDeclarator]) -> BoundStmtKind {
        let declared = self.resolve_named_type(&bind_type(ty), ty.span);
        let mut bound = Vec::with_capacity(declarators.len());
        for declarator in declarators {
            let initializer = declarator
                .initializer
                .as_ref()
                .map(|expr| self.bind_expression(expr));
            if let Some(initializer) = &initializer {
                self.check_convertible(&initializer.ty, &declared, declarator.span);
            }
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
        if !has_implicit_conversion(source, target) {
            self.report(Diagnostic::new(
                DiagnosticKind::NoImplicitConversion {
                    from: source.to_string().into(),
                    to: target.to_string().into(),
                },
                span,
            ));
        }
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
