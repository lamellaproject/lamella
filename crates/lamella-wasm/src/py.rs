//! The Python embedding: compile a Python source string and run its `main()`.

use crate::{Diagnostic, RunResult};
use lamella_py_frontend::FrontendError;
use lamella_py_runtime::{ObjectModel, run};

/// Compiles `source` and runs its `main()`, capturing the rendered result and any
/// compile/runtime diagnostics. Never panics: a parse error or a trap becomes a
/// diagnostic, exactly as [`crate::run_bytes`] does for C#.
#[must_use]
pub fn run_py_str(source: &str) -> RunResult {
    let module = match lamella_py_frontend::compile_str("main", source) {
        Ok(module) => module,
        Err(error) => return compile_error_result(&error),
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

/// Compile-CHECKS `source` WITHOUT running it -- the editor / LSP diagnostics path (a "check",
/// not a run). A clean compile yields no diagnostics; an error yields one `PY-COMPILE` diagnostic.
/// Behind the `py` feature; reuses [`crate::RunResult`].
#[must_use]
pub fn check_py_str(source: &str) -> RunResult {
    match lamella_py_frontend::compile_str("main", source) {
        Ok(_) => RunResult {
            stdout: String::new(),
            exit_code: 0,
            diagnostics: Vec::new(),
        },
        Err(error) => compile_error_result(&error),
    }
}

/// A `PY-COMPILE` diagnostic built from a front-end error, carrying the 1-based source line where
/// one is known (lex/parse errors expose it); a lowering error is position-less, so line 0.
fn compile_error_result(error: &FrontendError) -> RunResult {
    let line = match error {
        FrontendError::Lex(e) => e.line,
        FrontendError::Parse(e) => e.line,
        FrontendError::Compile(_) => 0,
    };
    RunResult {
        stdout: String::new(),
        exit_code: -1,
        diagnostics: vec![Diagnostic {
            code: "PY-COMPILE",
            severity: "error",
            line,
            column: 0,
            message: format!("{error}"),
        }],
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

    #[test]
    fn check_reports_a_compile_error_without_running() {
        let bad = check_py_str("def main( ->\n    return 1\n");
        assert_eq!(bad.diagnostics.len(), 1);
        assert_eq!(bad.diagnostics[0].code, "PY-COMPILE");
        assert_eq!(bad.stdout, "");
        assert!(check_py_str("def main() -> int:\n    return 6 * 7\n").diagnostics.is_empty());
    }
}
