//! A recursive-descent parser for the Python subset.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use crate::ast::{
    Assign, AssignTarget, BinOp, BoolOp, CallArg, CmpOp, CompClause, ExceptHandler, Expr, FuncDef,
    Keyword, ModuleAst, ParamDef, Stmt, UnaryOp,
};
use crate::lexer::{FStringPart, Tok, Token};

/// Whether an expression is a `range(...)` call -- the counted-loop form of `for`.
fn is_range_call(iter: &Expr) -> bool {
    matches!(iter, Expr::Call { func, .. } if matches!(&**func, Expr::Name(n) if n == "range"))
}

/// The binary operator of an augmented-assignment token (`+=`, `<<=`, ...), or `None`
/// if the token is not one.
fn aug_assign_op(tok: &Tok) -> Option<BinOp> {
    Some(match tok {
        Tok::PlusEq => BinOp::Add,
        Tok::MinusEq => BinOp::Sub,
        Tok::StarEq => BinOp::Mul,
        Tok::DoubleStarEq => BinOp::Pow,
        Tok::SlashEq => BinOp::TrueDiv,
        Tok::SlashSlashEq => BinOp::FloorDiv,
        Tok::PercentEq => BinOp::Mod,
        Tok::AmperEq => BinOp::BitAnd,
        Tok::PipeEq => BinOp::BitOr,
        Tok::CaretEq => BinOp::BitXor,
        Tok::LtLtEq => BinOp::LShift,
        Tok::GtGtEq => BinOp::RShift,
        Tok::AtEq => BinOp::MatMul,
        _ => return None,
    })
}

/// A parse failure: the offending line and a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// The 1-based source line the error was detected on.
    pub line: u32,
    /// What went wrong.
    pub message: String,
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

/// Parse a token stream (ending in [`Tok::Eof`]) into a module AST.
pub fn parse(tokens: Vec<Token>) -> Result<ModuleAst, ParseError> {
    let mut parser = Parser { tokens, pos: 0, temp_seq: 0, line_ended: true };
    parser.parse_module()
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    /// A monotonic counter for synthetic temp names (e.g. a `match` subject), so nested desugarings
    /// never collide.
    temp_seq: usize,
    /// Whether the most recent simple statement ended the line (a real newline) rather than a `;`
    /// separator. An inline suite (`if c: a; b`) reads simple statements until the line ends.
    line_ended: bool,
}

/// A `case` pattern in the supported match subset. Structural (sequence / class / mapping) patterns
/// are out of the subset; every supported pattern reduces to an equality test or an unconditional
/// capture, so a whole match desugars to a nested `if` tree over the subject temp.
enum MatchPattern {
    /// `_` -- matches anything, binds nothing.
    Wildcard,
    /// a bare name -- matches anything, binds the name to the subject.
    Capture(String),
    /// a literal or dotted value (`1`, `"a"`, `None`, `Color.RED`) -- matches when subject == value.
    Value(Expr),
    /// `p1 | p2 | ...` -- matches when any alternative sub-pattern matches. Every alternative must
    /// bind the same set of names (CPython's rule); the whole pattern binds those names from whichever
    /// alternative matched. A non-binding OR (values, bare classes, non-capturing sequences, wildcards)
    /// binds nothing, the common case.
    Or(Vec<MatchPattern>),
    /// `(p0, p1, ...)` / `[p0, ...]` -- a sequence pattern: matches a list/tuple whose elements match
    /// the sub-patterns (each a capture / wildcard / value / nested sequence / class). With
    /// `star = None` the length is fixed; with `star = Some(i)`, `elems[i]` is a `*name` (or `*_`)
    /// that binds the SLICE of surplus items and the length becomes a lower bound -- so `[x, *rest, y]`
    /// matches any 2+-element sequence.
    Sequence { elems: Vec<MatchPattern>, star: Option<usize> },
    /// `Cls(p0, ..., attr=subpat, ...)` / `Cls()` -- a class pattern. Positional sub-patterns match
    /// the values `__match_class__(subject, cls, k)` extracts (each `p_i` against element `i` of the
    /// returned tuple -- from the class's `__match_args__`, or the whole subject for a self-match
    /// builtin like `int`); keyword sub-patterns match the named attributes (`isinstance` + `hasattr`,
    /// a missing attribute a non-match per CPython's swallowed `AttributeError`). `temp` names the
    /// walrus temp that holds the `__match_class__` result; it is present iff there are positional
    /// sub-patterns.
    Class {
        cls: Expr,
        positional: Vec<MatchPattern>,
        keywords: Vec<(String, MatchPattern)>,
        temp: Option<String>,
    },
    /// `pattern as name` -- matches when `pattern` matches, and additionally binds `name` to the
    /// subject (the whole value matched at this level). `as _` is rejected (a wildcard target).
    As {
        pattern: Box<MatchPattern>,
        name: String,
    },
    /// `{key: subpat, ..., **rest}` -- a mapping pattern: matches a `dict` (subset match -- extra keys
    /// are ignored) that CONTAINS each `key` (an evaluated literal / value expression), whose value
    /// matches the sub-pattern. A missing key is a non-match. `**rest` captures the remaining items as
    /// a new dict. `items` are the `(key, sub-pattern)` pairs in order.
    Mapping {
        items: Vec<(Expr, MatchPattern)>,
        rest: Option<String>,
    },
}

/// `lhs and rhs` -- a short-circuiting conjunction (used to chain a pattern's tests).
fn bool_and(lhs: Expr, rhs: Expr) -> Expr {
    Expr::BoolBinary {
        op: BoolOp::And,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

/// `lhs or rhs` -- a short-circuiting disjunction.
fn bool_or(lhs: Expr, rhs: Expr) -> Expr {
    Expr::BoolBinary {
        op: BoolOp::Or,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

/// `type(subject) is list or type(subject) is tuple` -- the "is a matchable sequence" test for a
/// sequence pattern. Exact `list`/`tuple` only (so `str`/`dict`/other are correctly excluded); a
/// subclass or a registered `Sequence` is a documented narrowing of CPython's Sequence-ABC rule.
fn is_sequence(subject: &Expr) -> Expr {
    let type_is = |ty: &str| Expr::Compare {
        op: CmpOp::Is,
        lhs: Box::new(call1("type", subject.clone())),
        rhs: Box::new(Expr::Name(String::from(ty))),
    };
    bool_or(type_is("list"), type_is("tuple"))
}

/// `subject[i]` -- an element of the subject by position.
fn index_at(subject: &Expr, i: usize) -> Expr {
    Expr::Subscript {
        value: Box::new(subject.clone()),
        index: Box::new(Expr::Int(i as i64)),
    }
}

/// `subject[-k]` -- a from-the-end element access, for a sequence element AFTER a `*` star pattern.
fn neg_index_at(subject: &Expr, k: usize) -> Expr {
    Expr::Subscript {
        value: Box::new(subject.clone()),
        index: Box::new(Expr::Int(-(k as i64))),
    }
}

/// `subject[lower:upper]` -- the surplus slice a `*` star pattern binds. `upper` is `None` for an
/// open end (`subject[lower:]`, no elements after the star) or a negative literal
/// (`subject[lower:-post]`).
fn slice_from(subject: &Expr, lower: usize, upper: Option<i64>) -> Expr {
    Expr::Subscript {
        value: Box::new(subject.clone()),
        index: Box::new(Expr::Slice {
            lower: Some(Box::new(Expr::Int(lower as i64))),
            upper: upper.map(|u| Box::new(Expr::Int(u))),
            step: None,
        }),
    }
}

/// The subject access for the sequence-pattern element at `elems` index `i`: a positive index
/// before the `*` star (or when there is none), a negative (from-the-end) index after it.
fn seq_elem(subject: &Expr, i: usize, elems_len: usize, star: Option<usize>) -> Expr {
    match star {
        Some(s) if i > s => neg_index_at(subject, elems_len - i),
        _ => index_at(subject, i),
    }
}

/// `subject.attr` -- an attribute of the subject, for a class-pattern keyword sub-pattern.
fn attr_of(subject: &Expr, attr: &str) -> Expr {
    Expr::Attribute {
        value: Box::new(subject.clone()),
        attr: String::from(attr),
    }
}

/// `isinstance(subject, cls)` -- a class pattern's type test.
fn isinstance_of(subject: &Expr, cls: &Expr) -> Expr {
    call2("isinstance", subject.clone(), cls.clone())
}

/// `hasattr(subject, "attr")` -- guards a class keyword sub-pattern so a missing attribute is a
/// non-match (CPython swallows the `AttributeError`), not an error, and gates the attribute read.
fn has_attr(subject: &Expr, attr: &str) -> Expr {
    call2("hasattr", subject.clone(), Expr::Str(String::from(attr)))
}

/// One `case pattern [if guard]: body` clause.
struct MatchCase {
    pattern: MatchPattern,
    guard: Option<Expr>,
    body: Vec<Stmt>,
}

/// `subject == value`.
fn eq(subject: &Expr, value: Expr) -> Expr {
    Expr::Compare {
        op: CmpOp::Eq,
        lhs: Box::new(subject.clone()),
        rhs: Box::new(value),
    }
}

/// Combine two alternative tests with `or` for an OR pattern. `None` means "always matches" (a
/// wildcard alternative), which makes the whole OR always match.
fn or_tests(acc: Option<Expr>, next: Option<Expr>) -> Option<Expr> {
    match (acc, next) {
        (None, _) | (_, None) => None,
        (Some(a), Some(b)) => Some(bool_or(a, b)),
    }
}

/// Whether a pattern binds any name (a capture / as-pattern, or a structural pattern containing one).
/// Used to tell a non-binding OR (no bindings to reconcile) from a binding OR (bind from whichever
/// alternative matched).
fn pattern_binds(pattern: &MatchPattern) -> bool {
    match pattern {
        MatchPattern::Wildcard | MatchPattern::Value(_) => false,
        MatchPattern::Capture(_) | MatchPattern::As { .. } => true,
        MatchPattern::Or(alts) => alts.iter().any(pattern_binds),
        MatchPattern::Sequence { elems, .. } => elems.iter().any(pattern_binds),
        MatchPattern::Class {
            positional,
            keywords,
            ..
        } => {
            positional.iter().any(pattern_binds)
                || keywords.iter().any(|(_, p)| pattern_binds(p))
        }
        MatchPattern::Mapping { items, rest } => {
            rest.is_some() || items.iter().any(|(_, p)| pattern_binds(p))
        }
    }
}

/// Collect the names a pattern binds (a capture, an `as`, a `*rest` / `**rest`, or a structural
/// pattern containing them). Used to check that an OR pattern's alternatives all bind the same names.
fn pattern_bound_names(pattern: &MatchPattern, out: &mut Vec<String>) {
    match pattern {
        MatchPattern::Wildcard | MatchPattern::Value(_) => {}
        MatchPattern::Capture(n) => out.push(n.clone()),
        MatchPattern::As { pattern, name } => {
            out.push(name.clone());
            pattern_bound_names(pattern, out);
        }
        MatchPattern::Or(alts) => {
            if let Some(first) = alts.first() {
                pattern_bound_names(first, out);
            }
        }
        MatchPattern::Sequence { elems, .. } => {
            for e in elems {
                pattern_bound_names(e, out);
            }
        }
        MatchPattern::Class {
            positional,
            keywords,
            ..
        } => {
            for p in positional {
                pattern_bound_names(p, out);
            }
            for (_, p) in keywords {
                pattern_bound_names(p, out);
            }
        }
        MatchPattern::Mapping { items, rest } => {
            if let Some(r) = rest {
                out.push(r.clone());
            }
            for (_, p) in items {
                pattern_bound_names(p, out);
            }
        }
    }
}

/// A pattern's bound names, sorted and de-duplicated -- for comparing two alternatives' bindings.
fn sorted_bound_names(pattern: &MatchPattern) -> Vec<String> {
    let mut names = Vec::new();
    pattern_bound_names(pattern, &mut names);
    names.sort();
    names.dedup();
    names
}

/// The success body of a case, gated by its guard: `if guard: body else: rest` when there is a
/// guard (a failing guard falls through to the remaining cases), else just `body`.
fn guard_body(body: Vec<Stmt>, guard: &Option<Expr>, rest: &[Stmt]) -> Vec<Stmt> {
    match guard {
        Some(g) => vec![Stmt::If {
            test: g.clone(),
            body,
            orelse: rest.to_vec(),
        }],
        None => body,
    }
}

/// Match `pattern` against the value `subject`, returning the boolean test (`None` when the pattern
/// matches unconditionally -- a wildcard or a capture) and the bindings to run once the pattern has
/// matched. Structural patterns recurse against sub-subjects (`subject[i]`, `subject.attr`); each
/// test is an `and`-chain that short-circuits, so a length / `isinstance` / `hasattr` guard always
/// precedes the sub-subject access it protects. `subject` may be evaluated more than once, so the
/// caller passes a cheap, re-evaluable expression (a temp name, or an index / attribute access) --
/// a documented narrowing for a side-effecting `__getitem__` / `__getattribute__` (CPython evaluates
/// each sub-access once).
fn match_pattern(pattern: &MatchPattern, subject: &Expr) -> (Option<Expr>, Vec<Stmt>) {
    match pattern {
        MatchPattern::Wildcard => (None, Vec::new()),
        MatchPattern::Capture(name) => (
            None,
            vec![Stmt::Assign(Assign {
                target: name.clone(),
                annotation: None,
                value: Some(subject.clone()),
            })],
        ),
        MatchPattern::Value(value) => (Some(eq(subject, value.clone())), Vec::new()),
        MatchPattern::Or(alts) => {
            let matched: Vec<(Option<Expr>, Vec<Stmt>)> =
                alts.iter().map(|a| match_pattern(a, subject)).collect();
            let mut iter = matched.iter();
            let mut test = iter.next().expect("an OR pattern has alternatives").0.clone();
            for (t, _) in iter {
                test = or_tests(test, t.clone());
            }
            if !alts.iter().any(pattern_binds) {
                return (test, Vec::new());
            }
            let mut chain = matched.last().expect("an OR has alternatives").1.clone();
            for (t, b) in matched.iter().rev().skip(1) {
                chain = match t {
                    None => b.clone(),
                    Some(cond) => vec![Stmt::If {
                        test: cond.clone(),
                        body: b.clone(),
                        orelse: chain,
                    }],
                };
            }
            (test, chain)
        }
        MatchPattern::Sequence { elems, star } => match_sequence(subject, elems, *star),
        MatchPattern::Class {
            cls,
            positional,
            keywords,
            temp,
        } => match_class(subject, cls, positional, keywords, temp),
        MatchPattern::As { pattern, name } => {
            let (test, mut binds) = match_pattern(pattern, subject);
            binds.push(Stmt::Assign(Assign {
                target: name.clone(),
                annotation: None,
                value: Some(subject.clone()),
            }));
            (test, binds)
        }
        MatchPattern::Mapping { items, rest } => match_mapping(subject, items, rest),
    }
}

/// The synthetic key/value targets of the `**rest` dict comprehension. Dotted, so they cannot collide
/// with a user identifier, and comprehension-scoped, so reuse across nested mapping patterns is safe.
const MAP_REST_KEY: &str = ".mapkey";
const MAP_REST_VAL: &str = ".mapval";

/// The test + bindings for a mapping pattern against `subject` (see [`match_pattern`]): the subject is
/// a `dict` that contains each key (`isinstance` + `key in subject`, so a missing key is a non-match
/// and the membership gates the `subject[key]` read), whose value matches the sub-pattern. `**rest`
/// binds the remaining items as a new dict.
fn match_mapping(
    subject: &Expr,
    items: &[(Expr, MatchPattern)],
    rest: &Option<String>,
) -> (Option<Expr>, Vec<Stmt>) {
    let mut test = call2("isinstance", subject.clone(), Expr::Name(String::from("dict")));
    let mut binds: Vec<Stmt> = Vec::new();
    for (key, subpat) in items {
        test = bool_and(
            test,
            Expr::Compare {
                op: CmpOp::In,
                lhs: Box::new(key.clone()),
                rhs: Box::new(subject.clone()),
            },
        );
        let access = Expr::Subscript {
            value: Box::new(subject.clone()),
            index: Box::new(key.clone()),
        };
        let (sub_test, sub_binds) = match_pattern(subpat, &access);
        if let Some(t) = sub_test {
            test = bool_and(test, t);
        }
        binds.extend(sub_binds);
    }
    if let Some(name) = rest {
        let items_call = Expr::Call {
            func: Box::new(attr_of(subject, "items")),
            args: Vec::new(),
            keywords: Vec::new(),
        };
        let conditions = if items.is_empty() {
            Vec::new()
        } else {
            vec![Expr::Compare {
                op: CmpOp::NotIn,
                lhs: Box::new(Expr::Name(String::from(MAP_REST_KEY))),
                rhs: Box::new(Expr::Tuple(items.iter().map(|(k, _)| k.clone()).collect())),
            }]
        };
        let comp = Expr::DictComp {
            key: Box::new(Expr::Name(String::from(MAP_REST_KEY))),
            value: Box::new(Expr::Name(String::from(MAP_REST_VAL))),
            clauses: vec![CompClause {
                targets: vec![String::from(MAP_REST_KEY), String::from(MAP_REST_VAL)],
                iterable: items_call,
                conditions,
            }],
        };
        binds.push(Stmt::Assign(Assign {
            target: name.clone(),
            annotation: None,
            value: Some(comp),
        }));
    }
    (Some(test), binds)
}

/// The test + bindings for a sequence pattern against `subject` (see [`match_pattern`]): the subject
/// is a `list`/`tuple` of the right length whose elements match the sub-patterns.
fn match_sequence(
    subject: &Expr,
    elems: &[MatchPattern],
    star: Option<usize>,
) -> (Option<Expr>, Vec<Stmt>) {
    let elems_len = elems.len();
    let fixed = elems_len - usize::from(star.is_some());
    let len_op = if star.is_some() { CmpOp::Ge } else { CmpOp::Eq };
    let mut test = bool_and(
        is_sequence(subject),
        Expr::Compare {
            op: len_op,
            lhs: Box::new(call1("len", subject.clone())),
            rhs: Box::new(Expr::Int(fixed as i64)),
        },
    );
    let mut binds: Vec<Stmt> = Vec::new();
    for (i, elem) in elems.iter().enumerate() {
        if star == Some(i) {
            if let MatchPattern::Capture(name) = elem {
                let post = elems_len - i - 1;
                let value = call1(
                    "list",
                    slice_from(subject, i, (post > 0).then(|| -(post as i64))),
                );
                binds.push(Stmt::Assign(Assign {
                    target: name.clone(),
                    annotation: None,
                    value: Some(value),
                }));
            }
        } else {
            let (sub_test, sub_binds) = match_pattern(elem, &seq_elem(subject, i, elems_len, star));
            if let Some(t) = sub_test {
                test = bool_and(test, t);
            }
            binds.extend(sub_binds);
        }
    }
    (Some(test), binds)
}

/// The test + bindings for a class pattern against `subject` (see [`match_pattern`]). Keyword
/// sub-patterns match the named attributes of the subject, each behind a `hasattr` guard so a missing
/// attribute is a non-match (matching CPython's swallowed `AttributeError`). Positional sub-patterns
/// match the tuple `__match_class__(subject, cls, k)` extracts -- a runtime helper that performs the
/// `isinstance` test, honours the class's `__match_args__` (and the self-match rule for builtins like
/// `int`), and returns `None` for a non-match -- each `p_i` against element `i`.
fn match_class(
    subject: &Expr,
    cls: &Expr,
    positional: &[MatchPattern],
    keywords: &[(String, MatchPattern)],
    temp: &Option<String>,
) -> (Option<Expr>, Vec<Stmt>) {
    let mut binds: Vec<Stmt> = Vec::new();
    let mut test = if positional.is_empty() {
        isinstance_of(subject, cls)
    } else {
        let t = temp
            .as_ref()
            .expect("a positional class pattern carries a walrus temp");
        let extract = Expr::Call {
            func: Box::new(Expr::Name(String::from("__match_class__"))),
            args: vec![
                subject.clone(),
                cls.clone(),
                Expr::Int(positional.len() as i64),
            ],
            keywords: Vec::new(),
        };
        let bound = Expr::Walrus {
            target: t.clone(),
            value: Box::new(extract),
        };
        let mut cond = Expr::Compare {
            op: CmpOp::IsNot,
            lhs: Box::new(bound),
            rhs: Box::new(Expr::None),
        };
        let extracted = Expr::Name(t.clone());
        for (i, subpat) in positional.iter().enumerate() {
            let (sub_test, sub_binds) = match_pattern(subpat, &index_at(&extracted, i));
            if let Some(x) = sub_test {
                cond = bool_and(cond, x);
            }
            binds.extend(sub_binds);
        }
        cond
    };
    for (attr, subpat) in keywords {
        test = bool_and(test, has_attr(subject, attr));
        let (sub_test, sub_binds) = match_pattern(subpat, &attr_of(subject, attr));
        if let Some(t) = sub_test {
            test = bool_and(test, t);
        }
        binds.extend(sub_binds);
    }
    (Some(test), binds)
}

/// The nested-if statement list for `cases`, over the subject temp `subj`. Each case tests its
/// pattern (and guard); on failure control falls through to the remaining cases (the `orelse`).
fn build_case_tree(cases: &[MatchCase], subj: &str) -> Vec<Stmt> {
    let Some((first, rest_cases)) = cases.split_first() else {
        return Vec::new();
    };
    let rest = build_case_tree(rest_cases, subj);
    let (test, binds) = match_pattern(&first.pattern, &Expr::Name(String::from(subj)));
    let mut success = binds;
    success.extend(guard_body(first.body.clone(), &first.guard, &rest));
    match test {
        None => success,
        Some(t) => vec![Stmt::If {
            test: t,
            body: success,
            orelse: rest,
        }],
    }
}

/// One element of a list or set display: a plain value, or a `*`-spread iterable.
enum DisplayElem {
    Plain(Expr),
    Star(Expr),
}

/// `list(e)` / `set(e)` -- materialize an iterable as a new list / set (to spread `*e` in a display).
fn call1(func: &str, arg: Expr) -> Expr {
    Expr::Call {
        func: Box::new(Expr::Name(String::from(func))),
        args: vec![arg],
        keywords: Vec::new(),
    }
}

/// `func(a, b)` -- a two-argument call by name (`isinstance(x, C)`, `hasattr(x, "n")`).
fn call2(func: &str, a: Expr, b: Expr) -> Expr {
    Expr::Call {
        func: Box::new(Expr::Name(String::from(func))),
        args: vec![a, b],
        keywords: Vec::new(),
    }
}

/// Build a list display, desugaring any `*` spread: runs of plain elements become `[..]` literals,
/// concatenated with `+` around each `list(star)`. With no stars this is a plain `Expr::List`.
fn build_list_display(elems: Vec<DisplayElem>) -> Expr {
    build_spread_display(elems, Expr::List, |e| call1("list", e), BinOp::Add)
}

/// Build a set display, desugaring any `*` spread: runs of plain elements become `{..}` set literals,
/// unioned with `|` around each `set(star)`. With no stars this is a plain `Expr::Set`.
fn build_set_display(elems: Vec<DisplayElem>) -> Expr {
    build_spread_display(elems, Expr::Set, |e| call1("set", e), BinOp::BitOr)
}

/// Build a tuple display, desugaring any `*` spread: runs of plain elements become `(..)` tuple
/// literals, concatenated with `+` around each `tuple(star)`. With no stars this is a plain
/// `Expr::Tuple`, so an ordinary `1, 2` / `(1, 2)` is unchanged.
fn build_tuple_display(elems: Vec<DisplayElem>) -> Expr {
    build_spread_display(elems, Expr::Tuple, |e| call1("tuple", e), BinOp::Add)
}

fn build_spread_display(
    elems: Vec<DisplayElem>,
    literal: impl Fn(Vec<Expr>) -> Expr,
    spread: impl Fn(Expr) -> Expr,
    join: BinOp,
) -> Expr {
    if elems.iter().all(|e| matches!(e, DisplayElem::Plain(_))) {
        let plains = elems
            .into_iter()
            .map(|e| match e {
                DisplayElem::Plain(x) => x,
                DisplayElem::Star(_) => unreachable!("checked all-plain"),
            })
            .collect();
        return literal(plains);
    }
    let mut parts: Vec<Expr> = Vec::new();
    let mut run: Vec<Expr> = Vec::new();
    for e in elems {
        match e {
            DisplayElem::Plain(x) => run.push(x),
            DisplayElem::Star(x) => {
                if !run.is_empty() {
                    parts.push(literal(core::mem::take(&mut run)));
                }
                parts.push(spread(x));
            }
        }
    }
    if !run.is_empty() {
        parts.push(literal(run));
    }
    parts
        .into_iter()
        .reduce(|a, b| Expr::Binary {
            op: join,
            lhs: Box::new(a),
            rhs: Box::new(b),
        })
        .expect("a starred display has at least one part")
}

/// One entry of a dict display: a `key: value` pair, or a `**mapping` unpack.
enum DictItem {
    Pair(Expr, Expr),
    DoubleStar(Expr),
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].kind
    }

    /// The token one past the cursor (clamped to the trailing `Eof`).
    fn peek2(&self) -> &Tok {
        let i = (self.pos + 1).min(self.tokens.len() - 1);
        &self.tokens[i].kind
    }

    fn current_line(&self) -> u32 {
        self.tokens[self.pos].line
    }

    fn advance(&mut self) {
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
    }

    fn at(&self, kind: &Tok) -> bool {
        self.peek() == kind
    }

    fn eat(&mut self, kind: &Tok) -> bool {
        if self.at(kind) {
            self.advance();
            true
        } else {
            false
        }
    }

    fn error(&self, message: impl Into<String>) -> ParseError {
        ParseError {
            line: self.current_line(),
            message: message.into(),
        }
    }

    fn expect(&mut self, kind: &Tok, what: &str) -> Result<(), ParseError> {
        if self.at(kind) {
            self.advance();
            Ok(())
        } else {
            Err(self.error(format!("expected {what}")))
        }
    }

    fn expect_name(&mut self) -> Result<String, ParseError> {
        if let Tok::Name(name) = self.peek() {
            let name = name.clone();
            self.advance();
            Ok(name)
        } else if let Tok::Reserved(word) = self.peek() {
            Err(self.error(format!(
                "'{word}' is a reserved keyword and cannot be used as a name"
            )))
        } else {
            Err(self.error("expected a name"))
        }
    }

    /// End a simple statement: a `;` separates it from the next simple statement on the same line
    /// (`a; b`), otherwise a real newline ends the line. Records which happened in `line_ended` so an
    /// inline suite knows when to stop.
    fn expect_newline(&mut self) -> Result<(), ParseError> {
        if self.eat(&Tok::Semicolon) {
            self.line_ended = self.eat(&Tok::Newline);
            Ok(())
        } else {
            self.expect(&Tok::Newline, "end of line")?;
            self.line_ended = true;
            Ok(())
        }
    }


    fn parse_module(&mut self) -> Result<ModuleAst, ParseError> {
        let mut body = Vec::new();
        while !self.at(&Tok::Eof) {
            body.push(self.parse_statement()?);
        }
        Ok(ModuleAst { body })
    }

    fn parse_statement(&mut self) -> Result<Stmt, ParseError> {
        match self.peek() {
            Tok::KwDef => self.parse_funcdef(),
            Tok::KwIf => self.parse_if(),
            Tok::KwWhile => self.parse_while(),
            Tok::KwFor => self.parse_for(),
            Tok::KwTry => self.parse_try(),
            Tok::KwWith => self.parse_with(),
            Tok::KwClass => self.parse_classdef(),
            Tok::At => self.parse_decorated(),
            Tok::Reserved(s) if s == "import" => self.parse_import(),
            Tok::Reserved(s) if s == "from" => self.parse_from_import(),
            Tok::Name(n) if n == "match" && self.looks_like_match() => self.parse_match(),
            _ => self.parse_small_stmt(),
        }
    }

    /// `import module [as alias] (, module [as alias])*` -- simple (undotted) module names.
    fn parse_import(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        let mut modules = Vec::new();
        loop {
            let module = self.expect_name()?;
            if self.at(&Tok::Dot) {
                return Err(self.error("dotted module names (import a.b) are not supported in this subset"));
            }
            let bound = if self.eat(&Tok::KwAs) {
                self.expect_name()?
            } else {
                module.clone()
            };
            modules.push((module, bound));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect_newline()?;
        Ok(Stmt::Import { modules })
    }

    /// `from module import name [as alias] (, name [as alias])*`, or `from module import *`. Dotted
    /// module names are out of this subset.
    fn parse_from_import(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        let module = self.expect_name()?;
        if self.at(&Tok::Dot) {
            return Err(self.error("dotted module names (from a.b import ...) are not supported in this subset"));
        }
        if !matches!(self.peek(), Tok::Reserved(s) if s == "import") {
            return Err(self.error("expected 'import' after the module name in a `from` import"));
        }
        self.advance();
        if self.eat(&Tok::Star) {
            self.expect_newline()?;
            return Ok(Stmt::ImportStar { module });
        }
        let mut names = Vec::new();
        loop {
            let name = self.expect_name()?;
            let bound = if self.eat(&Tok::KwAs) {
                self.expect_name()?
            } else {
                name.clone()
            };
            names.push((name, bound));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect_newline()?;
        Ok(Stmt::ImportFrom { module, names })
    }

    /// Disambiguate the soft keyword `match`: it begins a match statement when the line has the shape
    /// `match <subject> : NEWLINE INDENT case ...`. A read-only scan finds the subject-ending `:` at
    /// bracket depth 0, then checks that an indented `case` clause follows -- so a subject beginning
    /// with `[` / `(` / `{` / `-` / `True` (which a single-token peek cannot tell apart from a
    /// subscript / call / operator on a variable named `match`) is recognized, while `match(x)`,
    /// `match[i] = 5`, `match = 5`, and `match: int = 5` stay plain expressions / assignments.
    fn looks_like_match(&self) -> bool {
        let kind = |i: usize| self.tokens.get(i).map(|t| &t.kind);
        let mut i = self.pos + 1;
        let mut depth: i32 = 0;
        let colon = loop {
            match kind(i) {
                Some(Tok::LParen | Tok::LBracket | Tok::LBrace) => depth += 1,
                Some(Tok::RParen | Tok::RBracket | Tok::RBrace) => depth -= 1,
                Some(Tok::Colon) if depth == 0 => break i,
                Some(Tok::Newline) if depth <= 0 => return false,
                Some(Tok::Eof) | None => return false,
                _ => {}
            }
            i += 1;
        };
        let mut j = colon + 1;
        while matches!(kind(j), Some(Tok::Newline)) {
            j += 1;
        }
        if !matches!(kind(j), Some(Tok::Indent)) {
            return false;
        }
        j += 1;
        while matches!(kind(j), Some(Tok::Newline)) {
            j += 1;
        }
        matches!(kind(j), Some(Tok::Name(n)) if n == "case")
    }

    /// A fresh synthetic local name (dotted, so it cannot collide with a user identifier).
    fn fresh_temp(&mut self, tag: &str) -> String {
        let n = self.temp_seq;
        self.temp_seq += 1;
        format!(".{tag}{n}")
    }

    /// `match subject:` NEWLINE INDENT (`case` clause)+ DEDENT. The subject is bound to a temp once,
    /// then the cases become a nested if-tree over that temp (a failing guard falls to the next case);
    /// the whole thing is wrapped in `if True:` so it stays one statement (match adds no new scope).
    fn parse_match(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        let subject = self.parse_expr()?;
        self.expect(&Tok::Colon, "':' after the match subject")?;
        self.expect_newline()?;
        self.expect(&Tok::Indent, "an indented block of `case` clauses")?;
        let mut cases = Vec::new();
        while !self.at(&Tok::Dedent) && !self.at(&Tok::Eof) {
            cases.push(self.parse_case()?);
        }
        self.expect(&Tok::Dedent, "a dedent ending the match block")?;
        if cases.is_empty() {
            return Err(self.error("a match statement needs at least one `case`"));
        }
        let temp = self.fresh_temp("match");
        let mut body = vec![Stmt::Assign(Assign {
            target: temp.clone(),
            annotation: None,
            value: Some(subject),
        })];
        body.extend(build_case_tree(&cases, &temp));
        Ok(Stmt::If {
            test: Expr::Bool(true),
            body,
            orelse: Vec::new(),
        })
    }

    /// `case pattern [if guard]:` suite.
    fn parse_case(&mut self) -> Result<MatchCase, ParseError> {
        match self.peek() {
            Tok::Name(n) if n == "case" => self.advance(),
            _ => return Err(self.error("expected a `case` clause in the match block")),
        }
        let pattern = self.parse_pattern()?;
        let guard = if self.eat(&Tok::KwIf) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect(&Tok::Colon, "':' after the case pattern")?;
        let body = self.parse_suite()?;
        Ok(MatchCase {
            pattern,
            guard,
            body,
        })
    }

    /// One case pattern: an OR pattern (`a | b | ...`) or a single closed pattern, optionally
    /// followed by `as name` to also capture the matched value (`[1, 2] as pair`). `as` binds looser
    /// than `|`, so `a | b as c` is `(a | b) as c`.
    fn parse_pattern(&mut self) -> Result<MatchPattern, ParseError> {
        let pattern = self.parse_or_pattern()?;
        if self.eat(&Tok::KwAs) {
            let name = self.expect_name()?;
            if name == "_" {
                return Err(self.error("cannot use '_' as the capture target of an `as` pattern"));
            }
            return Ok(MatchPattern::As {
                pattern: Box::new(pattern),
                name,
            });
        }
        Ok(pattern)
    }

    /// An OR pattern `a | b | ...` of closed patterns, or a single closed pattern when there is no
    /// `|`. Every alternative must be non-binding (a value, a bare class, a non-capturing sequence, or
    /// a wildcard) -- CPython requires the alternatives to bind the same names, and this subset
    /// supports only the non-binding case; a capture inside an `|` is rejected.
    fn parse_or_pattern(&mut self) -> Result<MatchPattern, ParseError> {
        let first = self.parse_closed_pattern()?;
        if !self.at(&Tok::Pipe) {
            return Ok(first);
        }
        let mut alts = vec![first];
        while self.eat(&Tok::Pipe) {
            alts.push(self.parse_closed_pattern()?);
        }
        let expected = sorted_bound_names(&alts[0]);
        for alt in &alts[1..] {
            if sorted_bound_names(alt) != expected {
                return Err(self.error(
                    "the alternatives of an `|` (or) pattern must all bind the same names",
                ));
            }
        }
        Ok(MatchPattern::Or(alts))
    }

    fn parse_closed_pattern(&mut self) -> Result<MatchPattern, ParseError> {
        match self.peek() {
            Tok::Name(n) if n == "_" => {
                self.advance();
                Ok(MatchPattern::Wildcard)
            }
            Tok::Name(_) => {
                let name = self.expect_name()?;
                let mut expr = Expr::Name(name.clone());
                let mut dotted = false;
                while self.eat(&Tok::Dot) {
                    dotted = true;
                    let attr = self.expect_name()?;
                    expr = Expr::Attribute {
                        value: Box::new(expr),
                        attr,
                    };
                }
                if self.at(&Tok::LParen) {
                    self.parse_class_pattern(expr)
                } else if dotted {
                    Ok(MatchPattern::Value(expr))
                } else {
                    Ok(MatchPattern::Capture(name))
                }
            }
            Tok::LBracket => {
                self.advance();
                let (elems, star, _) = self.parse_pattern_sequence(&Tok::RBracket)?;
                self.expect(&Tok::RBracket, "']' closing the sequence pattern")?;
                Ok(MatchPattern::Sequence { elems, star })
            }
            Tok::LParen => {
                self.advance();
                let (elems, star, trailing_comma) = self.parse_pattern_sequence(&Tok::RParen)?;
                self.expect(&Tok::RParen, "')' closing the pattern")?;
                if elems.len() == 1 && !trailing_comma && star.is_none() {
                    Ok(elems.into_iter().next().expect("one grouped pattern"))
                } else {
                    Ok(MatchPattern::Sequence { elems, star })
                }
            }
            Tok::LBrace => self.parse_mapping_pattern(),
            _ => {
                let negate = self.eat(&Tok::Minus);
                let lit = self.parse_atom()?;
                let value = if negate {
                    Expr::Unary {
                        op: UnaryOp::Neg,
                        operand: Box::new(lit),
                    }
                } else {
                    lit
                };
                Ok(MatchPattern::Value(value))
            }
        }
    }

    /// A class pattern `Cls(...)` / `pkg.Cls(...)`: the class reference `cls` has been parsed and the
    /// parser is positioned at the `(`. Each argument is a keyword sub-pattern `attr=pattern`; the
    /// subject matches when it is an `isinstance` of `cls` and every named attribute matches its
    /// sub-pattern. Positional sub-patterns (`Cls(a, b)`) need the class's `__match_args__`, which
    /// this runtime does not provide, so they are rejected here with a precise message.
    fn parse_class_pattern(&mut self, cls: Expr) -> Result<MatchPattern, ParseError> {
        self.expect(&Tok::LParen, "'(' opening the class pattern")?;
        let mut positional: Vec<MatchPattern> = Vec::new();
        let mut keywords: Vec<(String, MatchPattern)> = Vec::new();
        if !self.at(&Tok::RParen) {
            loop {
                if matches!(self.peek(), Tok::Name(_)) && matches!(self.peek2(), Tok::Assign) {
                    let attr = self.expect_name()?;
                    self.expect(&Tok::Assign, "'=' in a keyword class sub-pattern")?;
                    if keywords.iter().any(|(a, _)| a == &attr) {
                        return Err(self.error(format!(
                            "attribute name repeated in class pattern: {attr}"
                        )));
                    }
                    keywords.push((attr, self.parse_pattern()?));
                } else {
                    if !keywords.is_empty() {
                        return Err(self.error(
                            "a positional sub-pattern cannot follow a keyword sub-pattern in a \
                             class pattern",
                        ));
                    }
                    positional.push(self.parse_pattern()?);
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
                if self.at(&Tok::RParen) {
                    break;
                }
            }
        }
        self.expect(&Tok::RParen, "')' closing the class pattern")?;
        let temp = (!positional.is_empty()).then(|| self.fresh_temp("mc"));
        Ok(MatchPattern::Class {
            cls,
            positional,
            keywords,
            temp,
        })
    }

    /// A mapping pattern `{key: subpat, ..., **rest}`: `key: sub-pattern` items (each key an evaluated
    /// literal / value expression) with an optional trailing `**rest` capture. The subject matches a
    /// `dict` that contains each key (a subset match). A duplicate key or a non-literal key is
    /// rejected, as is a `**` rest that is not last.
    fn parse_mapping_pattern(&mut self) -> Result<MatchPattern, ParseError> {
        self.expect(&Tok::LBrace, "'{' opening the mapping pattern")?;
        let mut items: Vec<(Expr, MatchPattern)> = Vec::new();
        let mut rest: Option<String> = None;
        if !self.at(&Tok::RBrace) {
            loop {
                if self.eat(&Tok::DoubleStar) {
                    let name = self.expect_name()?;
                    if name == "_" {
                        return Err(self.error(
                            "cannot use '_' as the `**` rest target of a mapping pattern",
                        ));
                    }
                    rest = Some(name);
                    self.eat(&Tok::Comma);
                    break;
                }
                let key = match self.parse_closed_pattern()? {
                    MatchPattern::Value(e) => e,
                    _ => {
                        return Err(self.error(
                            "a mapping-pattern key must be a literal or a value (a dotted name)",
                        ))
                    }
                };
                if items.iter().any(|(k, _)| k == &key) {
                    return Err(self.error("a mapping pattern has a duplicate key"));
                }
                self.expect(&Tok::Colon, "':' after a mapping-pattern key")?;
                items.push((key, self.parse_pattern()?));
                if !self.eat(&Tok::Comma) {
                    break;
                }
                if self.at(&Tok::RBrace) {
                    break;
                }
            }
        }
        self.expect(&Tok::RBrace, "'}' closing the mapping pattern")?;
        Ok(MatchPattern::Mapping { items, rest })
    }

    /// The comma-separated patterns of a sequence pattern, up to `terminator`. Returns the patterns,
    /// the index of a `*name`/`*_` star element (at most one), and whether a trailing comma closed the
    /// list (to tell a 1-group `(p)` from a 1-sequence `(p,)`). A `|` (or) directly inside a sequence
    /// is out of the subset; each non-star element is a capture / wildcard / value / nested sequence /
    /// class pattern (matched recursively).
    fn parse_pattern_sequence(
        &mut self,
        terminator: &Tok,
    ) -> Result<(Vec<MatchPattern>, Option<usize>, bool), ParseError> {
        let mut elems = Vec::new();
        let mut star = None;
        if self.at(terminator) {
            return Ok((elems, star, false));
        }
        let mut trailing_comma = false;
        loop {
            if self.eat(&Tok::Star) {
                if star.is_some() {
                    return Err(self.error("a sequence pattern allows at most one `*` pattern"));
                }
                let elem = self.parse_closed_pattern()?;
                if !matches!(elem, MatchPattern::Capture(_) | MatchPattern::Wildcard) {
                    return Err(self.error("a `*` pattern must be `*name` or `*_`"));
                }
                star = Some(elems.len());
                elems.push(elem);
            } else {
                elems.push(self.parse_pattern()?);
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
            if self.at(terminator) {
                trailing_comma = true;
                break;
            }
        }
        Ok((elems, star, trailing_comma))
    }

    /// `('@' expr NEWLINE)+` then a `def` or `class`. Each decorator names a callable applied to the
    /// defined function/class; they wrap it bottom-up, so `@a` `@b` `def f` becomes `f = a(b(f))`.
    fn parse_decorated(&mut self) -> Result<Stmt, ParseError> {
        let mut decorators = Vec::new();
        while self.eat(&Tok::At) {
            decorators.push(self.parse_expr()?);
            self.expect_newline()?;
        }
        let inner = match self.peek() {
            Tok::KwDef => self.parse_funcdef()?,
            Tok::KwClass => self.parse_classdef()?,
            _ => return Err(self.error("a decorator must be followed by a 'def' or 'class'")),
        };
        Ok(Stmt::Decorated {
            decorators,
            inner: Box::new(inner),
        })
    }

    /// A non-compound statement: `return`, an assignment, or an expression
    /// statement. Consumes the trailing [`Tok::Newline`].
    fn parse_small_stmt(&mut self) -> Result<Stmt, ParseError> {
        if self.at(&Tok::KwDel) {
            self.parse_del()
        } else if self.at(&Tok::KwAssert) {
            self.parse_assert()
        } else if self.at(&Tok::KwReturn) {
            self.parse_return()
        } else if self.at(&Tok::KwBreak) {
            self.advance();
            self.expect_newline()?;
            Ok(Stmt::Break)
        } else if self.at(&Tok::KwContinue) {
            self.advance();
            self.expect_newline()?;
            Ok(Stmt::Continue)
        } else if self.at(&Tok::KwPass) {
            self.advance();
            self.expect_newline()?;
            Ok(Stmt::Pass)
        } else if self.at(&Tok::KwRaise) {
            self.parse_raise()
        } else if matches!(self.peek(), Tok::Reserved(s) if s == "nonlocal") {
            self.parse_nonlocal()
        } else if matches!(self.peek(), Tok::Reserved(s) if s == "global") {
            self.parse_global()
        } else {
            self.parse_assign_or_expr()
        }
    }

    /// `nonlocal name (, name)*` -- a declaration; it binds the listed names to an enclosing
    /// function's cells (the compiler validates each has such a binding). Emits no code itself.
    fn parse_nonlocal(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        let mut names = vec![self.expect_name()?];
        while self.eat(&Tok::Comma) {
            names.push(self.expect_name()?);
        }
        self.expect_newline()?;
        Ok(Stmt::Nonlocal(names))
    }

    /// `global name (, name)*` -- a declaration; it binds the listed names to the module globals, so
    /// this function's reads resolve the global and its assignments store to the module namespace.
    /// Emits no code itself.
    fn parse_global(&mut self) -> Result<Stmt, ParseError> {
        self.advance();
        let mut names = vec![self.expect_name()?];
        while self.eat(&Tok::Comma) {
            names.push(self.expect_name()?);
        }
        self.expect_newline()?;
        Ok(Stmt::Global(names))
    }

    fn parse_raise(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwRaise, "'raise'")?;
        let exc = if self.at(&Tok::Newline) {
            None
        } else {
            Some(self.parse_expr()?)
        };
        let cause = if matches!(self.peek(), Tok::Reserved(s) if s == "from") {
            self.advance();
            if exc.is_none() {
                return Err(self.error("'raise from' needs an exception before 'from'"));
            }
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect_newline()?;
        Ok(Stmt::Raise { exc, cause })
    }

    /// `del target (, target)* NEWLINE` -- unbind each target. A bare name is supported; a subscript
    /// or attribute target is unsupported (compiled as an error).
    /// The parser is at another `=` after `first = second` -- a CHAINED assignment
    /// `first = second = ... = value`. Collect the remaining targets and the final value into a
    /// MultiAssign; each target may be a name, a subscript, or an attribute.
    fn finish_chain(&mut self, first: Expr, second: Expr, line: u32) -> Result<Stmt, ParseError> {
        let mut target_exprs = vec![first, second];
        let value;
        loop {
            self.advance();
            let next = self.parse_rhs_value()?;
            if self.at(&Tok::Assign) {
                target_exprs.push(next);
            } else {
                value = next;
                break;
            }
        }
        self.expect_newline()?;
        let targets = target_exprs
            .into_iter()
            .map(|e| self.assign_target(e, line))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Stmt::MultiAssign { targets, value })
    }

    /// `assert test [, msg] NEWLINE` -- desugars to `if not test: raise AssertionError[(msg)]`,
    /// reusing the existing If + Not + Raise. (The runtime must provide `AssertionError` as a
    /// built-in exception for the raised call to resolve; the desugar is inert until it does.)
    fn parse_assert(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwAssert, "'assert'")?;
        let test = self.parse_expr()?;
        let exc = if self.eat(&Tok::Comma) {
            let msg = self.parse_expr()?;
            Expr::Call {
                func: Box::new(Expr::Name(String::from("AssertionError"))),
                args: vec![msg],
                keywords: Vec::new(),
            }
        } else {
            Expr::Name(String::from("AssertionError"))
        };
        self.expect_newline()?;
        Ok(Stmt::If {
            test: Expr::Not {
                operand: Box::new(test),
            },
            body: vec![Stmt::Raise {
                exc: Some(exc),
                cause: None,
            }],
            orelse: Vec::new(),
        })
    }

    fn parse_del(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwDel, "'del'")?;
        let mut targets = vec![self.parse_expr()?];
        while self.eat(&Tok::Comma) {
            if self.at(&Tok::Newline) {
                break;
            }
            targets.push(self.parse_expr()?);
        }
        self.expect_newline()?;
        Ok(Stmt::Delete(targets))
    }

    fn parse_return(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwReturn, "'return'")?;
        let value = if self.at(&Tok::Newline) {
            None
        } else {
            Some(self.parse_rhs_value()?)
        };
        self.expect_newline()?;
        Ok(Stmt::Return(value))
    }

    /// `assignment_stmt`, `annotated_assignment_stmt`, or `expression_stmt`. The
    /// statement is parsed as an expression first; a following `:` or `=` then
    /// reinterprets it as an (annotated) assignment, restricted to a bare name
    /// as the target.
    fn parse_assign_or_expr(&mut self) -> Result<Stmt, ParseError> {
        let target_line = self.current_line();
        if self.at(&Tok::Star) {
            self.advance();
            let first = self.parse_expr()?;
            return self.finish_tuple_or_expr_stmt(vec![DisplayElem::Star(first)], target_line);
        }
        let expr = self.parse_expr()?;
        if let Some(op) = aug_assign_op(self.peek()) {
            self.advance();
            let value = self.parse_expr()?;
            self.expect_newline()?;
            return match expr {
                Expr::Subscript { value: container, index } => {
                    if matches!(&*index, Expr::Slice { .. }) {
                        return Err(self.error("augmented slice assignment is out of the subset"));
                    }
                    Ok(Stmt::SetItem {
                        container: *container,
                        index: *index,
                        value,
                        op: Some(op),
                    })
                }
                Expr::Attribute { value: obj, attr } => Ok(Stmt::SetAttr {
                    obj: *obj,
                    attr,
                    value,
                    op: Some(op),
                }),
                other => {
                    let target = self.target_name(other, target_line)?;
                    Ok(Stmt::Assign(Assign {
                        target: target.clone(),
                        annotation: None,
                        value: Some(Expr::InplaceBinary {
                            op,
                            lhs: Box::new(Expr::Name(target)),
                            rhs: Box::new(value),
                        }),
                    }))
                }
            };
        }
        match self.peek() {
            Tok::Colon => {
                let target = self.target_name(expr, target_line)?;
                self.advance();
                let annotation = Some(self.parse_expr()?);
                let value = if self.eat(&Tok::Assign) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                self.expect_newline()?;
                Ok(Stmt::Assign(Assign {
                    target,
                    annotation,
                    value,
                }))
            }
            Tok::Assign if matches!(&expr, Expr::Attribute { .. }) => {
                self.advance();
                let rhs = self.parse_expr()?;
                if self.at(&Tok::Assign) {
                    return self.finish_chain(expr, rhs, target_line);
                }
                self.expect_newline()?;
                let Expr::Attribute { value, attr } = expr else {
                    unreachable!("guarded to an attribute")
                };
                Ok(Stmt::SetAttr {
                    obj: *value,
                    attr,
                    value: rhs,
                    op: None,
                })
            }
            Tok::Assign if matches!(&expr, Expr::Subscript { .. }) => {
                self.advance();
                let rhs = self.parse_expr()?;
                if self.at(&Tok::Assign) {
                    return self.finish_chain(expr, rhs, target_line);
                }
                self.expect_newline()?;
                let Expr::Subscript { value, index } = expr else {
                    unreachable!("guarded to a subscript")
                };
                Ok(Stmt::SetItem {
                    container: *value,
                    index: *index,
                    value: rhs,
                    op: None,
                })
            }
            Tok::Assign if matches!(&expr, Expr::Tuple(_) | Expr::List(_)) => {
                self.advance();
                let value = self.parse_rhs_value()?;
                if self.at(&Tok::Assign) {
                    return self.finish_chain(expr, value, target_line);
                }
                self.expect_newline()?;
                let AssignTarget::Tuple(targets) = self.assign_target(expr, target_line)? else {
                    unreachable!("a tuple/list expression converts to a Tuple target")
                };
                Ok(Stmt::TupleAssign {
                    targets,
                    star: None,
                    value,
                })
            }
            Tok::Assign => {
                self.advance();
                let value = self.parse_rhs_value()?;
                if self.at(&Tok::Assign) {
                    return self.finish_chain(expr, value, target_line);
                }
                self.expect_newline()?;
                let target = self.target_name(expr, target_line)?;
                Ok(Stmt::Assign(Assign {
                    target,
                    annotation: None,
                    value: Some(value),
                }))
            }
            Tok::Comma => self.finish_tuple_or_expr_stmt(vec![DisplayElem::Plain(expr)], target_line),
            _ => {
                self.expect_newline()?;
                Ok(Stmt::Expr(expr))
            }
        }
    }

    /// A value position -- an assignment RHS or a `return` value: a single expression, or an
    /// unparenthesized expression list `1, 2, 3`, which becomes a tuple display (so `a, b = 1, 2`
    /// unpacks and `return a, b` returns `(a, b)`). A lone trailing comma makes a 1-tuple (`a,`).
    /// A `*` spread is allowed in the list (`1, *rest, 2`) and desugars like a starred tuple
    /// display; a bare `*x` with no comma is a syntax error.
    fn parse_rhs_value(&mut self) -> Result<Expr, ParseError> {
        let first = self.parse_display_elem()?;
        if !self.at(&Tok::Comma) {
            return match first {
                DisplayElem::Plain(e) => Ok(e),
                DisplayElem::Star(_) => Err(self.error("can't use a starred expression here")),
            };
        }
        let mut elems = vec![first];
        while self.eat(&Tok::Comma) {
            if self.at(&Tok::Newline) || self.at(&Tok::Eof) {
                break;
            }
            elems.push(self.parse_display_elem()?);
        }
        Ok(build_tuple_display(elems))
    }

    /// Finish a top-level comma list once its leading element is collected: either a
    /// tuple-unpacking assignment (`a, b = v` / `*a, b = v` -- an `=` follows, so the elements are
    /// targets, with at most one starred target) or a bare-tuple expression statement (`a, b` /
    /// `1, 2` / `a,` / `a, *rest` -- a newline follows, so the elements build a tuple, where a `*`
    /// is a spread and any number are allowed). The leading `,` is not yet consumed; the loop takes
    /// the rest (with an optional trailing comma). A lone starred `*b` (no comma) is not a statement.
    fn finish_tuple_or_expr_stmt(
        &mut self,
        mut elems: Vec<DisplayElem>,
        line: u32,
    ) -> Result<Stmt, ParseError> {
        let mut saw_comma = false;
        while self.eat(&Tok::Comma) {
            saw_comma = true;
            if matches!(
                self.peek(),
                Tok::Assign | Tok::Newline | Tok::Semicolon | Tok::Eof
            ) {
                break;
            }
            elems.push(self.parse_display_elem()?);
        }
        if self.eat(&Tok::Assign) {
            let mut star = None;
            let mut inners: Vec<Expr> = Vec::with_capacity(elems.len());
            for (i, e) in elems.into_iter().enumerate() {
                match e {
                    DisplayElem::Plain(x) => inners.push(x),
                    DisplayElem::Star(x) => {
                        if star.is_some() {
                            return Err(self.error("only one starred target is allowed"));
                        }
                        star = Some(i);
                        inners.push(x);
                    }
                }
            }
            let value = self.parse_rhs_value()?;
            if self.at(&Tok::Assign) {
                if star.is_some() {
                    return Err(self
                        .error("a starred target in a chained assignment is out of the subset"));
                }
                return self.finish_chain(Expr::Tuple(inners), value, line);
            }
            self.expect_newline()?;
            let targets = inners
                .into_iter()
                .map(|e| self.assign_target(e, line))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Stmt::TupleAssign { targets, star, value })
        } else {
            if !saw_comma && matches!(elems.as_slice(), [DisplayElem::Star(_)]) {
                return Err(self.error("can't use a starred expression here"));
            }
            self.expect_newline()?;
            Ok(Stmt::Expr(build_tuple_display(elems)))
        }
    }

    /// Require an assignment target to be a bare name (attribute, subscript, and
    /// tuple targets are not supported in this subset).
    fn target_name(&self, expr: Expr, line: u32) -> Result<String, ParseError> {
        match expr {
            Expr::Name(name) => Ok(name),
            _ => Err(ParseError {
                line,
                message: String::from(
                    "only a bare name is a valid assignment target (attribute, subscript, \
                     and tuple targets are not supported in this subset)",
                ),
            }),
        }
    }

    /// Convert a parsed expression into a tuple-unpacking target: a bare name, a subscript `c[i]`
    /// (or a slice), an attribute `o.x`, or a nested tuple/list `(a, b)` / `[a, b]` (recursive).
    /// A literal or a call is not a valid target.
    fn assign_target(&self, expr: Expr, line: u32) -> Result<AssignTarget, ParseError> {
        match expr {
            Expr::Name(name) => Ok(AssignTarget::Name(name)),
            Expr::Subscript { value, index } => {
                Ok(AssignTarget::Subscript {
                    container: *value,
                    index: *index,
                })
            }
            Expr::Attribute { value, attr } => Ok(AssignTarget::Attribute { obj: *value, attr }),
            Expr::Tuple(elems) | Expr::List(elems) => {
                if elems.is_empty() {
                    return Err(ParseError {
                        line,
                        message: String::from("an empty () / [] is not an assignment target"),
                    });
                }
                let targets = elems
                    .into_iter()
                    .map(|e| self.assign_target(e, line))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(AssignTarget::Tuple(targets))
            }
            _ => Err(ParseError {
                line,
                message: String::from(
                    "a tuple-unpacking target must be a name, subscript, attribute, or a nested \
                     tuple/list target",
                ),
            }),
        }
    }

    fn parse_if(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwIf, "'if'")?;
        let test = self.parse_expr()?;
        self.expect(&Tok::Colon, "':'")?;
        let body = self.parse_suite()?;
        let orelse = self.parse_elif_else()?;
        Ok(Stmt::If { test, body, orelse })
    }

    /// The `("elif" ...)* ["else" ...]` tail of an `if`. An `elif` is desugared
    /// into a nested `if` in the enclosing clause's `orelse`, which keeps the AST
    /// to a single conditional node shape.
    fn parse_elif_else(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if self.at(&Tok::KwElif) {
            self.advance();
            let test = self.parse_expr()?;
            self.expect(&Tok::Colon, "':'")?;
            let body = self.parse_suite()?;
            let orelse = self.parse_elif_else()?;
            Ok(vec![Stmt::If { test, body, orelse }])
        } else if self.eat(&Tok::KwElse) {
            self.expect(&Tok::Colon, "':'")?;
            self.parse_suite()
        } else {
            Ok(Vec::new())
        }
    }

    fn parse_for(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwFor, "'for'")?;
        let line = self.current_line();
        let mut targets = Vec::new();
        let mut star: Option<usize> = None;
        loop {
            if self.eat(&Tok::Star) {
                if star.is_some() {
                    return Err(self.error("a for-loop target may have at most one starred name"));
                }
                star = Some(targets.len());
            }
            targets.push(self.for_target(line)?);
            if !self.eat(&Tok::Comma) {
                break;
            }
            if self.at(&Tok::KwIn) {
                break;
            }
        }
        self.expect(&Tok::KwIn, "'in'")?;
        let iter = self.parse_rhs_value()?;
        self.expect(&Tok::Colon, "':'")?;
        let mut body = self.parse_suite()?;
        let orelse = self.parse_loop_else()?;

        if star.is_none() {
            if let [AssignTarget::Name(name)] = targets.as_slice() {
                let target = name.clone();
                if is_range_call(&iter) {
                    if let Ok((start, stop, step)) = self.range_bounds(iter.clone()) {
                        return Ok(Stmt::For { target, start, stop, step, body, orelse });
                    }
                }
                return Ok(Stmt::ForIter { target, iterable: iter, body, orelse });
            }
        }

        let (unpack_targets, unpack_star) = match targets.as_slice() {
            [AssignTarget::Tuple(inner)] if star.is_none() => (inner.clone(), None),
            _ => (targets, star),
        };
        let tmp = String::from(".unpack");
        let mut new_body = Vec::with_capacity(body.len() + 1);
        new_body.push(Stmt::TupleAssign {
            targets: unpack_targets,
            star: unpack_star,
            value: Expr::Name(tmp.clone()),
        });
        new_body.append(&mut body);
        Ok(Stmt::ForIter {
            target: tmp,
            iterable: iter,
            body: new_body,
            orelse,
        })
    }

    /// A single `for`-loop target: a name or a (possibly nested) parenthesized/bracketed tuple of
    /// targets. Parsed as a primary (an atom plus `.`/`[]` trailers) so the loop's `in` keyword --
    /// a comparison operator to `parse_expr` -- is not consumed.
    fn for_target(&mut self, line: u32) -> Result<AssignTarget, ParseError> {
        let expr = self.parse_trailer()?;
        let target = self.assign_target(expr, line)?;
        match target {
            AssignTarget::Name(_) | AssignTarget::Tuple(_) => Ok(target),
            AssignTarget::Subscript { .. } | AssignTarget::Attribute { .. } => {
                Err(self.error("a for-loop target must be a name or a tuple of names"))
            }
        }
    }

    fn parse_try(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwTry, "'try'")?;
        self.expect(&Tok::Colon, "':' after 'try'")?;
        let body = self.parse_suite()?;
        let mut handlers = Vec::new();
        let mut seen_bare = false;
        while self.at(&Tok::KwExcept) {
            if seen_bare {
                return Err(self.error("the bare 'except:' must be the last handler"));
            }
            self.advance();
            let (typ, name) = if self.at(&Tok::Colon) {
                (None, None)
            } else {
                let t = self.parse_expr()?;
                let n = if self.at(&Tok::KwAs) {
                    self.advance();
                    Some(self.expect_name()?)
                } else {
                    None
                };
                (Some(t), n)
            };
            if typ.is_none() {
                seen_bare = true;
            }
            self.expect(&Tok::Colon, "':' after the except clause")?;
            let handler_body = self.parse_suite()?;
            handlers.push(ExceptHandler {
                typ,
                name,
                body: handler_body,
            });
        }
        let orelse = if self.at(&Tok::KwElse) {
            self.advance();
            self.expect(&Tok::Colon, "':' after 'else'")?;
            self.parse_suite()?
        } else {
            Vec::new()
        };
        let finalbody = if self.at(&Tok::KwFinally) {
            self.advance();
            self.expect(&Tok::Colon, "':' after 'finally'")?;
            self.parse_suite()?
        } else {
            Vec::new()
        };
        if handlers.is_empty() && finalbody.is_empty() {
            return Err(self.error("'try' needs at least one 'except' or a 'finally'"));
        }
        if !orelse.is_empty() && handlers.is_empty() {
            return Err(self.error("'try ... else' needs an 'except' clause"));
        }
        Ok(Stmt::Try {
            body,
            handlers,
            orelse,
            finalbody,
        })
    }

    fn parse_classdef(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwClass, "'class'")?;
        let name = self.expect_name()?;
        let bases = if self.eat(&Tok::LParen) {
            let mut bases = Vec::new();
            while !self.at(&Tok::RParen) {
                if matches!(self.peek(), Tok::Name(_)) && matches!(self.peek2(), Tok::Assign) {
                    return Err(
                        self.error("a keyword class argument (e.g. `metaclass=`) is out of the subset")
                    );
                }
                bases.push(self.parse_expr()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RParen, "')' closing the base list")?;
            bases
        } else {
            Vec::new()
        };
        self.expect(&Tok::Colon, "':' after the class header")?;
        let body = self.parse_suite()?;
        for stmt in &body {
            if !matches!(
                stmt,
                Stmt::FuncDef(_)
                    | Stmt::Assign(_)
                    | Stmt::Pass
                    | Stmt::Decorated { .. }
                    | Stmt::Expr(Expr::Str(_))
            ) {
                return Err(self.error(
                    "a class body supports only methods, attribute assignments, and a docstring in this subset",
                ));
            }
        }
        Ok(Stmt::ClassDef { name, bases, body })
    }

    /// Only `range(stop)`, `range(start, stop)`, or `range(start, stop, step)` are
    /// iterable in this subset; pull out the bounds (a missing start is `0`, a
    /// missing step is `1`). The step must be a non-zero integer literal.
    fn range_bounds(&self, iter: Expr) -> Result<(Expr, Expr, i64), ParseError> {
        if let Expr::Call { func, args, keywords } = iter {
            if matches!(&*func, Expr::Name(n) if n == "range") {
                if !keywords.is_empty() {
                    return Err(self.error("range() takes no keyword arguments"));
                }
                let mut args = args;
                match args.len() {
                    1 => return Ok((Expr::Int(0), args.pop().unwrap(), 1)),
                    2 => {
                        let stop = args.pop().unwrap();
                        let start = args.pop().unwrap();
                        return Ok((start, stop, 1));
                    }
                    3 => {
                        let step_expr = args.pop().unwrap();
                        let stop = args.pop().unwrap();
                        let start = args.pop().unwrap();
                        let Expr::Int(step) = step_expr else {
                            return Err(
                                self.error("the range() step must be an integer literal")
                            );
                        };
                        if step == 0 {
                            return Err(self.error("range() step must not be zero"));
                        }
                        return Ok((start, stop, step));
                    }
                    _ => {}
                }
            }
        }
        Err(self.error(
            "'for' iterates only range(stop), range(start, stop), or range(start, stop, step) \
             in this subset",
        ))
    }

    fn parse_while(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwWhile, "'while'")?;
        let test = self.parse_expr()?;
        self.expect(&Tok::Colon, "':'")?;
        let body = self.parse_suite()?;
        let orelse = self.parse_loop_else()?;
        Ok(Stmt::While { test, body, orelse })
    }

    /// `with context ["as" name] ":" suite` -- a single context manager. Multiple managers in
    /// one `with` (`with a, b:`) are unsupported.
    fn parse_with(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwWith, "'with'")?;
        let items = if self.at(&Tok::LParen) {
            let saved = self.pos;
            match self.try_parenthesized_with_items() {
                Some(items) => items,
                None => {
                    self.pos = saved;
                    self.parse_with_items()?
                }
            }
        } else {
            self.parse_with_items()?
        };
        self.expect(&Tok::Colon, "':'")?;
        let body = self.parse_suite()?;
        let mut current = body;
        for (context, optional_name) in items.into_iter().rev() {
            current = vec![Stmt::With {
                context,
                optional_name,
                body: current,
            }];
        }
        Ok(current
            .into_iter()
            .next()
            .expect("at least one context manager was parsed"))
    }

    /// One `with` item: a context-manager expression and an optional `as name` target.
    fn parse_with_item(&mut self) -> Result<(Expr, Option<String>), ParseError> {
        let context = self.parse_expr()?;
        let optional_name = if self.eat(&Tok::KwAs) {
            Some(self.expect_name()?)
        } else {
            None
        };
        Ok((context, optional_name))
    }

    /// A comma-separated list of `with` items (the unparenthesized form).
    fn parse_with_items(&mut self) -> Result<Vec<(Expr, Option<String>)>, ParseError> {
        let mut items = vec![self.parse_with_item()?];
        while self.eat(&Tok::Comma) {
            items.push(self.parse_with_item()?);
        }
        Ok(items)
    }

    /// Try to parse a PEP 617 parenthesized `with` item list `( item (, item)* ,? )` that is
    /// immediately followed by `:`. Returns `None` (leaving the cursor wherever it got to -- the
    /// caller restores it) when the shape does not match, e.g. `with (a or b) as x:` where the `)`
    /// is followed by `as`, so `(a or b)` is a parenthesized expression rather than an item list.
    fn try_parenthesized_with_items(&mut self) -> Option<Vec<(Expr, Option<String>)>> {
        self.eat(&Tok::LParen);
        let mut items = Vec::new();
        loop {
            items.push(self.parse_with_item().ok()?);
            if self.eat(&Tok::Comma) {
                if self.at(&Tok::RParen) {
                    break;
                }
                continue;
            }
            break;
        }
        if self.eat(&Tok::RParen) && self.at(&Tok::Colon) {
            Some(items)
        } else {
            None
        }
    }

    /// An optional `else:` suite on a `while`/`for` (run when the loop exits without
    /// `break`). Empty when the clause is absent.
    fn parse_loop_else(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if self.eat(&Tok::KwElse) {
            self.expect(&Tok::Colon, "':'")?;
            self.parse_suite()
        } else {
            Ok(Vec::new())
        }
    }

    /// `suite: stmt_list NEWLINE | NEWLINE INDENT statement+ DEDENT`. The
    /// single-line form holds one simple statement (no `;`-separated list).
    fn parse_suite(&mut self) -> Result<Vec<Stmt>, ParseError> {
        if self.eat(&Tok::Newline) {
            self.expect(&Tok::Indent, "an indented block")?;
            let mut body = Vec::new();
            while !self.at(&Tok::Dedent) && !self.at(&Tok::Eof) {
                body.push(self.parse_statement()?);
            }
            self.expect(&Tok::Dedent, "a dedent ending the block")?;
            Ok(body)
        } else {
            let mut stmts = vec![self.parse_small_stmt()?];
            while !self.line_ended {
                stmts.push(self.parse_small_stmt()?);
            }
            Ok(stmts)
        }
    }

    fn parse_funcdef(&mut self) -> Result<Stmt, ParseError> {
        self.expect(&Tok::KwDef, "'def'")?;
        let name = self.expect_name()?;
        self.expect(&Tok::LParen, "'(' after the function name")?;
        let params = self.parse_params()?;
        self.expect(&Tok::RParen, "')'")?;
        let ret = if self.eat(&Tok::Arrow) {
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect(&Tok::Colon, "':'")?;
        let body = self.parse_suite()?;
        Ok(Stmt::FuncDef(FuncDef {
            name,
            params,
            ret,
            body,
        }))
    }

    /// `parameter ("," parameter)* [","]`, where `parameter: identifier [":"
    /// expression] ["=" expression]`. A parameter without a default may not follow one
    /// with a default (a SyntaxError in Python). A bare `*` marks the following parameters
    /// keyword-only; `*args` collects surplus positionals; `**kwargs` collects surplus keywords; and
    /// a `/` marks the preceding parameters positional-only.
    /// Def parameters, in parentheses, with optional annotations.
    fn parse_params(&mut self) -> Result<Vec<ParamDef>, ParseError> {
        self.parse_param_list(&Tok::RParen, true)
    }

    /// Parse a comma-separated parameter list up to `terminator` (`)` for a def, `:` for a lambda),
    /// handling `/` (positional-only marker), `*args`, a bare `*` that introduces keyword-only
    /// params, `**kwargs`, and defaults. Annotations (`name: type`) are parsed only when
    /// `allow_annotations`; a lambda forbids them because its `:` ends the parameter list.
    fn parse_param_list(
        &mut self,
        terminator: &Tok,
        allow_annotations: bool,
    ) -> Result<Vec<ParamDef>, ParseError> {
        let mut params = Vec::new();
        if self.at(terminator) {
            return Ok(params);
        }
        let mut seen_default = false;
        let mut after_star = false;
        loop {
            if self.eat(&Tok::Slash) {
                if params.is_empty() {
                    return Err(self.error("at least one parameter must precede `/`"));
                }
                if after_star {
                    return Err(self.error("`/` must appear before `*` in a parameter list"));
                }
                if params.iter().any(|p| p.positional_only) {
                    return Err(self.error("only one `/` is allowed in a parameter list"));
                }
                for p in &mut params {
                    p.positional_only = true;
                }
                if self.eat(&Tok::Comma) {
                    if self.at(terminator) {
                        break;
                    }
                    continue;
                }
                if self.at(terminator) {
                    break;
                }
                return Err(self.error("expected ',' or the end of the parameter list after '/'"));
            }
            if self.eat(&Tok::DoubleStar) {
                let name = self.expect_name()?;
                let annotation = if allow_annotations && self.eat(&Tok::Colon) {
                    Some(self.parse_expr()?)
                } else {
                    None
                };
                params.push(ParamDef {
                    name,
                    annotation,
                    default: None,
                    keyword_only: false,
                    positional_only: false,
                    is_vararg: false,
                    is_varkwarg: true,
                });
                self.eat(&Tok::Comma);
                if !self.at(terminator) {
                    return Err(self.error("`**kwargs` must be the last parameter"));
                }
                break;
            }
            if self.eat(&Tok::Star) {
                if after_star {
                    return Err(self.error("only one `*` is allowed in a parameter list"));
                }
                after_star = true;
                seen_default = false;
                if let Tok::Name(_) = self.peek() {
                    let name = self.expect_name()?;
                    let annotation = if allow_annotations && self.eat(&Tok::Colon) {
                        Some(self.parse_expr()?)
                    } else {
                        None
                    };
                    params.push(ParamDef {
                        name,
                        annotation,
                        default: None,
                        keyword_only: false,
                        positional_only: false,
                        is_vararg: true,
                        is_varkwarg: false,
                    });
                    if self.eat(&Tok::Comma) {
                        if self.at(terminator) {
                            break;
                        }
                        continue;
                    }
                    break;
                }
                if !self.eat(&Tok::Comma) {
                    return Err(self.error("named keyword-only parameter required after `*`"));
                }
                continue;
            }
            let name = self.expect_name()?;
            let annotation = if allow_annotations && self.eat(&Tok::Colon) {
                Some(self.parse_expr()?)
            } else {
                None
            };
            let default = if self.eat(&Tok::Assign) {
                if !after_star {
                    seen_default = true;
                }
                Some(self.parse_expr()?)
            } else {
                if seen_default && !after_star {
                    return Err(
                        self.error("a non-default parameter cannot follow a default parameter")
                    );
                }
                None
            };
            params.push(ParamDef {
                name,
                annotation,
                default,
                keyword_only: after_star,
                positional_only: false,
                is_vararg: false,
                is_varkwarg: false,
            });
            if self.eat(&Tok::Comma) {
                if self.at(terminator) {
                    break;
                }
                continue;
            }
            break;
        }
        if after_star
            && !params.iter().any(|p| p.is_vararg)
            && !params.iter().any(|p| p.keyword_only)
        {
            return Err(self.error("named keyword-only parameter required after `*`"));
        }
        Ok(params)
    }

    /// Lambda parameters: the full parameter grammar (`*args`, a bare `*` + keyword-only params,
    /// `**kwargs`, `/`, defaults) but WITHOUT annotations -- a lambda's `:` ends the parameter list,
    /// so `name: type` is unavailable -- and terminated by that `:` rather than `)`.
    fn parse_lambda_params(&mut self) -> Result<Vec<ParamDef>, ParseError> {
        self.parse_param_list(&Tok::Colon, false)
    }


    fn parse_expr(&mut self) -> Result<Expr, ParseError> {
        if self.at(&Tok::KwLambda) {
            return self.parse_lambda();
        }
        if self.at(&Tok::KwYield) {
            return self.parse_yield();
        }
        let expr = self.parse_conditional()?;
        if self.at(&Tok::ColonEqual) {
            let Expr::Name(name) = expr else {
                return Err(self.error("the target of `:=` must be a bare name"));
            };
            self.advance();
            let value = self.parse_expr()?;
            return Ok(Expr::Walrus {
                target: name,
                value: Box::new(value),
            });
        }
        Ok(expr)
    }

    /// `yield_expr: "yield" [expression_list]`. A bare `yield` yields `None`; `yield e` yields `e`;
    /// and `yield a, b` yields the tuple `(a, b)` (an expression list, like a bare value position),
    /// with a `*` spread allowed after the first element. A `yield` anywhere in a function body makes
    /// it a generator. `yield from e` delegates to the sub-iterator `e` (a single expression, not an
    /// expression list).
    fn parse_yield(&mut self) -> Result<Expr, ParseError> {
        self.expect(&Tok::KwYield, "'yield'")?;
        if matches!(self.peek(), Tok::Reserved(s) if s == "from") {
            self.advance();
            let value = self.parse_expr()?;
            return Ok(Expr::YieldFrom(Box::new(value)));
        }
        let bare = matches!(
            self.peek(),
            Tok::Newline
                | Tok::RParen
                | Tok::RBracket
                | Tok::RBrace
                | Tok::Comma
                | Tok::Colon
                | Tok::Eof
        );
        if bare {
            return Ok(Expr::Yield(None));
        }
        let first = self.parse_expr()?;
        if !self.at(&Tok::Comma) {
            return Ok(Expr::Yield(Some(Box::new(first))));
        }
        let mut elems = vec![DisplayElem::Plain(first)];
        while self.eat(&Tok::Comma) {
            if matches!(
                self.peek(),
                Tok::Newline
                    | Tok::RParen
                    | Tok::RBracket
                    | Tok::RBrace
                    | Tok::Colon
                    | Tok::Eof
            ) {
                break;
            }
            elems.push(self.parse_display_elem()?);
        }
        Ok(Expr::Yield(Some(Box::new(build_tuple_display(elems)))))
    }

    /// `lambda_expr: "lambda" [param_list] ":" expression`. The body is a single
    /// expression (not a bare-comma tuple), so `f(lambda: x, 1)` reads the lambda body as
    /// `x` and `1` as a second argument; a trailing lambda (`x if c else lambda: 0`) needs
    /// parentheses.
    fn parse_lambda(&mut self) -> Result<Expr, ParseError> {
        self.expect(&Tok::KwLambda, "'lambda'")?;
        let params = self.parse_lambda_params()?;
        self.expect(&Tok::Colon, "':' after the lambda parameters")?;
        let body = self.parse_expr()?;
        Ok(Expr::Lambda {
            params,
            body: Box::new(body),
        })
    }

    /// `conditional: or_test ["if" or_test "else" conditional]` -- the ternary
    /// `body if test else orelse`, the lowest-precedence expression form, with the
    /// `else` branch right-associative.
    fn parse_conditional(&mut self) -> Result<Expr, ParseError> {
        let body = self.parse_or()?;
        if self.at(&Tok::KwIf) {
            self.advance();
            let test = self.parse_or()?;
            if !self.eat(&Tok::KwElse) {
                return Err(self.error("expected 'else' in a conditional expression"));
            }
            let orelse = self.parse_conditional()?;
            Ok(Expr::Conditional {
                test: Box::new(test),
                body: Box::new(body),
                orelse: Box::new(orelse),
            })
        } else {
            Ok(body)
        }
    }

    /// `or_test: and_test ("or" and_test)*` -- left-associative, just above the
    /// conditional.
    fn parse_or(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_and()?;
        while self.at(&Tok::KwOr) {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = Expr::BoolBinary {
                op: BoolOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `and_test: not_test ("and" not_test)*` -- left-associative, just above `or`.
    fn parse_and(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_not()?;
        while self.at(&Tok::KwAnd) {
            self.advance();
            let rhs = self.parse_not()?;
            lhs = Expr::BoolBinary {
                op: BoolOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `not_test: "not" not_test | comparison` -- right-associative, just above `and`
    /// and below a comparison (so `not a < b` is `not (a < b)`).
    fn parse_not(&mut self) -> Result<Expr, ParseError> {
        if self.at(&Tok::KwNot) {
            self.advance();
            let operand = self.parse_not()?;
            Ok(Expr::Not {
                operand: Box::new(operand),
            })
        } else {
            self.parse_comparison()
        }
    }

    /// A comparison, including Python's chains (`a < b < c`), which desugar to the
    /// `and` of the adjacent comparisons -- `(a < b) and (b < c)`. A shared middle
    /// operand is re-evaluated per comparison (exact for the side-effect-free
    /// operands chains typically use, e.g. `0 <= i < n`).
    fn parse_comparison(&mut self) -> Result<Expr, ParseError> {
        let first = self.parse_bitor()?;
        if self.peek_cmp_op().is_none() {
            return Ok(first);
        }
        let mut lhs = first;
        let mut chain: Option<Expr> = None;
        let mut middle = 0u32;
        while let Some((op, width)) = self.peek_cmp_op() {
            for _ in 0..width {
                self.advance();
            }
            let rhs = self.parse_bitor()?;
            let (this_rhs, next_lhs) = if self.peek_cmp_op().is_some() {
                if matches!(
                    &rhs,
                    Expr::Name(_)
                        | Expr::Int(_)
                        | Expr::Float(_)
                        | Expr::Imaginary(_)
                        | Expr::Bool(_)
                        | Expr::None
                        | Expr::Str(_)
                ) {
                    (rhs.clone(), rhs)
                } else {
                    let temp = format!(".cmp{middle}");
                    middle += 1;
                    let bound = Expr::Walrus {
                        target: temp.clone(),
                        value: Box::new(rhs),
                    };
                    (bound, Expr::Name(temp))
                }
            } else {
                (rhs, Expr::None)
            };
            let cmp = Expr::Compare {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(this_rhs),
            };
            chain = Some(match chain {
                None => cmp,
                Some(prev) => Expr::BoolBinary {
                    op: BoolOp::And,
                    lhs: Box::new(prev),
                    rhs: Box::new(cmp),
                },
            });
            lhs = next_lhs;
        }
        Ok(chain.expect("a comparison operator was present"))
    }

    /// The comparison operator at the cursor, with the number of tokens it spans (`not in`
    /// is two). Membership (`in` / `not in`) sits at the comparison level, like `<`.
    fn peek_cmp_op(&self) -> Option<(CmpOp, usize)> {
        Some(match self.peek() {
            Tok::EqEq => (CmpOp::Eq, 1),
            Tok::NotEq => (CmpOp::Ne, 1),
            Tok::Lt => (CmpOp::Lt, 1),
            Tok::Le => (CmpOp::Le, 1),
            Tok::Gt => (CmpOp::Gt, 1),
            Tok::Ge => (CmpOp::Ge, 1),
            Tok::KwIn => (CmpOp::In, 1),
            Tok::KwNot if matches!(self.peek2(), Tok::KwIn) => (CmpOp::NotIn, 2),
            Tok::Reserved(s) if s == "is" && matches!(self.peek2(), Tok::KwNot) => (CmpOp::IsNot, 2),
            Tok::Reserved(s) if s == "is" => (CmpOp::Is, 1),
            _ => return None,
        })
    }

    /// `or_expr: xor_expr ("|" xor_expr)*` -- bitwise OR, left-associative (Python
    /// precedence: just below comparison, just above `^`).
    fn parse_bitor(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_bitxor()?;
        while matches!(self.peek(), Tok::Pipe) {
            self.advance();
            let rhs = self.parse_bitxor()?;
            lhs = Expr::Binary {
                op: BinOp::BitOr,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `xor_expr: and_expr ("^" and_expr)*` -- bitwise XOR, left-associative.
    fn parse_bitxor(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_bitand()?;
        while matches!(self.peek(), Tok::Caret) {
            self.advance();
            let rhs = self.parse_bitand()?;
            lhs = Expr::Binary {
                op: BinOp::BitXor,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `and_expr: shift_expr ("&" shift_expr)*` -- bitwise AND, left-associative.
    fn parse_bitand(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_shift()?;
        while matches!(self.peek(), Tok::Amper) {
            self.advance();
            let rhs = self.parse_shift()?;
            lhs = Expr::Binary {
                op: BinOp::BitAnd,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `shift_expr: a_expr (("<<" | ">>") a_expr)*` -- left-associative (Python
    /// precedence: just above additive, just below `&`).
    fn parse_shift(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_additive()?;
        loop {
            let op = match self.peek() {
                Tok::LtLt => BinOp::LShift,
                Tok::GtGt => BinOp::RShift,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_additive()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `a_expr: m_expr | a_expr "+" m_expr | a_expr "-" m_expr` -- left-associative.
    fn parse_additive(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_multiplicative()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `m_expr` for the subset's operators (`*`, `//`, `%`) -- left-associative.
    /// True division `/` produces a float and is rejected.
    fn parse_multiplicative(&mut self) -> Result<Expr, ParseError> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::DoubleSlash => BinOp::FloorDiv,
                Tok::Percent => BinOp::Mod,
                Tok::Slash => BinOp::TrueDiv,
                Tok::At => BinOp::MatMul,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = Expr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    /// `u_expr: power | ("-" | "+" | "~") u_expr` -- unary minus, plus, and bitwise
    /// inversion, right-associative. A unary operator applied directly to an integer
    /// literal is folded to a constant (`-3`, `~3`); otherwise it becomes a `Unary`.
    fn parse_unary(&mut self) -> Result<Expr, ParseError> {
        let op = match self.peek() {
            Tok::Minus => UnaryOp::Neg,
            Tok::Plus => UnaryOp::Pos,
            Tok::Tilde => UnaryOp::Invert,
            _ => return self.parse_power(),
        };
        let line = self.current_line();
        self.advance();
        let operand = self.parse_unary()?;
        if let Expr::Int(value) = operand {
            let folded = match op {
                UnaryOp::Neg => value.checked_neg().ok_or_else(|| ParseError {
                    line,
                    message: String::from("integer literal out of range"),
                })?,
                UnaryOp::Pos => value,
                UnaryOp::Invert => !value,
            };
            return Ok(Expr::Int(folded));
        }
        Ok(Expr::Unary {
            op,
            operand: Box::new(operand),
        })
    }

    /// `power: primary ["**" u_expr]` -- exponentiation, RIGHT-associative and binding tighter than
    /// a unary operator on its left but looser on its right: `-2 ** 2` is `-(2 ** 2)`, `2 ** -1` is
    /// `2 ** (-1)`, and `2 ** 3 ** 2` is `2 ** (3 ** 2)`. The right operand is a u_expr, so recursing
    /// through parse_unary yields both the right-associativity and a unary right operand.
    fn parse_power(&mut self) -> Result<Expr, ParseError> {
        let base = self.parse_trailer()?;
        if self.eat(&Tok::DoubleStar) {
            let exp = self.parse_unary()?;
            return Ok(Expr::Binary {
                op: BinOp::Pow,
                lhs: Box::new(base),
                rhs: Box::new(exp),
            });
        }
        Ok(base)
    }

    /// Postfix attribute reference (`primary "." identifier`), call (`primary "("
    /// [args] ")"`), and subscript (`primary "[" expr "]"`) -- all left-associative and
    /// binding tightest.
    fn parse_trailer(&mut self) -> Result<Expr, ParseError> {
        let mut expr = self.parse_atom()?;
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.advance();
                    let attr = self.expect_name()?;
                    expr = Expr::Attribute {
                        value: Box::new(expr),
                        attr,
                    };
                }
                Tok::LParen => {
                    self.advance();
                    let call_args = self.parse_args()?;
                    self.expect(&Tok::RParen, "')' closing the call")?;
                    expr = Self::build_call(Box::new(expr), call_args);
                }
                Tok::LBracket => {
                    self.advance();
                    let first = self.parse_slice_or_index()?;
                    let index = if self.at(&Tok::Comma) {
                        let mut items = vec![first];
                        while self.eat(&Tok::Comma) {
                            if self.at(&Tok::RBracket) {
                                break;
                            }
                            items.push(self.parse_slice_or_index()?);
                        }
                        Expr::Tuple(items)
                    } else {
                        first
                    };
                    self.expect(&Tok::RBracket, "']' closing the subscript")?;
                    expr = Expr::Subscript {
                        value: Box::new(expr),
                        index: Box::new(index),
                    };
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    /// A subscript index: a plain expression `s[i]`, or a slice `s[lower:upper:step]`
    /// where each part is optional (6.3.2.1). A `:` is what makes it a slice.
    fn parse_slice_or_index(&mut self) -> Result<Expr, ParseError> {
        let lower = if self.at(&Tok::Colon) {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        if !self.at(&Tok::Colon) {
            return Ok(*lower.expect("a non-slice index parsed an expression"));
        }
        self.advance();
        let upper = if self.at(&Tok::Colon) || self.at(&Tok::RBracket) {
            None
        } else {
            Some(Box::new(self.parse_expr()?))
        };
        let step = if self.eat(&Tok::Colon) {
            if self.at(&Tok::RBracket) {
                None
            } else {
                Some(Box::new(self.parse_expr()?))
            }
        } else {
            None
        };
        Ok(Expr::Slice { lower, upper, step })
    }

    /// Desugar an f-string into its literal parts and each replacement field, concatenated left to
    /// right (2.4.3). A bare field is `str(value)`; a `!r`/`!s` conversion is `repr`/`str`; a
    /// `:spec` formats via `"{:spec}".format(value)`. An empty f-string is the empty string.
    fn parse_fstring(&mut self, parts: Vec<FStringPart>) -> Result<Expr, ParseError> {
        let mut acc: Option<Expr> = None;
        for part in parts {
            let piece = match part {
                FStringPart::Literal(s) => Expr::Str(s),
                FStringPart::Expr {
                    text,
                    conversion,
                    spec,
                    debug,
                } => self.fstring_field(&text, conversion, spec.as_deref(), debug.as_deref())?,
            };
            acc = Some(match acc {
                None => piece,
                Some(prev) => Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(prev),
                    rhs: Box::new(piece),
                },
            });
        }
        Ok(acc.unwrap_or(Expr::Str(String::new())))
    }

    /// Desugar one `{expr[=][!conv][:spec]}` field. The expression is parsed; a `!r`/`!s` conversion
    /// wraps it in `repr`/`str` (yielding a string); a `:spec` wraps it in `"{:spec}".format(value)`,
    /// which reaches the interpreter's format mini-language. With neither, the field is `str(value)`.
    /// For a `{expr=}` self-documenting field, `debug` is the literal prefix (e.g. `x=`) prepended
    /// to the value, and the default rendering (no conversion, no spec) is `repr`, not `str`.
    fn fstring_field(
        &self,
        text: &str,
        conversion: Option<char>,
        spec: Option<&str>,
        debug: Option<&str>,
    ) -> Result<Expr, ParseError> {
        let mut node = self.parse_embedded_expr(text)?;
        if let Some(conv) = conversion {
            let name = match conv {
                'r' => "repr",
                's' => "str",
                'a' => {
                    return Err(self.error("f-string !a (ascii) conversion is out of the subset"));
                }
                _ => unreachable!("the lexer scans only r/s/a"),
            };
            node = Expr::Call {
                func: Box::new(Expr::Name(String::from(name))),
                args: vec![node],
                keywords: Vec::new(),
            };
        }
        let value = if let Some(spec) = spec {
            let template = match self.fstring_spec_expr(spec)? {
                None => Expr::Str(format!("{{:{spec}}}")),
                Some(spec_expr) => Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::Binary {
                        op: BinOp::Add,
                        lhs: Box::new(Expr::Str(String::from("{:"))),
                        rhs: Box::new(spec_expr),
                    }),
                    rhs: Box::new(Expr::Str(String::from("}"))),
                },
            };
            Expr::Call {
                func: Box::new(Expr::Attribute {
                    value: Box::new(template),
                    attr: String::from("format"),
                }),
                args: vec![node],
                keywords: Vec::new(),
            }
        } else if conversion.is_some() {
            node
        } else if debug.is_some() {
            Expr::Call {
                func: Box::new(Expr::Name(String::from("repr"))),
                args: vec![node],
                keywords: Vec::new(),
            }
        } else {
            Expr::Call {
                func: Box::new(Expr::Attribute {
                    value: Box::new(Expr::Str(String::from("{}"))),
                    attr: String::from("format"),
                }),
                args: vec![node],
                keywords: Vec::new(),
            }
        };
        match debug {
            Some(prefix) => Ok(Expr::Binary {
                op: BinOp::Add,
                lhs: Box::new(Expr::Str(String::from(prefix))),
                rhs: Box::new(value),
            }),
            None => Ok(value),
        }
    }

    /// If a format spec itself contains nested `{expr}` replacement fields (`f"{x:{w}}"`,
    /// `f"{x:.{p}f}"`), build the spec as a runtime string: literal chunks and `str(expr)` for each
    /// nested field, concatenated left to right. Returns `None` for a plain spec with no nested
    /// field (the caller then uses a compile-time template). A nested field is a plain expression
    /// (its own conversion/spec is out of the subset); `{{`/`}}` are literal braces.
    fn fstring_spec_expr(&self, spec: &str) -> Result<Option<Expr>, ParseError> {
        if !spec.contains('{') {
            return Ok(None);
        }
        let chars: Vec<char> = spec.chars().collect();
        let mut parts: Vec<Expr> = Vec::new();
        let mut literal = String::new();
        let mut i = 0;
        while i < chars.len() {
            match chars[i] {
                '{' if chars.get(i + 1) == Some(&'{') => {
                    literal.push('{');
                    i += 2;
                }
                '}' if chars.get(i + 1) == Some(&'}') => {
                    literal.push('}');
                    i += 2;
                }
                '{' => {
                    if !literal.is_empty() {
                        parts.push(Expr::Str(core::mem::take(&mut literal)));
                    }
                    i += 1;
                    let start = i;
                    let mut depth = 0i32;
                    while i < chars.len() && !(chars[i] == '}' && depth == 0) {
                        match chars[i] {
                            '(' | '[' | '{' => depth += 1,
                            ')' | ']' | '}' => depth -= 1,
                            _ => {}
                        }
                        i += 1;
                    }
                    if i >= chars.len() {
                        return Err(self.error("unterminated nested field in an f-string format spec"));
                    }
                    let src: String = chars[start..i].iter().collect();
                    i += 1;
                    let expr = self.parse_embedded_expr(&src)?;
                    parts.push(Expr::Call {
                        func: Box::new(Expr::Name(String::from("str"))),
                        args: vec![expr],
                        keywords: Vec::new(),
                    });
                }
                c => {
                    literal.push(c);
                    i += 1;
                }
            }
        }
        if !literal.is_empty() {
            parts.push(Expr::Str(literal));
        }
        Ok(Some(
            parts
                .into_iter()
                .reduce(|a, b| Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(a),
                    rhs: Box::new(b),
                })
                .unwrap_or_else(|| Expr::Str(String::new())),
        ))
    }

    /// Re-lex and parse a replacement field's raw source as one expression. Surrounding
    /// whitespace is insignificant and trimmed, so `{ x }` and `{x = }` (whose captured source
    /// carries the spaces) re-lex without the leading run being read as indentation.
    fn parse_embedded_expr(&self, raw: &str) -> Result<Expr, ParseError> {
        let tokens = crate::lexer::tokenize(raw.trim())
            .map_err(|e| self.error(format!("in f-string expression: {}", e.message)))?;
        let mut sub = Parser { tokens, pos: 0, temp_seq: 0, line_ended: true };
        let expr = sub.parse_expr()?;
        if !matches!(sub.peek(), Tok::Newline | Tok::Eof) {
            return Err(self.error("unexpected trailing tokens in an f-string expression"));
        }
        Ok(expr)
    }

    /// A comma-separated target list `a` or `a, b, c` (a trailing comma before `in` is ok).
    fn parse_target_list(&mut self) -> Result<Vec<String>, ParseError> {
        let paren = self.eat(&Tok::LParen);
        let mut targets = vec![self.expect_name()?];
        while self.eat(&Tok::Comma) {
            if self.at(&Tok::KwIn) || (paren && self.at(&Tok::RParen)) {
                break;
            }
            targets.push(self.expect_name()?);
        }
        if paren {
            self.expect(&Tok::RParen, "')' closing the parenthesized comprehension target")?;
        }
        Ok(targets)
    }

    /// The `for target(s) in iterable [if cond ...]` clause chain of a comprehension (the
    /// first `for` not yet consumed). Multiple `for`s nest; iterables and filters parse below
    /// the conditional, so a trailing `if` is a filter, not a conditional expression.
    fn parse_comp_clauses(&mut self) -> Result<Vec<CompClause>, ParseError> {
        let mut clauses = Vec::new();
        while self.eat(&Tok::KwFor) {
            let targets = self.parse_target_list()?;
            self.expect(&Tok::KwIn, "'in' in the comprehension")?;
            let iterable = self.parse_or()?;
            let mut conditions = Vec::new();
            while self.eat(&Tok::KwIf) {
                conditions.push(self.parse_or()?);
            }
            clauses.push(CompClause {
                targets,
                iterable,
                conditions,
            });
        }
        Ok(clauses)
    }

    /// A dict display `{key: value, ...}` (the `{` not yet consumed); `{}` is the empty
    /// dict. A set display `{x, ...}` (no colon) is out of subset.
    /// One list/set display element: `*iterable` (a spread) or a plain expression.
    fn parse_display_elem(&mut self) -> Result<DisplayElem, ParseError> {
        if self.eat(&Tok::Star) {
            Ok(DisplayElem::Star(self.parse_expr()?))
        } else {
            Ok(DisplayElem::Plain(self.parse_expr()?))
        }
    }

    /// Build a dict display, desugaring any `**` unpack: `{**d1, k: v, **d2}` becomes
    /// `{_k: _v for _t in (D0, d1, ...) for _k, _v in _t.items()}` -- consecutive `key: value` pairs
    /// group into a dict literal, each `**` contributes its mapping, and the comprehension merges them
    /// left to right (later keys win, like Python). With no `**` this is a plain `Expr::Dict`.
    fn build_dict_display(&mut self, items: Vec<DictItem>) -> Expr {
        if items.iter().all(|i| matches!(i, DictItem::Pair(_, _))) {
            let pairs = items
                .into_iter()
                .map(|i| match i {
                    DictItem::Pair(k, v) => (k, v),
                    DictItem::DoubleStar(_) => unreachable!("checked all-pairs"),
                })
                .collect();
            return Expr::Dict(pairs);
        }
        let mut mappings: Vec<Expr> = Vec::new();
        let mut run: Vec<(Expr, Expr)> = Vec::new();
        for item in items {
            match item {
                DictItem::Pair(k, v) => run.push((k, v)),
                DictItem::DoubleStar(m) => {
                    if !run.is_empty() {
                        mappings.push(Expr::Dict(core::mem::take(&mut run)));
                    }
                    mappings.push(m);
                }
            }
        }
        if !run.is_empty() {
            mappings.push(Expr::Dict(run));
        }
        let t = self.fresh_temp("dm");
        let k = self.fresh_temp("dk");
        let v = self.fresh_temp("dv");
        let items_call = Expr::Call {
            func: Box::new(Expr::Attribute {
                value: Box::new(Expr::Name(t.clone())),
                attr: String::from("items"),
            }),
            args: Vec::new(),
            keywords: Vec::new(),
        };
        Expr::DictComp {
            key: Box::new(Expr::Name(k.clone())),
            value: Box::new(Expr::Name(v.clone())),
            clauses: vec![
                CompClause {
                    targets: vec![t],
                    iterable: Expr::Tuple(mappings),
                    conditions: Vec::new(),
                },
                CompClause {
                    targets: vec![k, v],
                    iterable: items_call,
                    conditions: Vec::new(),
                },
            ],
        }
    }

    fn parse_dict(&mut self) -> Result<Expr, ParseError> {
        self.advance();
        if self.at(&Tok::RBrace) {
            self.advance();
            return Ok(Expr::Dict(Vec::new()));
        }
        if self.at(&Tok::DoubleStar) {
            let first = self.parse_dict_item()?;
            return self.finish_dict(first);
        }
        if self.at(&Tok::Star) {
            let first = self.parse_display_elem()?;
            return self.finish_set(first);
        }
        let key = self.parse_expr()?;
        if !self.eat(&Tok::Colon) {
            if self.at(&Tok::KwFor) {
                let clauses = self.parse_comp_clauses()?;
                self.expect(&Tok::RBrace, "'}' closing the comprehension")?;
                return Ok(Expr::SetComp {
                    element: Box::new(key),
                    clauses,
                });
            }
            return self.finish_set(DisplayElem::Plain(key));
        }
        let value = self.parse_expr()?;
        if self.at(&Tok::KwFor) {
            let clauses = self.parse_comp_clauses()?;
            self.expect(&Tok::RBrace, "'}' closing the comprehension")?;
            return Ok(Expr::DictComp {
                key: Box::new(key),
                value: Box::new(value),
                clauses,
            });
        }
        self.finish_dict(DictItem::Pair(key, value))
    }

    /// Parse the remaining `{...}` set elements after `first` (each a value or `*iterable`), then
    /// desugar any spreads via build_set_display.
    fn finish_set(&mut self, first: DisplayElem) -> Result<Expr, ParseError> {
        let mut elems = vec![first];
        while self.eat(&Tok::Comma) {
            if self.at(&Tok::RBrace) {
                break;
            }
            elems.push(self.parse_display_elem()?);
        }
        self.expect(&Tok::RBrace, "'}' closing the set")?;
        Ok(build_set_display(elems))
    }

    /// Parse the remaining `{...}` dict entries after `first` (each `key: value` or `**mapping`),
    /// then desugar any unpacks via build_dict_display.
    fn finish_dict(&mut self, first: DictItem) -> Result<Expr, ParseError> {
        let mut items = vec![first];
        while self.eat(&Tok::Comma) {
            if self.at(&Tok::RBrace) {
                break;
            }
            items.push(self.parse_dict_item()?);
        }
        self.expect(&Tok::RBrace, "'}' closing the dict")?;
        Ok(self.build_dict_display(items))
    }

    fn parse_dict_item(&mut self) -> Result<DictItem, ParseError> {
        if self.eat(&Tok::DoubleStar) {
            Ok(DictItem::DoubleStar(self.parse_expr()?))
        } else {
            let k = self.parse_expr()?;
            self.expect(&Tok::Colon, "':' in the dict")?;
            let v = self.parse_expr()?;
            Ok(DictItem::Pair(k, v))
        }
    }

    /// A call's argument list: positional arguments, then keyword arguments `name=value`.
    /// A positional argument after a keyword argument, and a repeated keyword, are syntax
    /// errors (matching CPython). `*args`/`**kwargs` unpacking is out of this subset.
    fn parse_args(&mut self) -> Result<Vec<CallArg>, ParseError> {
        let mut out: Vec<CallArg> = Vec::new();
        let mut seen_keyword = false;
        if self.at(&Tok::RParen) {
            return Ok(out);
        }
        loop {
            if self.eat(&Tok::DoubleStar) {
                out.push(CallArg::DoubleStar(self.parse_expr()?));
                seen_keyword = true;
            } else if self.eat(&Tok::Star) {
                out.push(CallArg::Star(self.parse_expr()?));
            } else if matches!(self.peek(), Tok::Name(_)) && matches!(self.peek2(), Tok::Assign) {
                let name = self.expect_name()?;
                self.advance();
                if out
                    .iter()
                    .any(|a| matches!(a, CallArg::Keyword(n, _) if *n == name))
                {
                    return Err(self.error(format!("keyword argument '{name}' repeated")));
                }
                out.push(CallArg::Keyword(name, self.parse_expr()?));
                seen_keyword = true;
            } else {
                if seen_keyword {
                    return Err(self.error("positional argument follows keyword argument"));
                }
                let first = self.parse_expr()?;
                if self.at(&Tok::KwFor) {
                    if !out.is_empty() {
                        return Err(self.error(
                            "a generator expression must be parenthesized unless it is the sole argument",
                        ));
                    }
                    let clauses = self.parse_comp_clauses()?;
                    out.push(CallArg::Positional(Expr::GeneratorExp {
                        element: Box::new(first),
                        clauses,
                    }));
                    break;
                }
                out.push(CallArg::Positional(first));
            }
            if self.eat(&Tok::Comma) {
                if self.at(&Tok::RParen) {
                    break;
                }
                continue;
            }
            break;
        }
        Ok(out)
    }

    /// Lower a parsed argument list to a call expression: the simple [`Expr::Call`] when there is no
    /// `*` / `**` unpacking (so the common path stays a plain positional+keyword call), else the
    /// general [`Expr::CallEx`].
    fn build_call(func: Box<Expr>, call_args: Vec<CallArg>) -> Expr {
        let has_unpack = call_args
            .iter()
            .any(|a| matches!(a, CallArg::Star(_) | CallArg::DoubleStar(_)));
        if has_unpack {
            return Expr::CallEx {
                func,
                args: call_args,
            };
        }
        let mut args = Vec::new();
        let mut keywords = Vec::new();
        for a in call_args {
            match a {
                CallArg::Positional(e) => args.push(e),
                CallArg::Keyword(name, value) => keywords.push(Keyword { name, value }),
                CallArg::Star(_) | CallArg::DoubleStar(_) => {
                    unreachable!("guarded by has_unpack")
                }
            }
        }
        Expr::Call {
            func,
            args,
            keywords,
        }
    }

    fn parse_atom(&mut self) -> Result<Expr, ParseError> {
        match self.peek().clone() {
            Tok::Int(value) => {
                self.advance();
                Ok(Expr::Int(value))
            }
            Tok::Float(bits) => {
                self.advance();
                Ok(Expr::Float(bits))
            }
            Tok::Imaginary(bits) => {
                self.advance();
                Ok(Expr::Imaginary(bits))
            }
            Tok::BigInt(digits) => {
                let digits = digits.clone();
                self.advance();
                Ok(Expr::BigInt(digits))
            }
            Tok::Bytes(data) => {
                self.advance();
                let mut joined = data;
                while let Tok::Bytes(next) = self.peek().clone() {
                    joined.extend_from_slice(&next);
                    self.advance();
                }
                Ok(Expr::Bytes(joined))
            }
            Tok::Ellipsis => {
                self.advance();
                Ok(Expr::Name(String::from("Ellipsis")))
            }
            Tok::Str(value) => {
                self.advance();
                let mut joined = value;
                while let Tok::Str(next) = self.peek().clone() {
                    joined.push_str(&next);
                    self.advance();
                }
                Ok(Expr::Str(joined))
            }
            Tok::FString(parts) => {
                self.advance();
                self.parse_fstring(parts)
            }
            Tok::LBracket => {
                self.advance();
                if self.at(&Tok::RBracket) {
                    self.advance();
                    return Ok(Expr::List(Vec::new()));
                }
                let first = self.parse_display_elem()?;
                if matches!(first, DisplayElem::Plain(_)) && self.at(&Tok::KwFor) {
                    let DisplayElem::Plain(element) = first else {
                        unreachable!("guarded to a plain element")
                    };
                    let clauses = self.parse_comp_clauses()?;
                    self.expect(&Tok::RBracket, "']' closing the comprehension")?;
                    Ok(Expr::ListComp {
                        element: Box::new(element),
                        clauses,
                    })
                } else {
                    let mut elems = vec![first];
                    while self.eat(&Tok::Comma) {
                        if self.at(&Tok::RBracket) {
                            break;
                        }
                        elems.push(self.parse_display_elem()?);
                    }
                    self.expect(&Tok::RBracket, "']' closing the list")?;
                    Ok(build_list_display(elems))
                }
            }
            Tok::KwTrue => {
                self.advance();
                Ok(Expr::Bool(true))
            }
            Tok::KwFalse => {
                self.advance();
                Ok(Expr::Bool(false))
            }
            Tok::KwNone => {
                self.advance();
                Ok(Expr::None)
            }
            Tok::Name(name) => {
                self.advance();
                Ok(Expr::Name(name))
            }
            Tok::LParen => {
                self.advance();
                if self.at(&Tok::RParen) {
                    self.advance();
                    return Ok(Expr::Tuple(Vec::new()));
                }
                let first = self.parse_display_elem()?;
                if matches!(first, DisplayElem::Plain(_)) && self.at(&Tok::KwFor) {
                    let DisplayElem::Plain(element) = first else {
                        unreachable!("guarded to a plain element")
                    };
                    let clauses = self.parse_comp_clauses()?;
                    self.expect(&Tok::RParen, "')' closing the generator expression")?;
                    Ok(Expr::GeneratorExp {
                        element: Box::new(element),
                        clauses,
                    })
                } else if self.eat(&Tok::Comma) {
                    let mut elems = vec![first];
                    while !self.at(&Tok::RParen) {
                        elems.push(self.parse_display_elem()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(&Tok::RParen, "')'")?;
                    Ok(build_tuple_display(elems))
                } else {
                    match first {
                        DisplayElem::Plain(e) => {
                            self.expect(&Tok::RParen, "')'")?;
                            Ok(e)
                        }
                        DisplayElem::Star(_) => {
                            Err(self.error("can't use a starred expression here"))
                        }
                    }
                }
            }
            Tok::LBrace => self.parse_dict(),
            Tok::Reserved(word) => Err(self.error(format!(
                "'{word}' is a reserved keyword not supported in this subset"
            ))),
            _ => Err(self.error("expected an expression")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::tokenize;

    fn parse_src(source: &str) -> Result<ModuleAst, ParseError> {
        parse(tokenize(source).expect("tokenizes"))
    }

    fn parse_ok(source: &str) -> ModuleAst {
        parse_src(source).expect("parses")
    }

    #[test]
    fn attribute_access_is_an_expression_statement() {
        let module = parse_ok("obj.x\n");
        assert_eq!(
            module.body,
            vec![Stmt::Expr(Expr::Attribute {
                value: Box::new(Expr::Name("obj".into())),
                attr: "x".into(),
            })]
        );
    }

    #[test]
    fn return_of_a_bare_tuple() {
        let module = parse_ok("def f():\n    return a, b\n");
        let Stmt::FuncDef(f) = &module.body[0] else { panic!("a def") };
        assert_eq!(
            f.body,
            vec![Stmt::Return(Some(Expr::Tuple(vec![
                Expr::Name("a".into()),
                Expr::Name("b".into()),
            ])))]
        );
    }

    #[test]
    fn assign_a_bare_tuple_value() {
        let module = parse_ok("x = a, b\ns = 7,\n");
        assert_eq!(
            module.body[0],
            Stmt::Assign(Assign {
                target: "x".into(),
                annotation: None,
                value: Some(Expr::Tuple(vec![Expr::Name("a".into()), Expr::Name("b".into())])),
            })
        );
        assert_eq!(
            module.body[1],
            Stmt::Assign(Assign {
                target: "s".into(),
                annotation: None,
                value: Some(Expr::Tuple(vec![Expr::Int(7)])),
            })
        );
    }

    #[test]
    fn chained_assign_with_a_bare_tuple_value() {
        let module = parse_ok("a = b = 1, 2\n");
        let Stmt::MultiAssign { targets, value } = &module.body[0] else {
            panic!("a multi-assign")
        };
        assert_eq!(targets.len(), 2);
        assert_eq!(*value, Expr::Tuple(vec![Expr::Int(1), Expr::Int(2)]));
    }

    #[test]
    fn chained_assign_with_tuple_targets() {
        for src in [
            "x, y = p, q = 1, 2\n",
            "(x, y) = p, q = 1, 2\n",
            "[x, y] = p, q = 1, 2\n",
        ] {
            let module = parse_ok(src);
            let Stmt::MultiAssign { targets, value } = &module.body[0] else {
                panic!("a multi-assign: {src:?}")
            };
            assert_eq!(targets.len(), 2, "{src:?}");
            assert!(matches!(&targets[0], AssignTarget::Tuple(t) if t.len() == 2), "{src:?}");
            assert!(matches!(&targets[1], AssignTarget::Tuple(t) if t.len() == 2), "{src:?}");
            assert_eq!(*value, Expr::Tuple(vec![Expr::Int(1), Expr::Int(2)]), "{src:?}");
        }
        assert!(matches!(&parse_ok("m = n, o = 1, 2\n").body[0], Stmt::MultiAssign { targets, .. }
            if matches!(&targets[0], AssignTarget::Name(_)) && matches!(&targets[1], AssignTarget::Tuple(_))));
        assert!(matches!(&parse_ok("a, b = c = 1, 2\n").body[0], Stmt::MultiAssign { targets, .. }
            if matches!(&targets[0], AssignTarget::Tuple(_)) && matches!(&targets[1], AssignTarget::Name(_))));
        assert!(parse_src("*a, b = c, d = 1, 2, 3\n").is_err());
    }

    #[test]
    fn for_over_a_bare_tuple_iterable() {
        let module = parse_ok("for x in 1, 2, 3:\n    pass\n");
        let Stmt::ForIter { target, iterable, .. } = &module.body[0] else {
            panic!("a general for-iter")
        };
        assert_eq!(target, "x");
        assert_eq!(*iterable, Expr::Tuple(vec![Expr::Int(1), Expr::Int(2), Expr::Int(3)]));
    }

    #[test]
    fn semicolon_separated_simple_statements() {
        let module = parse_ok("a = 1; b = 2; print(a)\n");
        assert_eq!(module.body.len(), 3);
        assert!(matches!(&module.body[0], Stmt::Assign(a) if a.target == "a"));
        assert!(matches!(&module.body[1], Stmt::Assign(a) if a.target == "b"));
        assert!(matches!(&module.body[2], Stmt::Expr(_)));
    }

    #[test]
    fn inline_suite_holds_all_semicolon_statements() {
        let module = parse_ok("if x: a = 1; b = 2\nc = 3\n");
        let Stmt::If { body, .. } = &module.body[0] else { panic!("an if") };
        assert_eq!(body.len(), 2, "both inline statements are in the if body");
        assert_eq!(module.body.len(), 2, "the if, then `c = 3` -- b did not leak out");
    }

    #[test]
    fn for_with_a_nested_tuple_target() {
        let module = parse_ok("for i, (a, b) in items:\n    pass\n");
        let Stmt::ForIter { target, body, .. } = &module.body[0] else { panic!("a for-iter") };
        assert_eq!(target, ".unpack");
        let Stmt::TupleAssign { targets, .. } = &body[0] else { panic!("an unpack at body start") };
        assert_eq!(targets.len(), 2);
        assert!(matches!(&targets[0], AssignTarget::Name(n) if n == "i"));
        assert!(matches!(&targets[1], AssignTarget::Tuple(inner) if inner.len() == 2));
    }

    #[test]
    fn annotated_assignment_with_value() {
        let module = parse_ok("a: int = 0\n");
        assert_eq!(
            module.body,
            vec![Stmt::Assign(Assign {
                target: "a".into(),
                annotation: Some(Expr::Name("int".into())),
                value: Some(Expr::Int(0)),
            })]
        );
    }

    #[test]
    fn bare_annotation_has_no_value() {
        let module = parse_ok("a: int\n");
        let Stmt::Assign(assign) = &module.body[0] else {
            panic!("expected an assignment");
        };
        assert_eq!(assign.value, None);
        assert!(assign.annotation.is_some());
    }

    #[test]
    fn augmented_assignment_desugars_to_an_inplace_binary_assign() {
        let module = parse_ok("x += 5\n");
        let Stmt::Assign(assign) = &module.body[0] else {
            panic!("expected an assignment");
        };
        assert_eq!(assign.target, "x");
        assert_eq!(assign.annotation, None);
        let Some(Expr::InplaceBinary { op, lhs, .. }) = &assign.value else {
            panic!("expected an in-place binary value");
        };
        assert_eq!(*op, BinOp::Add);
        assert_eq!(**lhs, Expr::Name("x".into()));
    }

    #[test]
    fn all_augmented_operators_map_to_their_binops() {
        for (src, want) in [
            ("x += 1\n", BinOp::Add),
            ("x -= 1\n", BinOp::Sub),
            ("x *= 1\n", BinOp::Mul),
            ("x /= 1\n", BinOp::TrueDiv),
            ("x //= 1\n", BinOp::FloorDiv),
            ("x %= 1\n", BinOp::Mod),
            ("x **= 1\n", BinOp::Pow),
            ("x &= 1\n", BinOp::BitAnd),
            ("x |= 1\n", BinOp::BitOr),
            ("x ^= 1\n", BinOp::BitXor),
            ("x <<= 1\n", BinOp::LShift),
            ("x >>= 1\n", BinOp::RShift),
        ] {
            let module = parse_ok(src);
            let Stmt::Assign(assign) = &module.body[0] else {
                panic!("expected an assignment for {src:?}");
            };
            let Some(Expr::InplaceBinary { op, .. }) = &assign.value else {
                panic!("expected an in-place binary value for {src:?}");
            };
            assert_eq!(*op, want, "for source {src:?}");
        }
    }

    #[test]
    fn boolean_precedence_is_or_below_and_below_not() {
        let module = parse_ok("a or b and c\n");
        let Stmt::Expr(Expr::BoolBinary { op, rhs, .. }) = &module.body[0] else {
            panic!("expected a top-level boolean expression");
        };
        assert_eq!(*op, BoolOp::Or);
        assert!(matches!(
            **rhs,
            Expr::BoolBinary {
                op: BoolOp::And,
                ..
            }
        ));
    }

    #[test]
    fn not_binds_below_a_comparison() {
        let module = parse_ok("not a < b\n");
        let Stmt::Expr(Expr::Not { operand }) = &module.body[0] else {
            panic!("expected a top-level `not`");
        };
        assert!(matches!(**operand, Expr::Compare { .. }));
    }

    #[test]
    fn conditional_expression_is_right_associative() {
        let module = parse_ok("a if p else b if q else c\n");
        let Stmt::Expr(Expr::Conditional { orelse, .. }) = &module.body[0] else {
            panic!("expected a conditional expression");
        };
        assert!(matches!(**orelse, Expr::Conditional { .. }));
    }

    #[test]
    fn for_over_range_extracts_its_bounds() {
        let module = parse_ok("for i in range(5):\n    x = i\n");
        let Stmt::For {
            target, start, stop, ..
        } = &module.body[0]
        else {
            panic!("expected a for statement");
        };
        assert_eq!(target, "i");
        assert_eq!(*start, Expr::Int(0));
        assert_eq!(*stop, Expr::Int(5));
        let two = parse_ok("for i in range(2, 9):\n    x = i\n");
        let Stmt::For { start, stop, .. } = &two.body[0] else {
            panic!("expected a for statement");
        };
        assert_eq!(*start, Expr::Int(2));
        assert_eq!(*stop, Expr::Int(9));
    }

    #[test]
    fn for_dispatches_range_vs_general_iterable() {
        assert!(matches!(
            parse_ok("for x in range(3):\n    y = x\n").body[0],
            Stmt::For { .. }
        ));
        assert!(matches!(
            parse_ok("for x in stuff:\n    y = x\n").body[0],
            Stmt::ForIter { .. }
        ));
    }

    #[test]
    fn pass_parses_to_a_no_op() {
        assert!(matches!(parse_ok("pass\n").body[0], Stmt::Pass));
    }

    #[test]
    fn loops_take_an_optional_else_clause() {
        let with = parse_ok("for i in range(3):\n    pass\nelse:\n    pass\n");
        let Stmt::For { orelse, .. } = &with.body[0] else {
            panic!("expected a for loop");
        };
        assert_eq!(orelse.len(), 1);
        let without = parse_ok("while x:\n    pass\n");
        let Stmt::While { orelse, .. } = &without.body[0] else {
            panic!("expected a while loop");
        };
        assert!(orelse.is_empty());
    }

    #[test]
    fn fstring_desugars_to_format_and_concat() {
        let single = parse_ok("f\"{x}\"\n");
        let Stmt::Expr(Expr::Call { func, args, .. }) = &single.body[0] else {
            panic!("expected \"{{}}\".format(x)");
        };
        let Expr::Attribute { value, attr } = &**func else {
            panic!("expected a .format attribute call");
        };
        assert!(matches!(&**value, Expr::Str(s) if s == "{}"));
        assert_eq!(attr, "format");
        assert!(matches!(&args[0], Expr::Name(n) if n == "x"));
        assert!(matches!(parse_ok("f\"plain\"\n").body[0], Stmt::Expr(Expr::Str(_))));
        let braces = parse_ok("f\"{{x}}\"\n");
        let Stmt::Expr(Expr::Str(s)) = &braces.body[0] else {
            panic!("expected literal braces");
        };
        assert_eq!(s, "{x}");
        assert!(matches!(
            parse_ok("f\"a{x}\"\n").body[0],
            Stmt::Expr(Expr::Binary { .. })
        ));
    }

    #[test]
    fn fstring_debug_field_desugars_with_prefix_and_repr() {
        let m = parse_ok("f\"{x=}\"\n");
        let Stmt::Expr(Expr::Binary { op: BinOp::Add, lhs, rhs }) = &m.body[0] else {
            panic!("expected a prefix + value concatenation");
        };
        assert!(matches!(&**lhs, Expr::Str(s) if s == "x="));
        let Expr::Call { func, args, .. } = &**rhs else {
            panic!("expected repr(x)");
        };
        assert!(matches!(&**func, Expr::Name(n) if n == "repr"));
        assert!(matches!(&args[0], Expr::Name(n) if n == "x"));
        let spaced = parse_ok("f\"{ x = }\"\n");
        let Stmt::Expr(Expr::Binary { lhs, .. }) = &spaced.body[0] else {
            panic!("expected a prefix + value concatenation");
        };
        assert!(matches!(&**lhs, Expr::Str(s) if s == " x = "));
        let spec = parse_ok("f\"{x=:d}\"\n");
        let Stmt::Expr(Expr::Binary { rhs, .. }) = &spec.body[0] else {
            panic!("expected a prefix + value concatenation");
        };
        assert!(matches!(&**rhs, Expr::Call { func, .. }
            if matches!(&**func, Expr::Attribute { attr, .. } if attr == "format")));
        let cmp = parse_ok("f\"{a == b}\"\n");
        let Stmt::Expr(Expr::Call { func, args, .. }) = &cmp.body[0] else {
            panic!("expected \"{{}}\".format(a == b)");
        };
        assert!(matches!(&**func, Expr::Attribute { attr, .. } if attr == "format"));
        assert!(matches!(&args[0], Expr::Compare { .. }));
    }

    #[test]
    fn fstring_nested_spec_builds_a_dynamic_template() {
        let m = parse_ok("f\"{x:{w}}\"\n");
        let Stmt::Expr(Expr::Call { func, args, .. }) = &m.body[0] else {
            panic!("expected a .format(...) call");
        };
        let Expr::Attribute { value, attr } = &**func else {
            panic!("expected an attribute call");
        };
        assert_eq!(attr, "format");
        assert!(matches!(&**value, Expr::Binary { op: BinOp::Add, .. }));
        assert!(matches!(&args[0], Expr::Name(n) if n == "x"));
        let plain = parse_ok("f\"{x:.2f}\"\n");
        let Stmt::Expr(Expr::Call { func, .. }) = &plain.body[0] else {
            panic!("expected a .format(...) call");
        };
        let Expr::Attribute { value, .. } = &**func else {
            panic!("expected an attribute call");
        };
        assert!(matches!(&**value, Expr::Str(s) if s == "{:.2f}"));
    }

    #[test]
    fn tuple_and_dict_displays_parse() {
        assert!(matches!(parse_ok("(a, b)\n").body[0], Stmt::Expr(Expr::Tuple(ref v)) if v.len() == 2));
        assert!(matches!(parse_ok("(a,)\n").body[0], Stmt::Expr(Expr::Tuple(ref v)) if v.len() == 1));
        assert!(matches!(parse_ok("()\n").body[0], Stmt::Expr(Expr::Tuple(ref v)) if v.is_empty()));
        assert!(matches!(parse_ok("(a)\n").body[0], Stmt::Expr(Expr::Name(_))));
        assert!(matches!(parse_ok("{1: 2, 3: 4}\n").body[0], Stmt::Expr(Expr::Dict(ref p)) if p.len() == 2));
        assert!(matches!(parse_ok("{}\n").body[0], Stmt::Expr(Expr::Dict(ref p)) if p.is_empty()));
        assert!(matches!(parse_ok("{1, 2}\n").body[0], Stmt::Expr(Expr::Set(_))));
        assert!(matches!(parse_ok("{}\n").body[0], Stmt::Expr(Expr::Dict(_))));
    }

    #[test]
    fn star_in_tuple_display_desugars_to_concat() {
        assert!(matches!(
            parse_ok("x = 1, *b, 2\n").body[0],
            Stmt::Assign(Assign { value: Some(Expr::Binary { op: BinOp::Add, .. }), .. })
        ));
        assert!(matches!(
            parse_ok("(1, *b, 2)\n").body[0],
            Stmt::Expr(Expr::Binary { op: BinOp::Add, .. })
        ));
        assert!(matches!(parse_ok("(*b,)\n").body[0], Stmt::Expr(Expr::Call { .. })));
        assert!(matches!(
            parse_ok("x = *b,\n").body[0],
            Stmt::Assign(Assign { value: Some(Expr::Call { .. }), .. })
        ));
        assert!(matches!(parse_ok("(1, 2)\n").body[0], Stmt::Expr(Expr::Tuple(ref v)) if v.len() == 2));
        assert!(matches!(parse_ok("y = 1, 2, 3\n").body[0],
            Stmt::Assign(Assign { value: Some(Expr::Tuple(ref v)), .. }) if v.len() == 3));
        assert!(parse_src("z = *b\n").is_err());
        assert!(parse_src("(*b)\n").is_err());
    }

    #[test]
    fn bare_tuple_expression_statement() {
        assert!(matches!(
            parse_ok("a, b\n").body[0],
            Stmt::Expr(Expr::Tuple(ref v)) if v.len() == 2
        ));
        assert!(matches!(
            parse_ok("1, 2, 3\n").body[0],
            Stmt::Expr(Expr::Tuple(ref v)) if v.len() == 3
        ));
        assert!(matches!(
            parse_ok("a,\n").body[0],
            Stmt::Expr(Expr::Tuple(ref v)) if v.len() == 1
        ));
        assert!(matches!(parse_ok("a, *b\n").body[0], Stmt::Expr(Expr::Binary { .. })));
        assert!(matches!(parse_ok("*b, a\n").body[0], Stmt::Expr(Expr::Binary { .. })));
        assert!(matches!(parse_ok("a, b = p\n").body[0], Stmt::TupleAssign { .. }));
        assert!(matches!(parse_ok("*a, b = p\n").body[0], Stmt::TupleAssign { .. }));
        assert!(parse_src("*b\n").is_err());
    }

    #[test]
    fn matmul_operator_parses_as_infix_binary() {
        assert!(matches!(
            parse_ok("a @ b\n").body[0],
            Stmt::Expr(Expr::Binary { op: BinOp::MatMul, .. })
        ));
        let Stmt::Expr(Expr::Binary { op: BinOp::Add, lhs, .. }) = &parse_ok("a @ b + c\n").body[0]
        else {
            panic!("expected (a @ b) + c");
        };
        assert!(matches!(&**lhs, Expr::Binary { op: BinOp::MatMul, .. }));
        assert!(matches!(
            parse_ok("a @= b\n").body[0],
            Stmt::Assign(Assign { value: Some(Expr::InplaceBinary { op: BinOp::MatMul, .. }), .. })
        ));
        assert!(matches!(
            parse_ok("@d\ndef f():\n    pass\n").body[0],
            Stmt::Decorated { .. }
        ));
    }

    #[test]
    fn tuple_unpacking_parses() {
        assert!(matches!(
            parse_ok("a, b = p\n").body[0],
            Stmt::TupleAssign { .. }
        ));
        let m = parse_ok("a, b = 1, 2\n");
        let Stmt::TupleAssign { targets, star, value } = &m.body[0] else {
            panic!("expected a tuple assignment");
        };
        let names: Vec<&str> = targets
            .iter()
            .map(|t| match t {
                AssignTarget::Name(n) => n.as_str(),
                _ => panic!("expected a name target"),
            })
            .collect();
        assert_eq!(names, ["a", "b"]);
        assert_eq!(*star, None);
        assert!(matches!(value, Expr::Tuple(_)));
        let f = parse_ok("for k, v in d:\n    pass\n");
        let Stmt::ForIter { body, .. } = &f.body[0] else {
            panic!("expected a for-iter");
        };
        assert!(matches!(body[0], Stmt::TupleAssign { .. }));
        assert!(matches!(
            parse_ok("for x in d:\n    pass\n").body[0],
            Stmt::ForIter { .. }
        ));
        assert!(matches!(
            &parse_ok("a, = x\n").body[0],
            Stmt::TupleAssign { targets, star: None, .. } if targets.len() == 1
        ));
    }

    #[test]
    fn starred_unpacking_parses() {
        let cases = [
            ("a, *b = seq\n", vec!["a", "b"], Some(1)),
            ("a, *b, c = seq\n", vec!["a", "b", "c"], Some(1)),
            ("*a, b = seq\n", vec!["a", "b"], Some(0)),
            ("*a, = seq\n", vec!["a"], Some(0)),
        ];
        for (src, want_targets, want_star) in cases {
            let m = parse_ok(src);
            let Stmt::TupleAssign { targets, star, .. } = &m.body[0] else {
                panic!("expected a starred assignment for {src:?}");
            };
            let names: Vec<&str> = targets
                .iter()
                .map(|t| match t {
                    AssignTarget::Name(n) => n.as_str(),
                    _ => panic!("expected a name target"),
                })
                .collect();
            assert_eq!(names, want_targets);
            assert_eq!(*star, want_star);
        }
        assert!(parse_src("a, *b, *c = seq\n").is_err());
    }

    #[test]
    fn tuple_targets_allow_subscript_and_attribute() {
        let m = parse_ok("a, xs[1], o.x = 1, 2, 3\n");
        let Stmt::TupleAssign { targets, .. } = &m.body[0] else {
            panic!("expected a tuple assignment");
        };
        assert!(matches!(targets[0], AssignTarget::Name(_)));
        assert!(matches!(targets[1], AssignTarget::Subscript { .. }));
        assert!(matches!(targets[2], AssignTarget::Attribute { .. }));
        let n = parse_ok("a, (b, c) = x\n");
        let Stmt::TupleAssign { targets, .. } = &n.body[0] else {
            panic!("expected a tuple assignment");
        };
        assert!(matches!(targets[1], AssignTarget::Tuple(_)));
        assert!(matches!(parse_ok("a, xs[1:2] = p\n").body[0], Stmt::TupleAssign { .. }));
    }

    #[test]
    fn nested_and_parenthesized_tuple_targets() {
        let m = parse_ok("(a, b) = pair\n");
        let Stmt::TupleAssign { targets, .. } = &m.body[0] else {
            panic!("expected a tuple assignment");
        };
        assert_eq!(targets.len(), 2);
        assert!(matches!(targets[0], AssignTarget::Name(_)));

        let m2 = parse_ok("a, (b, c) = row\n");
        let Stmt::TupleAssign { targets, .. } = &m2.body[0] else {
            panic!("expected a tuple assignment");
        };
        let AssignTarget::Tuple(inner) = &targets[1] else {
            panic!("expected a nested tuple target");
        };
        assert_eq!(inner.len(), 2);

        assert!(matches!(parse_ok("[a, b] = pair\n").body[0], Stmt::TupleAssign { .. }));
        assert!(parse_src("() = x\n").is_err());
    }

    #[test]
    fn exponentiation_precedence_and_associativity() {
        let m = parse_ok("x = 2 ** 3 ** 2\n");
        let Stmt::Assign(a) = &m.body[0] else { panic!("expected an assignment") };
        let Some(Expr::Binary { op: BinOp::Pow, rhs, .. }) = &a.value else {
            panic!("expected a Pow");
        };
        assert!(matches!(**rhs, Expr::Binary { op: BinOp::Pow, .. }));

        let m2 = parse_ok("y = 2 * 3 ** 2\n");
        let Stmt::Assign(a2) = &m2.body[0] else { panic!("expected an assignment") };
        let Some(Expr::Binary { op: BinOp::Mul, rhs, .. }) = &a2.value else {
            panic!("expected a Mul");
        };
        assert!(matches!(**rhs, Expr::Binary { op: BinOp::Pow, .. }));

        let m3 = parse_ok("z = -2 ** 2\n");
        let Stmt::Assign(a3) = &m3.body[0] else { panic!("expected an assignment") };
        let Some(Expr::Unary { op: UnaryOp::Neg, operand }) = &a3.value else {
            panic!("expected a unary neg");
        };
        assert!(matches!(**operand, Expr::Binary { op: BinOp::Pow, .. }));

        let m4 = parse_ok("w = 2 ** -1\n");
        let Stmt::Assign(a4) = &m4.body[0] else { panic!("expected an assignment") };
        let Some(Expr::Binary { op: BinOp::Pow, rhs, .. }) = &a4.value else {
            panic!("expected a Pow");
        };
        assert!(matches!(**rhs, Expr::Int(-1)));
    }

    #[test]
    fn walrus_parses_and_requires_a_name_target() {
        let m = parse_ok("y = (x := 5)\n");
        let Stmt::Assign(a) = &m.body[0] else {
            panic!("expected an assignment");
        };
        let Some(Expr::Walrus { target, .. }) = &a.value else {
            panic!("expected a walrus in the value");
        };
        assert_eq!(target, "x");
        assert!(parse_src("z = (a.b := 5)\n").is_err());
        assert!(parse_src("z = (a + b := 5)\n").is_err());
    }

    #[test]
    fn del_parses_names_and_a_target_list() {
        assert!(matches!(&parse_ok("del x\n").body[0], Stmt::Delete(ts) if ts.len() == 1));
        assert!(matches!(&parse_ok("del a, b, c\n").body[0], Stmt::Delete(ts) if ts.len() == 3));
        assert!(matches!(parse_ok("del xs[0]\n").body[0], Stmt::Delete(_)));
    }

    #[test]
    fn assert_desugars_to_a_conditional_raise() {
        let m = parse_ok("assert x > 0\n");
        let Stmt::If { test, body, orelse } = &m.body[0] else {
            panic!("expected the assert desugar (an if)");
        };
        assert!(matches!(test, Expr::Not { .. }));
        assert!(orelse.is_empty());
        assert_eq!(body.len(), 1);
        assert!(matches!(&body[0], Stmt::Raise { exc: Some(_), .. }));
        let m2 = parse_ok("assert ok, \"nope\"\n");
        let Stmt::If { body, .. } = &m2.body[0] else {
            panic!("expected the assert desugar");
        };
        assert!(matches!(&body[0], Stmt::Raise { exc: Some(Expr::Call { .. }), .. }));
    }

    #[test]
    fn is_and_is_not_parse_as_identity_comparisons() {
        let m = parse_ok("r = a is None\n");
        let Stmt::Assign(asn) = &m.body[0] else { panic!("expected an assignment") };
        assert!(matches!(
            asn.value,
            Some(Expr::Compare { op: CmpOp::Is, .. })
        ));
        let m2 = parse_ok("r = a is not b\n");
        let Stmt::Assign(asn2) = &m2.body[0] else { panic!("expected an assignment") };
        assert!(matches!(
            asn2.value,
            Some(Expr::Compare { op: CmpOp::IsNot, .. })
        ));
    }

    #[test]
    fn star_call_args_build_a_callex() {
        let m = parse_ok("r = f(a, *b, k=1, **c)\n");
        let Stmt::Assign(asn) = &m.body[0] else { panic!("expected an assignment") };
        let Some(Expr::CallEx { args, .. }) = &asn.value else {
            panic!("expected a CallEx");
        };
        assert!(matches!(args[0], CallArg::Positional(_)));
        assert!(matches!(args[1], CallArg::Star(_)));
        assert!(matches!(args[2], CallArg::Keyword(_, _)));
        assert!(matches!(args[3], CallArg::DoubleStar(_)));
        let plain = parse_ok("r = f(a, k=1)\n");
        let Stmt::Assign(asn2) = &plain.body[0] else { panic!("expected an assignment") };
        assert!(matches!(asn2.value, Some(Expr::Call { .. })));
    }

    #[test]
    fn decorators_wrap_a_def_or_class() {
        let m = parse_ok("@deco\ndef f():\n    return 1\n");
        let Stmt::Decorated { decorators, inner } = &m.body[0] else {
            panic!("expected a decorated def");
        };
        assert_eq!(decorators.len(), 1);
        assert!(matches!(&**inner, Stmt::FuncDef(_)));
        let m2 = parse_ok("@a\n@b\ndef g():\n    return 2\n");
        let Stmt::Decorated { decorators, .. } = &m2.body[0] else {
            panic!("expected a decorated def");
        };
        assert_eq!(decorators.len(), 2);
        assert!(matches!(
            parse_ok("@d\nclass C:\n    pass\n").body[0],
            Stmt::Decorated { .. }
        ));
    }

    #[test]
    fn match_statement_desugars_and_disambiguates() {
        let is_match = |src: &str| matches!(&parse_ok(src).body[0], Stmt::If { test: Expr::Bool(true), .. });
        assert!(is_match("match x:\n    case 1:\n        y = 2\n    case _:\n        y = 3\n"));
        for subj in ["[1, 2]", "(1, 2)", "{1: 2}", "-5", "True", "None", "x[1:2]"] {
            let src = format!("match {subj}:\n    case _:\n        y = 1\n");
            assert!(is_match(&src), "should be a match statement: {src:?}");
        }
        assert!(matches!(parse_ok("match = 5\n").body[0], Stmt::Assign(_)));
        assert!(matches!(parse_ok("match(x)\n").body[0], Stmt::Expr(_)));
        assert!(matches!(parse_ok("y = match + 1\n").body[0], Stmt::Assign(_)));
        assert!(!is_match("match[0] = 5\n"));
        assert!(!is_match("match.attr\n"));
    }

    #[test]
    fn sequence_patterns_parse_and_gate_the_subset() {
        for src in [
            "match p:\n    case (x, y):\n        pass\n",
            "match p:\n    case [a, b, c]:\n        pass\n",
            "match p:\n    case (0, y):\n        pass\n",
            "match p:\n    case (_, second, _):\n        pass\n",
            "match p:\n    case ():\n        pass\n",
            "match p:\n    case (x):\n        pass\n",
            "match p:\n    case (x,):\n        pass\n",
            "match p:\n    case [x, *rest]:\n        pass\n",
            "match p:\n    case [*init, y]:\n        pass\n",
            "match p:\n    case [a, *_, b]:\n        pass\n",
            "match p:\n    case ((a, b), c):\n        pass\n",
            "match p:\n    case [a, [b, *rest]]:\n        pass\n",
        ] {
            assert!(parse_src(src).is_ok(), "should parse: {src:?}");
        }
        for src in [
            "match p:\n    case [a, *b, *c]:\n        pass\n",
            "match p:\n    case (a | b, c):\n        pass\n",
        ] {
            assert!(parse_src(src).is_err(), "should reject: {src:?}");
        }
    }

    #[test]
    fn class_patterns_parse_and_gate_the_subset() {
        for src in [
            "match p:\n    case Point():\n        pass\n",
            "match p:\n    case int():\n        pass\n",
            "match p:\n    case Point(a):\n        pass\n",
            "match p:\n    case Point(0, y):\n        pass\n",
            "match p:\n    case int(n):\n        pass\n",
            "match p:\n    case Point(1 | 2, y):\n        pass\n",
            "match p:\n    case Point(x=0):\n        pass\n",
            "match p:\n    case Point(x=0, y=yy):\n        pass\n",
            "match p:\n    case Point(x=px, y=py):\n        pass\n",
            "match p:\n    case Point(x=1 | 2 | 3):\n        pass\n",
            "match p:\n    case Point(a, y=2):\n        pass\n",
            "match p:\n    case Point(x=0, y=0,):\n        pass\n",
            "match p:\n    case pkg.mod.Cls():\n        pass\n",
            "match p:\n    case Line(Point(0, 0), end):\n        pass\n",
            "match p:\n    case Seg(a=Point(x=0), b=Point(y=r)):\n        pass\n",
            "match p:\n    case [Point(x=0), Point(x=1)]:\n        pass\n",
            "match p:\n    case Box(items=[a, b]):\n        pass\n",
        ] {
            assert!(parse_src(src).is_ok(), "should parse: {src:?}");
        }
        for src in [
            "match p:\n    case Point(x=0, x=1):\n        pass\n",
            "match p:\n    case Point(x=0, a):\n        pass\n",
        ] {
            assert!(parse_src(src).is_err(), "should reject: {src:?}");
        }
    }

    #[test]
    fn as_patterns_capture_the_matched_value() {
        for src in [
            "match v:\n    case 0 as z:\n        pass\n",
            "match v:\n    case [a, b] as pair:\n        pass\n",
            "match v:\n    case int() as n:\n        pass\n",
            "match v:\n    case Point(x, y) as pt:\n        pass\n",
            "match v:\n    case 1 | 2 | 3 as small:\n        pass\n",
            "match v:\n    case x as whole:\n        pass\n",
            "match v:\n    case Box(item=[a] as it):\n        pass\n",
        ] {
            assert!(parse_src(src).is_ok(), "should parse: {src:?}");
        }
        assert!(matches!(
            &parse_ok("match v:\n    case [a] as p:\n        y = 1\n").body[0],
            Stmt::If { .. }
        ));
        assert!(parse_src("match v:\n    case [x] as _:\n        pass\n").is_err());
    }

    #[test]
    fn or_patterns_require_alternatives_to_bind_the_same_names() {
        for src in [
            "match v:\n    case 1 | 2 | 3:\n        pass\n",
            "match v:\n    case int() | float():\n        pass\n",
            "match v:\n    case [1] | [2, 3] | []:\n        pass\n",
            "match v:\n    case Point() | Circle():\n        pass\n",
            "match v:\n    case 1 | 'a' | None | True:\n        pass\n",
            "match v:\n    case 1 | _:\n        pass\n",
            "match v:\n    case [1 | 2, x]:\n        pass\n",
            "match v:\n    case (0, 0) | [0, 0]:\n        pass\n",
            "match v:\n    case [x] | (x,):\n        pass\n",
            "match v:\n    case [x, y] | (y, x):\n        pass\n",
        ] {
            assert!(parse_src(src).is_ok(), "should parse: {src:?}");
        }
        for src in [
            "match v:\n    case x | y:\n        pass\n",
            "match v:\n    case Point(a) | Circle():\n        pass\n",
            "match v:\n    case 1 | x:\n        pass\n",
            "match v:\n    case [x] | [x, y]:\n        pass\n",
        ] {
            assert!(parse_src(src).is_err(), "should reject: {src:?}");
        }
    }

    #[test]
    fn mapping_patterns_parse_and_gate_the_subset() {
        for src in [
            "match v:\n    case {}:\n        pass\n",
            "match v:\n    case {\"a\": x}:\n        pass\n",
            "match v:\n    case {\"a\": 1, \"b\": 2}:\n        pass\n",
            "match v:\n    case {\"a\": 1 | 2, \"b\": y}:\n        pass\n",
            "match v:\n    case {\"a\": x, **rest}:\n        pass\n",
            "match v:\n    case {**rest}:\n        pass\n",
            "match v:\n    case {\"a\": {\"b\": z}}:\n        pass\n",
            "match v:\n    case {Color.RED: x}:\n        pass\n",
            "match v:\n    case {1: a, 2: b,}:\n        pass\n",
        ] {
            assert!(parse_src(src).is_ok(), "should parse: {src:?}");
        }
        for src in [
            "match v:\n    case {x: 1}:\n        pass\n",
            "match v:\n    case {\"a\": 1, \"a\": 2}:\n        pass\n",
            "match v:\n    case {**_}:\n        pass\n",
        ] {
            assert!(parse_src(src).is_err(), "should reject: {src:?}");
        }
    }

    #[test]
    fn star_displays_desugar_but_plain_displays_do_not() {
        let value = |src: &str| -> Option<Expr> {
            match &parse_ok(src).body[0] {
                Stmt::Assign(a) => a.value.clone(),
                other => panic!("expected an assignment, got {other:?}"),
            }
        };
        assert!(matches!(value("v = [1, 2]\n"), Some(Expr::List(_))));
        assert!(!matches!(value("v = [*a, *b]\n"), Some(Expr::List(_))));
        assert!(matches!(value("v = {1, 2}\n"), Some(Expr::Set(_))));
        assert!(!matches!(value("v = {*a, 1}\n"), Some(Expr::Set(_))));
        assert!(matches!(value("v = {'k': 1}\n"), Some(Expr::Dict(_))));
        assert!(matches!(value("v = {**a, **b}\n"), Some(Expr::DictComp { .. })));
    }

    #[test]
    fn comprehensions_parse() {
        assert!(matches!(
            parse_ok("[x for x in r]\n").body[0],
            Stmt::Expr(Expr::ListComp { .. })
        ));
        let m = parse_ok("[x for a in xs if a for x in a]\n");
        let Stmt::Expr(Expr::ListComp { clauses, .. }) = &m.body[0] else {
            panic!("expected a list comprehension");
        };
        assert_eq!(clauses.len(), 2);
        assert_eq!(clauses[0].conditions.len(), 1);
        let d = parse_ok("{k: v for k, v in items}\n");
        let Stmt::Expr(Expr::DictComp { clauses, .. }) = &d.body[0] else {
            panic!("expected a dict comprehension");
        };
        assert_eq!(clauses[0].targets, ["k", "v"]);
        fn comp_targets(src: &str) -> Vec<String> {
            let m = parse_ok(src);
            let Stmt::Expr(Expr::ListComp { clauses, .. }) = &m.body[0] else {
                panic!("expected a list comprehension");
            };
            clauses[0].targets.clone()
        }
        assert_eq!(comp_targets("[a for (a, b) in pairs]\n"), ["a", "b"]);
        assert_eq!(comp_targets("[a for (a) in xs]\n"), ["a"]);
        assert_eq!(comp_targets("[a for (a,) in xs]\n"), ["a"]);
        assert_eq!(comp_targets("[a for (a, b, c) in xs]\n"), ["a", "b", "c"]);
        assert!(matches!(
            parse_ok("{k: v for k in r}\n").body[0],
            Stmt::Expr(Expr::DictComp { .. })
        ));
        assert!(matches!(parse_ok("[1, 2, 3]\n").body[0], Stmt::Expr(Expr::List(_))));
        assert!(matches!(parse_ok("{1: 2}\n").body[0], Stmt::Expr(Expr::Dict(_))));
        assert!(matches!(
            parse_ok("{x for x in r}\n").body[0],
            Stmt::Expr(Expr::SetComp { .. })
        ));
    }

    #[test]
    fn list_display_parses() {
        let m = parse_ok("[a, b, c]\n");
        let Stmt::Expr(Expr::List(items)) = &m.body[0] else {
            panic!("expected a list display");
        };
        assert_eq!(items.len(), 3);
        assert!(matches!(parse_ok("[]\n").body[0], Stmt::Expr(Expr::List(ref v)) if v.is_empty()));
        assert!(matches!(parse_ok("[1, 2,]\n").body[0], Stmt::Expr(Expr::List(ref v)) if v.len() == 2));
        assert!(matches!(
            parse_ok("[a, b][0]\n").body[0],
            Stmt::Expr(Expr::Subscript { .. })
        ));
    }

    #[test]
    fn slice_parses_with_optional_parts() {
        let sub_index = |src| {
            let m = parse_ok(src);
            let Stmt::Expr(Expr::Subscript { index, .. }) = m.body.into_iter().next().unwrap()
            else {
                panic!("expected a subscript");
            };
            *index
        };
        assert!(matches!(
            sub_index("s[1:3]\n"),
            Expr::Slice {
                lower: Some(_),
                upper: Some(_),
                step: None
            }
        ));
        assert!(matches!(
            sub_index("s[:]\n"),
            Expr::Slice {
                lower: None,
                upper: None,
                step: None
            }
        ));
        assert!(matches!(
            sub_index("s[::2]\n"),
            Expr::Slice {
                lower: None,
                upper: None,
                step: Some(_)
            }
        ));
        assert!(matches!(sub_index("s[i]\n"), Expr::Name(_)));
    }

    #[test]
    fn class_def_parses() {
        let m = parse_ok("class C(Base):\n    k = 1\n    def m(self):\n        return self.k\n");
        let Stmt::ClassDef { name, bases, body } = &m.body[0] else {
            panic!("expected a class def");
        };
        assert_eq!(name, "C");
        assert_eq!(bases.len(), 1);
        assert_eq!(body.len(), 2);
        assert!(matches!(
            &parse_ok("class D:\n    pass\n").body[0],
            Stmt::ClassDef { bases, .. } if bases.is_empty()
        ));
        assert!(matches!(
            &parse_ok("class E(A, B, C):\n    pass\n").body[0],
            Stmt::ClassDef { bases, .. } if bases.len() == 3
        ));
        assert!(parse_src("class F(metaclass=M):\n    pass\n").is_err());
        assert!(matches!(parse_ok("obj.x = 5\n").body[0], Stmt::SetAttr { .. }));
    }

    #[test]
    fn a_class_body_takes_a_docstring() {
        let m = parse_ok("class C:\n    \"\"\"doc\"\"\"\n    def m(self):\n        return 1\n");
        let Stmt::ClassDef { body, .. } = &m.body[0] else {
            panic!("expected a class def");
        };
        assert_eq!(body.len(), 2);
        assert!(matches!(&body[0], Stmt::Expr(Expr::Str(s)) if s == "doc"));
        assert!(matches!(&parse_ok("class D:\n    \"doc\"\n").body[0], Stmt::ClassDef { body, .. } if body.len() == 1));
        assert!(parse_src("class E:\n    print(1)\n    def m(self):\n        return 1\n").is_err());
        assert!(parse_src("class F:\n    x + 1\n").is_err());
    }

    #[test]
    fn try_except_and_raise_parse() {
        assert!(matches!(
            parse_ok("raise E\n").body[0],
            Stmt::Raise {
                exc: Some(_),
                cause: None
            }
        ));
        assert!(matches!(
            parse_ok("raise\n").body[0],
            Stmt::Raise {
                exc: None,
                cause: None
            }
        ));
        assert!(matches!(
            parse_ok("raise E from C\n").body[0],
            Stmt::Raise {
                exc: Some(_),
                cause: Some(_)
            }
        ));
        let src = "try:\n    x = 1\nexcept E as e:\n    x = 2\nexcept:\n    x = 3\nelse:\n    x = 4\n";
        let Stmt::Try {
            handlers, orelse, ..
        } = &parse_ok(src).body[0]
        else {
            panic!("expected a try statement");
        };
        assert_eq!(handlers.len(), 2);
        assert_eq!(handlers[0].name.as_deref(), Some("e"));
        assert!(handlers[1].typ.is_none());
        assert_eq!(orelse.len(), 1);
        assert!(parse_src("try:\n    pass\nexcept:\n    pass\nexcept E:\n    pass\n").is_err());
        assert!(parse_src("raise from C\n").is_err());
    }

    #[test]
    fn membership_parses_at_the_comparison_level() {
        assert!(matches!(
            parse_ok("x in c\n").body[0],
            Stmt::Expr(Expr::Compare { op: CmpOp::In, .. })
        ));
        assert!(matches!(
            parse_ok("x not in c\n").body[0],
            Stmt::Expr(Expr::Compare {
                op: CmpOp::NotIn,
                ..
            })
        ));
    }

    #[test]
    fn subscript_assignment_is_setitem() {
        assert!(matches!(parse_ok("c[i] = v\n").body[0], Stmt::SetItem { .. }));
        assert!(matches!(parse_ok("c[1:2] = v\n").body[0], Stmt::SetItem { .. }));
        assert!(matches!(parse_ok("a = v\n").body[0], Stmt::Assign(_)));
    }

    #[test]
    fn subscript_parses_left_associative() {
        let module = parse_ok("s[i]\n");
        let Stmt::Expr(Expr::Subscript { value, index }) = &module.body[0] else {
            panic!("expected a subscript");
        };
        assert!(matches!(&**value, Expr::Name(n) if n == "s"));
        assert!(matches!(&**index, Expr::Name(n) if n == "i"));
        let chained = parse_ok("m[i][j]\n");
        let Stmt::Expr(Expr::Subscript { value, .. }) = &chained.body[0] else {
            panic!("expected a subscript");
        };
        assert!(matches!(&**value, Expr::Subscript { .. }));
    }

    #[test]
    fn comma_subscript_is_a_tuple_index() {
        let module = parse_ok("d[1, 2]\n");
        let Stmt::Expr(Expr::Subscript { index, .. }) = &module.body[0] else {
            panic!("expected a subscript");
        };
        let Expr::Tuple(items) = &**index else { panic!("a tuple index") };
        assert_eq!(items.len(), 2);
        let one = parse_ok("d[1,]\n");
        let Stmt::Expr(Expr::Subscript { index, .. }) = &one.body[0] else {
            panic!("expected a subscript");
        };
        assert!(matches!(&**index, Expr::Tuple(items) if items.len() == 1));
    }

    #[test]
    fn ellipsis_parses_to_the_ellipsis_name() {
        assert_eq!(
            parse_ok("...\n").body[0],
            Stmt::Expr(Expr::Name("Ellipsis".into()))
        );
        assert!(parse_src("def f():\n    ...\n").is_ok());
        assert!(parse_src("x = [..., 1]\n").is_ok());
    }

    #[test]
    fn adjacent_string_literals_concatenate() {
        assert_eq!(parse_ok("\"ab\" \"cd\"\n").body[0], Stmt::Expr(Expr::Str("abcd".into())));
        assert!(matches!(
            parse_ok("\"ab\" + \"cd\"\n").body[0],
            Stmt::Expr(Expr::Binary { .. })
        ));
        assert_eq!(
            parse_ok("b\"ab\" b\"cd\"\n").body[0],
            Stmt::Expr(Expr::Bytes(b"abcd".to_vec()))
        );
        assert!(parse_src("b\"a\" \"b\"\n").is_err());
    }

    #[test]
    fn multiple_assignment_collects_targets() {
        let module = parse_ok("a = b = c = 0\n");
        let Stmt::MultiAssign { targets, value } = &module.body[0] else {
            panic!("expected a multiple assignment");
        };
        let names: Vec<&str> = targets
            .iter()
            .map(|t| match t {
                AssignTarget::Name(n) => n.as_str(),
                _ => panic!("expected a name target"),
            })
            .collect();
        assert_eq!(names, ["a", "b", "c"]);
        assert_eq!(*value, Expr::Int(0));
        assert!(matches!(parse_ok("a = 0\n").body[0], Stmt::Assign(_)));
        let m2 = parse_ok("xs[0] = obj.x = b = 5\n");
        let Stmt::MultiAssign { targets, .. } = &m2.body[0] else {
            panic!("expected a multiple assignment");
        };
        assert_eq!(targets.len(), 3);
        assert!(matches!(targets[0], AssignTarget::Subscript { .. }));
        assert!(matches!(targets[1], AssignTarget::Attribute { .. }));
        assert!(matches!(targets[2], AssignTarget::Name(_)));
    }

    #[test]
    fn chained_comparison_desugars_to_and() {
        let module = parse_ok("a < b < c\n");
        let Stmt::Expr(Expr::BoolBinary { op, lhs, rhs }) = &module.body[0] else {
            panic!("expected a boolean expression");
        };
        assert_eq!(*op, BoolOp::And);
        assert!(matches!(**lhs, Expr::Compare { .. }));
        assert!(matches!(**rhs, Expr::Compare { .. }));
        let single = parse_ok("a < b\n");
        assert!(matches!(single.body[0], Stmt::Expr(Expr::Compare { .. })));
    }

    #[test]
    fn break_and_continue_parse() {
        let module = parse_ok("while x:\n    break\n    continue\n");
        let Stmt::While { body, .. } = &module.body[0] else {
            panic!("expected a while");
        };
        assert!(matches!(body[0], Stmt::Break));
        assert!(matches!(body[1], Stmt::Continue));
    }

    #[test]
    fn range_with_a_literal_step_is_counted_else_iterates_the_value() {
        let module = parse_ok("for i in range(0, 10, 2):\n    x = i\n");
        let Stmt::For { step, .. } = &module.body[0] else {
            panic!("expected a counted for");
        };
        assert_eq!(*step, 2);
        let var_step = parse_ok("for i in range(0, 10, n):\n    x = i\n");
        assert!(matches!(var_step.body[0], Stmt::ForIter { .. }));
        let zero_step = parse_ok("for i in range(0, 10, 0):\n    x = i\n");
        assert!(matches!(zero_step.body[0], Stmt::ForIter { .. }));
    }

    #[test]
    fn precedence_matches_the_reference() {
        let module = parse_ok("1 + 2 * 3\n");
        let Stmt::Expr(Expr::Binary { op, rhs, .. }) = &module.body[0] else {
            panic!("expected a binary expression at the top");
        };
        assert_eq!(*op, BinOp::Add);
        assert!(matches!(
            **rhs,
            Expr::Binary {
                op: BinOp::Mul,
                ..
            }
        ));
    }

    #[test]
    fn unary_minus_folds_into_a_literal() {
        let module = parse_ok("x = -3\n");
        let Stmt::Assign(assign) = &module.body[0] else {
            panic!("expected an assignment");
        };
        assert_eq!(assign.value, Some(Expr::Int(-3)));
    }

    #[test]
    fn function_with_annotations_and_a_while_loop() {
        let src = "\
def fib(n: int) -> int:
    a: int = 0
    while n > 0:
        a = a + 1
        n = n - 1
    return a
";
        let module = parse_ok(src);
        let Stmt::FuncDef(func) = &module.body[0] else {
            panic!("expected a function definition");
        };
        assert_eq!(func.name, "fib");
        assert_eq!(func.params.len(), 1);
        assert_eq!(func.params[0].name, "n");
        assert_eq!(func.params[0].annotation, Some(Expr::Name("int".into())));
        assert_eq!(func.ret, Some(Expr::Name("int".into())));
        assert!(matches!(func.body.last(), Some(Stmt::Return(Some(_)))));
        assert!(func.body.iter().any(|s| matches!(s, Stmt::While { .. })));
    }

    #[test]
    fn elif_desugars_to_a_nested_if() {
        let src = "\
if a:
    x = 1
elif b:
    x = 2
else:
    x = 3
";
        let module = parse_ok(src);
        let Stmt::If { orelse, .. } = &module.body[0] else {
            panic!("expected an if");
        };
        assert_eq!(orelse.len(), 1);
        assert!(matches!(orelse[0], Stmt::If { .. }));
    }

    #[test]
    fn single_line_suite() {
        let module = parse_ok("def f(): return 1\n");
        let Stmt::FuncDef(func) = &module.body[0] else {
            panic!("expected a function definition");
        };
        assert_eq!(func.body, vec![Stmt::Return(Some(Expr::Int(1)))]);
    }

    #[test]
    fn out_of_subset_constructs_are_rejected_clearly() {
        assert!(parse_src("xs[1:2] += p\n").is_err());
        assert!(parse_src("1 = x\n").is_err());
        assert!(parse_src("def f(a=1, b): return a\n").is_err());
        assert!(parse_src("import a.b\n").is_err());
        assert!(parse_src("from a.b import *\n").is_err());
    }

    #[test]
    fn import_star_parses() {
        let m = parse_ok("from math import *\n");
        assert_eq!(m.body, vec![Stmt::ImportStar { module: "math".into() }]);
    }

    #[test]
    fn import_statements_parse() {
        assert!(matches!(
            &parse_ok("import math\n").body[0],
            Stmt::Import { modules } if modules == &[("math".into(), "math".into())]
        ));
        assert!(matches!(
            &parse_ok("import math as m\n").body[0],
            Stmt::Import { modules } if modules == &[("math".into(), "m".into())]
        ));
        assert!(matches!(
            &parse_ok("from math import sqrt\n").body[0],
            Stmt::ImportFrom { module, names }
                if module == "math" && names == &[("sqrt".into(), "sqrt".into())]
        ));
        assert!(matches!(
            &parse_ok("from math import pi, sqrt as s\n").body[0],
            Stmt::ImportFrom { module, names }
                if module == "math"
                    && names == &[("pi".into(), "pi".into()), ("sqrt".into(), "s".into())]
        ));
    }

    #[test]
    fn augmented_subscript_and_attribute_targets_parse() {
        assert!(matches!(
            parse_ok("c[i] += 5\n").body[0],
            Stmt::SetItem { op: Some(_), .. }
        ));
        assert!(matches!(
            parse_ok("obj.x *= 2\n").body[0],
            Stmt::SetAttr { op: Some(_), .. }
        ));
        assert!(matches!(
            parse_ok("c[i] = v\n").body[0],
            Stmt::SetItem { op: None, .. }
        ));
        assert!(matches!(
            parse_ok("obj.x = v\n").body[0],
            Stmt::SetAttr { op: None, .. }
        ));
    }

    #[test]
    fn generator_expressions_parse() {
        let m = parse_ok("total = sum(x for x in xs)\n");
        let Stmt::Assign(a) = &m.body[0] else {
            panic!("expected an assignment");
        };
        let Some(Expr::Call { args, .. }) = &a.value else {
            panic!("expected a call");
        };
        assert!(matches!(args[0], Expr::GeneratorExp { .. }));
        let m2 = parse_ok("g = (x * 2 for x in xs)\n");
        let Stmt::Assign(a2) = &m2.body[0] else {
            panic!("expected an assignment");
        };
        assert!(matches!(a2.value, Some(Expr::GeneratorExp { .. })));
        assert!(parse_src("f(y, x for x in xs)\n").is_err());
    }

    #[test]
    fn default_parameter_values_parse() {
        let m = parse_ok("def f(a, b=1, c=2):\n    return a\n");
        let Stmt::FuncDef(func) = &m.body[0] else {
            panic!("expected a function definition");
        };
        assert_eq!(func.params.len(), 3);
        assert_eq!(func.params[0].default, None);
        assert_eq!(func.params[1].default, Some(Expr::Int(1)));
        assert_eq!(func.params[2].default, Some(Expr::Int(2)));
        let ann = parse_ok("def g(n: int = 5):\n    return n\n");
        let Stmt::FuncDef(func) = &ann.body[0] else {
            panic!("expected a function definition");
        };
        assert_eq!(func.params[0].annotation, Some(Expr::Name("int".into())));
        assert_eq!(func.params[0].default, Some(Expr::Int(5)));
    }

    #[test]
    fn keyword_arguments_parse() {
        let m = parse_ok("f(1, x=2, y=3)\n");
        let Stmt::Expr(Expr::Call { args, keywords, .. }) = &m.body[0] else {
            panic!("expected a call");
        };
        assert_eq!(args.as_slice(), &[Expr::Int(1)]);
        assert_eq!(keywords.len(), 2);
        assert_eq!(keywords[0].name, "x");
        assert_eq!(keywords[0].value, Expr::Int(2));
        assert_eq!(keywords[1].name, "y");
        assert_eq!(keywords[1].value, Expr::Int(3));
        let cmp = parse_ok("f(x == 1)\n");
        let Stmt::Expr(Expr::Call { args, keywords, .. }) = &cmp.body[0] else {
            panic!("expected a call");
        };
        assert!(keywords.is_empty());
        assert!(matches!(args[0], Expr::Compare { .. }));
    }

    #[test]
    fn keyword_argument_errors_match_python() {
        assert!(parse_src("f(x=1, y)\n").is_err());
        assert!(parse_src("f(x=1, x=2)\n").is_err());
        assert!(parse_src("for i in range(stop=3):\n    pass\n").is_ok());
    }

    #[test]
    fn lambda_expressions_parse() {
        let m = parse_ok("f = lambda x: x + 1\n");
        let Stmt::Assign(a) = &m.body[0] else {
            panic!("expected an assignment");
        };
        let Some(Expr::Lambda { params, body }) = &a.value else {
            panic!("expected a lambda value");
        };
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "x");
        assert!(matches!(**body, Expr::Binary { .. }));
        let z = parse_ok("g = lambda: 0\n");
        let Stmt::Assign(a) = &z.body[0] else {
            panic!("expected an assignment");
        };
        let Some(Expr::Lambda { params, body }) = &a.value else {
            panic!("expected a lambda value");
        };
        assert!(params.is_empty());
        assert_eq!(**body, Expr::Int(0));
        let d = parse_ok("h = lambda a, n=2: a + n\n");
        let Stmt::Assign(a) = &d.body[0] else {
            panic!("expected an assignment");
        };
        let Some(Expr::Lambda { params, .. }) = &a.value else {
            panic!("expected a lambda value");
        };
        assert_eq!(params.len(), 2);
        assert_eq!(params[1].default, Some(Expr::Int(2)));
        assert_eq!(params[1].annotation, None);
    }

    #[test]
    fn lambda_body_is_a_single_expression() {
        let m = parse_ok("f(lambda: x, 1)\n");
        let Stmt::Expr(Expr::Call { args, .. }) = &m.body[0] else {
            panic!("expected a call");
        };
        assert_eq!(args.len(), 2);
        assert!(matches!(&args[0], Expr::Lambda { .. }));
        assert_eq!(args[1], Expr::Int(1));
    }

    #[test]
    fn yield_expressions_parse() {
        let m = parse_ok("def g():\n    yield x\n");
        let Stmt::FuncDef(func) = &m.body[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Yield(Some(v))) = &func.body[0] else {
            panic!("expected a yield expression");
        };
        assert_eq!(**v, Expr::Name("x".into()));
        let bare = parse_ok("def g():\n    yield\n");
        let Stmt::FuncDef(func) = &bare.body[0] else {
            panic!("expected a function");
        };
        assert!(matches!(func.body[0], Stmt::Expr(Expr::Yield(None))));
        let rv = parse_ok("def g():\n    x = yield\n");
        let Stmt::FuncDef(func) = &rv.body[0] else {
            panic!("expected a function");
        };
        let Stmt::Assign(a) = &func.body[0] else {
            panic!("expected an assignment");
        };
        assert!(matches!(a.value, Some(Expr::Yield(None))));
    }

    #[test]
    fn yield_of_a_bare_tuple() {
        let m = parse_ok("def g():\n    yield a, b\n");
        let Stmt::FuncDef(func) = &m.body[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Yield(Some(v))) = &func.body[0] else {
            panic!("expected a yield expression");
        };
        assert!(matches!(&**v, Expr::Tuple(elems) if elems.len() == 2));
        let one = parse_ok("def g():\n    yield a,\n");
        let Stmt::FuncDef(func) = &one.body[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::Yield(Some(v))) = &func.body[0] else {
            panic!("expected a yield expression");
        };
        assert!(matches!(&**v, Expr::Tuple(elems) if elems.len() == 1));
        let spread = parse_ok("def g():\n    yield a, *b\n");
        let Stmt::FuncDef(func) = &spread.body[0] else {
            panic!("expected a function");
        };
        assert!(matches!(
            func.body[0],
            Stmt::Expr(Expr::Yield(Some(ref v))) if matches!(**v, Expr::Binary { .. })
        ));
    }

    #[test]
    fn yield_from_parses() {
        let m = parse_ok("def g():\n    yield from xs\n");
        let Stmt::FuncDef(func) = &m.body[0] else {
            panic!("expected a function");
        };
        let Stmt::Expr(Expr::YieldFrom(v)) = &func.body[0] else {
            panic!("expected a yield-from expression");
        };
        assert_eq!(**v, Expr::Name("xs".into()));
        let rv = parse_ok("def g():\n    x = yield from g2()\n");
        let Stmt::FuncDef(func) = &rv.body[0] else {
            panic!("expected a function");
        };
        let Stmt::Assign(a) = &func.body[0] else {
            panic!("expected an assignment");
        };
        assert!(matches!(a.value, Some(Expr::YieldFrom(_))));
    }

    #[test]
    fn keyword_only_params_parse() {
        let m = parse_ok("def f(a, *, b):\n    return b\n");
        let Stmt::FuncDef(func) = &m.body[0] else {
            panic!("expected a function");
        };
        assert_eq!(func.params.len(), 2);
        assert!(!func.params[0].keyword_only);
        assert!(func.params[1].keyword_only);
    }

    #[test]
    fn kwonly_defaults_parse_and_a_dangling_star_is_gated() {
        let module = parse_ok("def f(a, *, b=1):\n    return b\n");
        let Stmt::FuncDef(f) = &module.body[0] else { panic!("a def") };
        let b = f.params.iter().find(|p| p.name == "b").expect("param b");
        assert!(b.keyword_only && b.default.is_some(), "b is keyword-only with a default");
        assert!(parse_src("def f(a, *):\n    return a\n").is_err());
    }

    #[test]
    fn lambda_takes_the_full_parameter_grammar() {
        let module = parse_ok("f = lambda a, b=1, *args, c, d=4, **kw: a\n");
        let Stmt::Assign(assign) = &module.body[0] else { panic!("an assignment") };
        let Some(Expr::Lambda { params, .. }) = &assign.value else { panic!("a lambda value") };
        assert!(params.iter().any(|p| p.is_vararg && p.name == "args"));
        assert!(params.iter().any(|p| p.is_varkwarg && p.name == "kw"));
        assert!(params.iter().any(|p| p.keyword_only && p.name == "c" && p.default.is_none()));
        assert!(params.iter().any(|p| p.keyword_only && p.name == "d" && p.default.is_some()));
        assert!(parse_src("f = lambda a, b, /, c: a\n").is_ok());
    }

    #[test]
    fn positional_only_marker_sets_the_flag() {
        let module = parse_ok("def f(a, b, /, c):\n    return a\n");
        let Stmt::FuncDef(f) = &module.body[0] else { panic!("a def") };
        assert_eq!(f.params.len(), 3);
        assert!(f.params[0].positional_only && f.params[1].positional_only);
        assert!(!f.params[2].positional_only);
        assert!(parse_src("def f(/, a):\n    return a\n").is_err());
        assert!(parse_src("def f(a, *, b, /):\n    return b\n").is_err());
    }

    #[test]
    fn varargs_parses() {
        let m = parse_ok("def f(a, *args):\n    return a\n");
        let Stmt::FuncDef(func) = &m.body[0] else {
            panic!("expected a function");
        };
        assert_eq!(func.params.len(), 2);
        assert!(!func.params[0].is_vararg);
        assert!(func.params[1].is_vararg);
        assert!(!func.params[1].keyword_only);
        let m2 = parse_ok("def f(a, *args, b):\n    return b\n");
        let Stmt::FuncDef(func) = &m2.body[0] else {
            panic!("expected a function");
        };
        assert!(func.params[1].is_vararg);
        assert!(func.params[2].keyword_only);
    }

    #[test]
    fn varkwargs_parse() {
        let m = parse_ok("def f(a, **kw):\n    return a\n");
        let Stmt::FuncDef(func) = &m.body[0] else {
            panic!("expected a function");
        };
        assert_eq!(func.params.len(), 2);
        assert!(func.params[1].is_varkwarg);
        assert!(parse_src("def f(**kw, a):\n    return a\n").is_err());
        assert!(matches!(parse_ok("x = 2 ** 3\n").body[0], Stmt::Assign(_)));
    }

    #[test]
    fn float_literals_and_true_division_parse() {
        let m = parse_ok("x = 3.14\n");
        let Stmt::Assign(a) = &m.body[0] else {
            panic!("expected an assignment");
        };
        let Some(Expr::Float(bits)) = &a.value else {
            panic!("expected a float literal");
        };
        assert_eq!(f64::from_bits(*bits), 3.14);
        let m2 = parse_ok("y = a / b\n");
        let Stmt::Assign(a2) = &m2.body[0] else {
            panic!("expected an assignment");
        };
        assert!(matches!(
            a2.value,
            Some(Expr::Binary {
                op: BinOp::TrueDiv,
                ..
            })
        ));
        assert!(matches!(parse_ok("w = 1.5e-3\n").body[0], Stmt::Assign(_)));
    }

    #[test]
    fn with_statement_parses() {
        let m = parse_ok("with ctx() as x:\n    print(x)\n");
        let Stmt::With {
            optional_name,
            body,
            ..
        } = &m.body[0]
        else {
            panic!("expected a with statement");
        };
        assert_eq!(optional_name.as_deref(), Some("x"));
        assert_eq!(body.len(), 1);
        let m2 = parse_ok("with ctx():\n    pass\n");
        let Stmt::With { optional_name, .. } = &m2.body[0] else {
            panic!("expected a with statement");
        };
        assert!(optional_name.is_none());
    }

    #[test]
    fn parenthesized_with_items_parse() {
        assert!(parse_src("with (a() as x, b() as y):\n    pass\n").is_ok());
        assert!(parse_src("with (a(), b()):\n    pass\n").is_ok());
        assert!(parse_src("with (a() as x,):\n    pass\n").is_ok());
        let m = parse_ok("with (a() as x, b() as y):\n    pass\n");
        let Stmt::With { optional_name, body, .. } = &m.body[0] else { panic!("a with") };
        assert_eq!(optional_name.as_deref(), Some("x"));
        assert!(matches!(&body[0], Stmt::With { .. }), "the second manager nests inside");
        assert!(parse_src("with (a or b) as x:\n    pass\n").is_ok());
        assert!(parse_src("with (a() if c else b()) as x:\n    pass\n").is_ok());
    }

    #[test]
    fn multiple_context_managers_nest() {
        let module = parse_ok("with a() as x, b() as y:\n    pass\n");
        let Stmt::With { optional_name, body, .. } = &module.body[0] else { panic!("a with") };
        assert_eq!(optional_name.as_deref(), Some("x"));
        assert_eq!(body.len(), 1, "the inner with is the outer's whole body");
        let Stmt::With { optional_name: inner, .. } = &body[0] else { panic!("a nested with") };
        assert_eq!(inner.as_deref(), Some("y"));
    }
}
