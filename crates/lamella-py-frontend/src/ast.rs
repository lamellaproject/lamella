//! The abstract syntax tree for the Python subset.

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// A binary arithmetic or bitwise operator. The typed AOT lane lowers the integer forms; true
/// division (`/`), exponentiation (`**`), and any float operand stay dynamic (the interpreter).
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
    /// `/` -- true division (always a float result).
    TrueDiv,
    /// `**` -- exponentiation, right-associative (a non-negative integer exponent gives an integer).
    Pow,
    /// `@` -- matrix multiplication (`__matmul__`); no builtin numeric type implements it.
    MatMul,
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
    /// `in` -- a membership test.
    In,
    /// `not in` -- a negated membership test.
    NotIn,
    /// `is` -- an object-identity test.
    Is,
    /// `is not` -- a negated object-identity test.
    IsNot,
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
    /// A floating-point literal -- the `f64` value's raw bits (`to_bits`), so `Expr` stays `Eq`.
    /// A float literal, stored as its IEEE-754 f64 bit pattern. Heap-boxed in the interpreter; the
    /// typed AOT lane lowers it to a native `f64` (annotated or inferred `float`).
    Float(u64),
    /// An imaginary literal `Nj` -- the imaginary part's `f64` bits. Dynamic (a heap `complex`).
    Imaginary(u64),
    /// An integer literal too large for `i64` -- its decimal digits. Dynamic (an arbitrary-precision int).
    BigInt(String),
    /// A bytes literal `b"..."` -- its decoded bytes. Dynamic (a heap `bytes`).
    Bytes(Vec<u8>),
    /// A string literal (its decoded value). A dynamic value -- no typed AOT lowering.
    Str(String),
    /// A `True` or `False` literal.
    Bool(bool),
    /// The `None` literal.
    None,
    /// A bare name -- a local, a parameter, or a global/built-in.
    Name(String),
    /// Attribute access, `value.attr` -- the one dynamic operation in the typed subset.
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
    /// The in-place form of a binary operation, produced only by an augmented assignment `x OP= y`
    /// (never written directly). Like [`Expr::Binary`] but compiles to `InplaceBinOp`, which mutates
    /// a mutable target in place (`x += y` on a list extends it) rather than always building a new
    /// value. `lhs` is the current value of the target; the enclosing assignment stores the result.
    InplaceBinary {
        /// The operator.
        op: BinOp,
        /// The left operand (the target's current value).
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
    /// A logical negation, `not operand` -- always a boolean (`True`/`False`).
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
    /// A single comparison, `lhs <op> rhs`. Chained comparisons (`a < b < c`)
    /// are desugared by the parser into `and`-joined pairs.
    Compare {
        /// The operator.
        op: CmpOp,
        /// The left operand.
        lhs: Box<Expr>,
        /// The right operand.
        rhs: Box<Expr>,
    },
    /// A call, `func(args..., name=value...)`.
    Call {
        /// The callee expression.
        func: Box<Expr>,
        /// The positional arguments, in order.
        args: Vec<Expr>,
        /// The keyword arguments `name=value`, in source order. Empty for a purely
        /// positional call. (`*args` / `**kwargs` unpacking uses [`Expr::CallEx`].)
        keywords: Vec<Keyword>,
    },
    /// A call with `*args` / `**kwargs` unpacking, `f(a, *seq, k=1, **map)`. Kept separate from
    /// [`Expr::Call`] so the common (no-unpacking) call stays simple; the arguments are one ordered
    /// list of [`CallArg`], evaluated left to right. Always a dynamic operation.
    CallEx {
        /// The callee expression.
        func: Box<Expr>,
        /// The arguments -- positional, starred, keyword, double-starred -- in source order.
        args: Vec<CallArg>,
    },
    /// A subscript, `value[index]`. A dynamic operation (no typed lowering).
    Subscript {
        /// The container being indexed.
        value: Box<Expr>,
        /// The index expression (a plain expression, or a `Slice`).
        index: Box<Expr>,
    },
    /// A slice `[lower:upper:step]`, appearing only as a subscript index. Each bound is
    /// `None` when omitted (6.3.2.1).
    Slice {
        /// The lower bound (`None` if omitted).
        lower: Option<Box<Expr>>,
        /// The upper bound (`None` if omitted).
        upper: Option<Box<Expr>>,
        /// The step (`None` if omitted).
        step: Option<Box<Expr>>,
    },
    /// A list display `[a, b, c]`. A dynamic value (no typed lowering).
    List(Vec<Expr>),
    /// A tuple display `(a, b, c)` / `(a,)` / `()`. A dynamic value.
    Tuple(Vec<Expr>),
    /// A dict display `{k: v, ...}` -- key/value pairs in source order. A dynamic value.
    Dict(Vec<(Expr, Expr)>),
    /// A set display `{a, b, c}` -- elements in source order (deduped at runtime). A dynamic
    /// value. `{}` is an empty dict, not a set.
    Set(Vec<Expr>),
    /// A list comprehension `[element for target in iterable [if cond]]`.
    ListComp {
        /// The element expression, evaluated per innermost item.
        element: Box<Expr>,
        /// One or more `for ... [if ...]` clauses, outermost first.
        clauses: Vec<CompClause>,
    },
    /// A set comprehension `{element for target in iterable [if cond]}`.
    SetComp {
        /// The element expression.
        element: Box<Expr>,
        /// One or more `for ... [if ...]` clauses, outermost first.
        clauses: Vec<CompClause>,
    },
    /// A dict comprehension `{key: value for target in iterable [if cond]}`.
    DictComp {
        /// The key expression.
        key: Box<Expr>,
        /// The value expression.
        value: Box<Expr>,
        /// One or more `for ... [if ...]` clauses, outermost first.
        clauses: Vec<CompClause>,
    },
    /// A generator expression `(element for target in iterable [if cond])`. Materialized EAGERLY
    /// (compiled as a list comprehension) in this increment -- indistinguishable from a lazy
    /// generator when consumed immediately (`sum(x for x in xs)`, the common case); true lazy
    /// evaluation is unsupported.
    GeneratorExp {
        /// The element expression, evaluated per innermost item.
        element: Box<Expr>,
        /// One or more `for ... [if ...]` clauses, outermost first.
        clauses: Vec<CompClause>,
    },
    /// A lambda `lambda params: body` -- an anonymous function whose body is a single
    /// expression. Compiled by hoisting to a synthetic module function; capturing an
    /// enclosing function's local (a closure) is not yet supported.
    Lambda {
        /// The parameters, in order (no annotations; defaults allowed).
        params: Vec<ParamDef>,
        /// The body expression, evaluated and returned when the lambda is called.
        body: Box<Expr>,
    },
    /// A `yield` expression -- `yield` or `yield value` -- suspending a generator and producing
    /// `value` (or `None`). A `yield` anywhere in a function body makes that function a generator.
    /// The expression's own value is what a later `send` injects (`None` under `next`).
    Yield(Option<Box<Expr>>),
    /// A `yield from iterable` expression -- delegates to a sub-iterator: it re-yields each of the
    /// sub-iterator's values to this generator's caller (forwarding any sent value or thrown
    /// exception into the sub) until the sub is exhausted. The expression's own value is the sub's
    /// return value (its `StopIteration.value`). Like `yield`, it makes the enclosing function a
    /// generator.
    YieldFrom(Box<Expr>),
    /// `name := value` -- the walrus (assignment expression): binds `name` and yields `value`.
    Walrus {
        /// The bound name (the target is a bare name).
        target: String,
        /// The assigned value, which is also the expression's result.
        value: Box<Expr>,
    },
}

/// One keyword argument at a call site, `name=value`. (A `**mapping` unpack carries no name and
/// appears as [`CallArg::DoubleStar`] inside an [`Expr::CallEx`] instead.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keyword {
    /// The keyword name.
    pub name: String,
    /// The argument value.
    pub value: Expr,
}

/// One argument of an [`Expr::CallEx`] star-call, in source order. `*`/`**` unpacking makes the
/// argument count dynamic, so these are resolved at run time rather than folded into a fixed arity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallArg {
    /// A plain positional argument, `value`.
    Positional(Expr),
    /// An iterable unpacked into the positional arguments, `*value`.
    Star(Expr),
    /// A keyword argument, `name=value`.
    Keyword(String, Expr),
    /// A mapping unpacked into the keyword arguments, `**value`.
    DoubleStar(Expr),
}

/// One `for target(s) in iterable [if cond ...]` clause of a comprehension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompClause {
    /// The loop target(s): one name, or several for a tuple target `for k, v in ...`.
    pub targets: Vec<String>,
    /// The iterable.
    pub iterable: Expr,
    /// Zero or more `if` filters that follow this `for` (before the next `for`).
    pub conditions: Vec<Expr>,
}

/// A target of a tuple-unpacking assignment `a, b[i], c.x, (d, e) = seq`: a bare name, a subscript,
/// an attribute, or a nested tuple/list target (unpacked recursively).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssignTarget {
    /// `name` -- binds a local/global.
    Name(String),
    /// `container[index]` -- a subscript store.
    Subscript {
        /// The container.
        container: Expr,
        /// The index.
        index: Expr,
    },
    /// `obj.attr` -- an attribute store.
    Attribute {
        /// The object.
        obj: Expr,
        /// The attribute name.
        attr: String,
    },
    /// `(a, b)` or `[a, b]` -- a nested tuple/list target; its element sequence is unpacked
    /// recursively. (A starred target `*x` inside a nested tuple is unsupported.)
    Tuple(Vec<AssignTarget>),
}

impl AssignTarget {
    /// Append the names this target binds -- recursing into nested tuples -- to `out`. A subscript
    /// or attribute target binds no name (it stores into an existing object), so it appends nothing.
    pub fn collect_names<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            AssignTarget::Name(name) => out.push(name),
            AssignTarget::Tuple(elems) => {
                for elem in elems {
                    elem.collect_names(out);
                }
            }
            AssignTarget::Subscript { .. } | AssignTarget::Attribute { .. } => {}
        }
    }
}

/// A statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// A function definition.
    FuncDef(FuncDef),
    /// `return` with an optional value (a bare `return` yields `None`).
    Return(Option<Expr>),
    /// An assignment or an annotated assignment/declaration. The parser assigns
    /// only to a bare name.
    Assign(Assign),
    /// A multiple assignment, `a = b = value` -- the value is evaluated once and bound
    /// to every target (left to right).
    MultiAssign {
        /// The targets, in source order (two or more) -- names, subscripts, or attributes; the one
        /// value is bound to each (`a = b[i] = o.x = value`).
        targets: Vec<AssignTarget>,
        /// The value bound to each target.
        value: Expr,
    },
    /// Tuple-unpacking assignment `a, b = expr` -- the value is a sequence whose elements
    /// bind to the targets in order. Flat targets only (nested `(a, (b, c))` is post-cut).
    TupleAssign {
        /// The targets, in source order (names, subscripts, or attributes).
        targets: Vec<AssignTarget>,
        /// The index of the starred target `*name`, if any (`a, *b, c` -> `Some(1)`); the
        /// star binds a list of the leftover elements.
        star: Option<usize>,
        /// The sequence value to unpack.
        value: Expr,
    },
    /// A subscript assignment `container[index] = value` on a mutable container.
    SetItem {
        /// The container being mutated.
        container: Expr,
        /// The index.
        index: Expr,
        /// The value to store (the right-hand side; for an augmented store, the operand).
        value: Expr,
        /// For an augmented store (`c[i] += v`), the operator; `None` for a plain `c[i] = v`. The
        /// container and index are evaluated once, then combined with the current element.
        op: Option<BinOp>,
    },
    /// An attribute assignment `obj.attr = value` (e.g. `self.x = v`).
    SetAttr {
        /// The object whose attribute is set.
        obj: Expr,
        /// The attribute name.
        attr: String,
        /// The value to store (the right-hand side; for an augmented store, the operand).
        value: Expr,
        /// For an augmented store (`obj.x += v`), the operator; `None` for a plain `obj.x = v`.
        op: Option<BinOp>,
    },
    /// An expression evaluated for its effect; its value is discarded.
    Expr(Expr),
    /// `del target, ...` -- unbind each target. A bare name is supported (it unbinds the local);
    /// subscript / attribute targets (`del xs[i]`, `del o.x`) are unsupported (need interp ops).
    Delete(Vec<Expr>),
    /// `nonlocal name, ...` -- declare that these names bind to the nearest enclosing FUNCTION's
    /// variables (a shared cell), so an assignment in this function writes through rather than
    /// creating a new local. Each name must be bound in some enclosing function scope.
    Nonlocal(Vec<String>),
    /// `global name, ...` -- declare that these names bind the MODULE globals, so a read resolves the
    /// global and an assignment stores to the module namespace (not a frame local). Excludes each name
    /// from this function's locals / cells.
    Global(Vec<String>),
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
    /// A `while` loop.
    While {
        /// The condition, tested before each iteration.
        test: Expr,
        /// The loop body.
        body: Vec<Stmt>,
        /// The `else` clause, run if the loop exits normally (not via `break`).
        orelse: Vec<Stmt>,
    },
    /// A `for` loop over `range(...)` (the only iterable in this subset): the loop
    /// variable runs `start, start+step, ...`, stopping before `stop`. The range
    /// bounds are evaluated once.
    For {
        /// The loop variable, bound to each value in turn.
        target: String,
        /// The inclusive lower bound (`range`'s start; `0` for `range(stop)`).
        start: Expr,
        /// The exclusive upper bound (`range`'s stop).
        stop: Expr,
        /// The step (`range`'s step; `1` when omitted). Must be a non-zero integer literal.
        step: i64,
        /// The loop body.
        body: Vec<Stmt>,
        /// The `else` clause, run if the loop runs to completion (not via `break`).
        orelse: Vec<Stmt>,
    },
    /// A `for target in iterable:` over a general iterable (anything other than a
    /// `range(...)` call, which stays the counted `For` above) -- the iterator protocol.
    ForIter {
        /// The loop variable, bound to each item.
        target: String,
        /// The iterable expression.
        iterable: Expr,
        /// The loop body.
        body: Vec<Stmt>,
        /// The `else` clause, run if the loop runs to completion (not via `break`).
        orelse: Vec<Stmt>,
    },
    /// A `raise` statement. `raise exc` raises a value; a bare `raise` (`exc` is `None`)
    /// re-raises the exception currently being handled; `raise exc from cause` chains the
    /// cause onto the raised exception's `__cause__`.
    Raise {
        /// The exception to raise, or `None` for a bare re-raise.
        exc: Option<Expr>,
        /// The explicit cause (`raise exc from cause`), or `None`. `raise exc from None`
        /// carries `cause` as the `None` literal (to suppress implicit chaining).
        cause: Option<Expr>,
    },
    /// A `try` statement with `except` handlers and an optional `else`. (A `finally`
    /// clause parses into `finalbody`.)
    Try {
        /// The protected body.
        body: Vec<Stmt>,
        /// The `except` handlers, in source order (a bare `except:`, if present, is last).
        handlers: Vec<ExceptHandler>,
        /// The `else` clause -- run when the body completes with no exception; empty if absent.
        orelse: Vec<Stmt>,
        /// The `finally` clause; empty if absent.
        finalbody: Vec<Stmt>,
    },
    /// A `with context [as name]:` statement (a single context manager). Desugars at compile time
    /// to the `__enter__` / `try` / `finally` / `__exit__` protocol.
    With {
        /// The context-manager expression: evaluated once; its `__enter__` runs on entry and
        /// `__exit__` on exit (including on exception).
        context: Expr,
        /// The optional `as name` target, bound to `__enter__`'s result.
        optional_name: Option<String>,
        /// The `with` body.
        body: Vec<Stmt>,
    },
    /// A `class Name [(Base, ...)]:` definition. The body holds method definitions (`FuncDef`)
    /// and class-attribute assignments (`Assign`).
    ClassDef {
        /// The class name.
        name: String,
        /// The base-class expressions, in order (empty inherits `object`); more than one is
        /// multiple inheritance, resolved by the runtime's C3 MRO.
        bases: Vec<Expr>,
        /// The class body.
        body: Vec<Stmt>,
    },
    /// A decorated function or class -- one or more `@decorator` lines above a `def`/`class`.
    /// Desugars to `name = d0(d1(... dn(name) ...))`: the decorators wrap the freshly defined name,
    /// applied bottom-up (the decorator nearest the `def` first).
    Decorated {
        /// The decorator expressions, in source (top-to-bottom) order.
        decorators: Vec<Expr>,
        /// The decorated statement -- a [`Stmt::FuncDef`] or [`Stmt::ClassDef`].
        inner: Box<Stmt>,
    },
    /// `import module [as alias], ...` -- import one or more modules, binding each to its `as`
    /// alias or the module name. Simple (undotted) module names only in this subset.
    Import {
        /// Each `(module_name, bound_name)`.
        modules: Vec<(String, String)>,
    },
    /// `from module import name [as alias], ...` -- bind members off a module.
    ImportFrom {
        /// The module imported from (a simple name).
        module: String,
        /// Each `(member_name, bound_name)`.
        names: Vec<(String, String)>,
    },
    /// `from module import *` -- bind every public name of a module into the current module's
    /// namespace (its `__all__` if defined, else every name not starting with `_`). The bound names
    /// are not known until run time, so they resolve as globals. Only valid at module level.
    ImportStar {
        /// The module imported from (a simple name).
        module: String,
    },
    /// `break` -- exit the innermost enclosing loop.
    Break,
    /// `continue` -- skip to the next iteration of the innermost enclosing loop.
    Continue,
    /// `pass` -- a no-op statement (a placeholder where a statement is required).
    Pass,
}

/// An assignment statement: a target name, an optional annotation, and an optional
/// value. The three forms are `name = value`, `name: ann = value`, and the bare
/// declaration `name: ann` (which records a type but stores nothing).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assign {
    /// The target name (assignments target only a bare name in this subset).
    pub target: String,
    /// The annotation expression, if the assignment is annotated.
    pub annotation: Option<Expr>,
    /// The value to store, or `None` for a bare annotated declaration.
    pub value: Option<Expr>,
}

/// One `except [type [as name]]:` clause of a [`Stmt::Try`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExceptHandler {
    /// The exception type to match, or `None` for a bare `except:` (catches anything).
    pub typ: Option<Expr>,
    /// The name the caught exception is bound to (`as name`), if any.
    pub name: Option<String>,
    /// The handler body.
    pub body: Vec<Stmt>,
}

/// A function parameter declaration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParamDef {
    /// The parameter name.
    pub name: String,
    /// The parameter's annotation expression, if any.
    pub annotation: Option<Expr>,
    /// The parameter's default value expression, if any (`b=1`). Evaluated at
    /// def-execution time in the defining scope (Python semantics).
    pub default: Option<Expr>,
    /// Whether this parameter is keyword-only -- it follows a bare `*` in the list, so it binds
    /// only by name, never positionally.
    pub keyword_only: bool,
    /// Whether this parameter is positional-only -- it precedes a `/` in the list, so it binds
    /// only by position, never by name.
    pub positional_only: bool,
    /// Whether this is the `*args` parameter -- it collects surplus positional arguments into a
    /// tuple. At most one per parameter list; the params after it are keyword-only.
    pub is_vararg: bool,
    /// Whether this is the `**kwargs` parameter -- it collects unmatched keyword arguments into a
    /// dict. At most one per parameter list, and it must be last.
    pub is_varkwarg: bool,
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
