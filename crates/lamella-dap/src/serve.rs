//! The serve loop: read framed DAP requests, dispatch them to a [`Debugger`], and
//! write the responses and events back.

use crate::adapter::Debugger;
use crate::protocol::{Message, read_message, write_message};
use std::io::{self, BufRead, Write};
use std::sync::mpsc::TryRecvError;
use std::thread;
use std::time::Duration;

/// Reads requests from `reader`, dispatches each to `debugger`, and writes the
/// resulting responses and events to `writer`, until a `disconnect` request or
/// the end of the stream.
///
/// However the session ends, the target is released: a `disconnect` request releases it, and any
/// other end -- the stream closing, or a frame that cannot be read or written -- releases it here,
/// through [`Debugger::release_target`], so a client that goes away does not leave it stopped.
///
/// # Errors
/// Returns an [`io::Error`] if reading a frame, parsing it, or writing a reply
/// fails.
pub fn serve<R: BufRead, W: Write>(
    debugger: &mut Debugger,
    reader: &mut R,
    writer: &mut W,
) -> io::Result<()> {
    let ended = serve_requests(debugger, reader, writer);
    release_unless_disconnected(debugger, ended)
}

fn serve_requests<R: BufRead, W: Write>(
    debugger: &mut Debugger,
    reader: &mut R,
    writer: &mut W,
) -> io::Result<Ended> {
    while let Some(message) = read_message(reader)? {
        let Message::Request(request) = message else {
            continue;
        };
        let disconnecting = request.command == "disconnect";
        for reply in debugger.handle(&request) {
            write_message(writer, &reply)?;
        }
        while debugger.is_running() {
            let polled = debugger.poll();
            if polled.is_empty() {
                break;
            }
            for reply in polled {
                write_message(writer, &reply)?;
            }
        }
        if disconnecting {
            return Ok(Ended::DisconnectRequest);
        }
    }
    Ok(Ended::StreamClosed)
}

/// Like [`serve`], but polls a free-running ("resume-now") backend concurrently with reading
/// client requests, so an asynchronous stop -- a breakpoint the device hits while running --
/// surfaces on its own, without waiting for the next request. A reader thread feeds requests
/// over a channel; the main loop blocks on it while the target is stopped, but while the
/// target runs it polls the backend (emitting any stop events) and only takes a request if
/// one is already waiting (so a pause still interrupts a run, and a breakpoint hit with no
/// pending request is never missed).
///
/// [`serve`] suffices for the synchronous interpreter (which never leaves the target
/// running); this is for the device backend.
///
/// The target is released however the session ends, as for [`serve`].
///
/// # Errors
/// Returns an [`io::Error`] if writing a reply fails.
pub fn serve_polled<R: BufRead + Send + 'static, W: Write>(
    debugger: &mut Debugger,
    reader: R,
    writer: &mut W,
) -> io::Result<()> {
    let ended = serve_requests_polled(debugger, reader, writer);
    release_unless_disconnected(debugger, ended)
}

fn serve_requests_polled<R: BufRead + Send + 'static, W: Write>(
    debugger: &mut Debugger,
    reader: R,
    writer: &mut W,
) -> io::Result<Ended> {
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let mut reader = reader;
        while let Ok(Some(message)) = read_message(&mut reader) {
            if tx.send(message).is_err() {
                break;
            }
        }
    });

    loop {
        let message = if debugger.is_running() {
            match rx.try_recv() {
                Ok(message) => message,
                Err(TryRecvError::Empty) => {
                    for reply in debugger.poll() {
                        write_message(writer, &reply)?;
                    }
                    if debugger.is_running() {
                        thread::sleep(Duration::from_millis(10));
                    }
                    continue;
                }
                Err(TryRecvError::Disconnected) => return Ok(Ended::StreamClosed),
            }
        } else {
            match rx.recv() {
                Ok(message) => message,
                Err(_) => return Ok(Ended::StreamClosed),
            }
        };

        let Message::Request(request) = message else {
            continue;
        };
        let disconnecting = request.command == "disconnect";
        for reply in debugger.handle(&request) {
            write_message(writer, &reply)?;
        }
        if disconnecting {
            return Ok(Ended::DisconnectRequest);
        }
    }
}

/// How a session's request loop ended.
enum Ended {
    /// The client sent `disconnect`, whose handler released the target.
    DisconnectRequest,
    /// The client's stream closed without one.
    StreamClosed,
}

/// Releases the target unless a `disconnect` request already did, and passes the loop's own outcome
/// on. Nobody is left to tell when this release fails -- the client is gone -- so the reason goes to
/// standard error.
fn release_unless_disconnected(
    debugger: &mut Debugger,
    ended: io::Result<Ended>,
) -> io::Result<()> {
    if !matches!(ended, Ok(Ended::DisconnectRequest)) {
        if let Err(reason) = debugger.release_target() {
            eprintln!(
                "the session ended without a disconnect, and the target could not be released: {reason}"
            );
        }
    }
    ended.map(|_| ())
}

#[cfg(all(test, feature = "interpreter"))]
mod tests {
    use super::*;
    use crate::protocol::{Message, Request};
    use lamella_cil::{Instruction, MethodBodyImage, Opcode, Operand};
    use lamella_token::Token;
    use lamella_cil_runtime::Module;
    use std::io::Cursor;

    fn program() -> (Module, u32) {
        let mut module = Module::new();
        let write_line = module.add_intrinsic(
            0,
            lamella_cil_runtime::intrinsics::console_write_line,
            lamella_cil_runtime::intrinsic_registry::intrinsic_id("console_write_line"),
            1,
        );
        module.bind_token(0, Token(0x0A00_0001), write_line);
        let hi: Vec<u16> = "hi".encode_utf16().collect();
        module.bind_string(0, Token(0x7000_0001), &hi);
        let main = module.add_method_image(
            0,
            MethodBodyImage {
                max_stack: 8,
                init_locals: true,
                local_var_sig: None,
                code: vec![
                    Instruction::new(Opcode::Ldstr, Operand::Token(Token(0x7000_0001))),
                    Instruction::new(Opcode::Call, Operand::Token(Token(0x0A00_0001))),
                    Instruction::simple(Opcode::Ret),
                ]
                .into_boxed_slice(),
                handlers: <Box<[lamella_cil::EhClause]>>::default(),
            },
            0,
        );
        (module, main)
    }

    fn request_frames(commands: &[&str]) -> Vec<u8> {
        let mut input = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            let message = Message::Request(Request {
                seq: index as i64 + 1,
                command: (*command).to_owned(),
                arguments: None,
            });
            write_message(&mut input, &message).unwrap();
        }
        input
    }

    fn read_all(bytes: Vec<u8>) -> Vec<Message> {
        let mut reader = Cursor::new(bytes);
        let mut messages = Vec::new();
        while let Some(message) = read_message(&mut reader).unwrap() {
            messages.push(message);
        }
        messages
    }

    #[test]
    fn serves_a_full_scripted_session() {
        let (module, main) = program();
        let mut debugger = Debugger::new(module, main);

        let input = request_frames(&["initialize", "launch", "continue", "disconnect"]);
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        serve(&mut debugger, &mut reader, &mut output).unwrap();

        let messages = read_all(output);
        let responses = messages
            .iter()
            .filter(|m| matches!(m, Message::Response(r) if r.success))
            .count();
        assert_eq!(responses, 4);
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "initialized"))
        );
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "terminated"))
        );
        assert_eq!(debugger.output_string(), "hi\n");
    }

    #[test]
    fn stops_at_a_clean_end_of_stream_without_disconnect() {
        let (module, main) = program();
        let mut debugger = Debugger::new(module, main);
        let input = request_frames(&["initialize"]);
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        serve(&mut debugger, &mut reader, &mut output).unwrap();
        assert!(!read_all(output).is_empty());
    }

    #[test]
    fn a_session_that_cannot_start_shows_the_user_why_when_it_is_launched() {
        let reason = "--probe stlink needs the `st` feature {and this server was built without it}";
        let mut debugger = Debugger::refusing(reason);
        let input = request_frames(&["initialize", "launch", "disconnect"]);
        let mut output = Vec::new();
        serve(&mut debugger, &mut Cursor::new(input), &mut output).unwrap();

        let messages = read_all(output);
        let response = |command: &str| {
            messages
                .iter()
                .find_map(|m| match m {
                    Message::Response(r) if r.command == command => Some(r),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no {command} response"))
        };
        assert!(response("initialize").success);
        let launch = response("launch");
        assert!(!launch.success);
        let error = &launch.body.as_ref().expect("a structured error")["error"];
        assert_eq!(error["showUser"], serde_json::json!(true));
        assert_eq!(error["format"], serde_json::json!("{reason}"));
        assert_eq!(error["variables"]["reason"], serde_json::json!(reason));
        assert!(response("disconnect").success);
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "initialized"))
        );
    }

    /// A backend that counts the times it is released, answers every release with `outcome`, and
    /// answers every launch with `launch`. Everything else is inert.
    struct Releasing {
        releases: std::rc::Rc<std::cell::Cell<u32>>,
        outcome: Result<(), String>,
        launch: Result<(), String>,
    }

    impl lamella_debug_backend::DebugBackend for Releasing {
        fn launch(&mut self) -> Result<(), String> {
            self.launch.clone()
        }
        fn resume(&mut self) -> lamella_debug_backend::Stop {
            lamella_debug_backend::Stop::Done
        }
        fn step(&mut self) -> lamella_debug_backend::Stop {
            lamella_debug_backend::Stop::Step
        }
        fn release(&mut self) -> Result<(), String> {
            self.releases.set(self.releases.get() + 1);
            self.outcome.clone()
        }
        fn depth(&self) -> usize {
            1
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn stack(&self) -> Vec<lamella_debug_backend::Frame> {
            Vec::new()
        }
        fn variables(
            &self,
            _frame: usize,
            _scope: lamella_debug_backend::Scope,
        ) -> Vec<lamella_debug_backend::Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<lamella_debug_backend::Register> {
            Vec::new()
        }
        fn disassemble(
            &self,
            _address: u64,
            _offset: i64,
            _count: usize,
        ) -> Vec<lamella_debug_backend::Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    fn releasing(outcome: Result<(), String>) -> (Debugger, std::rc::Rc<std::cell::Cell<u32>>) {
        let releases = std::rc::Rc::new(std::cell::Cell::new(0));
        let backend = Releasing {
            releases: std::rc::Rc::clone(&releases),
            outcome,
            launch: Ok(()),
        };
        (Debugger::with_backend(Box::new(backend)), releases)
    }

    #[test]
    fn a_launch_the_backend_cannot_start_shows_the_user_its_reason() {
        let reason = "the target did not start the run: {NOTHING_TO_RUN}";
        let backend = Releasing {
            releases: std::rc::Rc::new(std::cell::Cell::new(0)),
            outcome: Ok(()),
            launch: Err(reason.to_owned()),
        };
        let mut debugger = Debugger::with_backend(Box::new(backend));
        let input = request_frames(&["initialize", "launch", "disconnect"]);
        let mut output = Vec::new();
        serve(&mut debugger, &mut Cursor::new(input), &mut output).unwrap();

        let messages = read_all(output);
        let launch = messages
            .iter()
            .find_map(|m| match m {
                Message::Response(r) if r.command == "launch" => Some(r),
                _ => None,
            })
            .expect("a launch response");
        assert!(!launch.success);
        let error = &launch.body.as_ref().expect("a structured error")["error"];
        assert_eq!(error["showUser"], serde_json::json!(true));
        assert_eq!(error["format"], serde_json::json!("{reason}"));
        let shown = error["variables"]["reason"].as_str().expect("the reason travels as a variable");
        assert!(shown.contains(reason), "the backend's own words reach the user: {shown}");
        assert!(
            !messages
                .iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "initialized"))
        );
    }

    #[test]
    fn a_disconnect_releases_the_target_once() {
        let (mut debugger, releases) = releasing(Ok(()));
        let input = request_frames(&["initialize", "launch", "disconnect"]);
        let mut output = Vec::new();
        serve(&mut debugger, &mut Cursor::new(input), &mut output).unwrap();
        assert_eq!(releases.get(), 1);
        assert!(
            read_all(output).iter().any(
                |m| matches!(m, Message::Response(r) if r.command == "disconnect" && r.success)
            )
        );
    }

    #[test]
    fn a_client_that_goes_away_without_a_disconnect_still_releases_the_target() {
        let (mut debugger, releases) = releasing(Ok(()));
        let input = request_frames(&["initialize", "launch"]);
        serve(&mut debugger, &mut Cursor::new(input), &mut Vec::new()).unwrap();
        assert_eq!(releases.get(), 1, "serve");

        let (mut debugger, releases) = releasing(Ok(()));
        let input = request_frames(&["initialize", "launch"]);
        serve_polled(&mut debugger, Cursor::new(input), &mut Vec::new()).unwrap();
        assert_eq!(releases.get(), 1, "serve_polled");
    }

    #[test]
    fn a_target_that_cannot_be_released_is_reported_to_the_user() {
        let (mut debugger, _) = releasing(Err("the probe stopped answering".to_owned()));
        let input = request_frames(&["initialize", "launch", "disconnect"]);
        let mut output = Vec::new();
        serve(&mut debugger, &mut Cursor::new(input), &mut output).unwrap();
        let messages = read_all(output);
        let disconnect = messages
            .iter()
            .find_map(|m| match m {
                Message::Response(r) if r.command == "disconnect" => Some(r),
                _ => None,
            })
            .expect("a disconnect response");
        assert!(!disconnect.success);
        let error = &disconnect.body.as_ref().expect("a structured error")["error"];
        assert_eq!(error["showUser"], serde_json::json!(true));
        let reason = error["variables"]["reason"].as_str().expect("the reason");
        assert!(reason.contains("the probe stopped answering"), "{reason}");
    }

    /// A FREE-RUNNING TARGET, WHICH IS THE SHAPE THE OTHER TEST TARGETS IN THIS MODULE ARE NOT. Its `resume` returns
    /// `Running` and records that it was asked; the stop arrives later, from `poll`, as it does on a
    /// device where the core runs on after the probe lets it go. The synchronous fakes finish inside
    /// `resume`, so nothing here covered the sequence a board actually takes.
    struct FreeRunning {
        /// Times `resume` was asked for, which is what says a run was started at all.
        resumes: std::rc::Rc<std::cell::Cell<u32>>,
        /// Polls remaining before the target reports its stop.
        polls_before_stopping: std::cell::Cell<u32>,
        /// Set once the stop has been reported, so a client thread can know it may disconnect.
        stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl lamella_debug_backend::DebugBackend for FreeRunning {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> lamella_debug_backend::Stop {
            self.resumes.set(self.resumes.get() + 1);
            lamella_debug_backend::Stop::Running
        }
        fn poll(&mut self) -> lamella_debug_backend::Stop {
            let left = self.polls_before_stopping.get();
            if left == 0 {
                self.stopped
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                return lamella_debug_backend::Stop::Breakpoint;
            }
            self.polls_before_stopping.set(left - 1);
            lamella_debug_backend::Stop::Running
        }
        fn step(&mut self) -> lamella_debug_backend::Stop {
            lamella_debug_backend::Stop::Step
        }
        fn depth(&self) -> usize {
            1
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn stack(&self) -> Vec<lamella_debug_backend::Frame> {
            Vec::new()
        }
        fn variables(
            &self,
            _frame: usize,
            _scope: lamella_debug_backend::Scope,
        ) -> Vec<lamella_debug_backend::Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<lamella_debug_backend::Register> {
            Vec::new()
        }
        fn disassemble(
            &self,
            _address: u64,
            _offset: i64,
            _count: usize,
        ) -> Vec<lamella_debug_backend::Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    fn free_running(
        polls: u32,
    ) -> (
        Debugger,
        std::rc::Rc<std::cell::Cell<u32>>,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        let resumes = std::rc::Rc::new(std::cell::Cell::new(0));
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let backend = FreeRunning {
            resumes: std::rc::Rc::clone(&resumes),
            polls_before_stopping: std::cell::Cell::new(polls),
            stopped: std::sync::Arc::clone(&stopped),
        };
        (Debugger::with_backend(Box::new(backend)), resumes, stopped)
    }

    /// A CLIENT WHOSE STREAM STAYS OPEN WHILE THE TARGET RUNS, which is the condition the polled loop
    /// is written for and the one a scripted `Cursor` cannot express: at EOF that loop ends without a
    /// last poll -- correctly, since the client it would tell has gone.
    ///
    /// It hands over `opening`, then holds the stream open until `stopped` says the stop has been
    /// reported, then hands over `closing` and ends.
    struct ClientStream {
        opening: std::io::Cursor<Vec<u8>>,
        closing: std::io::Cursor<Vec<u8>>,
        stopped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl std::io::Read for ClientStream {
        fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
            let read = self.opening.read(out)?;
            if read > 0 {
                return Ok(read);
            }
            while !self.stopped.load(std::sync::atomic::Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.closing.read(out)
        }
    }

    /// A launch request carrying `arguments`, which the scripted helper above cannot express.
    fn request_with(seq: i64, command: &str, arguments: serde_json::Value) -> Vec<u8> {
        let mut out = Vec::new();
        write_message(
            &mut out,
            &Message::Request(Request {
                seq,
                command: command.to_owned(),
                arguments: Some(arguments),
            }),
        )
        .unwrap();
        out
    }

    /// `configurationDone` STARTS THE PROGRAM on a free-running target.
    ///
    /// There is no stop to report at launch: DAP's `stopped` says "the execution of the debuggee has
    /// stopped", and with `stopOnEntry` absent nothing has. What starts the program is
    /// `configurationDone`, and what says so is the backend being asked to resume -- which every
    /// test target in this module but this one finishes inside, so the sequence a board takes needs
    /// the free-running one to be covered at all.
    #[test]
    fn configuration_done_starts_a_free_running_target() {
        let (mut debugger, resumes, _) = free_running(3);
        let mut input = request_frames(&["initialize", "launch"]);
        input.extend(request_frames(&["configurationDone"]).iter().copied());
        serve_polled(&mut debugger, Cursor::new(input), &mut Vec::new()).unwrap();

        assert_eq!(
            resumes.get(),
            1,
            "configurationDone must start the program: a client that sends no continue is a client \
             whose board never runs"
        );
    }

    /// A STOP THE TARGET REACHES AFTER THE RESUME IS REPORTED, which is the other half of what a
    /// client needs and the half a breakpoint depends on: the program is started by
    /// `configurationDone` and halts later, on its own, with no request outstanding.
    ///
    /// Driven through [`serve_polled`] against a client that keeps its stream open, because that is
    /// the loop `device-dap-server` runs and the only one that can report this.
    #[test]
    fn a_stop_reached_while_running_is_reported_to_the_client() {
        let (mut debugger, resumes, stopped) = free_running(3);
        let client = ClientStream {
            opening: Cursor::new(request_frames(&[
                "initialize",
                "launch",
                "configurationDone",
            ])),
            closing: Cursor::new(request_frames(&["disconnect"])),
            stopped,
        };
        let mut output = Vec::new();
        serve_polled(&mut debugger, std::io::BufReader::new(client), &mut output).unwrap();

        assert_eq!(resumes.get(), 1, "the program was started");
        let messages = read_all(output);
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "stopped")),
            "the stop the target reached while running must reach the client: {messages:?}"
        );
    }

    /// With `stopOnEntry`, the program is NOT started, and the client is told it is stopped.
    ///
    /// The other half of the same contract: here there IS something to report at launch, because the
    /// core is held where the deploy left it, and `stopped` is how a client learns it may set up and
    /// then continue.
    #[test]
    fn stop_on_entry_reports_a_stop_and_starts_nothing() {
        let (mut debugger, resumes, _) = free_running(0);
        let mut input = request_frames(&["initialize"]);
        input.extend(
            request_with(2, "launch", serde_json::json!({ "stopOnEntry": true }))
                .iter()
                .copied(),
        );
        input.extend(request_frames(&["configurationDone"]).iter().copied());
        let mut output = Vec::new();
        serve_polled(&mut debugger, Cursor::new(input), &mut output).unwrap();

        assert_eq!(resumes.get(), 0, "stopOnEntry runs nothing");
        let messages = read_all(output);
        let entry = messages.iter().any(|m| match m {
            Message::Event(event) if event.event == "stopped" => {
                event
                    .body
                    .as_ref()
                    .and_then(|body| body.get("reason"))
                    .and_then(serde_json::Value::as_str)
                    == Some("entry")
            }
            _ => false,
        });
        assert!(
            entry,
            "a held core is reported stopped at entry: {messages:?}"
        );
    }
}
