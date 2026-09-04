#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's Python 3 front end.


extern crate alloc;

pub mod ast;
pub mod boardfacts;
pub mod compile;
pub mod complete;
pub mod exc;
pub mod lexer;
pub mod lower;
mod named_chars;
pub mod parser;
pub mod profile;

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
    /// A board fact the program names and the board does not state.
    BoardFact(boardfacts::BoardFactError),
    /// A constant the target image's capability profile cannot materialize.
    Capability(profile::CapabilityError),
    /// A failure in a module reached through the import graph, naming the module it came from.
    ///
    /// The name has to travel with the error because the inner one's line number is a line in THAT
    /// module's source, not in the source the caller handed over -- so without the name a bundle
    /// build reports a position in a file it never names, and the reader looks for it in the wrong
    /// place. A consumer that maps an error onto the caller's buffer must therefore NOT use the
    /// inner line as its own; the module's name and the inner line both survive in the `Display`
    /// text, which is where they belong until a diagnostic can carry a file.
    InModule {
        /// The module whose source failed to compile.
        module: alloc::string::String,
        /// What went wrong inside it.
        error: alloc::boxed::Box<FrontendError>,
    },
}

impl core::fmt::Display for FrontendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FrontendError::Lex(e) => write!(f, "lex error: {e}"),
            FrontendError::Parse(e) => write!(f, "syntax error: {e}"),
            FrontendError::Compile(e) => write!(f, "compile error: {e}"),
            FrontendError::BoardFact(e) => write!(f, "board fact error: {e}"),
            FrontendError::Capability(e) => write!(f, "capability error: {e}"),
            FrontendError::InModule { module, error } => write!(f, "in module '{module}': {error}"),
        }
    }
}

impl From<boardfacts::BoardFactError> for FrontendError {
    fn from(e: boardfacts::BoardFactError) -> Self {
        FrontendError::BoardFact(e)
    }
}

impl From<profile::CapabilityError> for FrontendError {
    fn from(e: profile::CapabilityError) -> Self {
        FrontendError::Capability(e)
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
///
/// Compiles for an image that provides every capability ([`profile::Profile::FULL`]). A caller
/// targeting a knob-limited tier wants [`compile_str_for_profile`], which refuses at a source line
/// what that tier's interpreter would otherwise refuse at run time.
pub fn compile_str(
    module_name: &str,
    source: &str,
) -> Result<bytecode::Module, FrontendError> {
    compile_str_for_profile(module_name, source, profile::Profile::FULL)
}

/// Compile Python `source` for an image whose capability [`profile::Profile`] is `profile`,
/// refusing anything that image could not materialize.
///
/// # Why the profile is an argument
///
/// The knobs are cargo features on the RUNTIME, and one front-end build compiles for every profile
/// -- the same process serves a host and a device tier in the browser IDE, and an on-device `eval`
/// compiles for the very image it is running inside. A `cfg!` here could only describe the machine
/// the compiler was built for, and a front end built into a device image with a mismatched feature
/// set would disagree with its own runtime in silence, because nothing compares two build
/// configurations. A value cannot drift from itself.
///
/// The refusal set is small on purpose, and the rule is not a taste call: the front end refuses
/// exactly what it cannot ENCODE, and nothing it merely cannot PREDICT -- see [`profile`] for what
/// is refused, and for the larger set that deliberately is not.
///
/// # Errors
///
/// Everything [`compile_str`] can fail with, plus [`FrontendError::Capability`] naming the missing
/// capability and the source line that needs it.
pub fn compile_str_for_profile(
    module_name: &str,
    source: &str,
    profile: profile::Profile,
) -> Result<bytecode::Module, FrontendError> {
    let tokens = lexer::tokenize(source)?;
    let mut ast = parser::parse(tokens)?;
    compile::resolve_relative_imports(&mut ast, module_name)?;
    let module = compile::compile_module(module_name, &ast)?;
    profile::check_module(&module, profile)?;
    Ok(module)
}

/// Compile Python `source` against ONE BOARD's generated facts: `import board` plus every
/// `board.*` the program reads are resolved to constants before compilation, and the import is
/// dropped. `board_source` is a generated `bsp/<board>/python/board.py`.
///
/// This is how a tier with no filesystem and no import machinery reads the same facts the
/// interpreter loads at run time -- the program is spelled identically either way; only the moment
/// the fact is bound differs. A program that never imports the board module compiles exactly as
/// [`compile_str`] would compile it.
pub fn compile_str_for_board(
    module_name: &str,
    source: &str,
    board_source: &str,
) -> Result<(bytecode::Module, usize), FrontendError> {
    compile_str_for_board_and_profile(module_name, source, board_source, profile::Profile::FULL)
}

/// [`compile_str_for_board`] against a board's facts AND a capability [`profile::Profile`] -- the
/// two halves of "compile for THIS board": what it can tell you about itself, and what its image
/// can run.
///
/// # Errors
///
/// Everything [`compile_str_for_board`] can fail with, plus [`FrontendError::Capability`].
pub fn compile_str_for_board_and_profile(
    module_name: &str,
    source: &str,
    board_source: &str,
    profile: profile::Profile,
) -> Result<(bytecode::Module, usize), FrontendError> {
    let facts = boardfacts::BoardFacts::parse(board_source)?;
    if facts.is_empty() {
        return Err(FrontendError::from(boardfacts::BoardFactError::Unparsable(
            alloc::string::String::from("it states no facts at all"),
        )));
    }
    let tokens = lexer::tokenize(source)?;
    let mut ast = parser::parse(tokens)?;
    let bound = boardfacts::fold_module(&mut ast, &facts)?;
    compile::resolve_relative_imports(&mut ast, module_name)?;
    let module = compile::compile_module(module_name, &ast)?;
    profile::check_module(&module, profile)?;
    Ok((module, bound))
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
    compile_bundle_for_profile(entry_name, entry_source, resolve, profile::Profile::FULL)
}

/// [`compile_bundle`] for an image whose capability [`profile::Profile`] is `profile`.
///
/// EVERY module in the bundle is checked, not just the entry: a bundle is the form a device is sent,
/// so an imported module's refused constant would otherwise reach the board and fail there -- the
/// exact distance this exists to close. A failure inside an imported module is named with that
/// module, as any other failure inside one is.
///
/// # Errors
///
/// Everything [`compile_bundle`] can fail with, plus [`FrontendError::Capability`].
pub fn compile_bundle_for_profile(
    entry_name: &str,
    entry_source: &str,
    resolve: &dyn Fn(&str) -> Option<alloc::string::String>,
    profile: profile::Profile,
) -> Result<bytecode::Bundle, FrontendError> {
    use alloc::collections::{BTreeSet, VecDeque};
    use alloc::string::String;
    use alloc::vec::Vec;

    let mut entry_ast = parser::parse(lexer::tokenize(entry_source)?)?;
    compile::resolve_relative_imports(&mut entry_ast, entry_name)?;
    let entry = compile::compile_module(entry_name, &entry_ast)?;
    profile::check_module(&entry, profile)?;

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
        let in_module = |e: FrontendError| FrontendError::InModule {
            module: name.clone(),
            error: alloc::boxed::Box::new(e),
        };
        let mut ast = parser::parse(lexer::tokenize(&source).map_err(|e| in_module(e.into()))?)
            .map_err(|e| in_module(e.into()))?;
        compile::resolve_relative_imports(&mut ast, &name).map_err(|e| in_module(e.into()))?;
        for imported in top_level_imports(&ast) {
            if !seen.contains(&imported) {
                queue.push_back(imported);
            }
        }
        let module = compile::compile_module(&name, &ast).map_err(|e| in_module(e.into()))?;
        profile::check_module(&module, profile).map_err(|e| in_module(e.into()))?;
        modules.push(module);
    }
    Ok(bytecode::Bundle { entry, modules })
}

/// The module names a module imports at top level (`import m [, n]`, `from m import ...`, and
/// `from m import *`).
///
/// A `from m import a` yields `m` AND `m.a`, because `a` may be a SUBMODULE rather than a name
/// inside `m` -- `from pkg import sub` is how most packages are entered, and the submodule has to
/// be in the bundle for the import to find it. Nothing here decides which it is: a `m.a` that names
/// no module simply does not resolve, and the walk skips an unresolved name as native or built-in
/// already. So the cost of the guess is one failed lookup and the cost of not guessing is a program
/// that cannot import its own package.
fn top_level_imports(module: &ast::ModuleAst) -> alloc::vec::Vec<alloc::string::String> {
    let mut names = alloc::vec::Vec::new();
    for stmt in &module.body {
        match &stmt.kind {
            ast::StmtKind::Import { modules } => {
                for (module_name, _alias) in modules {
                    names.push(module_name.clone());
                }
            }
            ast::StmtKind::ImportFrom { module, names: members, .. } => {
                names.push(module.clone());
                for (member, _bound) in members {
                    names.push(alloc::format!("{module}.{member}"));
                }
            }
            ast::StmtKind::ImportStar { module, .. } => names.push(module.clone()),
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

    #[test]
    fn a_bundled_module_that_fails_to_compile_is_named_in_the_error() {
        let resolve = |name: &str| -> Option<String> {
            match name {
                "broken" => Some(String::from("def f(:\n")),
                _ => None,
            }
        };
        let err = compile_bundle("main", "import broken\n", &resolve)
            .expect_err("the imported module does not parse");
        assert!(
            matches!(&err, FrontendError::InModule { module, .. } if module == "broken"),
            "the failing module is named: {err}"
        );
        let text = alloc::format!("{err}");
        assert!(text.contains("in module 'broken'"), "{text}");
        assert!(text.contains("line 1"), "the inner position survives too: {text}");
        assert!(compile_bundle("main", "import math\n", &resolve).is_ok());
    }

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

        let fib = module.functions.iter_bodies().find(|f| f.name == "fib").unwrap();
        assert_eq!(fib.params[0].ty, StaticType::Int);
        assert_eq!(fib.ret_ty, StaticType::Int);
        assert!(fib.local_types.iter().all(|t| *t == StaticType::Int));
        assert_eq!(fib.cache_count, 0);

        let get_x = module.functions.iter_bodies().find(|f| f.name == "get_x").unwrap();
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

    /// Lines survive the WIRE: through the trailing debug section the bytecode format reserves, and
    /// with NO change to [`lamella_py_bytecode::FORMAT_VERSION`].
    ///
    /// That reservation is what this row measures rather than argues. The section carries a line
    /// table per code object, and an artifact this build writes declares the SAME format version it
    /// declared before -- so a device whose reader predates line tables reads this artifact, skips a
    /// section it does not understand, and runs the program without them.
    #[test]
    fn line_tables_survive_the_wire_without_a_version_bump() {
        use bytecode::{FeatureFlags, FORMAT_VERSION};

        let module =
            compile_str("m", "def f(a):
    b = a
    return b
").expect("compiles");
        let source_lines: Vec<Option<u32>> = {
            let f = &module.functions[0];
            (0..f.ops.len()).map(|i| f.line_for(i)).collect()
        };
        assert!(source_lines.iter().all(Option::is_some), "the compiler produced lines");

        let bytes = module.encode(FeatureFlags::FIRST_LIGHT);
        assert_eq!(
            u16::from_le_bytes([bytes[4], bytes[5]]),
            FORMAT_VERSION,
            "carrying lines must not have moved the format version -- that is what the reservation bought"
        );

        let (back, _) = bytecode::Module::decode(&bytes).expect("decodes");
        let f = &back.functions[0];
        let decoded: Vec<Option<u32>> = (0..f.ops.len()).map(|i| f.line_for(i)).collect();
        assert_eq!(decoded, source_lines, "every op reports the same line after a round trip");
        assert_eq!(back, module, "and the module is otherwise unchanged");
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
    fn compile_bundle_resolves_relative_imports_against_each_module() {
        let resolve = |name: &str| -> Option<String> {
            match name {
                "pkg.mod" => Some(String::from("from .helpers import DOUBLE\n")),
                "pkg.sub.deep" => Some(String::from("from .helpers import DEEP\nfrom ..helpers import DOUBLE\n")),
                "pkg.helpers" => Some(String::from("DOUBLE = 2\n")),
                "pkg.sub.helpers" => Some(String::from("DEEP = 3\n")),
                _ => None,
            }
        };
        let bundle = compile_bundle("app", "import pkg.mod\nimport pkg.sub.deep\n", &resolve)
            .expect("compiles");
        let names: alloc::vec::Vec<&str> = bundle.modules.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"pkg.helpers"), "got {names:?}");
        assert!(names.contains(&"pkg.sub.helpers"), "got {names:?}");
        assert!(names.contains(&"pkg.mod") && names.contains(&"pkg.sub.deep"), "got {names:?}");
        assert!(!names.contains(&"helpers"), "a relative name reached the walk: {names:?}");
        assert!(!names.contains(&""), "an empty module name reached the walk: {names:?}");
    }

    #[test]
    fn compile_bundle_queues_a_submodule_named_by_a_from_import() {
        let resolve = |name: &str| -> Option<String> {
            match name {
                "pkg" => None,
                "pkg.sub" => Some(String::from("VALUE = 1\n")),
                "pkg.mod" => Some(String::from("from . import sub\nR = sub.VALUE\n")),
                _ => None,
            }
        };
        let bundle = compile_bundle("app", "import pkg.mod\n", &resolve).expect("compiles");
        let names: alloc::vec::Vec<&str> = bundle.modules.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"pkg.sub"), "the submodule must be bundled: {names:?}");
        assert!(names.contains(&"pkg.mod"), "got {names:?}");
        let plain = |name: &str| -> Option<String> {
            match name {
                "helpers" => Some(String::from("from math import sqrt\nX = 1\n")),
                _ => None,
            }
        };
        let bundle = compile_bundle("app", "import helpers\n", &plain).expect("compiles");
        let names: alloc::vec::Vec<&str> = bundle.modules.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["helpers"], "a non-module member must not be bundled: {names:?}");
    }

    #[test]
    fn compile_bundle_refuses_a_relative_import_with_no_package() {
        let resolve = |name: &str| -> Option<String> {
            match name {
                "loose" => Some(String::from("from . import x\n")),
                _ => None,
            }
        };
        let err = compile_bundle("app", "import loose\n", &resolve).expect_err("refuses");
        let text = alloc::format!("{err}");
        assert!(text.contains("loose"), "names the module it came from: {text}");
        assert!(text.contains("no known parent package"), "got: {text}");
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
