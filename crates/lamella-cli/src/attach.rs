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

/// What a run stores on the board: a baked C# image, or a Python bundle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sent {
    Image,
    Bundle,
}

/// Compile a C# program, store it on the named board, and follow it to its end. A Python program
/// takes [`run_python_on_target`], which every build has.
#[cfg(feature = "bake")]
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
#[cfg(feature = "bake")]
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
    run_connected(path, target, &mut transport, &session, first_line, image_for, wording, Sent::Image,
        &mut std::io::stdout(), &mut err, &mut std::io::stderr(), &mut crate::ending::watch_for_ctrl_c, &crate::ending::ctrl_c_pressed)
}

/// Compile a Python program into a bundle for the board that answers, store it there, and follow it
/// to its end, as a C# program is followed. Nothing here is behind `bake`: a bundle is not baked.
pub(crate) fn run_python_on_target(path: &Path, target: &str) -> Ended {
    let mut err = std::io::stderr();
    if crate::python_on_device::is_bundle(path) {
        full_failure(&mut err, &format!(
            "lamella run: {} is a Python bundle, and a run builds its program from source. A bundle is stored \
             on a board and started there by deploy:\n    lamella deploy {} --device {target}",
            path.display(),
            path.display()
        ));
        return Ended::Failed;
    }
    let (mut transport, session) = match crate::deploy::connect_checked("run", target, on_device::run_caps(), None) {
        Ok(connected) => connected,
        Err(refusal) => { full_failure(&mut err, &refusal); return Ended::Failed; }
    };
    run_connected(path, target, &mut transport, &session, |_| None,
        |answered| crate::python_on_device::bundle_for("run", path, answered), Wording::Full, Sent::Bundle,
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
    sent: Sent,
    out: &mut dyn Write,
    err: &mut dyn Write,
    program_err: &mut dyn Write,
    watch: &mut dyn FnMut() -> bool,
    stop: &dyn Fn() -> bool,
) -> Ended {
    let program = path.display().to_string();
    let takes = match sent {
        Sent::Image => crate::python_on_device::takes_csharp("run", &program, target, session),
        Sent::Bundle => crate::python_on_device::takes_python("run", path, target, session),
    };
    if let Err(refusal) = takes {
        failure(err, &refusal, wording);
        return Ended::Failed;
    }
    if !session.caps.has(Capabilities::EXEC_NO_RESET) {
        fail(err, "The board cannot start a stored program.");
        return Ended::Failed;
    }
    let answered = lamella_wire_host::boards::board_answering(session.identity.product_model);
    let image = match image_for(answered.as_deref()) {
        Ok(image) => image,
        Err(error) => { failure(err, &error, wording); return Ended::Failed; }
    };
    if wording == Wording::Full {
        say(err, &format!("Built {} B from {}", image.len(), path.display()));
    }
    let stored = match sent {
        Sent::Image => on_device::store_image_worded(transport, session, target, &image, err, wording),
        Sent::Bundle => on_device::store_bundle_worded(transport, session, target, &image, err, wording),
    };
    if let Err(refusal) = stored {
        failure(err, &refusal, wording);
        return Ended::Failed;
    }
    if wording == Wording::Full { say(err, &forever_warning()); }
    let _ = watch();
    let watch = Watch { stop: Some(stop), ..Watch::to_the_end() };
    let introduction = first_line(session.identity.product_model);
    match on_device::start_scheduled_worded(transport, target, err, out, program_err, watch, wording, introduction.as_deref()) {
        Ok(end) => report_end(&end, target, err, wording, sent),
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
fn report_end(end: &RunEnd, target: &str, err: &mut dyn Write, wording: Wording, sent: Sent) -> Ended {
    if wording == Wording::Full || !matches!(end, RunEnd::Done(_)) {
        let sentence = match end {
            RunEnd::Trapped(code) if sent == Sent::Image => crate::ending::stopped_sentence(*code),
            _ => end.describe(target),
        };
        if wording == Wording::Brief { fail(err, &sentence); }
        else { say(err, &sentence); }
    }
    match end {
        RunEnd::Done(code) => Ended::Program(*code),
        RunEnd::Interrupted => Ended::Interrupted,
        RunEnd::Trapped(code) => Ended::Stopped(*code),
        _ => Ended::Failed,
    }
}

/// `run --device`'s wording for a source it cannot compile.
///
/// **IT IS NOT `deploy`'s SENTENCE AND MUST NOT BECOME IT.** `deploy` says `--board` builds an
/// image ahead of time, which is false for this verb, whose other mode runs the program on THIS
/// machine. That is the one mode a Python program does have here, so it is the one named. A shared
/// sentence would point the reader at something this verb cannot do.
#[cfg(feature = "bake")]
fn cannot_run_on_a_target(path: &Path, what: &crate::deploy::Uncompilable) -> String {
    match what {
        crate::deploy::Uncompilable::Python => format!(
            "lamella run: {} is a Python program, which this route does not build. It runs on a \
             board whose\nLamella firmware runs Python, and on this machine:\n\
             \x20   lamella run {} --device <device>\n\
             \x20   lamella run {}",
            path.display(),
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
                deploy::DEPLOY_IMAGE | deploy::DEPLOY_BUNDLE => self.reply(deploy::XFER_RESULT, seq, &[deploy::xfer::MATCHED]),
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
        let cil = lamella_wire::Surface { tier: lamella_wire::msg::tier::CIL, ..lamella_wire::Surface::default() };
        Negotiated { version: lamella_wire::PROTOCOL_VERSION, caps, target_caps: caps,
            identity: lamella_wire::TargetIdentity::default().with_surface(cil),
            max_inbound_payload: Some(lamella_wire::MAX_PAYLOAD as u16),
            image_formats: Some(vec![lamella_cil_runtime::BAKED_IMAGE_FORMAT]) }
    }

    /// The fewest bytes that a host reads as an image of the format this tool writes holding a program,
    /// which is what it asks of an image before it sends it: an image that passes the checks a board's boot
    /// makes of an image's bytes ([`lamella_cil_runtime::baked_image_refusal`]), an entry point naming its
    /// one method, whose body is one `ret`, after the shortest directory the format writes
    /// ([`lamella_cil_runtime::BAKED_IMAGE_MIN_LEN`]), its checksum the one its bytes hash to. Those checks
    /// are not all of a boot: they leave out the board's intrinsic registry, and of the entry's body they
    /// ask only a length of at least one byte inside the arena, not that it runs.
    fn an_image() -> Vec<u8> {
        let record = lamella_cil_runtime::BAKED_IMAGE_MIN_LEN;
        let mut image = vec![0; record + 25];
        image[..4].copy_from_slice(b"LML1");
        image[4..8].copy_from_slice(&u32::from(lamella_cil_runtime::BAKED_IMAGE_FORMAT).to_le_bytes());
        let word = |image: &mut Vec<u8>, index: usize, value: u64| image[8 + index * 8..16 + index * 8].copy_from_slice(&value.to_le_bytes());
        word(&mut image, 0, 1);
        word(&mut image, 1, 1);
        word(&mut image, 16, (record - (8 + 19 * 8)) as u64);
        word(&mut image, 17, 25);
        image[record + 8..record + 16].copy_from_slice(&[24, 0, 0, 0, 1, 0, 0, 0]);
        image[record + 24] = 0x2A;
        if let Some(lamella_cil_runtime::module::BakeError::ChecksumMismatch { computed, .. }) =
            lamella_cil_runtime::baked_image_refusal(&image)
        {
            word(&mut image, 18, computed);
        }
        assert!(lamella_cil_runtime::baked_image_holds_a_program(&image), "the fixture must be an image a board's boot takes");
        image
    }

    /// The project introduction follows flashing and the empty line; program streams stay apart.
    #[test]
    fn a_project_run_prints_after_flashing_and_adds_no_exit_line() {
        for code in [0, 7] {
            let mut board = Board::new(Some(code));
            let introduction = crate::own_lines::running_interpreted("test board");
            let mut err = Shared::default();
            let before_build = err.clone();
            let (mut out, mut program_err) = (Vec::new(), Vec::new());
            let ended = run_connected(Path::new("App.dll"), "usb", &mut board, &session(),
                |_| Some(introduction.clone()), |_| {
                    assert_eq!(before_build.text(), "");
                    Ok(an_image())
                }, Wording::Brief, Sent::Image, &mut out, &mut err, &mut program_err, &mut || true, &|| false);
            assert_eq!(ended, Ended::Program(code));
            assert_eq!(out, b"attach-out-marker\n");
            assert_eq!(program_err, b"attach-err-marker\n");
            assert_eq!(err.text(), format!("Writing flash: 1289 of 1289 bytes.\nFlash complete\n\n{introduction}\n"));
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
            |_| Ok(an_image()), Wording::Brief, Sent::Image, &mut out, &mut err, &mut program_err,
            &mut || true, &|| seen.text().contains("attach-out-marker"));
        assert_eq!(ended, Ended::Interrupted);
        assert_eq!(String::from_utf8(err).unwrap(), "Writing flash: 1289 of 1289 bytes.\nFlash complete\n\nDisconnected from the board.\n");
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
            Wording::Brief, Sent::Image, &mut out, &mut err, &mut program_err, &mut || panic!("started a watch"), &|| false);
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
            Wording::Full, Sent::Image, &mut out, &mut err, &mut program_err, &mut || true, &|| false);
        assert_eq!(ended, Ended::Failed);
        assert_eq!(String::from_utf8(err).unwrap(), "Compilation failed.\n\nApp.cs(1,2): error CS0103: FirstName\nApp.cs(2,3): error CS0103: SecondName\n");
        assert!(board.requests.is_empty());
    }

    /// The session with a board whose firmware runs Python.
    fn python_session() -> Negotiated {
        let caps = Capabilities(Capabilities::BUNDLE | Capabilities::EXEC_NO_RESET);
        let python = lamella_wire::Surface { tier: lamella_wire::msg::tier::PYTHON, ..lamella_wire::Surface::default() };
        Negotiated { version: lamella_wire::PROTOCOL_VERSION, caps, target_caps: caps,
            identity: lamella_wire::TargetIdentity::default().with_surface(python),
            max_inbound_payload: Some(lamella_wire::MAX_PAYLOAD as u16),
            image_formats: Some(vec![lamella_py_bytecode::BUNDLE_FORMAT_VERSION]) }
    }

    /// **A PYTHON PROGRAM IS STORED BY ITS OWN MESSAGE AND FOLLOWED AS A C# ONE IS**: its bundle goes
    /// by `DEPLOY_BUNDLE`, never down the image path, and its output and exit code come back.
    #[test]
    fn a_python_run_stores_its_bundle_by_deploy_bundle_and_follows_it() {
        use lamella_wire_host::deploy;
        let mut board = Board::new(Some(3));
        let (mut out, mut err, mut program_err) = (Vec::new(), Vec::new(), Vec::new());
        let mut bundle = vec![0; 300];
        bundle[..4].copy_from_slice(&lamella_py_bytecode::MAGIC);
        bundle[4..6].copy_from_slice(&lamella_py_bytecode::BUNDLE_FORMAT_VERSION.to_le_bytes());
        let ended = run_connected(Path::new("app.py"), "usb", &mut board, &python_session(), |_| None,
            |_| Ok(bundle), Wording::Brief, Sent::Bundle, &mut out, &mut err, &mut program_err,
            &mut || true, &|| false);
        assert_eq!(ended, Ended::Program(3));
        assert!(board.requests.contains(&deploy::DEPLOY_BUNDLE), "{:?}", board.requests);
        assert!(!board.requests.contains(&deploy::DEPLOY_IMAGE), "{:?}", board.requests);
        assert_eq!(out, b"attach-out-marker\n");
    }

    /// **EACH LANGUAGE IS REFUSED BY A BOARD OF THE OTHER, BEFORE ANYTHING IS BUILT OR SENT.**
    #[test]
    fn a_program_is_refused_by_a_board_that_runs_the_other_language() {
        let csharp_board = {
            let mut session = session();
            let cil = lamella_wire::Surface { tier: lamella_wire::msg::tier::CIL, ..lamella_wire::Surface::default() };
            session.identity = session.identity.with_surface(cil);
            session
        };
        for (path, sent, session, said) in [
            ("app.py", Sent::Bundle, csharp_board, "app.py is a Python program, and the board at usb runs C# programs."),
            ("App.cs", Sent::Image, python_session(), "App.cs is C#, and the board at usb runs Python programs."),
        ] {
            let mut board = Board::new(None);
            let (mut out, mut err, mut program_err) = (Vec::new(), Vec::new(), Vec::new());
            let ended = run_connected(Path::new(path), "usb", &mut board, &session, |_| None,
                |_| panic!("built a program the board cannot run"), Wording::Full, sent, &mut out, &mut err,
                &mut program_err, &mut || panic!("started a watch"), &|| false);
            let err = String::from_utf8(err).unwrap();
            assert_eq!(ended, Ended::Failed, "{err}");
            assert!(err.contains(said), "{err}");
            assert!(board.requests.is_empty(), "nothing was sent: {:?}", board.requests);
        }
    }

    /// **A PROGRAM IS REFUSED FOR ITS LANGUAGE BEFORE THE RUN IS ASKED ABOUT**: a board whose firmware
    /// runs Python and starts no stored program without a reset, as the micro:bit v2's, is told a C#
    /// program is C#, as a deploy tells it, and not that it cannot start one.
    #[test]
    fn a_program_of_the_other_language_is_refused_for_its_language_first() {
        let mut session = python_session();
        session.caps = Capabilities(Capabilities::BUNDLE);
        session.target_caps = session.caps;
        let mut board = Board::new(None);
        let (mut out, mut err, mut program_err) = (Vec::new(), Vec::new(), Vec::new());
        let ended = run_connected(Path::new("App.cs"), "COM8", &mut board, &session, |_| None,
            |_| panic!("built a program the board cannot run"), Wording::Full, Sent::Image, &mut out, &mut err,
            &mut program_err, &mut || panic!("started a watch"), &|| false);
        let err = String::from_utf8(err).unwrap();
        assert_eq!(ended, Ended::Failed, "{err}");
        assert!(err.contains("App.cs is C#, and the board at COM8 runs Python programs."), "{err}");
        assert!(board.requests.is_empty(), "nothing was sent: {:?}", board.requests);
    }

    /// A Python program an exception stopped ends with 1, as CPython 3.14's `python` ends it, and is
    /// said in the words a trap has on every board, not in the C# runtime's table of codes.
    #[test]
    fn a_python_run_a_trap_ended_is_said_without_the_csharp_table() {
        let mut err = Vec::new();
        let ended = report_end(&RunEnd::Trapped(1), "usb", &mut err, Wording::Full, Sent::Bundle);
        assert_eq!(ended, Ended::Stopped(1));
        let err = String::from_utf8(err).unwrap();
        assert_eq!(err, "The program stopped with code 1: it trapped, as reported above.\n");
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
    #[cfg(feature = "bake")]
    #[test]
    fn a_python_program_is_pointed_at_a_run_this_tool_still_takes() {
        let text =
            cannot_run_on_a_target(Path::new("app.py"), &crate::deploy::Uncompilable::Python);
        assert!(!text.contains("--board"), "a removed flag: {text}");
        assert!(
            text.contains("lamella run app.py") && text.contains("on this machine"),
            "the run that works: {text}"
        );
        assert!(text.contains("lamella run app.py --device <device>"), "and on a board: {text}");
    }

    /// **HOW A RUN ENDED IS THIS TOOL'S LINE, ON STANDARD ERROR, AND THE PROGRAM'S CODE COMES
    /// BACK.** A program's exit code is the run's, unchanged; a trap ends the run with the code its
    /// stop carries, said in words; every other end is 1; and every line of the end is marked as
    /// this tool's.
    #[test]
    fn a_run_end_is_said_on_standard_error_as_this_tools() {
        let said = |end: RunEnd| {
            let mut err = Vec::new();
            let ended = report_end(&end, "usb", &mut err, Wording::Full, Sent::Image);
            (ended, String::from_utf8(err).unwrap())
        };
        assert_eq!(said(RunEnd::Done(0)), (Ended::Program(0), "The program ended, exit code 0.\n".to_owned()));
        assert_eq!(said(RunEnd::Done(3)).0, Ended::Program(3), "exit 3 comes back as 3");
        for code in [134, 1] {
            let (ended, err) = said(RunEnd::Trapped(code));
            assert_eq!((ended, ended.code()), (Ended::Stopped(code), code), "a trap carrying {code}");
            assert_eq!(err, format!("{}\n", crate::ending::stopped_sentence(code)));
        }
        let (ended, err) = said(RunEnd::Dropped(lamella_wire::TransportError::Closed));
        assert_eq!(ended, Ended::Failed);
        assert!(err.starts_with("The connection to usb dropped while the program was running"), "{err}");
        let (ended, err) = said(RunEnd::NoLongerRunning);
        assert_eq!(ended, Ended::Failed);
        assert!(err.starts_with("The program is no longer running on the board"), "{err}");
        assert!(!err.contains("lamella:"), "{err}");
    }
}
