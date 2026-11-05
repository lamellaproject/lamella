//! Run a program through Lamella Link, streaming its output on the program's own streams.
//!
//! The board stores the image, starts it under its scheduler without a reset, and is followed
//! until the program ends or the host disconnects. A project run keeps its introduction brief.

use crate::ending::Ended;
use crate::own_lines::{fail, full_failure, say};
use lamella_wire::{Capabilities, Negotiated, Transport};
use lamella_wire_host::on_device::{self, RunEnd, Watch, Wording};
use std::io::Write;
use std::path::Path;

/// Compile a program, store it on the named board, and follow it to its end.
pub fn run_on_target(path: &Path, target: &str, folders: &crate::folders::Folders) -> Ended {
    let board = match crate::project::BoardCheck::for_file("run", path) {
        Ok(board) => board,
        Err(error) => { full_failure(&mut std::io::stderr(), &error); return Ended::Failed; }
    };
    if let Err(refusal) = crate::bake::refuse_uncompilable(path, cannot_run_on_a_target) {
        full_failure(&mut std::io::stderr(), &refusal);
        return Ended::Failed;
    }
    run_image_on_target(path, target, board.as_ref(), |_| None,
        |answered| crate::bake::image_for_firmware(path, "run", cannot_run_on_a_target, answered, folders),
        Wording::Full)
}

/// Build an image for the board that answers, then store, start and follow it.
pub fn run_image_on_target(
    path: &Path,
    target: &str,
    board: Option<&crate::project::BoardCheck>,
    first_line: impl FnOnce(u16) -> Option<String>,
    image_for: impl FnOnce(Option<&str>) -> Result<Vec<u8>, String>,
    wording: Wording,
) -> Ended {
    let mut err = std::io::stderr();
    let (mut transport, session) = match crate::deploy::connect_checked("run", target, on_device::run_caps(), board) {
        Ok(connected) => connected,
        Err(refusal) => { failure(&mut err, &refusal, wording); return Ended::Failed; }
    };
    run_connected(path, target, &mut transport, &session, first_line, image_for, wording,
        &mut std::io::stdout(), &mut err, &mut std::io::stderr(), &mut crate::ending::watch_for_ctrl_c, &crate::ending::ctrl_c_pressed)
}

/// The connected part of a run, with transport, streams and cancellation supplied by the caller.
#[allow(clippy::too_many_arguments)]
fn run_connected(
    path: &Path,
    target: &str,
    transport: &mut impl Transport,
    session: &Negotiated,
    first_line: impl FnOnce(u16) -> Option<String>,
    image_for: impl FnOnce(Option<&str>) -> Result<Vec<u8>, String>,
    wording: Wording,
    out: &mut dyn Write,
    err: &mut dyn Write,
    program_err: &mut dyn Write,
    watch: &mut dyn FnMut() -> bool,
    stop: &dyn Fn() -> bool,
) -> Ended {
    if !session.caps.has(Capabilities::EXEC_NO_RESET) {
        fail(err, "The board cannot start a stored program.");
        return Ended::Failed;
    }
    if let Some(line) = first_line(session.identity.product_model) {
        say(err, &line);
    }
    let answered = lamella_wire_host::boards::board_answering(session.identity.product_model);
    let image = match image_for(answered.as_deref()) {
        Ok(image) => image,
        Err(error) => { failure(err, &error, wording); return Ended::Failed; }
    };
    if wording == Wording::Full {
        say(err, &format!("Built {} B from {}", image.len(), path.display()));
    }
    if let Err(refusal) = on_device::store_image_worded(transport, session, target, &image, err, wording) {
        failure(err, &refusal, wording);
        return Ended::Failed;
    }
    if wording == Wording::Full { say(err, &forever_warning()); }
    let _ = watch();
    let watch = Watch { stop: Some(stop), ..Watch::to_the_end() };
    match on_device::start_scheduled_worded(transport, target, err, out, program_err, watch, wording) {
        Ok(end) => report_end(&end, target, err, wording),
        Err(refusal) => { failure(err, &refusal, wording); Ended::Failed }
    }
}

fn failure(err: &mut dyn Write, refusal: &str, wording: Wording) {
    match wording {
        Wording::Brief => fail(err, refusal),
        Wording::Full => full_failure(err, refusal),
    }
}

/// A returned code stays the program's; a project run adds no closing line to it.
fn report_end(end: &RunEnd, target: &str, err: &mut dyn Write, wording: Wording) -> Ended {
    if wording == Wording::Full || !matches!(end, RunEnd::Done(_)) {
        if wording == Wording::Brief { fail(err, &end.describe(target)); }
        else { say(err, &end.describe(target)); }
    }
    match end {
        RunEnd::Done(code) => Ended::Program(*code),
        RunEnd::Interrupted => Ended::Interrupted,
        RunEnd::Trapped => Ended::Uncaught,
        _ => Ended::Failed,
    }
}

/// `run --device`'s wording for a source it cannot compile.
///
/// **IT IS NOT `deploy`'s SENTENCE AND MUST NOT BECOME IT.** `deploy` says `--board` builds an
/// image ahead of time, which is false for this verb, whose other mode runs the program on THIS
/// machine. That is the one mode a Python program does have here, so it is the one named. A shared
/// sentence would point the reader at something this verb cannot do.
fn cannot_run_on_a_target(path: &Path, what: &crate::deploy::Uncompilable) -> String {
    match what {
        crate::deploy::Uncompilable::Python => format!(
            "lamella run: {} is a Python program, and running ON a board compiles C#.\n\n\
             A Python program reaches a board as a BUNDLE, whose host-side send is not a library \
             call\nthis tool can make yet. What DOES work today is running it on this machine:\n\
             \x20   lamella run {}",
            path.display(),
            path.display()
        ),
        crate::deploy::Uncompilable::Other => format!(
            "lamella run: {} is not a C# file, and running ON a board compiles C#.",
            path.display()
        ),
    }
}

/// What to tell somebody before they wait on a program that will not end.
///
/// **STOPPING THIS TOOL MAY LEAVE THE PROGRAM RUNNING**, and saying so up front is the difference
/// between a known cost and a mysterious board. The recovery is stated rather than implied: the
/// next command that connects takes the board back.
#[must_use]
pub fn forever_warning() -> String {
    "The program is deployed to the board and stays there: the board runs it at every reset\nuntil \
     another is deployed. A program that loops forever runs until you stop this tool, and\nstopping \
     it may leave the program running; the next `lamella run` or `lamella deploy` takes\nthe board \
     back."
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Shared(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl Shared {
        fn text(&self) -> String { String::from_utf8(self.0.borrow().clone()).unwrap() }
    }

    impl Write for Shared {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
    }

    /// A protocol peer in memory; it records every request and never opens a carrier.
    struct Board {
        replies: lamella_wire::MemTransport,
        requests: Vec<u8>,
        exit: Option<i32>,
    }

    impl Board {
        fn new(exit: Option<i32>) -> Self {
            Self { replies: lamella_wire::MemTransport::new(), requests: Vec::new(), exit }
        }

        fn reply(&mut self, kind: u8, seq: u16, payload: &[u8]) {
            let mut wire = lamella_wire::MemTransport::new();
            wire.send(kind, seq, payload).unwrap();
            self.replies.feed(&wire.take_sent());
        }
    }

    impl Transport for Board {
        fn send(&mut self, kind: u8, seq: u16, _: &[u8]) -> Result<(), lamella_wire::TransportError> {
            use lamella_wire_host::{debug, deploy, exec};
            self.requests.push(kind);
            match kind {
                debug::ABORT => self.reply(exec::EXEC_ACK, seq, &[exec::ack::IDLE]),
                deploy::DEPLOY_STATUS => self.reply(deploy::DEPLOY_STATUS_RESULT, seq, &[0; 10]),
                deploy::DEPLOY_IMAGE => self.reply(deploy::XFER_RESULT, seq, &[deploy::xfer::MATCHED]),
                exec::EXEC => {
                    self.reply(exec::EXEC_ACK, seq, &[exec::ack::STARTED]);
                    for (stream, text) in [(debug::output::STDOUT, "attach-out-marker\n"), (debug::output::STDERR, "attach-err-marker\n")] {
                        let mut output = vec![stream, 0];
                        output.extend_from_slice(text.as_bytes());
                        self.reply(debug::EVT_OUTPUT, 0, &output);
                    }
                    if let Some(code) = self.exit {
                        let mut stopped = vec![debug::reason::DONE];
                        stopped.extend_from_slice(&[0; 8]);
                        stopped.extend_from_slice(&code.to_le_bytes());
                        stopped.push(0);
                        self.reply(debug::EVT_STOPPED, seq, &stopped);
                    }
                }
                _ => panic!("unexpected request {kind}"),
            }
            Ok(())
        }
        fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, lamella_wire::TransportError> {
            self.replies.poll()
        }
    }

    fn session() -> Negotiated {
        let caps = Capabilities(Capabilities::BAKED_IMAGE | Capabilities::EXEC_NO_RESET);
        Negotiated { version: lamella_wire::PROTOCOL_VERSION, caps, target_caps: caps,
            identity: lamella_wire::TargetIdentity::default(), max_inbound_payload: None }
    }

    /// The project introduction precedes image preparation and the two program streams stay apart.
    #[test]
    fn a_project_run_prints_before_building_and_adds_no_exit_line() {
        for code in [0, 7] {
            let mut board = Board::new(Some(code));
            let introduction = crate::own_lines::running_interpreted("test board");
            let mut err = Shared::default();
            let before_build = err.clone();
            let (mut out, mut program_err) = (Vec::new(), Vec::new());
            let ended = run_connected(Path::new("App.dll"), "usb", &mut board, &session(),
                |_| Some(introduction.clone()), |_| {
                    assert_eq!(before_build.text(), format!("{introduction}\n"));
                    Ok(vec![0; 256])
                }, Wording::Brief, &mut out, &mut err, &mut program_err, &mut || true, &|| false);
            assert_eq!(ended, Ended::Program(code));
            assert_eq!(out, b"attach-out-marker\n");
            assert_eq!(program_err, b"attach-err-marker\n");
            assert_eq!(err.text(), format!("{introduction}\nErasing the program area of flash...\n\n"));
        }
    }

    /// Stopping after the program's output disconnects without an abort or a later status query.
    #[test]
    fn a_project_run_disconnects_after_output_without_stopping_the_board() {
        let mut board = Board::new(None);
        let mut out = Shared::default();
        let seen = out.clone();
        let (mut err, mut program_err) = (Vec::new(), Vec::new());
        let ended = run_connected(Path::new("App.dll"), "usb", &mut board, &session(), |_| None,
            |_| Ok(vec![0; 256]), Wording::Brief, &mut out, &mut err, &mut program_err,
            &mut || true, &|| seen.text().contains("attach-out-marker"));
        assert_eq!(ended, Ended::Interrupted);
        assert_eq!(String::from_utf8(err).unwrap(), "Erasing the program area of flash...\n\nDisconnected from the board.\n");
        assert_eq!(board.requests.last(), Some(&lamella_wire_host::exec::EXEC));
        assert_eq!(board.requests.iter().filter(|&&kind| kind == lamella_wire_host::debug::ABORT).count(), 1);
    }

    /// A board that cannot start under its scheduler is refused before image preparation or transfer.
    #[test]
    fn a_run_needs_the_scheduler_before_it_builds_or_sends_anything() {
        let mut board = Board::new(None);
        let mut session = session();
        session.caps = Capabilities(Capabilities::BAKED_IMAGE);
        let (mut out, mut err, mut program_err) = (Vec::new(), Vec::new(), Vec::new());
        let ended = run_connected(Path::new("App.dll"), "usb", &mut board, &session,
            |_| panic!("introduced an unsupported run"), |_| panic!("built an unsupported run"),
            Wording::Brief, &mut out, &mut err, &mut program_err, &mut || panic!("started a watch"), &|| false);
        assert_eq!(ended, Ended::Failed);
        assert_eq!(err, b"The board cannot start a stored program.\n");
        assert!(board.requests.is_empty() && out.is_empty() && program_err.is_empty());
    }

    /// A direct run keeps the full diagnostic body from compilation.
    #[test]
    fn a_direct_runs_build_failure_keeps_each_diagnostic() {
        let mut board = Board::new(None);
        let (mut out, mut err, mut program_err) = (Vec::new(), Vec::new(), Vec::new());
        let ended = run_connected(Path::new("App.cs"), "usb", &mut board, &session(), |_| None,
            |_| Err("lamella run: compilation failed.\n\nApp.cs(1,2): error CS0103: FirstName\nApp.cs(2,3): error CS0103: SecondName".to_owned()),
            Wording::Full, &mut out, &mut err, &mut program_err, &mut || true, &|| false);
        assert_eq!(ended, Ended::Failed);
        assert_eq!(String::from_utf8(err).unwrap(), "Compilation failed.\n\nApp.cs(1,2): error CS0103: FirstName\nApp.cs(2,3): error CS0103: SecondName\n");
        assert!(board.requests.is_empty());
    }

    /// **THE COST OF STOPPING IS STATED BEFORE IT IS PAID**, with the recovery, and so is what the
    /// run leaves on the board.
    #[test]
    fn the_warning_names_both_the_cost_and_the_recovery() {
        let text = forever_warning();
        let flat = text.replace('\n', " ");
        assert!(flat.contains("may leave the program running"), "the cost: {text}");
        assert!(flat.contains("takes the board back"), "the recovery: {text}");
        assert!(flat.contains("runs it at every reset"), "and what stays on the board: {text}");
        let mut said = Vec::new();
        say(&mut said, &text);
        let said = String::from_utf8(said).expect("text");
        assert!(said.lines().all(|line| line.chars().count() <= 100), "{said}");
    }

    /// **A REFUSAL RECOMMENDS ONLY WHAT THIS TOOL STILL TAKES.** `run --board` was removed, and a
    /// Python program's refusal went on recommending it, so a reader who followed it met a second
    /// refusal for the flag the first one told them to type.
    #[test]
    fn a_python_program_is_pointed_at_a_run_this_tool_still_takes() {
        let text =
            cannot_run_on_a_target(Path::new("app.py"), &crate::deploy::Uncompilable::Python);
        assert!(!text.contains("--board"), "a removed flag: {text}");
        assert!(
            text.contains("lamella run app.py") && text.contains("on this machine"),
            "the run that works: {text}"
        );
    }

    /// **HOW A RUN ENDED IS THIS TOOL'S LINE, ON STANDARD ERROR, AND THE PROGRAM'S CODE COMES
    /// BACK.** A program's exit code is the run's, unchanged; a trap is 1; every other end is 1;
    /// and every line of the end is marked as this tool's.
    #[test]
    fn a_run_end_is_said_on_standard_error_as_this_tools() {
        let said = |end: RunEnd| {
            let mut err = Vec::new();
            let ended = report_end(&end, "usb", &mut err, Wording::Full);
            (ended, String::from_utf8(err).unwrap())
        };
        assert_eq!(said(RunEnd::Done(0)), (Ended::Program(0), "The program ended, exit code 0.\n".to_owned()));
        assert_eq!(said(RunEnd::Done(3)).0, Ended::Program(3), "exit 3 comes back as 3");
        assert_eq!(
            said(RunEnd::Trapped),
            (Ended::Uncaught, "The program stopped: it trapped, as reported above.\n".to_owned())
        );
        let (ended, err) = said(RunEnd::Dropped(lamella_wire::TransportError::Closed));
        assert_eq!(ended, Ended::Failed);
        assert!(err.starts_with("The connection to usb dropped while the program was running"), "{err}");
        let (ended, err) = said(RunEnd::NoLongerRunning);
        assert_eq!(ended, Ended::Failed);
        assert!(err.starts_with("The program is no longer running on the board"), "{err}");
        assert!(!err.contains("lamella:"), "{err}");
    }
}
