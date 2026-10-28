//! Local functions (C# 7.0, ECMA-334 7th ed 13.6.4): the binder's half.
//!
//! A local function is a method declared inside a block. Its name is in scope throughout the
//! block -- before the declaration as well as after it -- so every local function of a block is
//! declared, signature and all, when the block opens, and each body is bound where it is written,
//! with what it reads from the enclosing function recorded as it binds. Where the function finally
//! lives (a static method, an instance method of the enclosing type, or a method of a display
//! class) is decided after the whole method is bound, by [`crate::closure`], because it depends on
//! every call, conversion and lambda in the method.

use crate::bound::{Binder, BoundExpr, BoundExprKind, Capture, MethodReference, ScopeId};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::statement::{BoundStmt, BoundStmtKind};
use crate::symbols::{Accessibility, MethodSymbol, ParameterInfo, ParameterMode};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_syntax::ast::{
    Literal, LocalFunction, Modifier, ParameterModifier, Stmt, StmtKind, TypeRefKind,
};
use lamella_syntax::span::Span;
use lamella_syntax::version::Feature;

/// A local function's identity: unique within a compilation, increasing in the order the
/// declarations are reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LocalFunctionId(pub u32);

/// The prefix of the name a reference to a local function carries in the bound tree.
///
/// Unspellable in C# -- no identifier begins with `<` -- so no source method can be mistaken for
/// one, and none survives the lowering, which replaces every reference.
const REFERENCE_PREFIX: &str = "<local function>";

impl LocalFunctionId {
    /// The name a [`MethodReference`] to this function carries until the lowering names it.
    #[must_use]
    pub fn reference_name(self) -> Box<str> {
        format!("{REFERENCE_PREFIX}{}", self.0).into()
    }

    /// The function a reference name names, or `None` for every ordinary method name.
    #[must_use]
    pub fn from_reference_name(name: &str) -> Option<LocalFunctionId> {
        name.strip_prefix(REFERENCE_PREFIX)?
            .parse()
            .ok()
            .map(LocalFunctionId)
    }

    /// The function `method` calls or converts, or `None` when it is an ordinary method.
    #[must_use]
    pub fn of(method: &MethodReference) -> Option<LocalFunctionId> {
        LocalFunctionId::from_reference_name(&method.name)
    }
}

/// A bound local function declaration: the payload of [`BoundStmtKind::LocalFunction`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundLocalFunction {
    /// Its identity, which every reference to it carries.
    pub id: LocalFunctionId,
    /// The name the source gave it, which its metadata name spells.
    pub name: Box<str>,
    /// The name's span, where a diagnostic about the function itself is reported.
    pub name_span: Span,
    /// The parameters, named as written and typed as read in the body: a `ref int` parameter is
    /// `int` here, and [`Self::parameter_modes`] says it is passed by reference.
    pub parameters: Vec<(Box<str>, TypeSymbol)>,
    /// How each parameter is passed, parallel to [`Self::parameters`].
    pub parameter_modes: Vec<ParameterMode>,
    /// Each parameter's folded default argument, parallel to [`Self::parameters`].
    pub defaults: Vec<Option<Literal>>,
    /// Whether the last parameter is a `params` array.
    pub is_params: bool,
    /// The return type.
    pub return_type: TypeSymbol,
    /// The bound body; an expression body is the block it means.
    pub body: Box<BoundStmt>,
    /// The locals and parameters of enclosing functions the body reads, in first-seen order,
    /// with those of every lambda and local function nested in it.
    pub captures: Vec<Capture>,
    /// Whether the body, or anything nested in it, reads `this`.
    pub captures_this: bool,
    /// How many of `captures` precede the first read of `this`, when it reads `this`.
    pub this_position: Option<usize>,
    /// The local functions declared outside this one that its body calls or converts, nested
    /// functions included, in first-seen order. Not itself: a recursive call captures nothing.
    pub captured_functions: Vec<LocalFunctionId>,
    /// The scope the declaration was written in.
    pub declared_in: Option<ScopeId>,
    /// The scope its parameters were declared in -- the function's own outermost scope.
    pub body_scope: Option<ScopeId>,
    /// Whether it was declared `static`, so it captures nothing (C# 8.0).
    pub is_static: bool,
}

/// A local function as the block that declares it sees it: the signature its calls bind against.
#[derive(Debug, Clone)]
pub(crate) struct LocalFunctionSymbol {
    pub(crate) id: LocalFunctionId,
    pub(crate) name: Box<str>,
    /// The declaration statement's span, which identifies the declaration.
    pub(crate) declaration: Span,
    /// The name's span.
    pub(crate) name_span: Span,
    /// The scope it is declared in: the block it is in scope throughout.
    pub(crate) scope: ScopeId,
    /// Its signature as overload resolution sees it, named by [`LocalFunctionId::reference_name`].
    pub(crate) method: MethodSymbol,
    /// Each parameter as the body reads it.
    pub(crate) parameters: Vec<(Box<str>, TypeSymbol)>,
    pub(crate) is_static: bool,
    /// Whether anything refers to it -- itself included, measured (`CS8321`).
    pub(crate) used: bool,
}

/// Every local function declared directly in `statements`, in source order, with the span of its
/// declaration statement.
///
/// **A USING DECLARATION'S BODY IS THE SAME BLOCK.** The parser represents `using var x = e;
/// rest...` as a using statement whose body is the rest of the block, but C# declares `x` -- and
/// any local function after it -- in the block itself, so a call written before the `using` must
/// find a function declared after it (dotnet/iot's samples do exactly that, in the resource's own
/// initializer).
fn block_local_functions<'a>(statements: &'a [Stmt], found: &mut Vec<(&'a LocalFunction, Span)>) {
    for statement in statements {
        match &statement.kind {
            StmtKind::LocalFunction(function) => found.push((function, statement.span)),
            StmtKind::Using {
                body,
                declaration: true,
                ..
            } => {
                if let StmtKind::Block(rest) = &body.kind {
                    block_local_functions(rest, found);
                }
            }
            _ => {}
        }
    }
}

impl Binder {
    /// Declares every local function written directly in the block whose statements these are, in
    /// the scope the block has just opened (13.6.4: in scope throughout the block).
    ///
    pub(crate) fn declare_local_functions(&mut self, statements: &[Stmt]) {
        let mut found = Vec::new();
        block_local_functions(statements, &mut found);
        let Some(scope) = self.innermost_scope_id() else {
            return;
        };
        for (function, declaration) in found {
            if self
                .local_functions
                .iter()
                .any(|declared| declared.declaration == declaration)
            {
                continue;
            }
            if self
                .local_functions
                .iter()
                .any(|declared| declared.scope == scope && declared.name == function.name)
            {
                self.duplicate_local_functions.push(declaration);
                continue;
            }
            let symbol = self.local_function_symbol(function, declaration, scope);
            self.local_functions.push(symbol);
        }
    }

    /// The signature of `function`, resolved in the scope that declares it.
    fn local_function_symbol(
        &mut self,
        function: &LocalFunction,
        declaration: Span,
        scope: ScopeId,
    ) -> LocalFunctionSymbol {
        let id = LocalFunctionId(self.next_local_function_id);
        self.next_local_function_id += 1;
        let entered = self.enter_type_parameters(&function.type_parameters, &function.constraints);
        let return_type = self.resolve_type_ref(&function.return_type);
        let enclosing = self.current_type_or_error();
        let mut signature = Vec::new();
        let mut parameter_info = Vec::new();
        let mut parameters = Vec::new();
        for parameter in &function.parameters {
            let ty = self.resolve_type_ref(&parameter.ty);
            let mode = match parameter.modifier {
                Some(ParameterModifier::Ref) => ParameterMode::Ref,
                Some(ParameterModifier::Out) => ParameterMode::Out,
                _ => ParameterMode::Value,
            };
            let default = parameter.default_value.as_ref().and_then(|value| {
                let (bound, _) = self.bind_parameter_default(&enclosing, &ty, value);
                self.parameter_default_value(&bound, &ty)
            });
            signature.push(match mode {
                ParameterMode::Value => ty.clone(),
                ParameterMode::Ref | ParameterMode::Out => TypeSymbol::ByRef(Box::new(ty.clone())),
            });
            parameter_info.push(ParameterInfo {
                name: parameter.name.clone(),
                mode,
                default,
            });
            parameters.push((parameter.name.clone(), ty));
        }
        self.exit_type_parameters(entered);
        let is_params = function
            .parameters
            .last()
            .is_some_and(|parameter| parameter.modifier == Some(ParameterModifier::Params));
        LocalFunctionSymbol {
            id,
            name: function.name.clone(),
            declaration,
            name_span: function.name_span,
            scope,
            method: MethodSymbol {
                name: id.reference_name(),
                return_type,
                return_required_modifiers: Vec::new(),
                parameters: signature,
                parameter_info,
                return_tuple_names: crate::bind::tuple_element_names(&function.return_type),
                is_static: true,
                is_params,
                is_vararg: false,
                is_virtual: false,
                is_abstract: false,
                is_override: false,
                is_sealed: false,
                accessibility: Accessibility::Private,
                conditional: Vec::new(),
                sets_required_members: false,
                type_parameters: function
                    .type_parameters
                    .iter()
                    .map(|parameter| parameter.name.clone())
                    .collect(),
                explicit_interface: None,
                type_parameter_constraints: Vec::new(),
            },
            parameters,
            is_static: function.modifiers.contains(&Modifier::Static),
            used: false,
        }
    }

    /// The local function `name` resolves to here, with the depth of the scope that declares it,
    /// or `None`. Innermost first, as a local's name resolves.
    pub(crate) fn local_function_in_scope(&self, name: &str) -> Option<(usize, LocalFunctionId)> {
        let open = self.open_scope_id_list();
        open.iter().enumerate().rev().find_map(|(depth, scope)| {
            self.local_functions
                .iter()
                .find(|function| function.scope == *scope && &*function.name == name)
                .map(|function| (depth, function.id))
        })
    }

    /// The method group a reference to local function `id` binds to, recording the reference: a
    /// use for `CS8321`, and a capture of the function by every enclosing function it is declared
    /// outside of.
    pub(crate) fn local_function_group(&mut self, id: LocalFunctionId, depth: usize) -> BoundExpr {
        if let Some(function) = self.local_functions.iter_mut().find(|f| f.id == id) {
            function.used = true;
        }
        self.note_function_capture(id, depth);
        let receiver_type = self.current_type_or_error();
        BoundExpr {
            kind: BoundExprKind::MethodGroup {
                receiver: Box::new(BoundExpr {
                    kind: BoundExprKind::TypeReference(receiver_type.clone()),
                    ty: receiver_type,
                }),
                name: id.reference_name(),
            },
            ty: TypeSymbol::Error,
        }
    }

    /// The one candidate a local function's method group holds, when `name` is a reference name.
    pub(crate) fn local_function_candidate(&self, name: &str) -> Option<MethodSymbol> {
        let id = LocalFunctionId::from_reference_name(name)?;
        self.local_functions
            .iter()
            .find(|function| function.id == id)
            .map(|function| function.method.clone())
    }

    /// Binds a local function's declaration statement: its body, bound here in the scope it is
    /// written in, with what it captures recorded.
    pub(crate) fn bind_local_function(&mut self, function: &LocalFunction, span: Span) -> BoundStmtKind {
        let Some(symbol) = self
            .local_functions
            .iter()
            .find(|declared| declared.declaration == span)
            .cloned()
        else {
            if self.duplicate_local_functions.contains(&span) {
                self.report(Diagnostic::new(
                    DiagnosticKind::DuplicateLocal {
                        name: function.name.clone(),
                    },
                    function.name_span,
                ));
            }
            return BoundStmtKind::Empty;
        };
        if !function.type_parameters.is_empty() {
            self.gate_feature(Feature::GenericLocalFunction, function.name_span);
        } else if self.has_type_parameters_in_scope() {
            self.gate_feature(Feature::LocalFunctionInGenericScope, function.name_span);
        }
        if function.modifiers.contains(&Modifier::Async) {
            self.gate_feature(Feature::AsyncLocalFunction, function.name_span);
        }
        if function.modifiers.contains(&Modifier::Extern) {
            self.gate_feature(Feature::ExternLocalFunction, function.name_span);
        }
        if matches!(function.return_type.kind, TypeRefKind::ByRef { is_readonly: true, .. }) {
            self.gate_feature(Feature::ReadOnlyRefLocalFunction, function.name_span);
        }
        if function.modifiers.contains(&Modifier::Unsafe) && self.unsafe_option_missing() {
            self.report(Diagnostic::new(
                DiagnosticKind::UnsafeCodeRequiresOption,
                function.name_span,
            ));
        }
        if self.local_in_current_scope(&function.name) {
            self.report(Diagnostic::new(
                DiagnosticKind::DuplicateLocal {
                    name: function.name.clone(),
                },
                function.name_span,
            ));
        }
        let Some(body) = &function.body else {
            if !function.modifiers.contains(&Modifier::Extern) {
                self.report(Diagnostic::new(
                    DiagnosticKind::LocalFunctionMissingBody {
                        signature: self.local_function_signature(&symbol),
                    },
                    function.name_span,
                ));
            }
            return BoundStmtKind::LocalFunction(Box::new(self.bodiless_local_function(&symbol, span)));
        };
        let declared_in = self.innermost_scope_id();
        let entered = self.enter_type_parameters(&function.type_parameters, &function.constraints);
        self.enter_scope();
        let body_scope = self.innermost_scope_id();
        if let Some(scope) = body_scope {
            self.mark_function_scope(scope);
        }
        self.push_function_frame(Some(symbol.id), symbol.is_static);
        for (name, ty) in &symbol.parameters {
            self.declare_local(name, ty.clone());
        }
        let outer = self.enter_local_function_body(function, &symbol);
        let bound = self.bind_statement(body);
        self.exit_local_function_body(outer);
        let frame = self.pop_function_frame();
        self.exit_scope();
        self.exit_type_parameters(entered);
        let returns_value = !symbol.method.return_type.is_void()
            && !symbol.method.return_type.is_error()
            && !function.modifiers.contains(&Modifier::Async);
        if returns_value && !crate::flow::method_body_always_exits(&bound) {
            self.report(Diagnostic::new(
                DiagnosticKind::NotAllPathsReturn {
                    method: self.local_function_signature(&symbol),
                },
                function.name_span,
            ));
        }
        self.report_unhoistable_captures_at(&frame.captures, &frame.capture_spans);
        let labels = crate::flow::check_labels(&bound);
        self.extend_diagnostics(labels);
        let function = BoundLocalFunction {
            id: symbol.id,
            name: symbol.name.clone(),
            name_span: symbol.name_span,
            parameters: symbol.parameters.clone(),
            parameter_modes: symbol
                .method
                .parameter_info
                .iter()
                .map(|info| info.mode)
                .collect(),
            defaults: symbol
                .method
                .parameter_info
                .iter()
                .map(|info| info.default.clone())
                .collect(),
            is_params: symbol.method.is_params,
            return_type: symbol.method.return_type.clone(),
            body: Box::new(bound),
            captures: frame.captures,
            captures_this: frame.captures_this,
            this_position: frame.this_position,
            captured_functions: frame.captured_functions,
            declared_in,
            body_scope,
            is_static: symbol.is_static,
        };
        self.method_local_functions.push(function.clone());
        BoundStmtKind::LocalFunction(Box::new(function))
    }

    /// The declaration of a function written with no body, as the analyses over the method see it.
    fn bodiless_local_function(&self, symbol: &LocalFunctionSymbol, span: Span) -> BoundLocalFunction {
        BoundLocalFunction {
            id: symbol.id,
            name: symbol.name.clone(),
            name_span: symbol.name_span,
            parameters: symbol.parameters.clone(),
            parameter_modes: symbol.method.parameter_info.iter().map(|info| info.mode).collect(),
            defaults: symbol.method.parameter_info.iter().map(|info| info.default.clone()).collect(),
            is_params: symbol.method.is_params,
            return_type: symbol.method.return_type.clone(),
            body: Box::new(BoundStmt {
                kind: BoundStmtKind::Block(Vec::new()),
                span,
            }),
            captures: Vec::new(),
            captures_this: false,
            this_position: None,
            captured_functions: Vec::new(),
            declared_in: self.innermost_scope_id(),
            body_scope: None,
            is_static: symbol.is_static,
        }
    }

    /// The function as csc renders it in a diagnostic: its name and parameter types, `F(bool)`.
    fn local_function_signature(&self, symbol: &LocalFunctionSymbol) -> Box<str> {
        let mut text = String::from(&*symbol.name);
        text.push('(');
        for (index, ty) in symbol.method.parameters.iter().enumerate() {
            if index > 0 {
                text.push_str(", ");
            }
            match ty {
                TypeSymbol::ByRef(inner) => {
                    let mode = match symbol.method.parameter_mode(index) {
                        Some(ParameterMode::Out) => "out ",
                        _ => "ref ",
                    };
                    text.push_str(mode);
                    text.push_str(&inner.to_string());
                }
                other => text.push_str(&other.to_string()),
            }
        }
        text.push(')');
        text.into()
    }

    /// Refuses, by name, each local function declared since `first` that this build cannot lower:
    /// one converted to a delegate, or reached from a lambda, that captures a local declared
    /// anywhere but the method body's own scope -- itself, or through the local functions it
    /// refers to. Those captures need a display CLASS in a nested scope, chained to the enclosing
    /// one, which is lambdas' unbuilt shape too ([`Feature::LambdaCapturingNestedScope`]).
    ///
    /// Run once the method is bound, because a conversion may follow the declaration.
    pub(crate) fn gate_local_function_closures(&mut self, first: u32) {
        let converted = self.converted_local_functions.borrow().clone();
        let mut refused = Vec::new();
        for symbol in self.local_functions.iter().filter(|f| f.id.0 >= first) {
            if !converted.contains(&symbol.id)
                && !self.lambda_captured_local_functions.contains(&symbol.id)
            {
                continue;
            }
            let mut seen = alloc::vec![symbol.id];
            let mut pending = alloc::vec![symbol.id];
            let mut nested = false;
            while let Some(id) = pending.pop() {
                let Some(function) = self.method_local_functions.iter().find(|f| f.id == id) else {
                    continue;
                };
                if function
                    .captures
                    .iter()
                    .any(|capture| !self.is_method_body_scope(capture.scope))
                {
                    nested = true;
                    break;
                }
                for callee in &function.captured_functions {
                    if !seen.contains(callee) {
                        seen.push(*callee);
                        pending.push(*callee);
                    }
                }
            }
            if nested {
                refused.push(symbol.name_span);
            }
        }
        for span in refused {
            self.gate_feature(Feature::LocalFunctionDelegateNestedCapture, span);
        }
    }

    /// `CS8321` for each local function declared since `first` that nothing referred to, in
    /// declaration order.
    pub(crate) fn unused_local_functions(&self, first: u32) -> Vec<LocalFunctionId> {
        self.local_functions
            .iter()
            .filter(|function| function.id.0 >= first && !function.used)
            .map(|function| function.id)
            .collect()
    }
}
