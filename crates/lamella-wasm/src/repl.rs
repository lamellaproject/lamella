//! The in-page stateful C# REPL agent (feature `repl`).

#![allow(unsafe_code)]

use core::cell::RefCell;

use lamella_metadata::Assembly;
use lamella_wireline::engine::{CompileFailure, LoopbackLink, Outcome, Repl, ReplCompiler};

use crate::abi::result_buffer;
use crate::compile::split_refs;

thread_local! {
    /// The one live REPL session (the transcript lives here, across `eval` calls). wasm is
    /// single-threaded, so a process-global `RefCell` is enough -- the same shape the DAP
    /// agent uses for its sessions. `None` until `lamella_repl_new` builds one.
    static REPL: RefCell<Option<Repl>> = const { RefCell::new(None) };
}

/// THIS crate's in-wasm C# compiler, plugged into the engine's [`ReplCompiler`] seam: it
/// compiles a rendered compilation unit to program-assembly bytes via
/// [`lamella_assemble::compile_source`] (the same path [`crate::compile`] exposes as
/// `lamella_compile`). It owns the net8.0 reference-assembly bytes so it can bind every
/// submission against the BCL without the host re-sending them.
struct WasmCompiler {
    /// The reference assembly bytes (System.Runtime / System.Console, ...) the binder
    /// resolves BCL names against -- owned, so the session keeps them across submissions.
    references: Vec<Vec<u8>>,
}

impl ReplCompiler for WasmCompiler {
    fn compile(&self, source: &str) -> Result<Vec<u8>, CompileFailure> {
        let references: Vec<Assembly> =
            self.references.iter().filter_map(|bytes| Assembly::read(bytes).ok()).collect();
        let compiled =
            lamella_assemble::compile_source(source, "Repl.cs", "__Repl", "__Repl", &references, false);
        if let Some(image) = compiled.image {
            return Ok(image);
        }
        if let Some(emit_error) = compiled.emit_error {
            return Err(CompileFailure::Diagnostics(format!("{emit_error:?}")));
        }
        let mut text = String::new();
        for diagnostic in &compiled.diagnostics {
            if !text.is_empty() {
                text.push('\n');
            }
            let severity = if diagnostic.is_error() { "error" } else { "warning" };
            text.push_str(&format!("CS{:04}: {severity}: {}", diagnostic.code, diagnostic.message));
        }
        if text.is_empty() {
            text.push_str("compilation produced no image");
        }
        Err(CompileFailure::Diagnostics(text))
    }
}

/// Build a fresh process-global REPL: the compiler binds against `references` (the net8.0
/// ref assemblies) and the link runs against `corlib` (the managed corlib). Replaces any
/// existing session.
fn new_session(corlib: &[u8], references: Vec<Vec<u8>>) {
    let compiler = Box::new(WasmCompiler { references });
    let link = Box::new(LoopbackLink::new(corlib.to_vec()));
    let repl = Repl::new(compiler, link);
    REPL.with(|cell| *cell.borrow_mut() = Some(repl));
}

/// Evaluate one submission against the stored session, returning the result JSON bytes
/// (see [`lamella_repl_eval`] for the shape). When no session exists, a `compileError`.
fn eval(source: &[u8]) -> Vec<u8> {
    let source = core::str::from_utf8(source).unwrap_or("");
    REPL.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(repl) = guard.as_mut() else {
            return error_json("REPL not initialized");
        };
        match repl.eval(source) {
            Ok(Outcome::Empty) => outcome_json("", 0, "empty", false),
            Ok(Outcome::Ran { output, exit, persisted }) => {
                outcome_json(&output, exit, "ran", persisted)
            }
            Ok(Outcome::CompileError(text)) => error_json(&text),
            Err(engine_error) => error_json(&format!("{engine_error}")),
        }
    })
}

/// `{ "output", "exit", "kind", "persisted" }` for a run/empty outcome.
fn outcome_json(output: &str, exit: i32, kind: &str, persisted: bool) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "output": output,
        "exit": exit,
        "kind": kind,
        "persisted": persisted,
    }))
    .unwrap_or_default()
}

/// `{ "compileError": <text> }` for a source diagnostic / an uninitialized session.
fn error_json(message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({ "compileError": message })).unwrap_or_default()
}

/// Clear the stored session (drop the transcript).
fn reset() {
    REPL.with(|cell| {
        if let Some(repl) = cell.borrow_mut().as_mut() {
            repl.reset();
        }
    });
}

/// Builds the process-global REPL session: the compiler binds against the reference
/// assemblies packed at `refs_ptr..refs_ptr + refs_len` (the same `[u32 count]` then
/// `count` x `[u32 len][bytes]` packing `lamella_compile` takes -- System.Runtime /
/// System.Console), and the run side executes against the managed corlib at
/// `corlib_ptr..corlib_ptr + corlib_len`. Replaces any existing session, so it doubles as a
/// hard reset that re-supplies the corlib + references.
///
/// The references and the corlib are SEPARATE inputs on purpose: the managed corlib is a
/// runtime assembly and is not usable as a compile reference (the binder does not terminate
/// on it). A zero-length `refs` is allowed and means System-free code only.
///
/// # Safety
/// Both pointer/length pairs must be buffers the host filled via prior `lamella_alloc` calls
/// (a zero-length `refs` is allowed and means no references).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_repl_new(
    corlib_ptr: *const u8,
    corlib_len: usize,
    refs_ptr: *const u8,
    refs_len: usize,
) {
    let corlib = unsafe { core::slice::from_raw_parts(corlib_ptr, corlib_len) };
    let refs: &[u8] = if refs_len == 0 {
        &[]
    } else {
        unsafe { core::slice::from_raw_parts(refs_ptr, refs_len) }
    };
    let references = split_refs(refs).into_iter().map(<[u8]>::to_vec).collect();
    new_session(corlib, references);
}

/// Evaluates the UTF-8 C# submission at `src_ptr..src_ptr + src_len` against the stored
/// session, returning a `[u32 len][UTF-8 JSON]` buffer (free with
/// `lamella_dealloc(result, 4 + len)`). The JSON is one of:
///
/// - a run: `{ "output": string, "exit": number, "kind": string, "persisted": bool }`,
///   where `output` is the NEW console output beyond the previous run, `exit` is the
///   program's exit code (0 for a clean statement/expression, 70 on an unhandled trap),
///   `kind` is `"ran"` (or `"empty"` for a whitespace-only submission), and `persisted`
///   says whether the submission joined the transcript;
/// - a compile failure (or no session yet): `{ "compileError": string }`.
///
/// # Safety
/// `src_ptr`/`src_len` must be the UTF-8 buffer the host filled via a prior `lamella_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_repl_eval(src_ptr: *const u8, src_len: usize) -> *mut u8 {
    let source = unsafe { core::slice::from_raw_parts(src_ptr, src_len) };
    result_buffer(eval(source))
}

/// Clears the stored REPL session's transcript (the `#reset` command). A no-op when no
/// session exists. The corlib + references are retained, so evaluation can continue from an
/// empty session.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_repl_reset() {
    reset();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads the managed corlib + packs the net8.0 ref assemblies the way the host does.
    /// Returns `None` (and the test skips) when an asset is absent.
    fn assets() -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
        let corlib = std::fs::read(format!(
            "{}/../lamella-load/tests/fixtures/corlib.dll",
            env!("CARGO_MANIFEST_DIR")
        ))
        .ok()?;
        let runtime = std::fs::read(r"E:\lamella-web\corelib\System.Runtime.dll").ok()?;
        let console = std::fs::read(r"E:\lamella-web\corelib\System.Console.dll").ok()?;
        Some((corlib, vec![runtime, console]))
    }

    fn parse(buffer: Vec<u8>) -> serde_json::Value {
        serde_json::from_slice(&buffer).expect("the eval JSON parses")
    }

    #[test]
    fn eval_without_a_session_reports_not_initialized() {
        REPL.with(|cell| *cell.borrow_mut() = None);
        let value = parse(eval(b"40 + 2"));
        assert_eq!(value["compileError"], "REPL not initialized");
    }

    #[test]
    fn an_expression_prints_its_value_and_does_not_persist() {
        let Some((corlib, refs)) = assets() else {
            eprintln!("REPL assets absent; skipping");
            return;
        };
        new_session(&corlib, refs);
        let value = parse(eval(b"40 + 2"));
        assert_eq!(value["kind"], "ran", "got {value}");
        assert_eq!(value["output"], "42\n");
        assert_eq!(value["exit"], 0);
        assert_eq!(value["persisted"], false);
    }

    #[test]
    fn a_statement_persists_and_a_later_line_sees_it() {
        let Some((corlib, refs)) = assets() else {
            eprintln!("REPL assets absent; skipping");
            return;
        };
        new_session(&corlib, refs);
        let declared = parse(eval(b"int n = 40;"));
        assert_eq!(declared["persisted"], true, "got {declared}");
        assert_eq!(declared["output"], "");
        let used = parse(eval(b"Console.WriteLine(n + 2);"));
        assert_eq!(used["output"], "42\n", "got {used}");
        assert_eq!(used["persisted"], true);
    }

    #[test]
    fn a_typo_is_a_compile_error_not_a_panic() {
        let Some((corlib, refs)) = assets() else {
            eprintln!("REPL assets absent; skipping");
            return;
        };
        new_session(&corlib, refs);
        let value = parse(eval(b"nonsense +"));
        assert!(value.get("compileError").is_some(), "got {value}");
        assert_eq!(parse(eval(b"1 + 1"))["output"], "2\n");
    }

    #[test]
    fn reset_clears_the_transcript() {
        let Some((corlib, refs)) = assets() else {
            eprintln!("REPL assets absent; skipping");
            return;
        };
        new_session(&corlib, refs);
        assert_eq!(parse(eval(b"int k = 5;"))["persisted"], true);
        reset();
        let value = parse(eval(b"Console.WriteLine(k);"));
        assert!(value.get("compileError").is_some(), "got {value}");
    }
}
