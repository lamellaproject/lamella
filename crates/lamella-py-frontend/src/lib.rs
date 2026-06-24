#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's Python 3 front end.


extern crate alloc;

pub mod ast;
pub mod compile;
pub mod lexer;
pub mod lower;
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
    /// A lowering error (a construct outside the first-light subset).
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

/// Compile first-light Python `source` (named `module_name` for diagnostics) all
/// the way to a versioned [`bytecode::Module`]: tokenize, parse, then lower.
pub fn compile_str(
    module_name: &str,
    source: &str,
) -> Result<bytecode::Module, FrontendError> {
    let tokens = lexer::tokenize(source)?;
    let ast = parser::parse(tokens)?;
    let module = compile::compile_module(module_name, &ast)?;
    Ok(module)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use bytecode::{FeatureFlags, Module, Op, StaticType};

    /// The first-light tracer-bullet program: a typed iterative `fib` plus one
    /// dynamic attribute access. Exercises the whole pipeline end to end and
    /// round-trips through the versioned container. (The top-level `print(fib(10))`
    /// compiles too, but the first-light parity slice drives the call boundary from
    /// the harness over the `fib` body.)
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
}
