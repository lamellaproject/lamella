//! The async state-machine lowering (ECMA-334 5th ed, 12.8.8 and 15.15) -- a bound-tree to
//! bound-tree rewrite, so `emit_method` and the whole instruction emitter stay untouched.

use crate::expr::EmitError;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use lamella_binder::{
    Accessibility, BoundCatch, BoundExpr, BoundExprKind, BoundStmt, BoundStmtKind, FieldReference,
    MethodReference, SpecialType, TypeSymbol,
};
use lamella_syntax::ast::{AssignmentOperator, BinaryOperator, Literal, UnaryOperator};
use lamella_syntax::token::IntegerSuffix;
use lamella_syntax::span::Span;

/// The output of lowering one async method: the machine to synthesize and the two bodies.
#[derive(Debug)]
pub struct AsyncLowering {
    /// The machine's simple name, `<M>d__N`.
    pub machine_name: Box<str>,
    /// The machine as a type symbol -- its simple name alone, which is how a nested TypeDef is
    /// named; the `NestedClass` row carries the enclosure.
    pub machine_symbol: TypeSymbol,
    /// The machine's instance fields, in declaration order.
    pub fields: Vec<(Box<str>, TypeSymbol)>,
    /// `MoveNext`'s bound body, for the ordinary `emit_bound_body` path with the machine as the
    /// enclosing type.
    pub move_next_body: BoundStmt,
    /// The source method's replacement body (construct, initialize, run once, return the task).
    pub stub_body: BoundStmt,
}

const STATE: &str = "<>1__state";
const BUILDER: &str = "<>t__builder";
const ACTION: &str = "<>t__action";
const THIS_FIELD: &str = "<>4__this";
const DONE_LABEL: &str = "<>a__done";

/// Lowers one async method. `enclosing` is the DECLARING type (for `<>4__this`), `machine_name`
/// the caller-chosen `<M>d__N`, `returns_task` distinguishes `async Task` from `async void`
/// (which builder, and whether the stub returns), and `bound_body` is the method's body exactly
/// as `bind_method` produced it.
pub fn lower_async_method(
    enclosing: &TypeSymbol,
    machine_name: &str,
    is_static: bool,
    returns_task: bool,
    parameters: &[(Box<str>, TypeSymbol)],
    bound_body: &BoundStmt,
) -> Result<AsyncLowering, EmitError> {
    let machine_symbol = TypeSymbol::Named([Box::from(machine_name)].into());
    let builder_ty = builder_symbol(returns_task);

    let mut hoisted: BTreeMap<Box<str>, TypeSymbol> = BTreeMap::new();
    for (name, ty) in parameters {
        hoisted.insert(name.clone(), ty.clone());
    }
    collect_hoisted_locals(bound_body, &mut hoisted)?;

    let mut rewriter = Rewriter {
        machine: machine_symbol.clone(),
        builder_ty: builder_ty.clone(),
        is_static,
        hoisted,
        awaiters: Vec::new(),
        temp_count: 0,
        try_count: 0,
    };
    let body = rewriter.statement(bound_body)?;

    let span = bound_body.span;
    let mut move_next: Vec<BoundStmt> = Vec::new();
    let exception_name: Box<str> = Box::from("<>a__ex");
    let mut protected: Vec<BoundStmt> = body
        .routes
        .iter()
        .map(|route| rewriter.dispatch_arm(route, span))
        .collect();
    protected.extend(body.statements);
    protected.push(stmt(BoundStmtKind::Goto(Box::from(DONE_LABEL)), span));
    let catch_body = vec![
        rewriter.set_state(-2, span),
        rewriter.builder_call(
            "SetException",
            vec![BoundExpr {
                kind: BoundExprKind::Local(exception_name.clone()),
                ty: exception_symbol(),
            }],
            span,
        ),
        stmt(BoundStmtKind::Return(None), span),
    ];
    move_next.push(stmt(
        BoundStmtKind::Try {
            body: Box::new(stmt(BoundStmtKind::Block(protected), span)),
            catches: vec![BoundCatch {
                exception_type: Some(exception_symbol()),
                name: Some(exception_name),
                body: Box::new(stmt(BoundStmtKind::Block(catch_body), span)),
                span,
            }],
            finally: None,
        },
        span,
    ));
    move_next.push(stmt(
        BoundStmtKind::Labeled {
            label: Box::from(DONE_LABEL),
            body: Box::new(stmt(BoundStmtKind::Empty, span)),
        },
        span,
    ));
    move_next.push(rewriter.set_state(-2, span));
    move_next.push(rewriter.builder_call("SetResult", Vec::new(), span));
    move_next.push(stmt(BoundStmtKind::Return(None), span));

    let mut fields: Vec<(Box<str>, TypeSymbol)> = vec![
        (Box::from(STATE), TypeSymbol::Special(SpecialType::Int32)),
        (Box::from(BUILDER), builder_ty.clone()),
        (Box::from(ACTION), action_symbol()),
    ];
    if !is_static {
        fields.push((Box::from(THIS_FIELD), enclosing.clone()));
    }
    for (name, ty) in &rewriter.hoisted {
        fields.push((name.clone(), ty.clone()));
    }
    for (index, awaiter_ty) in rewriter.awaiters.iter().enumerate() {
        fields.push((awaiter_field_name(index).into(), awaiter_ty.clone()));
    }

    let stub_body = build_stub(
        &machine_symbol,
        &builder_ty,
        enclosing,
        is_static,
        returns_task,
        parameters,
        span,
    );

    Ok(AsyncLowering {
        machine_name: Box::from(machine_name),
        machine_symbol,
        fields,
        move_next_body: stmt(BoundStmtKind::Block(move_next), span),
        stub_body,
    })
}

/// The stub that replaces the async method's own body (its signature is untouched, so overload
/// resolution, delegates over it and reflection names never see the rewrite).
#[allow(clippy::too_many_arguments)]
fn build_stub(
    machine: &TypeSymbol,
    builder_ty: &TypeSymbol,
    enclosing: &TypeSymbol,
    is_static: bool,
    returns_task: bool,
    parameters: &[(Box<str>, TypeSymbol)],
    span: Span,
) -> BoundStmt {
    let sm: Box<str> = Box::from("<>a__sm");
    let sm_local = || BoundExpr {
        kind: BoundExprKind::Local(sm.clone()),
        ty: machine.clone(),
    };
    let mut statements: Vec<BoundStmt> = Vec::new();
    statements.push(stmt(
        BoundStmtKind::Local {
            ty: machine.clone(),
            declarators: vec![lamella_binder::BoundDeclarator {
                name: sm.clone(),
                initializer: Some(BoundExpr {
                    kind: BoundExprKind::ObjectCreation {
                        arguments: Vec::new(),
                        constructor: Some(MethodReference {
                            declaring_type: machine.clone(),
                            name: Box::from(".ctor"),
                            parameters: Vec::new(),
                            return_type: TypeSymbol::Special(SpecialType::Void),
                            is_static: false,
                            is_vararg: false,
                            instantiation: None,
                            declaring_instantiation: None,
                        }),
                        initializer: None,
                    },
                    ty: machine.clone(),
                }),
            }],
        },
        span,
    ));
    if !is_static {
        let enclosing_this = BoundExpr {
            kind: BoundExprKind::This,
            ty: enclosing.clone(),
        };
        statements.push(assign(
            field_access(machine, sm_local(), THIS_FIELD, enclosing),
            enclosing_this,
            span,
        ));
    }
    for (name, ty) in parameters {
        statements.push(assign(
            field_access(machine, sm_local(), name, ty),
            BoundExpr {
                kind: BoundExprKind::Local(name.clone()),
                ty: ty.clone(),
            },
            span,
        ));
    }
    statements.push(assign(
        field_access(machine, sm_local(), BUILDER, builder_ty),
        call_static(builder_ty, "Create", builder_ty.clone()),
        span,
    ));
    statements.push(assign(
        field_access(machine, sm_local(), STATE, &TypeSymbol::Special(SpecialType::Int32)),
        int_expr(-1),
        span,
    ));
    statements.push(stmt(
        BoundStmtKind::Expression(call(
            sm_local(),
            instance_method(
                machine,
                "MoveNext",
                Vec::new(),
                TypeSymbol::Special(SpecialType::Void),
            ),
            Vec::new(),
        )),
        span,
    ));
    if returns_task {
        statements.push(stmt(
            BoundStmtKind::Return(Some(BoundExpr {
                kind: BoundExprKind::PropertyAccess {
                    receiver: Box::new(field_access(machine, sm_local(), BUILDER, builder_ty)),
                    declaring_type: builder_ty.clone(),
                    setter_declaring_type: builder_ty.clone(),
                    getter_instantiation: None,
                    setter_instantiation: None,
                    name: Box::from("Task"),
                },
                ty: task_symbol(),
            })),
            span,
        ));
    }
    stmt(BoundStmtKind::Block(statements), span)
}

fn stmt(kind: BoundStmtKind, span: Span) -> BoundStmt {
    BoundStmt { kind, span }
}

fn int_expr(value: i64) -> BoundExpr {
    let magnitude = BoundExpr {
        kind: BoundExprKind::Literal(Literal::Integer {
            value: value.unsigned_abs(),
            suffix: IntegerSuffix::None,
        }),
        ty: TypeSymbol::Special(SpecialType::Int32),
    };
    if value >= 0 {
        magnitude
    } else {
        BoundExpr {
            kind: BoundExprKind::Unary {
                operator: UnaryOperator::Minus,
                operand: Box::new(magnitude),
            },
            ty: TypeSymbol::Special(SpecialType::Int32),
        }
    }
}

fn compiler_services(name: &str) -> TypeSymbol {
    TypeSymbol::Named(
        [
            Box::from("System"),
            Box::from("Runtime"),
            Box::from("CompilerServices"),
            Box::from(name),
        ]
        .into(),
    )
}

fn builder_symbol(returns_task: bool) -> TypeSymbol {
    compiler_services(if returns_task {
        "AsyncTaskMethodBuilder"
    } else {
        "AsyncVoidMethodBuilder"
    })
}

fn task_symbol() -> TypeSymbol {
    TypeSymbol::Named(
        [
            Box::from("System"),
            Box::from("Threading"),
            Box::from("Tasks"),
            Box::from("Task"),
        ]
        .into(),
    )
}

fn action_symbol() -> TypeSymbol {
    TypeSymbol::Named([Box::from("System"), Box::from("Action")].into())
}

fn exception_symbol() -> TypeSymbol {
    TypeSymbol::Named([Box::from("System"), Box::from("Exception")].into())
}

fn awaiter_field_name(index: usize) -> String {
    format!("<>u__{index}")
}

fn resume_label(index: usize) -> String {
    format!("<>a__resume{index}")
}

fn before_try_label(id: usize) -> String {
    format!("<>a__try{id}")
}

/// A field reference on the machine, for the synthesized accesses.
fn machine_field(machine: &TypeSymbol, name: &str, ty: &TypeSymbol) -> FieldReference {
    FieldReference {
        declaring_type: machine.clone(),
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

fn field_access(
    machine: &TypeSymbol,
    receiver: BoundExpr,
    name: &str,
    ty: &TypeSymbol,
) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::FieldAccess {
            receiver: Box::new(receiver),
            name: Box::from(name),
            field: Some(machine_field(machine, name, ty)),
        },
        ty: ty.clone(),
    }
}

fn assign(target: BoundExpr, value: BoundExpr, span: Span) -> BoundStmt {
    let ty = target.ty.clone();
    stmt(
        BoundStmtKind::Expression(BoundExpr {
            kind: BoundExprKind::Assignment {
                operator: AssignmentOperator::Assign,
                target: Box::new(target),
                value: Box::new(value),
                checked: false,
            },
            ty,
        }),
        span,
    )
}

/// An instance call through a resolved [`MethodReference`], as the binder would have built it.
fn call(receiver: BoundExpr, method: MethodReference, arguments: Vec<BoundExpr>) -> BoundExpr {
    let return_type = method.return_type.clone();
    let name = method.name.clone();
    BoundExpr {
        kind: BoundExprKind::Call {
            callee: Box::new(BoundExpr {
                kind: BoundExprKind::MethodGroup {
                    receiver: Box::new(receiver),
                    name,
                },
                ty: TypeSymbol::Error,
            }),
            arguments,
            method: Some(method),
        },
        ty: return_type,
    }
}

/// A parameterless STATIC call `Declaring.Name()` returning `return_type`.
fn call_static(declaring: &TypeSymbol, name: &str, return_type: TypeSymbol) -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Call {
            callee: Box::new(BoundExpr {
                kind: BoundExprKind::MethodGroup {
                    receiver: Box::new(BoundExpr {
                        kind: BoundExprKind::TypeReference(declaring.clone()),
                        ty: declaring.clone(),
                    }),
                    name: Box::from(name),
                },
                ty: TypeSymbol::Error,
            }),
            arguments: Vec::new(),
            method: Some(MethodReference {
                declaring_type: declaring.clone(),
                name: Box::from(name),
                parameters: Vec::new(),
                return_type: return_type.clone(),
                is_static: true,
                is_vararg: false,
                instantiation: None,
                declaring_instantiation: None,
            }),
        },
        ty: return_type,
    }
}

fn instance_method(
    declaring: &TypeSymbol,
    name: &str,
    parameters: Vec<TypeSymbol>,
    return_type: TypeSymbol,
) -> MethodReference {
    MethodReference {
        declaring_type: declaring.clone(),
        name: Box::from(name),
        parameters,
        return_type,
        is_static: false,
        is_vararg: false,
        instantiation: None,
        declaring_instantiation: None,
    }
}

/// Whether any await sits anywhere under `expr`.
fn expr_contains_await(expr: &BoundExpr) -> bool {
    let mut found = false;
    visit_expr(expr, &mut |e| {
        if matches!(e.kind, BoundExprKind::Await { .. }) {
            found = true;
        }
    });
    found
}

/// Whether any await sits anywhere under `statement` (expressions included).
fn stmt_contains_await(statement: &BoundStmt) -> bool {
    let mut found = false;
    visit_stmt_exprs(statement, &mut |e| {
        if matches!(e.kind, BoundExprKind::Await { .. }) {
            found = true;
        }
    });
    found
}

/// Depth-first over an expression and every child expression.
fn visit_expr(expr: &BoundExpr, f: &mut dyn FnMut(&BoundExpr)) {
    f(expr);
    match &expr.kind {
        BoundExprKind::Ref { operand, .. }
        | BoundExprKind::Unary { operand, .. }
        | BoundExprKind::Postfix { operand, .. }
        | BoundExprKind::Cast { operand, .. }
        | BoundExprKind::Conversion { operand, .. }
        | BoundExprKind::TypeTest { operand, .. }
        | BoundExprKind::MakeRef(operand)
        | BoundExprKind::RefType(operand)
        | BoundExprKind::Checked(operand)
        | BoundExprKind::Unchecked(operand)
        | BoundExprKind::Await { operand, .. } => visit_expr(operand, f),
        BoundExprKind::FieldAccess { receiver, .. }
        | BoundExprKind::PropertyAccess { receiver, .. }
        | BoundExprKind::MethodGroup { receiver, .. } => visit_expr(receiver, f),
        BoundExprKind::Call {
            callee, arguments, ..
        } => {
            visit_expr(callee, f);
            for argument in arguments {
                visit_expr(argument, f);
            }
        }
        BoundExprKind::ElementAccess {
            receiver,
            indices: arguments,
        } => {
            visit_expr(receiver, f);
            for argument in arguments {
                visit_expr(argument, f);
            }
        }
        BoundExprKind::IndexerAccess {
            receiver, indices, ..
        } => {
            visit_expr(receiver, f);
            for index in indices {
                visit_expr(index, f);
            }
        }
        BoundExprKind::ArrayCreation { lengths, elements } => {
            for length in lengths {
                visit_expr(length, f);
            }
            for element in elements {
                visit_expr(element, f);
            }
        }
        BoundExprKind::ObjectCreation { arguments, .. } => {
            for argument in arguments {
                visit_expr(argument, f);
            }
        }
        BoundExprKind::DelegateCreation { receiver, .. } => {
            if let Some(receiver) = receiver {
                visit_expr(receiver, f);
            }
        }
        BoundExprKind::Binary { left, right, .. } => {
            visit_expr(left, f);
            visit_expr(right, f);
        }
        BoundExprKind::Assignment { target, value, .. } => {
            visit_expr(target, f);
            visit_expr(value, f);
        }
        BoundExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => {
            visit_expr(condition, f);
            visit_expr(when_true, f);
            visit_expr(when_false, f);
        }
        BoundExprKind::NullCoalescing { left, right } => {
            visit_expr(left, f);
            visit_expr(right, f);
        }
        _ => {}
    }
}

/// Every expression a statement holds, statements recursed.
fn visit_stmt_exprs(statement: &BoundStmt, f: &mut dyn FnMut(&BoundExpr)) {
    match &statement.kind {
        BoundStmtKind::Block(statements) => {
            for inner in statements {
                visit_stmt_exprs(inner, f);
            }
        }
        BoundStmtKind::Local { declarators, .. } => {
            for declarator in declarators {
                if let Some(initializer) = &declarator.initializer {
                    visit_expr(initializer, f);
                }
            }
        }
        BoundStmtKind::Expression(expr) | BoundStmtKind::Throw(Some(expr)) => visit_expr(expr, f),
        BoundStmtKind::Return(Some(expr)) => visit_expr(expr, f),
        BoundStmtKind::If {
            condition,
            then_branch,
            else_branch,
        } => {
            visit_expr(condition, f);
            visit_stmt_exprs(then_branch, f);
            if let Some(else_branch) = else_branch {
                visit_stmt_exprs(else_branch, f);
            }
        }
        BoundStmtKind::While { condition, body } | BoundStmtKind::DoWhile { body, condition } => {
            visit_expr(condition, f);
            visit_stmt_exprs(body, f);
        }
        BoundStmtKind::For {
            initializer,
            condition,
            iterators,
            body,
        } => {
            for inner in initializer {
                visit_stmt_exprs(inner, f);
            }
            if let Some(condition) = condition {
                visit_expr(condition, f);
            }
            for iterator in iterators {
                visit_expr(iterator, f);
            }
            visit_stmt_exprs(body, f);
        }
        BoundStmtKind::ForEach {
            collection, body, ..
        } => {
            visit_expr(collection, f);
            visit_stmt_exprs(body, f);
        }
        BoundStmtKind::Switch {
            expression,
            sections,
        } => {
            visit_expr(expression, f);
            for section in sections {
                for inner in &section.statements {
                    visit_stmt_exprs(inner, f);
                }
            }
        }
        BoundStmtKind::Try {
            body,
            catches,
            finally,
        } => {
            visit_stmt_exprs(body, f);
            for catch in catches {
                visit_stmt_exprs(&catch.body, f);
            }
            if let Some(finally) = finally {
                visit_stmt_exprs(finally, f);
            }
        }
        BoundStmtKind::Lock { expression, body } => {
            visit_expr(expression, f);
            visit_stmt_exprs(body, f);
        }
        BoundStmtKind::Using { resource, body } => {
            for inner in resource {
                visit_stmt_exprs(inner, f);
            }
            visit_stmt_exprs(body, f);
        }
        BoundStmtKind::Fixed { init, body, .. } => {
            visit_expr(init, f);
            visit_stmt_exprs(body, f);
        }
        BoundStmtKind::Checked(body)
        | BoundStmtKind::Unchecked(body)
        | BoundStmtKind::Labeled { body, .. } => visit_stmt_exprs(body, f),
        _ => {}
    }
}

/// Walks the body collecting every `Local` declaration into the hoisted map, refusing the one
/// shape a shared field cannot carry: the same name at two different types.
fn collect_hoisted_locals(
    body: &BoundStmt,
    hoisted: &mut BTreeMap<Box<str>, TypeSymbol>,
) -> Result<(), EmitError> {
    let mut error = None;
    visit_locals(body, &mut |ty, name| {
        if let Some(existing) = hoisted.get(name) {
            if existing != ty && error.is_none() {
                error = Some(EmitError::Unsupported(
                    "two same-named locals of different types in an async method are not lowered \
                     yet (they cannot share a hoisted field)",
                ));
            }
        } else {
            hoisted.insert(Box::from(&**name), ty.clone());
        }
    });
    match error {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Every `Local` declaration under `body`, in order.
fn visit_locals(body: &BoundStmt, f: &mut dyn FnMut(&TypeSymbol, &Box<str>)) {
    match &body.kind {
        BoundStmtKind::Local { ty, declarators } => {
            for declarator in declarators {
                f(ty, &declarator.name);
            }
        }
        BoundStmtKind::Block(statements) => {
            for inner in statements {
                visit_locals(inner, f);
            }
        }
        BoundStmtKind::If {
            then_branch,
            else_branch,
            ..
        } => {
            visit_locals(then_branch, f);
            if let Some(else_branch) = else_branch {
                visit_locals(else_branch, f);
            }
        }
        BoundStmtKind::While { body, .. }
        | BoundStmtKind::DoWhile { body, .. }
        | BoundStmtKind::ForEach { body, .. }
        | BoundStmtKind::Lock { body, .. }
        | BoundStmtKind::Fixed { body, .. }
        | BoundStmtKind::Checked(body)
        | BoundStmtKind::Unchecked(body)
        | BoundStmtKind::Labeled { body, .. } => visit_locals(body, f),
        BoundStmtKind::For {
            initializer, body, ..
        } => {
            for inner in initializer {
                visit_locals(inner, f);
            }
            visit_locals(body, f);
        }
        BoundStmtKind::Using { resource, body } => {
            for inner in resource {
                visit_locals(inner, f);
            }
            visit_locals(body, f);
        }
        BoundStmtKind::Try {
            body,
            catches,
            finally,
        } => {
            visit_locals(body, f);
            for catch in catches {
                visit_locals(&catch.body, f);
            }
            if let Some(finally) = finally {
                visit_locals(finally, f);
            }
        }
        BoundStmtKind::Switch { sections, .. } => {
            for section in sections {
                for inner in &section.statements {
                    visit_locals(inner, f);
                }
            }
        }
        _ => {}
    }
}

/// One await site's route from an enclosing region's dispatch: the state value and the label
/// THIS level branches to for it -- the resume label itself, or the label before a nested `try`
/// (fall in; its own dispatch continues).
#[derive(Clone)]
struct AwaitRoute {
    index: usize,
    target: String,
}

/// A rewritten region: its statements, plus the routes an ENCLOSING dispatch needs for every
/// await suspended inside them.
struct Rewritten {
    statements: Vec<BoundStmt>,
    routes: Vec<AwaitRoute>,
}

struct Rewriter {
    machine: TypeSymbol,
    builder_ty: TypeSymbol,
    is_static: bool,
    /// Name -> type of every field-backed variable (parameters, user locals, spill temps).
    hoisted: BTreeMap<Box<str>, TypeSymbol>,
    /// One entry per await site, in state order: that site's awaiter type.
    awaiters: Vec<TypeSymbol>,
    temp_count: usize,
    try_count: usize,
}

impl Rewriter {
    fn machine_this(&self) -> BoundExpr {
        BoundExpr {
            kind: BoundExprKind::This,
            ty: self.machine.clone(),
        }
    }

    fn state_field(&self) -> BoundExpr {
        field_access(
            &self.machine,
            self.machine_this(),
            STATE,
            &TypeSymbol::Special(SpecialType::Int32),
        )
    }

    fn set_state(&self, value: i64, span: Span) -> BoundStmt {
        assign(self.state_field(), int_expr(value), span)
    }

    fn builder_call(&self, name: &str, arguments: Vec<BoundExpr>, span: Span) -> BoundStmt {
        let parameter_types: Vec<TypeSymbol> =
            arguments.iter().map(|argument| argument.ty.clone()).collect();
        let receiver = field_access(&self.machine, self.machine_this(), BUILDER, &self.builder_ty);
        stmt(
            BoundStmtKind::Expression(call(
                receiver,
                instance_method(
                    &self.builder_ty,
                    name,
                    parameter_types,
                    TypeSymbol::Special(SpecialType::Void),
                ),
                arguments,
            )),
            span,
        )
    }

    /// `if (<>1__state == index) goto target;`
    fn dispatch_arm(&self, route: &AwaitRoute, span: Span) -> BoundStmt {
        stmt(
            BoundStmtKind::If {
                condition: BoundExpr {
                    kind: BoundExprKind::Binary {
                        operator: BinaryOperator::Equal,
                        left: Box::new(self.state_field()),
                        right: Box::new(int_expr(route.index as i64)),
                        checked: false,
                    },
                    ty: TypeSymbol::Special(SpecialType::Boolean),
                },
                then_branch: Box::new(stmt(
                    BoundStmtKind::Goto(Box::from(&*route.target)),
                    span,
                )),
                else_branch: None,
            },
            span,
        )
    }

    fn fresh_temp(&mut self, ty: &TypeSymbol) -> Box<str> {
        let name: Box<str> = format!("<>s__{}", self.temp_count).into();
        self.temp_count += 1;
        self.hoisted.insert(name.clone(), ty.clone());
        name
    }

    fn temp_field(&self, name: &Box<str>) -> BoundExpr {
        let ty = self.hoisted.get(name).cloned().unwrap_or(TypeSymbol::Error);
        field_access(&self.machine, self.machine_this(), name, &ty)
    }

    /// Rewrites one statement (the entry point for the whole body too).
    fn statement(&mut self, statement: &BoundStmt) -> Result<Rewritten, EmitError> {
        let span = statement.span;
        let mut out = Rewritten {
            statements: Vec::new(),
            routes: Vec::new(),
        };
        match &statement.kind {
            BoundStmtKind::Block(statements) => {
                for inner in statements {
                    let mut rewritten = self.statement(inner)?;
                    out.statements.append(&mut rewritten.statements);
                    out.routes.append(&mut rewritten.routes);
                }
            }
            BoundStmtKind::Local { declarators, .. } => {
                for declarator in declarators {
                    if let Some(initializer) = &declarator.initializer {
                        let value =
                            self.expression(initializer, &mut out.statements, &mut out.routes)?;
                        let target = self.temp_field(&declarator.name);
                        out.statements.push(assign(target, value, span));
                    }
                }
            }
            BoundStmtKind::Expression(expr) => {
                if let BoundExprKind::Await { .. } = &expr.kind {
                    self.expand_await_statement(expr, &mut out.statements, &mut out.routes, span)?;
                } else {
                    let rewritten =
                        self.expression(expr, &mut out.statements, &mut out.routes)?;
                    out.statements.push(stmt(BoundStmtKind::Expression(rewritten), span));
                }
            }
            BoundStmtKind::Return(None) => {
                out.statements
                    .push(stmt(BoundStmtKind::Goto(Box::from(DONE_LABEL)), span));
            }
            BoundStmtKind::Return(Some(_)) => {
                return Err(EmitError::Unsupported(
                    "a value-returning return in an async method survived binding",
                ));
            }
            BoundStmtKind::If {
                condition,
                then_branch,
                else_branch,
            } => {
                let condition =
                    self.expression(condition, &mut out.statements, &mut out.routes)?;
                let mut then_branch = self.statement(then_branch)?;
                out.routes.append(&mut then_branch.routes);
                let else_rewritten = match else_branch {
                    Some(else_branch) => {
                        let mut rewritten = self.statement(else_branch)?;
                        out.routes.append(&mut rewritten.routes);
                        Some(Box::new(stmt(
                            BoundStmtKind::Block(rewritten.statements),
                            span,
                        )))
                    }
                    None => None,
                };
                out.statements.push(stmt(
                    BoundStmtKind::If {
                        condition,
                        then_branch: Box::new(stmt(
                            BoundStmtKind::Block(then_branch.statements),
                            span,
                        )),
                        else_branch: else_rewritten,
                    },
                    span,
                ));
            }
            BoundStmtKind::While { condition, body } => {
                if expr_contains_await(condition) {
                    let mut head: Vec<BoundStmt> = Vec::new();
                    let condition =
                        self.expression(condition, &mut head, &mut out.routes)?;
                    head.push(stmt(
                        BoundStmtKind::If {
                            condition: BoundExpr {
                                kind: BoundExprKind::Unary {
                                    operator: UnaryOperator::Not,
                                    operand: Box::new(condition),
                                },
                                ty: TypeSymbol::Special(SpecialType::Boolean),
                            },
                            then_branch: Box::new(stmt(BoundStmtKind::Break, span)),
                            else_branch: None,
                        },
                        span,
                    ));
                    let mut body = self.statement(body)?;
                    out.routes.append(&mut body.routes);
                    head.append(&mut body.statements);
                    out.statements.push(stmt(
                        BoundStmtKind::While {
                            condition: BoundExpr {
                                kind: BoundExprKind::Literal(Literal::Boolean(true)),
                                ty: TypeSymbol::Special(SpecialType::Boolean),
                            },
                            body: Box::new(stmt(BoundStmtKind::Block(head), span)),
                        },
                        span,
                    ));
                } else {
                    let condition = self.map_variables(condition)?;
                    let mut body = self.statement(body)?;
                    out.routes.append(&mut body.routes);
                    out.statements.push(stmt(
                        BoundStmtKind::While {
                            condition,
                            body: Box::new(stmt(BoundStmtKind::Block(body.statements), span)),
                        },
                        span,
                    ));
                }
            }
            BoundStmtKind::DoWhile { body, condition } => {
                if expr_contains_await(condition) {
                    return Err(EmitError::Unsupported(
                        "an await in a do-while condition is not lowered yet",
                    ));
                }
                let condition = self.map_variables(condition)?;
                let mut body = self.statement(body)?;
                out.routes.append(&mut body.routes);
                out.statements.push(stmt(
                    BoundStmtKind::DoWhile {
                        body: Box::new(stmt(BoundStmtKind::Block(body.statements), span)),
                        condition,
                    },
                    span,
                ));
            }
            BoundStmtKind::For {
                initializer,
                condition,
                iterators,
                body,
            } => {
                if condition.as_ref().is_some_and(expr_contains_await)
                    || iterators.iter().any(expr_contains_await)
                {
                    return Err(EmitError::Unsupported(
                        "an await in a for condition or iterator is not lowered yet",
                    ));
                }
                let mut rewritten_init: Vec<BoundStmt> = Vec::new();
                for inner in initializer {
                    let mut rewritten = self.statement(inner)?;
                    rewritten_init.append(&mut rewritten.statements);
                    out.routes.append(&mut rewritten.routes);
                }
                out.statements.append(&mut rewritten_init);
                let condition = match condition {
                    Some(condition) => Some(self.map_variables(condition)?),
                    None => None,
                };
                let iterators = iterators
                    .iter()
                    .map(|iterator| self.map_variables(iterator))
                    .collect::<Result<Vec<_>, _>>()?;
                let mut body = self.statement(body)?;
                out.routes.append(&mut body.routes);
                out.statements.push(stmt(
                    BoundStmtKind::For {
                        initializer: Vec::new(),
                        condition,
                        iterators,
                        body: Box::new(stmt(BoundStmtKind::Block(body.statements), span)),
                    },
                    span,
                ));
            }
            BoundStmtKind::ForEach {
                name,
                element_type,
                collection,
                body,
            } => {
                if stmt_contains_await(body) {
                    return Err(EmitError::Unsupported(
                        "an await inside a foreach body is not lowered yet (the enumerator lives \
                         in an IL slot that would die at the suspension)",
                    ));
                }
                if self.hoisted.contains_key(name) {
                    return Err(EmitError::Unsupported(
                        "a foreach variable sharing a hoisted local's name in an async method is \
                         not lowered yet",
                    ));
                }
                let collection =
                    self.expression(collection, &mut out.statements, &mut out.routes)?;
                let mut body = self.statement(body)?;
                out.routes.append(&mut body.routes);
                out.statements.push(stmt(
                    BoundStmtKind::ForEach {
                        name: name.clone(),
                        element_type: element_type.clone(),
                        collection,
                        body: Box::new(stmt(BoundStmtKind::Block(body.statements), span)),
                    },
                    span,
                ));
            }
            BoundStmtKind::Try {
                body,
                catches,
                finally,
            } => {
                let try_id = self.try_count;
                self.try_count += 1;
                let mut inner = self.statement(body)?;
                let mut protected: Vec<BoundStmt> = inner
                    .routes
                    .iter()
                    .map(|route| self.dispatch_arm(route, span))
                    .collect();
                protected.append(&mut inner.statements);
                let mut rewritten_catches: Vec<BoundCatch> = Vec::new();
                for catch in catches {
                    if stmt_contains_await(&catch.body) {
                        return Err(EmitError::Unsupported(
                            "an await inside a catch clause survived binding",
                        ));
                    }
                    if catch
                        .name
                        .as_ref()
                        .is_some_and(|name| self.hoisted.contains_key(name))
                    {
                        return Err(EmitError::Unsupported(
                            "a catch variable sharing a hoisted local's name in an async method \
                             is not lowered yet",
                        ));
                    }
                    let mut rewritten = self.statement(&catch.body)?;
                    out.routes.append(&mut rewritten.routes);
                    rewritten_catches.push(BoundCatch {
                        exception_type: catch.exception_type.clone(),
                        name: catch.name.clone(),
                        body: Box::new(stmt(BoundStmtKind::Block(rewritten.statements), span)),
                        span: catch.span,
                    });
                }
                let rewritten_finally = match finally {
                    Some(finally) => {
                        if stmt_contains_await(finally) {
                            return Err(EmitError::Unsupported(
                                "an await inside a finally clause survived binding",
                            ));
                        }
                        let mut rewritten = self.statement(finally)?;
                        out.routes.append(&mut rewritten.routes);
                        Some(Box::new(stmt(
                            BoundStmtKind::If {
                                condition: BoundExpr {
                                    kind: BoundExprKind::Binary {
                                        operator: BinaryOperator::LessThan,
                                        left: Box::new(self.state_field()),
                                        right: Box::new(int_expr(0)),
                                        checked: false,
                                    },
                                    ty: TypeSymbol::Special(SpecialType::Boolean),
                                },
                                then_branch: Box::new(stmt(
                                    BoundStmtKind::Block(rewritten.statements),
                                    span,
                                )),
                                else_branch: None,
                            },
                            span,
                        )))
                    }
                    None => None,
                };
                let had_inner_awaits = !inner.routes.is_empty();
                if had_inner_awaits {
                    let label = before_try_label(try_id);
                    out.statements.push(stmt(
                        BoundStmtKind::Labeled {
                            label: Box::from(&*label),
                            body: Box::new(stmt(BoundStmtKind::Empty, span)),
                        },
                        span,
                    ));
                    for route in &inner.routes {
                        out.routes.push(AwaitRoute {
                            index: route.index,
                            target: label.clone(),
                        });
                    }
                }
                out.statements.push(stmt(
                    BoundStmtKind::Try {
                        body: Box::new(stmt(BoundStmtKind::Block(protected), span)),
                        catches: rewritten_catches,
                        finally: rewritten_finally,
                    },
                    span,
                ));
            }
            BoundStmtKind::Switch {
                expression,
                sections,
            } => {
                let expression =
                    self.expression(expression, &mut out.statements, &mut out.routes)?;
                let mut rewritten_sections = Vec::new();
                for section in sections {
                    let mut statements = Vec::new();
                    for inner in &section.statements {
                        let mut rewritten = self.statement(inner)?;
                        statements.append(&mut rewritten.statements);
                        out.routes.append(&mut rewritten.routes);
                    }
                    rewritten_sections.push(lamella_binder::BoundSwitchSection {
                        labels: section.labels.clone(),
                        statements,
                    });
                }
                out.statements.push(stmt(
                    BoundStmtKind::Switch {
                        expression,
                        sections: rewritten_sections,
                    },
                    span,
                ));
            }
            BoundStmtKind::Throw(Some(expr)) => {
                let rewritten = self.expression(expr, &mut out.statements, &mut out.routes)?;
                out.statements
                    .push(stmt(BoundStmtKind::Throw(Some(rewritten)), span));
            }
            BoundStmtKind::Labeled { label, body } => {
                let mut body = self.statement(body)?;
                out.routes.append(&mut body.routes);
                out.statements.push(stmt(
                    BoundStmtKind::Labeled {
                        label: label.clone(),
                        body: Box::new(stmt(BoundStmtKind::Block(body.statements), span)),
                    },
                    span,
                ));
            }
            BoundStmtKind::Checked(body) => {
                let mut body = self.statement(body)?;
                out.routes.append(&mut body.routes);
                out.statements.push(stmt(
                    BoundStmtKind::Checked(Box::new(stmt(
                        BoundStmtKind::Block(body.statements),
                        span,
                    ))),
                    span,
                ));
            }
            BoundStmtKind::Unchecked(body) => {
                let mut body = self.statement(body)?;
                out.routes.append(&mut body.routes);
                out.statements.push(stmt(
                    BoundStmtKind::Unchecked(Box::new(stmt(
                        BoundStmtKind::Block(body.statements),
                        span,
                    ))),
                    span,
                ));
            }
            BoundStmtKind::Using { resource, body } => {
                let mut rewritten_resource: Vec<BoundStmt> = Vec::new();
                for inner in resource {
                    let mut rewritten = self.statement(inner)?;
                    rewritten_resource.append(&mut rewritten.statements);
                    out.routes.append(&mut rewritten.routes);
                }
                let mut body = self.statement(body)?;
                out.routes.append(&mut body.routes);
                out.statements.push(stmt(
                    BoundStmtKind::Using {
                        resource: rewritten_resource,
                        body: Box::new(stmt(BoundStmtKind::Block(body.statements), span)),
                    },
                    span,
                ));
            }
            BoundStmtKind::Lock { expression, body } => {
                let expression =
                    self.expression(expression, &mut out.statements, &mut out.routes)?;
                let mut body = self.statement(body)?;
                out.routes.append(&mut body.routes);
                out.statements.push(stmt(
                    BoundStmtKind::Lock {
                        expression,
                        body: Box::new(stmt(BoundStmtKind::Block(body.statements), span)),
                    },
                    span,
                ));
            }
            BoundStmtKind::Fixed {
                name,
                element,
                init,
                body,
            } => {
                if stmt_contains_await(body) {
                    return Err(EmitError::Unsupported(
                        "an await inside a fixed body is not lowered yet (the pin lives in an IL \
                         slot that would die at the suspension)",
                    ));
                }
                let init = self.expression(init, &mut out.statements, &mut out.routes)?;
                let mut body = self.statement(body)?;
                out.routes.append(&mut body.routes);
                out.statements.push(stmt(
                    BoundStmtKind::Fixed {
                        name: name.clone(),
                        element: element.clone(),
                        init,
                        body: Box::new(stmt(BoundStmtKind::Block(body.statements), span)),
                    },
                    span,
                ));
            }
            BoundStmtKind::Empty
            | BoundStmtKind::Break
            | BoundStmtKind::Continue
            | BoundStmtKind::Throw(None)
            | BoundStmtKind::Goto(_)
            | BoundStmtKind::GotoCase(_)
            | BoundStmtKind::GotoCaseString(_)
            | BoundStmtKind::GotoDefault
            | BoundStmtKind::Error => {
                out.statements.push(statement.clone());
            }
        }
        Ok(out)
    }

    /// Rewrites an expression, expanding awaits into `pre` and mapping every hoisted variable to
    /// its machine field. Order is preserved by SPILLING: when a later sibling contains an
    /// await, every earlier sibling's value is materialized into a temp field first.
    fn expression(
        &mut self,
        expr: &BoundExpr,
        pre: &mut Vec<BoundStmt>,
        routes: &mut Vec<AwaitRoute>,
    ) -> Result<BoundExpr, EmitError> {
        let ty = expr.ty.clone();
        let span = Span::empty_at(0);
        let rewritten = match &expr.kind {
            BoundExprKind::Await { .. } => {
                let result = self.expand_await_value(expr, pre, routes)?;
                return Ok(result);
            }
            BoundExprKind::Local(name) => {
                if self.hoisted.contains_key(name) {
                    let field_ty = self.hoisted.get(name).cloned().unwrap_or(TypeSymbol::Error);
                    field_access(&self.machine, self.machine_this(), name, &field_ty)
                } else {
                    expr.clone()
                }
            }
            BoundExprKind::This => {
                if self.is_static {
                    return Err(EmitError::Unsupported(
                        "`this` in a static async method survived binding",
                    ));
                }
                let enclosing_ty = ty.clone();
                field_access(&self.machine, self.machine_this(), THIS_FIELD, &enclosing_ty)
            }
            BoundExprKind::Base => {
                return Err(EmitError::Unsupported(
                    "a base access in an async method is not lowered yet",
                ));
            }
            _ if !expr_contains_await(expr) => {
                self.map_variables(expr)?
            }
            BoundExprKind::Call {
                callee,
                arguments,
                method,
            } => {
                let (receiver, name) = match &callee.kind {
                    BoundExprKind::MethodGroup { receiver, name } => (receiver, name.clone()),
                    _ => {
                        return Err(EmitError::Unsupported(
                            "an await under this call shape is not lowered yet",
                        ))
                    }
                };
                if arguments.iter().any(|argument| {
                    matches!(argument.kind, BoundExprKind::Ref { .. })
                }) {
                    return Err(EmitError::Unsupported(
                        "a call mixing ref/out arguments with an await is not lowered yet (an \
                         address cannot be spilled across a suspension)",
                    ));
                }
                let receiver = if expr_contains_await(receiver) {
                    return Err(EmitError::Unsupported(
                        "an await inside a call receiver is not lowered yet",
                    ));
                } else if arguments.iter().any(expr_contains_await)
                    && !matches!(
                        receiver.kind,
                        BoundExprKind::TypeReference(_) | BoundExprKind::NamespaceReference(_)
                    )
                {
                    let mapped = self.map_variables(receiver)?;
                    self.spill(mapped, pre, span)?
                } else {
                    self.map_variables(receiver)?
                };
                let arguments = self.rewrite_arguments(arguments, pre, routes, span)?;
                BoundExpr {
                    kind: BoundExprKind::Call {
                        callee: Box::new(BoundExpr {
                            kind: BoundExprKind::MethodGroup {
                                receiver: Box::new(receiver),
                                name,
                            },
                            ty: callee.ty.clone(),
                        }),
                        arguments,
                        method: method.clone(),
                    },
                    ty,
                }
            }
            BoundExprKind::ObjectCreation {
                arguments,
                constructor,
                initializer,
            } => {
                if initializer.is_some() {
                    return Err(EmitError::Unsupported(
                        "an await inside an object initializer is not lowered yet",
                    ));
                }
                if arguments.iter().any(|argument| {
                    matches!(argument.kind, BoundExprKind::Ref { .. })
                }) {
                    return Err(EmitError::Unsupported(
                        "a constructor call mixing ref/out arguments with an await is not \
                         lowered yet",
                    ));
                }
                let arguments = self.rewrite_arguments(arguments, pre, routes, span)?;
                BoundExpr {
                    kind: BoundExprKind::ObjectCreation {
                        arguments,
                        constructor: constructor.clone(),
                        initializer: None,
                    },
                    ty,
                }
            }
            BoundExprKind::Binary {
                operator,
                left,
                right,
                checked,
            } => {
                if matches!(operator, BinaryOperator::LogicalAnd | BinaryOperator::LogicalOr) {
                    return Err(EmitError::Unsupported(
                        "an await under && or || is not lowered yet (the short-circuit branch \
                         rewrite is not built)",
                    ));
                }
                let left = if expr_contains_await(right) {
                    let left = self.expression(left, pre, routes)?;
                    self.spill(left, pre, span)?
                } else {
                    self.expression(left, pre, routes)?
                };
                let right = self.expression(right, pre, routes)?;
                BoundExpr {
                    kind: BoundExprKind::Binary {
                        operator: *operator,
                        left: Box::new(left),
                        right: Box::new(right),
                        checked: *checked,
                    },
                    ty,
                }
            }
            BoundExprKind::Conditional { .. } => {
                return Err(EmitError::Unsupported(
                    "an await under a conditional (?:) is not lowered yet (the branch rewrite is \
                     not built)",
                ));
            }
            BoundExprKind::NullCoalescing { .. } => {
                return Err(EmitError::Unsupported(
                    "an await under a null-coalescing operator (??) is not lowered yet (the \
                     branch rewrite is not built)",
                ));
            }
            BoundExprKind::Assignment {
                operator,
                target,
                value,
                checked,
            } => {
                if *operator != AssignmentOperator::Assign {
                    return Err(EmitError::Unsupported(
                        "an await on the right of a compound assignment is not lowered yet",
                    ));
                }
                let target_ok = match &target.kind {
                    BoundExprKind::Local(_) => true,
                    BoundExprKind::FieldAccess { receiver, .. } => {
                        matches!(receiver.kind, BoundExprKind::This)
                    }
                    _ => false,
                };
                if !target_ok {
                    return Err(EmitError::Unsupported(
                        "an await on the right of this assignment target is not lowered yet",
                    ));
                }
                let value = self.expression(value, pre, routes)?;
                let target = self.expression(target, pre, routes)?;
                BoundExpr {
                    kind: BoundExprKind::Assignment {
                        operator: *operator,
                        target: Box::new(target),
                        value: Box::new(value),
                        checked: *checked,
                    },
                    ty,
                }
            }
            BoundExprKind::Conversion {
                operand,
                conversion,
            } => {
                let operand = self.expression(operand, pre, routes)?;
                BoundExpr {
                    kind: BoundExprKind::Conversion {
                        operand: Box::new(operand),
                        conversion: conversion.clone(),
                    },
                    ty,
                }
            }
            BoundExprKind::Cast { operand, checked } => {
                let operand = self.expression(operand, pre, routes)?;
                BoundExpr {
                    kind: BoundExprKind::Cast {
                        operand: Box::new(operand),
                        checked: *checked,
                    },
                    ty,
                }
            }
            BoundExprKind::Unary { operator, operand } => {
                let operand = self.expression(operand, pre, routes)?;
                BoundExpr {
                    kind: BoundExprKind::Unary {
                        operator: *operator,
                        operand: Box::new(operand),
                    },
                    ty,
                }
            }
            BoundExprKind::ElementAccess { receiver, indices } => {
                let receiver = if indices.iter().any(expr_contains_await) {
                    let receiver = self.expression(receiver, pre, routes)?;
                    self.spill(receiver, pre, span)?
                } else {
                    self.expression(receiver, pre, routes)?
                };
                let indices = self.rewrite_arguments(indices, pre, routes, span)?;
                BoundExpr {
                    kind: BoundExprKind::ElementAccess {
                        receiver: Box::new(receiver),
                        indices,
                    },
                    ty,
                }
            }
            BoundExprKind::ArrayCreation { lengths, elements } => {
                let lengths = self.rewrite_arguments(lengths, pre, routes, span)?;
                let elements = self.rewrite_arguments(elements, pre, routes, span)?;
                BoundExpr {
                    kind: BoundExprKind::ArrayCreation { lengths, elements },
                    ty,
                }
            }
            _ => {
                return Err(EmitError::Unsupported(
                    "an await under this expression form is not lowered yet",
                ));
            }
        };
        Ok(rewritten)
    }

    /// Rewrites an argument list with order-preserving spills: an argument BEFORE one that
    /// contains an await is materialized into a temp field first.
    fn rewrite_arguments(
        &mut self,
        arguments: &[BoundExpr],
        pre: &mut Vec<BoundStmt>,
        routes: &mut Vec<AwaitRoute>,
        span: Span,
    ) -> Result<Vec<BoundExpr>, EmitError> {
        let mut rewritten = Vec::with_capacity(arguments.len());
        for (index, argument) in arguments.iter().enumerate() {
            let later_awaits = arguments[index + 1..].iter().any(expr_contains_await);
            let value = self.expression(argument, pre, routes)?;
            rewritten.push(if later_awaits {
                self.spill(value, pre, span)?
            } else {
                value
            });
        }
        Ok(rewritten)
    }

    /// Materializes `value` into a fresh hoisted temp, returning the field read. A literal is
    /// order-immune and passes through unspilled.
    fn spill(
        &mut self,
        value: BoundExpr,
        pre: &mut Vec<BoundStmt>,
        span: Span,
    ) -> Result<BoundExpr, EmitError> {
        if matches!(value.kind, BoundExprKind::Literal(_)) {
            return Ok(value);
        }
        let ty = value.ty.clone();
        if ty.is_void() || ty.is_error() {
            return Err(EmitError::Unsupported(
                "a valueless operand beside an await is not lowered yet",
            ));
        }
        let name = self.fresh_temp(&ty);
        let target = self.temp_field(&name);
        pre.push(assign(target, value, span));
        Ok(self.temp_field(&name))
    }

    /// The uniform await sequence, producing the GetResult VALUE into a fresh temp field whose
    /// read is returned.
    fn expand_await_value(
        &mut self,
        await_expr: &BoundExpr,
        pre: &mut Vec<BoundStmt>,
        routes: &mut Vec<AwaitRoute>,
    ) -> Result<BoundExpr, EmitError> {
        let result_ty = await_expr.ty.clone();
        let get_result_call = self.expand_await_core(await_expr, pre, routes)?;
        if result_ty.is_void() {
            return Err(EmitError::Unsupported(
                "a void await in value position survived binding",
            ));
        }
        let span = Span::empty_at(0);
        let name = self.fresh_temp(&result_ty);
        pre.push(assign(self.temp_field(&name), get_result_call, span));
        let clear = self.clear_awaiter_statement(routes);
        pre.push(clear);
        Ok(self.temp_field(&name))
    }

    /// A statement-position await: the resume path calls GetResult and discards its value.
    fn expand_await_statement(
        &mut self,
        await_expr: &BoundExpr,
        out: &mut Vec<BoundStmt>,
        routes: &mut Vec<AwaitRoute>,
        span: Span,
    ) -> Result<(), EmitError> {
        let get_result_call = self.expand_await_core(await_expr, out, routes)?;
        out.push(stmt(BoundStmtKind::Expression(get_result_call), span));
        let clear = self.clear_awaiter_statement(routes);
        out.push(clear);
        Ok(())
    }

    /// `this.<>u__k = default(TAwaiter);` for the site just expanded (the last route pushed).
    fn clear_awaiter_statement(&mut self, routes: &[AwaitRoute]) -> BoundStmt {
        let index = routes.last().map(|route| route.index).unwrap_or(0);
        let awaiter_ty = self.awaiters[index].clone();
        let span = Span::empty_at(0);
        assign(
            field_access(
                &self.machine,
                self.machine_this(),
                &awaiter_field_name(index),
                &awaiter_ty,
            ),
            BoundExpr {
                kind: BoundExprKind::DefaultValue(awaiter_ty.clone()),
                ty: awaiter_ty,
            },
            span,
        )
    }

    /// The shared suspend/resume sequence: everything up to and including the resume label and
    /// the state reset, returning the (not-yet-emitted) `GetResult()` call for the caller to
    /// place -- assigned to a temp in value position, discarded in statement position.
    fn expand_await_core(
        &mut self,
        await_expr: &BoundExpr,
        pre: &mut Vec<BoundStmt>,
        routes: &mut Vec<AwaitRoute>,
    ) -> Result<BoundExpr, EmitError> {
        let BoundExprKind::Await {
            operand,
            get_awaiter,
            is_completed,
            on_completed,
            get_result,
        } = &await_expr.kind
        else {
            return Err(EmitError::Unsupported("expand_await_core on a non-await"));
        };
        let span = Span::empty_at(0);
        let index = self.awaiters.len();
        let awaiter_ty = get_awaiter.return_type.clone();
        self.awaiters.push(awaiter_ty.clone());
        let machine = self.machine.clone();
        let machine_this = self.machine_this();
        let awaiter_field = || {
            field_access(
                &machine,
                machine_this.clone(),
                &awaiter_field_name(index),
                &awaiter_ty,
            )
        };

        let operand = self.expression(operand, pre, routes)?;
        pre.push(assign(
            awaiter_field(),
            call(operand, get_awaiter.clone(), Vec::new()),
            span,
        ));

        let is_completed_read = BoundExpr {
            kind: BoundExprKind::PropertyAccess {
                receiver: Box::new(awaiter_field()),
                declaring_type: is_completed.declaring_type.clone(),
                setter_declaring_type: is_completed.declaring_type.clone(),
                getter_instantiation: None,
                setter_instantiation: None,
                name: Box::from("IsCompleted"),
            },
            ty: TypeSymbol::Special(SpecialType::Boolean),
        };
        let action_field = || field_access(&self.machine, self.machine_this(), ACTION, &action_symbol());
        let ensure_action = stmt(
            BoundStmtKind::If {
                condition: BoundExpr {
                    kind: BoundExprKind::Binary {
                        operator: BinaryOperator::Equal,
                        left: Box::new(action_field()),
                        right: Box::new(BoundExpr {
                            kind: BoundExprKind::Literal(Literal::Null),
                            ty: TypeSymbol::Special(SpecialType::Null),
                        }),
                        checked: false,
                    },
                    ty: TypeSymbol::Special(SpecialType::Boolean),
                },
                then_branch: Box::new(assign(
                    action_field(),
                    BoundExpr {
                        kind: BoundExprKind::DelegateCreation {
                            delegate_type: action_symbol(),
                            target: instance_method(
                                &self.machine,
                                "MoveNext",
                                Vec::new(),
                                TypeSymbol::Special(SpecialType::Void),
                            ),
                            receiver: Some(Box::new(self.machine_this())),
                        },
                        ty: action_symbol(),
                    },
                    span,
                )),
                else_branch: None,
            },
            span,
        );
        let register = stmt(
            BoundStmtKind::Expression(call(
                awaiter_field(),
                on_completed.clone(),
                vec![action_field()],
            )),
            span,
        );
        pre.push(stmt(
            BoundStmtKind::If {
                condition: BoundExpr {
                    kind: BoundExprKind::Unary {
                        operator: UnaryOperator::Not,
                        operand: Box::new(is_completed_read),
                    },
                    ty: TypeSymbol::Special(SpecialType::Boolean),
                },
                then_branch: Box::new(stmt(
                    BoundStmtKind::Block(vec![
                        self.set_state(index as i64, span),
                        ensure_action,
                        register,
                        stmt(BoundStmtKind::Return(None), span),
                    ]),
                    span,
                )),
                else_branch: None,
            },
            span,
        ));

        pre.push(stmt(
            BoundStmtKind::Labeled {
                label: Box::from(&*resume_label(index)),
                body: Box::new(stmt(BoundStmtKind::Empty, span)),
            },
            span,
        ));
        pre.push(self.set_state(-1, span));
        routes.push(AwaitRoute {
            index,
            target: resume_label(index),
        });

        Ok(call(awaiter_field(), get_result.clone(), Vec::new()))
    }

    /// Maps every `Local`/`This` leaf of an await-free expression to its machine field,
    /// rebuilding nothing else. `Base` refuses -- the machine has no base to reach through.
    fn map_variables(&mut self, expr: &BoundExpr) -> Result<BoundExpr, EmitError> {
        let mut error: Option<EmitError> = None;
        let rewritten = map_expr(expr, &mut |leaf| match &leaf.kind {
            BoundExprKind::Local(name) if self.hoisted.contains_key(name) => {
                let ty = self.hoisted.get(name).cloned().unwrap_or(TypeSymbol::Error);
                Some(field_access(&self.machine, self.machine_this(), name, &ty))
            }
            BoundExprKind::This if !self.is_static => Some(field_access(
                &self.machine,
                self.machine_this(),
                THIS_FIELD,
                &leaf.ty,
            )),
            BoundExprKind::Base => {
                if error.is_none() {
                    error = Some(EmitError::Unsupported(
                        "a base access in an async method is not lowered yet",
                    ));
                }
                None
            }
            _ => None,
        });
        match error {
            Some(error) => Err(error),
            None => Ok(rewritten),
        }
    }
}

/// Structural map over an expression: `replace` answers `Some` at a leaf it substitutes, `None`
/// to keep walking. Children rebuild around the substitutions.
fn map_expr(expr: &BoundExpr, replace: &mut dyn FnMut(&BoundExpr) -> Option<BoundExpr>) -> BoundExpr {
    if let Some(replacement) = replace(expr) {
        return replacement;
    }
    let ty = expr.ty.clone();
    let kind = match &expr.kind {
        BoundExprKind::Ref { out, operand } => BoundExprKind::Ref {
            out: *out,
            operand: Box::new(map_expr(operand, replace)),
        },
        BoundExprKind::Unary { operator, operand } => BoundExprKind::Unary {
            operator: *operator,
            operand: Box::new(map_expr(operand, replace)),
        },
        BoundExprKind::Postfix {
            operator,
            operand,
            step,
        } => BoundExprKind::Postfix {
            operator: *operator,
            operand: Box::new(map_expr(operand, replace)),
            step: step.clone(),
        },
        BoundExprKind::Cast { operand, checked } => BoundExprKind::Cast {
            operand: Box::new(map_expr(operand, replace)),
            checked: *checked,
        },
        BoundExprKind::Conversion {
            operand,
            conversion,
        } => BoundExprKind::Conversion {
            operand: Box::new(map_expr(operand, replace)),
            conversion: conversion.clone(),
        },
        BoundExprKind::TypeTest {
            operation,
            operand,
            target,
        } => BoundExprKind::TypeTest {
            operation: *operation,
            operand: Box::new(map_expr(operand, replace)),
            target: target.clone(),
        },
        BoundExprKind::FieldAccess {
            receiver,
            name,
            field,
        } => BoundExprKind::FieldAccess {
            receiver: Box::new(map_expr(receiver, replace)),
            name: name.clone(),
            field: field.clone(),
        },
        BoundExprKind::PropertyAccess {
            receiver,
            declaring_type,
            setter_declaring_type,
            getter_instantiation,
            setter_instantiation,
            name,
        } => BoundExprKind::PropertyAccess {
            receiver: Box::new(map_expr(receiver, replace)),
            declaring_type: declaring_type.clone(),
            setter_declaring_type: setter_declaring_type.clone(),
            getter_instantiation: getter_instantiation.clone(),
            setter_instantiation: setter_instantiation.clone(),
            name: name.clone(),
        },
        BoundExprKind::MethodGroup { receiver, name } => BoundExprKind::MethodGroup {
            receiver: Box::new(map_expr(receiver, replace)),
            name: name.clone(),
        },
        BoundExprKind::Call {
            callee,
            arguments,
            method,
        } => BoundExprKind::Call {
            callee: Box::new(map_expr(callee, replace)),
            arguments: arguments
                .iter()
                .map(|argument| map_expr(argument, replace))
                .collect(),
            method: method.clone(),
        },
        BoundExprKind::ElementAccess { receiver, indices } => BoundExprKind::ElementAccess {
            receiver: Box::new(map_expr(receiver, replace)),
            indices: indices.iter().map(|index| map_expr(index, replace)).collect(),
        },
        BoundExprKind::IndexerAccess {
            receiver,
            indices,
            setter,
        } => BoundExprKind::IndexerAccess {
            receiver: Box::new(map_expr(receiver, replace)),
            indices: indices.iter().map(|index| map_expr(index, replace)).collect(),
            setter: setter.clone(),
        },
        BoundExprKind::ArrayCreation { lengths, elements } => BoundExprKind::ArrayCreation {
            lengths: lengths.iter().map(|length| map_expr(length, replace)).collect(),
            elements: elements
                .iter()
                .map(|element| map_expr(element, replace))
                .collect(),
        },
        BoundExprKind::ObjectCreation {
            arguments,
            constructor,
            initializer,
        } => BoundExprKind::ObjectCreation {
            arguments: arguments
                .iter()
                .map(|argument| map_expr(argument, replace))
                .collect(),
            constructor: constructor.clone(),
            initializer: initializer.clone(),
        },
        BoundExprKind::DelegateCreation {
            delegate_type,
            target,
            receiver,
        } => BoundExprKind::DelegateCreation {
            delegate_type: delegate_type.clone(),
            target: target.clone(),
            receiver: receiver
                .as_ref()
                .map(|receiver| Box::new(map_expr(receiver, replace))),
        },
        BoundExprKind::Binary {
            operator,
            left,
            right,
            checked,
        } => BoundExprKind::Binary {
            operator: *operator,
            left: Box::new(map_expr(left, replace)),
            right: Box::new(map_expr(right, replace)),
            checked: *checked,
        },
        BoundExprKind::Assignment {
            operator,
            target,
            value,
            checked,
        } => BoundExprKind::Assignment {
            operator: *operator,
            target: Box::new(map_expr(target, replace)),
            value: Box::new(map_expr(value, replace)),
            checked: *checked,
        },
        BoundExprKind::Conditional {
            condition,
            when_true,
            when_false,
        } => BoundExprKind::Conditional {
            condition: Box::new(map_expr(condition, replace)),
            when_true: Box::new(map_expr(when_true, replace)),
            when_false: Box::new(map_expr(when_false, replace)),
        },
        BoundExprKind::NullCoalescing { left, right } => BoundExprKind::NullCoalescing {
            left: Box::new(map_expr(left, replace)),
            right: Box::new(map_expr(right, replace)),
        },
        BoundExprKind::Checked(operand) => {
            BoundExprKind::Checked(Box::new(map_expr(operand, replace)))
        }
        BoundExprKind::Unchecked(operand) => {
            BoundExprKind::Unchecked(Box::new(map_expr(operand, replace)))
        }
        other => other.clone(),
    };
    BoundExpr { kind, ty }
}
