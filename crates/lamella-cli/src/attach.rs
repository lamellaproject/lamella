//! `lamella run --device`: run a program on a board with its output on your terminal.

use crate::ending::Ended;
use crate::own_lines::{OwnLines, say};
use lamella_debug_backend::{DebugBackend, Stop};
use lamella_wire::Capabilities;
use lamella_wire_host::debug_backend::WireHostBackend;
use lamella_wire_host::on_device::{self, RunEnd, Watch};
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// The default serial baud for a Lamella Link carrier (USB-CDC ignores it; a real UART wants it).
const BAUD: u32 = 115_200;

/// How long to wait on each wire exchange while setting the session up.
const TIMEOUT: Duration = Duration::from_secs(20);


/// Compile `path`, deploy it to the firmware at `target`, and run it there, streaming its output.
/// The class library is the one `folders` names, or else the one the tool finds without it.
pub fn run_on_target(path: &Path, target: &str, folders: &crate::folders::Folders) -> Ended {
    let board = match crate::project::BoardCheck::for_file("run", path) {
        Ok(board) => board,
        Err(error) => {
            eprintln!("{error}");
            return Ended::Failed;
        }
    };
    if let Err(refusal) = crate::bake::refuse_uncompilable(path, cannot_run_on_a_target) {
        eprintln!("{refusal}");
        return Ended::Failed;
    }
    run_image_on_target(
        path,
        target,
        board.as_ref(),
        |_| None,
        |answered| crate::bake::image_for_firmware(path, "run", cannot_run_on_a_target, answered, folders),
    )
}

/// Deploy the image `image_for` builds for the board that answers at `target`, and run it there,
/// streaming its output; `path` names the program in what is said. Once the board has answered, the
/// run opens with the line `first_line` makes from the product model it answered as, if it makes one.
pub fn run_image_on_target(
    path: &Path,
    target: &str,
    board: Option<&crate::project::BoardCheck>,
    first_line: impl FnOnce(u16) -> Option<String>,
    image_for: impl FnOnce(Option<&str>) -> Result<Vec<u8>, String>,
) -> Ended {
    let Some((mut transport, session)) = crate::deploy::connect("run", target, on_device::run_caps(), board)
    else {
        return Ended::Failed;
    };
    let mut err = std::io::stderr();
    if let Some(line) = first_line(session.identity.product_model) {
        say(&mut err, &line);
    }
    let answered = lamella_wire_host::boards::board_answering(session.identity.product_model);
    let image = match image_for(answered.as_deref()) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("{error}");
            return Ended::Failed;
        }
    };
    say(&mut err, &format!("built {} B from {}", image.len(), path.display()));
    if !session.caps.has(Capabilities::EXEC_NO_RESET) {
        drop(transport);
        let board = on_device::the_board(lamella_wire_host::board_name(session.identity.product_model));
        say(
            &mut err,
            &format!(
                "{}'s firmware runs a program over this connection only under the debugger, which \
                 steps ONE thread:\na program that starts a thread or waits on a socket stops there. \
                 Firmware built from this\nrelease runs it under the board's scheduler.",
                on_device::capitalized(&board)
            ),
        );
        return run_under_the_debugger(image, target);
    }
    let mut lines = OwnLines::new(std::io::stderr());
    if let Err(refusal) = on_device::store_image(&mut transport, &session, target, &image, &mut lines) {
        say(&mut err, &refusal);
        return Ended::Failed;
    }
    say(&mut err, &forever_warning());
    let ran = on_device::start_scheduled(
        &mut transport,
        target,
        &mut lines,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        Watch::to_the_end(),
    );
    match ran {
        Ok(end) => report_end(&end, target, &mut err),
        Err(refusal) => {
            say(&mut err, &refusal);
            Ended::Failed
        }
    }
}

/// The line a run's end gets, on standard error, and how the run ends: with the program's own exit
/// code when it returned one, and with 1 for every other end.
fn report_end(end: &RunEnd, target: &str, err: &mut dyn Write) -> Ended {
    say(err, &end.describe(target));
    match end {
        RunEnd::Done(code) => Ended::Program(*code),
        RunEnd::Trapped => Ended::Uncaught,
        _ => Ended::Failed,
    }
}

/// Run `image` at `target` under the debugger, for a board whose firmware cannot start a stored
/// program without a reset: deployed, started halted, then resumed and watched.
fn run_under_the_debugger(image: Vec<u8>, target: &str) -> Ended {
    let mut err = std::io::stderr();
    let mut backend = match WireHostBackend::open_target(target, BAUD, image, TIMEOUT) {
        Ok(backend) => backend,
        Err(error) => {
            say(
                &mut err,
                &format!(
                    "cannot open {target}: {error:?}\n\nthis build can open: {}.\n`lamella devices` \
                     lists what is attached and what to write here.",
                    lamella_wire_host::available_carriers().join(", ")
                ),
            );
            return Ended::Failed;
        }
    };
    if let Err(reason) = backend.launch() {
        say(
            &mut err,
            &format!(
                "{target} would not take the program: {reason}.\n\nThis board's firmware runs a program \
                 over this connection only under the debugger, so it has to\noffer the debug \
                 capabilities -- stepping and breakpoints. A board whose firmware does not can\nstill \
                 take `lamella deploy`, which needs neither."
            ),
        );
        return Ended::Failed;
    }

    say(&mut err, &forever_warning());
    say(&mut err, &format!("running on {target}; output follows."));
    stream(&mut backend, &mut err)
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

/// Drive a debugger-run program to its end, printing output as it arrives.
///
/// **EVERY STOP DRAINS THE OUTPUT BEFORE IT IS ACTED ON.** A program that faults has usually
/// printed something explaining itself, and printing the fault first would put the explanation
/// after the complaint.
fn stream(backend: &mut WireHostBackend, err: &mut dyn Write) -> Ended {
    show(backend);
    let mut stop = backend.resume();
    loop {
        show(backend);
        match stop {
            Stop::Running => stop = backend.poll(),
            Stop::Done => {
                let code = backend.exit_code();
                say(err, &format!("the program ended, exit code {code}."));
                return Ended::Program(code);
            }
            Stop::Fault(why) => {
                say(err, &format!("the program stopped: {why}"));
                return Ended::Uncaught;
            }
            Stop::Exception(info) => {
                let what = if info.message.is_empty() {
                    info.type_name
                } else {
                    format!("{}: {}", info.type_name, info.message)
                };
                say(err, &format!("the program stopped on an exception: {what}"));
                return Ended::Uncaught;
            }
            Stop::Breakpoint | Stop::Step => {
                say(
                    err,
                    "the target halted, which `run` has no way to continue from -- it sets no \
                     breakpoints.\nA debugger session (the editor, over `lamella-dap`) is the tool \
                     that can.",
                );
                return Ended::Failed;
            }
        }
    }
}

/// Print whatever a debugger-run program has produced since the last look.
fn show(backend: &mut WireHostBackend) {
    if let Some(text) = backend.take_output() {
        print!("{text}");
        let _ = std::io::stdout().flush();
    }
}

/// What to tell somebody before they wait on a program that will not end.
///
/// **STOPPING THIS TOOL MAY LEAVE THE PROGRAM RUNNING**, and saying so up front is the difference
/// between a known cost and a mysterious board. The recovery is stated rather than implied: the
/// next command that connects takes the board back.
#[must_use]
pub fn forever_warning() -> String {
    "the program is deployed to the board and stays there: the board runs it at every reset\nuntil \
     another is deployed. A program that loops forever runs until you stop this tool, and\nstopping \
     it may leave the program running; the next `lamella run` or `lamella deploy` takes\nthe board \
     back."
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            let ended = report_end(&end, "usb", &mut err);
            (ended, String::from_utf8(err).unwrap())
        };
        assert_eq!(said(RunEnd::Done(0)), (Ended::Program(0), "lamella: the program ended, exit code 0.\n".to_owned()));
        assert_eq!(said(RunEnd::Done(3)).0, Ended::Program(3), "exit 3 comes back as 3");
        assert_eq!(
            said(RunEnd::Trapped),
            (Ended::Uncaught, "lamella: the program stopped: it trapped, as reported above.\n".to_owned())
        );
        let (ended, err) = said(RunEnd::Dropped(lamella_wire::TransportError::Closed));
        assert_eq!(ended, Ended::Failed);
        assert!(err.starts_with("lamella: the connection to usb dropped while the program was running"), "{err}");
        let (ended, err) = said(RunEnd::NoLongerRunning);
        assert_eq!(ended, Ended::Failed);
        assert!(err.starts_with("lamella: the program is no longer running on the board"), "{err}");
        assert!(err.lines().all(|line| line.starts_with("lamella:")), "{err}");
    }
}
