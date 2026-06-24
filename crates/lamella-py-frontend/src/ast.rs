//! The abstract syntax tree for the first-light Python subset.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// A binary arithmetic operator (the `+ - * // %` of the subset). True division
/// (`/`) is intentionally absent -- it produces a float, which is out of first
/// light.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    /// `+`
    Add,
    /// `-`
    Sub,
    /// `*`
    Mul,
    /// `//`
    FloorDiv,
    /// `%`
    Mod,
    /// `&`
    BitAnd,
    /// `|`
    BitOr,
    /// `^`
    BitXor,
    /// `<<`
    LShift,
    /// `>>`
    RShift,
}

/// A comparison operator (`== != < <= > >=`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// A unary operator (`- + ~`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    /// `-a` -- arithmetic negation.
    Neg,
    /// `+a` -- unary plus (identity for ints).
    Pos,
    /// `~a` -- bitwise inversion.
    Invert,
}

/// A short-circuiting boolean operator (`and`, `or`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoolOp {
    /// `a and b` -- evaluates `b` only if `a` is truthy.
    And,
    /// `a or b` -- evaluates `b` only if `a` is falsey.
    Or,
}

/// An expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// An integer literal (already folded to its signed value, so a source `-3`
    /// arrives here as `Int(-3)`).
    Int(i64),
    /// A `True` or `False` literal.
    Bool(bool),
    /// The `None` literal.
    None,
    /// A bare name -- a local, a parameter, or a global/built-in.
    Name(String),
    /// Attribute access, `value.attr` -- the one dynamic operation in first light.
    Attribute {
        /// The object whose attribute is read.
        value: Box<Expr>,
        /// The attribute name.
        attr: String,
    },
    /// A binary arithmetic expression, `lhs <op> rhs`.
    Binary {
        /// The operator.
        op: BinOp,
        /// The left operand.
        lhs: Box<Expr>,
        /// The right operand.
        rhs: Box<Expr>,
    },
    /// A unary expression, `<op> operand`.
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        operand: Box<Expr>,
    },
    /// A short-circuiting boolean expression, `lhs and rhs` / `lhs or rhs`. The result
    /// is one of the operand values (not coerced to a bool), per Python.
    BoolBinary {
        /// The operator.
        op: BoolOp,
        /// The left operand (always evaluated).
        lhs: Box<Expr>,
        /// The right operand (evaluated only when the operator does not short-circuit).
        rhs: Box<Expr>,
    },
    /// A logical negation, `not operand` -- always a boolean (`0`/`1`).
    Not {
        /// The operand whose truthiness is negated.
        operand: Box<Expr>,
    },
    /// A conditional expression, `body if test else orelse` -- evaluates and yields
    /// `body` when `test` is truthy, otherwise `orelse`.
    Conditional {
        /// The condition.
        test: Box<Expr>,
        /// The value when the condition is truthy.
        body: Box<Expr>,
        /// The value when the condition is falsey.
        orelse: Box<Expr>,
    },
    /// A single comparison, `lhs <op> rhs`. First light does not chain
    /// comparisons (`a < b < c`).
    Compare {
        /// The operator.
        op: CmpOp,
        /// The left operand.
        lhs: Box<Expr>,
        /// The right operand.
        rhs: Box<Expr>,
    },
    /// A call, `func(args...)`.
    Call {
        /// The callee expression.
        func: Box<Expr>,
        /// The positional arguments, in order.
        args: Vec<Expr>,
    },
}

/// A statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// A function definition.
    FuncDef(FuncDef),
    /// `return` with an optional value (a bare `return` yields `None`).
    Return(Option<Expr>),
    /// An assignment or an annotated assignment/declaration. First light assigns
    /// only to a bare name.
    Assign(Assign),
    /// An expression evaluated for its effect; its value is discarded.
    Expr(Expr),
    /// An `if`/`elif`/`else`. Each `elif` is desugared by the parser into a
    /// nested `If` in the preceding clause's `orelse`.
    If {
        /// The condition.
        test: Expr,
        /// The body run when `test` is truthy.
        body: Vec<Stmt>,
        /// The `else` body (empty when there is no `else`).
        orelse: Vec<Stmt>,
    },
    /// A `while` loop (no `else` clause in the first-light subset).
    While {
        /// The condition, tested before each iteration.
        test: Expr,
        /// The loop body.
        body: Vec<Stmt>,
    },
    /// A `for` loop over `range(...)` (first light's only iterable): the loop variable
    /// runs `start, start+1, ..., stop-1`. `start` and `stop` are evaluated once.
    For {
        /// The loop variable, bound to each value in turn.
        target: String,
        /// The inclusive lower bound (`range`'s start; `0` for `range(stop)`).
        start: Expr,
        /// The exclusive upper bound (`range`'s stop).
        stop: Expr,
        /// The loop body.
        body: Vec<Stmt>,
    },
}

/// An assignment statement: a target name, an optional annotation, and an optional
/// value. The three first-light forms are `name = value`, `name: ann = value`,
/// and the bare declaration `name: ann` (which records a type but stores nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assign {
    /// The target name (first light assigns only to a bare name).
    pub target: String,
    /// The annotation expression, if the assignment is annotated.
    pub annotation: Option<Expr>,
    /// The value to store, or `None` for a bare annotated declaration.
    pub value: Option<Expr>,
}

/// A function parameter declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamDef {
    /// The parameter name.
    pub name: String,
    /// The parameter's annotation expression, if any.
    pub annotation: Option<Expr>,
}

/// A function definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncDef {
    /// The function name.
    pub name: String,
    /// The parameters, in order.
    pub params: Vec<ParamDef>,
    /// The return annotation expression, if any.
    pub ret: Option<Expr>,
    /// The function body.
    pub body: Vec<Stmt>,
}

/// A whole module: its top-level statements in source order (function definitions
/// among them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleAst {
    /// The top-level statements.
    pub body: Vec<Stmt>,
}
