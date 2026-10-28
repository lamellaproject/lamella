//! Closure lowering: a lambda expression or a local function becomes a method of the type that
//! holds what it captures, and every site that names one becomes the delegate creation or call csc
//! writes there (ECMA-334 7th ed 12.19, 13.6.4).

use crate::expr::EmitError;
use alloc::boxed::Box;
use alloc::collections::{BTreeMap, BTreeSet};
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_binder::local_function::{BoundLocalFunction, LocalFunctionId};
use lamella_binder::statement::BoundLambdaBody;
use lamella_binder::symbols::ParameterMode;
use lamella_binder::{
    Accessibility, BoundExpr, BoundExprKind, BoundStmt, BoundStmtKind, Capture, FieldReference,
    MethodReference, ScopeId, SpecialType, TypeSymbol,
};
use lamella_syntax::ast::Literal;
use lamella_syntax::span::Span;

/// The closure type's simple name -- one per enclosing type, holding every non-capturing lambda
/// that type's methods write. csc's name, and the reason it is a constant rather than a format:
/// there is only ever one, however many methods contribute to it.
pub(crate) const CLOSURE_TYPE: &str = "<>c";

/// The singleton instance field on [`CLOSURE_TYPE`], created by its `.cctor`.
pub(crate) const SINGLETON_FIELD: &str = "<>9";

/// A display class's simple name, before its two indices: the enclosing method's ordinal and the
/// environment's number within that method. csc's, measured.
pub(crate) const DISPLAY_CLASS_PREFIX: &str = "<>c__DisplayClass";

/// The field an environment holds the enclosing instance in. Hoisted by NAME rather than by the
/// source's spelling, because `this` has none.
pub(crate) const THIS_FIELD: &str = "<>4__this";

/// The local holding the method body's display CLASS.
///
pub(crate) const LOCALS_LOCAL: &str = "<>8__locals";

/// Where a lowered lambda's or local function's body lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LambdaHome {
    /// A lambda that captures nothing: a method on the per-enclosing-type `<>c` singleton, with a
    /// cached delegate.
    ClosureType,
    /// Captures `this` and no environment that has to be a class: an instance method of the
    /// ENCLOSING type -- `private hidebysig`, `[CompilerGenerated]` -- reading the instance as its
    /// own `this`. A local function here may also take struct environments by reference.
    EnclosingType,
    /// Captures a class environment: an instance method of the display class, `assembly
    /// hidebysig` and no attribute of its own, reading the class as its `this`.
    DisplayClass,
    /// A local function that captures neither `this` nor a class environment: a static method of
    /// the enclosing type, `assembly hidebysig static`, `[CompilerGenerated]`, taking each struct
    /// environment it captures by reference after its own parameters.
    StaticMethod,
}

/// One field of an environment, in csc's order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DisplayField {
    /// `<>4__this`, the enclosing instance.
    This,
    /// A captured local or parameter, named as the source wrote it.
    Named(Box<str>, TypeSymbol),
}

/// One environment a method needs: the display class or struct, and the fields it holds.
#[derive(Debug, Clone)]
pub(crate) struct DisplayClass {
    /// The synthesized type, qualified by the enclosing type -- see [`closure_symbol`] for why a
    /// bare name is not enough.
    pub(crate) symbol: TypeSymbol,
    /// The fields in FIRST-CAPTURE order, `<>4__this` among them where `this` was first read --
    /// csc's order, measured: `() => f + a` gives `<>4__this, a`.
    pub(crate) fields: Vec<DisplayField>,
    /// A struct, passed by reference, rather than a class.
    pub(crate) is_struct: bool,
    /// Its `S`: the environments' running number within the method, from 0.
    pub(crate) suffix: usize,
}

impl DisplayClass {
    /// Whether `<>4__this` is among its fields.
    #[must_use]
    pub(crate) fn hoists_this(&self) -> bool {
        self.fields.iter().any(|field| *field == DisplayField::This)
    }

    /// The local -- or, in a function that receives it by reference, the parameter -- that holds
    /// it. One name for both, so an access is spelled the same wherever it is written.
    #[must_use]
    pub(crate) fn local(&self) -> Box<str> {
        if self.is_struct {
            format!("{LOCALS_LOCAL}{}", self.suffix).into()
        } else {
            Box::from(LOCALS_LOCAL)
        }
    }

    /// The type of the field `name` holds, when it holds one.
    fn field_type(&self, name: &str) -> Option<&TypeSymbol> {
        self.fields.iter().find_map(|field| match field {
            DisplayField::Named(field, ty) if &**field == name => Some(ty),
            _ => None,
        })
    }
}

/// What a local function's declaration adds to the method it becomes.
#[derive(Debug, Clone)]
pub(crate) struct LocalFunctionShape {
    /// How each declared parameter is passed.
    pub(crate) parameter_modes: Vec<ParameterMode>,
    /// Each declared parameter's default, for its `Param` row's `Constant`.
    pub(crate) defaults: Vec<Option<Literal>>,
    /// Whether the last declared parameter is a `params` array.
    pub(crate) is_params: bool,
    /// The struct environments it takes by reference after its declared parameters, in order:
    /// the parameter's name and the struct's type. csc writes no `Param` row for these.
    pub(crate) environments: Vec<(Box<str>, TypeSymbol)>,
}

/// One lowered lambda or local function: the method to synthesize and, for a cached lambda, the
/// static field its delegate is kept in.
#[derive(Debug, Clone)]
pub(crate) struct LoweredLambda {
    /// Which type the synthesized method belongs to.
    pub(crate) home: LambdaHome,
    /// The synthesized method's name: `<M>b__N_K`/`<M>b__K` for a lambda,
    /// `<M>g__F|N_K`/`<M>g__F|K` for a local function -- the enclosing method's name, its ordinal
    /// within the declaring type, and the function's number within the method. The forms without
    /// `N` are a display class's, which already spells the method ordinal in its own name.
    pub(crate) method_name: Box<str>,
    /// The cache field's name, `<>9__N_K`, for a cached lambda.
    ///
    pub(crate) cache_name: Option<Box<str>>,
    /// The delegate type a lambda's site constructs; the error type for a local function.
    pub(crate) delegate_type: TypeSymbol,
    /// The declared parameters, named as the source wrote them and typed as the body reads them.
    pub(crate) parameters: Vec<(Box<str>, TypeSymbol)>,
    /// Its return type.
    pub(crate) return_type: TypeSymbol,
    /// Its body, already wrapped into the block an expression body means and rewritten.
    pub(crate) body: BoundStmt,
    /// The ENCLOSING method's ordinal -- the `N` in the names above. Carried so the closure type's
    /// members can be emitted in csc's order, which is the enclosing members' order and not the
    /// order emission happened to discover them in.
    pub(crate) ordinal: usize,
    /// For a local function, what its declaration adds; `None` for a lambda.
    pub(crate) local: Option<LocalFunctionShape>,
}

impl LoweredLambda {
    /// The parameter types the method is keyed, declared and called by: each declared parameter,
    /// `T&` when it is `ref` or `out`, then each struct environment as `S&`.
    #[must_use]
    pub(crate) fn signature_parameters(&self) -> Vec<TypeSymbol> {
        let mut types: Vec<TypeSymbol> = Vec::new();
        for (index, (_, ty)) in self.parameters.iter().enumerate() {
            let by_ref = self.local.as_ref().is_some_and(|local| {
                local
                    .parameter_modes
                    .get(index)
                    .is_some_and(|mode| *mode != ParameterMode::Value)
            });
            types.push(if by_ref {
                TypeSymbol::ByRef(Box::new(ty.clone()))
            } else {
                ty.clone()
            });
        }
        if let Some(local) = &self.local {
            for (_, ty) in &local.environments {
                types.push(TypeSymbol::ByRef(Box::new(ty.clone())));
            }
        }
        types
    }

    /// Whether the synthesized method is `static`.
    #[must_use]
    pub(crate) fn is_static(&self) -> bool {
        self.home == LambdaHome::StaticMethod
    }
}

/// What one method's lowering produced: the rewritten body, and the members it contributes.
#[derive(Debug)]
pub(crate) struct LambdaLowering {
    /// The enclosing method's body with every lambda and local function site replaced.
    pub(crate) body: BoundStmt,
    /// The lambdas and local functions found, in the order their sites were rewritten -- source
    /// order, which is the order csc emits their methods in.
    pub(crate) sites: Vec<LoweredLambda>,
    /// The method body's display CLASS, when it needs one.
    pub(crate) display: Option<DisplayClass>,
    /// The display STRUCTS, in their number's order.
    pub(crate) structs: Vec<DisplayClass>,
}

/// The closure type as a symbol: the ENCLOSING type's path with `<>c` appended.
///
#[must_use]
pub(crate) fn closure_symbol(enclosing: &TypeSymbol) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = match enclosing {
        TypeSymbol::Named(path) => path.to_vec(),
        _ => Vec::new(),
    };
    parts.push(Box::from(CLOSURE_TYPE));
    TypeSymbol::Named(parts.into_boxed_slice())
}

/// A display class or struct as a symbol: the ENCLOSING type's path with
/// `<>c__DisplayClass{N}_{S}`.
///
/// Qualified for the reason [`closure_symbol`] is, and unlike `<>c` there is more than one of these
/// per type, so the two indices are load-bearing: `N` is the enclosing method's member ordinal and
/// `S` numbers the environments of that method.
#[must_use]
pub(crate) fn display_class_symbol(enclosing: &TypeSymbol, ordinal: usize, scope: usize) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = match enclosing {
        TypeSymbol::Named(path) => path.to_vec(),
        _ => Vec::new(),
    };
    parts.push(Box::from(display_class_name(ordinal, scope)));
    TypeSymbol::Named(parts.into_boxed_slice())
}

/// The display class's simple name, which is what the `TypeDef` row carries.
#[must_use]
pub(crate) fn display_class_name(ordinal: usize, scope: usize) -> String {
    format!("{DISPLAY_CLASS_PREFIX}{ordinal}_{scope}")
}

/// What the lowering needs to know about the scopes a method opened.
///
/// Narrow on purpose. The lowering needs these questions answered and a [`Binder`] answers them,
/// but a test should be able to describe a scope shape in a few lines rather than bind a program to
/// produce one.
///
/// [`Binder`]: lamella_binder::Binder
pub(crate) trait ScopeTree {
    /// The scope that encloses `scope`, or `None` when it is outermost.
    fn parent(&self, scope: ScopeId) -> Option<ScopeId>;

    /// `scope`, or the one id that stands for it when several are one scope to the language: a
    /// method's or a function's parameters and its body block, and a using declaration's scopes
    /// with the block it is written in.
    fn canonical(&self, scope: ScopeId) -> ScopeId;

    /// Whether `scope` holds a lambda's or a local function's parameters.
    fn is_function(&self, scope: ScopeId) -> bool;

    /// Whether `scope` declares any name.
    fn declares_names(&self, scope: ScopeId) -> bool;

    /// The span of the statement that opened `scope`, when one was recorded.
    fn span(&self, scope: ScopeId) -> Option<Span>;

    /// The method body's own scope: its parameters'.
    fn root(&self) -> Option<ScopeId>;
}

impl ScopeTree for lamella_binder::Binder {
    fn parent(&self, scope: ScopeId) -> Option<ScopeId> {
        self.scope_parent(scope)
    }

    fn canonical(&self, scope: ScopeId) -> ScopeId {
        self.canonical_scope(scope)
    }

    fn is_function(&self, scope: ScopeId) -> bool {
        self.scope_is_function(scope)
    }

    fn declares_names(&self, scope: ScopeId) -> bool {
        self.scope_declares_names(scope)
    }

    fn span(&self, scope: ScopeId) -> Option<Span> {
        self.scope_span(scope)
    }

    fn root(&self) -> Option<ScopeId> {
        self.method_root_scope()
    }
}

/// A variable some function captures: `this`, or a local or parameter by its name and the scope
/// that declares it -- which is what tells two locals of one name apart.
#[derive(Debug, Clone)]
enum Variable {
    This,
    Named {
        name: Box<str>,
        scope: ScopeId,
        ty: TypeSymbol,
    },
}

impl Variable {
    /// The same variable, whatever type either copy carries.
    fn same(&self, other: &Variable) -> bool {
        match (self, other) {
            (Variable::This, Variable::This) => true,
            (
                Variable::Named { name, scope, .. },
                Variable::Named {
                    name: other_name,
                    scope: other_scope,
                    ..
                },
            ) => name == other_name && scope == other_scope,
            _ => false,
        }
    }

    fn field(&self) -> DisplayField {
        match self {
            Variable::This => DisplayField::This,
            Variable::Named { name, ty, .. } => DisplayField::Named(name.clone(), ty.clone()),
        }
    }
}

/// Which function: a lambda by its place in the walk, a local function by its identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FunctionKey {
    Lambda(usize),
    Local(LocalFunctionId),
}

/// One lambda or local function, as the survey found it and the plan placed it.
#[derive(Debug, Clone)]
struct Function {
    key: FunctionKey,
    /// A local function's name, which its method's name spells.
    name: Box<str>,
    declared_in: Option<ScopeId>,
    body_scope: Option<ScopeId>,
    /// What it captures, `this` included, in first-capture order.
    variables: Vec<Variable>,
    captured_functions: Vec<LocalFunctionId>,
    is_lambda: bool,
    /// A local function made a delegate of somewhere in the method.
    converted: bool,
    /// A local function's declared parameters, how each is passed, and its return type -- what a
    /// call written before the declaration needs to name the method it becomes.
    parameters: Vec<(Box<str>, TypeSymbol)>,
    parameter_modes: Vec<ParameterMode>,
    return_type: TypeSymbol,
    scope: ScopeId,
    /// The environments it captures, in the order the plan gave them to it: outer scopes first.
    envs: Vec<usize>,
    /// The class environment it lives on.
    containing: Option<usize>,
    captures_this: bool,
    ordinal: usize,
    home: LambdaHome,
}

impl Function {
    /// Whether it can take an environment by reference: a local function nothing makes a
    /// delegate of (an async or iterator one is refused before here).
    fn takes_references(&self) -> bool {
        !self.is_lambda && !self.converted
    }
}

/// One environment, as the plan built it.
#[derive(Debug, Clone)]
struct Env {
    scope: ScopeId,
    variables: Vec<Variable>,
    is_struct: bool,
    /// Dropped by step 4: a method body's environment holding only `this`.
    removed: bool,
    suffix: usize,
    class: DisplayClass,
}

/// `captures` with `this` placed where it was first read.
fn variables_of(
    captures: &[Capture],
    captures_this: bool,
    this_position: Option<usize>,
    tree: &dyn ScopeTree,
) -> Vec<Variable> {
    let mut variables: Vec<Variable> = captures
        .iter()
        .map(|capture| Variable::Named {
            name: capture.name.clone(),
            scope: tree.canonical(capture.scope),
            ty: capture.ty.clone(),
        })
        .collect();
    if captures_this {
        let at = this_position.unwrap_or(variables.len()).min(variables.len());
        variables.insert(at, Variable::This);
    }
    variables
}

/// The plan for one method's closures: Roslyn's analysis over the functions the survey found.
struct Plan {
    /// The relevant scopes in pre-order: the method body's first, each scope before its nested ones.
    preorder: Vec<ScopeId>,
    /// Each relevant scope's enclosing relevant scope.
    parents: BTreeMap<ScopeId, ScopeId>,
}

impl Plan {
    /// The scope `scope` is part of for closures: its canonical form, or the nearest enclosing
    /// one that is a function's, declares a name, or is the method body.
    fn real(tree: &dyn ScopeTree, root: ScopeId, scope: ScopeId) -> ScopeId {
        let mut current = tree.canonical(scope);
        loop {
            if current == root || tree.is_function(current) || tree.declares_names(current) {
                return current;
            }
            match tree.parent(current) {
                Some(parent) => current = tree.canonical(parent),
                None => return current,
            }
        }
    }

    /// Whether `scope` is `ancestor` or nested in it.
    fn within(&self, mut scope: ScopeId, ancestor: ScopeId) -> bool {
        loop {
            if scope == ancestor {
                return true;
            }
            match self.parents.get(&scope) {
                Some(parent) => scope = *parent,
                None => return false,
            }
        }
    }
}

/// Places every function and builds every environment: the six steps the module doc lists.
///
/// Returns the environments; each function's `scope`, `envs`, `containing`, `captures_this`,
/// `ordinal` and `home` are filled in.
fn plan(
    functions: &mut [Function],
    tree: &dyn ScopeTree,
    enclosing: &TypeSymbol,
    method_ordinal: usize,
    has_this: bool,
) -> Result<Vec<Env>, EmitError> {
    let Some(root) = tree.root().map(|scope| tree.canonical(scope)) else {
        return Err(EmitError::Unsupported(
            "a closure reached the lowering with no method scope recorded",
        ));
    };
    let mut relevant: BTreeSet<ScopeId> = BTreeSet::new();
    relevant.insert(root);
    for function in functions.iter_mut() {
        function.scope = match function.declared_in {
            Some(scope) => Plan::real(tree, root, scope),
            None => root,
        };
        relevant.insert(function.scope);
        if let Some(body) = function.body_scope {
            relevant.insert(Plan::real(tree, root, body));
        }
        for variable in &function.variables {
            if let Variable::Named { scope, .. } = variable {
                relevant.insert(Plan::real(tree, root, *scope));
            }
        }
    }
    let mut parents: BTreeMap<ScopeId, ScopeId> = BTreeMap::new();
    let mut pending: Vec<ScopeId> = relevant.iter().copied().collect();
    while let Some(scope) = pending.pop() {
        if scope == root || parents.contains_key(&scope) {
            continue;
        }
        let parent = match tree.parent(scope) {
            Some(parent) => Plan::real(tree, root, parent),
            None => root,
        };
        parents.insert(scope, parent);
        if relevant.insert(parent) {
            pending.push(parent);
        }
    }
    let mut children: BTreeMap<ScopeId, Vec<ScopeId>> = BTreeMap::new();
    for (child, parent) in &parents {
        children.entry(*parent).or_default().push(*child);
    }
    for list in children.values_mut() {
        list.sort_unstable();
    }
    let mut preorder: Vec<ScopeId> = Vec::new();
    let mut stack = alloc::vec![root];
    while let Some(scope) = stack.pop() {
        preorder.push(scope);
        if let Some(list) = children.get(&scope) {
            for child in list.iter().rev() {
                stack.push(*child);
            }
        }
    }
    let shape = Plan { preorder, parents };
    let mut visit: Vec<usize> = Vec::new();
    for scope in &shape.preorder {
        for (index, function) in functions.iter().enumerate() {
            if function.scope == *scope {
                visit.push(index);
            }
        }
    }
    let mut declared: BTreeMap<ScopeId, Vec<Variable>> = BTreeMap::new();
    for function in functions.iter() {
        for variable in &function.variables {
            let scope = match variable {
                Variable::This => root,
                Variable::Named { scope, .. } => Plan::real(tree, root, *scope),
            };
            let list = declared.entry(scope).or_default();
            if !list.iter().any(|seen| seen.same(variable)) {
                list.push(variable.clone());
            }
        }
    }
    let mut envs: Vec<Env> = Vec::new();
    for scope in &shape.preorder {
        let Some(variables) = declared.get(scope).cloned() else {
            continue;
        };
        let nested: Vec<usize> = visit
            .iter()
            .copied()
            .filter(|index| shape.within(functions[*index].scope, *scope))
            .collect();
        let mut closures: Vec<usize> = Vec::new();
        loop {
            let mut added = false;
            for index in &nested {
                if closures.contains(index) {
                    continue;
                }
                let function = &functions[*index];
                let captures_variable = function
                    .variables
                    .iter()
                    .any(|variable| variables.iter().any(|declared| declared.same(variable)));
                let captures_closure = function.captured_functions.iter().any(|callee| {
                    closures
                        .iter()
                        .any(|closure| functions[*closure].key == FunctionKey::Local(*callee))
                });
                if captures_variable || captures_closure {
                    closures.push(*index);
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
        let is_struct = closures.iter().all(|index| functions[*index].takes_references());
        let holds_this = variables.iter().any(|variable| matches!(variable, Variable::This));
        let env = envs.len();
        for index in &closures {
            functions[*index].envs.push(env);
            if holds_this {
                functions[*index].captures_this = true;
            }
        }
        envs.push(Env {
            scope: *scope,
            variables,
            is_struct,
            removed: false,
            suffix: 0,
            class: DisplayClass {
                symbol: TypeSymbol::Error,
                fields: Vec::new(),
                is_struct,
                suffix: 0,
            },
        });
    }
    for index in &visit {
        let function = &functions[*index];
        if function.envs.is_empty() {
            continue;
        }
        let mut remaining = function.envs.clone();
        let mut containing = None;
        let mut current = Some(function.scope);
        while let Some(scope) = current {
            if let Some(env) = remaining
                .iter()
                .position(|env| envs[*env].scope == scope)
                .map(|at| remaining.remove(at))
            {
                if !envs[env].is_struct {
                    containing = Some(env);
                    break;
                }
            }
            current = shape.parents.get(&scope).copied();
        }
        if remaining.iter().any(|env| !envs[*env].is_struct) {
            return Err(EmitError::Unsupported(
                "a closure reached the lowering needing a chain of display classes",
            ));
        }
        functions[*index].containing = containing;
    }
    if has_this {
        if let Some(env) = envs.iter().position(|env| env.scope == root) {
            let only_this = matches!(envs[env].variables.as_slice(), [Variable::This]);
            if only_this {
                let removable = !envs[env].is_struct
                    || !functions
                        .iter()
                        .any(|function| function.envs.contains(&env) && function.containing.is_some());
                if removable {
                    envs[env].removed = true;
                    for function in functions.iter_mut() {
                        function.envs.retain(|captured| *captured != env);
                        if function.containing == Some(env) {
                            function.containing = None;
                        }
                    }
                }
            }
        }
    }
    if envs
        .iter()
        .any(|env| !env.removed && !env.is_struct && env.scope != root)
    {
        return Err(EmitError::Unsupported(
            "a closure reached the lowering needing a display class in a nested scope",
        ));
    }
    for function in functions.iter_mut() {
        function.home = if function.containing.is_some() {
            LambdaHome::DisplayClass
        } else if function.captures_this {
            LambdaHome::EnclosingType
        } else if function.is_lambda && function.envs.is_empty() {
            LambdaHome::ClosureType
        } else {
            LambdaHome::StaticMethod
        };
    }
    for (ordinal, index) in visit.iter().enumerate() {
        functions[*index].ordinal = ordinal;
    }
    let mut suffix = 0;
    for scope in &shape.preorder {
        for env in envs.iter_mut().filter(|env| env.scope == *scope && !env.removed) {
            env.suffix = suffix;
            env.class = DisplayClass {
                symbol: display_class_symbol(enclosing, method_ordinal, suffix),
                fields: env.variables.iter().map(Variable::field).collect(),
                is_struct: env.is_struct,
                suffix,
            };
            suffix += 1;
        }
    }
    Ok(envs)
}

/// The `<>9` field on the closure type: `public static readonly <>c <>9`, csc's flags.
#[must_use]
pub(crate) fn singleton_field(enclosing: &TypeSymbol) -> FieldReference {
    FieldReference {
        declaring_type: closure_symbol(enclosing),
        name: Box::from(SINGLETON_FIELD),
        ty: closure_symbol(enclosing),
        is_static: true,
        is_readonly: true,
        is_volatile: false,
        accessibility: Accessibility::Public,
        constant: None,
        declaring_instantiation: None,
    }
}

/// A read of the singleton, `<>c.<>9` -- the receiver an uncached delegate creation names.
#[must_use]
fn singleton_read(enclosing: &TypeSymbol) -> BoundExpr {
    BoundExpr {
        ty: closure_symbol(enclosing),
        kind: BoundExprKind::FieldAccess {
            receiver: Box::new(BoundExpr {
                ty: closure_symbol(enclosing),
                kind: BoundExprKind::TypeReference(closure_symbol(enclosing)),
            }),
            field: Some(singleton_field(enclosing)),
            name: Box::from(SINGLETON_FIELD),
        },
    }
}

/// The field a captured name -- or `this` -- lives on, once it has been hoisted.
#[must_use]
fn capture_field(display: &DisplayClass, name: &str, ty: &TypeSymbol) -> FieldReference {
    FieldReference {
        declaring_type: display.symbol.clone(),
        name: Box::from(name),
        ty: ty.clone(),
        is_static: false,
        is_readonly: false,
        is_volatile: false,
        accessibility: Accessibility::Public,
        constant: None,
        declaring_instantiation: None,
    }
}

/// `<receiver>.<name>` for a hoisted capture.
#[must_use]
fn capture_access(display: &DisplayClass, name: &str, ty: &TypeSymbol, receiver: BoundExpr) -> BoundExpr {
    BoundExpr {
        ty: ty.clone(),
        kind: BoundExprKind::FieldAccess {
            receiver: Box::new(receiver),
            name: Box::from(name),
            field: Some(capture_field(display, name, ty)),
        },
    }
}

/// `<target> = <value>;`
#[must_use]
fn assign(target: BoundExpr, value: BoundExpr, span: Span) -> BoundStmt {
    BoundStmt {
        kind: BoundStmtKind::Expression(BoundExpr {
            ty: target.ty.clone(),
            kind: BoundExprKind::Assignment {
                operator: lamella_syntax::ast::AssignmentOperator::Assign,
                target: Box::new(target),
                value: Box::new(value),
                checked: false,
            },
        }),
        span,
    }
}

/// A lambda's body as the block it means: `return e;` for a value, the expression alone for a
/// `void` delegate, and a written block unchanged.
///
/// The same two-way choice `parse_expression_body` makes for `=> e;`, and for the same reason --
/// getting it backwards is CS0127 or CS0161.
fn lambda_block(invoke: &MethodReference, body: &BoundLambdaBody, span: Span) -> BoundStmt {
    match body {
        BoundLambdaBody::Expression(value) if invoke.return_type.is_void() => BoundStmt {
            kind: BoundStmtKind::Expression(value.clone()),
            span,
        },
        BoundLambdaBody::Expression(value) => BoundStmt {
            kind: BoundStmtKind::Return(Some(value.clone())),
            span,
        },
        BoundLambdaBody::Block(statement) => statement.clone(),
    }
}

/// Prepends `prologue` to `body`, flattening a block so a method does not gain a level of nesting
/// for every feature that needs a prologue.
fn prepend(mut prologue: Vec<BoundStmt>, body: BoundStmt, span: Span) -> BoundStmt {
    if prologue.is_empty() {
        return body;
    }
    match body.kind {
        BoundStmtKind::Block(statements) => prologue.extend(statements),
        other => prologue.push(BoundStmt {
            kind: other,
            span: body.span,
        }),
    }
    BoundStmt {
        kind: BoundStmtKind::Block(prologue),
        span,
    }
}

/// Which of the two walks over the method body is running.
///
/// **THE SAME WALK RUNS TWICE, AND THAT IS THE POINT.** The first function's home depends on the
/// LAST function's captures -- a variable a later lambda captures makes an earlier local function's
/// environment a class -- so nothing can be decided until every function has been seen.
///
/// One walker, two modes. A second WALKER is the worse alternative: `Lowering::statement` is
/// exhaustive over every statement kind with no wildcard, deliberately, so a copy of it is a
/// second place for a new statement kind to be forgotten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Find the functions. Records nothing else, renames nothing, and returns every node unchanged.
    Survey,
    /// Replace each site and build each function's method.
    Rewrite,
}

/// One function body being rewritten -- or the method's own.
struct Context {
    /// The function, as an index into the survey; `None` for the method body.
    function: Option<usize>,
    /// The environments this body owns that the walk is inside, innermost last.
    active: Vec<usize>,
}

/// Lowers every lambda and local function in one method body.
///
///
/// Returns `Ok(None)` when the body holds no lambda and no local function -- the overwhelmingly
/// common case, and the one that must stay on the ordinary emission path.
pub(crate) fn lower_lambdas(
    enclosing: &TypeSymbol,
    method_name: &str,
    ordinal: usize,
    body: &BoundStmt,
    params: &[(Box<str>, TypeSymbol)],
    tree: &dyn ScopeTree,
    has_this: bool,
) -> Result<Option<LambdaLowering>, EmitError> {
    let mut state = Lowering {
        enclosing,
        method_name,
        ordinal,
        cached: method_name != ".cctor",
        pass: Pass::Survey,
        tree,
        functions: Vec::new(),
        converted: BTreeSet::new(),
        lambda_index: 0,
        envs: Vec::new(),
        contexts: Vec::new(),
        sites: Vec::new(),
        error: None,
        span: body.span,
        temporaries: 0,
    };
    let _ = state.statement(body);
    if let Some(error) = state.error {
        return Err(error);
    }
    if state.functions.is_empty() {
        return Ok(None);
    }
    for function in &mut state.functions {
        if let FunctionKey::Local(id) = function.key {
            function.converted = state.converted.contains(&id);
        }
    }
    state.envs = plan(&mut state.functions, tree, enclosing, ordinal, has_this)?;
    state.pass = Pass::Rewrite;
    state.lambda_index = 0;
    let root = tree.root().map(|scope| tree.canonical(scope));
    let root_env = state
        .envs
        .iter()
        .position(|env| !env.removed && Some(env.scope) == root);
    state.contexts.push(Context {
        function: None,
        active: root_env.into_iter().collect(),
    });
    let rewritten = state.statement(body);
    if let Some(error) = state.error {
        return Err(error);
    }
    let prologue = match root_env {
        Some(env) => state.prologue(env, params, body.span),
        None => Vec::new(),
    };
    let rewritten = prepend(prologue, rewritten, body.span);
    let mut display = None;
    let mut structs = Vec::new();
    for env in state.envs.into_iter().filter(|env| !env.removed) {
        if env.is_struct {
            structs.push(env.class);
        } else {
            display = Some(env.class);
        }
    }
    Ok(Some(LambdaLowering {
        body: rewritten,
        sites: state.sites,
        display,
        structs,
    }))
}

struct Lowering<'a> {
    /// The type whose `<>c` these sites land on -- part of the closure symbol, so two enclosing
    /// types do not share one key.
    enclosing: &'a TypeSymbol,
    method_name: &'a str,
    ordinal: usize,
    /// Whether a lambda in this method gets a cache slot; see [`LoweredLambda::cache_name`].
    cached: bool,
    /// Which walk this is; see [`Pass`].
    pass: Pass,
    tree: &'a dyn ScopeTree,
    /// SURVEY fills this, in walk order; the plan fills in each one's place.
    functions: Vec<Function>,
    /// SURVEY fills this: the local functions some conversion makes a delegate of.
    converted: BTreeSet<LocalFunctionId>,
    /// The next lambda's place in the walk, which is how both passes name the same lambda.
    lambda_index: usize,
    envs: Vec<Env>,
    /// The bodies being rewritten, innermost last.
    contexts: Vec<Context>,
    sites: Vec<LoweredLambda>,
    error: Option<EmitError>,
    /// The span of the statement being rewritten. A bound EXPRESSION carries no span of its own,
    /// so a synthesized statement takes the span of the statement it was written in.
    span: Span,
    /// The next compiler temporary's number, for the names a hoisted pattern or iteration variable
    /// is renamed to.
    temporaries: usize,
}

impl Lowering<'_> {
    fn fail(&mut self, reason: &'static str) {
        self.error.get_or_insert(EmitError::Unsupported(reason));
    }

    fn function_index(&self, key: FunctionKey) -> Option<usize> {
        self.functions.iter().position(|function| function.key == key)
    }

    fn context(&self) -> &Context {
        self.contexts.last().expect("a context is always open while rewriting")
    }

    /// The function whose body is being rewritten, or `None` in the method body.
    fn current_function(&self) -> Option<&Function> {
        self.contexts
            .last()
            .and_then(|context| context.function)
            .map(|index| &self.functions[index])
    }

    /// The environment holding `name` as it is read here, when it is hoisted.
    ///
    /// **A FUNCTION'S OWN CAPTURE LIST IS EXACT**: it names the declaring scope, so a capture is
    /// found in the one environment holding that variable. A name the function does not capture is
    /// its own -- or, in the method body, the method's -- and lives in the innermost environment
    /// this body owns and the walk is inside, if any. Within one function the binder admits no two
    /// visible locals of one name (CS0136), so the innermost is the one.
    fn environment_of(&self, name: &str) -> Option<usize> {
        if let Some(function) = self.current_function() {
            if let Some(variable) = function.variables.iter().find(
                |variable| matches!(variable, Variable::Named { name: captured, .. } if &**captured == name),
            ) {
                return self
                    .envs
                    .iter()
                    .position(|env| !env.removed && env.variables.iter().any(|v| v.same(variable)));
            }
        }
        self.context()
            .active
            .iter()
            .rev()
            .copied()
            .find(|env| self.envs[*env].class.field_type(name).is_some())
    }

    /// The expression that reaches environment `env` from the body being rewritten: the local
    /// that holds it, the by-reference parameter that carries it -- both spelled by the same name
    /// -- or, in a method of the display class itself, `this`.
    fn environment_access(&mut self, env: usize) -> BoundExpr {
        let class = &self.envs[env].class;
        let symbol = class.symbol.clone();
        let local = class.local();
        let context = self.context();
        let owned = context.active.contains(&env);
        let (received, lives_on) = match context.function.map(|index| &self.functions[index]) {
            Some(function) => (
                class.is_struct && function.envs.contains(&env),
                function.containing == Some(env),
            ),
            None => (false, false),
        };
        if owned || received {
            BoundExpr {
                ty: symbol,
                kind: BoundExprKind::Local(local),
            }
        } else if lives_on {
            BoundExpr {
                ty: symbol,
                kind: BoundExprKind::This,
            }
        } else {
            self.fail("a closure environment was named where it cannot be reached");
            BoundExpr {
                ty: TypeSymbol::Error,
                kind: BoundExprKind::Error,
            }
        }
    }

    /// `this` -- the enclosing instance -- as the body being rewritten reaches it: its own `this`,
    /// unless the body is a method of a display class, which holds the instance in `<>4__this`.
    fn enclosing_instance(&mut self) -> BoundExpr {
        let lives_on = self.current_function().and_then(|function| match function.home {
            LambdaHome::DisplayClass => function.containing,
            _ => None,
        });
        match lives_on {
            Some(env) => {
                let class = self.envs[env].class.clone();
                if !class.hoists_this() {
                    self.fail("a display class method read `this` its class does not hold");
                }
                capture_access(
                    &class,
                    THIS_FIELD,
                    self.enclosing,
                    BoundExpr {
                        ty: class.symbol.clone(),
                        kind: BoundExprKind::This,
                    },
                )
            }
            None => BoundExpr {
                ty: self.enclosing.clone(),
                kind: BoundExprKind::This,
            },
        }
    }

    /// The stores that seed environment `env` where it opens: `<>4__this` and each captured
    /// PARAMETER, in field order -- csc's order, measured (`a, <>4__this` stores `a` first). A
    /// captured local is not copied: its declaration IS its store, and was rewritten into one.
    ///
    /// A class is allocated first, `newobj` ahead of everything; a struct is a zeroed local.
    fn prologue(&mut self, env: usize, params: &[(Box<str>, TypeSymbol)], span: Span) -> Vec<BoundStmt> {
        let class = self.envs[env].class.clone();
        let mut prologue: Vec<BoundStmt> = Vec::new();
        let initializer = (!class.is_struct).then(|| BoundExpr {
            ty: class.symbol.clone(),
            kind: BoundExprKind::ObjectCreation {
                constructor: Some(MethodReference {
                    declaring_type: class.symbol.clone(),
                    name: Box::from(".ctor"),
                    parameters: Vec::new(),
                    return_type: TypeSymbol::Special(SpecialType::Void),
                    is_static: false,
                    is_vararg: false,
                    instantiation: None,
                    declaring_instantiation: None,
                }),
                arguments: Vec::new(),
                initializer: None,
            },
        });
        prologue.push(BoundStmt {
            kind: BoundStmtKind::Local {
                ty: class.symbol.clone(),
                declarators: alloc::vec![lamella_binder::BoundDeclarator {
                    name: class.local(),
                    initializer,
                }],
            },
            span,
        });
        let instance = BoundExpr {
            ty: class.symbol.clone(),
            kind: BoundExprKind::Local(class.local()),
        };
        for field in &class.fields {
            match field {
                DisplayField::This => {
                    let value = BoundExpr {
                        ty: self.enclosing.clone(),
                        kind: BoundExprKind::This,
                    };
                    let target = capture_access(&class, THIS_FIELD, self.enclosing, instance.clone());
                    prologue.push(assign(target, value, span));
                }
                DisplayField::Named(name, ty) => {
                    if let Some((_, param_ty)) = params.iter().find(|(param, _)| param == name) {
                        let value = BoundExpr {
                            ty: param_ty.clone(),
                            kind: BoundExprKind::Local(name.clone()),
                        };
                        let target = capture_access(&class, name, ty, instance.clone());
                        prologue.push(assign(target, value, span));
                    }
                }
            }
        }
        prologue
    }

    /// The method a local function becomes, as a call to it names it.
    fn lowered_reference(&self, index: usize) -> MethodReference {
        let function = &self.functions[index];
        let declaring_type = match function.home {
            LambdaHome::DisplayClass => function
                .containing
                .map_or_else(|| self.enclosing.clone(), |env| self.envs[env].class.symbol.clone()),
            _ => self.enclosing.clone(),
        };
        let mut parameters: Vec<TypeSymbol> = Vec::new();
        for (position, (_, ty)) in function.parameters.iter().enumerate() {
            let by_ref = function
                .parameter_modes
                .get(position)
                .is_some_and(|mode| *mode != ParameterMode::Value);
            parameters.push(if by_ref {
                TypeSymbol::ByRef(Box::new(ty.clone()))
            } else {
                ty.clone()
            });
        }
        for env in self.struct_environments(index) {
            parameters.push(TypeSymbol::ByRef(Box::new(self.envs[env].class.symbol.clone())));
        }
        MethodReference {
            declaring_type,
            name: self.method_name_of(index),
            parameters,
            return_type: function.return_type.clone(),
            is_static: function.home == LambdaHome::StaticMethod,
            is_vararg: false,
            instantiation: None,
            declaring_instantiation: None,
        }
    }

    /// A function's synthesized method name; see [`LoweredLambda::method_name`].
    fn method_name_of(&self, index: usize) -> Box<str> {
        let function = &self.functions[index];
        let on_class = function.home == LambdaHome::DisplayClass;
        Box::from(match (function.is_lambda, on_class) {
            (true, true) => format!("<{}>b__{}", self.method_name, function.ordinal),
            (true, false) => format!("<{}>b__{}_{}", self.method_name, self.ordinal, function.ordinal),
            (false, true) => format!("<{}>g__{}|{}", self.method_name, function.name, function.ordinal),
            (false, false) => format!(
                "<{}>g__{}|{}_{}",
                self.method_name, function.name, self.ordinal, function.ordinal
            ),
        })
    }

    /// The struct environments function `index` takes by reference, in order.
    fn struct_environments(&self, index: usize) -> Vec<usize> {
        self.functions[index]
            .envs
            .iter()
            .copied()
            .filter(|env| self.envs[*env].is_struct)
            .collect()
    }

    /// Rewrites a function's body in its own context, with the prologue of the environment its
    /// body scope owns.
    fn function_body(
        &mut self,
        index: usize,
        body: &BoundStmt,
        params: &[(Box<str>, TypeSymbol)],
    ) -> BoundStmt {
        let body_scope = self.functions[index]
            .body_scope
            .map(|scope| self.tree.canonical(scope));
        let owned = self
            .envs
            .iter()
            .position(|env| !env.removed && Some(env.scope) == body_scope);
        self.contexts.push(Context {
            function: Some(index),
            active: owned.into_iter().collect(),
        });
        let rewritten = self.statement(body);
        let prologue = match owned {
            Some(env) => self.prologue(env, params, body.span),
            None => Vec::new(),
        };
        self.contexts.pop();
        prepend(prologue, rewritten, body.span)
    }

    /// Replaces one lambda site, recording the method it becomes.
    fn lambda(&mut self, expr: &BoundExpr) -> BoundExpr {
        let BoundExprKind::Lambda {
            delegate_type,
            invoke,
            parameters,
            body,
            captures,
            captures_this,
            declared_in,
            captured_functions,
            body_scope,
            this_position,
        } = &expr.kind
        else {
            return expr.clone();
        };
        let walk_index = self.lambda_index;
        self.lambda_index += 1;
        let block = lambda_block(invoke, body, self.span);
        if self.pass == Pass::Survey {
            self.functions.push(Function {
                key: FunctionKey::Lambda(walk_index),
                name: Box::from(""),
                declared_in: *declared_in,
                body_scope: *body_scope,
                variables: variables_of(captures, *captures_this, *this_position, self.tree),
                captured_functions: captured_functions.clone(),
                is_lambda: true,
                converted: true,
                parameters: parameters.clone(),
                parameter_modes: Vec::new(),
                return_type: invoke.return_type.clone(),
                scope: ScopeId(0),
                envs: Vec::new(),
                containing: None,
                captures_this: false,
                ordinal: 0,
                home: LambdaHome::ClosureType,
            });
            let _ = self.statement(&block);
            return expr.clone();
        }
        let Some(index) = self.function_index(FunctionKey::Lambda(walk_index)) else {
            self.fail("a lambda reached the rewrite that the survey did not see");
            return expr.clone();
        };
        let home = self.functions[index].home;
        let method_name = self.method_name_of(index);
        let function_ordinal = self.functions[index].ordinal;
        let cache_name: Option<Box<str>> = (self.cached && home == LambdaHome::ClosureType)
            .then(|| Box::from(format!("<>9__{}_{}", self.ordinal, function_ordinal)));
        let slot = self.sites.len();
        self.sites.push(LoweredLambda {
            home,
            method_name: method_name.clone(),
            cache_name: cache_name.clone(),
            delegate_type: delegate_type.clone(),
            parameters: parameters.clone(),
            return_type: invoke.return_type.clone(),
            body: BoundStmt {
                kind: BoundStmtKind::Error,
                span: self.span,
            },
            ordinal: self.ordinal,
            local: None,
        });
        let rewritten = self.function_body(index, &block, parameters);
        self.sites[slot].body = rewritten;
        let declaring = match home {
            LambdaHome::ClosureType => closure_symbol(self.enclosing),
            LambdaHome::DisplayClass => match self.functions[index].containing {
                Some(env) => self.envs[env].class.symbol.clone(),
                None => self.enclosing.clone(),
            },
            LambdaHome::EnclosingType | LambdaHome::StaticMethod => self.enclosing.clone(),
        };
        let target = MethodReference {
            declaring_type: declaring,
            name: method_name,
            parameters: parameters.iter().map(|(_, ty)| ty.clone()).collect(),
            return_type: invoke.return_type.clone(),
            is_static: false,
            is_vararg: false,
            instantiation: None,
            declaring_instantiation: None,
        };
        let Some(cache_name) = cache_name else {
            let receiver = match home {
                LambdaHome::ClosureType => singleton_read(self.enclosing),
                LambdaHome::EnclosingType | LambdaHome::StaticMethod => self.enclosing_instance(),
                LambdaHome::DisplayClass => match self.functions[index].containing {
                    Some(env) => self.environment_access(env),
                    None => self.enclosing_instance(),
                },
            };
            return BoundExpr {
                ty: delegate_type.clone(),
                kind: BoundExprKind::DelegateCreation {
                    delegate_type: delegate_type.clone(),
                    target,
                    receiver: Some(Box::new(receiver)),
                    virtual_slot: false,
                },
            };
        };
        let cache = FieldReference {
            declaring_type: closure_symbol(self.enclosing),
            name: cache_name,
            ty: delegate_type.clone(),
            is_static: true,
            is_readonly: false,
            is_volatile: false,
            accessibility: Accessibility::Public,
            constant: None,
            declaring_instantiation: None,
        };
        BoundExpr {
            ty: delegate_type.clone(),
            kind: BoundExprKind::CachedDelegate {
                cache: Box::new(cache),
                singleton: Box::new(singleton_field(self.enclosing)),
                target: Box::new(target),
                delegate_type: delegate_type.clone(),
            },
        }
    }

    /// A local function's declaration: in the survey, the function; in the rewrite, the method it
    /// becomes. The statement itself becomes nothing.
    fn local_function(&mut self, function: &BoundLocalFunction) -> BoundStmtKind {
        if self.pass == Pass::Survey {
            self.functions.push(Function {
                key: FunctionKey::Local(function.id),
                name: function.name.clone(),
                declared_in: function.declared_in,
                body_scope: function.body_scope,
                variables: variables_of(
                    &function.captures,
                    function.captures_this,
                    function.this_position,
                    self.tree,
                ),
                captured_functions: function.captured_functions.clone(),
                is_lambda: false,
                converted: false,
                parameters: function.parameters.clone(),
                parameter_modes: function.parameter_modes.clone(),
                return_type: function.return_type.clone(),
                scope: ScopeId(0),
                envs: Vec::new(),
                containing: None,
                captures_this: false,
                ordinal: 0,
                home: LambdaHome::StaticMethod,
            });
            let _ = self.statement(&function.body);
            return BoundStmtKind::Empty;
        }
        let Some(index) = self.function_index(FunctionKey::Local(function.id)) else {
            self.fail("a local function reached the rewrite that the survey did not see");
            return BoundStmtKind::Empty;
        };
        let environments: Vec<(Box<str>, TypeSymbol)> = self
            .struct_environments(index)
            .into_iter()
            .map(|env| (self.envs[env].class.local(), self.envs[env].class.symbol.clone()))
            .collect();
        let slot = self.sites.len();
        self.sites.push(LoweredLambda {
            home: self.functions[index].home,
            method_name: self.method_name_of(index),
            cache_name: None,
            delegate_type: TypeSymbol::Error,
            parameters: function.parameters.clone(),
            return_type: function.return_type.clone(),
            body: BoundStmt {
                kind: BoundStmtKind::Error,
                span: self.span,
            },
            ordinal: self.ordinal,
            local: Some(LocalFunctionShape {
                parameter_modes: function.parameter_modes.clone(),
                defaults: function.defaults.clone(),
                is_params: function.is_params,
                environments,
            }),
        });
        let rewritten = self.function_body(index, &function.body, &function.parameters);
        self.sites[slot].body = rewritten;
        BoundStmtKind::Empty
    }

    /// A call to local function `index`: the lowered method, its receiver, and each struct
    /// environment it takes, by reference, after the arguments.
    fn local_function_call(&mut self, index: usize, arguments: Vec<BoundExpr>, ty: &TypeSymbol) -> BoundExpr {
        let target = self.lowered_reference(index);
        let receiver = self.local_function_receiver(index);
        let mut arguments = arguments;
        for env in self.struct_environments(index) {
            let access = self.environment_access(env);
            arguments.push(BoundExpr {
                ty: access.ty.clone(),
                kind: BoundExprKind::Ref {
                    out: false,
                    operand: Box::new(access),
                },
            });
        }
        BoundExpr {
            ty: ty.clone(),
            kind: BoundExprKind::Call {
                callee: Box::new(BoundExpr {
                    ty: TypeSymbol::Error,
                    kind: BoundExprKind::MethodGroup {
                        receiver: Box::new(receiver),
                        name: target.name.clone(),
                    },
                }),
                arguments,
                method: Some(target),
            },
        }
    }

    /// What a call to, or a delegate over, local function `index` is made on: its type for a
    /// static method, the enclosing instance for one on the enclosing type, the environment it
    /// lives on otherwise.
    fn local_function_receiver(&mut self, index: usize) -> BoundExpr {
        match self.functions[index].home {
            LambdaHome::StaticMethod | LambdaHome::ClosureType => BoundExpr {
                ty: self.enclosing.clone(),
                kind: BoundExprKind::TypeReference(self.enclosing.clone()),
            },
            LambdaHome::EnclosingType => self.enclosing_instance(),
            LambdaHome::DisplayClass => match self.functions[index].containing {
                Some(env) => self.environment_access(env),
                None => self.enclosing_instance(),
            },
        }
    }

    /// Rewrites one expression, replacing every lambda, every hoisted variable and every
    /// reference to a local function inside it.
    ///
    /// A replacement STOPS the walk at that node, which is what makes nesting work: `map_expr`
    /// does not recurse into a value the callback returned, so a lambda's body is reached only
    /// through [`Self::lambda`], and a call's arguments are rewritten here before the call is.
    fn expression(&mut self, expr: &BoundExpr) -> BoundExpr {
        crate::awaitlower::map_expr(expr, &mut |inner| self.replace(inner))
    }

    /// The replacement [`Self::expression`] makes at `expr`, or `None` to keep walking.
    fn replace(&mut self, expr: &BoundExpr) -> Option<BoundExpr> {
        match &expr.kind {
            BoundExprKind::Lambda { .. } => Some(self.lambda(expr)),
            BoundExprKind::Call {
                arguments,
                method: Some(method),
                ..
            } => {
                let id = LocalFunctionId::of(method)?;
                if self.pass == Pass::Survey {
                    return None;
                }
                let Some(index) = self.function_index(FunctionKey::Local(id)) else {
                    self.fail("a call named a local function the survey did not see");
                    return Some(expr.clone());
                };
                let arguments: Vec<BoundExpr> =
                    arguments.iter().map(|argument| self.expression(argument)).collect();
                Some(self.local_function_call(index, arguments, &expr.ty))
            }
            BoundExprKind::DelegateCreation {
                delegate_type,
                target,
                ..
            } => {
                let id = LocalFunctionId::of(target)?;
                if self.pass == Pass::Survey {
                    self.converted.insert(id);
                    return None;
                }
                let Some(index) = self.function_index(FunctionKey::Local(id)) else {
                    self.fail("a conversion named a local function the survey did not see");
                    return Some(expr.clone());
                };
                let lowered = self.lowered_reference(index);
                let receiver = match self.functions[index].home {
                    LambdaHome::StaticMethod | LambdaHome::ClosureType => None,
                    _ => Some(Box::new(self.local_function_receiver(index))),
                };
                Some(BoundExpr {
                    ty: expr.ty.clone(),
                    kind: BoundExprKind::DelegateCreation {
                        delegate_type: delegate_type.clone(),
                        target: lowered,
                        receiver,
                        virtual_slot: false,
                    },
                })
            }
            _ if self.pass == Pass::Survey => None,
            BoundExprKind::This => {
                let class = self.current_function().and_then(|function| match function.home {
                    LambdaHome::DisplayClass => function.containing,
                    _ => None,
                });
                match class {
                    Some(env) if self.envs[env].class.hoists_this() => Some(self.enclosing_instance()),
                    _ => None,
                }
            }
            BoundExprKind::Local(name) => {
                let env = self.environment_of(name)?;
                let class = self.envs[env].class.clone();
                let ty = class.field_type(name)?.clone();
                let receiver = self.environment_access(env);
                Some(capture_access(&class, name, &ty, receiver))
            }
            BoundExprKind::TypeTest {
                operation,
                operand,
                target,
                declares: Some(name),
            } => {
                let env = self.environment_of(name)?;
                let class = self.envs[env].class.clone();
                let ty = class.field_type(name)?.clone();
                let temporary: Box<str> = format!("<>p__{}", self.next_temporary()).into();
                let operand = self.expression(operand);
                let test = BoundExpr {
                    ty: expr.ty.clone(),
                    kind: BoundExprKind::TypeTest {
                        operation: *operation,
                        operand: Box::new(operand),
                        target: target.clone(),
                        declares: Some(temporary.clone()),
                    },
                };
                let receiver = self.environment_access(env);
                let store = BoundExpr {
                    ty: ty.clone(),
                    kind: BoundExprKind::Assignment {
                        operator: lamella_syntax::ast::AssignmentOperator::Assign,
                        target: Box::new(capture_access(&class, name, &ty, receiver)),
                        value: Box::new(BoundExpr {
                            ty: ty.clone(),
                            kind: BoundExprKind::Local(temporary),
                        }),
                        checked: false,
                    },
                };
                Some(BoundExpr {
                    ty: expr.ty.clone(),
                    kind: BoundExprKind::Sequence {
                        spilled: alloc::vec![test, store],
                        value: Box::new(BoundExpr {
                            ty: expr.ty.clone(),
                            kind: BoundExprKind::Temp(0),
                        }),
                    },
                })
            }
            _ => None,
        }
    }

    fn next_temporary(&mut self) -> usize {
        let next = self.temporaries;
        self.temporaries += 1;
        next
    }

    fn optional(&mut self, expr: &Option<BoundExpr>) -> Option<BoundExpr> {
        expr.as_ref().map(|inner| self.expression(inner))
    }

    fn boxed(&mut self, stmt: &BoundStmt) -> Box<BoundStmt> {
        Box::new(self.statement(stmt))
    }

    /// A local declaration, with any HOISTED declarator turned into a field store.
    ///
    /// **A CAPTURED LOCAL HAS NO SLOT AT ALL.** Its storage is the environment's field, so
    /// `int n = 3;` stores `n` there and declares nothing. Keeping the slot and assigning both
    /// would give the enclosing method a copy the closure cannot see.
    ///
    /// A declarator with NO initializer disappears entirely: the field is already zeroed.
    ///
    /// ONE STATEMENT PER DECLARATOR, IN SOURCE ORDER, because one statement can declare both kinds
    /// and collecting the survivors would reorder their initializers' side effects.
    fn local_declaration(
        &mut self,
        ty: &TypeSymbol,
        declarators: &[lamella_binder::BoundDeclarator],
    ) -> BoundStmtKind {
        let hoisted: Vec<Option<usize>> = if self.pass == Pass::Rewrite {
            declarators
                .iter()
                .map(|declarator| {
                    let env = self.environment_of(&declarator.name)?;
                    self.context().active.contains(&env).then_some(env)
                })
                .collect()
        } else {
            alloc::vec![None; declarators.len()]
        };
        if hoisted.iter().all(Option::is_none) {
            return BoundStmtKind::Local {
                ty: ty.clone(),
                declarators: declarators
                    .iter()
                    .map(|declarator| lamella_binder::BoundDeclarator {
                        name: declarator.name.clone(),
                        initializer: self.optional(&declarator.initializer),
                    })
                    .collect(),
            };
        }
        let mut statements: Vec<BoundStmt> = Vec::new();
        for (declarator, env) in declarators.iter().zip(hoisted) {
            let Some(env) = env else {
                statements.push(BoundStmt {
                    kind: BoundStmtKind::Local {
                        ty: ty.clone(),
                        declarators: alloc::vec![lamella_binder::BoundDeclarator {
                            name: declarator.name.clone(),
                            initializer: self.optional(&declarator.initializer),
                        }],
                    },
                    span: self.span,
                });
                continue;
            };
            let Some(initializer) = &declarator.initializer else {
                continue;
            };
            let value = self.expression(initializer);
            let class = self.envs[env].class.clone();
            let receiver = self.environment_access(env);
            let field_ty = class.field_type(&declarator.name).cloned().unwrap_or_else(|| ty.clone());
            let target = capture_access(&class, &declarator.name, &field_ty, receiver);
            statements.push(assign(target, value, self.span));
        }
        BoundStmtKind::Block(statements)
    }

    /// One catch clause. Its exception variable's scope is the clause, which the binder recorded
    /// by the clause's span; when something captures the variable, the clause opens that scope's
    /// environment, the exception lands in a temporary, and the temporary is stored into the field
    /// before anything reads it -- ahead of the filter when there is one, else atop the body.
    fn catch_clause(&mut self, catch: &lamella_binder::BoundCatch) -> lamella_binder::BoundCatch {
        let opened = (self.pass == Pass::Rewrite)
            .then(|| self.opened_environment_at(catch.span))
            .flatten();
        let Some(env) = opened else {
            return lamella_binder::BoundCatch {
                exception_type: catch.exception_type.clone(),
                name: catch.name.clone(),
                filter: self.optional(&catch.filter),
                body: self.boxed(&catch.body),
                span: catch.span,
            };
        };
        if let Some(context) = self.contexts.last_mut() {
            context.active.push(env);
        }
        let class = self.envs[env].class.clone();
        let hoisted = catch
            .name
            .as_ref()
            .filter(|name| class.field_type(name).is_some())
            .cloned();
        let (name, store) = match &hoisted {
            Some(name) => {
                let temporary: Box<str> = format!("<>e__{}", self.next_temporary()).into();
                let ty = class.field_type(name).cloned().unwrap_or(TypeSymbol::Error);
                let receiver = self.environment_access(env);
                let target = capture_access(&class, name, &ty, receiver);
                let value = BoundExpr {
                    ty: ty.clone(),
                    kind: BoundExprKind::Local(temporary.clone()),
                };
                let store = BoundExpr {
                    ty,
                    kind: BoundExprKind::Assignment {
                        operator: lamella_syntax::ast::AssignmentOperator::Assign,
                        target: Box::new(target),
                        value: Box::new(value),
                        checked: false,
                    },
                };
                (Some(temporary), Some(store))
            }
            None => (catch.name.clone(), None),
        };
        let filter = self.optional(&catch.filter);
        let body = self.statement(&catch.body);
        if let Some(context) = self.contexts.last_mut() {
            context.active.pop();
        }
        let (filter, mut prologue) = match (filter, store) {
            (Some(filter), Some(store)) => (
                Some(BoundExpr {
                    ty: filter.ty.clone(),
                    kind: BoundExprKind::Sequence {
                        spilled: alloc::vec![store, filter.clone()],
                        value: Box::new(BoundExpr {
                            ty: filter.ty,
                            kind: BoundExprKind::Temp(1),
                        }),
                    },
                }),
                Vec::new(),
            ),
            (filter, Some(store)) => (
                filter,
                alloc::vec![BoundStmt {
                    kind: BoundStmtKind::Expression(store),
                    span: catch.body.span,
                }],
            ),
            (filter, None) => (filter, Vec::new()),
        };
        let mut declaration = self.prologue(env, &[], catch.body.span);
        declaration.append(&mut prologue);
        lamella_binder::BoundCatch {
            exception_type: catch.exception_type.clone(),
            name,
            filter,
            body: Box::new(prepend(declaration, body, catch.body.span)),
            span: catch.span,
        }
    }

    /// The environment of the scope a construct with span `span` opened, when this body owns it,
    /// something captures its variables, and the walk has not already entered it.
    fn opened_environment_at(&self, span: Span) -> Option<usize> {
        let context = self.context();
        let root = self.tree.root().map(|scope| self.tree.canonical(scope));
        let body = context
            .function
            .and_then(|index| self.functions[index].body_scope)
            .map(|scope| self.tree.canonical(scope));
        self.envs
            .iter()
            .enumerate()
            .find(|(index, env)| {
                !env.removed
                    && Some(env.scope) != root
                    && Some(env.scope) != body
                    && !context.active.contains(index)
                    && self.tree.span(env.scope) == Some(span)
            })
            .map(|(index, _)| index)
    }

    /// The environment `stmt` opens, when it is the statement that opened a scope whose variables
    /// this body owns and something captures.
    fn opened_environment(&self, stmt: &BoundStmt) -> Option<usize> {
        if self.pass != Pass::Rewrite {
            return None;
        }
        self.opened_environment_at(stmt.span)
    }

    /// Rewrites one statement.
    ///
    /// **EXHAUSTIVE ON PURPOSE, WITH NO WILDCARD.** A statement kind added later fails to compile
    /// here rather than silently carrying an unlowered closure past the rewrite.
    fn statement(&mut self, stmt: &BoundStmt) -> BoundStmt {
        if let Some(env) = self.opened_environment(stmt) {
            if let Some(context) = self.contexts.last_mut() {
                context.active.push(env);
            }
            let inner = self.statement_kind(stmt);
            if let Some(context) = self.contexts.last_mut() {
                context.active.pop();
            }
            let declaration = self.prologue(env, &[], stmt.span);
            return prepend(declaration, inner, stmt.span);
        }
        self.statement_kind(stmt)
    }

    fn statement_kind(&mut self, stmt: &BoundStmt) -> BoundStmt {
        self.span = stmt.span;
        let kind = match &stmt.kind {
            BoundStmtKind::Block(statements) => {
                BoundStmtKind::Block(statements.iter().map(|s| self.statement(s)).collect())
            }
            BoundStmtKind::LocalFunction(function) => self.local_function(function),
            BoundStmtKind::Empty => BoundStmtKind::Empty,
            BoundStmtKind::Local { ty, declarators } => self.local_declaration(ty, declarators),
            BoundStmtKind::Expression(expr) => BoundStmtKind::Expression(self.expression(expr)),
            BoundStmtKind::If {
                condition,
                then_branch,
                else_branch,
            } => BoundStmtKind::If {
                condition: self.expression(condition),
                then_branch: self.boxed(then_branch),
                else_branch: else_branch.as_ref().map(|branch| self.boxed(branch)),
            },
            BoundStmtKind::While { condition, body } => BoundStmtKind::While {
                condition: self.expression(condition),
                body: self.boxed(body),
            },
            BoundStmtKind::Return(value) => BoundStmtKind::Return(self.optional(value)),
            BoundStmtKind::DoWhile { body, condition } => BoundStmtKind::DoWhile {
                body: self.boxed(body),
                condition: self.expression(condition),
            },
            BoundStmtKind::For {
                initializer,
                condition,
                iterators,
                body,
            } => BoundStmtKind::For {
                initializer: initializer.iter().map(|s| self.statement(s)).collect(),
                condition: self.optional(condition),
                iterators: iterators.iter().map(|e| self.expression(e)).collect(),
                body: self.boxed(body),
            },
            BoundStmtKind::ForEach {
                name,
                element_type,
                collection,
                element_conversion,
                body,
            } => {
                let collection = self.expression(collection);
                let element_conversion = element_conversion
                    .as_ref()
                    .map(|conversion| Box::new(self.expression(conversion)));
                let hoisted = (self.pass == Pass::Rewrite)
                    .then(|| self.environment_of(name))
                    .flatten()
                    .filter(|env| self.context().active.contains(env));
                match hoisted {
                    Some(env) => {
                        let temporary: Box<str> = format!("<>i__{}", self.next_temporary()).into();
                        let class = self.envs[env].class.clone();
                        let receiver = self.environment_access(env);
                        let target = capture_access(&class, name, element_type, receiver);
                        let store = assign(
                            target,
                            BoundExpr {
                                ty: element_type.clone(),
                                kind: BoundExprKind::Local(temporary.clone()),
                            },
                            body.span,
                        );
                        let body = self.statement(body);
                        BoundStmtKind::ForEach {
                            name: temporary,
                            element_type: element_type.clone(),
                            collection,
                            element_conversion,
                            body: Box::new(prepend(alloc::vec![store], body, stmt.span)),
                        }
                    }
                    None => BoundStmtKind::ForEach {
                        name: name.clone(),
                        element_type: element_type.clone(),
                        collection,
                        element_conversion,
                        body: self.boxed(body),
                    },
                }
            }
            BoundStmtKind::Break => BoundStmtKind::Break,
            BoundStmtKind::Continue => BoundStmtKind::Continue,
            BoundStmtKind::Throw(value) => BoundStmtKind::Throw(self.optional(value)),
            BoundStmtKind::Switch {
                expression,
                sections,
                subject,
            } => BoundStmtKind::Switch {
                expression: self.expression(expression),
                subject: subject.clone(),
                sections: sections
                    .iter()
                    .map(|section| lamella_binder::BoundSwitchSection {
                        labels: section
                            .labels
                            .iter()
                            .map(|label| match label {
                                lamella_binder::BoundSwitchLabel::Pattern {
                                    test,
                                    guard,
                                    text,
                                    catch_all,
                                } => lamella_binder::BoundSwitchLabel::Pattern {
                                    test: test.as_ref().map(|test| self.expression(test)),
                                    guard: guard.as_ref().map(|guard| self.expression(guard)),
                                    text: text.clone(),
                                    catch_all: *catch_all,
                                },
                                other => other.clone(),
                            })
                            .collect(),
                        statements: section
                            .statements
                            .iter()
                            .map(|s| self.statement(s))
                            .collect(),
                    })
                    .collect(),
            },
            BoundStmtKind::Try {
                body,
                catches,
                finally,
            } => BoundStmtKind::Try {
                body: self.boxed(body),
                catches: catches.iter().map(|catch| self.catch_clause(catch)).collect(),
                finally: finally.as_ref().map(|block| self.boxed(block)),
            },
            BoundStmtKind::Lock { expression, body } => BoundStmtKind::Lock {
                expression: self.expression(expression),
                body: self.boxed(body),
            },
            BoundStmtKind::Using { resource, body } => BoundStmtKind::Using {
                resource: resource.iter().map(|s| self.statement(s)).collect(),
                body: self.boxed(body),
            },
            BoundStmtKind::Fixed {
                name,
                element,
                init,
                body,
            } => BoundStmtKind::Fixed {
                name: name.clone(),
                element: element.clone(),
                init: self.expression(init),
                body: self.boxed(body),
            },
            BoundStmtKind::Checked(inner) => BoundStmtKind::Checked(self.boxed(inner)),
            BoundStmtKind::Unchecked(inner) => BoundStmtKind::Unchecked(self.boxed(inner)),
            BoundStmtKind::Labeled { label, body } => BoundStmtKind::Labeled {
                label: label.clone(),
                body: self.boxed(body),
            },
            BoundStmtKind::Goto(label) => BoundStmtKind::Goto(label.clone()),
            BoundStmtKind::GotoCase(value) => BoundStmtKind::GotoCase(*value),
            BoundStmtKind::GotoCaseString(value) => BoundStmtKind::GotoCaseString(value.clone()),
            BoundStmtKind::GotoDefault => BoundStmtKind::GotoDefault,
            BoundStmtKind::Error => BoundStmtKind::Error,
        };
        BoundStmt {
            kind,
            span: stmt.span,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A scope shape written directly.
    ///
    struct Shape {
        /// Child to parent. An id absent from the left column is outermost.
        parents: Vec<(usize, usize)>,
        /// Scope pairs that are one scope: (merged, into).
        merged: Vec<(usize, usize)>,
        /// The scopes that declare a name.
        declaring: Vec<usize>,
        /// The scopes holding a function's parameters.
        functions: Vec<usize>,
    }

    impl ScopeTree for Shape {
        fn parent(&self, scope: ScopeId) -> Option<ScopeId> {
            self.parents
                .iter()
                .find(|(child, _)| *child == scope.0)
                .map(|(_, parent)| ScopeId(*parent))
        }

        fn canonical(&self, scope: ScopeId) -> ScopeId {
            self.merged
                .iter()
                .find(|(merged, _)| *merged == scope.0)
                .map_or(scope, |(_, into)| ScopeId(*into))
        }

        fn is_function(&self, scope: ScopeId) -> bool {
            self.functions.contains(&scope.0)
        }

        fn declares_names(&self, scope: ScopeId) -> bool {
            self.declaring.contains(&scope.0)
        }

        fn span(&self, _scope: ScopeId) -> Option<Span> {
            None
        }

        fn root(&self) -> Option<ScopeId> {
            Some(ScopeId(0))
        }
    }

    fn enclosing() -> TypeSymbol {
        TypeSymbol::Named(vec![Box::from("P")].into_boxed_slice())
    }

    fn int() -> TypeSymbol {
        TypeSymbol::Special(SpecialType::Int32)
    }

    fn named(name: &str, scope: usize) -> Variable {
        Variable::Named {
            name: Box::from(name),
            scope: ScopeId(scope),
            ty: int(),
        }
    }

    fn function(key: FunctionKey, declared_in: usize, body: usize, variables: Vec<Variable>) -> Function {
        Function {
            key,
            name: Box::from("F"),
            declared_in: Some(ScopeId(declared_in)),
            body_scope: Some(ScopeId(body)),
            variables,
            captured_functions: Vec::new(),
            is_lambda: matches!(key, FunctionKey::Lambda(_)),
            converted: matches!(key, FunctionKey::Lambda(_)),
            parameters: Vec::new(),
            parameter_modes: Vec::new(),
            return_type: TypeSymbol::Special(SpecialType::Void),
            scope: ScopeId(0),
            envs: Vec::new(),
            containing: None,
            captures_this: false,
            ordinal: 0,
            home: LambdaHome::ClosureType,
        }
    }

    /// A lambda in a nested block that declares a local is numbered AFTER a lambda
    /// written later in the method body, because the body's own functions come first.
    #[test]
    fn functions_are_numbered_scope_by_scope_not_in_source_order() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 1), (3, 2), (4, 1)],
            merged: vec![(1, 0)],
            declaring: vec![0, 2],
            functions: vec![3, 4],
        };
        let mut functions = vec![
            function(FunctionKey::Lambda(0), 2, 3, Vec::new()),
            function(FunctionKey::Lambda(1), 1, 4, Vec::new()),
        ];
        plan(&mut functions, &shape, &enclosing(), 0, false).expect("plans");
        assert_eq!(functions[0].ordinal, 1, "the nested block's lambda");
        assert_eq!(functions[1].ordinal, 0, "the body's own lambda");
        assert_eq!(functions[0].home, LambdaHome::ClosureType);
    }

    /// A block holding only a local function declares no local, so it is no scope
    /// and source order holds.
    #[test]
    fn a_block_that_declares_nothing_is_not_a_scope() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 1), (3, 2), (4, 1)],
            merged: vec![(1, 0)],
            declaring: vec![0],
            functions: vec![3, 4],
        };
        let mut functions = vec![
            function(FunctionKey::Local(LocalFunctionId(0)), 2, 3, Vec::new()),
            function(FunctionKey::Local(LocalFunctionId(1)), 1, 4, Vec::new()),
        ];
        plan(&mut functions, &shape, &enclosing(), 0, false).expect("plans");
        assert_eq!(functions[0].ordinal, 0);
        assert_eq!(functions[1].ordinal, 1);
        assert_eq!(functions[0].home, LambdaHome::StaticMethod);
    }

    /// Local functions capturing locals take a display STRUCT per scope,
    /// numbered in the scopes' order, and each takes every struct it reaches.
    #[test]
    fn local_functions_share_a_struct_per_scope() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 1), (3, 2), (4, 3)],
            merged: vec![(1, 0)],
            declaring: vec![0, 2],
            functions: vec![4],
        };
        let mut functions = vec![function(
            FunctionKey::Local(LocalFunctionId(0)),
            3,
            4,
            vec![named("total", 0), named("j", 2)],
        )];
        let envs = plan(&mut functions, &shape, &enclosing(), 0, false).expect("plans");
        assert_eq!(envs.len(), 2);
        assert!(envs.iter().all(|env| env.is_struct));
        assert_eq!(envs[0].suffix, 0);
        assert_eq!(envs[1].suffix, 1);
        assert_eq!(functions[0].envs, vec![0, 1], "outer scope first");
        assert_eq!(functions[0].home, LambdaHome::StaticMethod);
    }

    /// Converting a capturing local function to a delegate makes its environment a
    /// CLASS, and the function a method of it.
    #[test]
    fn a_converted_local_function_lives_on_a_display_class() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 1)],
            merged: vec![(1, 0)],
            declaring: vec![0],
            functions: vec![2],
        };
        let mut local = function(FunctionKey::Local(LocalFunctionId(0)), 1, 2, vec![named("k", 0)]);
        local.converted = true;
        let mut functions = vec![local];
        let envs = plan(&mut functions, &shape, &enclosing(), 0, false).expect("plans");
        assert!(!envs[0].is_struct);
        assert_eq!(functions[0].home, LambdaHome::DisplayClass);
        assert_eq!(functions[0].containing, Some(0));
    }

    /// A `this`-only lambda in a nested scope lands on the method body's display
    /// class when that class exists -- `this` is one of the body's variables, so its environment
    /// holds it.
    #[test]
    fn a_this_only_lambda_lands_on_the_body_class_from_a_nested_scope() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 1), (3, 1), (4, 3)],
            merged: vec![(1, 0)],
            declaring: vec![0, 3],
            functions: vec![2, 4],
        };
        let mut functions = vec![
            function(FunctionKey::Lambda(0), 1, 2, vec![named("x", 0)]),
            function(FunctionKey::Lambda(1), 3, 4, vec![Variable::This]),
        ];
        let envs = plan(&mut functions, &shape, &enclosing(), 1, true).expect("plans");
        assert_eq!(envs.len(), 1);
        assert_eq!(functions[1].home, LambdaHome::DisplayClass);
        assert!(envs[0].class.hoists_this());
    }

    /// An environment holding only `this` is dropped, and its function becomes an
    /// instance method of the enclosing type.
    #[test]
    fn an_environment_of_this_alone_is_dropped() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 1)],
            merged: vec![(1, 0)],
            declaring: vec![0],
            functions: vec![2],
        };
        let mut functions = vec![function(
            FunctionKey::Local(LocalFunctionId(0)),
            1,
            2,
            vec![Variable::This],
        )];
        let envs = plan(&mut functions, &shape, &enclosing(), 1, true).expect("plans");
        assert!(envs.iter().all(|env| env.removed));
        assert_eq!(functions[0].home, LambdaHome::EnclosingType);
        assert!(functions[0].envs.is_empty());
    }

    /// The fields follow first capture, `<>4__this` included.
    #[test]
    fn fields_follow_first_capture_this_included() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 1)],
            merged: vec![(1, 0)],
            declaring: vec![0],
            functions: vec![2],
        };
        let mut functions = vec![function(
            FunctionKey::Local(LocalFunctionId(0)),
            1,
            2,
            vec![named("a", 0), Variable::This, named("b", 1)],
        )];
        let envs = plan(&mut functions, &shape, &enclosing(), 1, true).expect("plans");
        assert_eq!(
            envs[0].class.fields,
            vec![
                DisplayField::Named(Box::from("a"), int()),
                DisplayField::This,
                DisplayField::Named(Box::from("b"), int()),
            ]
        );
        assert_eq!(functions[0].home, LambdaHome::EnclosingType);
    }
}
