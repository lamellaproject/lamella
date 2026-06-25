//! The Python embedding: compile a Python source string and run its `main()`.

use crate::{Diagnostic, RunResult};
use lamella_py_runtime::{ObjectModel, run};

/// Compiles `source` and runs its `main()`, capturing the rendered result and any
/// compile/runtime diagnostics. Never panics: a parse error or a trap becomes a
/// diagnostic, exactly as [`crate::run_bytes`] does for C#.
#[must_use]
pub fn run_py_str(source: &str) -> RunResult {
    let module = match lamella_py_frontend::compile_str("main", source) {
        Ok(module) => module,
        Err(error) => return diagnostic_result("PY-COMPILE", format!("{error}")),
    };
    let Some(main_co) = module
        .functions
        .iter()
        .find(|function| function.name == "main")
    else {
        return diagnostic_result(
            "PY-NO-MAIN",
            "the program must define a `main()` function".to_owned(),
        );
    };
    let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
    match run(main_co, &module.functions, &[], &mut model) {
        Ok(value) => RunResult {
            stdout: String::new(),
            exit_code: value.as_fixnum().and_then(|n| i32::try_from(n).ok()).unwrap_or(0),
            diagnostics: Vec::new(),
        },
        Err(trap) => diagnostic_result("PY-TRAP", format!("{trap:?}")),
    }
}

fn diagnostic_result(code: &'static str, message: String) -> RunResult {
    RunResult {
        stdout: String::new(),
        exit_code: -1,
        diagnostics: vec![Diagnostic {
            code,
            severity: "error",
            line: 0,
            column: 0,
            message,
        }],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_a_typed_main_and_returns_its_value_as_the_exit_code() {
        let result = run_py_str("def main() -> int:\n    return 6 * 7\n");
        assert_eq!(result.stdout, "");
        assert_eq!(result.exit_code, 42);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn a_parse_error_becomes_a_diagnostic_not_a_panic() {
        let result = run_py_str("def main( ->\n    return 1\n");
        assert_eq!(result.exit_code, -1);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].severity, "error");
    }
}
