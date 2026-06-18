//! The syntax tree the parser builds from the token stream.

use crate::span::Span;
use crate::token::{IntegerSuffix, RealSuffix};
use alloc::boxed::Box;
use alloc::vec::Vec;

/// An expression: a [`ExprKind`] together with the source [`Span`] it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expr {
    /// What kind of expression this is, with its children.
    pub kind: ExprKind,
    /// The byte range the expression covers in the source.
    pub span: Span,
}

impl Expr {
    /// Creates an expression node of `kind` covering `span`.
    #[must_use]
    pub fn new(kind: ExprKind, span: Span) -> Expr {
        Expr { kind, span }
    }
}

/// The kind of an [`Expr`], with any child expressions (ECMA-334 1st ed, 14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExprKind {
    /// A literal value (14.5.1): the lexer decoded it; this carries the result.
    Literal(Literal),
    /// A simple name (14.5.2): a bare identifier, its `@` prefix already removed.
    Name(Box<str>),
    /// The `this` access (14.5.7).
    This,
    /// A parenthesized expression (14.5.3): the parentheses group, they are not
    /// part of the value, so the inner expression is kept directly.
    Parenthesized(Box<Expr>),
    /// A member access `receiver.name` (14.5.4).
    MemberAccess {
        /// The expression whose member is named.
        receiver: Box<Expr>,
        /// The accessed member's name.
        name: Box<str>,
    },
    /// An invocation `receiver(arguments)` (14.5.5).
    Invocation {
        /// The expression being invoked.
        receiver: Box<Expr>,
        /// The argument expressions, in order.
        arguments: Vec<Expr>,
    },
    /// An element access `receiver[arguments]` (14.5.6).
    ElementAccess {
        /// The expression being indexed.
        receiver: Box<Expr>,
        /// The index argument expressions, in order.
        arguments: Vec<Expr>,
    },
    /// A prefix unary operation, including pre-increment and pre-decrement (14.6).
    Unary {
        /// The operator applied.
        operator: UnaryOperator,
        /// The operand it applies to.
        operand: Box<Expr>,
    },
    /// A postfix `++` or `--` (14.5.9).
    PostfixUnary {
        /// Whether the operator increments or decrements.
        operator: PostfixOperator,
        /// The operand it applies to.
        operand: Box<Expr>,
    },
    /// A binary operation (14.7 through 14.12).
    Binary {
        /// The operator applied.
        operator: BinaryOperator,
        /// The left operand.
        left: Box<Expr>,
        /// The right operand.
        right: Box<Expr>,
    },
    /// A conditional `condition ? when_true : when_false` (14.13).
    Conditional {
        /// The condition tested.
        condition: Box<Expr>,
        /// The value when the condition is true.
        when_true: Box<Expr>,
        /// The value when the condition is false.
        when_false: Box<Expr>,
    },
    /// An assignment, simple or compound (14.14).
    Assignment {
        /// Which assignment operator was used.
        operator: AssignmentOperator,
        /// The assignment target.
        target: Box<Expr>,
        /// The value assigned.
        value: Box<Expr>,
    },
    /// A `typeof` expression (14.5.11): `typeof ( type )`.
    TypeOf(TypeRef),
    /// A `checked ( expression )` (14.5.12), forcing overflow checking on.
    Checked(Box<Expr>),
    /// An `unchecked ( expression )` (14.5.12), forcing overflow checking off.
    Unchecked(Box<Expr>),
    /// An `is` or `as` type test (14.9.9, 14.9.10): the operand against a type.
    TypeTest {
        /// Whether this is `is` or `as`.
        operation: TypeTestOperation,
        /// The expression being tested or converted.
        operand: Box<Expr>,
        /// The type tested against.
        target: TypeRef,
    },
    /// A placeholder for an expression that could not be parsed. It is emitted
    /// with a diagnostic so the parser can keep building a tree for the rest.
    Error,
}

/// Whether a [`ExprKind::TypeTest`] is an `is` or an `as` (14.9.9, 14.9.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTestOperation {
    /// `is`: tests whether the operand is of the type, yielding a `bool`.
    Is,
    /// `as`: converts to the type or yields `null`, never throwing.
    As,
}

/// A reference to a type (ECMA-334 1st ed, clause 11): a predefined type, a
/// (possibly qualified) type name, or an array of one of those. Pointer types
/// (unsafe code) are deferred with the rest of unsafe support.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TypeRef {
    /// What the type is, with any element type.
    pub kind: TypeRefKind,
    /// The byte range the type covers in the source.
    pub span: Span,
}

impl TypeRef {
    /// Creates a type reference of `kind` covering `span`.
    #[must_use]
    pub fn new(kind: TypeRefKind, span: Span) -> TypeRef {
        TypeRef { kind, span }
    }
}

/// The kind of a [`TypeRef`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeRefKind {
    /// A predefined type keyword, such as `int` or `string` (11.1.4).
    Predefined(PredefinedType),
    /// A type name, its parts in order: `A.B.C` is `["A", "B", "C"]` (11.1).
    Name(Vec<Box<str>>),
    /// An array type (12.1): an element type and the rank (dimension count) of
    /// this array. `int[][]` nests an `Array` whose element is another `Array`.
    Array {
        /// The element type.
        element: Box<TypeRef>,
        /// The number of dimensions, so `T[]` is 1 and `T[,]` is 2.
        rank: u8,
    },
    /// A placeholder for a type that could not be parsed, emitted with a
    /// diagnostic for recovery.
    Error,
}

/// A predefined type (ECMA-334 1st ed, 11.1.4): the type keywords.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredefinedType {
    /// `bool`.
    Bool,
    /// `byte`.
    Byte,
    /// `sbyte`.
    Sbyte,
    /// `short`.
    Short,
    /// `ushort`.
    Ushort,
    /// `int`.
    Int,
    /// `uint`.
    Uint,
    /// `long`.
    Long,
    /// `ulong`.
    Ulong,
    /// `char`.
    Char,
    /// `float`.
    Float,
    /// `double`.
    Double,
    /// `decimal`.
    Decimal,
    /// `string`.
    String,
    /// `object`.
    Object,
    /// `void`, valid only in a few positions but parsed uniformly here.
    Void,
}

/// A literal value as decoded by the lexer (9.4.4): the parser lifts the token's
/// decoded payload into the tree unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Literal {
    /// An integer literal: its value and the suffix constraining its type.
    Integer {
        /// The numeric value.
        value: u64,
        /// The `U`/`L` suffix, if any.
        suffix: IntegerSuffix,
    },
    /// A real literal: only the suffix is kept; binding computes the value with
    /// the target type's rounding.
    Real {
        /// The `F`/`D`/`M` suffix, if any.
        suffix: RealSuffix,
    },
    /// A character literal: one UTF-16 code unit.
    Character(u16),
    /// A string literal: its decoded UTF-16 code units.
    String(Box<[u16]>),
    /// A boolean literal, `true` or `false`.
    Boolean(bool),
    /// The null literal.
    Null,
}

/// A prefix unary operator (14.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    /// Unary `+`.
    Plus,
    /// Unary `-`.
    Minus,
    /// Logical negation `!`.
    Not,
    /// Bitwise complement `~`.
    Complement,
    /// Pre-increment `++`.
    PreIncrement,
    /// Pre-decrement `--`.
    PreDecrement,
}

/// A postfix unary operator (14.5.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PostfixOperator {
    /// Postfix `++`.
    Increment,
    /// Postfix `--`.
    Decrement,
}

/// A binary operator (14.7 through 14.12). All are left-associative.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    /// `*`.
    Multiply,
    /// `/`.
    Divide,
    /// `%`.
    Modulo,
    /// `+`.
    Add,
    /// `-`.
    Subtract,
    /// `<<`.
    LeftShift,
    /// `>>`.
    RightShift,
    /// `<`.
    LessThan,
    /// `>`.
    GreaterThan,
    /// `<=`.
    LessThanOrEqual,
    /// `>=`.
    GreaterThanOrEqual,
    /// `==`.
    Equal,
    /// `!=`.
    NotEqual,
    /// `&`.
    BitwiseAnd,
    /// `^`.
    BitwiseXor,
    /// `|`.
    BitwiseOr,
    /// `&&`.
    LogicalAnd,
    /// `||`.
    LogicalOr,
}

/// An assignment operator, simple or compound (14.14).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentOperator {
    /// `=`.
    Assign,
    /// `+=`.
    Add,
    /// `-=`.
    Subtract,
    /// `*=`.
    Multiply,
    /// `/=`.
    Divide,
    /// `%=`.
    Modulo,
    /// `&=`.
    And,
    /// `|=`.
    Or,
    /// `^=`.
    Xor,
    /// `<<=`.
    LeftShift,
    /// `>>=`.
    RightShift,
}
