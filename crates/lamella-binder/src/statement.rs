//! Statement binding (ECMA-334 1st ed, clause 15).

use crate::bind::bind_type;
use crate::bound::{
    Binder, BoundExpr, BoundExprKind, MethodReference, constant_int_value, constant_literal_value,
};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::special::SpecialType;
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
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
    /// A `fixed` statement (unsafe, 15.7): `name` is bound to a pointer to the first
    /// element of the pinned `init` (an array/string of `element` type) for the body.
    Fixed {
        /// The pointer variable bound for the body.
        name: Box<str>,
        /// The pointed-to (and array element) type, for `ldelema` and the pointer width.
        element: TypeSymbol,
        /// The pinned source array/string.
        init: BoundExpr,
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
    /// A `goto` statement (15.9.3), naming the label to branch to.
    Goto(Box<str>),
    /// `goto case constant;` -- a jump to a case of the enclosing switch (15.9.3).
    GotoCase(i64),
    /// `goto case "string";` -- a jump to a string case of the enclosing switch.
    GotoCaseString(Box<[u16]>),
    /// `goto default;` -- a jump to the default section of the enclosing switch.
    GotoDefault,
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundSwitchLabel {
    /// `case constant:` -- an integral/char/enum constant's value.
    Case(i64),
    /// `case "string":` -- a string constant (UTF-16), matched by value.
    CaseString(Box<[u16]>),
    /// `case null:` -- the null reference, matched against a `string` governing value.
    CaseNull,
    /// `default:`.
    Default,
}

/// Render a section label as the fall-through diagnostics quote it: `case 5:`,
/// `case "hi":`, `case null:` or `default:`.
///
/// A label written as anything but a decimal literal -- `case K:`, `case 0x10:`,
/// `case E.B:` -- renders as its FOLDED VALUE, because the binder holds bound labels
/// and not source text. The reported code is unaffected; only the quoted text differs
/// from the oracle's, and closing that needs source access threaded to the binder.
fn switch_label_text(label: &BoundSwitchLabel) -> Box<str> {
    match label {
        BoundSwitchLabel::Case(value) => format!("case {value}:").into(),
        BoundSwitchLabel::CaseString(text) => {
            format!("case \"{}\":", String::from_utf16_lossy(text)).into()
        }
        BoundSwitchLabel::CaseNull => Box::from("case null:"),
        BoundSwitchLabel::Default => Box::from("default:"),
    }
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
    /// The `catch (...)` clause header's span, for a debug build's sequence point on it
    /// (a breakpoint on the catch clause).
    pub span: Span,
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
                if self.conditional_call_omitted(&bound) {
                    BoundStmtKind::Empty
                } else {
                    BoundStmtKind::Expression(bound)
                }
            }
            StmtKind::LocalDeclaration {
                ty,
                declarators,
                is_const,
            } => {
                if let Some(name) = crate::program::restricted_array_element(ty) {
                    self.report(Diagnostic::new(
                        DiagnosticKind::RestrictedTypeArrayElement { ty: name.into() },
                        ty.span,
                    ));
                }
                if *is_const {
                    self.bind_const_local(ty, declarators)
                } else {
                    self.bind_local(ty, declarators)
                }
            }
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
                self.enter_loop();
                let body = Box::new(self.bind_statement(body));
                self.exit_loop();
                BoundStmtKind::While { condition, body }
            }
            StmtKind::Return(value) => {
                let value = value.as_ref().map(|expr| self.bind_expression(expr));
                if self.return_leaves_finally() {
                    self.report(Diagnostic::new(
                        DiagnosticKind::ControlLeavesFinally,
                        stmt.span,
                    ));
                }
                self.check_return(value.as_ref(), stmt.span);
                let value = value.map(|v| self.convert_to_return_type(v));
                BoundStmtKind::Return(value)
            }
            StmtKind::DoWhile { body, condition } => {
                self.enter_loop();
                let body = Box::new(self.bind_statement(body));
                self.exit_loop();
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
                let single_dimension_array = matches!(collection.ty, TypeSymbol::Array { rank: 1, .. });
                self.enter_readonly_local(name, "foreach iteration variable");
                let enumerable = if single_dimension_array {
                    None
                } else {
                    self.bind_for_each_enumerable(ty.span, &element_type, name, collection.clone(), body)
                };
                if let Some(desugared) = enumerable {
                    self.exit_readonly_local();
                    desugared
                } else {
                    if !single_dimension_array
                        && !collection.ty.is_error()
                        && !matches!(collection.ty, TypeSymbol::Array { .. })
                    {
                        self.report(Diagnostic::new(
                            DiagnosticKind::ForEachNotEnumerable {
                                ty: format!("{}", collection.ty).into(),
                            },
                            stmt.span,
                        ));
                    }
                    self.enter_scope();
                    self.declare_local(name, element_type.clone());
                    self.enter_loop();
                    let body = Box::new(self.bind_statement(body));
                    self.exit_loop();
                    self.exit_scope();
                    self.exit_readonly_local();
                    BoundStmtKind::ForEach {
                        name: name.clone(),
                        element_type,
                        collection,
                        body,
                    }
                }
            }
            StmtKind::Break => {
                if !self.in_loop_or_switch() {
                    self.report(Diagnostic::new(DiagnosticKind::NoEnclosingLoop, stmt.span));
                } else if self.jump_leaves_finally() {
                    self.report(Diagnostic::new(
                        DiagnosticKind::ControlLeavesFinally,
                        stmt.span,
                    ));
                }
                BoundStmtKind::Break
            }
            StmtKind::Continue => {
                if !self.in_loop() {
                    self.report(Diagnostic::new(DiagnosticKind::NoEnclosingLoop, stmt.span));
                } else if self.jump_leaves_finally() {
                    self.report(Diagnostic::new(
                        DiagnosticKind::ControlLeavesFinally,
                        stmt.span,
                    ));
                }
                BoundStmtKind::Continue
            }
            StmtKind::Throw(value) => {
                let bound = value.as_ref().map(|expr| self.bind_expression(expr));
                if value.is_none() && !self.in_catch() {
                    self.report(Diagnostic::new(
                        DiagnosticKind::RethrowOutsideCatch,
                        stmt.span,
                    ));
                }
                if let (Some(operand), Some(expr)) = (&bound, value.as_ref()) {
                    if self.is_provably_not_exception(&operand.ty) {
                        self.report(Diagnostic::new(
                            DiagnosticKind::CaughtTypeMustBeException,
                            expr.span,
                        ));
                    }
                }
                BoundStmtKind::Throw(bound)
            }
            StmtKind::Switch {
                expression,
                sections,
            } => {
                let switch_span = expression.span;
                let expression = self.bind_expression(expression);
                let expression = self.coerce_switch_governing(expression);
                if matches!(expression.ty, TypeSymbol::Special(SpecialType::Boolean)) {
                    self.report(Diagnostic::new(
                        DiagnosticKind::FeatureRequiresLaterVersion {
                            feature: "switch on boolean type".into(),
                            required: "C# 2.0".into(),
                        },
                        switch_span,
                    ));
                } else if !expression.ty.is_error()
                    && !self.is_switch_governing_type(&expression.ty)
                {
                    self.report(Diagnostic::new(
                        DiagnosticKind::SwitchGoverningType,
                        switch_span,
                    ));
                }
                self.enter_scope();
                self.enter_switch();
                let mut seen_values: Vec<i64> = Vec::new();
                let mut seen_strings: Vec<Box<[u16]>> = Vec::new();
                let mut seen_null = false;
                let mut seen_default = false;
                let mut bound_sections = Vec::with_capacity(sections.len());
                for (index, section) in sections.iter().enumerate() {
                    let mut labels = Vec::with_capacity(section.labels.len());
                    for label in &section.labels {
                        let bound = self.bind_switch_label(label, &expression.ty);
                        let duplicate = match &bound {
                            BoundSwitchLabel::Case(value) if seen_values.contains(value) => {
                                Some(format!("case {value}").into())
                            }
                            BoundSwitchLabel::Case(value) => {
                                seen_values.push(*value);
                                None
                            }
                            BoundSwitchLabel::CaseString(text) if seen_strings.contains(text) => {
                                Some(Box::<str>::from("a duplicate string case"))
                            }
                            BoundSwitchLabel::CaseString(text) => {
                                seen_strings.push(text.clone());
                                None
                            }
                            BoundSwitchLabel::CaseNull if seen_null => {
                                Some(Box::<str>::from("case null"))
                            }
                            BoundSwitchLabel::CaseNull => {
                                seen_null = true;
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
                    if !statements.is_empty() && crate::flow::switch_section_completes(&statements) {
                        let label = labels
                            .last()
                            .map(switch_label_text)
                            .unwrap_or_else(|| Box::from("default:"));
                        let kind = if index + 1 == sections.len() {
                            DiagnosticKind::SwitchFallOutFinal { label }
                        } else {
                            DiagnosticKind::SwitchFallThrough { label }
                        };
                        self.report(Diagnostic::new(kind, section_anchor(section, switch_span)));
                    }
                    bound_sections.push(BoundSwitchSection { labels, statements });
                }
                self.exit_switch();
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
                catches: {
                    if let Some(index) = catches
                        .iter()
                        .position(|catch| catch.exception_type.is_none())
                    {
                        for later in &catches[index + 1..] {
                            self.report(Diagnostic::new(
                                DiagnosticKind::CatchAfterGeneralCatch,
                                later.span,
                            ));
                        }
                    }
                    catches.iter().map(|catch| self.bind_catch(catch)).collect()
                },
                finally: finally_block.as_ref().map(|block| {
                    self.enter_finally();
                    let bound = Box::new(self.bind_statement(block));
                    self.exit_finally();
                    bound
                }),
            },
            StmtKind::Lock { expression, body } => self.bind_lock(expression, body),
            StmtKind::Using { resource, body } => self.bind_using(resource, body),
            StmtKind::Fixed {
                ty,
                name,
                init,
                body,
            } => self.bind_fixed(ty, name, init, body),
            StmtKind::Checked(inner) => {
                let saved_checked = self.checked_context;
                let saved_unchecked = self.unchecked_context;
                self.checked_context = true;
                self.unchecked_context = false;
                let bound = self.bind_statement(inner);
                self.checked_context = saved_checked;
                self.unchecked_context = saved_unchecked;
                BoundStmtKind::Checked(Box::new(bound))
            }
            StmtKind::Unchecked(inner) => {
                let saved_checked = self.checked_context;
                let saved_unchecked = self.unchecked_context;
                self.checked_context = false;
                self.unchecked_context = true;
                let bound = self.bind_statement(inner);
                self.checked_context = saved_checked;
                self.unchecked_context = saved_unchecked;
                BoundStmtKind::Unchecked(Box::new(bound))
            }
            StmtKind::Labeled { label, statement } => BoundStmtKind::Labeled {
                label: label.clone(),
                body: Box::new(self.bind_statement(statement)),
            },
            StmtKind::Goto(lamella_syntax::ast::GotoTarget::Label(name)) => {
                BoundStmtKind::Goto(name.clone())
            }
            StmtKind::Goto(lamella_syntax::ast::GotoTarget::Case(expr)) => {
                if !self.in_switch() {
                    self.report(Diagnostic::new(
                        DiagnosticKind::GotoCaseOutsideSwitch,
                        stmt.span,
                    ));
                }
                if let ExprKind::Literal(Literal::String(text)) = &expr.kind {
                    BoundStmtKind::GotoCaseString(text.clone())
                } else {
                    match self.case_label_value(expr) {
                        Some(value) => BoundStmtKind::GotoCase(value),
                        None => BoundStmtKind::Error,
                    }
                }
            }
            StmtKind::Goto(lamella_syntax::ast::GotoTarget::Default) => {
                BoundStmtKind::GotoDefault
            }
            StmtKind::Error => BoundStmtKind::Error,
        };
        BoundStmt {
            kind,
            span: stmt.span,
        }
    }

    /// Binds a `switch` label: a `case` constant to its value, or `default`. A
    /// non-constant case is `CS0150`, recovered as `case 0`.
    fn bind_switch_label(&mut self, label: &SwitchLabel, governing: &TypeSymbol) -> BoundSwitchLabel {
        match label {
            SwitchLabel::Default => BoundSwitchLabel::Default,
            SwitchLabel::Case(expr) => {
                let bound = self.bind_expression(expr);
                self.record_case_label_uses(&bound);
                self.check_assignable(&bound, governing, expr.span);
                match crate::bound::constant_literal_value(&bound) {
                    Some(Literal::String(text)) => BoundSwitchLabel::CaseString(text),
                    Some(Literal::Null) => BoundSwitchLabel::CaseNull,
                    Some(literal) => match crate::bound::literal_int_value(&literal) {
                        Some(value) => BoundSwitchLabel::Case(value),
                        None => {
                            self.report(Diagnostic::new(
                                DiagnosticKind::ConstantExpected,
                                expr.span,
                            ));
                            BoundSwitchLabel::Case(0)
                        }
                    },
                    None => {
                        self.report(Diagnostic::new(DiagnosticKind::ConstantExpected, expr.span));
                        BoundSwitchLabel::Case(0)
                    }
                }
            }
        }
    }

    /// A `case`/`goto case` label's constant value (15.7.2): the label is bound and folded as a
    /// constant expression (14.15) -- an integer/char/enum constant, or any arithmetic, cast, or
    /// member reference over them. `None` when it is not a constant integer, which the caller
    /// reports as `CS0150`. Locals a folded label references are recorded so the unused-local
    /// check is not misled.
    fn case_label_value(&mut self, expr: &Expr) -> Option<i64> {
        let bound = self.bind_expression(expr);
        self.record_case_label_uses(&bound);
        constant_int_value(&bound)
    }

    /// Whether `ty` can be PROVEN not to derive from `System.Exception` -- which makes a
    /// `catch` or `throw` of it CS0155.
    ///
    /// Conservative in ONE direction. An unresolved type, or a class whose base chain leaves
    /// this compilation, answers false, so an exception type we cannot see is never falsely
    /// flagged; the cost is missing it. A primitive, `string` or `object` is decided outright
    /// -- `object` is `Exception`'s BASE, not its descendant -- and a struct, enum, interface
    /// or delegate cannot derive from a class at all.
    /// Whether `ty` can be PROVEN not to derive from `System.<name>` -- the same conservative walk
    /// [`Self::is_provably_not_exception`] makes, generalized so the attribute rule can reuse it.
    /// A chain that leaves this compilation answers false ("cannot prove"), so an unresolvable
    /// base never manufactures a diagnostic.
    pub(crate) fn is_provably_not_derived_from_system(&self, ty: &TypeSymbol, name: &str) -> bool {
        let Some(mut current) = self.model().get_by_symbol(ty) else {
            return false;
        };
        if current.kind != crate::symbols::TypeKind::Class {
            return true;
        }
        for _ in 0..64 {
            if &*current.namespace == "System" && &*current.name == name {
                return false;
            }
            match &current.base {
                None | Some(TypeSymbol::Special(_)) => return true,
                Some(base) => match self.model().get_by_symbol(base) {
                    Some(next) => current = next,
                    None => return false,
                },
            }
        }
        false
    }

    pub(crate) fn is_provably_not_exception(&self, ty: &TypeSymbol) -> bool {
        match ty {
            TypeSymbol::Error => return false,
            TypeSymbol::Special(SpecialType::Null) => return false,
            TypeSymbol::Special(_) => return true,
            _ => {}
        }
        let Some(mut current) = self.model().get_by_symbol(ty) else {
            return false;
        };
        if current.kind != crate::symbols::TypeKind::Class {
            return true;
        }
        for _ in 0..64 {
            if &*current.namespace == "System" && &*current.name == "Exception" {
                return false;
            }
            match &current.base {
                None | Some(TypeSymbol::Special(_)) => return true,
                Some(base) => match self.model().get_by_symbol(base) {
                    Some(next) => current = next,
                    None => return false,
                },
            }
        }
        false
    }

    fn bind_catch(&mut self, catch: &CatchClause) -> BoundCatch {
        let exception_type = catch
            .exception_type
            .as_ref()
            .map(|ty| self.resolve_named_type(&bind_type(ty), ty.span));
        if let (Some(resolved), Some(written)) = (&exception_type, &catch.exception_type) {
            if self.is_provably_not_exception(resolved) {
                self.report(Diagnostic::new(
                    DiagnosticKind::CaughtTypeMustBeException,
                    written.span,
                ));
            }
        }
        self.enter_scope();
        if let Some(name) = &catch.name {
            let ty = exception_type.clone().unwrap_or(TypeSymbol::Error);
            self.declare_local(name, ty);
        }
        self.enter_catch();
        let body = Box::new(self.bind_statement(&catch.body));
        self.exit_catch();
        self.exit_scope();
        BoundCatch {
            exception_type,
            name: catch.name.clone(),
            body,
            span: catch.span,
        }
    }

    /// Desugars `foreach (V name in collection)` over a non-array collection into the
    /// enumerator pattern (15.8.4): a block that declares the enumerator, then
    /// `while (e.MoveNext())` whose body binds `name = (V)e.Current` ahead of the original
    /// body, the loop wrapped in `try { ... } finally { <e> as IDisposable, disposed if non-null }`.
    /// `None` when the collection has no `GetEnumerator` (the array/error path is kept).
    fn bind_for_each_enumerable(
        &mut self,
        span: Span,
        element_type: &TypeSymbol,
        name: &str,
        collection: BoundExpr,
        body: &Stmt,
    ) -> Option<BoundStmtKind> {
        let get_enumerator = self.resolve_instance_method(&collection.ty, "GetEnumerator", span)?;
        let enumerator_type = get_enumerator.return_type.clone();
        let ienumerator: TypeSymbol = {
            let parts: alloc::vec::Vec<Box<str>> =
                alloc::vec!["System".into(), "Collections".into(), "IEnumerator".into()];
            TypeSymbol::Named(parts.into_boxed_slice())
        };
        let pattern_move_next = self
            .resolve_instance_method(&enumerator_type, "MoveNext", span)
            .filter(|method| {
                matches!(method.return_type, TypeSymbol::Special(SpecialType::Boolean))
            });
        let pattern_current = self.resolve_property_getter(&enumerator_type, "Current", span);
        let (move_next, get_current) = match (pattern_move_next, pattern_current) {
            (Some(move_next), Some(get_current)) => (move_next, get_current),
            _ => (
                self.resolve_instance_method(&ienumerator, "MoveNext", span)?,
                self.resolve_property_getter(&ienumerator, "Current", span)?,
            ),
        };

        let enumerator: Box<str> = format!("<enumerator>{}", span.start).into();
        let call = |receiver: BoundExpr, method: MethodReference| -> BoundExpr {
            let return_type = method.return_type.clone();
            BoundExpr {
                kind: BoundExprKind::Call {
                    callee: Box::new(BoundExpr {
                        kind: BoundExprKind::MethodGroup {
                            receiver: Box::new(receiver),
                            name: method.name.clone(),
                        },
                        ty: TypeSymbol::Error,
                    }),
                    arguments: Vec::new(),
                    method: Some(method),
                },
                ty: return_type,
            }
        };
        let enumerator_ref = || BoundExpr {
            kind: BoundExprKind::Local(enumerator.clone()),
            ty: enumerator_type.clone(),
        };

        let enumerator_decl = BoundStmt {
            kind: BoundStmtKind::Local {
                ty: enumerator_type.clone(),
                declarators: alloc::vec![BoundDeclarator {
                    name: enumerator.clone(),
                    initializer: Some(call(collection, get_enumerator)),
                }],
            },
            span,
        };
        let condition = call(enumerator_ref(), move_next);
        let element_value = BoundExpr {
            kind: BoundExprKind::Cast {
                operand: Box::new(call(enumerator_ref(), get_current)),
                checked: false,
            },
            ty: element_type.clone(),
        };

        self.enter_scope();
        self.declare_local(name, element_type.clone());
        self.enter_loop();
        let bound_body = self.bind_statement(body);
        self.exit_loop();
        self.exit_scope();

        let element_decl = BoundStmt {
            kind: BoundStmtKind::Local {
                ty: element_type.clone(),
                declarators: alloc::vec![BoundDeclarator {
                    name: name.into(),
                    initializer: Some(element_value),
                }],
            },
            span,
        };
        let while_stmt = BoundStmt {
            kind: BoundStmtKind::While {
                condition,
                body: Box::new(BoundStmt {
                    kind: BoundStmtKind::Block(alloc::vec![element_decl, bound_body]),
                    span,
                }),
            },
            span,
        };

        let idisposable: TypeSymbol = {
            let parts: alloc::vec::Vec<Box<str>> =
                alloc::vec!["System".into(), "IDisposable".into()];
            TypeSymbol::Named(parts.into_boxed_slice())
        };
        let loop_stmt = match self.resolve_instance_method(&idisposable, "Dispose", span) {
            Some(dispose) => {
                let disposable: Box<str> = format!("<disposable>{}", span.start).into();
                let disposable_ref = || BoundExpr {
                    kind: BoundExprKind::Local(disposable.clone()),
                    ty: idisposable.clone(),
                };
                let disposable_decl = BoundStmt {
                    kind: BoundStmtKind::Local {
                        ty: idisposable.clone(),
                        declarators: alloc::vec![BoundDeclarator {
                            name: disposable.clone(),
                            initializer: Some(BoundExpr {
                                kind: BoundExprKind::TypeTest {
                                    operation: lamella_syntax::ast::TypeTestOperation::As,
                                    operand: Box::new(enumerator_ref()),
                                    target: idisposable.clone(),
                                },
                                ty: idisposable.clone(),
                            }),
                        }],
                    },
                    span,
                };
                let guard = BoundStmt {
                    kind: BoundStmtKind::If {
                        condition: BoundExpr {
                            kind: BoundExprKind::Binary {
                                operator: lamella_syntax::ast::BinaryOperator::NotEqual,
                                left: Box::new(disposable_ref()),
                                right: Box::new(BoundExpr {
                                    kind: BoundExprKind::Literal(Literal::Null),
                                    ty: TypeSymbol::Special(SpecialType::Object),
                                }),
                                checked: false,
                            },
                            ty: TypeSymbol::Special(SpecialType::Boolean),
                        },
                        then_branch: Box::new(BoundStmt {
                            kind: BoundStmtKind::Expression(call(disposable_ref(), dispose)),
                            span,
                        }),
                        else_branch: None,
                    },
                    span,
                };
                let finally = BoundStmt {
                    kind: BoundStmtKind::Block(alloc::vec![disposable_decl, guard]),
                    span: Span::HIDDEN,
                };
                BoundStmt {
                    kind: BoundStmtKind::Try {
                        body: Box::new(while_stmt),
                        catches: Vec::new(),
                        finally: Some(Box::new(finally)),
                    },
                    span,
                }
            }
            None => while_stmt,
        };
        Some(BoundStmtKind::Block(alloc::vec![enumerator_decl, loop_stmt]))
    }

    /// Desugars `lock (x) body` (15.12) to the monitor pattern: evaluate `x` once into an
    /// `object` temp, `Monitor.Enter` it, then `try { body } finally { Monitor.Exit }`. 1st-ed
    /// CIL puts Enter before the try (the `ref taken` overload is 2.0+); the locking is identical.
    fn bind_lock(&mut self, expression: &Expr, body: &Stmt) -> BoundStmtKind {
        let span = expression.span;
        let object_ty = TypeSymbol::Special(SpecialType::Object);
        let void_ty = TypeSymbol::Special(SpecialType::Void);
        let monitor: TypeSymbol = {
            let parts: alloc::vec::Vec<Box<str>> =
                alloc::vec!["System".into(), "Threading".into(), "Monitor".into()];
            TypeSymbol::Named(parts.into_boxed_slice())
        };
        let lock_obj: Box<str> = format!("<lock>{}", span.start).into();
        let monitor_call = |name: &str| BoundStmt {
            kind: BoundStmtKind::Expression(BoundExpr {
                kind: BoundExprKind::Call {
                    callee: Box::new(BoundExpr {
                        kind: BoundExprKind::MethodGroup {
                            receiver: Box::new(BoundExpr {
                                kind: BoundExprKind::TypeReference(monitor.clone()),
                                ty: monitor.clone(),
                            }),
                            name: name.into(),
                        },
                        ty: TypeSymbol::Error,
                    }),
                    arguments: alloc::vec![BoundExpr {
                        kind: BoundExprKind::Local(lock_obj.clone()),
                        ty: object_ty.clone(),
                    }],
                    method: Some(MethodReference {
                        declaring_type: monitor.clone(),
                        name: name.into(),
                        parameters: alloc::vec![object_ty.clone()],
                        return_type: void_ty.clone(),
                        is_static: true,
                        is_vararg: false,
                    }),
                },
                ty: void_ty.clone(),
            }),
            span,
        };
        let held = self.bind_expression(expression);
        if !held.ty.is_error() && self.is_value_type(&held.ty) {
            let ty = format!("{}", held.ty).into();
            self.report(Diagnostic::new(
                DiagnosticKind::LockRequiresReferenceType { ty },
                span,
            ));
        }
        let held = self.convert(held, &object_ty);
        let lock_decl = BoundStmt {
            kind: BoundStmtKind::Local {
                ty: object_ty.clone(),
                declarators: alloc::vec![BoundDeclarator {
                    name: lock_obj.clone(),
                    initializer: Some(held),
                }],
            },
            span,
        };
        let bound_body = self.bind_statement(body);
        let guarded = BoundStmt {
            kind: BoundStmtKind::Try {
                body: Box::new(bound_body),
                catches: Vec::new(),
                finally: Some(Box::new(BoundStmt {
                    kind: BoundStmtKind::Block(alloc::vec![monitor_call("Exit")]),
                    span: Span::HIDDEN,
                })),
            },
            span,
        };
        BoundStmtKind::Block(alloc::vec![lock_decl, monitor_call("Enter"), guarded])
    }

    /// Desugars `using (resource) body` (15.13) to `try`/`finally` that disposes the resource:
    /// `{ <resource decl>; try { body } finally { IDisposable __d = r as IDisposable;
    /// if (__d != null) __d.Dispose(); } }`. A declaration's resources are disposed in reverse
    /// (nested-using order); an expression resource is held in a temp. The `as`+null-check form
    /// is conformant (a null resource is a no-op), like the foreach `Dispose` (15.8.4).
    /// Reports `CS1674` when `ty` provably does not implement `System.IDisposable`. Conservative
    /// in the same direction as the exception rule: a type that does not resolve, or one whose
    /// interface list this compilation cannot see in full, reports nothing rather than accusing
    /// code it cannot check.
    fn check_disposable(&mut self, ty: &TypeSymbol, span: Span) {
        if ty.is_error() || !matches!(ty, TypeSymbol::Named(_)) {
            return;
        }
        if self.model().get_by_symbol(ty).is_none() {
            return;
        }
        let implements = self.transitive_interfaces(ty).iter().any(|interface| {
            self.model()
                .get_by_symbol(interface)
                .is_some_and(|info| &*info.namespace == "System" && &*info.name == "IDisposable")
        });
        if !implements {
            self.report(Diagnostic::new(
                DiagnosticKind::UsingRequiresDisposable {
                    ty: format!("{ty}").into(),
                },
                span,
            ));
        }
    }

    fn bind_using(&mut self, resource: &UsingResource, body: &Stmt) -> BoundStmtKind {
        self.enter_scope();
        let mut resource_decls: alloc::vec::Vec<BoundStmt> = Vec::new();
        let mut resources: alloc::vec::Vec<(Box<str>, TypeSymbol)> = Vec::new();
        match resource {
            UsingResource::Declaration { ty, declarators } => {
                let resource_ty = self.resolve_named_type(&bind_type(ty), ty.span);
                self.check_disposable(&resource_ty, ty.span);
                let kind = self.bind_local(ty, declarators);
                resource_decls.push(BoundStmt { kind, span: ty.span });
                for declarator in declarators {
                    resources.push((declarator.name.clone(), resource_ty.clone()));
                    self.enter_readonly_local(&declarator.name, "using variable");
                }
            }
            UsingResource::Expression(expression) => {
                let span = expression.span;
                let bound = self.bind_expression(expression);
                let resource_ty = if matches!(bound.ty, TypeSymbol::Special(SpecialType::Null)) {
                    TypeSymbol::Special(SpecialType::Object)
                } else {
                    bound.ty.clone()
                };
                let temp: Box<str> = format!("<using>{}", span.start).into();
                resource_decls.push(BoundStmt {
                    kind: BoundStmtKind::Local {
                        ty: resource_ty.clone(),
                        declarators: alloc::vec![BoundDeclarator {
                            name: temp.clone(),
                            initializer: Some(bound),
                        }],
                    },
                    span,
                });
                resources.push((temp, resource_ty));
            }
        }
        let bound_body = self.bind_statement(body);
        for _ in 0..resources.len() {
            self.exit_readonly_local();
        }
        self.exit_scope();

        let span = body.span;
        let idisposable: TypeSymbol = {
            let parts: alloc::vec::Vec<Box<str>> =
                alloc::vec!["System".into(), "IDisposable".into()];
            TypeSymbol::Named(parts.into_boxed_slice())
        };
        let Some(dispose) = self.resolve_instance_method(&idisposable, "Dispose", span) else {
            resource_decls.push(bound_body);
            return BoundStmtKind::Block(resource_decls);
        };
        let mut finally_stmts: alloc::vec::Vec<BoundStmt> = Vec::new();
        for (index, (name, resource_ty)) in resources.iter().enumerate().rev() {
            if self.is_value_type(resource_ty) {
                if let Some(dispose) = self.resolve_instance_method(resource_ty, "Dispose", span) {
                    finally_stmts.push(BoundStmt {
                        kind: BoundStmtKind::Expression(BoundExpr {
                            kind: BoundExprKind::Call {
                                callee: Box::new(BoundExpr {
                                    kind: BoundExprKind::MethodGroup {
                                        receiver: Box::new(BoundExpr {
                                            kind: BoundExprKind::Local(name.clone()),
                                            ty: resource_ty.clone(),
                                        }),
                                        name: dispose.name.clone(),
                                    },
                                    ty: TypeSymbol::Error,
                                }),
                                arguments: Vec::new(),
                                method: Some(dispose.clone()),
                            },
                            ty: dispose.return_type.clone(),
                        }),
                        span,
                    });
                }
                continue;
            }
            let disposable: Box<str> = format!("<dispose>{}_{}", span.start, index).into();
            let disposable_ref = || BoundExpr {
                kind: BoundExprKind::Local(disposable.clone()),
                ty: idisposable.clone(),
            };
            finally_stmts.push(BoundStmt {
                kind: BoundStmtKind::Local {
                    ty: idisposable.clone(),
                    declarators: alloc::vec![BoundDeclarator {
                        name: disposable.clone(),
                        initializer: Some(BoundExpr {
                            kind: BoundExprKind::TypeTest {
                                operation: lamella_syntax::ast::TypeTestOperation::As,
                                operand: Box::new(BoundExpr {
                                    kind: BoundExprKind::Local(name.clone()),
                                    ty: resource_ty.clone(),
                                }),
                                target: idisposable.clone(),
                            },
                            ty: idisposable.clone(),
                        }),
                    }],
                },
                span,
            });
            finally_stmts.push(BoundStmt {
                kind: BoundStmtKind::If {
                    condition: BoundExpr {
                        kind: BoundExprKind::Binary {
                            operator: lamella_syntax::ast::BinaryOperator::NotEqual,
                            left: Box::new(disposable_ref()),
                            right: Box::new(BoundExpr {
                                kind: BoundExprKind::Literal(Literal::Null),
                                ty: TypeSymbol::Special(SpecialType::Object),
                            }),
                            checked: false,
                        },
                        ty: TypeSymbol::Special(SpecialType::Boolean),
                    },
                    then_branch: Box::new(BoundStmt {
                        kind: BoundStmtKind::Expression(BoundExpr {
                            kind: BoundExprKind::Call {
                                callee: Box::new(BoundExpr {
                                    kind: BoundExprKind::MethodGroup {
                                        receiver: Box::new(disposable_ref()),
                                        name: dispose.name.clone(),
                                    },
                                    ty: TypeSymbol::Error,
                                }),
                                arguments: Vec::new(),
                                method: Some(dispose.clone()),
                            },
                            ty: dispose.return_type.clone(),
                        }),
                        span,
                    }),
                    else_branch: None,
                },
                span,
            });
        }
        let guarded = BoundStmt {
            kind: BoundStmtKind::Try {
                body: Box::new(bound_body),
                catches: Vec::new(),
                finally: Some(Box::new(BoundStmt {
                    kind: BoundStmtKind::Block(finally_stmts),
                    span: Span::HIDDEN,
                })),
            },
            span,
        };
        resource_decls.push(guarded);
        BoundStmtKind::Block(resource_decls)
    }

    /// Binds a `fixed (T* name = init) body`: `init` (an array/string) is pinned, and `name`
    /// is a `T*` bound (definitely assigned) in the body's scope.
    fn bind_fixed(
        &mut self,
        ty: &lamella_syntax::ast::TypeRef,
        name: &str,
        init: &Expr,
        body: &Stmt,
    ) -> BoundStmtKind {
        let pointer_ty = self.resolve_named_type(&bind_type(ty), ty.span);
        let element = match &pointer_ty {
            TypeSymbol::Pointer(inner) => (**inner).clone(),
            _ => TypeSymbol::Error,
        };
        let init = self.bind_expression(init);
        self.enter_scope();
        self.declare_local(name, pointer_ty);
        let body = Box::new(self.bind_statement(body));
        self.exit_scope();
        BoundStmtKind::Fixed {
            name: name.into(),
            element,
            init,
            body,
        }
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
        self.enter_loop();
        let body = Box::new(self.bind_statement(body));
        self.exit_loop();
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
        if declared.is_void() {
            self.report(Diagnostic::new(DiagnosticKind::VoidLocal, ty.span));
        }
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
                if matches!(&expr.kind, ExprKind::ArrayInitializer(_)) {
                    let (lengths, elements) = match self.bind_rectangular_array(expr, &declared, &[]) {
                        Some(rectangular) => rectangular,
                        None => (Vec::new(), self.bind_array_initializer(expr, &declared)),
                    };
                    return BoundExpr {
                        kind: BoundExprKind::ArrayCreation { lengths, elements },
                        ty: declared.clone(),
                    };
                }
                let value = self.bind_expression(expr);
                if value.ty.is_error() || !self.assignable(&value, &declared) {
                    self.exempt_local_from_unused(&declarator.name);
                }
                self.check_assignable(&value, &declared, declarator.span);
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

    /// Binds a local constant declaration `const T x = value;` (15.5.1): each initializer is a
    /// constant expression (14.15) folded at compile time and bound to the name, which then reads
    /// as that constant. A local constant has no storage, so the declaration emits nothing (an
    /// empty statement). A non-constant initializer is `CS0150`.
    fn bind_const_local(
        &mut self,
        ty: &TypeRef,
        declarators: &[VariableDeclarator],
    ) -> BoundStmtKind {
        let declared = self.resolve_named_type(&bind_type(ty), ty.span);
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
            let value = declarator.initializer.as_ref().map(|expr| {
                let bound = self.bind_expression(expr);
                self.check_assignable(&bound, &declared, declarator.span);
                self.convert(bound, &declared)
            });
            match value.as_ref().and_then(constant_literal_value) {
                Some(folded) => self.declare_const_local(&declarator.name, folded, declared.clone()),
                None => {
                    self.report(Diagnostic::new(DiagnosticKind::ConstantExpected, declarator.span));
                    self.declare_local(&declarator.name, declared.clone());
                }
            }
        }
        BoundStmtKind::Empty
    }

    fn bind_condition(&mut self, condition: &Expr) -> BoundExpr {
        let bound = self.bind_expression(condition);
        let boolean = TypeSymbol::Special(SpecialType::Boolean);
        if bound.ty.is_error() || crate::conversion::converts(self.model(), &bound.ty, &boolean) {
            return bound;
        }
        if self
            .user_conversion(&bound.ty, &boolean, "op_Implicit")
            .is_some()
        {
            return self.convert(bound, &boolean);
        }
        if let Some(call) = self.bind_operator_true(&bound) {
            return call;
        }
        self.report_no_implicit_conversion(&bound.ty, &boolean, condition.span);
        bound
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
    fn a_switch_section_whose_endpoint_is_reachable_falls_through() {

        assert_eq!(codes("switch (1) { case 0: if (1 > 0) break; }"), [8070]);
        assert_eq!(codes("switch (1) { case 0: break; }"), []);
        assert_eq!(codes("switch (1) { case 0: { break; } }"), []);
        assert_eq!(codes("switch (1) { case 0: if (1 > 0) break; else break; }"), []);
        assert_eq!(codes("switch (1) { case 0: case 1: break; }"), []);
        assert_eq!(codes("switch (1) { case 0: while (true) { } }"), []);
        assert_eq!(codes("switch (1) { case 0: while (true) { break; } }"), [8070]);
        assert_eq!(codes("switch (1) { case 0: goto case 1; case 1: break; }"), []);
    }

    #[test]
    fn well_typed_locals_and_conditions_are_clean() {
        assert_eq!(codes("int x = 1;"), []);
        assert_eq!(codes("long n = 1;"), []);
        assert_eq!(codes("while (true) ;"), []);
        assert_eq!(codes("{ int x = 1; int y = x + 2; }"), []);
    }

    #[test]
    fn switch_on_bool_is_gated_as_a_post_1_0_feature() {
        assert_eq!(codes("{ bool b = true; switch (b) { case true: break; } }"), [8022]);
        assert_eq!(codes("{ int n = 1; switch (n) { case 1: break; } }"), []);
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
        assert_eq!(codes("{ int n = 1; lock (n) { int m = n; } }"), [185]);
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
        assert_eq!(codes("throw;"), [156]);
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
