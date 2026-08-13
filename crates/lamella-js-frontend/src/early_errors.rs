//! Early errors that need a TREE rather than a token.

use crate::{format, String, Vec};
use crate::ast::*;
use crate::diagnostic::{DiagnosticKind, Diagnostics, Phase};
use crate::source::Span;

/// Checks a parsed program and reports what it finds.
pub fn check(program: &Program, diagnostics: &mut Diagnostics) {
    let mut checker = Checker {
        diagnostics,
        labels: Vec::new(),
        iteration_depth: 0,
        switch_depth: 0,
        super_property_allowed: false,
        super_call_allowed: false,
        new_target_allowed: false,
    };
    checker.check_block_scope(&program.body, ScopeKind::TopLevel);
    for statement in &program.body {
        checker.statement(statement);
    }
}

struct Checker<'a> {
    diagnostics: &'a mut Diagnostics,
    /// Labels in force, innermost last, each with whether it labels an iteration statement.
    labels: Vec<(String, bool)>,
    iteration_depth: usize,
    switch_depth: usize,
    /// Whether `super.x` is legal here: inside any method, class or object-literal.
    ///
    /// **A COUNTER WAS THE WRONG SHAPE AND IT WAS WRONG IN BOTH DIRECTIONS.** Nesting does not
    /// accumulate an allowance -- it REPLACES one. An ordinary `function () {}` written inside a
    /// method has no `super` of its own, so the allowance must go back to false there and a counter
    /// left it standing; and a class nested inside a method restores it on the way out, which a
    /// saved-and-restored flag does correctly and a decrement only does by luck. An arrow is the one
    /// boundary that does NOT reset either flag, because it has no `super` binding of its own and
    /// reads the enclosing method's.
    super_property_allowed: bool,
    /// Whether `super(...)` is legal here: **only inside the constructor of a DERIVED class**.
    ///
    /// SEPARATE FROM THE PROPERTY FLAG BECAUSE THE TWO RULES ARE NOT THE SAME RULE, and one flag
    /// for both accepts `super()` in every method of every class -- which is a SyntaxError the
    /// standard states for each method form separately. `class C { m() { super(); } }` parsed
    /// happily here, and so did `class C { constructor() { super(); } }` with no `extends`.
    super_call_allowed: bool,
    /// Whether `new.target` is legal here: inside any FUNCTION, and nowhere else.
    ///
    /// 16.1.1: *"It is a Syntax Error if StatementList Contains NewTarget"* for a Script's own
    /// body. `Contains` stops at a function boundary and does NOT stop at an ARROW -- an arrow has
    /// no `new.target` of its own and reads the enclosing function's -- so `() => new.target` at the
    /// top level is refused and the identical text inside any `function` is fine. Same save-and-
    /// restore shape as the two `super` flags, and reset in the same two places, because it is the
    /// same kind of question: what construct am I standing in.
    new_target_allowed: bool,
}

/// What a name was declared as, for the redeclaration rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BindingKind {
    Lexical,
    Var,
}

/// Which kind of scope a statement list is.
///
/// # WARNING: A FUNCTION DECLARATION CHANGES CATEGORY WITH ITS SURROUNDINGS
///
/// At the top level of a script or a function body a function declaration is **var-scoped**, so
/// `var f; function f() {}` is perfectly legal and extremely common. Inside a BLOCK it is lexically
/// scoped, so the same pair is a redeclaration. Treating it as lexical everywhere -- the obvious
/// simplification, and what the first version did -- rejected 14 legal programs in the corpus,
/// including a test whose whole subject was that the pair is allowed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScopeKind {
    /// A script or a function body.
    TopLevel,
    /// A block, a switch case list, or a try/catch/finally block.
    Block,
}

impl Checker<'_> {
    fn error(&mut self, span: Span, message: impl Into<String>) {
        self.diagnostics.error(Phase::Early, DiagnosticKind::EarlyError, span, message);
    }

    /// The redeclaration rules for one statement list.
    ///
    /// # THE ASYMMETRY IS THE RULE, AND IT IS EASY TO GET BACKWARDS
    ///
    /// Two `var`s of the same name are **fine** -- `var a; var a;` is legal everywhere and always has
    /// been. Two lexical declarations are an error, and so is a lexical declaration colliding with a
    /// `var` **in either order**. A check that simply forbade duplicates would reject an enormous
    /// amount of working code; a check that only compared lexical against lexical would miss half
    /// the collisions.
    fn check_block_scope(&mut self, body: &[Statement], scope: ScopeKind) {
        let mut declared: Vec<(String, BindingKind, Span)> = Vec::new();
        for statement in body {
            collect_top_level_declarations(statement, scope, &mut declared);
        }
        let mut seen: Vec<(String, BindingKind)> = Vec::new();
        let mut collisions: Vec<(String, Span)> = Vec::new();
        for (name, kind, span) in &declared {
            if let Some((_, previous)) = seen.iter().find(|(seen_name, _)| seen_name == name) {
                if !(*previous == BindingKind::Var && *kind == BindingKind::Var) {
                    collisions.push((name.clone(), *span));
                }
            }
            seen.push((name.clone(), *kind));
        }
        for (name, span) in collisions {
            self.error(span, format!("`{name}` has already been declared in this scope"));
        }
    }

    fn statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Block { body, .. } => {
                self.check_block_scope(body, ScopeKind::Block);
                for inner in body {
                    self.statement(inner);
                }
            }
            Statement::If { consequent, alternate, .. } => {
                self.statement(consequent);
                if let Some(alternate) = alternate {
                    self.statement(alternate);
                }
            }
            Statement::While { body, .. } | Statement::DoWhile { body, .. } => {
                self.iteration_depth += 1;
                self.statement(body);
                self.iteration_depth -= 1;
            }
            Statement::With { body, .. } => self.statement(body),
            Statement::For { body, .. }
            | Statement::ForIn { body, .. }
            | Statement::ForOf { body, .. } => {
                let head = match statement {
                    Statement::For { init, .. } => init.as_deref(),
                    Statement::ForIn { left, .. } | Statement::ForOf { left, .. } => Some(&**left),
                    _ => None,
                };
                if let Some(head) = head {
                    if matches!(statement, Statement::ForIn { .. } | Statement::ForOf { .. }) {
                        self.check_for_head(head);
                    }
                    self.check_head_against_body(head, body);
                }
                self.iteration_depth += 1;
                self.statement(body);
                self.iteration_depth -= 1;
            }
            Statement::Switch { cases, .. } => {
                let mut body = Vec::new();
                for case in cases {
                    body.extend(case.body.iter().cloned());
                }
                self.check_block_scope(&body, ScopeKind::Block);
                self.switch_depth += 1;
                for case in cases {
                    for inner in &case.body {
                        self.statement(inner);
                    }
                }
                self.switch_depth -= 1;
            }
            Statement::Labeled { label, body, .. } => {
                if self.labels.iter().any(|(name, _)| name == label) {
                    let span = statement.span();
                    self.error(span, format!("label `{label}` has already been declared"));
                }
                let labels_an_iteration = matches!(
                    **body,
                    Statement::While { .. }
                        | Statement::DoWhile { .. }
                        | Statement::For { .. }
                        | Statement::ForIn { .. }
                        | Statement::ForOf { .. }
                        | Statement::Labeled { .. }
                );
                self.labels.push((label.clone(), labels_an_iteration));
                self.statement(body);
                self.labels.pop();
            }
            Statement::Break { label, span } => self.check_break(label.as_deref(), *span),
            Statement::Continue { label, span } => self.check_continue(label.as_deref(), *span),
            Statement::Try { block, handler, finalizer, .. } => {
                self.check_block_scope(block, ScopeKind::Block);
                for inner in block {
                    self.statement(inner);
                }
                if let Some(handler) = handler {
                    self.check_block_scope(&handler.body, ScopeKind::Block);
                    if let Some(param) = &handler.param {
                        let mut names = Vec::new();
                        collect_bound_names(param, &mut names);
                        self.check_duplicate_bound_names(&names, "a catch parameter");
                        self.check_header_against_lexical_body(
                            core::slice::from_ref(param),
                            &handler.body,
                            ScopeKind::Block,
                        );
                    }
                    for inner in &handler.body {
                        self.statement(inner);
                    }
                }
                if let Some(finalizer) = finalizer {
                    self.check_block_scope(finalizer, ScopeKind::Block);
                    for inner in finalizer {
                        self.statement(inner);
                    }
                }
            }
            Statement::Function(function) => {
                self.plain_function_body(&function.params, &function.body);
            }
            Statement::Class(class) => self.class_body(class),
            _ => {}
        }
        self.expressions_of(statement);
    }

    /// Visits the expressions a statement holds, so nested function bodies are reached.
    ///
    /// # WARNING: THE STRUCTURAL GAP THIS CLOSES, AND IT MADE FOUR CLUSTERS UNREACHABLE
    ///
    /// A pass that silently visits a subset of the tree reports clean on the part it skipped, which
    /// is indistinguishable from that part being correct.
    fn expressions_of(&mut self, statement: &Statement) {
        match statement {
            Statement::Expression { expression, .. }
            | Statement::Throw { argument: expression, .. } => self.expression(expression),
            Statement::Return { argument: Some(expression), .. } => self.expression(expression),
            Statement::Declaration { kind, declarations, .. } => {
                if *kind != DeclarationKind::Var {
                    let mut names = Vec::new();
                    for declarator in declarations {
                        collect_bound_names(&declarator.target, &mut names);
                    }
                    self.check_let_is_not_lexically_bound(&names);
                }
                for declarator in declarations {
                    self.pattern(&declarator.target);
                    if let Some(init) = &declarator.init {
                        self.expression(init);
                    }
                }
            }
            Statement::If { test, .. }
            | Statement::While { test, .. }
            | Statement::DoWhile { test, .. } => self.expression(test),
            Statement::With { object, .. } => self.expression(object),
            Statement::Switch { discriminant, cases, .. } => {
                self.expression(discriminant);
                for case in cases {
                    if let Some(test) = &case.test {
                        self.expression(test);
                    }
                }
            }
            Statement::For { init, test, update, .. } => {
                if let Some(init) = init {
                    self.for_init(init);
                }
                if let Some(test) = test {
                    self.expression(test);
                }
                if let Some(update) = update {
                    self.expression(update);
                }
            }
            Statement::ForIn { left, right, .. } | Statement::ForOf { left, right, .. } => {
                self.for_init(left);
                self.expression(right);
            }
            _ => {}
        }
    }

    fn for_init(&mut self, init: &ForInit) {
        match init {
            ForInit::Declaration { declarations, .. } => {
                for declarator in declarations {
                    self.pattern(&declarator.target);
                    if let Some(value) = &declarator.init {
                        self.expression(value);
                    }
                }
            }
            ForInit::Expression(expression) => self.expression(expression),
            ForInit::Pattern(pattern) => self.pattern(pattern),
        }
    }

    /// Descends a PATTERN, checking any expression -- and so any function body -- it reaches.
    ///
    /// A pattern is not all names. Defaults, computed keys and the base of a member target are
    /// arbitrary expressions, so a pass that owns rules about function bodies has to walk them.
    fn pattern(&mut self, pattern: &Pattern) {
        match pattern {
            Pattern::Identifier { .. } => {}
            Pattern::Member { object, property, .. } => {
                if matches!(&**object, Expression::Super { .. }) {
                    if !self.super_property_allowed {
                        self.error(
                            pattern.span(),
                            "`super.x` is only legal inside a method or a constructor",
                        );
                    }
                } else {
                    self.expression(object);
                }
                if let MemberProperty::Computed { expression, .. } = &**property {
                    self.expression(expression);
                }
            }
            Pattern::Default { target, value, .. } => {
                self.pattern(target);
                self.expression(value);
            }
            Pattern::Rest { argument, .. } => self.pattern(argument),
            Pattern::Array { elements, rest, .. } => {
                for element in elements.iter().flatten() {
                    self.pattern(element);
                }
                if let Some(rest) = rest {
                    self.pattern(rest);
                }
            }
            Pattern::Object { properties, rest, .. } => {
                for property in properties {
                    if let crate::ast::PropertyKey::Computed { expression, .. } = &property.key {
                        self.expression(expression);
                    }
                    self.pattern(&property.value);
                }
                if let Some(rest) = rest {
                    self.pattern(rest);
                }
            }
        }
    }

    /// Descends an expression, checking any function body it reaches.
    fn expression(&mut self, expression: &Expression) {
        match expression {
            Expression::Function(function) => {
                self.plain_function_body(&function.params, &function.body);
            }
            Expression::Class(class) => self.class_body(class),
            Expression::Super { span } => {
                self.error(*span, "`super` must be called or have a property accessed");
            }
            Expression::Arrow(arrow) => match &arrow.body {
                ArrowBody::Block(body) => self.function_body_with_params(&arrow.params, body),
                ArrowBody::Expression(inner) => {
                    self.function_body_expression(&arrow.params, inner)
                }
            },
            Expression::Unary { argument, .. } | Expression::Update { argument, .. } => {
                self.expression(argument);
            }
            Expression::Binary { left, right, .. } | Expression::Logical { left, right, .. } => {
                self.expression(left);
                self.expression(right);
            }
            Expression::Assignment { value, .. } => self.expression(value),
            Expression::Conditional { test, consequent, alternate, .. } => {
                self.expression(test);
                self.expression(consequent);
                self.expression(alternate);
            }
            Expression::Call { callee, arguments, .. }
            | Expression::New { callee, arguments, .. } => {
                if matches!(&**callee, Expression::Super { .. }) {
                    if !self.super_call_allowed {
                        self.error(
                            expression.span(),
                            "`super()` is only legal in the constructor of a derived class",
                        );
                    }
                } else {
                    self.expression(callee);
                }
                for argument in arguments {
                    match argument {
                        Argument::Expression(inner)
                        | Argument::Spread { argument: inner, .. } => self.expression(inner),
                    }
                }
            }
            Expression::Member { object, property, .. } => {
                if matches!(&**object, Expression::Super { .. }) {
                    if !self.super_property_allowed {
                        self.error(
                            expression.span(),
                            "`super.x` is only legal inside a method or a constructor",
                        );
                    }
                } else {
                    self.expression(object);
                }
                if let MemberProperty::Computed { expression, .. } = &**property {
                    self.expression(expression);
                }
            }
            Expression::Sequence { expressions, .. } => {
                for inner in expressions {
                    self.expression(inner);
                }
            }
            Expression::Parenthesized { expression, .. } => self.expression(expression),
            Expression::Array { elements, .. } => {
                for element in elements {
                    match element {
                        ArrayElement::Expression(inner)
                        | ArrayElement::Spread { argument: inner, .. } => self.expression(inner),
                        ArrayElement::Hole => {}
                    }
                }
            }
            Expression::Object { properties, .. } => {
                for property in properties {
                    match property {
                        ObjectProperty::Property { value, .. }
                        | ObjectProperty::CoverInitializedName { value, .. }
                        | ObjectProperty::Spread { argument: value, .. } => self.expression(value),
                        ObjectProperty::Method { function, .. } => {
                            self.method_body(&function.params, &function.body, false);
                        }
                    }
                }
            }
            Expression::Template { expressions, .. } => {
                for inner in expressions {
                    self.expression(inner);
                }
            }
            Expression::Tagged { tag, quasi, .. } => {
                self.expression(tag);
                self.expression(quasi);
            }
            Expression::NewTarget { span } => {
                if !self.new_target_allowed {
                    self.error(*span, "`new.target` is only legal inside a function");
                }
            }
            _ => {}
        }
    }

    /// An arrow whose body is a single expression: still a function boundary.
    ///
    /// **AND ITS PARAMETERS ARE STILL PART OF IT**, exactly as they are in
    /// [`Self::function_body_with_params`]. Walking only the body hides `(a = super.x) => a` and
    /// `(a = new.target) => a` from every rule in this pass while the identical text in a
    /// block-bodied arrow is checked -- two spellings of one construct disagreeing because a rule
    /// landed in one of them.
    fn function_body_expression(&mut self, params: &[Pattern], body: &Expression) {
        let labels = core::mem::take(&mut self.labels);
        let (iteration, switch) = (self.iteration_depth, self.switch_depth);
        self.iteration_depth = 0;
        self.switch_depth = 0;
        for parameter in params {
            self.pattern(parameter);
        }
        self.expression(body);
        self.labels = labels;
        self.iteration_depth = iteration;
        self.switch_depth = switch;
    }

    /// Walks a class: its heritage, and every member body with `super` allowed.
    ///
    /// A PASS THAT DOES NOT ENTER A CLASS REPORTS CLEAN ON EVERY CLASS. Every early-error rule
    /// this pass has -- redeclaration, `break` placement, duplicate parameters -- is silent about
    /// a method body it never reaches, and **a pass that visits a SUBSET of the tree reports clean
    /// on the part it skipped** rather than reporting that it skipped it.
    fn class_body(&mut self, class: &crate::ast::Class) {
        if let Some(heritage) = &class.heritage {
            self.expression(heritage);
        }
        let mut seen_constructor: Option<Span> = None;
        for member in &class.members {
            let is_constructor = class_member_is_constructor(member);
            if is_constructor && class_member_is_special(member) {
                self.error(member.span, "`constructor` may not be a generator method");
            }
            if is_constructor {
                if seen_constructor.is_some() {
                    self.error(member.span, "a class may not have two `constructor` members");
                } else {
                    seen_constructor = Some(member.span);
                }
            } else if !member.is_static
                && !member.computed
                && matches!(member.kind, crate::ast::MethodKind::Get | crate::ast::MethodKind::Set)
                && class_member_key_is(member, "constructor")
            {
                self.error(member.span, "`constructor` may not be a getter or a setter");
            }
            if member.is_static && !member.computed && class_member_key_is(member, "prototype") {
                self.error(member.span, "a static class member may not be named `prototype`");
            }
            self.method_body(
                &member.function.params,
                &member.function.body,
                is_constructor && class.heritage.is_some(),
            );
        }
    }

    /// A method's body: `super.x` is allowed, and `super()` only where the caller says.
    fn method_body(&mut self, params: &[Pattern], body: &[Statement], allow_super_call: bool) {
        let saved = (self.super_property_allowed, self.super_call_allowed, self.new_target_allowed);
        self.super_property_allowed = true;
        self.super_call_allowed = allow_super_call;
        self.new_target_allowed = true;
        self.function_body_with_params(params, body);
        (self.super_property_allowed, self.super_call_allowed, self.new_target_allowed) = saved;
    }

    /// An ordinary function's body: it has no `super` of its own, whatever encloses it.
    ///
    /// `class C { m() { function inner() { super.x; } } }` is a SyntaxError. A counter that only
    /// went up on the way into a class never came back down here, so an ordinary function nested in
    /// a method inherited an allowance it does not have.
    fn plain_function_body(&mut self, params: &[Pattern], body: &[Statement]) {
        let saved = (self.super_property_allowed, self.super_call_allowed, self.new_target_allowed);
        self.super_property_allowed = false;
        self.super_call_allowed = false;
        self.new_target_allowed = true;
        self.function_body_with_params(params, body);
        (self.super_property_allowed, self.super_call_allowed, self.new_target_allowed) = saved;
    }

    /// A function body, with the parameters that share its scope.
    ///
    /// It starts a fresh label environment and a fresh loop context, because **`break` does not
    /// cross a function boundary**: a `break` inside a function nested in a loop is an error even
    /// though a loop is lexically outside it, and carrying the counters in would accept it.
    ///
    /// # WARNING: THE PARAMETERS AND THE BODY ARE ONE SCOPE FOR LEXICAL DECLARATIONS
    ///
    /// `function f(a) { let a; }` is a SyntaxError -- BoundNames of the FormalParameters may not
    /// occur in the LexicallyDeclaredNames of the body. **`var` does not collide**, because a `var`
    /// of a parameter name is the same binding rather than a second one, so
    /// `function f(a) { var a; }` is perfectly legal and very old code does it constantly.
    ///
    /// The block-scope rule cannot see this: the parameters are not statements, so they are not in
    /// the list it walks. It is the same shape as the loop head against its body, and the same
    /// var/lexical asymmetry as everywhere else.
    fn function_body_with_params(&mut self, params: &[Pattern], body: &[Statement]) {
        let labels = core::mem::take(&mut self.labels);
        let (iteration, switch) = (self.iteration_depth, self.switch_depth);
        self.iteration_depth = 0;
        self.switch_depth = 0;
        self.check_block_scope(body, ScopeKind::TopLevel);
        self.check_header_against_lexical_body(params, body, ScopeKind::TopLevel);
        for parameter in params {
            self.pattern(parameter);
        }
        for statement in body {
            self.statement(statement);
        }
        self.labels = labels;
        self.iteration_depth = iteration;
        self.switch_depth = switch;
    }

    /// Names bound by a header -- function parameters, or a `catch` parameter -- against the LEXICAL
    /// declarations of the body that shares their scope.
    ///
    /// THE SCOPE KIND IS THE CALLER'S AND IT WAS HARD-CODED TO `Block`, WHICH REJECTED A LEGAL
    /// PROGRAM. A function declaration at the top level of a FUNCTION BODY is var-scoped, so
    /// `function f(a = 1) { function a() {} }` is legal -- `LexicallyDeclaredNames` of a
    /// `FunctionBody` is `TopLevelLexicallyDeclaredNames`, which excludes function declarations.
    /// Inside a `catch` BLOCK it is lexical and `catch (e) { function e() {} }` is the SyntaxError
    /// this check exists for. One `ScopeKind` served both and got the function-body half backwards:
    /// a rule that fires on a program the language accepts, which is the direction an early-error
    /// pass is least likely to be caught in.
    fn check_header_against_lexical_body(
        &mut self,
        header: &[Pattern],
        body: &[Statement],
        scope: ScopeKind,
    ) {
        if header.is_empty() {
            return;
        }
        let mut bound = Vec::new();
        for pattern in header {
            collect_bound_names(pattern, &mut bound);
        }
        let mut declared = Vec::new();
        for statement in body {
            collect_top_level_declarations(statement, scope, &mut declared);
        }
        for (name, kind, span) in declared {
            if kind == BindingKind::Lexical && bound.iter().any(|(bound, _)| *bound == name) {
                self.error(span, format!("`{name}` is already bound by the parameter list"));
            }
        }
    }

    fn check_break(&mut self, label: Option<&str>, span: Span) {
        match label {
            None => {
                if self.iteration_depth == 0 && self.switch_depth == 0 {
                    self.error(span, "`break` must be inside a loop or a switch");
                }
            }
            Some(label) => {
                if !self.labels.iter().any(|(name, _)| name == label) {
                    self.error(span, format!("`break` names an undeclared label `{label}`"));
                }
            }
        }
    }

    fn check_continue(&mut self, label: Option<&str>, span: Span) {
        match label {
            None => {
                if self.iteration_depth == 0 {
                    self.error(span, "`continue` must be inside a loop");
                }
            }
            Some(label) => match self.labels.iter().find(|(name, _)| name == label) {
                None => self.error(span, format!("`continue` names an undeclared label `{label}`")),
                Some((_, labels_an_iteration)) => {
                    if !labels_an_iteration {
                        self.error(span, format!("`continue` names `{label}`, which is not a loop"));
                    }
                }
            },
        }
    }

    /// A lexical loop head may not be re-declared by a `var` in the body.
    ///
    /// # IT IS THE **VAR**-DECLARED NAMES AND NOTHING ELSE, AND THIS COMPARED EVERYTHING
    ///
    /// The rule reads "any element of the BoundNames of ForDeclaration also occurs in the
    /// **VarDeclaredNames** of Statement". A `var` in the body would have to share a binding the
    /// head made lexical, wherever in the body it is written -- `for (let x of xs) { { var x; } }`
    /// collides too, because a `var` is not stopped by a block.
    ///
    /// **`for (let x of xs) { let x; }` IS LEGAL** and this refused it. The body is a block and a
    /// block is its own scope, so the inner `let` shadows the head's binding exactly as it would
    /// shadow anything else. The doc that stood here asserted the opposite confidently, and the
    /// code did what the doc said -- a reject firing on a program the language accepts, which
    /// produces no failing test anywhere because the corpus has nothing that asserts "this parses"
    /// for an ordinary construct.
    ///
    /// A `var` head does not collide with anything here: `for (var x of xs) { let x; }` is a
    /// different rule's business, and `var` creates no per-iteration lexical binding.
    fn check_head_against_body(&mut self, head: &ForInit, body: &Statement) {
        let ForInit::Declaration { kind, declarations, .. } = head else { return };
        if *kind == DeclarationKind::Var {
            return;
        }
        let mut head_names = Vec::new();
        for declarator in declarations {
            collect_bound_names(&declarator.target, &mut head_names);
        }
        if head_names.is_empty() {
            return;
        }
        let mut declared = Vec::new();
        match body {
            Statement::Block { body, .. } => {
                for statement in body {
                    collect_top_level_declarations(statement, ScopeKind::Block, &mut declared);
                }
            }
            single => collect_top_level_declarations(single, ScopeKind::Block, &mut declared),
        }
        for (name, kind, span) in declared {
            if kind != BindingKind::Var {
                continue;
            }
            if head_names.iter().any(|(head_name, _)| *head_name == name) {
                self.error(span, format!("`{name}` in the body re-declares the loop head's binding"));
            }
        }
    }

    /// `It is a Syntax Error if the BoundNames of X contains any duplicate entries`, which the
    /// standard states separately for a LexicalDeclaration, a ForDeclaration and a CatchParameter.
    ///
    /// # THREE PRODUCTIONS SAY IT, SO IT IS ONE FUNCTION AND NOT AN INLINE CHECK PER HEAD
    ///
    /// Written inline in the for-in/of head, it leaves `let [x, x] = [];` and `catch ([x, x]) {}`
    /// with no rule at all -- and a MISSING early error is the quiet kind of wrong: the program
    /// parses, runs, and the second binding simply wins. The corpus notices because those tests
    /// call `$DONOTEVALUATE()`, so the failure arrives as "a non-object was thrown at run time"
    /// rather than as anything about duplicates.
    ///
    /// `what` names the production rather than the rule, because the three are the same sentence
    /// about different grammar and a reader of the diagnostic needs to know which one they wrote.
    fn check_duplicate_bound_names(&mut self, names: &[(String, Span)], what: &str) {
        let mut seen: Vec<&str> = Vec::new();
        let mut duplicates = Vec::new();
        for (name, span) in names {
            if seen.contains(&name.as_str()) {
                duplicates.push((name.clone(), *span));
            }
            seen.push(name);
        }
        for (name, span) in duplicates {
            self.error(span, format!("duplicate binding `{name}` in {what}"));
        }
    }

    /// `It is a Syntax Error if the BoundNames of X contains "let"`.
    ///
    /// **A LEXICAL DECLARATION ONLY, AND NOT A CATCH PARAMETER.** `catch (let) {}` is legal in
    /// sloppy code -- `let` is a perfectly ordinary `BindingIdentifier` there -- so the two rules
    /// travel together in the grammar and apart here. Sharing one function for both would reject a
    /// legal program, which is the failure a "these are the same rule" reading reaches for first.
    ///
    /// It is a STATIC SEMANTIC applied AFTER the production is chosen, which is the whole point of
    /// `let` and `let = 1` written on two lines: no automatic semicolon is inserted, because ASI
    /// needs an offending token and `let let` matches LexicalDeclaration before static semantics are
    /// considered. The error comes from this rule, not from the parse.
    fn check_let_is_not_lexically_bound(&mut self, names: &[(String, Span)]) {
        for (name, span) in names {
            if name == "let" {
                self.error(*span, "`let` may not be the name bound by a lexical declaration");
            }
        }
    }

    /// The for-in / for-of head rules.
    ///
    /// The head declares exactly one binding, so a duplicate there is an error even though the same
    /// two names would be fine as two `var` statements -- and `let` may not be the name bound by a
    /// lexical declaration anywhere.
    fn check_for_head(&mut self, head: &ForInit) {
        let ForInit::Declaration { kind, declarations, span } = head else { return };
        if declarations.len() > 1 {
            self.error(*span, "a for-in/of head declares exactly one binding");
        }
        for declarator in declarations {
            if let Some(init) = &declarator.init {
                self.error(init.span(), "a for-in/of head binding may not have an initializer");
            }
        }
        let mut names: Vec<(String, Span)> = Vec::new();
        for declarator in declarations {
            collect_bound_names(&declarator.target, &mut names);
        }
        if *kind != DeclarationKind::Var {
            self.check_duplicate_bound_names(&names, "a for-in/of head");
            self.check_let_is_not_lexically_bound(&names);
        }
    }
}

/// Whether a class member's key is written as this exact name.
///
/// A STRING KEY COUNTS AND A COMPUTED ONE DOES NOT. `class C { static "prototype"() {} }` is the
/// same SyntaxError as `class C { static prototype() {} }` -- the standard's rule is on the
/// ClassElementName's *string value*, which a quoted key has and `["prototype"]` does not have until
/// it runs. A test on the identifier form alone misses half the corpus cases. A NUMERIC key can
/// never be either of the two names this is asked about, so it is simply not one.
fn class_member_key_is(member: &crate::ast::ClassMember, name: &str) -> bool {
    match &member.key {
        crate::ast::PropertyKey::Identifier { name: written, .. } => written == name,
        crate::ast::PropertyKey::String { value, .. } => value.to_lossy_string() == name,
        _ => false,
    }
}

/// Whether this member IS the class's own callable, rather than a member that happens to be so
/// named.
///
/// All four conditions matter, and the evaluator tests the same four: a `static constructor`, a
/// getter called `constructor` and a COMPUTED key that evaluates to `"constructor"` are ordinary
/// members. This mirrors `class_value_node`'s `constructor_index`, and the two must agree -- they
/// are one rule with two readers.
fn class_member_is_constructor(member: &crate::ast::ClassMember) -> bool {
    !member.is_static
        && !member.computed
        && member.kind == crate::ast::MethodKind::Normal
        && class_member_key_is(member, "constructor")
}

/// Whether a class member is a **special method**: 15.7.1's `SpecialMethod`, which a
/// `constructor` may not be.
#[must_use]
pub(crate) fn class_member_is_special(member: &crate::ast::ClassMember) -> bool {
    member.function.is_generator || member.function.is_async
}

/// The declarations a statement contributes to ITS OWN scope.
///
/// Deliberately not recursive into nested blocks or functions: `{ let a; }` inside a block does
/// not collide with a `let a;` beside it, because it is a different scope. Recursing would reject
/// correct code, which is the failure mode a redeclaration check reaches for first.
fn collect_top_level_declarations(
    statement: &Statement,
    scope: ScopeKind,
    out: &mut Vec<(String, BindingKind, Span)>,
) {
    match statement {
        Statement::Declaration { kind, declarations, .. } => {
            let binding =
                if *kind == DeclarationKind::Var { BindingKind::Var } else { BindingKind::Lexical };
            for declarator in declarations {
                let mut names = Vec::new();
                collect_bound_names(&declarator.target, &mut names);
                for (name, span) in names {
                    out.push((name, binding, span));
                }
            }
        }
        Statement::Class(class) => {
            if let Some(name) = &class.name {
                out.push((name.clone(), BindingKind::Lexical, class.span));
            }
        }
        Statement::Function(function) => {
            if let Some(name) = &function.name {
                let kind = match scope {
                    ScopeKind::TopLevel => BindingKind::Var,
                    ScopeKind::Block => BindingKind::Lexical,
                };
                out.push((name.clone(), kind, function.span));
            }
        }
        Statement::If { consequent, alternate, .. } => {
            collect_vars_only(consequent, out);
            if let Some(alternate) = alternate {
                collect_vars_only(alternate, out);
            }
        }
        Statement::While { body, .. }
        | Statement::DoWhile { body, .. }
        | Statement::For { body, .. }
        | Statement::ForIn { body, .. }
        | Statement::ForOf { body, .. }
        | Statement::With { body, .. }
        | Statement::Labeled { body, .. } => collect_vars_only(body, out),
        Statement::Block { body, .. } => {
            for inner in body {
                collect_vars_only(inner, out);
            }
        }
        _ => {}
    }
}

/// Collects only `var` declarations, which cross block boundaries to their function scope.
fn collect_vars_only(statement: &Statement, out: &mut Vec<(String, BindingKind, Span)>) {
    match statement {
        Statement::Declaration { kind: DeclarationKind::Var, declarations, .. } => {
            for declarator in declarations {
                let mut names = Vec::new();
                collect_bound_names(&declarator.target, &mut names);
                for (name, span) in names {
                    out.push((name, BindingKind::Var, span));
                }
            }
        }
        Statement::Block { body, .. } => {
            for inner in body {
                collect_vars_only(inner, out);
            }
        }
        Statement::If { consequent, alternate, .. } => {
            collect_vars_only(consequent, out);
            if let Some(alternate) = alternate {
                collect_vars_only(alternate, out);
            }
        }
        Statement::While { body, .. }
        | Statement::DoWhile { body, .. }
        | Statement::For { body, .. }
        | Statement::ForIn { body, .. }
        | Statement::ForOf { body, .. }
        | Statement::With { body, .. }
        | Statement::Labeled { body, .. } => collect_vars_only(body, out),
        _ => {}
    }
}

fn collect_bound_names(pattern: &Pattern, out: &mut Vec<(String, Span)>) {
    match pattern {
        Pattern::Identifier { name, span } => out.push((name.clone(), *span)),
        Pattern::Default { target, .. } | Pattern::Rest { argument: target, .. } => {
            collect_bound_names(target, out);
        }
        Pattern::Array { elements, rest, .. } => {
            for element in elements.iter().flatten() {
                collect_bound_names(element, out);
            }
            if let Some(rest) = rest {
                collect_bound_names(rest, out);
            }
        }
        Pattern::Object { properties, rest, .. } => {
            for property in properties {
                collect_bound_names(&property.value, out);
            }
            if let Some(rest) = rest {
                collect_bound_names(rest, out);
            }
        }
        Pattern::Member { .. } => {}
    }
}
