#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's Python 3 front end.


extern crate alloc;

pub mod ast;
pub mod compile;
pub mod exc;
pub mod lexer;
pub mod lower;
mod named_chars;
pub mod parser;

/// The shared bytecode contract (the `lamella-py-bytecode` crate), re-exported so
/// callers can name the emitted [`bytecode::Module`] without a separate dependency.
#[doc(no_inline)]
pub use lamella_py_bytecode as bytecode;

/// A failure anywhere in the front-end pipeline: lexing, parsing, or lowering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrontendError {
    /// A lexical error.
    Lex(lexer::LexError),
    /// A syntax error.
    Parse(parser::ParseError),
    /// A lowering error (a construct outside the typed subset).
    Compile(compile::CompileError),
}

impl core::fmt::Display for FrontendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FrontendError::Lex(e) => write!(f, "lex error: {e}"),
            FrontendError::Parse(e) => write!(f, "syntax error: {e}"),
            FrontendError::Compile(e) => write!(f, "compile error: {e}"),
        }
    }
}

impl From<lexer::LexError> for FrontendError {
    fn from(e: lexer::LexError) -> Self {
        FrontendError::Lex(e)
    }
}

impl From<parser::ParseError> for FrontendError {
    fn from(e: parser::ParseError) -> Self {
        FrontendError::Parse(e)
    }
}

impl From<compile::CompileError> for FrontendError {
    fn from(e: compile::CompileError) -> Self {
        FrontendError::Compile(e)
    }
}

/// Compile Python `source` (named `module_name` for diagnostics) all the way to
/// a versioned [`bytecode::Module`]: tokenize, parse, then lower.
pub fn compile_str(
    module_name: &str,
    source: &str,
) -> Result<bytecode::Module, FrontendError> {
    let tokens = lexer::tokenize(source)?;
    let ast = parser::parse(tokens)?;
    let module = compile::compile_module(module_name, &ast)?;
    Ok(module)
}

/// Compile a multi-file Python program into a [`bytecode::Bundle`]: the `entry` module plus,
/// transitively, the managed `.py` modules its imports resolve to. `resolve(name)` returns a module's
/// source by name, or `None` for a name that stays a native / built-in module (not bundled -- e.g.
/// `math`). The import graph is walked breadth-first with a seen-set, so a module reached by several
/// paths (a diamond) or an import cycle compiles once and terminates.
///
/// Only TOP-LEVEL imports are followed into the bundle; a module reached solely through a
/// function-body import resolves at run time (native, else `ModuleNotFoundError`) -- a documented
/// first-cut narrowing.
pub fn compile_bundle(
    entry_name: &str,
    entry_source: &str,
    resolve: &dyn Fn(&str) -> Option<alloc::string::String>,
) -> Result<bytecode::Bundle, FrontendError> {
    use alloc::collections::{BTreeSet, VecDeque};
    use alloc::string::String;
    use alloc::vec::Vec;

    let entry_ast = parser::parse(lexer::tokenize(entry_source)?)?;
    let entry = compile::compile_module(entry_name, &entry_ast)?;

    let mut modules: Vec<bytecode::Module> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = top_level_imports(&entry_ast).into_iter().collect();
    while let Some(name) = queue.pop_front() {
        if !seen.insert(name.clone()) {
            continue;
        }
        let Some(source) = resolve(&name) else {
            continue;
        };
        let ast = parser::parse(lexer::tokenize(&source)?)?;
        for imported in top_level_imports(&ast) {
            if !seen.contains(&imported) {
                queue.push_back(imported);
            }
        }
        modules.push(compile::compile_module(&name, &ast)?);
    }
    Ok(bytecode::Bundle { entry, modules })
}

/// The module names a module imports at top level (`import m [, n]`, `from m import ...`, and
/// `from m import *`).
fn top_level_imports(module: &ast::ModuleAst) -> alloc::vec::Vec<alloc::string::String> {
    let mut names = alloc::vec::Vec::new();
    for stmt in &module.body {
        match stmt {
            ast::Stmt::Import { modules } => {
                for (module_name, _alias) in modules {
                    names.push(module_name.clone());
                }
            }
            ast::Stmt::ImportFrom { module, .. } => names.push(module.clone()),
            ast::Stmt::ImportStar { module } => names.push(module.clone()),
            _ => {}
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use bytecode::{FeatureFlags, Module, Op, StaticType};

    /// A typed iterative `fib` plus one dynamic attribute access. Exercises the
    /// whole pipeline end to end and round-trips through the versioned container.
    /// (The top-level `print(fib(10))` compiles too, but the typed parity slice
    /// drives the call boundary from the harness over the `fib` body.)
    const FIRST_LIGHT: &str = "\
def fib(n: int) -> int:
    a: int = 0
    b: int = 1
    i: int = 0
    while i < n:
        t: int = a + b
        a = b
        b = t
        i = i + 1
    return a

def get_x(obj) -> int:
    return obj.x

print(fib(10))
";

    #[test]
    fn first_light_program_compiles_end_to_end() {
        let module = compile_str("first_light", FIRST_LIGHT).expect("compiles");
        assert_eq!(module.functions.len(), 2);

        let fib = module.functions.iter().find(|f| f.name == "fib").unwrap();
        assert_eq!(fib.params[0].ty, StaticType::Int);
        assert_eq!(fib.ret_ty, StaticType::Int);
        assert!(fib.local_types.iter().all(|t| *t == StaticType::Int));
        assert_eq!(fib.cache_count, 0);

        let get_x = module.functions.iter().find(|f| f.name == "get_x").unwrap();
        assert_eq!(get_x.cache_count, 1);

        assert!(module.body.ops.iter().any(|op| matches!(op, Op::LoadGlobal(_))));
        assert!(module.body.ops.iter().any(|op| matches!(op, Op::Call(_))));
    }

    #[test]
    fn first_light_module_round_trips_through_the_container() {
        let module = compile_str("first_light", FIRST_LIGHT).expect("compiles");
        let bytes = module.encode(FeatureFlags::FIRST_LIGHT);
        let (decoded, features) = Module::decode(&bytes).expect("decodes");
        assert_eq!(decoded, module);
        assert!(features.contains(FeatureFlags::FIRST_LIGHT));
    }

    #[test]
    fn errors_carry_a_diagnostic() {
        let err = compile_str("m", "a = )\n").unwrap_err();
        let _: String = alloc::format!("{err}");
        assert!(matches!(err, FrontendError::Parse(_)));
    }

    #[test]
    fn compile_bundle_walks_the_import_graph() {
        let resolve = |name: &str| -> Option<String> {
            match name {
                "helpers" => Some(String::from(
                    "MAX = 10\nimport math\ndef double(x):\n    return x * 2\n",
                )),
                "config" => Some(String::from("NAME = \"cfg\"\n")),
                _ => None,
            }
        };
        let bundle = compile_bundle(
            "__main__",
            "import helpers\nfrom config import NAME\nimport math\n",
            &resolve,
        )
        .expect("compiles");
        assert_eq!(bundle.entry.name, "__main__");
        assert_eq!(bundle.modules.len(), 2);
        let names: alloc::vec::Vec<&str> = bundle.modules.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"helpers") && names.contains(&"config"));
        assert!(!names.contains(&"math"), "a native module is not bundled");
        let bytes = bundle.encode(FeatureFlags::FIRST_LIGHT);
        let (decoded, _) = bytecode::Bundle::decode(&bytes).expect("decodes");
        assert_eq!(decoded, bundle);
    }

    #[test]
    fn compile_bundle_walks_a_star_import() {
        let resolve = |name: &str| -> Option<String> {
            match name {
                "shapes" => Some(String::from("__all__ = [\"PI\"]\nPI = 3\n")),
                _ => None,
            }
        };
        let bundle = compile_bundle("__main__", "from shapes import *\nprint(PI)\n", &resolve)
            .expect("compiles");
        let names: alloc::vec::Vec<&str> = bundle.modules.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"shapes"), "a star-imported module is a bundle dependency");
    }

    #[test]
    fn compile_bundle_dedups_a_diamond_and_a_cycle() {
        let resolve = |name: &str| -> Option<String> {
            match name {
                "a" => Some(String::from("import shared\nX = 1\n")),
                "b" => Some(String::from("import shared\nY = 2\n")),
                "shared" => Some(String::from("import a\nZ = 3\n")),
                _ => None,
            }
        };
        let bundle = compile_bundle("__main__", "import a\nimport b\n", &resolve).expect("compiles");
        assert_eq!(bundle.modules.len(), 3);
    }
}
