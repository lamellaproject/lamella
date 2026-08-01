//! The Python embedding: compile a Python program and run it the way CPython runs a script.

use crate::{Diagnostic, RunResult};
use lamella_py_frontend::FrontendError;
use lamella_py_runtime::{run, run_bundle, Bundle, ObjectModel, Trap};

/// The interpreter collects at a safe point once the object heap is three quarters full, so this
/// figure bounds a program's LIVE SET rather than its total allocation -- except across a call into a
/// managed (Python-authored) module, which runs a nested driver loop where no safe point exists and
/// whose total allocation this must still hold.
///
/// The figure is HEADROOM, and it is worth saying what it is not: it is not what the interpreter
/// needs. The runtime lane's measured floors (`device_footprint.rs`) are small -- every ordinary
/// program inside 16 KiB, the two heaviest (`re`, `json`) at 8. The one expensive case is `random`,
/// whose MT19937 seeding needs **256 KiB**: 624 state words, every one past the 31-bit fixnum, and
/// all of them minted inside `random.py`, so the collection that would reclaim them cannot run until
/// the call returns. That is the managed-module exception above, and it is why a collector did not
/// make this case cheap.
///
/// So a device profile sizes this from those numbers. A BROWSER cannot: the program is whatever the
/// person typed, and a page that refuses a big list to save memory it never had to save is the wrong
/// trade. Wasm memory grows on demand and the run is torn down after, so we take the headroom here
/// and leave the sizing argument to the tier that actually has a RAM budget.
const HEAP_BYTES: usize = 16 * 1024 * 1024;

/// Resolves an `import` to a module the runtime carries as source. A miss is NOT an error: the
/// name stays native, else `ModuleNotFoundError` at run time -- the host-shaped behaviour.
fn bundled_module(name: &str) -> Option<String> {
    lamella_py_runtime::pystdlib::bundled_module(name).map(String::from)
}

/// Compiles `source` together with the managed modules its top-level imports reach -- one bundle,
/// used by both the run and the check so the two can never compile different programs.
fn compile(source: &str) -> Result<Bundle, FrontendError> {
    lamella_py_frontend::compile_bundle("main", source, &bundled_module)
}

/// Compiles `source` and runs it, capturing its `print` output and any compile/runtime
/// diagnostics. Never panics: a parse error or a trap becomes a diagnostic, exactly as
/// [`crate::run_bytes`] does for C#.
#[must_use]
pub fn run_py_str(source: &str) -> RunResult {
    let bundle = match compile(source) {
        Ok(bundle) => bundle,
        Err(error) => return compile_error_result(&error),
    };
    let entry_functions = bundle.entry.functions.clone();
    let mut model = ObjectModel::new(Vec::new(), HEAP_BYTES);

    if let Err(trap) = run_bundle(bundle, &mut model) {
        return trap_result(&mut model, trap);
    }
    let out = model.take_stdout();
    if !out.is_empty() {
        return RunResult {
            stdout: out,
            exit_code: 0,
            diagnostics: Vec::new(),
        };
    }
    let Some(main_co) = entry_functions
        .iter()
        .find(|function| function.name == "main")
    else {
        return RunResult {
            stdout: String::new(),
            exit_code: 0,
            diagnostics: Vec::new(),
        };
    };
    match run(main_co, &entry_functions, &[], &mut model) {
        Ok(value) => {
            let mut stdout = model.take_stdout();
            stdout.push_str(&model.display(value));
            stdout.push('\n');
            RunResult {
                stdout,
                exit_code: value
                    .as_fixnum()
                    .and_then(|n| i32::try_from(n).ok())
                    .unwrap_or(0),
                diagnostics: Vec::new(),
            }
        }
        Err(trap) => trap_result(&mut model, trap),
    }
}

/// Compile-CHECKS `source` WITHOUT running it -- the editor / LSP diagnostics path (a "check",
/// not a run). A clean compile yields no diagnostics; an error yields one `PY-COMPILE`
/// diagnostic. It compiles the same bundle [`run_py_str`] does, imports included, so the squiggles
/// describe the program that will actually run. A module name that resolves to nothing is not an
/// error here for the same reason it is not one at compile time: it may be native.
#[must_use]
pub fn check_py_str(source: &str) -> RunResult {
    match compile(source) {
        Ok(_) => RunResult {
            stdout: String::new(),
            exit_code: 0,
            diagnostics: Vec::new(),
        },
        Err(error) => compile_error_result(&error),
    }
}

/// Compiles `source` into the DEPLOYABLE BUNDLE BYTES -- the versioned `LPYC` container a device
/// loads through the wire's `RUN_BUNDLE` / `DEPLOY_BUNDLE` ops -- or an EMPTY vector when the program
/// does not compile.
///
/// # Why this export exists at all
///
/// Everything needed to put Python on a board was already in the tree except a way to GET THE BYTES
/// OUT OF THE BROWSER. `Bundle::encode` has existed in `lamella-py-bytecode`; the host's chunked
/// deploy has been payload-agnostic for months; the ops are allocated. But [`run_py_str`] builds a
/// `Bundle` and consumes it internally, so no caller on the far side of this wasm boundary could ever
/// obtain one. One function, and the browser goes from "runs Python" to "can hand Python to a device".
///
/// # It compiles the SAME bundle the run and the check do
///
/// All three go through the private `compile`, so the artifact a device is asked to run is the artifact
/// the editor checked and the artifact the page ran. That is the property worth protecting here: a
/// deploy path that compiled its own way could ship something the user never saw pass.
///
/// # Empty means "did not compile", and the reason is a call away
///
/// An empty result is the only failure this can have, and the caller gets the REASON from
/// [`check_py_str`] on the same source -- same `compile`, so the two cannot disagree about why. That is
/// deliberately not the shape of a build that returns an empty buffer and no way to learn anything: the
/// reason is available, from an entry point that already exists, and the JS seam makes the two calls
/// look like one.
#[must_use]
pub fn bundle_py_str(source: &str) -> Vec<u8> {
    use lamella_py_frontend::bytecode::FeatureFlags;
    match compile(source) {
        Ok(bundle) => bundle.encode(FeatureFlags::FIRST_LIGHT),
        Err(_) => Vec::new(),
    }
}

/// A `PY-TRAP` diagnostic that keeps whatever the program printed BEFORE the fault -- discarding
/// it would hide the half of the run that worked -- and names an uncaught Python exception by its
/// type, the way CPython's traceback does, rather than by an internal trap kind.
fn trap_result(model: &mut ObjectModel, trap: Trap) -> RunResult {
    let stdout = model.take_stdout();
    let pending = model.take_pending_exception();
    let message = match pending.and_then(|e| model.exception_type_name(e).map(String::from)) {
        Some(name) => name,
        None => format!("{trap:?}"),
    };
    RunResult {
        stdout,
        exit_code: -1,
        diagnostics: vec![Diagnostic {
            code: "PY-TRAP",
            severity: "error",
            line: 0,
            column: 0,
            message,
        }],
    }
}

/// A `PY-COMPILE` diagnostic built from a front-end error, carrying the 1-based source line where
/// one is known (lex/parse errors expose it); a lowering error is position-less, so line 0.
fn compile_error_result(error: &FrontendError) -> RunResult {
    let line = match error {
        FrontendError::Lex(e) => e.line,
        FrontendError::Parse(e) => e.line,
        FrontendError::Compile(_) => 0,
        FrontendError::BoardFact(_) => 0,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A failing assertion should say WHY the program did not produce what was expected, and the
    /// reason is always in the diagnostics. `Diagnostic` is a wire type with no `Debug`.
    fn why(result: &RunResult) -> String {
        result
            .diagnostics
            .iter()
            .map(|d| format!("{}: {}", d.code, d.message))
            .collect::<Vec<_>>()
            .join("; ")
    }

    #[test]
    fn top_level_print_is_the_programs_output() {
        let result = run_py_str("print(\"hello\")\n");
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.exit_code, 0);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn a_program_that_only_defines_main_still_runs_it() {
        let result = run_py_str("def main() -> int:\n    return 6 * 7\n");
        assert_eq!(result.stdout, "42\n");
        assert_eq!(result.exit_code, 42);
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn print_inside_main_reaches_stdout_too() {
        let result = run_py_str("def main() -> int:\n    print(\"inside\")\n    return 1\n");
        assert_eq!(result.stdout, "inside\n1\n");
        assert_eq!(result.exit_code, 1);
    }

    #[test]
    fn an_import_reaches_a_module_the_runtime_carries() {
        let result = run_py_str("import json\nprint(json.dumps([1, 2]))\n");
        assert_eq!(result.stdout, "[1, 2]\n", "{}", why(&result));
        assert!(result.diagnostics.is_empty());
    }

    #[test]
    fn an_allocation_heavy_module_does_not_exhaust_the_arena() {
        let result = run_py_str("import random\nrandom.seed(1234)\nprint(random.randint(0, 9) >= 0)\n");
        assert_eq!(result.stdout, "True\n", "{}", why(&result));
    }

    /// The bundle a device would be sent DECODES BACK into a bundle. Checking the magic and version by
    /// hand would only prove the first eight bytes; a round trip proves the whole container, which is
    /// what a target's own decoder will do to it.
    #[test]
    fn the_deployable_bundle_decodes_as_a_bundle() {
        use lamella_py_frontend::bytecode::{Bundle as B, FeatureFlags, BUNDLE_FORMAT_VERSION, MAGIC};

        let bytes = bundle_py_str("print(6 * 7)\n");
        assert!(!bytes.is_empty(), "a program that compiles must produce bundle bytes");
        assert_eq!(&bytes[0..4], &MAGIC, "the container must carry the LPYC magic");
        assert_eq!(
            u16::from_le_bytes([bytes[4], bytes[5]]),
            BUNDLE_FORMAT_VERSION,
            "the container must declare the BUNDLE version, not the bare-module one"
        );
        let (decoded, features) = B::decode(&bytes).expect("the bundle we emit must decode");
        assert!(features.contains(FeatureFlags::FIRST_LIGHT));
        assert_eq!(decoded.entry.name, "main");
    }

    /// The point of a bundle rather than a module: a device has no filesystem, so an imported module
    /// must travel INSIDE the artifact. If this regressed, a deploy would succeed and the board would
    /// fail on the import -- far away from the cause.
    #[test]
    fn an_imported_module_travels_inside_the_bundle() {
        use lamella_py_frontend::bytecode::Bundle as B;

        let (bare, _) = B::decode(&bundle_py_str("print(1)\n")).expect("decodes");
        assert!(bare.modules.is_empty(), "a program importing nothing carries no modules");

        let (withjson, _) =
            B::decode(&bundle_py_str("import json\nprint(json.dumps([1]))\n")).expect("decodes");
        assert!(
            withjson.modules.iter().any(|m| m.name == "json"),
            "an imported managed module must be carried in the bundle, not resolved on the device"
        );
    }

    /// A program that does not compile yields EMPTY bytes -- never a container a device would accept --
    /// and the reason is available from the check path on the same source.
    #[test]
    fn a_program_that_does_not_compile_yields_no_bundle_and_a_reason() {
        let bad = "def oops(\n";
        assert!(bundle_py_str(bad).is_empty(), "a broken program must not produce a container");
        let checked = check_py_str(bad);
        assert_eq!(checked.diagnostics.len(), 1, "the check supplies the reason the bundle call omits");
        assert_eq!(checked.diagnostics[0].code, "PY-COMPILE");
    }

    /// The deploy artifact is the artifact the editor checked and the page ran -- all three go through
    /// one `compile`. A deploy path that compiled its own way could ship something never seen to pass.
    #[test]
    fn the_bundle_agrees_with_the_run_and_the_check() {
        let source = "import itertools\nprint(len(list(itertools.islice(itertools.count(), 3))))\n";
        assert!(check_py_str(source).diagnostics.is_empty(), "the check passes");
        assert_eq!(run_py_str(source).stdout, "3\n", "the run produces the answer");
        assert!(!bundle_py_str(source).is_empty(), "and the same source yields a deployable bundle");
    }

    #[test]
    fn an_unresolvable_import_fails_loudly_rather_than_silently() {
        let result = run_py_str("import nosuchmodule\nprint(1)\n");
        assert_eq!(result.exit_code, -1);
        assert_eq!(result.diagnostics.len(), 1);
        assert_eq!(result.diagnostics[0].code, "PY-TRAP");
    }

    #[test]
    fn a_trap_keeps_the_output_produced_before_it() {
        let result = run_py_str("print(\"before\")\nraise ValueError(\"boom\")\n");
        assert_eq!(result.stdout, "before\n");
        assert_eq!(result.exit_code, -1);
        assert_eq!(result.diagnostics[0].code, "PY-TRAP");
        assert_eq!(result.diagnostics[0].message, "ValueError");
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

    #[test]
    fn check_does_not_squiggle_an_import_the_run_would_accept() {
        assert!(check_py_str("import json\nprint(json.dumps(1))\n")
            .diagnostics
            .is_empty());
    }
}
