//! Embedding the interpreter: run a managed assembly and capture its result.

pub mod abi;
#[cfg(feature = "aot")]
pub mod aot;
#[cfg(feature = "compile")]
pub mod compile;
#[cfg(feature = "dap")]
pub mod dap;

use lamella_load::load;
use lamella_metadata::Assembly;
use lamella_cil_runtime::{Value, Vm, run};

/// A diagnostic surfaced to the embedder. For Tier 1 these are runtime issues
/// (a malformed image, a failed load, or a trap); `line`/`column` are 1-based and
/// 0 means "no source location" until source mapping exists.
pub struct Diagnostic {
    /// A short stable code, e.g. `LAMELLA-TRAP`.
    pub code: &'static str,
    /// `"error"` or `"warning"`.
    pub severity: &'static str,
    /// 1-based source line, or 0 when unknown.
    pub line: u32,
    /// 1-based source column, or 0 when unknown.
    pub column: u32,
    /// A human-readable message.
    pub message: String,
}

/// The outcome of running an assembly.
pub struct RunResult {
    /// Everything the program wrote to the console.
    pub stdout: String,
    /// The entry point's `int` return value, or 0 for a `void`/other entry; -1 on
    /// a load failure or trap.
    pub exit_code: i32,
    /// Any runtime diagnostics (empty on a clean run).
    pub diagnostics: Vec<Diagnostic>,
}

/// Loads a managed assembly from its PE bytes and runs its entry point, capturing
/// the console output, exit code, and any runtime diagnostics. Never panics on bad
/// input: malformed bytes become a diagnostic.
#[must_use]
pub fn run_bytes(assembly_bytes: &[u8]) -> RunResult {
    let assembly = match Assembly::read(assembly_bytes) {
        Ok(assembly) => assembly,
        Err(error) => {
            return failure(
                "LAMELLA-IMAGE",
                format!("could not read assembly: {error:?}"),
            );
        }
    };
    let program = match load(&assembly) {
        Ok(program) => program,
        Err(error) => return failure("LAMELLA-LOAD", format!("{error}")),
    };

    let mut vm = Vm::new();
    match run(&program.module, &mut vm, program.entry, Vec::new()) {
        Ok(result) => RunResult {
            stdout: vm.output_string(),
            exit_code: match result {
                Some(Value::Int32(value)) => value,
                _ => 0,
            },
            diagnostics: Vec::new(),
        },
        Err(trap) => RunResult {
            stdout: vm.output_string(),
            exit_code: -1,
            diagnostics: vec![error("LAMELLA-TRAP", format!("{trap}"))],
        },
    }
}

/// Serializes a [`RunResult`] to the JSON the embedder receives:
/// `{ "stdout": string, "exitCode": number, "diagnostics": [ ... ] }`. Hand-rolled
/// so the crate stays dependency-free.
#[must_use]
pub fn to_json(result: &RunResult) -> String {
    let mut json = String::from("{\"stdout\":");
    push_json_string(&mut json, &result.stdout);
    json.push_str(",\"exitCode\":");
    json.push_str(&result.exit_code.to_string());
    json.push_str(",\"diagnostics\":[");
    for (index, diagnostic) in result.diagnostics.iter().enumerate() {
        if index > 0 {
            json.push(',');
        }
        json.push_str("{\"code\":");
        push_json_string(&mut json, diagnostic.code);
        json.push_str(",\"severity\":");
        push_json_string(&mut json, diagnostic.severity);
        json.push_str(",\"line\":");
        json.push_str(&diagnostic.line.to_string());
        json.push_str(",\"column\":");
        json.push_str(&diagnostic.column.to_string());
        json.push_str(",\"message\":");
        push_json_string(&mut json, &diagnostic.message);
        json.push('}');
    }
    json.push_str("]}");
    json
}

fn failure(code: &'static str, message: String) -> RunResult {
    RunResult {
        stdout: String::new(),
        exit_code: -1,
        diagnostics: vec![error(code, message)],
    }
}

fn error(code: &'static str, message: String) -> Diagnostic {
    Diagnostic {
        code,
        severity: "error",
        line: 0,
        column: 0,
        message,
    }
}

/// Appends `text` to `json` as a quoted, escaped JSON string.
fn push_json_string(json: &mut String, text: &str) {
    json.push('"');
    for character in text.chars() {
        match character {
            '"' => json.push_str("\\\""),
            '\\' => json.push_str("\\\\"),
            '\n' => json.push_str("\\n"),
            '\r' => json.push_str("\\r"),
            '\t' => json.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                json.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => json.push(other),
        }
    }
    json.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Option<Vec<u8>> {
        let path = format!(
            "{}/../lamella-load/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(path).ok()
    }

    #[test]
    fn runs_hello_world() {
        let Some(bytes) = fixture("hello.dll") else {
            eprintln!("hello.dll absent; skipping");
            return;
        };
        let result = run_bytes(&bytes);
        assert_eq!(result.stdout, "Hello, World!\n");
        assert_eq!(result.exit_code, 0);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn arithmetic_returns_its_exit_code() {
        let Some(bytes) = fixture("arith.dll") else {
            eprintln!("arith.dll absent; skipping");
            return;
        };
        assert_eq!(run_bytes(&bytes).exit_code, 5);
    }

    #[test]
    fn malformed_bytes_become_a_diagnostic_not_a_panic() {
        let result = run_bytes(b"this is not a managed assembly");
        assert_eq!(result.exit_code, -1);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].severity, "error");
    }

    #[test]
    fn hello_world_serializes_to_json() {
        let Some(bytes) = fixture("hello.dll") else {
            eprintln!("hello.dll absent; skipping");
            return;
        };
        let json = to_json(&run_bytes(&bytes));
        assert!(json.contains(r#""stdout":"Hello, World!\n""#));
        assert!(json.contains(r#""exitCode":0"#));
        assert!(json.contains(r#""diagnostics":[]"#));
    }

    #[test]
    fn json_strings_are_escaped() {
        let result = RunResult {
            stdout: "a\"b\\c\n".to_owned(),
            exit_code: 0,
            diagnostics: Vec::new(),
        };
        assert!(to_json(&result).contains(r#""stdout":"a\"b\\c\n""#));
    }
}
