//! Embedding the interpreter: run a managed assembly and capture its result.

pub mod abi;
pub mod clock;
pub mod console;
#[cfg(feature = "aot")]
pub mod aot;
#[cfg(feature = "bake")]
pub mod bake;
#[cfg(feature = "bake")]
pub mod srcmap;
#[cfg(feature = "compile")]
pub mod compile;
#[cfg(feature = "dap")]
pub mod dap;
#[cfg(feature = "py")]
pub mod py;
#[cfg(feature = "repl")]
pub mod repl;

use lamella_load::{load, load_with_corlib};
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
    abi::with_static(assembly_bytes, |bytes| run_static(bytes, None))
}

/// [`run_bytes`] with a managed corlib loaded alongside the program -- the shape the REPL has always
/// used, now reachable from the one-shot run path too.
///
/// # Why this exists: a corlib-less run resolves ONLY what the loader intrinsic-binds
///
/// Without a corlib, every cross-assembly `MemberRef` a program makes has to hit one of the loader's
/// intrinsic bindings or it resolves to nothing. That covers a great deal -- `Console.WriteLine`,
/// `String.ToUpper`, `Math.Max` -- so a corlib-less run LOOKS complete, which is exactly what made
/// this hard to see. It is not complete: any corlib method whose body is MANAGED C# has no binding.
///
/// `System.Threading.Thread.Sleep` is the case a user hit. The loader binds the PRIVATE
/// `Thread.SleepThread` intrinsic, and the corlib's public `Sleep(int)` is one line of managed IL
/// that calls it -- so with no corlib there is nothing to resolve `Sleep` to, and the program traps
/// with `call token 0x0A... resolved to no method` AFTER printing, mid-run.
///
/// Passing a corlib is strictly more resolving power, never less: [`load_with_corlib`] resolves each
/// `MemberRef` against the corlib's name index and **falls back to a Rust intrinsic only when the
/// index has no match**. So every program that ran before runs the same way, and the ones that
/// trapped now find their method.
#[must_use]
pub fn run_bytes_with_corlib(corlib_bytes: &[u8], assembly_bytes: &[u8]) -> RunResult {
    abi::with_static(corlib_bytes, |corlib| {
        abi::with_static(assembly_bytes, |bytes| run_static(bytes, Some(corlib)))
    })
}

fn run_static(assembly_bytes: &'static [u8], corlib_bytes: Option<&'static [u8]>) -> RunResult {
    let assembly = match lamella_metadata::Assembly::read(assembly_bytes) {
        Ok(assembly) => assembly,
        Err(error) => return failure("LAMELLA-PARSE", format!("{error:?}")),
    };
    let corlib = match corlib_bytes.map(lamella_metadata::Assembly::read) {
        None => None,
        Some(Ok(corlib)) => Some(corlib),
        Some(Err(error)) => return failure("LAMELLA-CORLIB", format!("the corlib did not parse: {error:?}")),
    };
    let program = match corlib.as_ref() {
        Some(corlib) => load_with_corlib(corlib, &assembly),
        None => load(&assembly),
    };
    let program = match program {
        Ok(program) => program,
        Err(error) => return failure("LAMELLA-LOAD", format!("{error}")),
    };

    let mut vm = Vm::new();
    clock::install(&mut vm);
    console::install(&mut vm);
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
pub(crate) fn push_json_string(json: &mut String, text: &str) {
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
    fn the_console_tap_streams_the_same_text_the_buffer_ends_up_with() {
        let Some(bytes) = fixture("hello.dll") else {
            eprintln!("hello.dll absent; skipping");
            return;
        };
        let _ = console::take_streamed();
        let result = run_bytes(&bytes);
        let chunks = console::take_streamed();

        assert_eq!(chunks.concat(), result.stdout);
        assert!(!chunks.is_empty(), "the tap fired at least once");
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
