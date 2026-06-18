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
    /// A predefined type in expression position (14.5.4): the left side of a
    /// static member access such as `int.Parse`. Binding rejects it anywhere a
    /// value, rather than a type name, is required.
    PredefinedType(PredefinedType),
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
    /// A cast `( type ) operand` (14.6.6).
    Cast {
        /// The type cast to.
        target: TypeRef,
        /// The expression being cast.
        operand: Box<Expr>,
    },
    /// An object (or delegate) creation `new type ( arguments )` (14.5.10.1).
    ObjectCreation {
        /// The type being created (a non-array type).
        target: TypeRef,
        /// The constructor arguments, in order.
        arguments: Vec<Expr>,
    },
    /// An array creation `new element[lengths] extra-ranks` (14.5.10.2). When
    /// `lengths` is empty the size came from an initializer, which is not yet
    /// parsed; `rank` is the first dimension's rank and `extra_ranks` the trailing
    /// jagged ranks.
    ArrayCreation {
        /// The element (non-array) type.
        element: TypeRef,
        /// The size expressions of the first dimension; empty if unsized.
        lengths: Vec<Expr>,
        /// The rank of the first dimension.
        rank: u8,
        /// Trailing jagged rank-specifiers, outermost first.
        extra_ranks: Vec<u8>,
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

/// A statement: a [`StmtKind`] and the source [`Span`] it covers (clause 15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stmt {
    /// What kind of statement this is, with its children.
    pub kind: StmtKind,
    /// The byte range the statement covers in the source.
    pub span: Span,
}

impl Stmt {
    /// Creates a statement of `kind` covering `span`.
    #[must_use]
    pub fn new(kind: StmtKind, span: Span) -> Stmt {
        Stmt { kind, span }
    }
}

/// The kind of a [`Stmt`] (ECMA-334 1st ed, clause 15).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StmtKind {
    /// A block `{ ... }` (15.2).
    Block(Vec<Stmt>),
    /// The empty statement `;` (15.3).
    Empty,
    /// An expression statement `expression ;` (15.6). Binding checks that the
    /// expression is one allowed as a statement (a call, assignment, increment,
    /// decrement, or object creation).
    Expression(Expr),
    /// A local variable declaration `type declarators ;` (15.5.1).
    LocalDeclaration {
        /// The declared type, shared by every declarator.
        ty: TypeRef,
        /// The declared variables, in order.
        declarators: Vec<VariableDeclarator>,
    },
    /// A `return` statement, with its optional value (15.9.4).
    Return(Option<Expr>),
    /// An `if` statement with an optional `else` branch (15.7.1).
    If {
        /// The condition tested.
        condition: Expr,
        /// The statement run when the condition is true.
        then_branch: Box<Stmt>,
        /// The statement run otherwise, if an `else` is present.
        else_branch: Option<Box<Stmt>>,
    },
    /// A `while` statement (15.8.1).
    While {
        /// The loop condition.
        condition: Expr,
        /// The loop body.
        body: Box<Stmt>,
    },
    /// A `do body while ( condition ) ;` statement (15.8.2).
    DoWhile {
        /// The loop body, run before the first test.
        body: Box<Stmt>,
        /// The condition tested after each iteration.
        condition: Expr,
    },
    /// A `for` statement (15.8.3).
    For {
        /// The initializer clause, if any.
        initializer: Option<ForInitializer>,
        /// The loop condition, if any.
        condition: Option<Expr>,
        /// The iterator expressions run after each iteration.
        iterators: Vec<Expr>,
        /// The loop body.
        body: Box<Stmt>,
    },
    /// A `foreach ( type name in collection ) body` statement (15.8.4).
    ForEach {
        /// The iteration variable's type.
        ty: TypeRef,
        /// The iteration variable's name.
        name: Box<str>,
        /// The collection iterated over.
        collection: Expr,
        /// The loop body.
        body: Box<Stmt>,
    },
    /// A `break ;` statement (15.9.1).
    Break,
    /// A `continue ;` statement (15.9.2).
    Continue,
    /// A `throw expression_opt ;` statement (15.9.5).
    Throw(Option<Expr>),
    /// A `try` statement with catch clauses and/or a finally block (15.10).
    Try {
        /// The protected block.
        body: Box<Stmt>,
        /// The catch clauses, in order.
        catches: Vec<CatchClause>,
        /// The finally block, if present.
        finally_block: Option<Box<Stmt>>,
    },
    /// A `lock ( expression ) statement` (15.12).
    Lock {
        /// The object locked on.
        expression: Expr,
        /// The guarded statement.
        body: Box<Stmt>,
    },
    /// A `using ( resource ) statement` (15.13).
    Using {
        /// The resource acquired for the duration of the body.
        resource: UsingResource,
        /// The guarded statement.
        body: Box<Stmt>,
    },
    /// A `checked` block statement (15.11), forcing overflow checking on.
    Checked(Box<Stmt>),
    /// An `unchecked` block statement (15.11), forcing overflow checking off.
    Unchecked(Box<Stmt>),
    /// A `switch` statement (15.7.2).
    Switch {
        /// The value switched on.
        expression: Expr,
        /// The switch sections, in order.
        sections: Vec<SwitchSection>,
    },
    /// A labeled statement `label : statement` (15.4).
    Labeled {
        /// The label name.
        label: Box<str>,
        /// The labeled statement.
        statement: Box<Stmt>,
    },
    /// A `goto` statement (15.9.3).
    Goto(GotoTarget),
    /// A placeholder for a statement that could not be parsed, emitted with a
    /// diagnostic for recovery.
    Error,
}

/// One section of a `switch` statement (15.7.2): its labels and statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwitchSection {
    /// The `case`/`default` labels introducing the section.
    pub labels: Vec<SwitchLabel>,
    /// The statements run when a label matches.
    pub statements: Vec<Stmt>,
}

/// A `switch` label (15.7.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchLabel {
    /// `case constant-expression :`.
    Case(Expr),
    /// `default :`.
    Default,
}

/// The target of a `goto` statement (15.9.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GotoTarget {
    /// `goto label ;`.
    Label(Box<str>),
    /// `goto case constant-expression ;`.
    Case(Expr),
    /// `goto default ;`.
    Default,
}

/// One `catch` clause of a `try` statement (15.10).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatchClause {
    /// The caught exception type, or `None` for a general `catch`.
    pub exception_type: Option<TypeRef>,
    /// The bound exception variable's name, if any.
    pub name: Option<Box<str>>,
    /// The handler block.
    pub body: Box<Stmt>,
}

/// The resource of a `using` statement (15.13): a local declaration or an
/// expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsingResource {
    /// `type declarators`.
    Declaration {
        /// The declared type.
        ty: TypeRef,
        /// The declared variables.
        declarators: Vec<VariableDeclarator>,
    },
    /// An expression evaluating to the resource.
    Expression(Expr),
}

/// The initializer of a `for` statement (15.8.3): either a local variable
/// declaration or a list of statement expressions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForInitializer {
    /// `type declarators`.
    Declaration {
        /// The declared type.
        ty: TypeRef,
        /// The declared variables.
        declarators: Vec<VariableDeclarator>,
    },
    /// A comma-separated list of statement expressions.
    Expressions(Vec<Expr>),
}

/// One declared variable in a [`StmtKind::LocalDeclaration`] (15.5.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariableDeclarator {
    /// The variable's name.
    pub name: Box<str>,
    /// The initializer expression, if the declarator has one.
    pub initializer: Option<Expr>,
    /// The byte range the declarator covers.
    pub span: Span,
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
