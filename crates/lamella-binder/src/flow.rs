//! Flow analysis (ECMA-334 1st ed, clause 12).

use crate::bound::{constant_int_value, constant_literal_value, BoundExpr, BoundExprKind};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::statement::{BoundStmt, BoundStmtKind, BoundSwitchLabel, BoundSwitchSection};
use crate::symbols::{Model, TypeKind};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;
use lamella_syntax::ast::{AssignmentOperator, Literal, UnaryOperator};
use lamella_syntax::span::Span;

/// Whether executing `stmt` always transfers control away rather than reaching its
/// endpoint -- the structural, label-blind view (every `goto` is taken to exit). Codegen
/// consumes this form; the `CS0161` test uses the label-aware [`method_body_always_exits`].
#[must_use]
pub fn always_exits(stmt: &BoundStmt) -> bool {
    exits(stmt, &BTreeSet::new())
}

/// The `CS0161` endpoint test for a whole method body: like [`always_exits`], except a `goto`
/// to an UNDEFINED label (already `CS0159`) does NOT count as exiting. Such a `goto` cannot
/// jump, so csc treats the endpoint -- and thus the method endpoint -- as reachable and reports
/// `CS0161` too (`{ goto Nowhere; }` is CS0159 + CS0161). A valid program has no undefined
/// labels, so the set below is empty and this is identical to [`always_exits`]: it can never
/// turn a valid value-returning method into a false CS0161.
#[must_use]
pub fn method_body_always_exits(body: &BoundStmt) -> bool {
    exits(body, &undefined_goto_labels(body))
}

/// The `goto` targets in `body` that name no declared label -- exactly the labels [`check_labels`]
/// reports `CS0159` for. Computed the same way (collect every declared label method-wide, then keep
/// the goto targets not among them), so the exit analysis and CS0159 agree on which `goto`s cannot
/// jump.
fn undefined_goto_labels(body: &BoundStmt) -> BTreeSet<Box<str>> {
    let mut declared: BTreeSet<Box<str>> = BTreeSet::new();
    visit_statements(body, &mut |stmt| {
        if let BoundStmtKind::Labeled { label, .. } = &stmt.kind {
            declared.insert(label.clone());
        }
    });
    let mut undefined: BTreeSet<Box<str>> = BTreeSet::new();
    visit_statements(body, &mut |stmt| {
        if let BoundStmtKind::Goto(label) = &stmt.kind {
            if !declared.contains(label) {
                undefined.insert(label.clone());
            }
        }
    });
    undefined
}

/// The core endpoint-reachability test, threading the set of undefined `goto` labels (empty for
/// the structural [`always_exits`]): whether `stmt` always transfers control away rather than
/// reaching its endpoint.
fn exits(stmt: &BoundStmt, undefined_labels: &BTreeSet<Box<str>>) -> bool {
    use BoundStmtKind as Kind;
    match &stmt.kind {
        Kind::Return(_) | Kind::Throw(_) => true,
        Kind::Goto(label) => !undefined_labels.contains(label),
        Kind::GotoCase(_) | Kind::GotoCaseString(_) | Kind::GotoDefault => true,
        Kind::Block(statements) => statements.iter().any(|s| exits(s, undefined_labels)),
        Kind::If {
            then_branch,
            else_branch: Some(else_branch),
            ..
        } => exits(then_branch, undefined_labels) && exits(else_branch, undefined_labels),
        Kind::While { condition, body } => is_const_true(condition) && !loop_breaks(body),
        Kind::For {
            condition, body, ..
        } => condition.as_ref().is_none_or(is_const_true) && !loop_breaks(body),
        Kind::DoWhile { body, condition } => {
            exits(body, undefined_labels) || (is_const_true(condition) && !loop_breaks(body))
        }
        Kind::Lock { body, .. } | Kind::Using { body, .. } | Kind::Fixed { body, .. } => {
            exits(body, undefined_labels)
        }
        Kind::Checked(inner) | Kind::Unchecked(inner) => exits(inner, undefined_labels),
        Kind::Labeled { body, .. } => exits(body, undefined_labels),
        Kind::Try {
            body,
            catches,
            finally,
        } => {
            finally
                .as_ref()
                .is_some_and(|block| exits(block, undefined_labels))
                || (exits(body, undefined_labels)
                    && catches
                        .iter()
                        .all(|catch| exits(&catch.body, undefined_labels)))
        }
        Kind::Switch { sections, .. } => {
            let has_default = sections
                .iter()
                .any(|section| section.labels.contains(&BoundSwitchLabel::Default));
            has_default
                && sections.iter().all(|section| {
                    section
                        .statements
                        .iter()
                        .any(|s| exits(s, undefined_labels))
                })
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

/// Reports `CS0168`/`CS0219` for a declared local that no use reads. Uses are resolved to a
/// specific DECLARATION by lexical scope (see [`UnusedScan`]), not matched by bare name, so a
/// shadowed or duplicated outer local that is assigned-never-read is caught even though another
/// local of the same name is read. `CS0219` "assigned but its value is never used" fires only when
/// the initializer is a compile-time CONSTANT -- csc does not warn a local initialized with a
/// non-constant expression (`s.Length`, `M()`), whose evaluation may matter -- and `CS0168`
/// "declared but never used" when there is no initializer at all. A local of an unresolved type
/// already carries CS0246, so it is tracked for scope resolution but never itself warned.
#[must_use]
pub fn check_unused_locals(body: &BoundStmt, also_used: &BTreeSet<Box<str>>) -> Vec<Diagnostic> {
    let mut scan = UnusedScan {
        scopes: alloc::vec![BTreeMap::new()],
        declared: Vec::new(),
        used: BTreeSet::new(),
    };
    scan.statement(body);
    let seeded: Vec<usize> = scan
        .declared
        .iter()
        .enumerate()
        .filter(|(_, decl)| also_used.contains(&decl.name))
        .map(|(index, _)| index)
        .collect();
    scan.used.extend(seeded);
    let UnusedScan { declared, used, .. } = scan;
    declared
        .into_iter()
        .enumerate()
        .filter(|(index, decl)| decl.warnable && !used.contains(index))
        .filter_map(|(_, decl)| {
            let kind = match decl.initializer {
                Some(true) => DiagnosticKind::UnusedLocalValue { name: decl.name },
                None => DiagnosticKind::UnusedLocal { name: decl.name },
                Some(false) => return None,
            };
            Some(Diagnostic::new(kind, decl.span))
        })
        .collect()
}

/// One declared local for the unused-local scan.
struct LocalDecl {
    name: Box<str>,
    span: Span,
    /// `None` = no initializer (CS0168), `Some(true)` = a constant initializer (CS0219),
    /// `Some(false)` = a non-constant initializer (no warning -- matching csc).
    initializer: Option<bool>,
    /// Whether this declaration is a warning candidate. A local of an unresolved type (already
    /// CS0246) is tracked for scope resolution -- so a use of its name resolves to it rather than
    /// leaking to an outer local -- but is never itself warned.
    warnable: bool,
}

/// Resolves each local USE to the declaration it reads -- the innermost in-scope local of that name,
/// the FIRST declaration winning within a single scope (csc's recovery for a duplicate) -- so a
/// declaration is warned only when NO use resolves to IT, not merely when its name is reused. Every
/// use of a uniquely-named local still resolves to its one declaration, so this is identical to
/// name-keying on all code without repeated names; it differs only for a shadow (CS0136), a
/// duplicate (CS0128), or a valid same-name reuse across disjoint scopes -- exactly the cases csc
/// keys per-declaration.
struct UnusedScan {
    /// A stack of lexical scopes, innermost last; each maps a local name to the index in `declared`
    /// of the first declaration of that name in the scope.
    scopes: Vec<BTreeMap<Box<str>, usize>>,
    declared: Vec<LocalDecl>,
    /// The indices in `declared` that some use resolves to.
    used: BTreeSet<usize>,
}

impl UnusedScan {
    /// The declaration a use of `name` reads: the innermost scope that declares it.
    fn resolve(&self, name: &str) -> Option<usize> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    /// Marks the declaration a use of `name` reads (if any) used.
    fn use_local(&mut self, name: &str) {
        if let Some(index) = self.resolve(name) {
            self.used.insert(index);
        }
    }

    /// Records a local in the current (innermost) scope. A later same-name declaration in the same
    /// scope (a duplicate) does not displace the first for resolution, but is still tracked so it can
    /// be warned as the dead one.
    fn declare(&mut self, name: &str, span: Span, initializer: Option<bool>, warnable: bool) {
        let index = self.declared.len();
        self.declared.push(LocalDecl {
            name: name.into(),
            span,
            initializer,
            warnable,
        });
        self.scopes
            .last_mut()
            .expect("a scope is always open")
            .entry(name.into())
            .or_insert(index);
    }

    /// Resolves every local use in `expr` against the current scopes.
    fn uses(&mut self, expr: &BoundExpr) {
        visit_local_uses(expr, &mut |name| self.use_local(name));
    }

    /// Walks `statements` as one fresh lexical scope (a block, a `switch` body).
    fn block(&mut self, statements: &[BoundStmt]) {
        self.scopes.push(BTreeMap::new());
        for statement in statements {
            self.statement(statement);
        }
        self.scopes.pop();
    }

    /// Walks `stmt`, declaring the locals it introduces into the current scope and resolving the
    /// uses it contains. Only a block and the `for`/`using`/`switch` bodies open a new scope; an
    /// embedded non-block statement cannot declare a local, so it needs none.
    fn statement(&mut self, stmt: &BoundStmt) {
        match &stmt.kind {
            BoundStmtKind::Local { ty, declarators } => {
                let warnable = !ty.is_error() && !ty.is_void();
                for declarator in declarators {
                    let initializer = declarator
                        .initializer
                        .as_ref()
                        .map(|init| constant_literal_value(init).is_some());
                    self.declare(&declarator.name, stmt.span, initializer, warnable);
                    if let Some(init) = &declarator.initializer {
                        self.uses(init);
                    }
                }
            }
            BoundStmtKind::Block(statements) => self.block(statements),
            BoundStmtKind::Expression(expr) => self.uses(expr),
            BoundStmtKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                self.uses(condition);
                self.statement(then_branch);
                if let Some(else_branch) = else_branch {
                    self.statement(else_branch);
                }
            }
            BoundStmtKind::While { condition, body } | BoundStmtKind::DoWhile { condition, body } => {
                self.uses(condition);
                self.statement(body);
            }
            BoundStmtKind::For {
                initializer,
                condition,
                iterators,
                body,
            } => {
                self.scopes.push(BTreeMap::new());
                for statement in initializer {
                    self.statement(statement);
                }
                if let Some(condition) = condition {
                    self.uses(condition);
                }
                for iterator in iterators {
                    self.uses(iterator);
                }
                self.statement(body);
                self.scopes.pop();
            }
            BoundStmtKind::ForEach {
                collection, body, ..
            } => {
                self.uses(collection);
                self.statement(body);
            }
            BoundStmtKind::Return(value) | BoundStmtKind::Throw(value) => {
                if let Some(value) = value {
                    self.uses(value);
                }
            }
            BoundStmtKind::Try {
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
            }
            BoundStmtKind::Switch {
                expression,
                sections,
            } => {
                self.uses(expression);
                self.scopes.push(BTreeMap::new());
                for section in sections {
                    for statement in &section.statements {
                        self.statement(statement);
                    }
                }
                self.scopes.pop();
            }
            BoundStmtKind::Lock { expression, body } => {
                self.uses(expression);
                self.statement(body);
            }
            BoundStmtKind::Fixed { init, body, .. } => {
                self.uses(init);
                self.statement(body);
            }
            BoundStmtKind::Using { resource, body } => {
                self.scopes.push(BTreeMap::new());
                for statement in resource {
                    self.statement(statement);
                }
                self.statement(body);
                self.scopes.pop();
            }
            BoundStmtKind::Checked(inner)
            | BoundStmtKind::Unchecked(inner)
            | BoundStmtKind::Labeled { body: inner, .. } => self.statement(inner),
            BoundStmtKind::Empty
            | BoundStmtKind::Error
            | BoundStmtKind::Break
            | BoundStmtKind::Continue
            | BoundStmtKind::Goto(_)
            | BoundStmtKind::GotoCase(_)
            | BoundStmtKind::GotoCaseString(_)
            | BoundStmtKind::GotoDefault => {}
        }
    }
}

/// Visits every local USE in `expr` -- a `Local` reference in any position, an assignment target
/// included (counting a target as a use only makes the unused check more conservative) -- calling
/// `f` with each local's name. Shared by [`collect_uses`], which gathers the names, and
/// [`UnusedScan`], which resolves each name to its declaration.
fn visit_local_uses(expr: &BoundExpr, f: &mut dyn FnMut(&str)) {
    match &expr.kind {
        BoundExprKind::Local(name) => f(name),
        BoundExprKind::Literal(_)
        | BoundExprKind::This
        | BoundExprKind::Base
        | BoundExprKind::TypeReference(_)
        | BoundExprKind::NamespaceReference(_)
        | BoundExprKind::TypeOf(_)
        | BoundExprKind::SizeOf(_)
        | BoundExprKind::Error => {}
        BoundExprKind::FieldAccess { receiver, .. }
        | BoundExprKind::PropertyAccess { receiver, .. }
        | BoundExprKind::MethodGroup { receiver, .. } => visit_local_uses(receiver, f),
        BoundExprKind::Ref { operand, .. }
        | BoundExprKind::Dereference { operand }
        | BoundExprKind::AddressOf { operand } => visit_local_uses(operand, f),
        BoundExprKind::MakeRef(operand) | BoundExprKind::RefType(operand) => {
            visit_local_uses(operand, f);
        }
        BoundExprKind::RefValue { reference, .. } => visit_local_uses(reference, f),
        BoundExprKind::ArgListValue => {}
        BoundExprKind::ArgListLiteral(arguments) => {
            for argument in arguments {
                visit_local_uses(argument, f);
            }
        }
        BoundExprKind::StackAlloc { count, .. } => visit_local_uses(count, f),
        BoundExprKind::Call {
            callee, arguments, ..
        } => {
            visit_local_uses(callee, f);
            for argument in arguments {
                visit_local_uses(argument, f);
            }
        }
        BoundExprKind::ElementAccess { receiver, indices } => {
            visit_local_uses(receiver, f);
            for index in indices {
                visit_local_uses(index, f);
            }
        }
        BoundExprKind::IndexerAccess {
            receiver, indices, ..
        } => {
            visit_local_uses(receiver, f);
            for index in indices {
                visit_local_uses(index, f);
            }
        }
        BoundExprKind::ArrayCreation { lengths, elements } => {
            for length in lengths {
                visit_local_uses(length, f);
            }
            for element in elements {
                visit_local_uses(element, f);
            }
        }
        BoundExprKind::ObjectCreation { arguments, .. } => {
            for argument in arguments {
                visit_local_uses(argument, f);
            }
        }
        BoundExprKind::DelegateCreation { receiver, .. } => {
            if let Some(receiver) = receiver {
                visit_local_uses(receiver, f);
            }
        }
        BoundExprKind::Binary { left, right, .. } => {
            visit_local_uses(left, f);
            visit_local_uses(right, f);
        }
        BoundExprKind::Unary { operand, .. } | BoundExprKind::Postfix { operand, .. } => {
            visit_local_uses(operand, f);
        }
        BoundExprKind::Cast { operand, .. }
        | BoundExprKind::TypeTest { operand, .. }
        | BoundExprKind::Conversion { operand, .. } => visit_local_uses(operand, f),
        BoundExprKind::Checked(inner) | BoundExprKind::Unchecked(inner) => {
            visit_local_uses(inner, f);
        }
        BoundExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => {
            visit_local_uses(condition, f);
            visit_local_uses(when_true, f);
            visit_local_uses(when_false, f);
        }
        BoundExprKind::Assignment { target, value, .. } => {
            visit_local_uses(target, f);
            visit_local_uses(value, f);
        }
    }
}

/// Gathers every local name that appears anywhere in `expr` into `used`. Used by the binder to
/// record locals referenced in `switch` case-label expressions (which the bound tree folds to
/// constants) so [`check_unused_locals`] can seed them as used.
pub(crate) fn collect_uses(expr: &BoundExpr, used: &mut BTreeSet<Box<str>>) {
    visit_local_uses(expr, &mut |name| {
        used.insert(name.into());
    });
}

/// A set of `(declaring-type dotted name, field name)` pairs -- the key a field access and
/// a field declaration both reduce to, for the `CS0414` "assigned but never used" warning.
type FieldSet = BTreeSet<(Box<str>, Box<str>)>;

/// The dotted name of a field's declaring type, the key both a field access and a field
/// declaration reduce to. Only a source-declared (`Named`) type owns a private field; any
/// other form yields `None` and is simply not tracked (so never mis-warned).
pub(crate) fn field_type_key(ty: &TypeSymbol) -> Option<Box<str>> {
    match ty {
        TypeSymbol::Named(parts) => {
            let mut name = String::new();
            for (index, part) in parts.iter().enumerate() {
                if index > 0 {
                    name.push('.');
                }
                name.push_str(part);
            }
            Some(name.into())
        }
        _ => None,
    }
}

/// Records every field read and write in `stmt`, keyed by the field's declaring type and
/// name, for `CS0414`. A WRITE is a field that is the direct target of a simple `=` (an
/// initializer write is recorded at the declaration); every other field access -- a compound
/// `+=` target, a `ref`/`out` argument, any read position -- is a READ. Counting every
/// ambiguous position as a read keeps the warning a safe subset of csc (it under-warns rather
/// than risk a false positive).
pub(crate) fn collect_field_accesses(stmt: &BoundStmt, reads: &mut FieldSet, writes: &mut FieldSet) {
    match &stmt.kind {
        BoundStmtKind::Local { declarators, .. } => {
            for declarator in declarators {
                if let Some(initializer) = &declarator.initializer {
                    collect_field_uses(initializer, reads, writes);
                }
            }
        }
        BoundStmtKind::Empty
        | BoundStmtKind::Error
        | BoundStmtKind::Break
        | BoundStmtKind::Continue
        | BoundStmtKind::Goto(_)
        | BoundStmtKind::GotoCase(_)
        | BoundStmtKind::GotoCaseString(_)
        | BoundStmtKind::GotoDefault => {}
        BoundStmtKind::Block(statements) => {
            for statement in statements {
                collect_field_accesses(statement, reads, writes);
            }
        }
        BoundStmtKind::Expression(expr) => collect_field_uses(expr, reads, writes),
        BoundStmtKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            collect_field_uses(condition, reads, writes);
            collect_field_accesses(then_branch, reads, writes);
            if let Some(else_branch) = else_branch {
                collect_field_accesses(else_branch, reads, writes);
            }
        }
        BoundStmtKind::While { condition, body } | BoundStmtKind::DoWhile { condition, body } => {
            collect_field_uses(condition, reads, writes);
            collect_field_accesses(body, reads, writes);
        }
        BoundStmtKind::For {
            initializer,
            condition,
            iterators,
            body,
        } => {
            for statement in initializer {
                collect_field_accesses(statement, reads, writes);
            }
            if let Some(condition) = condition {
                collect_field_uses(condition, reads, writes);
            }
            for iterator in iterators {
                collect_field_uses(iterator, reads, writes);
            }
            collect_field_accesses(body, reads, writes);
        }
        BoundStmtKind::ForEach {
            collection, body, ..
        } => {
            collect_field_uses(collection, reads, writes);
            collect_field_accesses(body, reads, writes);
        }
        BoundStmtKind::Return(value) | BoundStmtKind::Throw(value) => {
            if let Some(value) = value {
                collect_field_uses(value, reads, writes);
            }
        }
        BoundStmtKind::Try {
            body,
            catches,
            finally,
        } => {
            collect_field_accesses(body, reads, writes);
            for catch in catches {
                collect_field_accesses(&catch.body, reads, writes);
            }
            if let Some(finally) = finally {
                collect_field_accesses(finally, reads, writes);
            }
        }
        BoundStmtKind::Switch {
            expression,
            sections,
        } => {
            collect_field_uses(expression, reads, writes);
            for section in sections {
                for statement in &section.statements {
                    collect_field_accesses(statement, reads, writes);
                }
            }
        }
        BoundStmtKind::Lock { expression, body } => {
            collect_field_uses(expression, reads, writes);
            collect_field_accesses(body, reads, writes);
        }
        BoundStmtKind::Fixed { init, body, .. } => {
            collect_field_uses(init, reads, writes);
            collect_field_accesses(body, reads, writes);
        }
        BoundStmtKind::Using { resource, body } => {
            for statement in resource {
                collect_field_accesses(statement, reads, writes);
            }
            collect_field_accesses(body, reads, writes);
        }
        BoundStmtKind::Checked(inner)
        | BoundStmtKind::Unchecked(inner)
        | BoundStmtKind::Labeled { body: inner, .. } => {
            collect_field_accesses(inner, reads, writes);
        }
    }
}

/// Records the field reads and writes in `expr` (see [`collect_field_accesses`]). Also the
/// entry point for a field initializer's own expression.
/// Marks a field-access expression's field as WRITTEN. Used for the in-place assignment forms --
/// a compound `+=`, a `ref`/`out` argument, an address-of, and `++`/`--` -- so a field mutated only
/// that way is not mistaken for read-never-written (which CS0649 would otherwise flag on valid code).
fn mark_field_write(expr: &BoundExpr, writes: &mut FieldSet) {
    if let BoundExprKind::FieldAccess {
        field: Some(field), ..
    } = &expr.kind
    {
        if let Some(key) = field_type_key(&field.declaring_type) {
            writes.insert((key, field.name.clone()));
        }
    }
}

pub(crate) fn collect_field_uses(expr: &BoundExpr, reads: &mut FieldSet, writes: &mut FieldSet) {
    match &expr.kind {
        BoundExprKind::Assignment {
            operator,
            target,
            value,
            ..
        } => {
            if matches!(operator, AssignmentOperator::Assign) {
                if let BoundExprKind::FieldAccess {
                    receiver,
                    field: Some(field),
                    ..
                } = &target.kind
                {
                    if let Some(key) = field_type_key(&field.declaring_type) {
                        writes.insert((key, field.name.clone()));
                    }
                    collect_field_uses(receiver, reads, writes);
                    collect_field_uses(value, reads, writes);
                    return;
                }
            } else {
                mark_field_write(target, writes);
            }
            collect_field_uses(target, reads, writes);
            collect_field_uses(value, reads, writes);
        }
        BoundExprKind::FieldAccess {
            receiver, field, ..
        } => {
            if let Some(field) = field {
                if let Some(key) = field_type_key(&field.declaring_type) {
                    reads.insert((key, field.name.clone()));
                }
            }
            collect_field_uses(receiver, reads, writes);
        }
        BoundExprKind::Literal(_)
        | BoundExprKind::This
        | BoundExprKind::Base
        | BoundExprKind::Local(_)
        | BoundExprKind::TypeReference(_)
        | BoundExprKind::NamespaceReference(_)
        | BoundExprKind::TypeOf(_)
        | BoundExprKind::SizeOf(_)
        | BoundExprKind::Error => {}
        BoundExprKind::PropertyAccess { receiver, .. }
        | BoundExprKind::MethodGroup { receiver, .. } => collect_field_uses(receiver, reads, writes),
        BoundExprKind::Ref { operand, .. } | BoundExprKind::AddressOf { operand } => {
            mark_field_write(operand, writes);
            collect_field_uses(operand, reads, writes);
        }
        BoundExprKind::Dereference { operand } => collect_field_uses(operand, reads, writes),
        BoundExprKind::MakeRef(operand) | BoundExprKind::RefType(operand) => {
            collect_field_uses(operand, reads, writes);
        }
        BoundExprKind::RefValue { reference, .. } => collect_field_uses(reference, reads, writes),
        BoundExprKind::ArgListValue => {}
        BoundExprKind::ArgListLiteral(arguments) => {
            for argument in arguments {
                collect_field_uses(argument, reads, writes);
            }
        }
        BoundExprKind::StackAlloc { count, .. } => collect_field_uses(count, reads, writes),
        BoundExprKind::Call {
            callee, arguments, ..
        } => {
            collect_field_uses(callee, reads, writes);
            for argument in arguments {
                collect_field_uses(argument, reads, writes);
            }
        }
        BoundExprKind::ElementAccess { receiver, indices } => {
            collect_field_uses(receiver, reads, writes);
            for index in indices {
                collect_field_uses(index, reads, writes);
            }
        }
        BoundExprKind::IndexerAccess {
            receiver, indices, ..
        } => {
            collect_field_uses(receiver, reads, writes);
            for index in indices {
                collect_field_uses(index, reads, writes);
            }
        }
        BoundExprKind::ArrayCreation { lengths, elements } => {
            for length in lengths {
                collect_field_uses(length, reads, writes);
            }
            for element in elements {
                collect_field_uses(element, reads, writes);
            }
        }
        BoundExprKind::ObjectCreation { arguments, .. } => {
            for argument in arguments {
                collect_field_uses(argument, reads, writes);
            }
        }
        BoundExprKind::DelegateCreation { receiver, .. } => {
            if let Some(receiver) = receiver {
                collect_field_uses(receiver, reads, writes);
            }
        }
        BoundExprKind::Binary { left, right, .. } => {
            collect_field_uses(left, reads, writes);
            collect_field_uses(right, reads, writes);
        }
        BoundExprKind::Postfix { operand, .. } => {
            mark_field_write(operand, writes);
            collect_field_uses(operand, reads, writes);
        }
        BoundExprKind::Unary { operator, operand } => {
            if matches!(
                operator,
                UnaryOperator::PreIncrement | UnaryOperator::PreDecrement
            ) {
                mark_field_write(operand, writes);
            }
            collect_field_uses(operand, reads, writes);
        }
        BoundExprKind::Cast { operand, .. }
        | BoundExprKind::TypeTest { operand, .. }
        | BoundExprKind::Conversion { operand, .. } => collect_field_uses(operand, reads, writes),
        BoundExprKind::Checked(inner) | BoundExprKind::Unchecked(inner) => {
            collect_field_uses(inner, reads, writes);
        }
        BoundExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => {
            collect_field_uses(condition, reads, writes);
            collect_field_uses(when_true, reads, writes);
            collect_field_uses(when_false, reads, writes);
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
    let mut goto_targets: BTreeSet<Box<str>> = BTreeSet::new();
    visit_statements(body, &mut |stmt| {
        if let BoundStmtKind::Goto(label) = &stmt.kind {
            goto_targets.insert(label.clone());
        }
    });
    let mut check = Unreachable {
        diagnostics: Vec::new(),
        goto_targets,
    };
    check.statement(body);
    check.diagnostics
}

struct Unreachable {
    diagnostics: Vec<Diagnostic>,
    /// The labels that some `goto` targets; a labeled statement among them is a jump landing
    /// point, so it is reachable even after a statement whose control does not fall through.
    goto_targets: BTreeSet<Box<str>>,
}

impl Unreachable {
    /// Processes a statement list, flagging only the first unreachable statement (as
    /// csc does). Returns whether control can reach the end of the list.
    fn block(&mut self, statements: &[BoundStmt]) -> bool {
        let mut reachable = true;
        for statement in statements {
            if !reachable && self.is_goto_target(statement) {
                reachable = true;
            }
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

    /// Whether `stmt` is a labeled statement whose label some `goto` targets.
    fn is_goto_target(&self, stmt: &BoundStmt) -> bool {
        matches!(&stmt.kind, BoundStmtKind::Labeled { label, .. } if self.goto_targets.contains(label))
    }

    /// Whether control can reach the end point of `stmt`, given its start is reached.
    fn statement(&mut self, stmt: &BoundStmt) -> bool {
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
            Kind::Lock { body, .. } | Kind::Using { body, .. } | Kind::Fixed { body, .. } => {
                self.statement(body)
            }
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
        Kind::Block(statements) => {
            for statement in statements {
                if loop_breaks(statement) {
                    return true;
                }
                if always_exits(statement) {
                    return false;
                }
            }
            false
        }
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
        Kind::Lock { body, .. } | Kind::Using { body, .. } | Kind::Fixed { body, .. } => {
            loop_breaks(body)
        }
        Kind::Checked(inner) | Kind::Unchecked(inner) | Kind::Labeled { body: inner, .. } => {
            loop_breaks(inner)
        }
        Kind::Return(_)
        | Kind::Throw(_)
        | Kind::Continue
        | Kind::Goto(_)
        | Kind::GotoCase(_)
        | Kind::GotoCaseString(_)
        | Kind::GotoDefault
        | Kind::Expression(_)
        | Kind::Local { .. }
        | Kind::Empty
        | Kind::Error => false,
    }
}

/// Visits `stmt` and every statement nested within it, depth-first.
fn visit_statements<'a>(stmt: &'a BoundStmt, visit: &mut impl FnMut(&'a BoundStmt)) {
    use BoundStmtKind as K;
    visit(stmt);
    match &stmt.kind {
        K::Block(statements) => statements.iter().for_each(|s| visit_statements(s, visit)),
        K::If {
            then_branch,
            else_branch,
            ..
        } => {
            visit_statements(then_branch, visit);
            if let Some(else_branch) = else_branch {
                visit_statements(else_branch, visit);
            }
        }
        K::While { body, .. } | K::DoWhile { body, .. } | K::ForEach { body, .. } => {
            visit_statements(body, visit);
        }
        K::For {
            initializer, body, ..
        } => {
            initializer.iter().for_each(|s| visit_statements(s, visit));
            visit_statements(body, visit);
        }
        K::Try {
            body,
            catches,
            finally,
        } => {
            visit_statements(body, visit);
            catches.iter().for_each(|c| visit_statements(&c.body, visit));
            if let Some(finally) = finally {
                visit_statements(finally, visit);
            }
        }
        K::Switch { sections, .. } => sections
            .iter()
            .for_each(|s| s.statements.iter().for_each(|s| visit_statements(s, visit))),
        K::Lock { body, .. } | K::Fixed { body, .. } => visit_statements(body, visit),
        K::Using { resource, body } => {
            resource.iter().for_each(|s| visit_statements(s, visit));
            visit_statements(body, visit);
        }
        K::Checked(inner) | K::Unchecked(inner) | K::Labeled { body: inner, .. } => {
            visit_statements(inner, visit);
        }
        _ => {}
    }
}

/// Whether a switch SECTION's statement list can complete normally -- control reaching the end of
/// the section, to fall into the next one (`CS0163`) or out of the switch entirely (`CS8070`).
///
/// This cannot reuse [`always_exits`], and the reason is the whole subtlety: a `break` LEAVES A
/// SWITCH without exiting the method, so it is the commonest way a section legitimately ends and
/// `exits` deliberately does not count it. `continue` is the same. Everything else defers to
/// `always_exits`, which is what makes a nested loop or switch behave correctly -- it CAPTURES its
/// own `break`, so `while (true) { break; }` inside a section lets that section complete (csc
/// agrees: `CS8070`) while `while (true) { }` does not.
#[must_use]
pub fn switch_section_completes(statements: &[BoundStmt]) -> bool {
    !statements.iter().any(section_transfers)
}

/// Whether a statement definitely transfers control away from its switch section.
fn section_transfers(stmt: &BoundStmt) -> bool {
    use BoundStmtKind as Kind;
    match &stmt.kind {
        Kind::Break | Kind::Continue => true,
        Kind::If {
            then_branch,
            else_branch: Some(else_branch),
            ..
        } => section_transfers(then_branch) && section_transfers(else_branch),
        Kind::If { .. } => false,
        Kind::Block(statements) => statements.iter().any(section_transfers),
        Kind::Checked(inner) | Kind::Unchecked(inner) | Kind::Labeled { body: inner, .. } => {
            section_transfers(inner)
        }
        Kind::Lock { body, .. } | Kind::Using { body, .. } | Kind::Fixed { body, .. } => {
            section_transfers(body)
        }
        _ => always_exits(stmt),
    }
}

/// Reports `CS0140` (a label declared twice) and `CS0159` (a `goto` to a label that does
/// not exist) within one method body -- labels share a single method-wide scope (8.7.1).
#[must_use]
pub fn check_labels(body: &BoundStmt) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let mut labels: BTreeSet<Box<str>> = BTreeSet::new();
    let mut label_decls: Vec<(Box<str>, lamella_syntax::span::Span)> = Vec::new();
    visit_statements(body, &mut |stmt| {
        if let BoundStmtKind::Labeled { label, .. } = &stmt.kind {
            if labels.insert(label.clone()) {
                label_decls.push((label.clone(), stmt.span));
            } else {
                diagnostics.push(Diagnostic::new(
                    DiagnosticKind::DuplicateLabel {
                        label: label.clone(),
                    },
                    stmt.span,
                ));
            }
        }
    });
    let mut goto_targets: BTreeSet<Box<str>> = BTreeSet::new();
    visit_statements(body, &mut |stmt| {
        if let BoundStmtKind::Goto(label) = &stmt.kind {
            goto_targets.insert(label.clone());
            if !labels.contains(label) {
                diagnostics.push(Diagnostic::new(
                    DiagnosticKind::UndefinedLabel {
                        label: label.clone(),
                    },
                    stmt.span,
                ));
            }
        }
    });
    for (label, span) in label_decls {
        if !goto_targets.contains(&label) {
            diagnostics.push(Diagnostic::new(DiagnosticKind::UnreferencedLabel, span));
        }
    }
    diagnostics
}

/// The section indices of a switch that a jump can land in: a `goto case`/`goto default`
/// reaching a labeled section, or a `goto label` reaching a named label declared inside one.
/// Under a constant governing value a section the constant does not select is still reachable
/// -- and so still contributes to the switch's definite-assignment intersection -- when a jump
/// targets it. Keeping a jump-targeted section is the conservative direction (it matches the
/// analysis's prior, jump-blind behavior); only a section that NO jump can reach and no case
/// selects is dropped. Nested switches are descended for `goto case`/`goto default` too, which
/// can only over-attribute (keep a section that a constant would otherwise drop), never the
/// reverse -- so it cannot turn a real diagnostic into a false one.
fn switch_jump_targets(sections: &[BoundSwitchSection]) -> BTreeSet<usize> {
    let mut case_section: BTreeMap<i64, usize> = BTreeMap::new();
    let mut default_section: Option<usize> = None;
    let mut label_section: BTreeMap<Box<str>, usize> = BTreeMap::new();
    for (index, section) in sections.iter().enumerate() {
        for label in &section.labels {
            match label {
                BoundSwitchLabel::Case(value) => {
                    case_section.insert(*value, index);
                }
                BoundSwitchLabel::Default => default_section = Some(index),
                _ => {}
            }
        }
        for statement in &section.statements {
            visit_statements(statement, &mut |stmt| {
                if let BoundStmtKind::Labeled { label, .. } = &stmt.kind {
                    label_section.insert(label.clone(), index);
                }
            });
        }
    }
    let mut targets = BTreeSet::new();
    for section in sections {
        for statement in &section.statements {
            visit_statements(statement, &mut |stmt| match &stmt.kind {
                BoundStmtKind::GotoCase(value) => {
                    if let Some(&index) = case_section.get(value) {
                        targets.insert(index);
                    }
                }
                BoundStmtKind::GotoDefault => {
                    if let Some(index) = default_section {
                        targets.insert(index);
                    }
                }
                BoundStmtKind::Goto(label) => {
                    if let Some(&index) = label_section.get(label) {
                        targets.insert(index);
                    }
                }
                _ => {}
            });
        }
    }
    targets
}

/// Which `switch` sections are statically reachable when the governing value is a compile-time
/// constant (clause 12, 15.7): the section the constant selects (its `case`, else the `default`),
/// plus every section a `goto case`/`goto default` targets. `None` means the value is not a
/// constant, so every section is reachable. A returned `Some(v)` has one entry per section, in
/// order; `v[i] == false` marks section `i` unreachable. Definite assignment drops an unreachable
/// section from its intersection (its assignments cannot count), and debug emission gives it
/// hidden sequence points (a debugger steps over its dead statements, as csc does) -- both read
/// this one computation so the two agree on exactly which sections a constant switch can enter.
#[must_use]
pub fn switch_section_reachability(
    expression: &BoundExpr,
    sections: &[BoundSwitchSection],
) -> Option<Vec<bool>> {
    let value = constant_int_value(expression)?;
    let has_default = sections
        .iter()
        .any(|section| section.labels.contains(&BoundSwitchLabel::Default));
    let entry = sections
        .iter()
        .position(|section| {
            section
                .labels
                .iter()
                .any(|label| matches!(label, BoundSwitchLabel::Case(v) if *v == value))
        })
        .or_else(|| {
            has_default.then(|| {
                sections
                    .iter()
                    .position(|section| section.labels.contains(&BoundSwitchLabel::Default))
                    .expect("has_default")
            })
        });
    Some(match entry {
        None => alloc::vec![false; sections.len()],
        Some(entry) => {
            let targets = switch_jump_targets(sections);
            (0..sections.len())
                .map(|index| index == entry || targets.contains(&index))
                .collect()
        }
    })
}

/// Reports `CS0165` for every read of a local that is not definitely assigned on
/// all paths to it (clause 12, Annex A). `parameters` start definitely assigned.
/// `model` distinguishes a struct (whose field assignment assigns the local) from a
/// reference type (whose field assignment reads the local).
#[must_use]
pub fn check_definite_assignment(
    body: &BoundStmt,
    parameters: &[Box<str>],
    out_parameters: &[Box<str>],
    model: &Model,
) -> Vec<Diagnostic> {
    let mut analyzer = Analyzer {
        diagnostics: Vec::new(),
        model,
        break_frames: Vec::new(),
        out_parameters: out_parameters.iter().cloned().collect(),
        unassigned_out: BTreeSet::new(),
    };
    let mut assigned: Assigned = parameters.iter().cloned().collect();
    for out in out_parameters {
        assigned.remove(out);
    }
    let flow = analyzer.statement(body, assigned);
    if let Flow::Reaches(final_assigned) = flow {
        analyzer.record_unassigned_out(&final_assigned);
    }
    let unassigned: Vec<Box<str>> = analyzer.unassigned_out.iter().cloned().collect();
    for parameter in unassigned {
        analyzer.diagnostics.push(Diagnostic::new(
            DiagnosticKind::OutParameterNotAssigned { parameter },
            body.span,
        ));
    }
    analyzer.diagnostics
}

struct Analyzer<'a> {
    diagnostics: Vec<Diagnostic>,
    model: &'a Model,
    /// A stack of break-target frames, one per enclosing `switch`/loop. A `break`
    /// records the definitely-assigned set at that point into the top frame; a `switch`
    /// then intersects its breaks (and fall-throughs) to know what is assigned after it.
    break_frames: Vec<Vec<Assigned>>,
    /// The method's `out` parameters, each of which must be assigned before every exit (CS0177).
    out_parameters: BTreeSet<Box<str>>,
    /// The `out` parameters found unassigned at some exit (a `return` or the reachable endpoint),
    /// accumulated across the walk and reported once each at the end.
    unassigned_out: BTreeSet<Box<str>>,
}

impl Analyzer<'_> {
    /// Whether `ty` is a struct, whose fields are assigned in place (12.x).
    fn is_struct(&self, ty: &TypeSymbol) -> bool {
        self.model
            .get_by_symbol(ty)
            .is_some_and(|info| info.kind == TypeKind::Struct)
    }

    /// Records every `out` parameter not in `assigned` as unassigned at this exit (CS0177). A
    /// `throw` is not an exit for this purpose -- an out parameter need not be assigned before it.
    fn record_unassigned_out(&mut self, assigned: &Assigned) {
        for parameter in &self.out_parameters {
            if !assigned.contains(parameter) {
                self.unassigned_out.insert(parameter.clone());
            }
        }
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
                let (_, breaks) = self.statement_in_loop(body, assigned.clone());
                if is_const_true(condition) {
                    Self::after_endless_loop(breaks)
                } else {
                    Flow::Reaches(assigned)
                }
            }
            BoundStmtKind::DoWhile { body, condition } => {
                let (flow, breaks) = self.statement_in_loop(body, assigned);
                match flow {
                    Flow::Exits => Self::after_endless_loop(breaks),
                    Flow::Reaches(mut assigned) => {
                        self.expression(condition, &mut assigned, span);
                        if is_const_true(condition) {
                            Self::after_endless_loop(breaks)
                        } else {
                            Flow::Reaches(assigned)
                        }
                    }
                }
            }
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
                let (_, breaks) = self.statement_in_loop(body, assigned.clone());
                for iterator in iterators {
                    let mut iterator_set = assigned.clone();
                    self.expression(iterator, &mut iterator_set, span);
                }
                if infinite {
                    Self::after_endless_loop(breaks)
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
                let (_, _breaks) = self.statement_in_loop(body, body_set);
                Flow::Reaches(assigned)
            }
            BoundStmtKind::Return(value) => {
                let mut assigned = assigned;
                if let Some(value) = value {
                    self.expression(value, &mut assigned, span);
                }
                self.record_unassigned_out(&assigned);
                Flow::Exits
            }
            BoundStmtKind::Throw(value) => {
                if let Some(value) = value {
                    let mut assigned = assigned;
                    self.expression(value, &mut assigned, span);
                }
                Flow::Exits
            }
            BoundStmtKind::Break => {
                if let Some(frame) = self.break_frames.last_mut() {
                    frame.push(assigned);
                }
                Flow::Exits
            }
            BoundStmtKind::Continue
            | BoundStmtKind::Goto(_)
            | BoundStmtKind::GotoCase(_)
            | BoundStmtKind::GotoCaseString(_)
            | BoundStmtKind::GotoDefault => Flow::Exits,
            BoundStmtKind::Switch {
                expression,
                sections,
            } => {
                let mut assigned = assigned;
                self.expression(expression, &mut assigned, span);
                let has_default = sections
                    .iter()
                    .any(|section| section.labels.contains(&BoundSwitchLabel::Default));
                let reachable = switch_section_reachability(expression, sections);
                self.break_frames.push(Vec::new());
                let mut after = Flow::Exits;
                for (index, section) in sections.iter().enumerate() {
                    if reachable.as_ref().is_some_and(|r| !r[index]) {
                        continue;
                    }
                    if let Flow::Reaches(set) = self.block(&section.statements, assigned.clone()) {
                        after = merge(after, Flow::Reaches(set));
                    }
                }
                for set in self.break_frames.pop().unwrap_or_default() {
                    after = merge(after, Flow::Reaches(set));
                }
                let can_skip = match &reachable {
                    Some(reachable) => reachable.iter().all(|&reachable| !reachable),
                    None => !has_default,
                };
                if can_skip {
                    merge(after, Flow::Reaches(assigned))
                } else {
                    after
                }
            }
            BoundStmtKind::Try {
                body,
                catches,
                finally,
            } => {
                let mut end = self.statement(body, assigned.clone());
                for catch in catches {
                    let mut catch_set = assigned.clone();
                    if let Some(name) = &catch.name {
                        catch_set.insert(name.clone());
                    }
                    let reached = self.statement(&catch.body, catch_set);
                    end = merge(end, reached);
                }
                match finally {
                    Some(finally) => match (end, self.statement(finally, assigned)) {
                        (Flow::Reaches(mut end), Flow::Reaches(finally)) => {
                            end.extend(finally);
                            Flow::Reaches(end)
                        }
                        _ => Flow::Exits,
                    },
                    None => end,
                }
            }
            BoundStmtKind::Lock { expression, body } => {
                let mut assigned = assigned;
                self.expression(expression, &mut assigned, span);
                self.statement(body, assigned)
            }
            BoundStmtKind::Fixed {
                name, init, body, ..
            } => {
                let mut assigned = assigned;
                self.expression(init, &mut assigned, span);
                assigned.insert(name.clone());
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

    /// Analyzes a loop body inside its own break frame, so a `break` targeting this
    /// loop is captured here and does not leak into an enclosing switch's exit paths.
    /// The captured breaks are discarded -- a loop's endpoint reachability is decided by
    /// its condition, not its breaks (an over-approximation that never rejects).
    /// Analyzes a loop body and returns its flow together with the assigned set captured at each
    /// `break` that targets THIS loop. The frame was pushed and discarded before, which is what
    /// made an endless loop's exit path invisible: control leaves `while (true)` only through a
    /// break, so those sets are the only thing that says what is assigned afterwards.
    fn statement_in_loop(&mut self, body: &BoundStmt, assigned: Assigned) -> (Flow, Vec<Assigned>) {
        self.break_frames.push(Vec::new());
        let flow = self.statement(body, assigned);
        (flow, self.break_frames.pop().unwrap_or_default())
    }

    /// Where control lands after an ENDLESS loop: nowhere if nothing breaks out of it, else the
    /// merge of every break's assigned set -- so only a local assigned before EVERY break survives.
    fn after_endless_loop(breaks: Vec<Assigned>) -> Flow {
        let mut after = Flow::Exits;
        for set in breaks {
            after = merge(after, Flow::Reaches(set));
        }
        after
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
            | BoundExprKind::TypeOf(_)
            | BoundExprKind::SizeOf(_)
            | BoundExprKind::Error => {}
            BoundExprKind::FieldAccess { receiver, .. }
            | BoundExprKind::PropertyAccess { receiver, .. }
            | BoundExprKind::MethodGroup { receiver, .. } => {
                self.expression(receiver, assigned, span);
            }
            BoundExprKind::Dereference { operand } => {
                self.expression(operand, assigned, span);
            }
            BoundExprKind::AddressOf { operand } => {
                if let BoundExprKind::Local(name) = &operand.kind {
                    assigned.insert(name.clone());
                } else {
                    self.expression(operand, assigned, span);
                }
            }
            BoundExprKind::StackAlloc { count, .. } => {
                self.expression(count, assigned, span);
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
            BoundExprKind::IndexerAccess {
                receiver, indices, ..
            } => {
                self.expression(receiver, assigned, span);
                for index in indices {
                    self.expression(index, assigned, span);
                }
            }
            BoundExprKind::ArrayCreation { lengths, elements } => {
                for length in lengths {
                    self.expression(length, assigned, span);
                }
                for element in elements {
                    self.expression(element, assigned, span);
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
            BoundExprKind::Ref { out, operand } => {
                if *out {
                    if let BoundExprKind::Local(name) = &operand.kind {
                        assigned.insert(name.clone());
                    }
                } else {
                    self.expression(operand, assigned, span);
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
            BoundExprKind::MakeRef(operand) => {
                self.expression(operand, assigned, span);
                if let BoundExprKind::Local(name) = &operand.kind {
                    assigned.insert(name.clone());
                }
            }
            BoundExprKind::RefType(reference) => {
                self.expression(reference, assigned, span);
            }
            BoundExprKind::RefValue { reference, .. } => {
                self.expression(reference, assigned, span);
            }
            BoundExprKind::ArgListValue => {}
            BoundExprKind::ArgListLiteral(arguments) => {
                for argument in arguments {
                    self.expression(argument, assigned, span);
                }
            }
            BoundExprKind::Conditional {
                condition,
                when_true,
                when_false,
            } => {
                self.expression(condition, assigned, span);
                let mut if_true = assigned.clone();
                let mut if_false = assigned.clone();
                self.expression(when_true, &mut if_true, span);
                self.expression(when_false, &mut if_false, span);
                *assigned = if_true.intersection(&if_false).cloned().collect();
            }
            BoundExprKind::Assignment {
                operator,
                target,
                value,
                ..
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
