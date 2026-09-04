//! The Debug Console REPL: a DAP `evaluate` handler backed by the Lamella Link REPL engine.
//! Each submission is compiled and run so declarations persist and an expression prints its value.

use lamella_cil_runtime::{Value, Vm, run};
use lamella_load::{load, resident_bytes};
use lamella_metadata::Assembly;
use lamella_wire::TransportError;
use lamella_wire_host::RunResult;
use lamella_wire_host::engine::{LcscCompiler, Outcome, Repl, ReplLink};
use serde_json::{Value as Json, json};

/// The REPL for a debug session: built lazily on the first `evaluate`, then reused so the
/// transcript persists across submissions.
pub enum ReplCell {
    /// A live session: the engine and its persistent transcript.
    Ready(Repl),
    /// The compiler could not be built (no reference assemblies found); the reason, reported once.
    Unavailable(String),
}

impl ReplCell {
    /// Build the engine over the compiler + the in-process interpreter runner.
    fn init() -> ReplCell {
        match LcscCompiler::discover() {
            Ok(compiler) => ReplCell::Ready(Repl::new(Box::new(compiler), Box::new(InProcLink))),
            Err(reason) => ReplCell::Unavailable(reason),
        }
    }
}

/// Evaluates one Debug Console submission, returning the DAP `evaluate` response body
/// (`{ result, variablesReference }`). The session is created on first use and kept in `slot`.
/// Always succeeds at the DAP level: a compile error, a trap, or an unavailable REPL is reported
/// as the `result` text so it shows in the console rather than as a failed request.
pub fn evaluate(
    slot: &mut Option<ReplCell>,
    expression: &str,
    has_frame: bool,
    context: &str,
) -> Json {
    if let Some(result) = frame_scoped_refusal(has_frame, context) {
        return json!({ "result": result, "variablesReference": 0 });
    }
    let cell = slot.get_or_insert_with(ReplCell::init);
    let result = match cell {
        ReplCell::Unavailable(reason) => format!("REPL unavailable: {reason}"),
        ReplCell::Ready(repl) => match repl.eval(expression) {
            Ok(Outcome::Empty) => String::new(),
            Ok(Outcome::Ran { output, .. }) => output.trim_end_matches('\n').to_owned(),
            Ok(Outcome::CompileError(text)) => text,
            Err(error) => error.to_string(),
        },
    };
    json!({ "result": result, "variablesReference": 0 })
}

/// What to answer a submission scoped to a PAUSED FRAME, or `None` when the session REPL is the
/// right place to evaluate it.
///
/// # Why a refusal rather than an answer
///
/// This console evaluates in a session of its own -- a compiler and an interpreter in this process
/// -- which has never seen the target and cannot read the frame execution is paused in. **An
/// expression naming a local is therefore not merely unanswerable here, it is answerable WRONGLY**:
/// the same name may exist in the session's own transcript, and the value returned would then be a
/// real value from the wrong place, with nothing about it saying so.
///
/// A watch showing a wrong value is worse than a watch showing none, and an unadorned compile error
/// is worse still, because it reads as *your expression is wrong* rather than *this console cannot
/// see your frame*. So a frame-scoped submission is answered by saying which of the two happened.
///
/// A submission with no frame -- the console used as a plain REPL, which is what it is when nothing
/// is paused -- is unaffected, and that is the case this console was built for.
fn frame_scoped_refusal(has_frame: bool, context: &str) -> Option<String> {
    let scoped = has_frame || matches!(context, "watch" | "hover");
    if !scoped {
        return None;
    }
    if matches!(context, "watch" | "hover") {
        return Some("<no frame access>".to_owned());
    }
    Some(
        "This console evaluates in its own session and cannot read the frame you are paused in, \
         so a name from that frame would either fail here or resolve to something else. Use the \
         variables view for the frame's values."
            .to_owned(),
    )
}

/// The engine's RUN seam: run a compiled program on this process's interpreter and capture its
/// output + exit code. `seq` is unused (there is no wire to correlate).
struct InProcLink;

impl ReplLink for InProcLink {
    fn run(&mut self, _seq: u16, program: &[u8]) -> Result<RunResult, TransportError> {
        Ok(run_in_proc(program))
    }
}

/// Load and run a compiled program on a fresh VM, returning its exit code + console output. A
/// read/load failure is reported as exit -1 with the reason as output; an unhandled trap is exit 70.
fn run_in_proc(program_bytes: &[u8]) -> RunResult {
    let program_bytes = resident_bytes(program_bytes);
    let Ok(assembly) = Assembly::read(program_bytes) else {
        return RunResult { exit: -1, stdout: "could not read the compiled program".to_owned() };
    };
    let program = match load(&assembly) {
        Ok(program) => program,
        Err(error) => return RunResult { exit: -1, stdout: format!("load failed: {error}") },
    };
    let mut vm = Vm::new();
    let outcome = run(&program.module, &mut vm, program.entry, Vec::new());
    let exit = match outcome {
        Ok(Some(Value::Int32(code))) => code,
        Ok(_) => 0,
        Err(_) => 70,
    };
    RunResult { exit, stdout: vm.output_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unavailable_repl_reports_the_reason_as_the_result() {
        let mut slot = Some(ReplCell::Unavailable("no references".to_owned()));
        let body = evaluate(&mut slot, "1 + 1", false, "repl");
        assert_eq!(body["result"], json!("REPL unavailable: no references"));
        assert_eq!(body["variablesReference"], json!(0));
    }

    #[test]
    fn a_frame_scoped_submission_is_refused_and_an_unscoped_one_is_not() {
        assert!(frame_scoped_refusal(true, "repl").is_some(), "a frameId scopes it");
        assert!(frame_scoped_refusal(false, "watch").is_some(), "a watch is scoped by definition");
        assert!(frame_scoped_refusal(false, "hover").is_some(), "so is a hover");
        assert_eq!(frame_scoped_refusal(false, "repl"), None, "a plain submission is not");
        assert_eq!(frame_scoped_refusal(false, ""), None, "nor one with no context at all");
    }

    #[test]
    fn a_frame_scoped_submission_never_reaches_the_session() {
        let mut slot = Some(ReplCell::Unavailable("no references".to_owned()));
        let body = evaluate(&mut slot, "x", true, "repl");
        assert!(
            body["result"].as_str().is_some_and(|said| said.contains("cannot read the frame")),
            "got {}",
            body["result"]
        );
        assert!(
            !body["result"].as_str().is_some_and(|said| said.contains("REPL unavailable")),
            "the session was consulted, which is the defect this closes"
        );
    }

    #[test]
    fn a_watch_gets_the_short_form_and_the_console_gets_the_sentence() {
        let watch = frame_scoped_refusal(false, "watch").expect("refused");
        let console = frame_scoped_refusal(true, "repl").expect("refused");
        assert!(watch.len() < 40, "a watch answer is short: {watch}");
        assert!(console.len() > 80, "the console answer says what to do instead: {console}");
        assert!(console.contains("variables view"), "and names it: {console}");
    }

    #[test]
    fn expressions_and_declarations_evaluate_over_the_engine() {
        if LcscCompiler::discover().is_err() {
            eprintln!("skipping: no reference assemblies on this host");
            return;
        }
        let mut slot = None;
        assert_eq!(evaluate(&mut slot, "1 + 2 * 3", false, "repl")["result"], json!("7"));
        assert_eq!(evaluate(&mut slot, "int x = 5;", false, "repl")["result"], json!(""));
        assert_eq!(evaluate(&mut slot, "x * 2", false, "repl")["result"], json!("10"));
    }

    #[test]
    fn a_compile_error_is_reported_and_the_session_recovers() {
        if LcscCompiler::discover().is_err() {
            eprintln!("skipping: no reference assemblies on this host");
            return;
        }
        let mut slot = None;
        let error = evaluate(&mut slot, "nonsense +", false, "repl");
        assert!(
            error["result"].as_str().is_some_and(|text| !text.is_empty()),
            "a compile error should carry diagnostics text; got {error}"
        );
        assert_eq!(evaluate(&mut slot, "1 + 1", false, "repl")["result"], json!("2"));
    }
}
