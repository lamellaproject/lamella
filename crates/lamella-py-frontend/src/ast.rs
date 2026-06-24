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
