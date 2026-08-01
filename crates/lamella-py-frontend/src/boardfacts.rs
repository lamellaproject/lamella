//! Board FACTS resolved at COMPILE time, for the tier that cannot resolve them at run time.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::ast::{
    Assign, AssignTarget, CallArg, CompClause, Expr, Keyword, ModuleAst, ParamDef, Stmt,
};

/// A value a generated board module can state: an integer, a string, or a dict of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FactValue {
    /// A number: a register address, a pin index, a divisor.
    Int(i64),
    /// Text: a board name, a role handle, a descriptive kind.
    Str(String),
    /// A named group, such as one role's descriptor.
    Dict(BTreeMap<String, FactValue>),
}

/// One board's generated facts, keyed by the module-level name that states them (`FACTS`, `BOARD`,
/// `DEVICES`, the role handles, ...).
#[derive(Debug, Clone, Default)]
pub struct BoardFacts {
    module: BTreeMap<String, FactValue>,
}

/// What went wrong resolving a board fact. Every one of these is a COMPILE error: a fact a program
/// names and the board does not state is a program that cannot run on that board, and saying so now
/// is the whole point of binding at compile time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardFactError {
    /// The board module could not be parsed.
    Unparsable(String),
    /// `board.<path>` names something the board does not state.
    Unknown {
        /// The path as the program spelled it.
        path: String,
    },
    /// `board.<path>` resolves, but not to something a program can use as a value here.
    NotAValue {
        /// The path as the program spelled it.
        path: String,
        /// What was found there instead.
        found: &'static str,
    },
    /// A subscript into a fact whose index is not a literal string.
    NonLiteralIndex {
        /// The path whose index could not be read.
        path: String,
    },
}

impl core::fmt::Display for BoardFactError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BoardFactError::Unparsable(why) => write!(f, "the board module does not parse: {why}"),
            BoardFactError::Unknown { path } => {
                write!(f, "this board states no fact `{path}`")
            }
            BoardFactError::NotAValue { path, found } => write!(
                f,
                "`{path}` is a {found}, which a compiled program cannot use as a value"
            ),
            BoardFactError::NonLiteralIndex { path } => write!(
                f,
                "`{path}` is indexed by something other than a literal string, so it cannot be \
                 resolved when the program is compiled"
            ),
        }
    }
}

impl FactValue {
    fn kind(&self) -> &'static str {
        match self {
            FactValue::Int(_) => "number",
            FactValue::Str(_) => "string",
            FactValue::Dict(_) => "group of facts",
        }
    }
}

impl BoardFacts {
    /// Read a generated board module. Statements that are not a module-level assignment of a
    /// literal are skipped: the generated file has none, and a future one gaining a shape we do not
    /// model should not fail a build over a fact nobody asked for.
    pub fn parse(source: &str) -> Result<Self, BoardFactError> {
        let tokens = crate::lexer::tokenize(source)
            .map_err(|e| BoardFactError::Unparsable(format!("{e:?}")))?;
        let ast = crate::parser::parse(tokens)
            .map_err(|e| BoardFactError::Unparsable(format!("{e:?}")))?;
        let mut module = BTreeMap::new();
        for stmt in &ast.body {
            if let Stmt::Assign(Assign {
                target,
                value: Some(value),
                ..
            }) = stmt
            {
                if let Some(fact) = literal(value) {
                    module.insert(target.clone(), fact);
                }
            }
        }
        Ok(Self { module })
    }

    /// True when the board states no facts at all -- a generated module we could not read anything
    /// out of, which is worth refusing rather than silently compiling a program with no facts.
    pub fn is_empty(&self) -> bool {
        self.module.is_empty()
    }

    fn get(&self, name: &str) -> Option<&FactValue> {
        self.module.get(name)
    }
}

/// Evaluate a literal expression to a fact. Anything else (a call, a name, arithmetic) is not a
/// fact -- the generated module contains none of it.
fn literal(expr: &Expr) -> Option<FactValue> {
    match expr {
        Expr::Int(v) => Some(FactValue::Int(*v)),
        Expr::Str(s) => Some(FactValue::Str(s.clone())),
        Expr::Bool(b) => Some(FactValue::Int(i64::from(*b))),
        Expr::Dict(entries) => {
            let mut out = BTreeMap::new();
            for (key, value) in entries {
                let Expr::Str(key) = key else { return None };
                out.insert(key.clone(), literal(value)?);
            }
            Some(FactValue::Dict(out))
        }
        _ => None,
    }
}

/// Resolve every `board.` expression in `ast` to the constant the board states, and drop the
/// `import board` that introduced it. Returns how many facts were bound.
///
/// A program that does not import the board module is returned untouched, and this reports 0.
pub fn fold_module(ast: &mut ModuleAst, facts: &BoardFacts) -> Result<usize, BoardFactError> {
    let Some(root) = imported_board_name(&ast.body) else {
        return Ok(0);
    };
    let mut folder = Folder {
        root,
        facts,
        bound: 0,
    };
    for stmt in &mut ast.body {
        folder.stmt(stmt)?;
    }
    let bound = folder.bound;
    drop_board_import(&mut ast.body);
    Ok(bound)
}

/// The local name `import board` binds, if the program imports it at all.
fn imported_board_name(body: &[Stmt]) -> Option<String> {
    for stmt in body {
        if let Stmt::Import { modules } = stmt {
            for (module, bound) in modules {
                if module == "board" {
                    return Some(bound.clone());
                }
            }
        }
    }
    None
}

/// Remove the now-resolved `import board`. Every fact it introduced is a literal by this point, so
/// leaving it would ask a device with no filesystem to import a module it does not need.
fn drop_board_import(body: &mut Vec<Stmt>) {
    body.retain(|stmt| !matches!(stmt, Stmt::Import { modules } if modules.iter().all(|(m, _)| m == "board")));
    for stmt in body.iter_mut() {
        if let Stmt::Import { modules } = stmt {
            modules.retain(|(m, _)| m != "board");
        }
    }
}

struct Folder<'a> {
    root: String,
    facts: &'a BoardFacts,
    bound: usize,
}

impl Folder<'_> {
    /// Rewrite one expression in place: resolve it if it IS a board chain, else recurse.
    fn expr(&mut self, expr: &mut Expr) -> Result<(), BoardFactError> {
        if let Some(resolved) = self.resolve(expr)? {
            *expr = resolved;
            self.bound += 1;
            return Ok(());
        }
        self.children(expr)
    }

    /// Resolve a `board.a.b["c"]` chain to a literal, or `None` when this is not such a chain.
    fn resolve(&self, expr: &Expr) -> Result<Option<Expr>, BoardFactError> {
        let Some((value, path)) = self.walk_chain(expr)? else {
            return Ok(None);
        };
        match value {
            FactValue::Int(v) => Ok(Some(Expr::Int(v))),
            FactValue::Str(s) => Ok(Some(Expr::Str(s))),
            other => Err(BoardFactError::NotAValue {
                path,
                found: other.kind(),
            }),
        }
    }

    /// Walk an attribute/subscript chain down to its root, returning the fact it names and the
    /// spelling of the path (for diagnostics). `Ok(None)` means the chain is not rooted at the
    /// board module and is none of our business.
    fn walk_chain(&self, expr: &Expr) -> Result<Option<(FactValue, String)>, BoardFactError> {
        match expr {
            Expr::Name(name) if *name == self.root => {
                Err(BoardFactError::NotAValue {
                    path: self.root.clone(),
                    found: "group of facts",
                })
            }
            Expr::Attribute { value, attr } => {
                let Some((parent, path)) = self.walk_step(value)? else {
                    return Ok(None);
                };
                let path = format!("{path}.{attr}");
                let found = match parent {
                    Some(FactValue::Dict(entries)) => entries.get(attr).cloned(),
                    None => self.facts.get(attr).cloned(),
                    Some(other) => {
                        return Err(BoardFactError::NotAValue {
                            path,
                            found: other.kind(),
                        });
                    }
                };
                found
                    .map(|v| Some((v, path.clone())))
                    .ok_or(BoardFactError::Unknown { path })
            }
            Expr::Subscript { value, index } => {
                let Some((parent, path)) = self.walk_step(value)? else {
                    return Ok(None);
                };
                let Some(parent) = parent else {
                    return Err(BoardFactError::NotAValue {
                        path,
                        found: "group of facts",
                    });
                };
                let Expr::Str(key) = &**index else {
                    return Err(BoardFactError::NonLiteralIndex { path });
                };
                let path = format!("{path}[{key:?}]");
                let FactValue::Dict(entries) = parent else {
                    return Err(BoardFactError::NotAValue {
                        path,
                        found: parent.kind(),
                    });
                };
                entries
                    .get(key)
                    .cloned()
                    .map(|v| Some((v, path.clone())))
                    .ok_or(BoardFactError::Unknown { path })
            }
            _ => Ok(None),
        }
    }

    /// One step down a chain. `Ok(Some((None, root)))` is the module itself; `Ok(None)` means this
    /// is not a board chain at all.
    #[allow(clippy::type_complexity)]
    fn walk_step(&self, expr: &Expr) -> Result<Option<(Option<FactValue>, String)>, BoardFactError> {
        if let Expr::Name(name) = expr {
            if *name == self.root {
                return Ok(Some((None, self.root.clone())));
            }
            return Ok(None);
        }
        Ok(self
            .walk_chain(expr)?
            .map(|(value, path)| (Some(value), path)))
    }

    /// Recurse into every expression a node contains. EXHAUSTIVE on purpose: a new `Expr` variant
    /// must fail this build rather than quietly become a place a board fact is not resolved.
    fn children(&mut self, expr: &mut Expr) -> Result<(), BoardFactError> {
        match expr {
            Expr::Int(_)
            | Expr::Float(_)
            | Expr::Imaginary(_)
            | Expr::BigInt(_)
            | Expr::Bytes(_)
            | Expr::Str(_)
            | Expr::Bool(_)
            | Expr::None
            | Expr::Name(_) => Ok(()),
            Expr::Attribute { value, .. } => self.expr(value),
            Expr::Binary { lhs, rhs, .. }
            | Expr::InplaceBinary { lhs, rhs, .. }
            | Expr::BoolBinary { lhs, rhs, .. }
            | Expr::Compare { lhs, rhs, .. } => {
                self.expr(lhs)?;
                self.expr(rhs)
            }
            Expr::Unary { operand, .. } | Expr::Not { operand } => self.expr(operand),
            Expr::Conditional { test, body, orelse } => {
                self.expr(test)?;
                self.expr(body)?;
                self.expr(orelse)
            }
            Expr::Call {
                func,
                args,
                keywords,
            } => {
                self.expr(func)?;
                for a in args {
                    self.expr(a)?;
                }
                for Keyword { value, .. } in keywords {
                    self.expr(value)?;
                }
                Ok(())
            }
            Expr::CallEx { func, args } => {
                self.expr(func)?;
                for arg in args {
                    match arg {
                        CallArg::Positional(e)
                        | CallArg::Star(e)
                        | CallArg::Keyword(_, e)
                        | CallArg::DoubleStar(e) => self.expr(e)?,
                    }
                }
                Ok(())
            }
            Expr::Subscript { value, index } => {
                self.expr(value)?;
                self.expr(index)
            }
            Expr::Slice { lower, upper, step } => {
                for part in [lower, upper, step].into_iter().flatten() {
                    self.expr(part)?;
                }
                Ok(())
            }
            Expr::List(items) | Expr::Tuple(items) | Expr::Set(items) => {
                for item in items {
                    self.expr(item)?;
                }
                Ok(())
            }
            Expr::Dict(entries) => {
                for (key, value) in entries {
                    self.expr(key)?;
                    self.expr(value)?;
                }
                Ok(())
            }
            Expr::ListComp { element, clauses }
            | Expr::SetComp { element, clauses }
            | Expr::GeneratorExp { element, clauses } => {
                self.expr(element)?;
                self.clauses(clauses)
            }
            Expr::DictComp {
                key,
                value,
                clauses,
            } => {
                self.expr(key)?;
                self.expr(value)?;
                self.clauses(clauses)
            }
            Expr::Lambda { params, body } => {
                self.params(params)?;
                self.expr(body)
            }
            Expr::Yield(value) => {
                if let Some(value) = value {
                    self.expr(value)?;
                }
                Ok(())
            }
            Expr::YieldFrom(value) => self.expr(value),
            Expr::Walrus { value, .. } => self.expr(value),
        }
    }

    /// Recurse into every expression a statement contains. EXHAUSTIVE for the same reason
    /// `children` is: a new `Stmt` variant should stop this build, not silently become a place
    /// where `board.…` survives compilation.
    fn stmt(&mut self, stmt: &mut Stmt) -> Result<(), BoardFactError> {
        match stmt {
            Stmt::FuncDef(func) => {
                self.params(&mut func.params)?;
                self.body(&mut func.body)
            }
            Stmt::Return(value) => {
                if let Some(value) = value {
                    self.expr(value)?;
                }
                Ok(())
            }
            Stmt::Assign(Assign { value, .. }) => {
                if let Some(value) = value {
                    self.expr(value)?;
                }
                Ok(())
            }
            Stmt::MultiAssign { targets, value } | Stmt::TupleAssign { targets, value, .. } => {
                for target in targets.iter_mut() {
                    self.target(target)?;
                }
                self.expr(value)
            }
            Stmt::SetItem {
                container,
                index,
                value,
                ..
            } => {
                self.expr(container)?;
                self.expr(index)?;
                self.expr(value)
            }
            Stmt::SetAttr { obj, value, .. } => {
                self.expr(obj)?;
                self.expr(value)
            }
            Stmt::Expr(value) => self.expr(value),
            Stmt::Delete(values) => {
                for value in values {
                    self.expr(value)?;
                }
                Ok(())
            }
            Stmt::If { test, body, orelse } | Stmt::While { test, body, orelse } => {
                self.expr(test)?;
                self.body(body)?;
                self.body(orelse)
            }
            Stmt::For {
                start,
                stop,
                body,
                orelse,
                ..
            } => {
                self.expr(start)?;
                self.expr(stop)?;
                self.body(body)?;
                self.body(orelse)
            }
            Stmt::ForIter {
                iterable,
                body,
                orelse,
                ..
            } => {
                self.expr(iterable)?;
                self.body(body)?;
                self.body(orelse)
            }
            Stmt::Raise { exc, cause } => {
                for part in [exc, cause].into_iter().flatten() {
                    self.expr(part)?;
                }
                Ok(())
            }
            Stmt::Try {
                body,
                handlers,
                orelse,
                finalbody,
            } => {
                self.body(body)?;
                for handler in handlers {
                    if let Some(typ) = &mut handler.typ {
                        self.expr(typ)?;
                    }
                    self.body(&mut handler.body)?;
                }
                self.body(orelse)?;
                self.body(finalbody)
            }
            Stmt::With { context, body, .. } => {
                self.expr(context)?;
                self.body(body)
            }
            Stmt::ClassDef { bases, body, .. } => {
                for base in bases {
                    self.expr(base)?;
                }
                self.body(body)
            }
            Stmt::Decorated { decorators, inner } => {
                for decorator in decorators {
                    self.expr(decorator)?;
                }
                self.stmt(inner)
            }
            Stmt::Nonlocal(_)
            | Stmt::Global(_)
            | Stmt::Import { .. }
            | Stmt::ImportFrom { .. }
            | Stmt::ImportStar { .. }
            | Stmt::Break
            | Stmt::Continue
            | Stmt::Pass => Ok(()),
        }
    }

    fn body(&mut self, body: &mut [Stmt]) -> Result<(), BoardFactError> {
        for stmt in body {
            self.stmt(stmt)?;
        }
        Ok(())
    }

    /// An assignment target can carry expressions too (`xs[board.FACTS[..]] = v`).
    fn target(&mut self, target: &mut AssignTarget) -> Result<(), BoardFactError> {
        match target {
            AssignTarget::Name(_) => Ok(()),
            AssignTarget::Subscript { container, index } => {
                self.expr(container)?;
                self.expr(index)
            }
            AssignTarget::Attribute { obj, .. } => self.expr(obj),
            AssignTarget::Tuple(targets) => {
                for target in targets.iter_mut() {
                    self.target(target)?;
                }
                Ok(())
            }
        }
    }

    fn clauses(&mut self, clauses: &mut [CompClause]) -> Result<(), BoardFactError> {
        for clause in clauses {
            self.expr(&mut clause.iterable)?;
            for condition in &mut clause.conditions {
                self.expr(condition)?;
            }
        }
        Ok(())
    }

    fn params(&mut self, params: &mut [ParamDef]) -> Result<(), BoardFactError> {
        for param in params {
            if let Some(default) = &mut param.default {
                self.expr(default)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    /// A trimmed generated module, in the shape `lamella-bsp-gen` emits.
    const BOARD: &str = "\
BOARD = \"microbit-v1\"
BOARD_MODEL = 1
I2C = \"i2c\"
FACTS = {
    \"i2c\": {
        \"kind\": \"i2c\",
        \"twi_base\": 0x40003000,
        \"psel_scl\": 0x0,
        \"psel_sda\": 0x1E,
    },
}
DEVICES = {
    \"display-row1\": {\"pin\": 13, \"active_low\": False},
}
";

    fn fold(source: &str) -> Result<(ModuleAst, usize), BoardFactError> {
        let facts = BoardFacts::parse(BOARD).expect("the board module parses");
        let tokens = crate::lexer::tokenize(source).expect("lexes");
        let mut ast = crate::parser::parse(tokens).expect("parses");
        let bound = fold_module(&mut ast, &facts)?;
        Ok((ast, bound))
    }

    /// The value a single-assignment program ends up with, so a test can assert the literal that
    /// replaced the chain.
    fn folded_value(source: &str) -> Expr {
        let (ast, bound) = fold(source).expect("folds");
        assert_eq!(bound, 1, "exactly one fact should have been bound");
        for stmt in &ast.body {
            if let Stmt::Assign(Assign {
                value: Some(value), ..
            }) = stmt
            {
                return value.clone();
            }
        }
        panic!("no assignment survived the fold");
    }

    #[test]
    fn a_nested_fact_becomes_an_integer_literal() {
        assert_eq!(
            folded_value("import board\nx = board.FACTS[\"i2c\"][\"twi_base\"]\n"),
            Expr::Int(0x4000_3000)
        );
    }

    #[test]
    fn a_module_level_constant_resolves_too() {
        assert_eq!(folded_value("import board\nx = board.BOARD_MODEL\n"), Expr::Int(1));
        assert_eq!(
            folded_value("import board\nx = board.BOARD\n"),
            Expr::Str("microbit-v1".to_string())
        );
    }

    #[test]
    fn a_bool_fact_is_an_integer_like_python() {
        assert_eq!(
            folded_value("import board\nx = board.DEVICES[\"display-row1\"][\"active_low\"]\n"),
            Expr::Int(0)
        );
    }

    /// The property the differential corpus depends on: no import, no change.
    #[test]
    fn a_program_that_does_not_import_the_board_is_untouched() {
        let source = "x = 1\ny = x + 2\n";
        let (ast, bound) = fold(source).expect("folds");
        assert_eq!(bound, 0);
        let tokens = crate::lexer::tokenize(source).expect("lexes");
        let untouched = crate::parser::parse(tokens).expect("parses");
        assert_eq!(ast, untouched, "a program without the import must be identical");
    }

    /// A local called `board` is not the board module.
    #[test]
    fn a_local_named_board_is_left_alone() {
        let (_, bound) = fold("board = 1\nx = board\n").expect("folds");
        assert_eq!(bound, 0);
    }

    #[test]
    fn the_resolved_import_is_dropped() {
        let (ast, _) = fold("import board\nx = board.BOARD_MODEL\n").expect("folds");
        assert!(
            !ast.body.iter().any(|s| matches!(s, Stmt::Import { .. })),
            "the import is fully resolved, so nothing should ask a device to perform it"
        );
    }

    /// An import of something else survives, and its own names are none of our business.
    #[test]
    fn an_unrelated_import_survives() {
        let (ast, _) = fold("import board\nimport math\nx = board.BOARD_MODEL\n").expect("folds");
        assert!(ast.body.iter().any(
            |s| matches!(s, Stmt::Import { modules } if modules.iter().any(|(m, _)| m == "math"))
        ));
    }

    #[test]
    fn a_fact_this_board_does_not_state_is_an_error() {
        let err = fold("import board\nx = board.FACTS[\"spi\"][\"base\"]\n").unwrap_err();
        assert_eq!(
            err,
            BoardFactError::Unknown {
                path: "board.FACTS[\"spi\"]".to_string()
            }
        );
    }

    #[test]
    fn a_group_of_facts_is_not_a_value() {
        let err = fold("import board\nx = board.FACTS[\"i2c\"]\n").unwrap_err();
        assert_eq!(
            err,
            BoardFactError::NotAValue {
                path: "board.FACTS[\"i2c\"]".to_string(),
                found: "group of facts"
            }
        );
    }

    #[test]
    fn an_index_that_is_not_a_literal_cannot_be_resolved_at_compile_time() {
        let err = fold("import board\nrole = \"i2c\"\nx = board.FACTS[role]\n").unwrap_err();
        assert_eq!(
            err,
            BoardFactError::NonLiteralIndex {
                path: "board.FACTS".to_string()
            }
        );
    }

    /// The walker has to reach everywhere, so this puts a fact inside a function, inside a loop,
    /// inside a call argument, inside a binary expression.
    #[test]
    fn a_fact_nested_deep_in_a_function_still_resolves() {
        let (_, bound) = fold(
            "import board\n\
             def go() -> int:\n\
             \x20   total = 0\n\
             \x20   while total < 3:\n\
             \x20       total = total + mmio_read32(board.FACTS[\"i2c\"][\"twi_base\"] + 4)\n\
             \x20   return total\n",
        )
        .expect("folds");
        assert_eq!(bound, 1);
    }

    /// Every fact site is bound, not just the first.
    #[test]
    fn every_site_is_bound() {
        let (_, bound) = fold(
            "import board\n\
             a = board.FACTS[\"i2c\"][\"twi_base\"]\n\
             b = board.FACTS[\"i2c\"][\"psel_scl\"]\n\
             c = board.FACTS[\"i2c\"][\"psel_sda\"]\n",
        )
        .expect("folds");
        assert_eq!(bound, 3);
    }
}
