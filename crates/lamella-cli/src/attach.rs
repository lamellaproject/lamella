//! `lamella run --target`: run a program on a board with its output on your terminal.

use lamella_debug_backend::{DebugBackend, Stop};
use lamella_wire::{Capabilities, Transport};
use lamella_wire_host::debug_backend::WireHostBackend;
use lamella_wire_host::{RunCollector, exec};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// The default serial baud for a Lamella Link carrier (USB-CDC ignores it; a real UART wants it).
const BAUD: u32 = 115_200;

/// How long to wait on each wire exchange while setting the session up.
const TIMEOUT: Duration = Duration::from_secs(20);

/// The sequence number the run goes out under, and so the one its end comes back at. None of the
/// store's steps uses it.
const RUN_SEQ: u16 = 9;

/// How long to wait for the board to acknowledge the start. It answers before the program runs a
/// single instruction, so the answer is prompt.
const START_PATIENCE: Duration = Duration::from_secs(2);

/// How long the program may say nothing before this tool asks the board whether it is still running
/// it.
///
/// A running program answers between two of its own steps and carries on, so asking costs one short
/// frame. A board that has left the program -- its firmware reset after a fault, or after running out
/// of memory -- answers that nothing is executing, and without the question this tool would wait on
/// it forever.
const QUIET_BEFORE_ASKING: Duration = Duration::from_secs(2);


/// Compile `path`, deploy it to the firmware at `target`, and run it there, streaming its output.
pub fn run_on_target(path: &Path, target: &str) -> ExitCode {
    let image = match crate::bake::image_for_firmware(path, "run", cannot_run_on_a_target) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    println!("built {} B from {}", image.len(), path.display());

    let caps = Capabilities(crate::deploy::host_caps().0 | Capabilities::EXEC_NO_RESET);
    let Some((mut transport, session)) = crate::deploy::connect("run", target, caps) else {
        return ExitCode::FAILURE;
    };
    if !session.caps.has(Capabilities::EXEC_NO_RESET) {
        drop(transport);
        let board = crate::deploy::the_board(lamella_wire_host::board_name(session.identity.product_model));
        println!(
            "{}'s firmware runs a program over this connection only under the debugger, which steps \
             ONE thread:\na program that starts a thread or waits on a socket stops there. Firmware \
             built from this\nrelease runs it under the board's scheduler.\n",
            crate::deploy::capitalized(&board)
        );
        return run_under_the_debugger(image, target);
    }
    run_scheduled(
        &mut transport,
        &session,
        target,
        &image,
        &mut std::io::stdout(),
        &mut std::io::stderr(),
        QUIET_BEFORE_ASKING,
    )
}

/// Deploy `image` over `transport`, start it under the board's scheduler without a reset, and print
/// what it prints until it ends.
///
/// `quiet` is how long the program may say nothing before the board is asked whether it is still
/// running it.
fn run_scheduled(
    transport: &mut impl Transport,
    session: &lamella_wire::Negotiated,
    target: &str,
    image: &[u8],
    out: &mut dyn Write,
    err: &mut dyn Write,
    quiet: Duration,
) -> ExitCode {
    if let Err(refusal) = crate::deploy::store_image(transport, session, target, "run", image, out) {
        let _ = writeln!(err, "{refusal}");
        return ExitCode::FAILURE;
    }
    if let Err(failure) = lamella_wire_host::start_execution(
        transport,
        RUN_SEQ,
        exec::exec_source::DEPLOYED,
        exec::exec_flags::NO_RESET,
        START_PATIENCE,
    ) {
        let _ = writeln!(
            err,
            "lamella run: the program is stored on {target}, and {failure}. It starts at the board's \
             next reset."
        );
        return ExitCode::FAILURE;
    }
    let _ = writeln!(out, "running on {target}; output follows.\n");
    let _ = out.flush();
    follow(transport, RUN_SEQ, target, out, err, quiet)
}

/// Print what the run at `seq` prints until it ends, then say how it ended.
///
/// **IT ASKS WHEN THE PROGRAM HAS BEEN QUIET A WHILE.** A board whose firmware reset under the
/// program -- a fault, or running out of memory -- sends no stop, and over a serial line its carrier
/// stays open. The question is asked at the run's own sequence number, and a board still running the
/// program answers that it is and carries on.
fn follow(
    transport: &mut impl Transport,
    seq: u16,
    target: &str,
    out: &mut dyn Write,
    err: &mut dyn Write,
    quiet: Duration,
) -> ExitCode {
    use lamella_wire_host::terminal::TerminalOutput;
    let mut run = RunCollector::new(seq);
    let mut terminal = TerminalOutput::new(out, err);
    let mut heard = Instant::now();
    let mut asked: Option<Instant> = None;
    loop {
        let mut printed = false;
        let polled = run.poll_streaming(transport, &mut |chunk| {
            printed = true;
            let _ = terminal.show(chunk, None);
        });
        if printed {
            heard = Instant::now();
        }
        match polled {
            Ok(true) => {
                let _ = terminal.end_open_line();
                let (out, err) = terminal.into_writers();
                return report_end(&run, out, err);
            }
            Ok(false) => {
                let now = Instant::now();
                let quiet_long = now.duration_since(heard) >= quiet;
                let asked_lately = asked.is_some_and(|at| now.duration_since(at) < quiet);
                if quiet_long && !asked_lately && transport.send(exec::EXEC_STATUS, seq, &[]).is_ok() {
                    asked = Some(now);
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                let _ = terminal.end_open_line();
                let (_, err) = terminal.into_writers();
                let _ = writeln!(
                    err,
                    "\nlamella run: the connection to {target} dropped while the program was running \
                     ({error:?}), so how it\nended is not known. A board's own USB leaves the bus when \
                     the board resets -- when its firmware\naborts, out of memory or on a fault, or \
                     when it loses power."
                );
                return ExitCode::FAILURE;
            }
        }
    }
}

/// The line a run's end gets, and the exit code it gives this tool.
///
/// The program's exit code is REPORTED rather than forwarded: a nonzero code would be
/// indistinguishable from the tool failing.
fn report_end(run: &RunCollector, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
    use lamella_wire_host::debug::reason;
    match (run.stop_reason(), run.exit_value()) {
        (None, _) if run.start_refusal() == Some(exec::ack::IDLE) => {
            let _ = writeln!(
                err,
                "\nthe program is no longer running on the board, and the board reported no stop: it \
                 says nothing is\nexecuting. A board that resets -- a firmware fault, or running out \
                 of memory -- ends a program\nthis way, with its output stopping where it ended."
            );
            ExitCode::FAILURE
        }
        (Some(reason::DONE), Some(code)) => {
            let _ = writeln!(out, "\nthe program ended, exit code {code}.");
            if code == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        (Some(reason::TRAP), _) => {
            let _ = writeln!(err, "\nthe program stopped: it trapped, as reported above.");
            ExitCode::FAILURE
        }
        (Some(reason::ABORTED), _) => {
            let _ = writeln!(err, "\nthe program stopped: something else connected to the board and aborted it.");
            ExitCode::FAILURE
        }
        (Some(other), _) => {
            let _ = writeln!(err, "\nthe program stopped, giving reason {other}, which this build has no word for.");
            ExitCode::FAILURE
        }
        (None, _) => {
            let _ = writeln!(err, "\nthe program stopped without saying why.");
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

/// `run --target`'s wording for a source it cannot compile.
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

    /// A carrier carrying the frames a board sent, which records what the host sends and answers
    /// an `EXEC_STATUS` with `status` -- or with nothing.
    struct Board {
        wire: lamella_wire::MemTransport,
        status: Option<u8>,
        asked: usize,
    }

    impl Board {
        fn sending(frames: &[(u8, u16, Vec<u8>)], status: Option<u8>) -> Self {
            let mut board = lamella_wire::MemTransport::new();
            for (msg_type, seq, payload) in frames {
                Transport::send(&mut board, *msg_type, *seq, payload).unwrap();
            }
            let mut wire = lamella_wire::MemTransport::new();
            wire.feed(&board.take_sent());
            Self { wire, status, asked: 0 }
        }
    }

    impl Transport for Board {
        fn send(&mut self, msg_type: u8, seq: u16, _payload: &[u8]) -> Result<(), lamella_wire::TransportError> {
            if msg_type == exec::EXEC_STATUS {
                self.asked += 1;
                if let Some(status) = self.status {
                    let mut board = lamella_wire::MemTransport::new();
                    Transport::send(&mut board, exec::EXEC_ACK, seq, &[status]).unwrap();
                    self.wire.feed(&board.take_sent());
                }
            }
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, lamella_wire::TransportError> {
            Transport::poll(&mut self.wire)
        }
    }

    /// An `EVT_OUTPUT` of `text` on standard output.
    fn printed(text: &str) -> (u8, u16, Vec<u8>) {
        let mut payload = vec![lamella_wire_host::debug::output::STDOUT, 0];
        payload.extend_from_slice(text.as_bytes());
        (lamella_wire_host::debug::EVT_OUTPUT, 0, payload)
    }

    /// The `EVT_STOPPED` a program that returned `exit` ends with, at `seq`.
    fn ended(seq: u16, exit: i32) -> (u8, u16, Vec<u8>) {
        let mut payload = vec![lamella_wire_host::debug::reason::DONE];
        payload.extend_from_slice(&[0; 8]);
        payload.extend_from_slice(&exit.to_le_bytes());
        payload.push(0);
        (lamella_wire_host::debug::EVT_STOPPED, seq, payload)
    }

    fn code(exit: ExitCode) -> String {
        format!("{exit:?}")
    }

    /// **A RUN IS FOLLOWED TO THE STOP AT ITS OWN SEQUENCE NUMBER**, and only that stop ends it: a
    /// stop the board sent for something else is not this program's end.
    #[test]
    fn a_run_is_followed_to_its_own_stop_and_its_exit_code_is_reported() {
        let mut board = Board::sending(&[printed("one\n"), ended(0, 9), printed("two\n"), ended(RUN_SEQ, 3)], None);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let exit = follow(&mut board, RUN_SEQ, "loopback", &mut out, &mut err, Duration::from_secs(60));
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code(exit), code(ExitCode::FAILURE), "exit 3 is reported, and is not success");
        assert!(out.contains("one\ntwo\n"), "both lines: {out}");
        assert!(out.ends_with("the program ended, exit code 3.\n"), "{out}");
        assert_eq!(board.asked, 0, "a program that keeps talking is not asked about");
    }

    /// **A STOP OUTRANKS THE ANSWER THAT FOLLOWS IT.** A board busy before the program -- joining its
    /// network -- answers "is it still running?" only after the program ends, right behind the stop,
    /// and the stop is how the program ended.
    #[test]
    fn a_stop_followed_by_an_idle_answer_is_reported_as_the_stop() {
        let idle = (exec::EXEC_ACK, RUN_SEQ, vec![exec::ack::IDLE]);
        let mut board = Board::sending(&[printed("hello\n"), ended(RUN_SEQ, 0), idle], None);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let exit = follow(&mut board, RUN_SEQ, "loopback", &mut out, &mut err, Duration::from_secs(60));
        let (out, err) = (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap());
        assert_eq!(code(exit), code(ExitCode::SUCCESS), "{out}{err}");
        assert!(out.ends_with("the program ended, exit code 0.\n"), "{out}");
        assert!(err.is_empty(), "{err}");
    }

    /// **A PROGRAM THE BOARD NO LONGER RUNS IS NOT WAITED ON FOREVER.** After a quiet spell the board
    /// is asked, and a board that says nothing is executing ends the run as one that went away.
    #[test]
    fn a_quiet_run_the_board_no_longer_executes_ends_as_gone() {
        let mut board = Board::sending(&[printed("started\n")], Some(exec::ack::IDLE));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let exit = follow(&mut board, RUN_SEQ, "loopback", &mut out, &mut err, Duration::from_millis(20));
        let (out, err) = (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap());
        assert_eq!(code(exit), code(ExitCode::FAILURE));
        assert!(out.contains("started\n"), "{out}");
        assert!(err.contains("no longer running on the board"), "{err}");
        assert_eq!(board.asked, 1, "asked once, and answered");
    }

    /// **A QUIET PROGRAM THE BOARD IS STILL RUNNING IS LEFT TO RUN**, asked about once per quiet
    /// spell rather than on every poll.
    #[test]
    fn a_quiet_run_the_board_still_executes_is_asked_about_once_per_spell() {
        let mut board = Board::sending(&[], Some(exec::ack::RUNNING));
        let quiet = Duration::from_millis(40);
        let exit = {
            let mut ending = Ending { board: &mut board, at: Instant::now() + quiet * 9, sent: false };
            let (mut out, mut err) = (Vec::new(), Vec::new());
            follow(&mut ending, RUN_SEQ, "loopback", &mut out, &mut err, quiet)
        };
        assert_eq!(code(exit), code(ExitCode::SUCCESS), "the run ended with exit 0");
        assert!((4..=10).contains(&board.asked), "asked once per quiet spell, not on every poll: {}", board.asked);
    }

    /// A [`Board`] that sends the run's ending stop once `at` has passed.
    struct Ending<'a> {
        board: &'a mut Board,
        at: Instant,
        sent: bool,
    }

    impl Transport for Ending<'_> {
        fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), lamella_wire::TransportError> {
            self.board.send(msg_type, seq, payload)
        }

        fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, lamella_wire::TransportError> {
            if !self.sent && Instant::now() >= self.at {
                self.sent = true;
                let (msg_type, seq, payload) = ended(RUN_SEQ, 0);
                let mut board = lamella_wire::MemTransport::new();
                Transport::send(&mut board, msg_type, seq, &payload).unwrap();
                self.board.wire.feed(&board.take_sent());
            }
            self.board.poll()
        }
    }

    /// **`run --target` AGAINST THE FIRMWARE's OWN SERVE, WITH THREADS.** The board takes the program
    /// through the deploy serve and runs it without a reset under the scheduler: a second thread, a
    /// lock both take and a join, which the debugger's one-thread session refuses. Every line it
    /// prints and its exit code reach this tool.
    #[cfg(feature = "bake")]
    #[test]
    fn a_program_that_starts_a_thread_runs_to_its_end_and_is_followed() {
        use crate::deploy::loopback::{LoopbackBoard, RamFlash};
        let Ok(compiler) = lamella_wire_host::engine::LcscCompiler::discover() else {
            return;
        };
        let image = crate::bake::compile_and_bake(
            &compiler,
            "using System;\nusing System.Threading;\n\npublic static class Program\n{\n    \
             static readonly object Gate = new object();\n    static int count;\n\n    \
             public static int Main()\n    {\n        Thread worker = new Thread(Work);\n        \
             worker.Start();\n        Count(\"main\");\n        worker.Join();\n        \
             Console.WriteLine(\"joined, count \" + count);\n        return count;\n    }\n\n    \
             static void Work()\n    {\n        Count(\"worker\");\n    }\n\n    \
             static void Count(string who)\n    {\n        for (int i = 0; i < 3; i++)\n        {\n            \
             lock (Gate)\n            {\n                count++;\n            }\n            \
             Console.WriteLine(who + \" \" + i);\n            Thread.Sleep(1);\n        }\n    }\n}\n",
        )
        .expect("a program bakes");
        let mut board = LoopbackBoard::on(RamFlash::holding(&[]));
        let caps = Capabilities(crate::deploy::host_caps().0 | Capabilities::EXEC_NO_RESET);
        let session = crate::deploy::hello_on(&mut board, 0, "run", "usb", caps).expect("the board answers a HELLO");
        assert!(session.caps.has(Capabilities::EXEC_NO_RESET), "the firmware's serve starts without a reset");

        let (mut out, mut err) = (Vec::new(), Vec::new());
        let exit = run_scheduled(&mut board, &session, "usb", &image, &mut out, &mut err, Duration::from_secs(60));
        let (out, err) = (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap());
        assert!(!board.reset, "the board did not reset to start it: {out}{err}");
        assert!(out.contains("running on usb; output follows."), "{out}{err}");
        for line in ["main 0", "main 2", "worker 0", "worker 2", "joined, count 6"] {
            assert!(out.contains(line), "{line}: {out}{err}");
        }
        assert!(out.ends_with("the program ended, exit code 6.\n"), "{out}{err}");
        assert_eq!(code(exit), code(ExitCode::FAILURE), "exit 6 is reported, and is not success");
        assert_eq!(
            lamella_cil_runtime::verified_image_checksum(board.flash.region),
            lamella_wire_host::baked_image_checksum(&image),
            "and the board stores the program it ran"
        );
    }
}
