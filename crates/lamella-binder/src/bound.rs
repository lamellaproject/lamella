//! The bound expression tree and the expression binder (ECMA-334 1st ed,
//! clause 14).

use crate::bind::bind_type;
use crate::conversion::has_implicit_conversion;
use crate::diagnostic::{Diagnostic, DiagnosticKind};
use crate::resolve::{TypeTable, resolve_type};
use crate::special::SpecialType;
use crate::types::TypeSymbol;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_syntax::ast::{
    AssignmentOperator, BinaryOperator, Expr, ExprKind, Literal, PostfixOperator,
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

/// Binds expressions, accumulating the semantic diagnostics found. Holds a stack
/// of local-variable scopes for name resolution.
#[derive(Debug, Default)]
pub struct Binder {
    diagnostics: Vec<Diagnostic>,
    scopes: Vec<BTreeMap<String, TypeSymbol>>,
    world: TypeTable,
}

impl Binder {
    /// A fresh binder with an empty reference world.
    #[must_use]
    pub fn new() -> Binder {
        Binder::default()
    }

    /// A binder that resolves named types against `world`.
    #[must_use]
    pub fn with_world(world: TypeTable) -> Binder {
        Binder {
            world,
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
        if !condition.ty.is_error() && !has_implicit_conversion(&condition.ty, &boolean) {
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
        } else if let Some(common) = conditional_result_type(&when_true.ty, &when_false.ty) {
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
                if !has_implicit_conversion(value, target) {
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

    /// Binds a simple name (14.5.2). For now a name resolves only to a local
    /// variable or parameter; anything else is `CS0103` (field, type, and
    /// namespace lookup arrive with the declaration model).
    fn bind_name(&mut self, name: &str, span: Span) -> BoundExpr {
        if let Some(ty) = self.lookup_local(name) {
            BoundExpr {
                kind: BoundExprKind::Local(name.into()),
                ty: ty.clone(),
            }
        } else {
            self.diagnostics.push(Diagnostic::new(
                DiagnosticKind::NameNotFound { name: name.into() },
                span,
            ));
            BoundExpr {
                kind: BoundExprKind::Error,
                ty: TypeSymbol::Error,
            }
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
fn conditional_result_type(when_true: &TypeSymbol, when_false: &TypeSymbol) -> Option<TypeSymbol> {
    if when_true == when_false {
        return Some(when_true.clone());
    }
    match (
        has_implicit_conversion(when_true, when_false),
        has_implicit_conversion(when_false, when_true),
    ) {
        (true, false) => Some(when_false.clone()),
        (false, true) => Some(when_true.clone()),
        _ => None,
    }
}

/// Whether a bound expression denotes something assignable. Only locals and
/// parameters qualify for now; fields, array elements, and properties follow with
/// member access.
fn is_lvalue(expr: &BoundExpr) -> bool {
    matches!(expr.kind, BoundExprKind::Local(_))
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
