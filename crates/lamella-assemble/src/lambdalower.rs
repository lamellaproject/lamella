//! Lambda lowering: a lambda expression becomes a method on a synthesized closure type, and the
//! site that wrote it becomes the delegate creation csc writes there (14.5.11, 14.7.3).

use crate::expr::EmitError;
use alloc::boxed::Box;
use alloc::format;
use alloc::vec::Vec;
use lamella_binder::{
    Accessibility, BoundExpr, BoundExprKind, BoundStmt, BoundStmtKind, Capture, FieldReference,
    MethodReference, ScopeId, SpecialType, TypeSymbol,
};
use lamella_binder::statement::BoundLambdaBody;
use lamella_syntax::span::Span;

/// The closure type's simple name -- one per enclosing type, holding every non-capturing lambda
/// that type's methods write. csc's name, and the reason it is a constant rather than a format:
/// there is only ever one, however many methods contribute to it.
pub(crate) const CLOSURE_TYPE: &str = "<>c";

/// The singleton instance field on [`CLOSURE_TYPE`], created by its `.cctor`.
pub(crate) const SINGLETON_FIELD: &str = "<>9";

/// A display class's simple name, before its two indices: the enclosing method's ordinal and the
/// index of the capturing scope within that method. csc's, measured.
pub(crate) const DISPLAY_CLASS_PREFIX: &str = "<>c__DisplayClass";

/// The field a display class holds the enclosing instance in, when a lambda on it reads `this`.
/// Hoisted by NAME rather than by the source's spelling, because `this` has none.
pub(crate) const THIS_FIELD: &str = "<>4__this";

/// The enclosing method's local holding the display-class instance.
///
/// csc emits an unnamed slot here; a name is needed because this emitter addresses locals by one.
/// It is unspellable in C#, so it cannot collide with a source local.
pub(crate) const LOCALS_LOCAL: &str = "<>8__locals";

/// Where a lowered lambda's body lives, which is decided entirely by what it captures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LambdaHome {
    /// Captures nothing: a method on the per-enclosing-type `<>c` singleton, with a cached
    /// delegate.
    ClosureType,
    /// Captures `this` and nothing else: a private instance method of the ENCLOSING type, and NO
    /// cache -- the delegate closes over a particular instance, so there is nothing to share.
    /// csc's, measured: two evaluations of one such lambda are two delegates.
    ///
    /// Conditional on the lambda's OWN scope having no display class: one written beside a
    /// display class lands on it instead, reaching the instance through `<>4__this`. The choice
    /// is per SCOPE rather than per method.
    EnclosingType,
    /// Captures a local, a parameter, or `this` alongside one: an instance method of the display
    /// class the enclosing method allocates, and no cache -- the delegate closes over that
    /// instance.
    DisplayClass,
}

/// The display class one method needs: the type, the fields it holds, and whether the enclosing
/// instance is one of them.
///
/// ONE PER METHOD IN THIS BUILD. A method whose captures come from more than one scope is refused
/// by [`Feature::LambdaCapturingNestedScope`](lamella_syntax::version::Feature) in the binder, so
/// a second scope cannot reach here.
#[derive(Debug, Clone)]
pub(crate) struct DisplayClass {
    /// The synthesized type, qualified by the enclosing type -- see [`closure_symbol`] for why a
    /// bare name is not enough.
    pub(crate) symbol: TypeSymbol,
    /// One field per captured name, in FIRST-SEEN order, named as the source wrote it.
    pub(crate) fields: Vec<(Box<str>, TypeSymbol)>,
    /// Whether `<>4__this` is among them.
    pub(crate) hoists_this: bool,
    /// The scope whose captures it holds. A `this`-only lambda written in THIS scope lands on the
    /// class; one written anywhere else stays on the enclosing type.
    pub(crate) scope: ScopeId,
    /// Its `S`: a running index over the method scopes that capture, from 0, in the order they
    /// opened. Not a depth and not a scope id.
    pub(crate) suffix: usize,
    /// The nearest ENCLOSING scope that also has a class, which this one links to. `None` for the
    /// outermost capturing scope, which carries no link field.
    pub(crate) parent: Option<ScopeId>,
}

/// One lowered lambda: the method to synthesize and, for the cached shape, the static field its
/// delegate is kept in.
#[derive(Debug, Clone)]
pub(crate) struct LoweredLambda {
    /// Which type the synthesized method belongs to.
    pub(crate) home: LambdaHome,
    /// The synthesized method's name, `<M>b__N_M` -- the enclosing method's name, its ordinal
    /// within the declaring type, and the lambda's ordinal within that method.
    pub(crate) method_name: Box<str>,
    /// The cache field's name, `<>9__N_M` -- the same two ordinals, which is what lets a reader
    /// pair a cache slot with the body it holds a delegate over.
    ///
    /// **`None` IN A STATIC CONSTRUCTOR, WHERE csc EMITS NO CACHE AT ALL.** A type's `.cctor` runs
    /// exactly once, by the runtime's guarantee, so nothing can ever evaluate the site a second
    /// time and read the slot back -- the field would be an allocation and a branch that no
    /// program can observe. Measured: a declared `static Q()` and a static field initializer both
    /// go uncached; an INSTANCE constructor and an instance field initializer both get a cache,
    /// because either can run any number of times.
    pub(crate) cache_name: Option<Box<str>>,
    /// The delegate type the site constructs.
    pub(crate) delegate_type: TypeSymbol,
    /// The synthesized method's parameters, named as the source wrote them and typed from the
    /// delegate's substituted `Invoke`.
    pub(crate) parameters: Vec<(Box<str>, TypeSymbol)>,
    /// Its return type -- `Invoke`'s.
    pub(crate) return_type: TypeSymbol,
    /// Its body, already wrapped into the block an expression body means.
    pub(crate) body: BoundStmt,
    /// The ENCLOSING method's ordinal -- the `N` in both names above. Carried so the closure
    /// type's members can be emitted in csc's order, which is the enclosing members' order and
    /// not the order emission happened to discover them in: a static constructor's body is
    /// lowered before the declared methods here and after them in csc.
    pub(crate) ordinal: usize,
}

/// What one method's lowering produced: the rewritten body, and the closure members it contributes.
#[derive(Debug)]
pub(crate) struct LambdaLowering {
    /// The enclosing method's body with every lambda site replaced.
    pub(crate) body: BoundStmt,
    /// The lambdas found, in source order.
    pub(crate) sites: Vec<LoweredLambda>,
    /// The display class to synthesize, or `None` when nothing in the method captures.
    pub(crate) display: Option<DisplayClass>,
}

/// The closure type as a symbol: the ENCLOSING type's path with `<>c` appended.
///
/// **QUALIFIED, THOUGH THE METADATA NAME IS THE SIMPLE ONE.** csc writes `<>c` as the `TypeDef`'s
/// name and lets the `NestedClass` row carry the enclosure, and so does this -- but `Tokens` keys
/// a type on its symbol's DISPLAY form, so a bare `<>c` is the same key for every closure type in
/// the assembly. The second one emitted then overwrote the first's field and method entries, and
/// the first type's bodies named the second's members: `Outer.Inner.Go()` reached
/// `S.<>c.<>9__0_0` and the runtime refused it with a `FieldAccessException`. Two closure types in
/// one assembly is all it takes, which is why nothing with a single one ever showed it.
#[must_use]
pub(crate) fn closure_symbol(enclosing: &TypeSymbol) -> TypeSymbol {
    let mut parts: Vec<Box<str>> = match enclosing {
        TypeSymbol::Named(path) => path.to_vec(),
        _ => Vec::new(),
    };
    parts.push(Box::from(CLOSURE_TYPE));
    TypeSymbol::Named(parts.into_boxed_slice())
}

/// A display class as a symbol: the ENCLOSING type's path with `<>c__DisplayClass{N}_{S}`.
///
/// Qualified for the reason [`closure_symbol`] is -- `Tokens` keys a type on its symbol's DISPLAY
/// form, so two methods of two types could otherwise produce one key. And unlike `<>c` there is
/// more than one of these per type, so the two indices are load-bearing: `N` is the enclosing
/// method's member ordinal and `S` counts the capturing scopes of that method.
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
pub(crate) fn display_class_name(ordinal: usize, scope: usize) -> alloc::string::String {
    format!("{DISPLAY_CLASS_PREFIX}{ordinal}_{scope}")
}

/// The field a nested display class holds pointing at the one enclosing it.
///
/// **NAMED FOR THE CHILD, TYPED AS THE PARENT** -- measured, and both halves are easy to get
/// backwards. `<>c__DisplayClass0_1` carries `CS$<>8__locals1` whose type is
/// `<>c__DisplayClass0_0`. The number is the CHILD's own suffix, not the parent's and not a depth.
pub(crate) const LOCALS_LINK_PREFIX: &str = "CS$<>8__locals";

/// The link field's name for a class with suffix `S`.
#[must_use]
pub(crate) fn locals_link_name(suffix: usize) -> alloc::string::String {
    format!("{LOCALS_LINK_PREFIX}{suffix}")
}

/// What the lowering needs to know about the scopes a method opened.
///
/// Narrow on purpose. The lowering needs two questions answered and a [`Binder`] answers both, but
/// a test should be able to describe a scope shape in three lines rather than bind a program to
/// produce one -- and the numbering rules this drives are exactly the kind that a program-level
/// test states imprecisely.
pub(crate) trait ScopeTree {
    /// The scope that encloses `scope`, or `None` when it is outermost.
    fn parent(&self, scope: ScopeId) -> Option<ScopeId>;

    /// `scope`, or the one id that stands for it when several are one scope to the language.
    ///
    /// A method body opens TWO -- one for the parameters and one for the body block -- and they
    /// are one scope to C#, which forbids a local shadowing a parameter, and ONE display class to
    /// csc: a captured parameter and a captured top-level local both land on `<>c__DisplayClass0_0`.
    fn canonical(&self, scope: ScopeId) -> ScopeId;
}

impl ScopeTree for lamella_binder::Binder {
    fn parent(&self, scope: ScopeId) -> Option<ScopeId> {
        self.scope_parent(scope)
    }

    fn canonical(&self, scope: ScopeId) -> ScopeId {
        let bodies = self.method_body_scope_ids();
        if bodies.iter().any(|body| *body == Some(scope)) {
            bodies.iter().flatten().min().copied().unwrap_or(scope)
        } else {
            scope
        }
    }
}

/// One display class per scope that actually captures, named and linked as csc names and links
/// them.
///
/// The suffix is a running index over the capturing scopes, counted from 0 in the order those
/// scopes open. It is neither a nesting depth nor a scope identifier: a scope that captures
/// nothing takes no number, and the enclosing method body reserves none when it captures nothing.
///
/// The chain links capturing scopes only, so a class points at the nearest ENCLOSING scope that
/// also holds a class rather than at its lexical parent.
///
/// A wrong suffix is not a build error. It is two synthesized types sharing a name, the second
/// overwriting the first, and a `FieldAccessException` at run time.
fn plan_display_classes(
    enclosing: &TypeSymbol,
    ordinal: usize,
    captures: &[Capture],
    tree: &dyn ScopeTree,
) -> Vec<DisplayClass> {
    let mut scopes: Vec<ScopeId> = Vec::new();
    for capture in captures {
        let scope = tree.canonical(capture.scope);
        if !scopes.contains(&scope) {
            scopes.push(scope);
        }
    }
    scopes.sort_unstable();

    scopes
        .iter()
        .enumerate()
        .map(|(suffix, scope)| DisplayClass {
            symbol: display_class_symbol(enclosing, ordinal, suffix),
            fields: captures
                .iter()
                .filter(|capture| tree.canonical(capture.scope) == *scope)
                .map(|capture| (capture.name.clone(), capture.ty.clone()))
                .collect(),
            hoists_this: false,
            scope: *scope,
            suffix,
            parent: {
                let mut walk = tree.parent(*scope).map(|id| tree.canonical(id));
                loop {
                    match walk {
                        Some(candidate) if scopes.contains(&candidate) => break Some(candidate),
                        Some(candidate) => walk = tree.parent(candidate).map(|id| tree.canonical(id)),
                        None => break None,
                    }
                }
            },
        })
        .collect()
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

/// Puts the display class's allocation, and the copies that seed it, at the top of the method.
///
/// csc's order, measured, and each part of it is a fact rather than tidiness:
///
/// ```text
///     newobj <>c__DisplayClassN_0::.ctor; stloc          FIRST, ahead of the opening `nop`
///     ldloc; ldarg.0; stfld <>4__this                    `this` before any local
///     ldloc; ldarg.{n}; stfld {name}                     each captured PARAMETER, at entry
/// ```
///
/// **A CAPTURED PARAMETER IS COPIED IN AND A CAPTURED LOCAL IS NOT.** A parameter's value already
/// exists when the method starts, so there is a copy to make; a local's declaration IS its store
/// and was rewritten into one where it stood. Copying a local here as well would run its
/// initializer at the wrong point in the method's flow.
///
/// The copies name the parameter as an ORDINARY local read, and are built AFTER the walk so the
/// capture rewrite cannot reach them: a captured parameter read through the very field it is
/// initializing would be a self-assignment of zero.
#[must_use]
fn prepend_prologue(
    body: BoundStmt,
    display: &DisplayClass,
    enclosing: &TypeSymbol,
    original: &BoundStmt,
    params: &[(Box<str>, TypeSymbol)],
) -> BoundStmt {
    let span = original.span;
    let instance = display_instance_read(Some(display));
    let mut prologue: Vec<BoundStmt> = Vec::new();
    prologue.push(BoundStmt {
        kind: BoundStmtKind::Local {
            ty: display.symbol.clone(),
            declarators: alloc::vec![lamella_binder::BoundDeclarator {
                name: Box::from(LOCALS_LOCAL),
                initializer: Some(BoundExpr {
                    ty: display.symbol.clone(),
                    kind: BoundExprKind::ObjectCreation {
                        constructor: Some(MethodReference {
                            declaring_type: display.symbol.clone(),
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
                }),
            }],
        },
        span,
    });
    if display.hoists_this {
        prologue.push(store(
            display,
            THIS_FIELD,
            enclosing,
            instance.clone(),
            BoundExpr {
                ty: enclosing.clone(),
                kind: BoundExprKind::This,
            },
            span,
        ));
    }
    for (name, ty) in params {
        let Some((field, field_ty)) = display
            .fields
            .iter()
            .find(|(captured, _)| captured == name)
        else {
            continue;
        };
        prologue.push(store(
            display,
            field,
            field_ty,
            instance.clone(),
            BoundExpr {
                ty: ty.clone(),
                kind: BoundExprKind::Local(name.clone()),
            },
            span,
        ));
    }
    match body.kind {
        BoundStmtKind::Block(statements) => prologue.extend(statements),
        other => prologue.push(BoundStmt { kind: other, span: body.span }),
    }
    BoundStmt {
        kind: BoundStmtKind::Block(prologue),
        span,
    }
}

/// `<instance>.<field> = <value>;`
#[must_use]
fn store(
    display: &DisplayClass,
    field: &str,
    ty: &TypeSymbol,
    instance: BoundExpr,
    value: BoundExpr,
    span: Span,
) -> BoundStmt {
    BoundStmt {
        kind: BoundStmtKind::Expression(BoundExpr {
            ty: ty.clone(),
            kind: BoundExprKind::Assignment {
                operator: lamella_syntax::ast::AssignmentOperator::Assign,
                target: Box::new(capture_access(display, field, ty, instance)),
                value: Box::new(value),
                checked: false,
            },
        }),
        span,
    }
}

/// The field a captured name lives on, once it has been hoisted.
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

/// A read of the display-class instance the enclosing method allocated -- the receiver a
/// capturing site names, mirroring [`singleton_read`] for the non-capturing one.
///
/// An ERROR node when there is no display class, which cannot happen on the path that calls this:
/// the home was chosen from the same `Option` a line earlier. It is a node rather than a panic
/// because a bad tree is a diagnostic in this emitter and never a crash.
#[must_use]
fn display_instance_read(display: Option<&DisplayClass>) -> BoundExpr {
    match display {
        Some(display) => BoundExpr {
            ty: display.symbol.clone(),
            kind: BoundExprKind::Local(Box::from(LOCALS_LOCAL)),
        },
        None => BoundExpr {
            ty: TypeSymbol::Error,
            kind: BoundExprKind::Error,
        },
    }
}

/// A lambda's body as the block it means: `return e;` for a value, the expression alone for a
/// `void` delegate, and a written block unchanged.
///
/// The same two-way choice `parse_expression_body` makes for `=> e;`, and for the same reason --
/// getting it backwards is CS0127 or CS0161. A free function because BOTH passes build it, and
/// two copies of a two-way choice is one place for a third case to be forgotten.
fn lambda_block(
    invoke: &MethodReference,
    body: &BoundLambdaBody,
    span: Span,
) -> BoundStmt {
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

/// Which of the two walks over the method body is running.
///
/// **THE SAME WALK RUNS TWICE, AND THAT IS THE POINT.** The first lambda's home depends on the
/// LAST lambda's captures -- a `this`-only lambda lands on the display class when one exists in
/// its scope and on the enclosing type when none does -- so nothing can be decided until every
/// lambda has been seen. Deciding during a single walk gives the first lambda a home the second
/// invalidates.
///
/// One walker, two modes. A second WALKER is the worse alternative: `Lowering::statement` is
/// exhaustive over every statement kind with no wildcard, deliberately, so a copy of it is a
/// second place for a new statement kind to be forgotten.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pass {
    /// Collect captures. Records nothing else, renames nothing, and returns every node unchanged.
    Survey,
    /// Replace each lambda site and record the members it contributes.
    Rewrite,
}

/// Lowers every lambda in one method body.
///
/// `ordinal` is the enclosing method's index among its declaring type's members, which is what
/// csc names the synthesized members after; see `compile::member_ordinals`.
///
/// Returns `Ok(None)` when the body holds no lambda at all -- the overwhelmingly common case, and
/// the one that must stay on the ordinary emission path rather than paying for a rewrite.
pub(crate) fn lower_lambdas(
    enclosing: &TypeSymbol,
    method_name: &str,
    ordinal: usize,
    body: &BoundStmt,
    params: &[(Box<str>, TypeSymbol)],
    tree: &dyn ScopeTree,
) -> Result<Option<LambdaLowering>, EmitError> {
    let mut state = Lowering {
        enclosing,
        method_name,
        ordinal,
        cached: method_name != ".cctor",
        pass: Pass::Survey,
        surveyed: Vec::new(),
        display: None,
        in_display_body: false,
        lambda_index: 0,
        sites: Vec::new(),
        error: None,
        span: body.span,
    };
    let _ = state.statement(body);
    if let Some(error) = state.error {
        return Err(error);
    }
    let planned = plan_display_classes(enclosing, ordinal, &state.surveyed, tree);
    state.display = planned.into_iter().next();
    state.pass = Pass::Rewrite;
    state.lambda_index = 0;
    let rewritten = state.statement(body);
    if let Some(error) = state.error {
        return Err(error);
    }
    if state.sites.is_empty() {
        return Ok(None);
    }
    let rewritten = match &state.display {
        Some(display) => prepend_prologue(rewritten, display, enclosing, body, params),
        None => rewritten,
    };
    Ok(Some(LambdaLowering {
        body: rewritten,
        sites: state.sites,
        display: state.display,
    }))
}

struct Lowering<'a> {
    /// The type whose `<>c` these sites land on -- part of the closure symbol, so two enclosing
    /// types do not share one key.
    enclosing: &'a TypeSymbol,
    /// The next lambda's index within this method.
    ///
    /// **SHARED ACROSS BOTH HOMES, WHICH IS csc's AND NOT AN ACCIDENT.** In a method holding one
    /// non-capturing lambda and one that captures `this`, csc names them `<M>b__N_0` on `<>c` and
    /// `<M>b__N_1` on the enclosing type: one counter over the method's lambdas in source order,
    /// whichever way each is lowered. Counting per home would give both index 0 and make the two
    /// names collide in the mixed case that is easiest to write.
    lambda_index: usize,
    method_name: &'a str,
    ordinal: usize,
    /// Whether a site in this method needs a cache slot; see [`LoweredLambda::cache_name`].
    cached: bool,
    /// Which walk this is; see [`Pass`].
    pass: Pass,
    /// SURVEY fills this: every capture in the method, first-seen order, deduplicated by name.
    surveyed: Vec<Capture>,
    /// REWRITE reads this: the display class the method needs, or `None` when nothing captures.
    display: Option<DisplayClass>,
    /// Whether the walk is inside a body that LANDS on the display class.
    ///
    /// It decides only the RECEIVER a captured name is read through: `this` inside such a body,
    /// which is the display-class instance, and the enclosing method's local outside it. The set
    /// of captured names is the same either way, which is why one flag is enough and a stack is
    /// not: a body on `<>c` or on the enclosing type cannot name a captured local at all -- it
    /// would have been a capture, and then the body would not be there.
    in_display_body: bool,
    sites: Vec<LoweredLambda>,
    error: Option<EmitError>,
    /// The span of the statement being rewritten. A bound EXPRESSION carries no span of its own,
    /// so a synthesized `return e;` takes the span of the statement the lambda was written in --
    /// which is the line a debugger should stop on when it steps into the body.
    span: Span,
}

impl Lowering<'_> {
    /// Replaces one lambda site, recording the members it contributes to the closure type.
    ///
    /// The lambda's ordinal is its index among the lambdas of this method, in the order this walk
    /// reaches them -- source order, because the walk is the tree's own.
    fn lambda(&mut self, expr: &BoundExpr) -> BoundExpr {
        let BoundExprKind::Lambda {
            delegate_type,
            invoke,
            parameters,
            body,
            captures,
            captures_this,
            declared_in,
        } = &expr.kind
        else {
            return expr.clone();
        };
        if self.pass == Pass::Survey {
            for capture in captures {
                if !self.surveyed.iter().any(|seen| seen.name == capture.name) {
                    self.surveyed.push(capture.clone());
                }
            }
            let block = lambda_block(invoke, body, self.span);
            let _ = self.statement(&block);
            return expr.clone();
        }
        let display_scope = self.display.as_ref().map(|display| display.scope);
        let home = if !captures.is_empty() {
            LambdaHome::DisplayClass
        } else if *captures_this && display_scope.is_some() && *declared_in == display_scope {
            LambdaHome::DisplayClass
        } else if *captures_this {
            LambdaHome::EnclosingType
        } else {
            LambdaHome::ClosureType
        };
        if home == LambdaHome::DisplayClass {
            let Some(display) = self.display.as_mut() else {
                self.error.get_or_insert(EmitError::Unsupported(
                    "a capturing lambda reached the lowering with no display class surveyed",
                ));
                return expr.clone();
            };
            display.hoists_this |= *captures_this;
        }
        let index = self.lambda_index;
        self.lambda_index += 1;
        let method_name: Box<str> = Box::from(match home {
            LambdaHome::DisplayClass => format!("<{}>b__{}", self.method_name, index),
            _ => format!("<{}>b__{}_{}", self.method_name, self.ordinal, index),
        });
        let cache_name: Option<Box<str>> = (self.cached && home == LambdaHome::ClosureType)
            .then(|| Box::from(format!("<>9__{}_{}", self.ordinal, index)));
        let block = lambda_block(invoke, body, self.span);
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
        });
        let outer_region = self.in_display_body;
        self.in_display_body = home == LambdaHome::DisplayClass;
        let block = self.statement(&block);
        self.in_display_body = outer_region;
        self.sites[index].body = block;
        let declaring = match home {
            LambdaHome::ClosureType => closure_symbol(self.enclosing),
            LambdaHome::EnclosingType => self.enclosing.clone(),
            LambdaHome::DisplayClass => match &self.display {
                Some(display) => display.symbol.clone(),
                None => self.enclosing.clone(),
            },
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
                LambdaHome::EnclosingType => BoundExpr {
                    ty: self.enclosing.clone(),
                    kind: BoundExprKind::This,
                },
                LambdaHome::DisplayClass => display_instance_read(self.display.as_ref()),
            };
            return BoundExpr {
                ty: delegate_type.clone(),
                kind: BoundExprKind::DelegateCreation {
                    delegate_type: delegate_type.clone(),
                    target,
                    receiver: Some(Box::new(receiver)),
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

    /// Rewrites one expression, replacing every lambda inside it.
    ///
    /// A replacement STOPS the walk at that node, which is what makes the nesting rule above
    /// work: `map_expr` does not recurse into a value the callback returned, so a lambda's body
    /// is reached only through [`Self::lambda`].
    fn expression(&mut self, expr: &BoundExpr) -> BoundExpr {
        crate::awaitlower::map_expr(expr, &mut |inner| {
            if matches!(inner.kind, BoundExprKind::Lambda { .. }) {
                return Some(self.lambda(inner));
            }
            self.captured_read(inner)
        })
    }

    /// A captured name -- or `this` -- as the field access it became. `None` for everything else,
    /// which leaves the walk alone.
    fn captured_read(&self, expr: &BoundExpr) -> Option<BoundExpr> {
        let display = self.display.as_ref()?;
        if matches!(expr.kind, BoundExprKind::This) {
            if !self.in_display_body || !display.hoists_this {
                return None;
            }
            return Some(capture_access(
                display,
                THIS_FIELD,
                self.enclosing,
                BoundExpr {
                    ty: display.symbol.clone(),
                    kind: BoundExprKind::This,
                },
            ));
        }
        let BoundExprKind::Local(name) = &expr.kind else {
            return None;
        };
        let (field_name, field_ty) = display
            .fields
            .iter()
            .find(|(captured, _)| captured == name)?;
        let receiver = if self.in_display_body {
            BoundExpr {
                ty: display.symbol.clone(),
                kind: BoundExprKind::This,
            }
        } else {
            display_instance_read(Some(display))
        };
        Some(capture_access(display, field_name, field_ty, receiver))
    }

    fn optional(&mut self, expr: &Option<BoundExpr>) -> Option<BoundExpr> {
        expr.as_ref().map(|inner| self.expression(inner))
    }

    fn boxed(&mut self, stmt: &BoundStmt) -> Box<BoundStmt> {
        Box::new(self.statement(stmt))
    }

    /// A local declaration, with any HOISTED declarator turned into a field store.
    ///
    /// **A CAPTURED LOCAL HAS NO SLOT AT ALL.** Its storage is the display-class field, so
    /// `int n = 3;` emits `<>8__locals.n = 3;` and declares nothing. Keeping the slot and
    /// assigning both would give the enclosing method a copy the delegate cannot see, which is
    /// the shared storage this feature is FOR.
    ///
    /// A declarator with NO initializer disappears entirely: the field is already zeroed by the
    /// allocation, and there is nothing to assign.
    ///
    /// ONE STATEMENT CAN DECLARE BOTH KINDS -- `int a = 1, b = 2;` where only `b` is captured --
    /// so this yields a BLOCK holding what is left of the declaration and the stores, in source
    /// order. A block is transparent downstream; it is not a scope in the bound tree.
    fn local_declaration(
        &mut self,
        ty: &TypeSymbol,
        declarators: &[lamella_binder::BoundDeclarator],
    ) -> BoundStmtKind {
        let hoisted: Vec<&lamella_binder::BoundDeclarator> = match &self.display {
            Some(display) => declarators
                .iter()
                .filter(|declarator| {
                    display
                        .fields
                        .iter()
                        .any(|(name, _)| *name == declarator.name)
                })
                .collect(),
            None => Vec::new(),
        };
        if hoisted.is_empty() {
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
        for declarator in declarators {
            if !hoisted.iter().any(|h| h.name == declarator.name) {
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
            }
            let Some(initializer) = &declarator.initializer else {
                continue;
            };
            let value = self.expression(initializer);
            let Some(display) = self.display.as_ref() else {
                continue;
            };
            let target = capture_access(
                display,
                &declarator.name,
                ty,
                display_instance_read(Some(display)),
            );
            statements.push(BoundStmt {
                kind: BoundStmtKind::Expression(BoundExpr {
                    ty: ty.clone(),
                    kind: BoundExprKind::Assignment {
                        operator: lamella_syntax::ast::AssignmentOperator::Assign,
                        target: Box::new(target),
                        value: Box::new(value),
                        checked: false,
                    },
                }),
                span: self.span,
            });
        }
        BoundStmtKind::Block(statements)
    }

    /// Rewrites one statement.
    ///
    /// **EXHAUSTIVE ON PURPOSE, WITH NO WILDCARD.** A statement kind added later fails to compile
    /// here rather than silently carrying an unlowered lambda past the rewrite -- which would
    /// reach the emitter as an unbuilt expression form and be reported as the wrong thing.
    fn statement(&mut self, stmt: &BoundStmt) -> BoundStmt {
        self.span = stmt.span;
        let kind = match &stmt.kind {
            BoundStmtKind::Block(statements) => {
                BoundStmtKind::Block(statements.iter().map(|s| self.statement(s)).collect())
            }
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
                body,
            } => BoundStmtKind::ForEach {
                name: name.clone(),
                element_type: element_type.clone(),
                collection: self.expression(collection),
                body: self.boxed(body),
            },
            BoundStmtKind::Break => BoundStmtKind::Break,
            BoundStmtKind::Continue => BoundStmtKind::Continue,
            BoundStmtKind::Throw(value) => BoundStmtKind::Throw(self.optional(value)),
            BoundStmtKind::Switch {
                expression,
                sections,
            } => BoundStmtKind::Switch {
                expression: self.expression(expression),
                sections: sections
                    .iter()
                    .map(|section| lamella_binder::BoundSwitchSection {
                        labels: section.labels.clone(),
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
                catches: catches
                    .iter()
                    .map(|catch| lamella_binder::BoundCatch {
                        exception_type: catch.exception_type.clone(),
                        name: catch.name.clone(),
                        filter: self.optional(&catch.filter),
                        body: self.boxed(&catch.body),
                        span: catch.span,
                    })
                    .collect(),
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
    /// The numbering rules were measured as SCOPE TREES against csc, so they are stated here as
    /// scope trees. Driving them through C# source instead would make each test a program a reader
    /// has to compile in their head, and would couple a naming rule to the binder's scope-opening
    /// choices -- which is the thing most likely to change underneath it.
    struct Shape {
        /// Child to parent. An id absent from the left column is outermost.
        parents: Vec<(usize, usize)>,
        /// The ids the method body occupies. Two of them is the normal case.
        bodies: Vec<usize>,
    }

    impl ScopeTree for Shape {
        fn parent(&self, scope: ScopeId) -> Option<ScopeId> {
            self.parents
                .iter()
                .find(|(child, _)| *child == scope.0)
                .map(|(_, parent)| ScopeId(*parent))
        }

        fn canonical(&self, scope: ScopeId) -> ScopeId {
            if self.bodies.contains(&scope.0) {
                ScopeId(*self.bodies.iter().min().expect("a body scope was matched"))
            } else {
                scope
            }
        }
    }

    fn captured(name: &str, scope: usize) -> Capture {
        Capture {
            name: Box::from(name),
            ty: TypeSymbol::Special(SpecialType::Int32),
            scope: ScopeId(scope),
        }
    }

    fn enclosing() -> TypeSymbol {
        TypeSymbol::Named(vec![Box::from("Q")].into_boxed_slice())
    }

    /// The names the planner ACTUALLY BUILT, read off the symbols.
    ///
    fn names(classes: &[DisplayClass]) -> Vec<alloc::string::String> {
        classes
            .iter()
            .map(|class| match &class.symbol {
                TypeSymbol::Named(path) => alloc::string::String::from(
                    path.last().expect("a display class symbol has a name").as_ref(),
                ),
                other => panic!("a display class must be a named type, got {other:?}"),
            })
            .collect()
    }

    /// s1 and s2, measured: the body scope does NOT reserve suffix 0.
    #[test]
    fn a_class_for_a_nested_scope_is_still_zero_when_the_body_captures_nothing() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 1)],
            bodies: vec![0, 1],
        };
        let classes = plan_display_classes(&enclosing(), 0, &[captured("b", 2)], &shape);
        assert_eq!(names(&classes), vec!["<>c__DisplayClass0_0"]);
        assert_eq!(
            classes[0].parent, None,
            "the outermost CAPTURING scope has no link, whatever encloses it lexically"
        );
    }

    /// s7, measured. The earlier round measured siblings only under a body that DID capture and
    /// reported `_1`/`_2`; taking that as the general rule names this pair one too high.
    #[test]
    fn two_siblings_under_a_non_capturing_body_are_zero_and_one() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 0), (3, 0)],
            bodies: vec![0, 1],
        };
        let classes = plan_display_classes(
            &enclosing(),
            0,
            &[captured("b", 2), captured("c", 3)],
            &shape,
        );
        assert_eq!(
            names(&classes),
            vec!["<>c__DisplayClass0_0", "<>c__DisplayClass0_1"]
        );
        assert_eq!(classes[0].parent, None);
        assert_eq!(classes[1].parent, None);
    }

    /// And the same two scopes when the body DOES capture: now they are `_1` and `_2`, and both
    /// link to the body class. One shape, two answers, decided by something outside both scopes.
    #[test]
    fn the_same_siblings_are_one_and_two_when_the_body_captures_too() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 0), (3, 0)],
            bodies: vec![0, 1],
        };
        let classes = plan_display_classes(
            &enclosing(),
            0,
            &[captured("a", 0), captured("b", 2), captured("c", 3)],
            &shape,
        );
        assert_eq!(
            names(&classes),
            vec![
                "<>c__DisplayClass0_0",
                "<>c__DisplayClass0_1",
                "<>c__DisplayClass0_2"
            ]
        );
        assert_eq!(classes[1].parent, Some(ScopeId(0)));
        assert_eq!(classes[2].parent, Some(ScopeId(0)));
    }

    /// s8, measured: a scope that captures nothing gets no class AND IS NOT A LINK. An emitter
    /// that walked one link per LEXICAL scope would emit a field csc does not have.
    #[test]
    fn a_middle_scope_that_captures_nothing_is_skipped_by_the_chain() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 1), (3, 2)],
            bodies: vec![0, 1],
        };
        let classes = plan_display_classes(
            &enclosing(),
            0,
            &[captured("a", 0), captured("c", 3)],
            &shape,
        );
        assert_eq!(classes.len(), 2, "the middle scope contributes no class");
        assert_eq!(
            classes[1].parent,
            Some(ScopeId(0)),
            "the inner class links straight to the outer, past the scope with no class"
        );
        assert_eq!(locals_link_name(classes[1].suffix), "CS$<>8__locals1");
    }

    /// The link field is named for the CHILD and typed as the PARENT, which is the pair easiest to
    /// get backwards. At depth three the numbers differ, so a mistake here cannot hide.
    #[test]
    fn a_link_field_is_named_for_its_own_class_not_the_one_it_points_at() {
        let shape = Shape {
            parents: vec![(1, 0), (2, 1), (3, 2)],
            bodies: vec![0, 1],
        };
        let classes = plan_display_classes(
            &enclosing(),
            0,
            &[captured("a", 0), captured("b", 2), captured("c", 3)],
            &shape,
        );
        assert_eq!(classes[2].suffix, 2);
        assert_eq!(locals_link_name(classes[2].suffix), "CS$<>8__locals2");
        assert_eq!(
            classes[2].parent,
            Some(ScopeId(2)),
            "and it points at the class one level out, which is suffix 1"
        );
        assert_eq!(classes[1].suffix, 1);
    }

    /// A captured PARAMETER and a captured top-level local reach the planner as two scope ids,
    /// because the binder opens one scope for parameters and another for the body block. csc gives
    /// them ONE class.
    #[test]
    fn a_captured_parameter_and_a_captured_local_share_one_class() {
        let shape = Shape {
            parents: vec![(1, 0)],
            bodies: vec![0, 1],
        };
        let classes = plan_display_classes(
            &enclosing(),
            0,
            &[captured("p", 0), captured("local", 1)],
            &shape,
        );
        assert_eq!(classes.len(), 1, "two binder scopes, one display class");
        assert_eq!(
            classes[0]
                .fields
                .iter()
                .map(|(name, _)| name.as_ref())
                .collect::<Vec<_>>(),
            vec!["p", "local"],
            "and it holds both, in first-seen order"
        );
    }

    /// Nothing captured is no class at all, rather than an empty one.
    #[test]
    fn no_captures_plans_no_class() {
        let shape = Shape {
            parents: vec![(1, 0)],
            bodies: vec![0, 1],
        };
        assert!(plan_display_classes(&enclosing(), 0, &[], &shape).is_empty());
    }
}
