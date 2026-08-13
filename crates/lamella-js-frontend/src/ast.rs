//! The syntax tree.

use crate::{Box, String, Vec};
use crate::source::Span;
use crate::string_value::JsString;

/// A parsed program.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    pub body: Vec<Statement>,
    /// Whether a `"use strict"` directive prologue made this program strict.
    pub strict: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Expression { expression: Expression, span: Span },
    Block { body: Vec<Statement>, span: Span },
    Empty { span: Span },
    /// `var` / `let` / `const`.
    Declaration { kind: DeclarationKind, declarations: Vec<Declarator>, span: Span },
    If { test: Expression, consequent: Box<Statement>, alternate: Option<Box<Statement>>, span: Span },
    While { test: Expression, body: Box<Statement>, span: Span },
    DoWhile { body: Box<Statement>, test: Expression, span: Span },
    /// A C-style `for`. The init clause is parsed with `[In]` withdrawn -- see [`crate::context`].
    For {
        init: Option<Box<ForInit>>,
        test: Option<Expression>,
        update: Option<Expression>,
        body: Box<Statement>,
        span: Span,
    },
    ForIn { left: Box<ForInit>, right: Expression, body: Box<Statement>, span: Span },
    ForOf { left: Box<ForInit>, right: Expression, body: Box<Statement>, span: Span },
    Return { argument: Option<Expression>, span: Span },
    Break { label: Option<String>, span: Span },
    Continue { label: Option<String>, span: Span },
    Throw { argument: Expression, span: Span },
    Try { block: Vec<Statement>, handler: Option<CatchClause>, finalizer: Option<Vec<Statement>>, span: Span },
    Labeled { label: String, body: Box<Statement>, span: Span },
    /// `with (object) body`, which runs `body` under an OBJECT environment record.
    ///
    /// A SyntaxError in strict mode, so this node only ever exists in sloppy code -- the parser
    /// reports the early error and still builds the node, because one refusal beside a parsed
    /// construct reads better than a trail of complaints about its body.
    With { object: Expression, body: Box<Statement>, span: Span },
    Switch { discriminant: Expression, cases: Vec<SwitchCase>, span: Span },
    Function(Box<Function>),
    Class(Box<Class>),
    Debugger { span: Span },
}

impl Statement {
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            Statement::Expression { span, .. }
            | Statement::Block { span, .. }
            | Statement::Empty { span }
            | Statement::Declaration { span, .. }
            | Statement::If { span, .. }
            | Statement::While { span, .. }
            | Statement::DoWhile { span, .. }
            | Statement::For { span, .. }
            | Statement::ForIn { span, .. }
            | Statement::ForOf { span, .. }
            | Statement::Return { span, .. }
            | Statement::Break { span, .. }
            | Statement::Continue { span, .. }
            | Statement::Throw { span, .. }
            | Statement::Try { span, .. }
            | Statement::Labeled { span, .. }
            | Statement::With { span, .. }
            | Statement::Switch { span, .. }
            | Statement::Debugger { span } => *span,
            Statement::Function(function) => function.span,
            Statement::Class(class) => class.span,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclarationKind {
    Var,
    Let,
    Const,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Declarator {
    pub target: Pattern,
    pub init: Option<Expression>,
    pub span: Span,
}

/// What sits to the left of a `for` header's `in` / `of`, or the init clause of a C-style `for`.
#[derive(Debug, Clone, PartialEq)]
pub enum ForInit {
    Declaration { kind: DeclarationKind, declarations: Vec<Declarator>, span: Span },
    Expression(Expression),
    Pattern(Pattern),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatchClause {
    /// `None` for the optional-catch-binding form, `catch { }`.
    pub param: Option<Pattern>,
    pub body: Vec<Statement>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SwitchCase {
    /// `None` for `default:`.
    pub test: Option<Expression>,
    pub body: Vec<Statement>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    pub name: Option<String>,
    pub params: Vec<Pattern>,
    pub body: Vec<Statement>,
    pub is_generator: bool,
    pub is_async: bool,
    /// Whether the body's own directive prologue made it strict.
    pub strict: bool,
    pub span: Span,
}

/// A binding or assignment target.
///
/// The pattern forms an array or object literal REFINES into. Kept as its own type rather than
/// reusing [`Expression`], so that a refinement failure is a type-level event a caller must handle
/// rather than a silently-accepted expression in a binding position.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    Identifier { name: String, span: Span },
    /// A member expression, legal as an assignment target and never as a binding target.
    Member { object: Box<Expression>, property: Box<MemberProperty>, optional: bool, span: Span },
    Array { elements: Vec<Option<Pattern>>, rest: Option<Box<Pattern>>, span: Span },
    Object { properties: Vec<ObjectPatternProperty>, rest: Option<Box<Pattern>>, span: Span },
    /// `a = 1` in a binding position: a default.
    Default { target: Box<Pattern>, value: Box<Expression>, span: Span },
    Rest { argument: Box<Pattern>, span: Span },
}

impl Pattern {
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            Pattern::Identifier { span, .. }
            | Pattern::Member { span, .. }
            | Pattern::Array { span, .. }
            | Pattern::Object { span, .. }
            | Pattern::Default { span, .. }
            | Pattern::Rest { span, .. } => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectPatternProperty {
    pub key: PropertyKey,
    pub value: Pattern,
    pub computed: bool,
    pub shorthand: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PropertyKey {
    Identifier { name: String, span: Span },
    String { value: JsString, span: Span },
    Number { value: f64, span: Span },
    Computed { expression: Box<Expression>, span: Span },
    Private { name: String, span: Span },
}

#[derive(Debug, Clone, PartialEq)]
pub enum MemberProperty {
    Identifier { name: String, span: Span },
    Computed { expression: Expression, span: Span },
    Private { name: String, span: Span },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expression {
    Identifier { name: String, span: Span },
    Number { value: f64, span: Span },
    String { value: JsString, span: Span },
    Boolean { value: bool, span: Span },
    Null { span: Span },
    This { span: Span },
    /// A template literal. `quasis` always has one more element than `expressions`.
    Template { quasis: Vec<TemplateElement>, expressions: Vec<Expression>, span: Span },
    Tagged { tag: Box<Expression>, quasi: Box<Expression>, span: Span },
    RegExp { body: String, flags: String, span: Span },
    /// An array literal. **`None` is a HOLE, not a missing element** -- `[,,]` and `[undefined]`
    /// are different programs and the elisions decide the length.
    Array { elements: Vec<ArrayElement>, span: Span },
    Object { properties: Vec<ObjectProperty>, span: Span },
    Function(Box<Function>),
    Arrow(Box<Arrow>),
    Class(Box<Class>),
    /// `super`, which is only meaningful as `super(...)` or `super.x` and never on its own.
    Super { span: Span },
    /// `new.target`, the meta-property.
    ///
    /// **IT IS NOT AN IDENTIFIER, AND A NODE OF ITS OWN IS THE WHOLE POINT.** Spelling it as
    /// `Identifier { name: "new.target" }` reads the VALUE back exactly -- no source text can spell
    /// that name, so nothing shadows it -- and answers every rule that asks what an operand *is*
    /// wrong: `new.target = 1`, `new.target += 1`, `new.target++`, `--new.target` and
    /// `for (new.target in o)` all become legal, because `AssignmentTargetType` of an
    /// `IdentifierReference` is `simple` where `NewTarget`'s is `invalid`; `typeof new.target`
    /// answers `"undefined"` even inside a constructor, because `typeof` on an unresolvable NAME is
    /// that string; and `delete new.target` becomes a strict-mode syntax error, because that rule is
    /// about `PrimaryExpression : IdentifierReference`.
    ///
    /// Five rules, each correctly implemented, all answering about the type the node is wearing.
    NewTarget { span: Span },
    Unary { operator: UnaryOperator, argument: Box<Expression>, span: Span },
    Update { operator: UpdateOperator, prefix: bool, argument: Box<Expression>, span: Span },
    Binary { operator: BinaryOperator, left: Box<Expression>, right: Box<Expression>, span: Span },
    Logical { operator: LogicalOperator, left: Box<Expression>, right: Box<Expression>, span: Span },
    Assignment { operator: AssignmentOperator, target: Box<AssignmentTarget>, value: Box<Expression>, span: Span },
    Conditional { test: Box<Expression>, consequent: Box<Expression>, alternate: Box<Expression>, span: Span },
    Call { callee: Box<Expression>, arguments: Vec<Argument>, optional: bool, span: Span },
    New { callee: Box<Expression>, arguments: Vec<Argument>, span: Span },
    Member { object: Box<Expression>, property: Box<MemberProperty>, optional: bool, span: Span },
    Sequence { expressions: Vec<Expression>, span: Span },
    /// `yield`, `yield x`, `yield* xs`.
    ///
    /// # IT DOES NOT SURVIVE `parse_script`, AND THAT IS WHY THE FORMAT DOES NOT MOVE
    ///
    /// A generator body is rewritten into a state machine before the tree leaves the parser, so a
    /// `Yield` is a node the TRANSFORM consumes rather than one the encoder ever writes. There is
    /// no tag for it, no artifact carries one, and every consumer of the format is unaffected --
    /// which is the whole point of desugaring rather than teaching the walker to suspend.
    ///
    /// The encoder therefore treats one as an engine defect rather than as an unsupported node:
    /// reaching it means the transform missed a shape it was supposed to rewrite, and that has to
    /// be loud rather than published as a gap this profile chose.
    Yield { argument: Option<Box<Expression>>, delegate: bool, span: Span },
    /// A parenthesized expression, **kept in the tree rather than unwrapped**.
    ///
    /// It is not redundant: `(a) = 1` and `a = 1` differ in legality in some positions, and the
    /// cover grammar needs to know a head was parenthesized to refine it into arrow parameters.
    Parenthesized { expression: Box<Expression>, span: Span },
}

impl Expression {
    #[must_use]
    pub fn span(&self) -> Span {
        match self {
            Expression::Identifier { span, .. }
            | Expression::Number { span, .. }
            | Expression::String { span, .. }
            | Expression::Boolean { span, .. }
            | Expression::Null { span }
            | Expression::This { span }
            | Expression::Template { span, .. }
            | Expression::Tagged { span, .. }
            | Expression::RegExp { span, .. }
            | Expression::Array { span, .. }
            | Expression::Object { span, .. }
            | Expression::Unary { span, .. }
            | Expression::Update { span, .. }
            | Expression::Binary { span, .. }
            | Expression::Logical { span, .. }
            | Expression::Assignment { span, .. }
            | Expression::Conditional { span, .. }
            | Expression::Call { span, .. }
            | Expression::New { span, .. }
            | Expression::Member { span, .. }
            | Expression::Sequence { span, .. }
            | Expression::Yield { span, .. }
            | Expression::Parenthesized { span, .. } => *span,
            Expression::Function(function) => function.span,
            Expression::Arrow(arrow) => arrow.span,
            Expression::Class(class) => class.span,
            Expression::Super { span } => *span,
            Expression::NewTarget { span } => *span,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Arrow {
    pub params: Vec<Pattern>,
    pub body: ArrowBody,
    pub is_async: bool,
    pub strict: bool,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrowBody {
    Expression(Box<Expression>),
    Block(Vec<Statement>),
}

/// One element position in an array literal.
#[derive(Debug, Clone, PartialEq)]
pub enum ArrayElement {
    /// An elision. **Preserved**, because `[,,].length` is 2 and `[undefined, undefined]` is a
    /// different program with different `in` behaviour.
    Hole,
    Expression(Expression),
    /// `comma_after` IS CARRIED BECAUSE A COVER NODE MUST BE LOSSLESS. `[...a]` and `[...a,]` are
    /// the SAME array expression -- a trailing comma in a literal means nothing -- and they are
    /// DIFFERENT patterns: `[...a,] = []` is a SyntaxError where `[...a] = []` is legal. A node that
    /// discards the comma cannot decide the pattern reading later, so the rule silently accepts.
    /// **This is the one bit of an array literal whose meaning depends on which reading wins**, and
    /// it is exactly the property the cover-grammar design exists to preserve.
    Spread { argument: Expression, span: Span, comma_after: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ObjectProperty {
    Property {
        key: PropertyKey,
        value: Expression,
        computed: bool,
        shorthand: bool,
        span: Span,
    },
    /// `{a = 1}` -- a `CoverInitializedName`.
    ///
    /// **An EARLY ERROR when this object is an expression, and perfectly legal when it refines
    /// into a pattern.** It has to survive the first parse for the refinement to be possible at all,
    /// which is the clearest example of why the tree must not decide early.
    CoverInitializedName { name: String, value: Expression, span: Span },
    Method { key: PropertyKey, function: Box<Function>, kind: MethodKind, computed: bool, span: Span },
    Spread { argument: Expression, span: Span },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodKind {
    Normal,
    Get,
    Set,
}

/// A `class`, whether it was written as a declaration or an expression.
///
/// **THE TWO FORMS DIFFER ONLY IN WHERE THE BINDING GOES**, so they share a node. A declaration
/// binds its name in the enclosing scope; an expression binds it only INSIDE its own body, so a
/// class expression can refer to itself by name without leaking one.
#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    pub name: Option<String>,
    /// The `extends` clause, which is an expression rather than a name -- `class C extends f()` is
    /// legal, and the value it produces is the parent.
    pub heritage: Option<Box<Expression>>,
    pub members: Vec<ClassMember>,
    pub span: Span,
}

/// One member of a class body.
///
/// **A CLASS BODY IS NOT AN OBJECT LITERAL, and the differences are all invisible in the syntax.**
/// Its methods are NON-ENUMERABLE where an object literal's are enumerable; its body is always
/// strict whatever surrounds it; and a member named `constructor` is not a method at all but the
/// class's own callable.
#[derive(Debug, Clone, PartialEq)]
pub struct ClassMember {
    pub key: PropertyKey,
    pub kind: MethodKind,
    /// Attached to the CONSTRUCTOR rather than to the prototype.
    pub is_static: bool,
    pub computed: bool,
    pub function: Box<Function>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Argument {
    Expression(Expression),
    Spread { argument: Expression, span: Span },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TemplateElement {
    /// `None` when the chunk held an invalid escape: legal in a tagged template, an error otherwise.
    pub cooked: Option<JsString>,
    pub raw: String,
    pub span: Span,
}

/// The left-hand side of an assignment, after refinement.
#[derive(Debug, Clone, PartialEq)]
pub enum AssignmentTarget {
    /// `parenthesized` IS NOT COSMETIC. `IsIdentifierRef` is false for a parenthesized
    /// left-hand side, and `NamedEvaluation` asks it: `f = function () {}` names the function
    /// `"f"` and `(f) = function () {}` leaves the name empty. The parentheses are otherwise
    /// transparent -- both assign to the same binding -- so the parser computed this bit to
    /// decide what may be refined and then dropped it, and the one consumer that needs it
    /// afterwards could not tell the two apart.
    Pattern { pattern: Pattern, parenthesized: bool },
    /// A target that failed refinement. Kept rather than dropped so the parser can report the
    /// original span and continue, instead of unwinding out of the expression.
    Invalid(Expression),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Minus,
    Plus,
    Not,
    BitwiseNot,
    TypeOf,
    Void,
    Delete,
    Await,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateOperator {
    Increment,
    Decrement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    Add, Subtract, Multiply, Divide, Remainder, Exponent,
    Equal, NotEqual, StrictEqual, StrictNotEqual,
    Less, LessEqual, Greater, GreaterEqual,
    ShiftLeft, ShiftRight, UnsignedShiftRight,
    BitwiseAnd, BitwiseOr, BitwiseXor,
    In, InstanceOf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalOperator {
    And,
    Or,
    /// `??` -- and the grammar forbids mixing it with `&&`/`||` without parentheses.
    NullishCoalescing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentOperator {
    Assign,
    AddAssign, SubtractAssign, MultiplyAssign, DivideAssign, RemainderAssign, ExponentAssign,
    ShiftLeftAssign, ShiftRightAssign, UnsignedShiftRightAssign,
    BitwiseAndAssign, BitwiseOrAssign, BitwiseXorAssign,
    LogicalAndAssign, LogicalOrAssign, NullishAssign,
}
