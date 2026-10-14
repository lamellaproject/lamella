//! `lamella run --device`: run a program on a board with its output on your terminal.

use lamella_debug_backend::{DebugBackend, Stop};
use lamella_wire::Capabilities;
use lamella_wire_host::debug_backend::WireHostBackend;
use lamella_wire_host::on_device::{self, RunEnd, Watch};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

/// The default serial baud for a Lamella Link carrier (USB-CDC ignores it; a real UART wants it).
const BAUD: u32 = 115_200;

/// How long to wait on each wire exchange while setting the session up.
const TIMEOUT: Duration = Duration::from_secs(20);


/// Compile `path`, deploy it to the firmware at `target`, and run it there, streaming its output.
pub fn run_on_target(path: &Path, target: &str) -> ExitCode {
    let board = match crate::project::BoardCheck::for_file("run", path) {
        Ok(board) => board,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(refusal) = crate::bake::refuse_uncompilable(path, cannot_run_on_a_target) {
        eprintln!("{refusal}");
        return ExitCode::FAILURE;
    }

    let Some((mut transport, session)) =
        crate::deploy::connect("run", target, on_device::run_caps(), board.as_ref())
    else {
        return ExitCode::FAILURE;
    };
    let answered = lamella_wire_host::boards::board_answering(session.identity.product_model);
    let image = match crate::bake::image_for_firmware(path, "run", cannot_run_on_a_target, answered.as_deref()) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    println!("built {} B from {}", image.len(), path.display());
    if !session.caps.has(Capabilities::EXEC_NO_RESET) {
        drop(transport);
        let board = on_device::the_board(lamella_wire_host::board_name(session.identity.product_model));
        println!(
            "{}'s firmware runs a program over this connection only under the debugger, which steps \
             ONE thread:\na program that starts a thread or waits on a socket stops there. Firmware \
             built from this\nrelease runs it under the board's scheduler.\n",
            on_device::capitalized(&board)
        );
        return run_under_the_debugger(image, target);
    }
    let ran = on_device::run_scheduled(
        &mut transport,
        &session,
        target,
        &image,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        Watch::to_the_end(),
    );
    match ran {
        Ok(end) => report_end(&end, target, &mut std::io::stdout(), &mut std::io::stderr()),
        Err(refusal) => {
            eprintln!("lamella run: {refusal}");
            ExitCode::FAILURE
        }
    }
}

/// The line a run's end gets, and the exit code it gives this tool.
///
/// The program's exit code is REPORTED rather than forwarded: a nonzero code would be
/// indistinguishable from the tool failing.
fn report_end(end: &RunEnd, target: &str, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    let said = end.describe(target);
    match end {
        RunEnd::Done(code) => {
            let _ = writeln!(out, "\n{said}");
            if *code == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        RunEnd::Dropped(_) => {
            let _ = writeln!(err, "\nlamella run: {said}");
            ExitCode::FAILURE
        }
        _ => {
            let _ = writeln!(err, "\n{said}");
            ExitCode::FAILURE
        }
    }
}

/// Run `image` at `target` under the debugger, for a board whose firmware cannot start a stored
/// program without a reset: deployed, started halted, then resumed and watched.
fn run_under_the_debugger(image: Vec<u8>, target: &str) -> ExitCode {
    let mut backend = match WireHostBackend::open_target(target, BAUD, image, TIMEOUT) {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("lamella run: cannot open {target}: {error:?}");
            eprintln!(
                "\nthis build can open: {}.\n\
                 `lamella devices` lists what is attached and what to write here.",
                lamella_wire_host::available_carriers().join(", ")
            );
            return ExitCode::FAILURE;
        }
    };
    if let Err(reason) = backend.launch() {
        eprintln!(
            "lamella run: {target} would not take the program: {reason}.\n\n\
             This board's firmware runs a program over this connection only under the debugger, so \
             it has to\noffer the debug capabilities -- stepping and breakpoints. A board whose \
             firmware does not can\nstill take `lamella deploy`, which needs neither."
        );
        return ExitCode::FAILURE;
    }

    println!("running on {target}; output follows.\n");
    stream(&mut backend)
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
fn stream(backend: &mut WireHostBackend) -> ExitCode {
    show(backend);
    let mut stop = backend.resume();
    loop {
        show(backend);
        match stop {
            Stop::Running => stop = backend.poll(),
            Stop::Done => {
                let code = backend.exit_code();
                println!("\nthe program ended, exit code {code}.");
                return if code == 0 {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                };
            }
            Stop::Fault(why) => {
                eprintln!("\nthe program stopped: {why}");
                return ExitCode::FAILURE;
            }
            Stop::Breakpoint | Stop::Step => {
                eprintln!(
                    "\nthe target halted, which `run` has no way to continue from -- it sets no \
                     breakpoints.\nA debugger session (the editor, over `lamella-dap`) is the tool \
                     that can."
                );
                return ExitCode::FAILURE;
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
    "(the program is deployed to the board and stays there: the board runs it at every reset until \
     another\n is deployed. A program that loops forever runs until you stop this tool, and stopping \
     it may leave the\n program running; the next `lamella run` or `lamella deploy` takes the board \
     back.)\n"
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
        assert!(text.contains("may leave the\n program running"), "the cost: {text}");
        assert!(text.contains("takes the board back"), "the recovery: {text}");
        assert!(text.contains("runs it at every reset"), "and what stays on the board: {text}");
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

    /// **HOW A RUN ENDED IS SAID WHERE A TERMINAL SHOWS IT, AND ONLY A RETURN OF 0 IS SUCCESS.** The
    /// program's exit code goes to standard output and is reported rather than forwarded; every other
    /// end goes to standard error, and a lost connection is this command's failure, so it names the
    /// command.
    #[test]
    fn a_run_end_is_said_on_the_stream_a_terminal_reads_it_from() {
        let said = |end: RunEnd| {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = report_end(&end, "usb", &mut out, &mut err);
            (format!("{code:?}"), String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
        };
        let (success, failure) = (format!("{:?}", ExitCode::SUCCESS), format!("{:?}", ExitCode::FAILURE));
        assert_eq!(said(RunEnd::Done(0)), (success, "\nthe program ended, exit code 0.\n".to_owned(), String::new()));
        assert_eq!(said(RunEnd::Done(3)).0, failure, "exit 3 is reported, and is not success");
        let (code, out, err) = said(RunEnd::Trapped);
        assert_eq!((code, out.as_str()), (failure, ""));
        assert_eq!(err, "\nthe program stopped: it trapped, as reported above.\n");
        let (_, _, err) = said(RunEnd::Dropped(lamella_wire::TransportError::Closed));
        assert!(err.starts_with("\nlamella run: the connection to usb dropped while the program was running"), "{err}");
        let (_, _, err) = said(RunEnd::NoLongerRunning);
        assert!(err.starts_with("\nthe program is no longer running on the board"), "{err}");
    }
}
