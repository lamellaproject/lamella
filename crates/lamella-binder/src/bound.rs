//! The bound expression tree and the expression binder (ECMA-334 1st ed,
//! clause 14).

use crate::bind::bind_type;
use crate::conversion::{can_cast, converts};
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::resolve::{TypeTable, resolve_type};
use crate::special::SpecialType;
use crate::symbols::{MethodSymbol, Model, TypeInfo};
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_syntax::ast::{
    AssignmentOperator, BinaryOperator, Expr, ExprKind, Literal, PostfixOperator, TypeRef,
    TypeTestOperation, UnaryOperator,
};
use lamella_syntax::span::Span;
use lamella_syntax::token::{IntegerSuffix, RealSuffix};

/// A bound expression: its kind and its resolved type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundExpr {
    /// What the expression is, after binding.
    pub kind: BoundExprKind,
    /// The expression's type (`TypeSymbol::Error` when binding failed).
    pub ty: TypeSymbol,
}

/// The kind of a [`BoundExpr`]. Grows as the binder learns more expression forms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundExprKind {
    /// A constant literal, retyped from the syntax (9.4.4).
    Literal(Literal),
    /// A reference to a local variable or parameter (14.5.2).
    Local(Box<str>),
    /// The `this` access (14.5.7); its type is the enclosing type.
    This,
    /// A `base` access (14.5.8); its type is the enclosing type's base class, used
    /// as the receiver of a non-virtual `base.member`.
    Base,
    /// A type name used as the receiver of a static member access (14.5.4). Its
    /// type is the named type so member lookup reaches the type's members.
    TypeReference(TypeSymbol),
    /// Access to an instance or static field through a receiver (14.5.4); the
    /// expression's type is the field's.
    FieldAccess {
        /// The receiver the field is read from.
        receiver: Box<BoundExpr>,
        /// The field name.
        name: Box<str>,
    },
    /// Access to a property through a receiver (14.5.4); the expression's type is
    /// the property's.
    PropertyAccess {
        /// The receiver the property is read from.
        receiver: Box<BoundExpr>,
        /// The property name.
        name: Box<str>,
    },
    /// A method group named through a receiver (14.5.4) -- not a value on its own,
    /// only the target of an invocation, so its type is the error type.
    MethodGroup {
        /// The receiver the method is called on.
        receiver: Box<BoundExpr>,
        /// The method name.
        name: Box<str>,
    },
    /// A method call (14.5.5); its type is the chosen overload's return type.
    Call {
        /// The callee (a method group).
        callee: Box<BoundExpr>,
        /// The bound arguments, in order.
        arguments: Vec<BoundExpr>,
    },
    /// An element access `receiver[indices]` (14.5.6); its type is the array's
    /// element type.
    ElementAccess {
        /// The indexed receiver.
        receiver: Box<BoundExpr>,
        /// The index arguments.
        indices: Vec<BoundExpr>,
    },
    /// An array creation `new T[...]` (14.5.10.2); its type is the array type.
    ArrayCreation {
        /// The dimension-length expressions.
        lengths: Vec<BoundExpr>,
    },
    /// An object creation `new T(args)` (14.5.10.1); its type is the created type.
    ObjectCreation {
        /// The constructor arguments.
        arguments: Vec<BoundExpr>,
    },
    /// A binary operation on two bound operands (14.7-14.12).
    Binary {
        /// The operator.
        operator: BinaryOperator,
        /// The left operand.
        left: Box<BoundExpr>,
        /// The right operand.
        right: Box<BoundExpr>,
    },
    /// A prefix unary operation (14.6).
    Unary {
        /// The operator.
        operator: UnaryOperator,
        /// The operand.
        operand: Box<BoundExpr>,
    },
    /// A postfix increment or decrement (14.5.9).
    Postfix {
        /// The operator.
        operator: PostfixOperator,
        /// The operand.
        operand: Box<BoundExpr>,
    },
    /// A cast to the expression's type (14.6.6).
    Cast {
        /// The operand being cast.
        operand: Box<BoundExpr>,
    },
    /// An `is`/`as` type test (14.9.9, 14.9.10); the tested type is the result
    /// type for `as` and `bool` for `is`.
    TypeTest {
        /// Whether this is `is` or `as`.
        operation: TypeTestOperation,
        /// The operand.
        operand: Box<BoundExpr>,
    },
    /// An assignment, simple or compound (14.14); its type is the target's.
    Assignment {
        /// The assignment operator.
        operator: AssignmentOperator,
        /// The assignment target (an lvalue).
        target: Box<BoundExpr>,
        /// The assigned value.
        value: Box<BoundExpr>,
    },
    /// A conditional expression `c ? a : b` (14.13).
    Conditional {
        /// The condition.
        condition: Box<BoundExpr>,
        /// The value when true.
        when_true: Box<BoundExpr>,
        /// The value when false.
        when_false: Box<BoundExpr>,
    },
    /// A `typeof` expression (14.5.11); its type is `System.Type`.
    TypeOf,
    /// A `checked` expression (14.5.12); the type is the operand's.
    Checked(Box<BoundExpr>),
    /// An `unchecked` expression (14.5.12); the type is the operand's.
    Unchecked(Box<BoundExpr>),
    /// An expression that could not be bound (yet), for recovery.
    Error,
}

/// The method currently being bound: its name (for `CS0127`) and declared return
/// type (for checking `return`).
#[derive(Debug, Clone)]
struct MethodContext {
    name: Box<str>,
    return_type: TypeSymbol,
}

/// Binds expressions, accumulating the semantic diagnostics found. Holds a stack
/// of local-variable scopes for name resolution.
#[derive(Debug, Default)]
pub struct Binder {
    diagnostics: Vec<Diagnostic>,
    scopes: Vec<BTreeMap<String, TypeSymbol>>,
    world: TypeTable,
    model: Model,
    current_type: Option<TypeSymbol>,
    current_method: Option<MethodContext>,
}

impl Binder {
    /// A fresh binder with an empty reference world.
    #[must_use]
    pub fn new() -> Binder {
        Binder::default()
    }

    /// A binder that resolves named types against `world` (existence only; member
    /// lookup needs [`Binder::with_model`]).
    #[must_use]
    pub fn with_world(world: TypeTable) -> Binder {
        Binder {
            world,
            ..Binder::default()
        }
    }

    /// A binder that resolves type names and looks members up against `model`.
    #[must_use]
    pub fn with_model(model: Model) -> Binder {
        Binder {
            world: model.type_table(),
            model,
            ..Binder::default()
        }
    }

    /// Records a diagnostic.
    pub(crate) fn report(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    /// Resolves a type against the reference world, reporting `CS0246` if unknown.
    pub(crate) fn resolve_named_type(&mut self, ty: &TypeSymbol, span: Span) -> TypeSymbol {
        resolve_type(&self.world, ty, &mut self.diagnostics, span)
    }

    /// Whether `from` implicitly converts to `to`, including reference conversions
    /// that walk the model's inheritance graph (13.1).
    pub(crate) fn converts(&self, from: &TypeSymbol, to: &TypeSymbol) -> bool {
        converts(&self.model, from, to)
    }

    /// The diagnostics gathered so far.
    #[must_use]
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Consumes the binder, returning its diagnostics.
    #[must_use]
    pub fn into_diagnostics(self) -> Vec<Diagnostic> {
        self.diagnostics
    }

    /// Sets the enclosing type whose members an unqualified name and `this`
    /// resolve against, for binding that type's method bodies.
    pub fn enter_type(&mut self, ty: TypeSymbol) {
        self.current_type = Some(ty);
    }

    /// Clears the enclosing type.
    pub fn exit_type(&mut self) {
        self.current_type = None;
    }

    /// Binds a method body end to end: the enclosing type is in scope for `this`
    /// and unqualified names, the parameters are declared as locals, and `return`
    /// statements are checked against `return_type` (15.9.4). Returns the bound
    /// body.
    pub fn bind_method(
        &mut self,
        enclosing_type: Option<TypeSymbol>,
        name: &str,
        return_type: TypeSymbol,
        parameters: &[(Box<str>, TypeSymbol)],
        body: &lamella_syntax::ast::Stmt,
    ) -> crate::statement::BoundStmt {
        let returns_value = !return_type.is_void();
        let body_span = body.span;
        self.current_type = enclosing_type;
        self.current_method = Some(MethodContext {
            name: name.into(),
            return_type,
        });
        self.enter_scope();
        for (parameter, ty) in parameters {
            self.declare_local(parameter, ty.clone());
        }
        let bound = self.bind_statement(body);
        self.exit_scope();
        if returns_value && !crate::flow::always_exits(&bound) {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::NotAllPathsReturn {
                    method: name.into(),
                },
                body_span,
            ));
        }
        let parameter_names: Vec<Box<str>> = parameters
            .iter()
            .map(|(parameter, _)| parameter.clone())
            .collect();
        self.diagnostics
            .extend(crate::flow::check_definite_assignment(
                &bound,
                &parameter_names,
            ));
        self.current_method = None;
        self.current_type = None;
        bound
    }

    /// Checks a `return` statement against the enclosing method's return type
    /// (15.9.4): `CS0127` for a value in a `void` method, `CS0126` for a missing
    /// value, `CS0029` for a value that does not convert.
    pub(crate) fn check_return(&mut self, value: Option<&BoundExpr>, span: Span) {
        let Some(method) = self.current_method.clone() else {
            return;
        };
        if method.return_type.is_void() {
            if value.is_some_and(|expr| !expr.ty.is_error()) {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ReturnValueInVoidMethod {
                        method: method.name,
                    },
                    span,
                ));
            }
        } else {
            match value {
                None => self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ReturnValueRequired {
                        ty: method.return_type.to_string().into(),
                    },
                    span,
                )),
                Some(expr)
                    if !expr.ty.is_error() && !self.converts(&expr.ty, &method.return_type) =>
                {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::NoImplicitConversion {
                            from: expr.ty.to_string().into(),
                            to: method.return_type.to_string().into(),
                        },
                        span,
                    ));
                }
                _ => {}
            }
        }
    }

    /// Opens a nested scope (a block or method body).
    pub fn enter_scope(&mut self) {
        self.scopes.push(BTreeMap::new());
    }

    /// Closes the innermost scope.
    pub fn exit_scope(&mut self) {
        self.scopes.pop();
    }

    /// Declares a local variable or parameter in the innermost scope.
    pub fn declare_local(&mut self, name: &str, ty: TypeSymbol) {
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.into(), ty);
        }
    }

    /// Looks a name up through the scope stack, innermost first.
    fn lookup_local(&self, name: &str) -> Option<&TypeSymbol> {
        self.scopes.iter().rev().find_map(|scope| scope.get(name))
    }

    /// Binds an expression (14).
    pub fn bind_expression(&mut self, expr: &Expr) -> BoundExpr {
        match &expr.kind {
            ExprKind::Literal(literal) => BoundExpr {
                kind: BoundExprKind::Literal(literal.clone()),
                ty: literal_type(literal),
            },
            ExprKind::Name(name) => self.bind_name(name, expr.span),
            ExprKind::This => self.this_expr(),
            ExprKind::Base => self.base_expr(),
            ExprKind::MemberAccess { receiver, name } => {
                self.bind_member_access(receiver, name, expr.span)
            }
            ExprKind::Invocation {
                receiver,
                arguments,
            } => self.bind_invocation(receiver, arguments, expr.span),
            ExprKind::ElementAccess {
                receiver,
                arguments,
            } => self.bind_element_access(receiver, arguments, expr.span),
            ExprKind::ObjectCreation { target, arguments } => {
                self.bind_object_creation(target, arguments, expr.span)
            }
            ExprKind::ArrayCreation {
                element,
                lengths,
                rank,
                extra_ranks,
                ..
            } => {
                let lengths = lengths
                    .iter()
                    .map(|length| self.bind_expression(length))
                    .collect();
                let mut ty = self.resolve_named_type(&bind_type(element), element.span);
                if !ty.is_error() {
                    for &extra in extra_ranks.iter().rev() {
                        ty = ty.into_array(extra);
                    }
                    ty = ty.into_array(*rank);
                }
                BoundExpr {
                    kind: BoundExprKind::ArrayCreation { lengths },
                    ty,
                }
            }
            ExprKind::Binary {
                operator,
                left,
                right,
            } => self.bind_binary(*operator, left, right, expr.span),
            ExprKind::Unary { operator, operand } => self.bind_unary(*operator, operand, expr.span),
            ExprKind::PostfixUnary { operator, operand } => {
                self.bind_postfix(*operator, operand, expr.span)
            }
            ExprKind::Cast { target, operand } => {
                let operand = self.bind_expression(operand);
                let ty = self.resolve_named_type(&bind_type(target), target.span);
                if !operand.ty.is_error()
                    && !ty.is_error()
                    && !can_cast(&self.model, &operand.ty, &ty)
                {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::CannotCast {
                            from: operand.ty.to_string().into(),
                            to: ty.to_string().into(),
                        },
                        target.span,
                    ));
                }
                BoundExpr {
                    kind: BoundExprKind::Cast {
                        operand: Box::new(operand),
                    },
                    ty,
                }
            }
            ExprKind::TypeTest {
                operation,
                operand,
                target,
            } => {
                let operand = self.bind_expression(operand);
                let target = self.resolve_named_type(&bind_type(target), target.span);
                let ty = match operation {
                    TypeTestOperation::Is => TypeSymbol::Special(SpecialType::Boolean),
                    TypeTestOperation::As => target,
                };
                BoundExpr {
                    kind: BoundExprKind::TypeTest {
                        operation: *operation,
                        operand: Box::new(operand),
                    },
                    ty,
                }
            }
            ExprKind::TypeOf(target) => {
                let _ = self.resolve_named_type(&bind_type(target), target.span);
                BoundExpr {
                    kind: BoundExprKind::TypeOf,
                    ty: system_type(),
                }
            }
            ExprKind::Checked(inner) => {
                let inner = self.bind_expression(inner);
                let ty = inner.ty.clone();
                BoundExpr {
                    kind: BoundExprKind::Checked(Box::new(inner)),
                    ty,
                }
            }
            ExprKind::Unchecked(inner) => {
                let inner = self.bind_expression(inner);
                let ty = inner.ty.clone();
                BoundExpr {
                    kind: BoundExprKind::Unchecked(Box::new(inner)),
                    ty,
                }
            }
            ExprKind::Conditional {
                condition,
                when_true,
                when_false,
            } => self.bind_conditional(condition, when_true, when_false),
            ExprKind::Assignment {
                operator,
                target,
                value,
            } => self.bind_assignment(*operator, target, value, expr.span),
            ExprKind::Parenthesized(inner) => self.bind_expression(inner),
            _ => BoundExpr {
                kind: BoundExprKind::Error,
                ty: TypeSymbol::Error,
            },
        }
    }

    fn bind_binary(
        &mut self,
        operator: BinaryOperator,
        left_expr: &Expr,
        right_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let left = self.bind_expression(left_expr);
        let right = self.bind_expression(right_expr);
        let ty = if left.ty.is_error() || right.ty.is_error() {
            TypeSymbol::Error
        } else if let Some(result) = binary_result_type(operator, &left.ty, &right.ty) {
            result
        } else {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::OperatorNotApplicable {
                    operator: operator_symbol(operator).into(),
                    left: left.ty.to_string().into(),
                    right: right.ty.to_string().into(),
                },
                span,
            ));
            TypeSymbol::Error
        };
        BoundExpr {
            kind: BoundExprKind::Binary {
                operator,
                left: Box::new(left),
                right: Box::new(right),
            },
            ty,
        }
    }

    fn bind_unary(
        &mut self,
        operator: UnaryOperator,
        operand_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let operand = self.bind_expression(operand_expr);
        let ty = if operand.ty.is_error() {
            TypeSymbol::Error
        } else if let Some(result) = unary_result_type(operator, &operand.ty) {
            result
        } else {
            self.report_unary(unary_operator_symbol(operator), &operand.ty, span);
            TypeSymbol::Error
        };
        BoundExpr {
            kind: BoundExprKind::Unary {
                operator,
                operand: Box::new(operand),
            },
            ty,
        }
    }

    fn bind_postfix(
        &mut self,
        operator: PostfixOperator,
        operand_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let operand = self.bind_expression(operand_expr);
        let ty = if operand.ty.is_error() {
            TypeSymbol::Error
        } else if as_special(&operand.ty).is_some_and(SpecialType::is_numeric) {
            operand.ty.clone()
        } else {
            let symbol = match operator {
                PostfixOperator::Increment => "++",
                PostfixOperator::Decrement => "--",
            };
            self.report_unary(symbol, &operand.ty, span);
            TypeSymbol::Error
        };
        BoundExpr {
            kind: BoundExprKind::Postfix {
                operator,
                operand: Box::new(operand),
            },
            ty,
        }
    }

    fn report_unary(&mut self, operator: &str, operand: &TypeSymbol, span: Span) {
        self.diagnostics.push(Diagnostic::new(
            DiagnosticKind::UnaryOperatorNotApplicable {
                operator: operator.into(),
                operand: operand.to_string().into(),
            },
            span,
        ));
    }

    fn bind_conditional(
        &mut self,
        condition: &Expr,
        when_true: &Expr,
        when_false: &Expr,
    ) -> BoundExpr {
        let condition_span = condition.span;
        let condition = self.bind_expression(condition);
        let boolean = TypeSymbol::Special(SpecialType::Boolean);
        if !condition.ty.is_error() && !self.converts(&condition.ty, &boolean) {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::NoImplicitConversion {
                    from: condition.ty.to_string().into(),
                    to: "bool".into(),
                },
                condition_span,
            ));
        }
        let span = when_false.span;
        let when_true = self.bind_expression(when_true);
        let when_false = self.bind_expression(when_false);
        let ty = if when_true.ty.is_error() || when_false.ty.is_error() {
            TypeSymbol::Error
        } else if let Some(common) =
            conditional_result_type(&self.model, &when_true.ty, &when_false.ty)
        {
            common
        } else {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::ConditionalTypeMismatch {
                    left: when_true.ty.to_string().into(),
                    right: when_false.ty.to_string().into(),
                },
                span,
            ));
            TypeSymbol::Error
        };
        BoundExpr {
            kind: BoundExprKind::Conditional {
                condition: Box::new(condition),
                when_true: Box::new(when_true),
                when_false: Box::new(when_false),
            },
            ty,
        }
    }

    fn bind_assignment(
        &mut self,
        operator: AssignmentOperator,
        target_expr: &Expr,
        value_expr: &Expr,
        span: Span,
    ) -> BoundExpr {
        let target_span = target_expr.span;
        let target = self.bind_expression(target_expr);
        let value = self.bind_expression(value_expr);
        if !target.ty.is_error() && !is_lvalue(&target) {
            self.diagnostics
                .push(Diagnostic::new(DiagnosticKind::NotAssignable, target_span));
        } else if !target.ty.is_error() && !value.ty.is_error() {
            self.check_assignment(operator, &target.ty, &value.ty, span);
        }
        let ty = target.ty.clone();
        BoundExpr {
            kind: BoundExprKind::Assignment {
                operator,
                target: Box::new(target),
                value: Box::new(value),
            },
            ty,
        }
    }

    fn check_assignment(
        &mut self,
        operator: AssignmentOperator,
        target: &TypeSymbol,
        value: &TypeSymbol,
        span: Span,
    ) {
        match compound_binary_operator(operator) {
            None => {
                if !self.converts(value, target) {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::NoImplicitConversion {
                            from: value.to_string().into(),
                            to: target.to_string().into(),
                        },
                        span,
                    ));
                }
            }
            Some(binary) => {
                if binary_result_type(binary, target, value).is_none() {
                    self.diagnostics.push(Diagnostic::new(
                        DiagnosticKind::OperatorNotApplicable {
                            operator: assignment_symbol(operator).into(),
                            left: target.to_string().into(),
                            right: value.to_string().into(),
                        },
                        span,
                    ));
                }
            }
        }
    }

    fn bind_member_access(&mut self, receiver_expr: &Expr, name: &str, span: Span) -> BoundExpr {
        let receiver = self.bind_expression(receiver_expr);
        if receiver.ty.is_error() {
            return error_expr();
        }
        match self.resolve_member(&receiver.ty, name) {
            MemberResolution::Field(ty) => BoundExpr {
                kind: BoundExprKind::FieldAccess {
                    receiver: Box::new(receiver),
                    name: name.into(),
                },
                ty,
            },
            MemberResolution::Property(ty) => BoundExpr {
                kind: BoundExprKind::PropertyAccess {
                    receiver: Box::new(receiver),
                    name: name.into(),
                },
                ty,
            },
            MemberResolution::MethodGroup => BoundExpr {
                kind: BoundExprKind::MethodGroup {
                    receiver: Box::new(receiver),
                    name: name.into(),
                },
                ty: TypeSymbol::Error,
            },
            MemberResolution::NoSuchMember(type_name) => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::MemberNotFound {
                        type_name: type_name.into(),
                        member: name.into(),
                    },
                    span,
                ));
                error_expr()
            }
            MemberResolution::Unknown => error_expr(),
        }
    }

    fn bind_invocation(
        &mut self,
        receiver_expr: &Expr,
        argument_exprs: &[Expr],
        span: Span,
    ) -> BoundExpr {
        let callee = self.bind_expression(receiver_expr);
        let arguments: Vec<BoundExpr> = argument_exprs
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        let group = match &callee.kind {
            BoundExprKind::MethodGroup { receiver, name } => {
                Some((receiver.ty.clone(), name.clone()))
            }
            _ => None,
        };
        let ty = match group {
            Some((receiver_ty, name))
                if !arguments.iter().any(|argument| argument.ty.is_error()) =>
            {
                let candidates = self.methods_in_chain(&receiver_ty, &name);
                let argument_types: Vec<TypeSymbol> = arguments
                    .iter()
                    .map(|argument| argument.ty.clone())
                    .collect();
                self.resolve_call(&name, &candidates, &argument_types, span)
            }
            _ => TypeSymbol::Error,
        };
        BoundExpr {
            kind: BoundExprKind::Call {
                callee: Box::new(callee),
                arguments,
            },
            ty,
        }
    }

    /// Resolves a call to a method group by overload resolution (14.4.2),
    /// reporting the appropriate diagnostic and returning the result type.
    fn resolve_call(
        &mut self,
        name: &str,
        candidates: &[MethodSymbol],
        argument_types: &[TypeSymbol],
        span: Span,
    ) -> TypeSymbol {
        match resolve_overload(&self.model, candidates, argument_types) {
            OverloadResult::Resolved(return_type) => return_type,
            OverloadResult::Ambiguous => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::AmbiguousCall {
                        method: name.into(),
                    },
                    span,
                ));
                TypeSymbol::Error
            }
            OverloadResult::WrongArgumentCount => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::NoOverloadForArgumentCount {
                        method: name.into(),
                        count: argument_types.len() as u32,
                    },
                    span,
                ));
                TypeSymbol::Error
            }
            OverloadResult::BadArgument { index, from, to } => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ArgumentConversion {
                        index: index as u32 + 1,
                        from: from.to_string().into(),
                        to: to.to_string().into(),
                    },
                    span,
                ));
                TypeSymbol::Error
            }
        }
    }

    fn bind_element_access(
        &mut self,
        receiver_expr: &Expr,
        argument_exprs: &[Expr],
        span: Span,
    ) -> BoundExpr {
        let receiver = self.bind_expression(receiver_expr);
        let indices: Vec<BoundExpr> = argument_exprs
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        let ty = match &receiver.ty {
            TypeSymbol::Error => TypeSymbol::Error,
            TypeSymbol::Array { element, .. } => (**element).clone(),
            other => {
                let type_name = other.to_string();
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::CannotIndex {
                        type_name: type_name.into(),
                    },
                    span,
                ));
                TypeSymbol::Error
            }
        };
        BoundExpr {
            kind: BoundExprKind::ElementAccess {
                receiver: Box::new(receiver),
                indices,
            },
            ty,
        }
    }

    fn bind_object_creation(
        &mut self,
        target: &TypeRef,
        argument_exprs: &[Expr],
        span: Span,
    ) -> BoundExpr {
        let target_ty = self.resolve_named_type(&bind_type(target), target.span);
        let arguments: Vec<BoundExpr> = argument_exprs
            .iter()
            .map(|argument| self.bind_expression(argument))
            .collect();
        let ty = if target_ty.is_error() {
            TypeSymbol::Error
        } else {
            if !arguments.iter().any(|argument| argument.ty.is_error()) {
                if let Some(constructors) = self
                    .type_info_of(&target_ty)
                    .map(|info| info.constructors.clone())
                {
                    let argument_types: Vec<TypeSymbol> = arguments
                        .iter()
                        .map(|argument| argument.ty.clone())
                        .collect();
                    self.check_constructor(&target_ty, &constructors, &argument_types, span);
                }
            }
            target_ty
        };
        BoundExpr {
            kind: BoundExprKind::ObjectCreation { arguments },
            ty,
        }
    }

    /// Resolves `new T(args)` against `T`'s constructors, reporting the diagnostic
    /// for a failed resolution. The created type is the result regardless.
    fn check_constructor(
        &mut self,
        target: &TypeSymbol,
        constructors: &[MethodSymbol],
        argument_types: &[TypeSymbol],
        span: Span,
    ) {
        match resolve_overload(&self.model, constructors, argument_types) {
            OverloadResult::Resolved(_) => {}
            OverloadResult::WrongArgumentCount => self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::NoConstructor {
                    type_name: target.to_string().into(),
                    count: argument_types.len() as u32,
                },
                span,
            )),
            OverloadResult::BadArgument { index, from, to } => {
                self.diagnostics.push(Diagnostic::new(
                    DiagnosticKind::ArgumentConversion {
                        index: index as u32 + 1,
                        from: from.to_string().into(),
                        to: to.to_string().into(),
                    },
                    span,
                ));
            }
            OverloadResult::Ambiguous => self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::AmbiguousCall {
                    method: target.to_string().into(),
                },
                span,
            )),
        }
    }

    /// Looks a member up on a type, walking the base-class chain (14.3, 14.5.4).
    fn resolve_member(&self, ty: &TypeSymbol, name: &str) -> MemberResolution {
        let mut current = self.type_info_of(ty);
        if current.is_none() {
            return MemberResolution::Unknown;
        }
        while let Some(info) = current {
            if let Some(field) = info.find_field(name) {
                return MemberResolution::Field(field.ty.clone());
            }
            if let Some(property) = info.find_property(name) {
                return MemberResolution::Property(property.ty.clone());
            }
            if info.methods_named(name).next().is_some() {
                return MemberResolution::MethodGroup;
            }
            current = info.base.as_ref().and_then(|base| self.type_info_of(base));
        }
        MemberResolution::NoSuchMember(ty.to_string())
    }

    /// The model entry for a named type, if any.
    fn type_info_of(&self, ty: &TypeSymbol) -> Option<&TypeInfo> {
        self.model.get_by_symbol(ty)
    }

    /// Every method named `name` on `ty` or any of its base classes -- the method
    /// group an invocation resolves over (most-derived first).
    fn methods_in_chain(&self, ty: &TypeSymbol, name: &str) -> Vec<MethodSymbol> {
        let mut methods = Vec::new();
        let mut current = self.type_info_of(ty);
        while let Some(info) = current {
            methods.extend(info.methods_named(name).cloned());
            current = info.base.as_ref().and_then(|base| self.type_info_of(base));
        }
        methods
    }

    /// Binds a simple name (14.5.2). For now a name resolves only to a local
    /// variable or parameter; anything else is `CS0103` (field, type, and
    /// namespace lookup arrive with the declaration model).
    fn bind_name(&mut self, name: &str, span: Span) -> BoundExpr {
        if let Some(ty) = self.lookup_local(name) {
            return BoundExpr {
                kind: BoundExprKind::Local(name.into()),
                ty: ty.clone(),
            };
        }
        if let Some(current) = self.current_type.clone() {
            match self.resolve_member(&current, name) {
                MemberResolution::Field(field_ty) => {
                    return BoundExpr {
                        kind: BoundExprKind::FieldAccess {
                            receiver: Box::new(self.this_expr()),
                            name: name.into(),
                        },
                        ty: field_ty,
                    };
                }
                MemberResolution::Property(property_ty) => {
                    return BoundExpr {
                        kind: BoundExprKind::PropertyAccess {
                            receiver: Box::new(self.this_expr()),
                            name: name.into(),
                        },
                        ty: property_ty,
                    };
                }
                MemberResolution::MethodGroup => {
                    return BoundExpr {
                        kind: BoundExprKind::MethodGroup {
                            receiver: Box::new(self.this_expr()),
                            name: name.into(),
                        },
                        ty: TypeSymbol::Error,
                    };
                }
                MemberResolution::NoSuchMember(_) | MemberResolution::Unknown => {}
            }
        }
        if self.model.get("", name).is_some() {
            let ty = TypeSymbol::Named([Box::from(name)].into());
            return BoundExpr {
                kind: BoundExprKind::TypeReference(ty.clone()),
                ty,
            };
        }
        self.diagnostics.push(Diagnostic::new(
            DiagnosticKind::NameNotFound { name: name.into() },
            span,
        ));
        error_expr()
    }

    /// The `this` access, typed as the enclosing type (the error type when there
    /// is none, for recovery).
    fn this_expr(&self) -> BoundExpr {
        BoundExpr {
            kind: BoundExprKind::This,
            ty: self.current_type.clone().unwrap_or(TypeSymbol::Error),
        }
    }

    /// The `base` access, typed as the enclosing type's base class (the error type
    /// when there is no enclosing type or it has no base, for recovery).
    fn base_expr(&self) -> BoundExpr {
        let base = self
            .current_type
            .as_ref()
            .and_then(|ty| self.type_info_of(ty))
            .and_then(|info| info.base.clone());
        BoundExpr {
            kind: BoundExprKind::Base,
            ty: base.unwrap_or(TypeSymbol::Error),
        }
    }
}

/// Binds a single expression and discards the diagnostics, for callers that only
/// want the typed tree.
#[must_use]
pub fn bind_expression(expr: &Expr) -> BoundExpr {
    let mut binder = Binder::new();
    binder.bind_expression(expr)
}

/// The result type of a binary operator on operand types, or `None` if the
/// operator does not apply (14.7-14.12).
fn binary_result_type(
    operator: BinaryOperator,
    left: &TypeSymbol,
    right: &TypeSymbol,
) -> Option<TypeSymbol> {
    use BinaryOperator as Op;
    let bool_type = TypeSymbol::Special(SpecialType::Boolean);
    let left_special = as_special(left);
    let right_special = as_special(right);
    match operator {
        Op::Multiply | Op::Divide | Op::Modulo | Op::Add | Op::Subtract => {
            binary_numeric_promotion(left_special?, right_special?).map(TypeSymbol::Special)
        }
        Op::LessThan | Op::GreaterThan | Op::LessThanOrEqual | Op::GreaterThanOrEqual => {
            binary_numeric_promotion(left_special?, right_special?).map(|_| bool_type)
        }
        Op::Equal | Op::NotEqual => equality_comparable(left, right).then_some(bool_type),
        Op::LogicalAnd | Op::LogicalOr => {
            let boolean = Some(SpecialType::Boolean);
            (left_special == boolean && right_special == boolean).then_some(bool_type)
        }
        Op::BitwiseAnd | Op::BitwiseOr | Op::BitwiseXor => {
            let boolean = Some(SpecialType::Boolean);
            if left_special == boolean && right_special == boolean {
                Some(bool_type)
            } else {
                let (left, right) = (left_special?, right_special?);
                (is_integral(left) && is_integral(right))
                    .then_some(binary_numeric_promotion(left, right).map(TypeSymbol::Special))
                    .flatten()
            }
        }
        Op::LeftShift | Op::RightShift => {
            let (left, right) = (left_special?, right_special?);
            (is_integral(left) && is_integral(right))
                .then_some(TypeSymbol::Special(shift_result(left)))
        }
    }
}

/// The outcome of looking a member up on a type.
enum MemberResolution {
    /// A field, with its type.
    Field(TypeSymbol),
    /// A property, with its type.
    Property(TypeSymbol),
    /// One or more methods of that name (a method group).
    MethodGroup,
    /// The type is known but has no such member; carries the type's display name.
    NoSuchMember(String),
    /// The type is not in the model, so members cannot be resolved.
    Unknown,
}

/// An error placeholder expression, used for recovery.
fn error_expr() -> BoundExpr {
    BoundExpr {
        kind: BoundExprKind::Error,
        ty: TypeSymbol::Error,
    }
}

/// The outcome of overload resolution over a method group (14.4.2).
enum OverloadResult {
    /// A unique best overload, with its return type.
    Resolved(TypeSymbol),
    /// Two or more applicable overloads with no unique best.
    Ambiguous,
    /// No overload accepts this number of arguments.
    WrongArgumentCount,
    /// A count matches but an argument does not convert to the parameter.
    BadArgument {
        /// The 0-based argument position.
        index: usize,
        /// The argument type.
        from: TypeSymbol,
        /// The parameter type.
        to: TypeSymbol,
    },
}

/// Chooses the overload for a call (14.4.2): the unique best among the applicable
/// candidates, or the diagnostic-bearing outcome otherwise. Conversions are
/// resolved against `model` so a derived argument matches a base parameter.
fn resolve_overload(
    model: &Model,
    candidates: &[MethodSymbol],
    arguments: &[TypeSymbol],
) -> OverloadResult {
    let applicable: Vec<&MethodSymbol> = candidates
        .iter()
        .filter(|candidate| is_applicable(model, candidate, arguments))
        .collect();
    if let Some(best) = best_candidate(model, &applicable, arguments) {
        return OverloadResult::Resolved(best.return_type.clone());
    }
    if !applicable.is_empty() {
        return OverloadResult::Ambiguous;
    }
    let Some(count_matched) = candidates
        .iter()
        .find(|candidate| candidate.parameters.len() == arguments.len())
    else {
        return OverloadResult::WrongArgumentCount;
    };
    for (index, (argument, parameter)) in
        arguments.iter().zip(&count_matched.parameters).enumerate()
    {
        if !converts(model, argument, parameter) {
            return OverloadResult::BadArgument {
                index,
                from: argument.clone(),
                to: parameter.clone(),
            };
        }
    }
    OverloadResult::WrongArgumentCount
}

/// Whether a method is applicable to the arguments: the counts match and every
/// argument converts to its parameter (14.4.2.1).
fn is_applicable(model: &Model, method: &MethodSymbol, arguments: &[TypeSymbol]) -> bool {
    method.parameters.len() == arguments.len()
        && arguments
            .iter()
            .zip(&method.parameters)
            .all(|(argument, parameter)| converts(model, argument, parameter))
}

/// The single applicable candidate better than every other, or `None` when none
/// is uniquely best.
fn best_candidate<'a>(
    model: &Model,
    applicable: &[&'a MethodSymbol],
    arguments: &[TypeSymbol],
) -> Option<&'a MethodSymbol> {
    applicable.iter().copied().find(|&candidate| {
        applicable.iter().all(|&other| {
            core::ptr::eq(candidate, other) || is_better(model, candidate, other, arguments)
        })
    })
}

/// Whether `c1` is a better function member than `c2` for the arguments: no worse
/// a parameter for every argument and strictly better for at least one, using the
/// better-conversion-target rule (14.4.2.2, 14.4.2.3 simplified).
fn is_better(
    model: &Model,
    c1: &MethodSymbol,
    c2: &MethodSymbol,
    arguments: &[TypeSymbol],
) -> bool {
    let mut strictly_better_somewhere = false;
    for index in 0..arguments.len() {
        let (p1, p2) = (&c1.parameters[index], &c2.parameters[index]);
        if p1 == p2 {
            continue;
        }
        if converts(model, p1, p2) {
            strictly_better_somewhere = true;
        } else {
            return false;
        }
    }
    strictly_better_somewhere
}

/// The special type of `ty`, if it is one.
fn as_special(ty: &TypeSymbol) -> Option<SpecialType> {
    match ty {
        TypeSymbol::Special(special) => Some(*special),
        _ => None,
    }
}

/// The `System.Type` named type, the result of a `typeof` expression (14.5.11).
fn system_type() -> TypeSymbol {
    TypeSymbol::Named([Box::from("System"), Box::from("Type")].into())
}

/// The type of a conditional expression from its branch types (14.13): the branch
/// type the other implicitly converts to, or `None` (`CS0173`) when there is no
/// one-way conversion between them.
fn conditional_result_type(
    model: &Model,
    when_true: &TypeSymbol,
    when_false: &TypeSymbol,
) -> Option<TypeSymbol> {
    if when_true == when_false {
        return Some(when_true.clone());
    }
    match (
        converts(model, when_true, when_false),
        converts(model, when_false, when_true),
    ) {
        (true, false) => Some(when_false.clone()),
        (false, true) => Some(when_true.clone()),
        _ => None,
    }
}

/// Whether a bound expression denotes something assignable: a local or parameter,
/// a field or (writable) property, or an array element. A read-only property's
/// missing setter is a finer check left for later.
fn is_lvalue(expr: &BoundExpr) -> bool {
    matches!(
        expr.kind,
        BoundExprKind::Local(_)
            | BoundExprKind::FieldAccess { .. }
            | BoundExprKind::PropertyAccess { .. }
            | BoundExprKind::ElementAccess { .. }
    )
}

/// The binary operator underlying a compound assignment, or `None` for simple `=`.
fn compound_binary_operator(operator: AssignmentOperator) -> Option<BinaryOperator> {
    use AssignmentOperator as A;
    Some(match operator {
        A::Assign => return None,
        A::Add => BinaryOperator::Add,
        A::Subtract => BinaryOperator::Subtract,
        A::Multiply => BinaryOperator::Multiply,
        A::Divide => BinaryOperator::Divide,
        A::Modulo => BinaryOperator::Modulo,
        A::And => BinaryOperator::BitwiseAnd,
        A::Or => BinaryOperator::BitwiseOr,
        A::Xor => BinaryOperator::BitwiseXor,
        A::LeftShift => BinaryOperator::LeftShift,
        A::RightShift => BinaryOperator::RightShift,
    })
}

/// The source symbol of an assignment operator, for diagnostics.
fn assignment_symbol(operator: AssignmentOperator) -> &'static str {
    use AssignmentOperator as A;
    match operator {
        A::Assign => "=",
        A::Add => "+=",
        A::Subtract => "-=",
        A::Multiply => "*=",
        A::Divide => "/=",
        A::Modulo => "%=",
        A::And => "&=",
        A::Or => "|=",
        A::Xor => "^=",
        A::LeftShift => "<<=",
        A::RightShift => ">>=",
    }
}

/// Whether two types may be compared with `==`/`!=`. Numeric pairs that promote,
/// `bool` pairs, identical types, and anything against `object` qualify; the
/// stricter reference-equality rules arrive with the type hierarchy.
fn equality_comparable(left: &TypeSymbol, right: &TypeSymbol) -> bool {
    if let (Some(left), Some(right)) = (as_special(left), as_special(right)) {
        if left.is_numeric() && right.is_numeric() {
            return binary_numeric_promotion(left, right).is_some();
        }
        if left == SpecialType::Boolean && right == SpecialType::Boolean {
            return true;
        }
    }
    left == right
        || matches!(left, TypeSymbol::Special(SpecialType::Object))
        || matches!(right, TypeSymbol::Special(SpecialType::Object))
}

/// Binary numeric promotion (14.2.6.2): the common type of two numeric operands,
/// or `None` if either is not numeric (or `decimal` is mixed with floating point).
fn binary_numeric_promotion(left: SpecialType, right: SpecialType) -> Option<SpecialType> {
    use SpecialType::{Decimal, Double, Int16, Int32, Int64, SByte, Single, UInt32, UInt64};
    if !left.is_numeric() || !right.is_numeric() {
        return None;
    }
    let has = |special: SpecialType| left == special || right == special;
    Some(if has(Decimal) {
        if has(Double) || has(Single) {
            return None;
        }
        Decimal
    } else if has(Double) {
        Double
    } else if has(Single) {
        Single
    } else if has(UInt64) {
        UInt64
    } else if has(Int64) {
        Int64
    } else if has(UInt32) {
        if matches!(left, SByte | Int16 | Int32) || matches!(right, SByte | Int16 | Int32) {
            Int64
        } else {
            UInt32
        }
    } else {
        Int32
    })
}

/// Whether a special type is one of the integral types (14.8 shift, bitwise).
fn is_integral(special: SpecialType) -> bool {
    use SpecialType::{Byte, Char, Int16, Int32, Int64, SByte, UInt16, UInt32, UInt64};
    matches!(
        special,
        SByte | Byte | Int16 | UInt16 | Int32 | UInt32 | Int64 | UInt64 | Char
    )
}

/// The result type of a shift, i.e. the unary-numeric-promoted left operand:
/// `int`, `uint`, `long`, or `ulong` (14.8).
fn shift_result(left: SpecialType) -> SpecialType {
    match left {
        SpecialType::Int32 | SpecialType::UInt32 | SpecialType::Int64 | SpecialType::UInt64 => left,
        _ => SpecialType::Int32,
    }
}

/// The result type of a prefix unary operator, or `None` if it does not apply
/// (14.6). The `++`/`--` cases keep the operand type; their lvalue requirement is
/// checked once name resolution lands.
fn unary_result_type(operator: UnaryOperator, operand: &TypeSymbol) -> Option<TypeSymbol> {
    use SpecialType::{Boolean, Int64, UInt32, UInt64};
    let special = as_special(operand)?;
    match operator {
        UnaryOperator::Plus => special
            .is_numeric()
            .then_some(TypeSymbol::Special(unary_numeric_promote(special))),
        UnaryOperator::Minus => match special {
            UInt64 => None,
            UInt32 => Some(TypeSymbol::Special(Int64)),
            other if other.is_numeric() => Some(TypeSymbol::Special(unary_numeric_promote(other))),
            _ => None,
        },
        UnaryOperator::Not => (special == Boolean).then_some(TypeSymbol::Special(Boolean)),
        UnaryOperator::Complement => {
            is_integral(special).then_some(TypeSymbol::Special(unary_numeric_promote(special)))
        }
        UnaryOperator::PreIncrement | UnaryOperator::PreDecrement => {
            special.is_numeric().then_some(operand.clone())
        }
    }
}

/// Unary numeric promotion (14.2.6.1): the smaller integral types and `char`
/// promote to `int`; every other numeric type is unchanged.
fn unary_numeric_promote(special: SpecialType) -> SpecialType {
    use SpecialType::{Byte, Char, Int16, Int32, SByte, UInt16};
    match special {
        SByte | Byte | Int16 | UInt16 | Char => Int32,
        other => other,
    }
}

/// The source symbol of a prefix unary operator, for diagnostics.
fn unary_operator_symbol(operator: UnaryOperator) -> &'static str {
    match operator {
        UnaryOperator::Plus => "+",
        UnaryOperator::Minus => "-",
        UnaryOperator::Not => "!",
        UnaryOperator::Complement => "~",
        UnaryOperator::PreIncrement => "++",
        UnaryOperator::PreDecrement => "--",
    }
}

/// The type of a literal (9.4.4).
fn literal_type(literal: &Literal) -> TypeSymbol {
    let special = match literal {
        Literal::Integer { value, suffix } => integer_literal_type(*value, *suffix),
        Literal::Real { suffix } => match suffix {
            RealSuffix::Float => SpecialType::Single,
            RealSuffix::Decimal => SpecialType::Decimal,
            RealSuffix::Double | RealSuffix::None => SpecialType::Double,
        },
        Literal::Character(_) => SpecialType::Char,
        Literal::String(_) => SpecialType::String,
        Literal::Boolean(_) => SpecialType::Boolean,
        Literal::Null => SpecialType::Object,
    };
    TypeSymbol::Special(special)
}

/// The type of an integer literal (9.4.4.2): the first type in the
/// suffix-determined list whose range holds the value.
fn integer_literal_type(value: u64, suffix: IntegerSuffix) -> SpecialType {
    let i32_max = i32::MAX as u64;
    let u32_max = u32::MAX as u64;
    let i64_max = i64::MAX as u64;
    match suffix {
        IntegerSuffix::None => {
            if value <= i32_max {
                SpecialType::Int32
            } else if value <= u32_max {
                SpecialType::UInt32
            } else if value <= i64_max {
                SpecialType::Int64
            } else {
                SpecialType::UInt64
            }
        }
        IntegerSuffix::Unsigned => {
            if value <= u32_max {
                SpecialType::UInt32
            } else {
                SpecialType::UInt64
            }
        }
        IntegerSuffix::Long => {
            if value <= i64_max {
                SpecialType::Int64
            } else {
                SpecialType::UInt64
            }
        }
        IntegerSuffix::UnsignedLong => SpecialType::UInt64,
    }
}

/// The source symbol of a binary operator, for diagnostics.
fn operator_symbol(operator: BinaryOperator) -> &'static str {
    use BinaryOperator as Op;
    match operator {
        Op::Multiply => "*",
        Op::Divide => "/",
        Op::Modulo => "%",
        Op::Add => "+",
        Op::Subtract => "-",
        Op::LeftShift => "<<",
        Op::RightShift => ">>",
        Op::LessThan => "<",
        Op::GreaterThan => ">",
        Op::LessThanOrEqual => "<=",
        Op::GreaterThanOrEqual => ">=",
        Op::Equal => "==",
        Op::NotEqual => "!=",
        Op::BitwiseAnd => "&",
        Op::BitwiseXor => "^",
        Op::BitwiseOr => "|",
        Op::LogicalAnd => "&&",
        Op::LogicalOr => "||",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_syntax::parser::parse_expression;

    fn bound_type(source: &str) -> TypeSymbol {
        bind_expression(&parse_expression(source).expr).ty
    }

    fn codes(source: &str) -> Vec<u16> {
        let mut binder = Binder::new();
        binder.bind_expression(&parse_expression(source).expr);
        binder
            .into_diagnostics()
            .iter()
            .map(Diagnostic::code)
            .collect()
    }

    fn special(source: &str) -> SpecialType {
        match bound_type(source) {
            TypeSymbol::Special(special) => special,
            other => panic!("expected a special type, got {other:?}"),
        }
    }

    #[test]
    fn integer_literal_types_follow_the_value_and_suffix() {
        assert_eq!(special("42"), SpecialType::Int32);
        assert_eq!(special("2147483648"), SpecialType::UInt32);
        assert_eq!(special("10000000000000000000"), SpecialType::UInt64);
        assert_eq!(special("1u"), SpecialType::UInt32);
        assert_eq!(special("1L"), SpecialType::Int64);
    }

    #[test]
    fn arithmetic_uses_binary_numeric_promotion() {
        assert_eq!(special("1 + 2"), SpecialType::Int32);
        assert_eq!(special("1 + 2L"), SpecialType::Int64);
        assert_eq!(special("1 + 2.0"), SpecialType::Double);
        assert_eq!(special("1 * 2.0f"), SpecialType::Single);
        assert_eq!(special("'a' + 1"), SpecialType::Int32);
    }

    #[test]
    fn relational_equality_and_logical_yield_bool() {
        assert_eq!(special("1 < 2"), SpecialType::Boolean);
        assert_eq!(special("1 == 2"), SpecialType::Boolean);
        assert_eq!(special("true != false"), SpecialType::Boolean);
        assert_eq!(special("true && false"), SpecialType::Boolean);
    }

    #[test]
    fn bitwise_and_shift_typing() {
        assert_eq!(special("1 & 2"), SpecialType::Int32);
        assert_eq!(special("true | false"), SpecialType::Boolean);
        assert_eq!(special("1 << 2"), SpecialType::Int32);
        assert_eq!(special("1L << 2"), SpecialType::Int64);
    }

    #[test]
    fn inapplicable_operators_are_cs0019() {
        assert_eq!(codes("true + 1"), [19]);
        assert_eq!(codes("1 && 2"), [19]);
        assert_eq!(codes("\"x\" - \"y\""), [19]);
        assert_eq!(codes("(true + 1) + 2"), [19]);
    }

    #[test]
    fn unary_operator_typing() {
        assert_eq!(special("-1"), SpecialType::Int32);
        assert_eq!(special("-1L"), SpecialType::Int64);
        assert_eq!(special("-1u"), SpecialType::Int64);
        assert_eq!(special("+1"), SpecialType::Int32);
        assert_eq!(special("!true"), SpecialType::Boolean);
        assert_eq!(special("~1"), SpecialType::Int32);
        assert_eq!(special("1++"), SpecialType::Int32);
        assert_eq!(special("++1L"), SpecialType::Int64);
    }

    #[test]
    fn inapplicable_unary_operators_are_cs0023() {
        assert_eq!(codes("-true"), [23]);
        assert_eq!(codes("!1"), [23]);
        assert_eq!(codes("~true"), [23]);
        assert_eq!(codes("true++"), [23]);
    }

    fn bound_in_scope(binder: &mut Binder, source: &str) -> TypeSymbol {
        binder.bind_expression(&parse_expression(source).expr).ty
    }

    #[test]
    fn simple_names_resolve_to_declared_locals() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("x", TypeSymbol::Special(SpecialType::Int32));
        binder.declare_local("name", TypeSymbol::Special(SpecialType::String));
        assert_eq!(
            bound_in_scope(&mut binder, "x"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "x + 1"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "name"),
            TypeSymbol::Special(SpecialType::String)
        );
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn unknown_names_are_cs0103() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.bind_expression(&parse_expression("missing").expr);
        let codes: Vec<u16> = binder.diagnostics().iter().map(Diagnostic::code).collect();
        assert_eq!(codes, [103]);
    }

    #[test]
    fn cast_typetest_typeof_and_checked() {
        assert_eq!(special("(long)1"), SpecialType::Int64);
        assert_eq!(special("1 is int"), SpecialType::Boolean);
        assert_eq!(special("1 as object"), SpecialType::Object);
        assert_eq!(bound_type("typeof(int)").to_string(), "System.Type");
        assert_eq!(special("checked(1 + 2)"), SpecialType::Int32);
        assert_eq!(special("unchecked(1)"), SpecialType::Int32);
    }

    #[test]
    fn casts_require_an_explicit_conversion() {
        assert_eq!(codes("(byte)1"), []);
        assert_eq!(codes("(int)1u"), []);
        assert_eq!(codes("(long)1"), []);
        assert_eq!(codes("(string)1"), [30]);
        assert_eq!(codes("(bool)1"), [30]);
    }

    #[test]
    fn conditional_result_type_and_condition_check() {
        assert_eq!(special("true ? 1 : 2"), SpecialType::Int32);
        assert_eq!(special("true ? 1 : 2L"), SpecialType::Int64);
        assert_eq!(special("false ? 2L : 1"), SpecialType::Int64);
        assert_eq!(codes("1 ? 1 : 2"), [29]);
        assert_eq!(codes("true ? 1 : \"x\""), [173]);
    }

    #[test]
    fn assignment_typing_and_checks() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("x", TypeSymbol::Special(SpecialType::Int32));
        assert_eq!(
            bound_in_scope(&mut binder, "x = 1"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        bound_in_scope(&mut binder, "x += 2");
        assert!(binder.diagnostics().is_empty());
        let before = binder.diagnostics().len();
        bound_in_scope(&mut binder, "x = true");
        assert_eq!(binder.diagnostics()[before].code(), 29);
    }

    #[test]
    fn assigning_to_a_non_variable_is_cs0131() {
        assert_eq!(codes("1 = 2"), [131]);
    }

    #[test]
    fn member_access_resolves_fields_method_groups_and_missing_members() {
        use crate::symbols::{FieldSymbol, MethodSymbol, TypeInfo, TypeKind};
        let mut model = Model::new();
        let mut widget = TypeInfo::new("", "Widget", TypeKind::Class);
        widget.fields.push(FieldSymbol {
            name: "count".into(),
            ty: TypeSymbol::Special(SpecialType::Int32),
            is_static: false,
        });
        widget.methods.push(MethodSymbol {
            name: "Area".into(),
            return_type: TypeSymbol::Special(SpecialType::Double),
            parameters: Vec::new(),
            is_static: false,
        });
        model.insert(widget);

        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("w", TypeSymbol::Named(["Widget".into()].into()));

        let count = binder.bind_expression(&parse_expression("w.count").expr);
        assert_eq!(count.ty, TypeSymbol::Special(SpecialType::Int32));
        let area = binder.bind_expression(&parse_expression("w.Area").expr);
        assert!(matches!(area.kind, BoundExprKind::MethodGroup { .. }));
        assert!(binder.diagnostics().is_empty());
        binder.bind_expression(&parse_expression("w.missing").expr);
        assert_eq!(binder.diagnostics().last().map(Diagnostic::code), Some(117));
    }

    #[test]
    fn array_creation_and_element_access() {
        assert_eq!(bound_type("new int[5]").to_string(), "int[]");
        assert_eq!(bound_type("new int[5, 6]").to_string(), "int[,]");
        assert_eq!(bound_type("new int[3][]").to_string(), "int[][]");

        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("a", TypeSymbol::Special(SpecialType::Int32).into_array(1));
        assert_eq!(
            bound_in_scope(&mut binder, "a[0]"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
        binder.declare_local("n", TypeSymbol::Special(SpecialType::Int32));
        bound_in_scope(&mut binder, "n[0]");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 21));
    }

    #[test]
    fn object_creation_resolves_constructors() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Point { Point(int x, int y) { } Point(int x) { } } class Empty { }",
        )
        .unit;
        let model = collect_model(&unit);
        let bound = |source: &str| {
            Binder::with_model(model.clone()).bind_expression(&parse_expression(source).expr)
        };
        let codes = |source: &str| {
            let mut binder = Binder::with_model(model.clone());
            binder.bind_expression(&parse_expression(source).expr);
            binder
                .into_diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(bound("new Point(1, 2)").ty.to_string(), "Point");
        assert!(codes("new Point(1, 2)").is_empty());
        assert_eq!(bound("new Empty()").ty.to_string(), "Empty");
        assert!(codes("new Empty()").is_empty());
        assert_eq!(codes("new Point(1, 2, 3)"), [1729]);
        assert_eq!(codes("new Point(true, 2)"), [1503]);
        assert_eq!(codes("new Gadget()"), [246]);
    }

    #[test]
    fn this_and_bare_names_resolve_against_the_enclosing_type() {
        use crate::symbols::{FieldSymbol, MethodSymbol, TypeInfo, TypeKind};
        let mut widget = TypeInfo::new("", "Widget", TypeKind::Class);
        widget.fields.push(FieldSymbol {
            name: "count".into(),
            ty: TypeSymbol::Special(SpecialType::Int32),
            is_static: false,
        });
        widget.methods.push(MethodSymbol {
            name: "Area".into(),
            return_type: TypeSymbol::Special(SpecialType::Double),
            parameters: Vec::new(),
            is_static: false,
        });
        let mut model = Model::new();
        model.insert(widget);

        let mut binder = Binder::with_model(model);
        binder.enter_type(TypeSymbol::Named(["Widget".into()].into()));
        binder.enter_scope();

        assert_eq!(bound_in_scope(&mut binder, "this").to_string(), "Widget");
        assert_eq!(
            bound_in_scope(&mut binder, "count"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "this.count"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "Area()"),
            TypeSymbol::Special(SpecialType::Double)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "this.Area()"),
            TypeSymbol::Special(SpecialType::Double)
        );
        assert!(binder.diagnostics().is_empty());
        bound_in_scope(&mut binder, "missing");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 103));
    }

    #[test]
    fn member_lookup_walks_the_base_chain() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Animal { int legs; int Speed() { } } \
             class Dog : Animal { string breed; }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("d", TypeSymbol::Named(["Dog".into()].into()));

        assert_eq!(
            bound_in_scope(&mut binder, "d.breed"),
            TypeSymbol::Special(SpecialType::String)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "d.legs"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "d.Speed()"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn base_access_resolves_against_the_base_class() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Animal { int Speed() { return 0; } } class Dog : Animal { }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_type(TypeSymbol::Named(["Dog".into()].into()));
        binder.enter_scope();
        assert_eq!(
            bound_in_scope(&mut binder, "base.Speed()"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn static_access_through_a_type_name() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit =
            parse_compilation_unit("class Calc { static int Zero; static int Pi() { return 3; } }")
                .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();

        assert_eq!(
            bound_in_scope(&mut binder, "Calc.Zero"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(
            bound_in_scope(&mut binder, "Calc.Pi()"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert!(binder.diagnostics().is_empty());
        bound_in_scope(&mut binder, "Nope");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 103));
    }

    #[test]
    fn enum_members_and_enum_casts() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit("enum Color { Red, Green, Blue }").unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();

        assert_eq!(
            bound_in_scope(&mut binder, "Color.Red").to_string(),
            "Color"
        );
        assert_eq!(
            bound_in_scope(&mut binder, "(int)Color.Red"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        assert_eq!(bound_in_scope(&mut binder, "(Color)1").to_string(), "Color");
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn property_access_and_member_assignment() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Box { int Width { get { return 0; } set { } } int height; }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("b", TypeSymbol::Named(["Box".into()].into()));

        assert_eq!(
            bound_in_scope(&mut binder, "b.Width"),
            TypeSymbol::Special(SpecialType::Int32)
        );
        bound_in_scope(&mut binder, "b.height = 5");
        bound_in_scope(&mut binder, "b.Width = 5");
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn reference_conversions_follow_the_base_chain() {
        use crate::declaration::collect_model;
        use lamella_syntax::parser::parse_compilation_unit;

        let unit = parse_compilation_unit(
            "class Animal { } class Dog : Animal { } class Pen { void Hold(Animal a) { } }",
        )
        .unit;
        let model = collect_model(&unit);
        let mut binder = Binder::with_model(model);
        binder.enter_scope();
        binder.declare_local("a", TypeSymbol::Named(["Animal".into()].into()));
        binder.declare_local("d", TypeSymbol::Named(["Dog".into()].into()));
        binder.declare_local("p", TypeSymbol::Named(["Pen".into()].into()));

        bound_in_scope(&mut binder, "a = d");
        bound_in_scope(&mut binder, "p.Hold(d)");
        assert!(binder.diagnostics().is_empty());
        assert!(binder.converts(
            &TypeSymbol::Named(["Dog".into()].into()),
            &TypeSymbol::Special(SpecialType::Object)
        ));
        bound_in_scope(&mut binder, "d = a");
        assert!(binder.diagnostics().iter().any(|d| d.code() == 29));
    }

    #[test]
    fn method_binding_checks_return_and_scopes_parameters() {
        use lamella_syntax::parser::parse_statement;
        let int = TypeSymbol::Special(SpecialType::Int32);
        let void = TypeSymbol::Special(SpecialType::Void);

        let codes = |return_type: TypeSymbol, source: &str| {
            let mut binder = Binder::new();
            let body = parse_statement(source).statement;
            binder.bind_method(None, "M", return_type, &[], &body);
            binder
                .into_diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(codes(int.clone(), "{ return 1; }"), []);
        assert_eq!(codes(int.clone(), "{ return; }"), [126]);
        assert_eq!(codes(int.clone(), "{ return \"x\"; }"), [29]);
        assert_eq!(codes(void.clone(), "{ return 1; }"), [127]);
        assert_eq!(codes(void, "{ return; }"), []);

        let mut binder = Binder::new();
        let body = parse_statement("{ return n; }").statement;
        binder.bind_method(None, "M", int.clone(), &[("n".into(), int)], &body);
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn definite_assignment_reports_cs0165() {
        use lamella_syntax::parser::parse_statement;
        let int = TypeSymbol::Special(SpecialType::Int32);
        let void = TypeSymbol::Special(SpecialType::Void);
        let codes = |source: &str| {
            let mut binder = Binder::new();
            let body = parse_statement(source).statement;
            binder.bind_method(None, "M", void.clone(), &[], &body);
            binder
                .into_diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(codes("{ int x; int y = x; }"), [165]);
        assert_eq!(codes("{ int x; x = 1; int y = x; }"), []);
        assert_eq!(
            codes("{ bool c = true; int x; if (c) x = 1; int y = x; }"),
            [165]
        );
        assert_eq!(
            codes("{ bool c = true; int x; if (c) x = 1; else x = 2; int y = x; }"),
            []
        );
        assert_eq!(
            codes("{ bool c = true; int x; if (c) return; else x = 1; int y = x; }"),
            []
        );
        assert_eq!(codes("{ int x; if (true) x = 1; int y = x; }"), []);

        let mut binder = Binder::new();
        let body = parse_statement("{ int y = p; }").statement;
        binder.bind_method(None, "M", void, &[("p".into(), int)], &body);
        assert!(binder.diagnostics().is_empty());
    }

    #[test]
    fn not_all_paths_return_is_cs0161() {
        use lamella_syntax::parser::parse_statement;
        let int = TypeSymbol::Special(SpecialType::Int32);
        let void = TypeSymbol::Special(SpecialType::Void);

        let codes = |return_type: TypeSymbol, source: &str| {
            let mut binder = Binder::new();
            let body = parse_statement(source).statement;
            binder.bind_method(None, "M", return_type, &[], &body);
            binder
                .into_diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };

        assert_eq!(codes(int.clone(), "{ int x = 1; }"), [161]);
        assert_eq!(
            codes(int.clone(), "{ if (true) return 1; else return 2; }"),
            []
        );
        assert_eq!(codes(int.clone(), "{ while (true) { } }"), []);
        assert_eq!(codes(int, "{ throw; }"), []);
        assert_eq!(codes(void, "{ int x = 1; }"), []);
    }

    #[test]
    fn invocation_does_overload_resolution() {
        use crate::symbols::{MethodSymbol, TypeInfo, TypeKind};

        fn method(
            name: &str,
            return_type: TypeSymbol,
            parameters: Vec<TypeSymbol>,
        ) -> MethodSymbol {
            MethodSymbol {
                name: name.into(),
                return_type,
                parameters,
                is_static: false,
            }
        }
        let int = TypeSymbol::Special(SpecialType::Int32);
        let long = TypeSymbol::Special(SpecialType::Int64);
        let double = TypeSymbol::Special(SpecialType::Double);
        let void = TypeSymbol::Special(SpecialType::Void);

        let mut calc = TypeInfo::new("", "Calc", TypeKind::Class);
        calc.methods
            .push(method("F", int.clone(), alloc::vec![int.clone()]));
        calc.methods
            .push(method("F", double.clone(), alloc::vec![double.clone()]));
        calc.methods
            .push(method("Take", void.clone(), alloc::vec![int.clone()]));
        calc.methods.push(method(
            "G",
            void.clone(),
            alloc::vec![int.clone(), long.clone()],
        ));
        calc.methods
            .push(method("G", void, alloc::vec![long, int.clone()]));
        let mut model = Model::new();
        model.insert(calc);

        let call_codes = |source: &str| {
            let mut binder = Binder::with_model(model.clone());
            binder.enter_scope();
            binder.declare_local("c", TypeSymbol::Named(["Calc".into()].into()));
            binder.bind_expression(&parse_expression(source).expr);
            binder
                .into_diagnostics()
                .iter()
                .map(Diagnostic::code)
                .collect::<Vec<_>>()
        };
        let call_type = |source: &str| {
            let mut binder = Binder::with_model(model.clone());
            binder.enter_scope();
            binder.declare_local("c", TypeSymbol::Named(["Calc".into()].into()));
            binder.bind_expression(&parse_expression(source).expr).ty
        };

        assert_eq!(call_type("c.F(1)"), int);
        assert_eq!(call_type("c.F(1.0)"), double);
        assert_eq!(call_type("c.F(1L)"), double);
        assert!(call_codes("c.F(1)").is_empty());
        assert_eq!(call_codes("c.Take(1, 2)"), [1501]);
        assert_eq!(call_codes("c.Take(\"x\")"), [1503]);
        assert_eq!(call_codes("c.G(1, 1)"), [121]);
    }

    #[test]
    fn scopes_nest_and_unwind() {
        let mut binder = Binder::new();
        binder.enter_scope();
        binder.declare_local("outer", TypeSymbol::Special(SpecialType::Int32));
        binder.enter_scope();
        binder.declare_local("inner", TypeSymbol::Special(SpecialType::Boolean));
        assert!(!bound_in_scope(&mut binder, "outer").is_error());
        assert!(!bound_in_scope(&mut binder, "inner").is_error());
        binder.exit_scope();
        assert!(!bound_in_scope(&mut binder, "outer").is_error());
        let before = binder.diagnostics().len();
        assert!(bound_in_scope(&mut binder, "inner").is_error());
        assert_eq!(binder.diagnostics().len(), before + 1);
    }
}
