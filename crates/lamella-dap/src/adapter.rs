//! The debug adapter: translates DAP requests into calls on a [`DebugBackend`] and
//! produces the responses and events DAP expects.

#[cfg(feature = "interpreter")]
use crate::interp_backend::InterpreterBackend;
use crate::frame_eval;
use crate::protocol::{Event, Message, Request, Response};
use lamella_debug_backend::{
    ChildReference, DebugBackend, ExceptionFilter, ExceptionInfo, Scope, Stop, Variable,
};
#[cfg(feature = "interpreter")]
use lamella_cil_runtime::Module;
use serde_json::{Value as Json, json};

#[cfg(feature = "interpreter")]
pub use crate::interp_backend::{decode_address, encode_address};

/// A debug session: the target behind the [`DebugBackend`] seam plus the adapter's
/// own DAP bookkeeping.
pub struct Debugger {
    backend: Box<dyn DebugBackend>,
    /// All console output forwarded to the client so far (also exposed for tests).
    output: String,
    /// Whether `launch` has started the target (so `pause` can report a stop).
    launched: bool,
    /// Whether `launch` asked to stop at the entry point instead of running.
    stop_on_entry: bool,
    /// Why this session cannot start, when the server that built it found out before it had a
    /// target to debug. `launch` answers with it instead of starting anything; see
    /// [`Debugger::refusing`].
    refusal: Option<String>,
    /// What names this session's target, for a session started without one
    /// ([`Debugger::choosing`]): asked at `launch` or `attach`, and gone once it has named one.
    chooser: Option<Chooser>,
    /// The entry stop the chooser decided, for the `launch` it answered. `None` where the session
    /// was given its target up front, and reads the request itself.
    chosen_stop_at_entry: Option<bool>,
    /// Told the outcome of every `launch` and `attach`, where the host asked to be
    /// ([`Debugger::reporting_starts`]).
    start_report: Option<StartReport>,
    /// Whether the program runs without debugging (`noDebug`): no breakpoint, exception filter or
    /// entry stop takes effect.
    no_debug: bool,
    /// Whether the backend left the target running, so the serve loop polls for the stop to come: a
    /// device after a resume, or a program a step left running.
    running: bool,
    /// The step command under way while a single step of it left the target running -- its thread
    /// waits on another, or for good -- so the poll that brings that single step's end carries the
    /// command on ([`Debugger::carry_step`]).
    stepping: Option<Stepping>,
    /// The threads frame ids have been given for, by slot. A frame's id is its thread's slot times
    /// [`FRAMES_PER_THREAD`] plus its place in the stack, so thread 1, always in slot 0, keeps the ids
    /// its frames had before the program's other threads were shown.
    frame_threads: Vec<u32>,
    /// Source and instruction breakpoints, kept apart so each `setBreakpoints` /
    /// `setInstructionBreakpoints` updates only its own kind. The backend has a single
    /// breakpoint set, so the *union* is what gets programmed -- otherwise an empty
    /// `setInstructionBreakpoints` (which VS Code sends right after `setBreakpoints`) would
    /// wipe the source breakpoints.
    source_breakpoints: Vec<u64>,
    instruction_breakpoints: Vec<u64>,
    /// What the client was last told of each instruction breakpoint that names an address, in the
    /// order of `instruction_breakpoints`: so a change in whether it is armed can be told by its id.
    instruction_answers: Vec<Json>,
    /// Function breakpoints, by the addresses their names resolved to, kept apart from the source and
    /// instruction breakpoints so that each kind's request replaces only that kind.
    function_breakpoints: Vec<u64>,
    /// What the client was last told of each function breakpoint that resolved, with how many of
    /// `function_breakpoints` it takes, in their order.
    function_answers: Vec<(usize, Json)>,
    /// The exception the target is stopped on, which `exceptionInfo` describes; cleared once the
    /// target runs again.
    stopped_exception: Option<ExceptionInfo>,
    /// Whether the backend has ended the program ([`DebugBackend::terminate`]), so the session has
    /// no hold left on the target to release.
    terminated: bool,
    /// Per-source-breakpoint hit-count / logpoint behavior, by code address. Rebuilt on each
    /// `setBreakpoints`; an address with no entry is an ordinary breakpoint that always stops.
    breakpoint_meta: Vec<BreakpointMeta>,
    /// Configuration a client sent before the session had a target: each request as it was sent,
    /// with the ids its pending answers carry. It is set on the target a launch starts
    /// ([`Debugger::arm_at_start`]), and a `breakpoint` event for each id tells the client how.
    early: Vec<(Request, Vec<i64>)>,
    /// The id the next pending answer to early configuration carries.
    next_breakpoint_id: i64,
    /// The inactive (over-capacity) breakpoint count last reported to the user, so the
    /// run-time "N inactive" note fires only when that count changes -- not on every continue.
    last_inactive_note: usize,
    /// How the target's source paths map to the client's: a request's path is mapped before the
    /// backend resolves it, and every path the backend gives is mapped back before the client sees it.
    path_map: PathMap,
    /// Whether the session has already said in the console that the frame it was asked about
    /// reports no variables, so the note is made once rather than on every hover.
    noted_absent_variables: bool,
    out_seq: i64,
    /// The Debug Console REPL, built on the first `evaluate` so a session that never uses the
    /// console pays nothing. See [`crate::repl_eval`].
    #[cfg(feature = "interpreter")]
    repl: Option<crate::repl_eval::ReplCell>,
}

/// What a session started without a target ([`Debugger::choosing`]) asks when its `launch` or
/// `attach` arrives. Given the request's command and arguments, it answers with the target they
/// name, or with why there is none, in words the client shows the user.
pub type Chooser = Box<dyn FnMut(&str, &Json) -> Result<Chosen, String>>;

/// What a session tells its host about each `launch` or `attach` it answers: `Ok` once the target has
/// started, or the reason it did not, as the client is told it. See [`Debugger::reporting_starts`].
pub type StartReport = Box<dyn FnMut(Result<(), &str>)>;

/// The target a [`Chooser`] named, and how the session it starts runs.
pub struct Chosen {
    /// The backend the session drives from here on.
    pub backend: Box<dyn DebugBackend>,
    /// Whether the program stops at its entry, rather than running to its first breakpoint.
    pub stop_at_entry: bool,
    /// Whether the program runs without debugging, as a client's "Run Without Debugging" asks: no
    /// breakpoint, exception filter or entry stop takes effect.
    pub no_debug: bool,
    /// Lines for the debug console before the program starts: whatever the launch asked for that
    /// the chooser could not take as asked, such as a field it does not know.
    pub notes: Vec<String>,
    /// How the source paths the program's build recorded map to the ones on this computer.
    pub path_map: PathMap,
}

/// How the source paths a program's build recorded map to the ones on this computer, where the build
/// rewrote them, as MSBuild's `PathMap` does: pairs of a prefix on this computer and the prefix the
/// program records in its place, tried in order.
///
/// Prefixes compare as the debugger compares paths, ignoring case and which separator a path uses.
/// A path no pair's prefix starts passes unchanged.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathMap {
    pairs: Vec<(String, String)>,
}

impl PathMap {
    /// A map of `pairs`, each a prefix on this computer and the prefix the program records for it.
    #[must_use]
    pub fn new(pairs: Vec<(String, String)>) -> PathMap {
        PathMap { pairs }
    }

    /// `path`, a source on this computer, as the program records it.
    #[must_use]
    pub fn to_program(&self, path: &str) -> String {
        self.pairs
            .iter()
            .find_map(|(local, program)| remapped(path, local, program))
            .unwrap_or_else(|| path.to_owned())
    }

    /// `path`, a source as the program records it, as it is on this computer.
    #[must_use]
    pub fn to_local(&self, path: &str) -> String {
        self.pairs
            .iter()
            .find_map(|(local, program)| remapped(path, program, local))
            .unwrap_or_else(|| path.to_owned())
    }
}

/// `path` with its prefix `from` replaced by `to`, the rest written with `to`'s separator, or `None`
/// where `from` does not start it.
fn remapped(path: &str, from: &str, to: &str) -> Option<String> {
    let compared = |text: &str| text.replace('\\', "/").to_ascii_lowercase();
    if from.is_empty() || !compared(path).starts_with(&compared(from)) {
        return None;
    }
    let separator = if to.contains('\\') { '\\' } else { '/' };
    let rest: String = path[from.len()..]
        .chars()
        .map(|character| if matches!(character, '\\' | '/') { separator } else { character })
        .collect();
    Some(format!("{to}{rest}"))
}

/// Per-breakpoint behavior for a source breakpoint: a hit-count condition and/or a logpoint
/// message, with the running hit count. Both fields empty means an ordinary breakpoint that
/// always stops.
struct BreakpointMeta {
    address: u64,
    /// The source a `setBreakpoints` named this breakpoint in, as the client spelled its path: the
    /// next `setBreakpoints` for that source replaces it, and one for any other source keeps it.
    document: String,
    hit_condition: Option<String>,
    log_message: Option<String>,
    hits: u32,
    /// What the client was last told of this breakpoint, its id included, so a later change in
    /// whether it is armed can be told by that id.
    answer: Json,
}

/// The request whose own breakpoints were just answered: a source's, or a kind with no source.
#[derive(Clone, Copy)]
enum Settled<'a> {
    Source(&'a str),
    Instruction,
    Function,
}

/// What to do when execution stops at a source breakpoint, decided from its [`BreakpointMeta`].
enum BreakpointAction {
    /// Report the stop to the client: a plain breakpoint, or a hit count now satisfied.
    Stop,
    /// Log the message and keep running -- a logpoint never stops.
    Log(String),
    /// Keep running without reporting -- a hit count not yet satisfied.
    Skip,
}

/// Interprets a DAP `hitCondition` against the running hit count: an optional operator
/// (`>`, `>=`, `<`, `<=`, `==`, `%`) then a number; a bare number means `==` (break on exactly
/// that hit), `%n` breaks every nth hit. An unparseable condition stops always (fail safe).
fn hit_satisfied(condition: &str, hits: u32) -> bool {
    let condition = condition.trim();
    let (op, number) = if let Some(rest) = condition.strip_prefix(">=") {
        (">=", rest)
    } else if let Some(rest) = condition.strip_prefix("<=") {
        ("<=", rest)
    } else if let Some(rest) = condition.strip_prefix("==") {
        ("==", rest)
    } else if let Some(rest) = condition.strip_prefix('>') {
        (">", rest)
    } else if let Some(rest) = condition.strip_prefix('<') {
        ("<", rest)
    } else if let Some(rest) = condition.strip_prefix('%') {
        ("%", rest)
    } else {
        ("==", condition)
    };
    let Ok(n) = number.trim().parse::<u32>() else {
        return true;
    };
    match op {
        ">=" => hits >= n,
        "<=" => hits <= n,
        ">" => hits > n,
        "<" => hits < n,
        "%" => n != 0 && hits % n == 0,
        _ => hits == n,
    }
}

impl Debugger {
    /// Creates a debugger over the interpreter, owning `module`, entered at `entry`.
    #[cfg(feature = "interpreter")]
    #[must_use]
    pub fn new(module: Module, entry: u32) -> Debugger {
        Debugger::with_backend(Box::new(InterpreterBackend::new(module, entry)))
    }

    /// Creates a debugger over the interpreter with source mapping from a standalone
    /// Portable PDB (`pdb_bytes`): source breakpoints and source-located stack frames.
    #[cfg(feature = "interpreter")]
    #[must_use]
    pub fn with_source(module: Module, entry: u32, pdb_bytes: Vec<u8>) -> Debugger {
        Debugger::with_backend(Box::new(InterpreterBackend::with_pdb(
            module, entry, pdb_bytes,
        )))
    }

    /// Creates a debugger over any [`DebugBackend`] -- the interpreter, or an on-device
    /// target. This is the seam an on-device adapter constructs.
    #[must_use]
    pub fn with_backend(backend: Box<dyn DebugBackend>) -> Debugger {
        Debugger {
            backend,
            output: String::new(),
            launched: false,
            stop_on_entry: false,
            refusal: None,
            chooser: None,
            chosen_stop_at_entry: None,
            start_report: None,
            no_debug: false,
            running: false,
            stepping: None,
            frame_threads: vec![lamella_debug_backend::MAIN_THREAD],
            source_breakpoints: Vec::new(),
            instruction_breakpoints: Vec::new(),
            instruction_answers: Vec::new(),
            function_answers: Vec::new(),
            function_breakpoints: Vec::new(),
            stopped_exception: None,
            terminated: false,
            breakpoint_meta: Vec::new(),
            early: Vec::new(),
            next_breakpoint_id: 1,
            last_inactive_note: 0,
            path_map: PathMap::default(),
            noted_absent_variables: false,
            out_seq: 0,
            #[cfg(feature = "interpreter")]
            repl: None,
        }
    }

    /// Creates a debugger that cannot start, and tells the client why.
    ///
    /// `initialize` is answered as usual, `launch` fails with `reason` marked for the client to show
    /// the user, and `disconnect` ends the session. This is for a server that found a problem before
    /// it had a target to debug -- a probe its build does not include, an argument it cannot honor --
    /// and must still answer the client that started it: a server that exits instead leaves the client
    /// nothing to report except that it exited.
    #[must_use]
    pub fn refusing(reason: impl Into<String>) -> Debugger {
        let mut debugger = Debugger::with_backend(Box::new(Unstartable));
        debugger.refusal = Some(reason.into());
        debugger
    }

    /// Creates a debugger with no target until its `launch` or `attach` names one: `chooser` is
    /// asked then, with the request's arguments, and the backend it answers with is the one the
    /// session drives.
    ///
    /// `initialize` is answered before any target is known, so it offers only what every target
    /// can do. Once the target is chosen, a `capabilities` event offers the rest, ahead of the
    /// `initialized` event that starts the client's configuration.
    #[must_use]
    pub fn choosing(chooser: Chooser) -> Debugger {
        let mut debugger = Debugger::with_backend(Box::new(Unstartable));
        debugger.chooser = Some(chooser);
        debugger
    }

    /// This session, mapping the target's source paths to the client's with `path_map`, for a
    /// session given its target up front. A chosen target brings its own (`Chosen::path_map`).
    #[must_use]
    pub fn mapping_paths(mut self, path_map: PathMap) -> Debugger {
        self.path_map = path_map;
        self
    }

    /// This session, telling `report` the outcome of every `launch` and `attach` it answers: `Ok`
    /// once the target has started, or the reason it did not -- a refusal, a chooser's answer that
    /// names no target, or a target that was chosen and then did not start.
    ///
    /// **A TARGET IS STARTED WHEN ITS START SUCCEEDS, NOT WHEN IT IS CHOSEN.** A host that counted the
    /// chooser's answers would count a board that was named and never written, and would end a
    /// session that started nothing with the status of one that ran.
    #[must_use]
    pub fn reporting_starts(mut self, report: StartReport) -> Debugger {
        self.start_report = Some(report);
        self
    }

    /// All console output the program has produced and the adapter has forwarded.
    #[must_use]
    pub fn output_string(&self) -> &str {
        &self.output
    }

    /// Whether the backend left the target running, so the serve loop should
    /// [`poll`](Debugger::poll) for the eventual stop: a device after a resume, an interpreter whose
    /// host polls it, or a program that runs on with nothing left to stop it but a pause -- every
    /// thread asleep for good, or a step whose thread waits for good.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Polls a target the backend left running: the output + stop events once it stops, or empty
    /// while it is still going. The serve loop calls this while [`is_running`](Debugger::is_running)
    /// holds; the backend's `poll` may block until the target halts, so this does not busy-wait.
    ///
    /// A stop at a breakpoint is taken through its hit count and its logpoint first, as a stop a
    /// command comes to is: a logpoint writes its message and the target runs on, and a hit count
    /// not yet met resumes it without a stop being reported.
    ///
    /// A step command whose single step left the target running goes on from that single step's
    /// end, so the step lands where it would have had nothing waited.
    #[must_use]
    pub fn poll(&mut self) -> Vec<Message> {
        let mut events: Vec<(&'static str, Option<Json>)> = Vec::new();
        if self.running {
            match self.backend.poll() {
                Stop::Running => self.flush_output(&mut events),
                stop => {
                    self.running = false;
                    let stop = match (stop, self.stepping.take()) {
                        (Stop::Step, Some(stepping)) => self.carry_step(stepping, &mut events),
                        (stop, _) => stop,
                    };
                    self.settle(stop, &mut events);
                }
            }
        }
        events
            .into_iter()
            .map(|(event, body)| self.event(event, body))
            .collect()
    }

    /// Hands the target back as the session ends, so the program goes on without a debugger. A
    /// `disconnect` request does this itself; the serve loops call it when a session ends any other
    /// way -- the client's stream closing, or a frame that cannot be read or written. See
    /// [`DebugBackend::release`].
    ///
    /// A target the session terminated is not released: ending the program ended the session's hold
    /// on it.
    ///
    /// # Errors
    /// The backend's reason, when the target could not be released and may still be stopped.
    pub fn release_target(&mut self) -> Result<(), String> {
        self.running = false;
        self.stepping = None;
        if self.terminated {
            return Ok(());
        }
        self.backend.release()
    }

    /// Ends the program through the backend, and with it the session's hold on the target.
    fn end_program(&mut self) -> Result<(), String> {
        self.running = false;
        self.stepping = None;
        let ended = self.backend.terminate();
        if ended.is_ok() {
            self.terminated = true;
            self.launched = false;
            self.stopped_exception = None;
        }
        ended
    }

    /// Handles one DAP request, returning the response followed by any events.
    pub fn handle(&mut self, request: &Request) -> Vec<Message> {
        let mut events: Vec<(&str, Option<Json>)> = Vec::new();
        let (success, body) = match request.command.as_str() {
            "initialize" => (true, Some(capabilities(self.backend.as_ref()))),
            "launch" | "attach" => {
                if let Some(reason) = self.refusal.clone() {
                    self.report_start(Err(&reason));
                    return self.fail_for_user(request, ERROR_SESSION_REFUSED, &reason);
                }
                let attaching = request.command == "attach";
                let mut unchosen: Option<(Chooser, Box<dyn DebugBackend>)> = None;
                if let Some(mut chooser) = self.chooser.take() {
                    let arguments = request.arguments.clone().unwrap_or(Json::Null);
                    match chooser(&request.command, &arguments) {
                        Ok(chosen) => {
                            let Chosen { backend, stop_at_entry, no_debug, notes, path_map } = chosen;
                            self.path_map = path_map;
                            for note in notes {
                                events.push((
                                    "output",
                                    Some(json!({ "category": "console", "output": format!("{note}\n") })),
                                ));
                            }
                            let before = std::mem::replace(&mut self.backend, backend);
                            unchosen = Some((chooser, before));
                            self.no_debug = no_debug;
                            self.chosen_stop_at_entry = Some(stop_at_entry);
                        }
                        Err(reason) => {
                            self.chooser = Some(chooser);
                            self.report_start(Err(&reason));
                            let (id, failed) = start_failure(attaching);
                            return self.fail_for_user(request, id, &format!("{failed}: {reason}"));
                        }
                    }
                }
                let started = self.start(request, attaching);
                self.flush_output(&mut events);
                if let Err(reason) = started {
                    if let Some((chooser, before)) = unchosen {
                        self.unchoose(chooser, before, &mut events);
                    }
                    self.report_start(Err(&reason));
                    let mut out: Vec<Message> =
                        events.into_iter().map(|(event, body)| self.event(event, body)).collect();
                    let (id, failed) = start_failure(attaching);
                    out.extend(self.fail_for_user(request, id, &format!("{failed}: {reason}")));
                    return out;
                }
                let chose = unchosen.is_some();
                if chose {
                    events.push((
                        "capabilities",
                        Some(json!({ "capabilities": capabilities(self.backend.as_ref()) })),
                    ));
                }
                self.arm_at_start(chose, &mut events);
                self.report_start(Ok(()));
                events.push(("initialized", None));
                (true, None)
            }
            "restart" => {
                if !self.backend.can_restart() {
                    return self.fail(
                        request,
                        "This target cannot restart its program within a session. End the session \
                         and start a new one.",
                    );
                }
                if let Err(reason) = self.backend.restart() {
                    return self.fail_for_user(
                        request,
                        ERROR_RESTART_FAILED,
                        &format!("The program could not be restarted: {reason}"),
                    );
                }
                for meta in &mut self.breakpoint_meta {
                    meta.hits = 0;
                }
                self.stopped_exception = None;
                self.terminated = false;
                self.launched = true;
                self.stepping = None;
                if self.stop_on_entry {
                    events.push(("stopped", Some(stopped("entry", self.backend.selected_thread()))));
                } else {
                    self.run(Action::Resume, &mut events);
                }
                (true, None)
            }
            "terminate" => {
                if !self.backend.can_terminate() {
                    return self.fail(
                        request,
                        "This target cannot end its program from the debugger. Disconnecting \
                         leaves the program running.",
                    );
                }
                if let Err(reason) = self.end_program() {
                    return self.fail_for_user(
                        request,
                        ERROR_TERMINATE_FAILED,
                        &format!("The program could not be ended: {reason}"),
                    );
                }
                events.push(("terminated", None));
                (true, None)
            }
            "setBreakpoints" | "setInstructionBreakpoints" | "setFunctionBreakpoints" | "setExceptionBreakpoints"
                if self.chooser.is_some() =>
            {
                (true, Some(self.keep_until_start(request)))
            }
            "setBreakpoints" | "setInstructionBreakpoints" | "setFunctionBreakpoints" if self.no_debug => {
                (true, Some(unarmed_without_debugging(request, "breakpoints")))
            }
            "setExceptionBreakpoints" if self.no_debug => {
                (true, Some(unarmed_without_debugging(request, "filters")))
            }
            "setExceptionBreakpoints" => (true, Some(self.set_exception_breakpoints(request))),
            "exceptionInfo" => match &self.stopped_exception {
                Some(info) => (true, Some(exception_info_body(info))),
                None => return self.fail(request, "The program is not stopped on an exception."),
            },
            "setFunctionBreakpoints" => (
                true,
                Some(self.set_function_breakpoints(request, None, &mut events)),
            ),
            "configurationDone" => {
                if self.stop_on_entry {
                    events.push(("stopped", Some(stopped("entry", self.backend.selected_thread()))));
                } else {
                    self.run(Action::Resume, &mut events);
                }
                (true, None)
            }
            "threads" => {
                let threads: Vec<Json> = self
                    .backend
                    .threads()
                    .iter()
                    .map(|thread| json!({ "id": thread.id, "name": thread.name }))
                    .collect();
                (true, Some(json!({ "threads": threads })))
            }
            "stackTrace" => (true, Some(self.stack_trace(arg_opt_u32(request, "threadId")))),
            "scopes" => (true, Some(self.scopes(arg_u32(request, "frameId")))),
            "variables" => (
                true,
                Some(self.variables(arg_u32(request, "variablesReference"))),
            ),
            "setVariable" => match self.set_variable(request) {
                Some(value) => (true, Some(json!({ "value": value, "variablesReference": 0 }))),
                None => {
                    let name = arg_str(request, "name");
                    return self.fail(request, &format!("cannot set {name}"));
                }
            },
            "continue" => {
                self.run(Action::Resume, &mut events);
                (true, Some(json!({ "allThreadsContinued": true })))
            }
            "stepIn" => {
                self.select_requested_thread(request);
                self.run(Action::StepIn, &mut events);
                (true, None)
            }
            "next" => {
                self.select_requested_thread(request);
                self.run(Action::StepOver, &mut events);
                (true, None)
            }
            "stepOut" => {
                self.select_requested_thread(request);
                self.run(Action::StepOut, &mut events);
                (true, None)
            }
            "pause" => {
                if self.running {
                    self.backend.pause();
                    self.running = false;
                }
                self.stepping = None;
                if self.launched {
                    events.push(("stopped", Some(stopped("pause", self.backend.selected_thread()))));
                }
                (true, None)
            }
            "setBreakpoints" => (
                true,
                Some(self.set_source_breakpoints(request, None, &mut events)),
            ),
            "setInstructionBreakpoints" => (
                true,
                Some(self.set_instruction_breakpoints(request, None, &mut events)),
            ),
            "disassemble" => (true, Some(self.disassemble(request))),
            "evaluate" => return self.evaluate(request),
            "disconnect" => {
                let terminate = arg_bool(request, "terminateDebuggee")
                    && self.backend.can_terminate()
                    && !self.terminated;
                let ended = if terminate { self.end_program() } else { self.release_target() };
                match ended {
                    Ok(()) => (true, None),
                    Err(reason) if terminate => {
                        return self.fail_for_user(
                            request,
                            ERROR_TERMINATE_FAILED,
                            &format!(
                                "The session has ended, but the program could not be ended and \
                                 may still be running or stopped: {reason}"
                            ),
                        );
                    }
                    Err(reason) => {
                        return self.fail_for_user(
                            request,
                            ERROR_TARGET_NOT_RELEASED,
                            &format!(
                                "The session has ended, but the target could not be released and \
                                 may still be stopped: {reason}"
                            ),
                        );
                    }
                }
            }
            _ => (false, None),
        };

        let mut out = Vec::with_capacity(1 + events.len());
        out.push(self.response(request, success, body));
        for (event, body) in events {
            out.push(self.event(event, body));
        }
        out
    }

    /// Starts the session's target, launched at its entry point or attached to where it already runs,
    /// and records whether `launch` asked to stop at the entry.
    fn start(&mut self, request: &Request, attaching: bool) -> Result<(), String> {
        let (stop_at_entry, no_debug) = match self.chosen_stop_at_entry.take() {
            Some(stop_at_entry) => (stop_at_entry, self.no_debug),
            None => (arg_bool(request, "stopOnEntry"), arg_bool(request, "noDebug")),
        };
        let started = if attaching {
            self.backend.attach()
        } else {
            self.backend.launch()
        };
        if started.is_ok() {
            self.no_debug = no_debug;
            self.stop_on_entry = !attaching && stop_at_entry && !no_debug;
        }
        self.launched = started.is_ok();
        started
    }

    /// Puts a session back as it was before its chooser named a target that then did not start: the
    /// chosen target handed back and dropped, `before` its target again, and `chooser` kept to answer
    /// the next `launch`.
    fn unchoose(
        &mut self,
        chooser: Chooser,
        before: Box<dyn DebugBackend>,
        events: &mut Vec<(&'static str, Option<Json>)>,
    ) {
        if let Err(reason) = self.backend.release() {
            events.push((
                "output",
                Some(json!({
                    "category": "console",
                    "output": format!("The target could not be handed back and may still be stopped: {reason}\n"),
                })),
            ));
        }
        self.backend = before;
        self.chooser = Some(chooser);
        self.no_debug = false;
        self.chosen_stop_at_entry = None;
        self.stop_on_entry = false;
    }

    /// Settles the breakpoints and exception filters set before the session's target started, once it
    /// has: a session that runs without debugging disarms every one of them, and a target the launch
    /// has just chosen is given the ones set while the session had no target -- the configuration kept
    /// for it ([`Debugger::keep_until_start`]) set as though it had just been sent, each pending answer
    /// settled by a `breakpoint` event with its id.
    ///
    /// **A CLIENT MAY CONFIGURE BEFORE IT LAUNCHES.** A breakpoint or filter armed then would stop a
    /// program the user asked to run without stopping; and one set before the launch named a target
    /// was answered against no target and armed on none, so without this it never reaches the target
    /// it was set for.
    fn arm_at_start(&mut self, chose: bool, events: &mut Vec<(&'static str, Option<Json>)>) {
        let failed = if self.no_debug {
            for (_, ids) in std::mem::take(&mut self.early) {
                for id in ids {
                    let mut unarmed = unverified(REASON_FAILED, String::from(WITHOUT_DEBUGGING));
                    unarmed["id"] = json!(id);
                    events.push(("breakpoint", Some(json!({ "reason": "changed", "breakpoint": unarmed }))));
                }
            }
            self.source_breakpoints.clear();
            self.instruction_breakpoints.clear();
            self.instruction_answers.clear();
            self.function_breakpoints.clear();
            self.function_answers.clear();
            self.breakpoint_meta.clear();
            let disarmed = self
                .backend
                .set_breakpoints(&[])
                .and_then(|()| self.backend.set_exception_filters(&[]));
            disarmed.err().map(|reason| {
                format!(
                    "The breakpoints set before the launch could not be removed from the target, so \
                     the program may still stop at one: {reason}"
                )
            })
        } else if chose {
            self.set_early(events);
            self.apply_breakpoints().err().map(|reason| {
                format!("The breakpoints set before the launch could not be set on the target: {reason}")
            })
        } else {
            None
        };
        if let Some(said) = failed {
            events.push((
                "output",
                Some(json!({ "category": "console", "output": format!("{said}\n") })),
            ));
        }
    }

    /// Keeps `request`, configuration sent before the session had a target, to set on the target a
    /// launch starts, and answers each thing it asks for as pending, with an id. A later request of
    /// the same kind -- for `setBreakpoints`, for the same source -- replaces it, as it would replace
    /// what a target had been given.
    fn keep_until_start(&mut self, request: &Request) -> Json {
        let list = if request.command == "setExceptionBreakpoints" {
            "filters"
        } else {
            "breakpoints"
        };
        let asked = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get(list))
            .and_then(Json::as_array)
            .map_or(0, Vec::len);
        let source = |request: &Request| {
            request
                .arguments
                .as_ref()
                .and_then(|arguments| arguments.get("source"))
                .and_then(|source| source.get("path"))
                .and_then(Json::as_str)
                .map(str::to_owned)
        };
        let path = source(request);
        self.early
            .retain(|(kept, _)| kept.command != request.command || source(kept) != path);
        let mut ids = Vec::with_capacity(asked);
        let mut answers = Vec::with_capacity(asked);
        for _ in 0..asked {
            let id = self.next_id();
            let mut answer = unverified(
                REASON_PENDING,
                String::from(
                    "The session has not started its program, so this cannot be resolved yet. It is \
                     set when the program starts.",
                ),
            );
            answer["id"] = json!(id);
            ids.push(id);
            answers.push(answer);
        }
        self.early.push((request.clone(), ids));
        json!({ "breakpoints": answers })
    }

    /// Sets the configuration kept before the session had a target on the target a launch has just
    /// started, and tells the client how each pending answer was settled: a `breakpoint` event for
    /// each, by its id, with what the request would have been answered had it been sent now.
    fn set_early(&mut self, events: &mut Vec<(&'static str, Option<Json>)>) {
        let mut early = std::mem::take(&mut self.early);
        early.sort_by_key(|(request, _)| match request.command.as_str() {
            "setBreakpoints" => 0,
            "setInstructionBreakpoints" => 1,
            "setFunctionBreakpoints" => 2,
            _ => 3,
        });
        for (request, ids) in early {
            let body = match request.command.as_str() {
                "setBreakpoints" => self.set_source_breakpoints(&request, Some(&ids), events),
                "setInstructionBreakpoints" => {
                    self.set_instruction_breakpoints(&request, Some(&ids), events)
                }
                "setFunctionBreakpoints" => {
                    self.set_function_breakpoints(&request, Some(&ids), events)
                }
                _ => self.set_exception_breakpoints(&request),
            };
            let answers = body["breakpoints"].as_array().cloned().unwrap_or_default();
            for (id, mut breakpoint) in ids.into_iter().zip(answers) {
                breakpoint["id"] = json!(id);
                events.push(("breakpoint", Some(json!({ "reason": "changed", "breakpoint": breakpoint }))));
            }
        }
    }

    /// Tells the host the outcome of a `launch` or `attach`, where it asked to be told
    /// ([`Debugger::reporting_starts`]).
    fn report_start(&mut self, outcome: Result<(), &str>) {
        if let Some(report) = self.start_report.as_mut() {
            report(outcome);
        }
    }

    /// Programs the backend with the union of source, instruction and function breakpoints. They
    /// share the backend's single comparator set, so every kind must be re-applied together;
    /// applying only one would erase the others (VS Code sends an empty
    /// `setInstructionBreakpoints` right after `setBreakpoints`, which otherwise wipes them).
    fn apply_breakpoints(&mut self) -> Result<(), String> {
        let mut all = self.source_breakpoints.clone();
        all.extend_from_slice(&self.instruction_breakpoints);
        all.extend_from_slice(&self.function_breakpoints);
        self.backend.set_breakpoints(&all)
    }

    /// Replaces the armed exception filters with the ones a `setExceptionBreakpoints` names, and
    /// answers each with whether it is armed, in the order they were named, which is how DAP pairs
    /// the answers with the request.
    fn set_exception_breakpoints(&mut self, request: &Request) -> Json {
        let named: Vec<String> = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("filters"))
            .and_then(Json::as_array)
            .map(|ids| ids.iter().filter_map(Json::as_str).map(String::from).collect())
            .unwrap_or_default();
        let offered = self.backend.exception_filters();
        let mut armed: Vec<ExceptionFilter> = Vec::new();
        let mut results = Vec::with_capacity(named.len());
        for id in &named {
            match filter_named(id).filter(|filter| offered.contains(filter)) {
                Some(filter) => {
                    if !armed.contains(&filter) {
                        armed.push(filter);
                    }
                    results.push(json!({ "verified": true }));
                }
                None => results.push(unverified(
                    REASON_FAILED,
                    format!("This target offers no exception filter named {id}."),
                )),
            }
        }
        if let Err(reason) = self.backend.set_exception_filters(&armed) {
            withdraw_verification(&mut results, &reason);
        }
        json!({ "breakpoints": results })
    }

    /// Replaces the function breakpoints. Each names a method, and the backend resolves the name to
    /// the first instruction of every method it matches; a name that matches none is reported
    /// unverified. `ids` are the ids a pending answer already gave them; otherwise each answer is given
    /// a new one.
    fn set_function_breakpoints(
        &mut self,
        request: &Request,
        ids: Option<&[i64]>,
        events: &mut Vec<(&'static str, Option<Json>)>,
    ) -> Json {
        let specs = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("breakpoints"))
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        let cap = self.backend.max_breakpoints();
        let ahead = self.source_breakpoints.len() + self.instruction_breakpoints.len();
        let resolves = self.backend.can_break_on_functions();
        let mut addresses: Vec<u64> = Vec::new();
        let mut results = Vec::with_capacity(specs.len());
        let mut kept: Vec<(usize, usize)> = Vec::new();
        for (index, spec) in specs.iter().enumerate() {
            let id = answer_id(ids, index).unwrap_or_else(|| self.next_id());
            if !resolves {
                results.push(with_id(
                    unverified(
                        REASON_FAILED,
                        String::from(
                            "This target cannot set a breakpoint on a method by its name. Set one on \
                             a line of the method instead.",
                        ),
                    ),
                    id,
                ));
                continue;
            }
            let name = spec.get("name").and_then(Json::as_str).unwrap_or_default().trim();
            let bound = if name.is_empty() {
                Vec::new()
            } else {
                self.backend.resolve_function_breakpoint(name)
            };
            let Some(&first) = bound.first() else {
                results.push(with_id(
                    unverified(
                        REASON_FAILED,
                        format!("No method named {name} was found, so no breakpoint can be set on it."),
                    ),
                    id,
                ));
                continue;
            };
            let armed = cap.map_or(true, |max| ahead + addresses.len() + bound.len() <= max);
            addresses.extend_from_slice(&bound);
            let mut breakpoint = json!({ "id": id, "instructionReference": first.to_string() });
            if let Some(location) = self.backend.source_location(first) {
                breakpoint["line"] = json!(location.line);
                breakpoint["source"] = json!({ "path": self.path_map.to_local(&location.file) });
            }
            set_verified(&mut breakpoint, armed, cap);
            kept.push((results.len(), bound.len()));
            results.push(breakpoint);
        }
        self.function_breakpoints = addresses;
        let applied = self.apply_breakpoints();
        if let Err(reason) = &applied {
            withdraw_verification(&mut results, reason);
        }
        self.function_answers = kept
            .into_iter()
            .map(|(index, takes)| (takes, results[index].clone()))
            .collect();
        if applied.is_ok() {
            self.restate(Settled::Function, events);
        }
        json!({ "breakpoints": results })
    }

    /// Replaces the instruction breakpoints. Each is an opaque code address carried in
    /// the `instructionReference` (the backend interprets it); no source mapping needed. `ids` are
    /// the ids a pending answer already gave them; otherwise each answer is given a new one.
    fn set_instruction_breakpoints(
        &mut self,
        request: &Request,
        ids: Option<&[i64]>,
        events: &mut Vec<(&'static str, Option<Json>)>,
    ) -> Json {
        let specs = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("breakpoints"))
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        let cap = self.backend.max_breakpoints();
        let source_count = self.source_breakpoints.len();
        let mut addresses = Vec::new();
        let mut results = Vec::new();
        let mut kept = Vec::new();
        for (index, spec) in specs.iter().enumerate() {
            let id = answer_id(ids, index).unwrap_or_else(|| self.next_id());
            let address = spec
                .get("instructionReference")
                .and_then(Json::as_str)
                .and_then(|reference| reference.parse::<u64>().ok());
            let offset = spec.get("offset").and_then(Json::as_i64).unwrap_or(0);
            match address {
                Some(base) => {
                    let address = base.wrapping_add(offset as u64);
                    let armed = cap.map_or(true, |max| source_count + addresses.len() < max);
                    addresses.push(address);
                    let mut breakpoint =
                        json!({ "id": id, "instructionReference": address.to_string() });
                    set_verified(&mut breakpoint, armed, cap);
                    kept.push(results.len());
                    results.push(breakpoint);
                }
                None => results.push(with_id(
                    unverified(
                        REASON_FAILED,
                        String::from(
                            "This breakpoint does not name a place in the program.",
                        ),
                    ),
                    id,
                )),
            }
        }
        self.instruction_breakpoints = addresses;
        let applied = self.apply_breakpoints();
        if let Err(reason) = &applied {
            withdraw_verification(&mut results, reason);
        }
        self.instruction_answers = kept.into_iter().map(|index| results[index].clone()).collect();
        if applied.is_ok() {
            self.restate(Settled::Instruction, events);
        }
        json!({ "breakpoints": results })
    }

    /// Replaces the source breakpoints of the request's `source`. Each is a `line` in it, resolved
    /// to a code address by the backend's source mapping; a line with no code resolves to nothing and
    /// is reported unverified. `ids` are the ids a pending answer already gave them; otherwise each
    /// answer is given a new one.
    ///
    /// **A BREAKPOINT KEEPS ITS PLACE.** Where the target holds fewer than are set, the ones set first
    /// hold its comparators: a breakpoint this request keeps stays where it was, one it adds goes
    /// after every other, and one it removes frees its place. Another breakpoint that gains or loses
    /// a comparator by this is told in a `breakpoint` event.
    fn set_source_breakpoints(
        &mut self,
        request: &Request,
        ids: Option<&[i64]>,
        events: &mut Vec<(&'static str, Option<Json>)>,
    ) -> Json {
        let arguments = request.arguments.as_ref();
        let document = arguments
            .and_then(|args| args.get("source"))
            .and_then(|source| source.get("path"))
            .and_then(Json::as_str)
            .unwrap_or_default();
        let specs = arguments
            .and_then(|args| args.get("breakpoints"))
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        let cap = self.backend.max_breakpoints();
        let recorded = self.path_map.to_program(document);
        let mut fresh: Vec<Option<(usize, BreakpointMeta)>> = Vec::new();
        let mut results = Vec::new();
        for (index, spec) in specs.iter().enumerate() {
            let id = answer_id(ids, index).unwrap_or_else(|| self.next_id());
            let requested = spec.get("line").and_then(Json::as_u64);
            let address = requested
                .and_then(|line| u32::try_from(line).ok())
                .and_then(|line| self.backend.resolve_source_breakpoint(&recorded, line));
            match address {
                Some(address) => {
                    fresh.push(Some((
                        results.len(),
                        BreakpointMeta {
                            address,
                            document: document.to_owned(),
                            hit_condition: spec
                                .get("hitCondition")
                                .and_then(Json::as_str)
                                .filter(|text| !text.trim().is_empty())
                                .map(String::from),
                            log_message: spec
                                .get("logMessage")
                                .and_then(Json::as_str)
                                .filter(|text| !text.is_empty())
                                .map(String::from),
                            hits: 0,
                            answer: Json::Null,
                        },
                    )));
                    let located = self.backend.source_location(address);
                    let (line, file) = match &located {
                        Some(location) => {
                            (u64::from(location.line), self.path_map.to_local(&location.file))
                        }
                        None => (requested.unwrap_or(0), document.to_owned()),
                    };
                    let mut breakpoint = json!({
                        "id": id,
                        "line": line,
                        "instructionReference": address.to_string(),
                    });
                    if !file.is_empty() {
                        breakpoint["source"] = json!({ "path": file });
                    }
                    results.push(breakpoint);
                }
                None if !self.backend.has_source() => results.push(with_id(
                    unverified(
                        REASON_PENDING,
                        String::from(
                            "No source mapping is loaded yet, so this line cannot be resolved. It \
                             binds once the program is running.",
                        ),
                    ),
                    id,
                )),
                None => results.push(with_id(
                    unverified(
                        REASON_FAILED,
                        match requested {
                            Some(line) => format!(
                                "There is no code on line {line} of this file, so no breakpoint can \
                                 be set there. Move it to a statement."
                            ),
                            None => String::from("This breakpoint names no line."),
                        },
                    ),
                    id,
                )),
            }
        }
        let mut placed: Vec<BreakpointMeta> = Vec::with_capacity(self.breakpoint_meta.len() + fresh.len());
        let mut answering: Vec<Option<usize>> = Vec::with_capacity(placed.capacity());
        for kept in std::mem::take(&mut self.breakpoint_meta) {
            if kept.document != document {
                placed.push(kept);
                answering.push(None);
                continue;
            }
            let renewed = fresh
                .iter_mut()
                .find(|slot| slot.as_ref().is_some_and(|(_, meta)| meta.address == kept.address))
                .and_then(Option::take);
            if let Some((index, meta)) = renewed {
                placed.push(meta);
                answering.push(Some(index));
            }
        }
        for (index, meta) in fresh.into_iter().flatten() {
            placed.push(meta);
            answering.push(Some(index));
        }
        for (position, index) in answering.iter().enumerate() {
            if let Some(index) = *index {
                set_verified(&mut results[index], cap.map_or(true, |max| position < max), cap);
            }
        }
        self.breakpoint_meta = placed;
        self.source_breakpoints = self.breakpoint_meta.iter().map(|kept| kept.address).collect();
        let applied = self.apply_breakpoints();
        if let Err(reason) = &applied {
            withdraw_verification(&mut results, reason);
        }
        for (meta, index) in self.breakpoint_meta.iter_mut().zip(&answering) {
            if let Some(index) = *index {
                meta.answer = results[index].clone();
            }
        }
        if applied.is_ok() {
            self.restate(Settled::Source(document), events);
        }
        json!({ "breakpoints": results })
    }

    /// A new id for a breakpoint's answer, which later events about it name it by.
    fn next_id(&mut self) -> i64 {
        let id = self.next_breakpoint_id;
        self.next_breakpoint_id += 1;
        id
    }

    /// Tells the client, in a `breakpoint` event, of each breakpoint it was answered for that has
    /// gained or lost one of the target's comparators since: the target holds the first
    /// `max_breakpoints` of the set `apply_breakpoints` gives it, in that order, so one request can
    /// move another's breakpoints past the limit or back within it. `settled` is the request just
    /// answered, whose own breakpoints are in its answer.
    fn restate(&mut self, settled: Settled<'_>, events: &mut Vec<(&'static str, Option<Json>)>) {
        let Some(max) = self.backend.max_breakpoints() else {
            return;
        };
        let mut tell = |answer: &mut Json, armed: bool| {
            if (answer["verified"] == json!(true)) != armed {
                set_verified(answer, armed, Some(max));
                events.push((
                    "breakpoint",
                    Some(json!({ "reason": "changed", "breakpoint": answer.clone() })),
                ));
            }
        };
        let mut position = 0;
        for meta in &mut self.breakpoint_meta {
            let armed = position < max;
            position += 1;
            if !matches!(settled, Settled::Source(document) if document == meta.document) {
                tell(&mut meta.answer, armed);
            }
        }
        for answer in &mut self.instruction_answers {
            let armed = position < max;
            position += 1;
            if !matches!(settled, Settled::Instruction) {
                tell(answer, armed);
            }
        }
        for (takes, answer) in &mut self.function_answers {
            let armed = position + *takes <= max;
            position += *takes;
            if !matches!(settled, Settled::Function) {
                tell(answer, armed);
            }
        }
    }

    /// Runs the target for one command and emits the resulting output + stop/terminate
    /// events.
    fn run(&mut self, action: Action, events: &mut Vec<(&'static str, Option<Json>)>) {
        self.stopped_exception = None;
        self.stepping = None;
        if matches!(action, Action::Resume) {
            self.note_inactive_breakpoints(events);
        }
        let stop = match action {
            Action::Resume => self.backend.resume(),
            Action::StepIn | Action::StepOver | Action::StepOut if self.backend.has_source() => {
                self.source_step(action, events)
            }
            Action::StepIn => self.backend.step(),
            Action::StepOver | Action::StepOut if !self.backend.tracks_depth() => {
                self.backend.step()
            }
            Action::StepOver | Action::StepOut => self.step_to_depth(action, events),
        };
        self.settle(stop, events);
    }

    /// Takes a stop through what its breakpoint asks for, then reports where the target is: at a
    /// logpoint the message is written and the target resumed, at a hit count not yet met the target
    /// is resumed with nothing reported, and any other stop, or a target left running, is reported as
    /// it is.
    ///
    /// Both [`Debugger::run`] and [`Debugger::poll`] come here. On a target that resumes at once, as a
    /// device does, every breakpoint stop arrives through `poll`, so a decision made only in `run`
    /// is never made for it.
    fn settle(&mut self, mut stop: Stop, events: &mut Vec<(&'static str, Option<Json>)>) {
        while matches!(stop, Stop::Breakpoint) {
            match self.breakpoint_action() {
                BreakpointAction::Stop => break,
                BreakpointAction::Log(message) => {
                    self.flush_output(events);
                    events.push((
                        "output",
                        Some(json!({ "category": "console", "output": format!("{message}\n") })),
                    ));
                    stop = self.backend.resume();
                }
                BreakpointAction::Skip => stop = self.backend.resume(),
            }
        }
        if matches!(stop, Stop::Running) {
            self.running = true;
            self.flush_output(events);
        } else {
            self.finish(stop, events);
        }
    }

    /// Decides what to do at the current breakpoint stop from its [`BreakpointMeta`]: counts the
    /// hit, then returns whether to log (logpoint), keep running (hit count not yet met), or stop.
    /// The current location is the innermost frame's address; an address with no metadata stops.
    fn breakpoint_action(&mut self) -> BreakpointAction {
        let Some(address) = self.backend.stack().first().map(|frame| frame.address) else {
            return BreakpointAction::Stop;
        };
        let Some(meta) = self
            .breakpoint_meta
            .iter_mut()
            .find(|entry| entry.address == address)
        else {
            return BreakpointAction::Stop;
        };
        meta.hits += 1;
        if let Some(message) = &meta.log_message {
            return BreakpointAction::Log(message.clone());
        }
        match &meta.hit_condition {
            Some(condition) if !hit_satisfied(condition, meta.hits) => BreakpointAction::Skip,
            _ => BreakpointAction::Stop,
        }
    }

    /// Emits a one-line console note when a run will leave breakpoints inactive -- more than
    /// the target can arm at once -- so a silently-dropped breakpoint never surprises the user
    /// mid-run. Throttled: fires only when the inactive count changes since the last note.
    fn note_inactive_breakpoints(&mut self, events: &mut Vec<(&'static str, Option<Json>)>) {
        let Some(max) = self.backend.max_breakpoints() else {
            return;
        };
        let total = self.source_breakpoints.len()
            + self.instruction_breakpoints.len()
            + self.function_breakpoints.len();
        let inactive = total.saturating_sub(max);
        if inactive == self.last_inactive_note {
            return;
        }
        self.last_inactive_note = inactive;
        if inactive > 0 {
            events.push((
                "output",
                Some(json!({
                    "category": "console",
                    "output": format!(
                        "{max} of {total} breakpoints active: this target has {max} hardware \
                         breakpoints; {inactive} inactive (shown grayed).\n"
                    ),
                })),
            ));
        }
    }

    /// Single-steps until `reached(current_depth, start_depth)` -- `next` stops once
    /// back at or above the start depth (a stepped-over call has returned), `stepOut`
    /// once below it (the current method has returned).
    ///
    /// Bounded by [`DebugBackend::step_budget`], because a callee that does not return leaves the
    /// depth condition permanently unmet.
    fn step_to_depth(&mut self, action: Action, events: &mut Vec<(&'static str, Option<Json>)>) -> Stop {
        let stepping = Stepping {
            action,
            by_source: false,
            start: self.backend.depth(),
            budget: self.backend.step_budget().max(1),
        };
        self.step_on(stepping, events)
    }

    /// Takes the next single step of a step command, and carries the command on from it
    /// ([`Debugger::carry_step`]).
    ///
    /// **A SINGLE STEP THAT LEAVES THE TARGET RUNNING KEEPS THE COMMAND**, for the poll that brings its
    /// end: on a target with threads, a step's thread can wait for another thread, or for good, and
    /// until it has run its instruction the program runs on, as it does on .NET.
    fn step_on(&mut self, stepping: Stepping, events: &mut Vec<(&'static str, Option<Json>)>) -> Stop {
        match self.backend.step() {
            Stop::Step => self.carry_step(stepping, events),
            Stop::Running => {
                self.stepping = Some(stepping);
                Stop::Running
            }
            other => other,
        }
    }

    /// Carries a step command on from a single step that has just completed: whether the command is
    /// done there, and if not, the next single step, until it is done, stops for another reason, or
    /// leaves the target running.
    fn carry_step(&mut self, mut stepping: Stepping, events: &mut Vec<(&'static str, Option<Json>)>) -> Stop {
        loop {
            if let Some(stop) = self.after_step(&mut stepping, events) {
                return stop;
            }
            match self.backend.step() {
                Stop::Step => {}
                Stop::Running => {
                    self.stepping = Some(stepping);
                    return Stop::Running;
                }
                other => return other,
            }
        }
    }

    /// What a step command does after one of its single steps has completed: `Some` stop once the
    /// command is done, or ends for another reason; `None` to take another single step.
    fn after_step(
        &mut self,
        stepping: &mut Stepping,
        events: &mut Vec<(&'static str, Option<Json>)>,
    ) -> Option<Stop> {
        let Stepping { action, by_source, start, .. } = *stepping;
        let waiting_for = if by_source {
            if matches!(action, Action::StepOver | Action::StepOut) && self.backend.depth() > start {
                match self.backend.run_to_return() {
                    Stop::Step => {}
                    other => return Some(other),
                }
            }
            if matches!(action, Action::StepIn)
                && self.backend.depth() > start
                && !self.innermost_frame_has_source()
            {
                match self.backend.run_to_return() {
                    Stop::Step => {}
                    other => return Some(other),
                }
            }
            if stepping_reached(action, self.backend.depth(), start) && self.backend.at_source_boundary() {
                return Some(Stop::Step);
            }
            "no source statement was reached"
        } else {
            if stepping_reached(action, self.backend.depth(), start) {
                return Some(Stop::Step);
            }
            "the call being stepped over has not returned"
        };
        stepping.budget -= 1;
        if stepping.budget == 0 {
            return Some(self.give_up_stepping(waiting_for, events));
        }
        None
    }

    /// Ends a step that ran out of budget: says so on the console and parks the target where it
    /// is, rather than stepping on or reporting a fault.
    ///
    /// Stopping is the honest outcome and a fault is not -- nothing has gone wrong with the
    /// target or the connection, the step simply had no reachable end. The message says which
    /// condition was being waited on, because the two have different remedies.
    fn give_up_stepping(
        &mut self,
        waiting_for: &str,
        events: &mut Vec<(&'static str, Option<Json>)>,
    ) -> Stop {
        let budget = self.backend.step_budget();
        self.flush_output(events);
        events.push((
            "output",
            Some(json!({
                "category": "console",
                "output": format!(
                    "The step gave up after {budget} instructions: {waiting_for}. The program is most \
                     likely in code that has no source lines, or past its end. It stopped where it \
                     is: Continue, or set a breakpoint where you want to stop.\n"
                ),
            })),
        ));
        Stop::Step
    }

    /// Single-steps to the next source statement (sequence point) at the call depth the
    /// step implies: `stepIn` stops at the next boundary anywhere (descending into a
    /// call), `next` at the next boundary in this frame or a caller (running a called
    /// method to completion), `stepOut` at the next boundary after the current method
    /// returns. Used when the backend has source info; otherwise stepping is per-CIL-op.
    ///
    /// Bounded by [`DebugBackend::step_budget`]. `has_source` is a property of the whole image
    /// and `at_source_boundary` a property of one pc, so a step that leaves the covered region
    /// satisfies the first and never the second -- the loop condition can be unreachable while
    /// the target is perfectly healthy.
    ///
    /// **A step in does not stop in code no source covers** (Just My Code): a call into the class
    /// library, or anything else [`DebugBackend::source_location`] cannot place, runs to its return
    /// through [`DebugBackend::run_to_return`], and the step lands on the next source statement after
    /// it. So a callback from that code into the program's own -- an override the library calls --
    /// runs through rather than stopping; a breakpoint set inside it still stops it. A backend without
    /// a `run_to_return` of its own single-steps through, as before.
    fn source_step(
        &mut self,
        action: Action,
        events: &mut Vec<(&'static str, Option<Json>)>,
    ) -> Stop {
        if matches!(action, Action::StepOut) {
            if let Some(stop) = self.backend.step_out() {
                return stop;
            }
        }
        let stepping = Stepping {
            action,
            by_source: true,
            start: self.backend.depth(),
            budget: self.backend.step_budget().max(1),
        };
        self.step_on(stepping, events)
    }

    /// Whether the frame execution is in right now is one a source file covers.
    fn innermost_frame_has_source(&self) -> bool {
        self.backend
            .stack()
            .first()
            .is_some_and(|frame| self.backend.frame_source_location(0, frame.address).is_some())
    }

    /// Flushes new program output as an `output` event, then emits the event for the
    /// stop -- so console output precedes the stop or terminate it accompanies.
    fn finish(&mut self, stop: Stop, events: &mut Vec<(&'static str, Option<Json>)>) {
        self.flush_output(events);
        let thread = self.backend.selected_thread();
        match stop {
            Stop::Breakpoint => events.push(("stopped", Some(stopped("breakpoint", thread)))),
            Stop::Step => events.push(("stopped", Some(stopped("step", thread)))),
            Stop::Exception(info) => {
                events.push(("stopped", Some(stopped_on_exception(&info, thread))));
                self.stopped_exception = Some(info);
            }
            Stop::Done => {
                let exit_code = self.backend.exit_code();
                events.push(("exited", Some(json!({ "exitCode": exit_code }))));
                events.push(("terminated", None));
            }
            Stop::Running => {}
            Stop::Fault(message) => {
                events.push((
                    "output",
                    Some(json!({ "category": "stderr", "output": format!("{message}\n") })),
                ));
                let exit_code = self.backend.fault_exit_code().unwrap_or(-1);
                events.push(("exited", Some(json!({ "exitCode": exit_code }))));
                events.push(("terminated", None));
            }
        }
    }

    /// Forwards any new program output as an `output` event (so console output precedes
    /// the stop or terminate it accompanies).
    fn flush_output(&mut self, events: &mut Vec<(&'static str, Option<Json>)>) {
        if let Some(text) = self.backend.take_output() {
            self.output.push_str(&text);
            events.push((
                "output",
                Some(json!({ "category": "stdout", "output": text })),
            ));
        }
        if let Some(text) = self.backend.take_debug_output() {
            events.push((
                "output",
                Some(json!({ "category": "console", "output": text })),
            ));
        }
    }

    /// The call stack of `thread`, or of the selected thread when the request names none.
    fn stack_trace(&mut self, thread: Option<u32>) -> Json {
        let thread = thread.unwrap_or_else(|| self.backend.selected_thread());
        let Some(frames) = self.on_thread(thread, |debugger| {
            let frames = debugger.backend.stack();
            frames
                .into_iter()
                .enumerate()
                .map(|(index, frame)| {
                    let source = debugger.backend.frame_source_location(index, frame.address);
                    (frame, source)
                })
                .collect::<Vec<_>>()
        }) else {
            return json!({ "stackFrames": [], "totalFrames": 0 });
        };
        let mut out = Vec::with_capacity(frames.len());
        for (index, (frame, source)) in frames.iter().enumerate() {
            let Some(id) = self.frame_id(thread, index) else {
                break;
            };
            let mut entry = json!({
                "id": id,
                "name": frame.name,
                "line": source.as_ref().map_or(0, |location| location.line),
                "column": source.as_ref().map_or(0, |location| location.column),
                "instructionPointerReference": frame.address.to_string(),
            });
            if let Some(location) = &source {
                entry["source"] = json!({ "path": self.path_map.to_local(&location.file) });
                entry["endLine"] = json!(location.end_line);
                entry["endColumn"] = json!(location.end_column);
            }
            out.push(entry);
        }
        let total = out.len();
        json!({ "stackFrames": out, "totalFrames": total })
    }

    /// The id the client names frame `index` of `thread` by, or `None` past [`FRAMES_PER_THREAD`].
    /// A thread is given its slot the first time one of its frames is named.
    fn frame_id(&mut self, thread: u32, index: usize) -> Option<u32> {
        if index >= FRAMES_PER_THREAD as usize {
            return None;
        }
        let slot = match self.frame_threads.iter().position(|&known| known == thread) {
            Some(slot) => slot,
            None => {
                self.frame_threads.push(thread);
                self.frame_threads.len() - 1
            }
        };
        u32::try_from(slot)
            .ok()?
            .checked_mul(FRAMES_PER_THREAD)?
            .checked_add(index as u32)
            .filter(|&id| id < MAX_FRAME_ID)
    }

    /// The thread and the stack position a frame id names ([`Debugger::frame_id`]). An id no stack
    /// trace gave names a frame of thread 1, as every id did before the program's other threads were
    /// shown.
    fn frame_of(&self, id: u32) -> (u32, usize) {
        let thread = self
            .frame_threads
            .get((id / FRAMES_PER_THREAD) as usize)
            .copied()
            .unwrap_or(lamella_debug_backend::MAIN_THREAD);
        (thread, (id % FRAMES_PER_THREAD) as usize)
    }

    /// Runs `read` with `thread` selected, then selects the thread that was selected before, so that
    /// reading another thread's frames leaves the stop's own thread the one a step moves. `None` when
    /// the program has no such thread.
    fn on_thread<T>(&mut self, thread: u32, read: impl FnOnce(&mut Self) -> T) -> Option<T> {
        let before = self.backend.selected_thread();
        if thread == before {
            return Some(read(self));
        }
        if !self.backend.select_thread(thread) {
            return None;
        }
        let value = read(self);
        self.backend.select_thread(before);
        Some(value)
    }

    /// Selects the thread a stepping request names, when it names one the program has: the thread a
    /// step moves. A request naming none leaves the thread of the last stop selected.
    fn select_requested_thread(&mut self, request: &Request) {
        if let Some(thread) = arg_opt_u32(request, "threadId") {
            self.backend.select_thread(thread);
        }
    }

    fn scopes(&self, frame_id: u32) -> Json {
        let reference = |kind: u32| frame_id * 3 + kind + 1;
        json!({ "scopes": [
            { "name": "Arguments", "variablesReference": reference(0), "expensive": false },
            { "name": "Locals",    "variablesReference": reference(1), "expensive": false },
            { "name": "Stack",     "variablesReference": reference(2), "expensive": false },
        ]})
    }

    /// Answers `variables` for either kind of reference this adapter hands out: a frame's scope, or
    /// a value's members at [`CHILDREN_BASE`] and above.
    fn variables(&mut self, reference: u32) -> Json {
        if reference == 0 {
            return json!({ "variables": [] });
        }
        let rows = if reference >= CHILDREN_BASE {
            self.backend.children(ChildReference(reference - CHILDREN_BASE))
        } else {
            let (thread, frame_index) = self.frame_of((reference - 1) / 3);
            let scope = match (reference - 1) % 3 {
                0 => Scope::Arguments,
                1 => Scope::Locals,
                _ => Scope::Stack,
            };
            self.on_thread(thread, |debugger| {
                debugger.backend.variables_with_children(frame_index, scope)
            })
            .unwrap_or_default()
        };
        let variables: Vec<Json> = rows
            .iter()
            .map(|(variable, children)| {
                json!({
                    "name": variable.name,
                    "value": variable.value,
                    "type": variable.kind,
                    "variablesReference": dap_reference(*children),
                })
            })
            .collect();
        json!({ "variables": variables })
    }

    /// Answers a DAP `evaluate`: a hover, a Watch row, or a Debug Console line.
    ///
    /// A submission scoped to a frame is read out of that frame's own variables; an unscoped
    /// console line goes to the debugger's evaluation session. See [`crate::frame_eval`] for why
    /// neither is ever substituted for the other.
    fn evaluate(&mut self, request: &Request) -> Vec<Message> {
        let expression = arg_str(request, "expression").trim().to_owned();
        let frame_id = arg_opt_u32(request, "frameId");
        match frame_eval::route(frame_id, arg_str(request, "context")) {
            frame_eval::Target::Frame { index, room } => {
                let (thread, index) = match frame_id {
                    Some(id) => self.frame_of(id),
                    None => (self.backend.selected_thread(), index),
                };
                let (visible, references): (Vec<Variable>, Vec<ChildReference>) = self
                    .on_thread(thread, |debugger| debugger.frame_variables(index))
                    .unwrap_or_default()
                    .into_iter()
                    .unzip();
                match frame_eval::resolve(&visible, &expression) {
                    Ok(variable) => {
                        let children = visible
                            .iter()
                            .position(|row| core::ptr::eq(row, variable))
                            .and_then(|at| references.get(at).copied())
                            .unwrap_or(ChildReference::NONE);
                        let body = json!({
                            "result": variable.value,
                            "type": variable.kind,
                            "variablesReference": dap_reference(children),
                        });
                        vec![self.response(request, true, Some(body))]
                    }
                    Err(refusal) => {
                        let message = refusal.message(&expression, room);
                        let mut out = self.fail(request, &message);
                        if let Some(note) = self.absent_variables_note(&refusal) {
                            let body = json!({ "category": "console", "output": note });
                            out.push(self.event("output", Some(body)));
                        }
                        out
                    }
                }
            }
            frame_eval::Target::Session => self.session_evaluate(request, &expression),
        }
    }

    /// The one console note a session makes when the frame it was asked about reports no
    /// variables at all, or `None` once it has been made or for any other refusal.
    ///
    /// A refused `evaluate` is how a hover stays quiet over a word that is not a variable, and
    /// that is the right behaviour -- but it is also what an editor does when the target can name
    /// no variables at all, so on such a target hovering a REAL variable is silent too. Silence at
    /// the point of use is indistinguishable from a debugger that is not working, and the reason
    /// only reaches someone who thinks to open a Watch row. This says it once, unprompted, where a
    /// person can read it, and then stays out of the way.
    fn absent_variables_note(&mut self, refusal: &frame_eval::Refusal) -> Option<String> {
        if !matches!(refusal, frame_eval::Refusal::NoVariables) || self.noted_absent_variables {
            return None;
        }
        self.noted_absent_variables = true;
        Some(String::from(
            "No value was read from the paused frame: the target reported no arguments and no \
             locals for it. Hovering a variable shows nothing while that holds, and a Watch row \
             gives the reason in its value.\n",
        ))
    }

    /// The variables a name is read from in frame `index`: its locals, then its arguments.
    ///
    /// Locals come first as the inner scope, though C# forbids a local and a parameter of one
    /// method sharing a name, so the order is a tie-break that valid source cannot reach. The
    /// evaluation stack is left out on purpose: its slots are interpreter scratch under synthetic
    /// names, and a name resolving to one would answer a question nobody asked.
    fn frame_variables(&self, index: usize) -> Vec<(Variable, ChildReference)> {
        let mut visible = self.backend.variables_with_children(index, Scope::Locals);
        visible.extend(self.backend.variables_with_children(index, Scope::Arguments));
        visible
    }

    /// Answers an unscoped Debug Console line: from the debugger's own evaluation session where the
    /// program runs in this process, and with a refusal everywhere else.
    ///
    /// **THE TARGET DECIDES, AT RUN TIME, AND THE BUILD DOES NOT.** One build serves this computer's
    /// interpreter and a board alike, so carrying the session says nothing about where this
    /// session's program runs; a submission evaluated here for a board would run on this computer
    /// and be shown as the board's answer. See [`DebugBackend::runs_in_this_process`].
    ///
    /// The refusal names the two ways to read the target's own values, because a console line is
    /// usually someone reaching for a value they can have: a refusal that only says no leaves them
    /// with the impression that the session cannot show them anything.
    fn session_evaluate(&mut self, request: &Request, expression: &str) -> Vec<Message> {
        #[cfg(feature = "interpreter")]
        if self.backend.runs_in_this_process() {
            let body = crate::repl_eval::evaluate(&mut self.repl, expression);
            return vec![self.response(request, true, Some(body))];
        }
        let _ = expression;
        self.fail(
            request,
            "This session evaluates nothing of its own: a submission typed here would run in this \
             process rather than on the target being debugged. Hover a variable, or add its name \
             to Watch, to read it from the paused frame.",
        )
    }

    /// Lists code starting near the `memoryReference` address, each entry with its own
    /// address so the client can set instruction breakpoints.
    fn disassemble(&self, request: &Request) -> Json {
        let arguments = request.arguments.as_ref();
        let reference = arguments
            .and_then(|args| args.get("memoryReference"))
            .and_then(Json::as_str)
            .and_then(|reference| reference.parse::<u64>().ok());
        let Some(reference) = reference else {
            return json!({ "instructions": [] });
        };
        let offset = arguments
            .and_then(|args| args.get("instructionOffset"))
            .and_then(Json::as_i64)
            .unwrap_or(0);
        let count = arguments
            .and_then(|args| args.get("instructionCount"))
            .and_then(Json::as_u64)
            .unwrap_or(0)
            .min(4096) as usize;
        let instructions: Vec<Json> = self
            .backend
            .disassemble(reference, offset, count)
            .iter()
            .map(|entry| json!({ "address": entry.address.to_string(), "instruction": entry.text }))
            .collect();
        json!({ "instructions": instructions })
    }

    /// Decodes a `setVariable` request -- `variablesReference` to (frame, scope) exactly as
    /// `variables` does, plus `name` and `value` -- and asks the backend to write it. Returns
    /// the backend's re-rendered value on success, or `None` (an unknown/uneditable variable, or
    /// a value that does not parse as the slot's kind) for the caller to report as a failure.
    fn set_variable(&mut self, request: &Request) -> Option<String> {
        let reference = arg_u32(request, "variablesReference");
        if reference == 0 || reference >= CHILDREN_BASE {
            return None;
        }
        let (thread, frame_index) = self.frame_of((reference - 1) / 3);
        let scope = match (reference - 1) % 3 {
            0 => Scope::Arguments,
            1 => Scope::Locals,
            _ => Scope::Stack,
        };
        let name = arg_str(request, "name");
        let value = arg_str(request, "value");
        self.on_thread(thread, |debugger| {
            debugger.backend.set_variable(frame_index, scope, name, value)
        })
        .flatten()
    }

    fn response(&mut self, request: &Request, success: bool, body: Option<Json>) -> Message {
        self.out_seq += 1;
        Message::Response(Response {
            seq: self.out_seq,
            request_seq: request.seq,
            success,
            command: request.command.clone(),
            message: (!success).then(|| "unsupported request".to_owned()),
            body,
        })
    }

    /// A standalone unsuccessful response carrying a custom `message` (the generic
    /// [`Self::response`] path always says "unsupported request"). Returned as the whole
    /// reply -- a failed request emits no follow-up events.
    fn fail(&mut self, request: &Request, message: &str) -> Vec<Message> {
        self.out_seq += 1;
        vec![Message::Response(Response {
            seq: self.out_seq,
            request_seq: request.seq,
            success: false,
            command: request.command.clone(),
            message: Some(message.to_owned()),
            body: None,
        })]
    }

    /// A standalone unsuccessful response whose reason the client shows the user: the body is DAP's
    /// `ErrorResponse`, a structured `error` message with `showUser` set. The `message` that
    /// [`Self::fail`] sends is the protocol's raw short form, which DAP says is not shown in the UI.
    ///
    /// The reason travels as a variable of the format string rather than as the format string itself,
    /// so a reason containing braces is shown as written instead of being read as placeholders.
    fn fail_for_user(&mut self, request: &Request, id: u32, reason: &str) -> Vec<Message> {
        self.out_seq += 1;
        vec![Message::Response(Response {
            seq: self.out_seq,
            request_seq: request.seq,
            success: false,
            command: request.command.clone(),
            message: Some(reason.to_owned()),
            body: Some(json!({
                "error": {
                    "id": id,
                    "format": "{reason}",
                    "variables": { "reason": reason },
                    "showUser": true,
                }
            })),
        })]
    }

    fn event(&mut self, event: &str, body: Option<Json>) -> Message {
        self.out_seq += 1;
        Message::Event(Event {
            seq: self.out_seq,
            event: event.to_owned(),
            body,
        })
    }
}

/// Withdraws the verification granted before the backend was asked to program the set, once it
/// answers that it could not.
///
/// The backend reports one reason for the whole set because it cannot say which address failed,
/// so every breakpoint this request had marked verified is greyed and carries that reason. Only
/// those: a breakpoint already unverified keeps the more specific message it was given (past the
/// hardware limit, or a line that carries no code), which is the truer explanation of the two.
fn withdraw_verification(results: &mut [Json], reason: &str) {
    for breakpoint in results {
        if breakpoint["verified"] == json!(true) {
            breakpoint["verified"] = json!(false);
            breakpoint["message"] = json!(reason);
            breakpoint["reason"] = json!(REASON_FAILED);
        }
    }
}

/// A breakpoint that could not be set, with both of the things DAP gives for saying so.
///
/// # A BARE `verified: false` IS A REFUSAL WITH THE REASON REMOVED
///
/// DAP carries two fields for this and we were setting neither on most paths. `Breakpoint.message`
/// is "a message about the state of the breakpoint. This is shown to the user and can be used to
/// explain why a breakpoint could not be verified", and `Breakpoint.reason` is "a machine-readable
/// explanation of why a breakpoint may not be verified ... the adapter should omit this property"
/// when it is verified. So an editor showed a greyed dot with nothing to hover and no way for a
/// client to tell a breakpoint that may bind later from one that never will.
///
/// The two `reason` values the specification defines, and the rule for choosing between them:
///
/// * [`REASON_PENDING`] -- "might be verified in the future, but the adapter cannot verify it in the
///   current state". Capacity, and a line asked about before any source mapping exists.
/// * [`REASON_FAILED`] -- "not able to be verified, and the adapter does not believe it can be
///   verified without intervention". A line with no code in a mapping we DO have, a reference that
///   is not a number, a target that refused the set.
///
/// The difference is not cosmetic: `pending` invites the client to wait, `failed` tells a person to
/// change something.
fn unverified(reason: &'static str, message: String) -> Json {
    json!({ "verified": false, "reason": reason, "message": message })
}

/// `answer`, carrying the id later events name it by.
fn with_id(mut answer: Json, id: i64) -> Json {
    answer["id"] = json!(id);
    answer
}

/// The id the answer at `index` already has, where a pending answer gave it one (`ids`).
fn answer_id(ids: Option<&[i64]>, index: usize) -> Option<i64> {
    ids.and_then(|ids| ids.get(index).copied())
}

/// Says in `answer` whether the target holds it, for a breakpoint the target has a place for: an
/// armed one carries no reason, and one past the target's `cap` carries the reason it waits.
fn set_verified(answer: &mut Json, armed: bool, cap: Option<usize>) {
    answer["verified"] = json!(armed);
    if armed {
        if let Some(fields) = answer.as_object_mut() {
            fields.remove("message");
            fields.remove("reason");
        }
    } else {
        answer["message"] = json!(over_capacity_message(cap));
        answer["reason"] = json!(REASON_PENDING);
    }
}

/// `Breakpoint.reason` for a breakpoint that may still bind without anyone doing anything.
const REASON_PENDING: &str = "pending";

/// `Breakpoint.reason` for one that will not bind until a person changes something.
const REASON_FAILED: &str = "failed";

/// The message shown on a breakpoint left unverified because the target's hardware
/// comparators are all in use -- the editor displays it on the greyed breakpoint.
fn over_capacity_message(cap: Option<usize>) -> String {
    match cap {
        Some(max) => format!(
            "Inactive: this target has {max} hardware breakpoints and they are all in use. \
             Disable another breakpoint to enable this one."
        ),
        None => "Inactive: breakpoint capacity reached.".to_string(),
    }
}

/// One execution command, resolved by [`Debugger::run`] into backend calls.
#[derive(Clone, Copy)]
enum Action {
    Resume,
    StepIn,
    StepOver,
    StepOut,
}

/// A `stopped` event's body: why, and the thread the stop happened on. Every thread stops with it, as
/// on .NET.
fn stopped(reason: &str, thread: u32) -> Json {
    json!({ "reason": reason, "threadId": thread, "allThreadsStopped": true })
}

/// The `stopped` event for an exception stop, with the exception's type as the text a client shows
/// beside the stop.
fn stopped_on_exception(info: &ExceptionInfo, thread: u32) -> Json {
    let mut body = stopped("exception", thread);
    body["description"] = json!("Paused on exception");
    body["text"] = json!(info.type_name);
    body
}

/// The `exceptionInfo` answer for the exception the program is stopped on.
///
/// `breakMode` says why it stopped, in DAP's words: `always` for a stop the `all` filter made at a
/// throw, and `unhandled` for a throw that nothing will catch.
fn exception_info_body(info: &ExceptionInfo) -> Json {
    let short_name = info.type_name.rsplit('.').next().unwrap_or(&info.type_name);
    let break_mode = match info.filter {
        ExceptionFilter::All => "always",
        ExceptionFilter::Uncaught => "unhandled",
    };
    let mut body = json!({
        "exceptionId": info.type_name,
        "breakMode": break_mode,
        "details": { "typeName": short_name, "fullTypeName": info.type_name },
    });
    if !info.message.is_empty() {
        body["description"] = json!(info.message);
        body["details"]["message"] = json!(info.message);
    }
    body
}

/// The DAP filter id of an exception filter, as `initialize` offers it and `setExceptionBreakpoints`
/// names it.
fn filter_id(filter: ExceptionFilter) -> &'static str {
    match filter {
        ExceptionFilter::All => "all",
        ExceptionFilter::Uncaught => "uncaught",
    }
}

/// The exception filter a DAP filter id names, the inverse of [`filter_id`].
fn filter_named(id: &str) -> Option<ExceptionFilter> {
    [ExceptionFilter::All, ExceptionFilter::Uncaught]
        .into_iter()
        .find(|filter| filter_id(*filter) == id)
}

/// How `initialize` offers an exception filter to the user. Uncaught exceptions are on by default,
/// because a throw that nothing catches ends the run, and stopping where it happened is what a
/// user debugging the program wants.
fn filter_offer(filter: ExceptionFilter) -> Json {
    let (label, description, default) = match filter {
        ExceptionFilter::All => (
            "All Exceptions",
            "Stop wherever an exception is thrown, whether or not something catches it.",
            false,
        ),
        ExceptionFilter::Uncaught => (
            "Uncaught Exceptions",
            "Stop where an exception is thrown that nothing will catch.",
            true,
        ),
    };
    json!({ "filter": filter_id(filter), "label": label, "description": description, "default": default })
}

/// Where the `variablesReference`s for a value's MEMBERS begin; a frame's scopes use the numbers
/// below it (`frame * 3 + scope + 1`).
///
/// The two kinds share one number space because DAP gives them one field, so they are kept apart by
/// range: a frame id reaches [`MAX_FRAME_ID`] only after frames of some 87,000 threads have been
/// named in one session ([`FRAMES_PER_THREAD`]).
const CHILDREN_BASE: u32 = 1 << 30;

/// The frame ids one thread's stack is given: a frame's id is its thread's slot times this, plus its
/// place in the stack. As many as the interpreter's deepest call stack has frames.
const FRAMES_PER_THREAD: u32 = 4096;

/// The first frame id whose scope references would reach [`CHILDREN_BASE`]: a frame is named only
/// below it.
const MAX_FRAME_ID: u32 = (CHILDREN_BASE - 1) / 3;

/// A step command a single step of it left waiting ([`Debugger::step_on`]): what it steps, how, from
/// what depth, and how many single steps it has left.
#[derive(Clone, Copy)]
struct Stepping {
    action: Action,
    /// Whether it steps by source statement, or by call depth alone.
    by_source: bool,
    /// The call depth it started at.
    start: usize,
    /// The single steps it may still take before it gives up.
    budget: usize,
}

/// Whether a step of `action` that started at depth `start` has reached a depth it may end at:
/// `stepIn` any, `next` this frame or a caller (a stepped-over call has returned), `stepOut` a caller
/// (the current method has returned).
fn stepping_reached(action: Action, depth: usize, start: usize) -> bool {
    match action {
        Action::StepIn | Action::Resume => true,
        Action::StepOver => depth <= start,
        Action::StepOut => depth < start,
    }
}

/// The DAP `variablesReference` for a backend's [`ChildReference`]: 0 for a leaf, and otherwise the
/// backend's number moved up past the scope references.
///
/// DAP bounds a reference to 2^31 - 1, so a backend number that would pass that is answered as a leaf:
/// a row that cannot be expanded is better than a reference that names nothing.
fn dap_reference(children: ChildReference) -> u32 {
    if children == ChildReference::NONE {
        return 0;
    }
    CHILDREN_BASE
        .checked_add(children.0)
        .filter(|&reference| reference <= i32::MAX as u32)
        .unwrap_or(0)
}

/// The identifier of the error a session that cannot start shows the user ([`Debugger::refusing`]).
/// DAP asks that each message a user can see carry an identifier unique within the adapter, so a
/// report can name which one it was.
const ERROR_SESSION_REFUSED: u32 = 1;

/// The identifier of the error shown when a session ends with its target still in the debugger's
/// hands ([`Debugger::release_target`]).
const ERROR_TARGET_NOT_RELEASED: u32 = 2;

/// The identifier of the error shown when the backend could not start the target
/// ([`DebugBackend::launch`]).
const ERROR_LAUNCH_FAILED: u32 = 3;

/// The identifier of the error shown when the backend could not attach to the target
/// ([`DebugBackend::attach`]).
const ERROR_ATTACH_FAILED: u32 = 4;

/// The identifier of the error shown when the backend could not end the program
/// ([`DebugBackend::terminate`]).
const ERROR_TERMINATE_FAILED: u32 = 5;

/// The identifier of the error shown when the backend could not restart the program
/// ([`DebugBackend::restart`]).
const ERROR_RESTART_FAILED: u32 = 6;

/// The error a failed `launch` or `attach` is reported under, and the words that lead its reason.
fn start_failure(attaching: bool) -> (u32, &'static str) {
    if attaching {
        (ERROR_ATTACH_FAILED, "The session could not attach")
    } else {
        (ERROR_LAUNCH_FAILED, "The session could not start")
    }
}

/// The answer to a breakpoint request in a session that runs without debugging: one unverified
/// breakpoint for each one the request's `list` names, saying why. Nothing is armed on the target.
fn unarmed_without_debugging(request: &Request, list: &str) -> Json {
    let asked = request
        .arguments
        .as_ref()
        .and_then(|arguments| arguments.get(list))
        .and_then(Json::as_array)
        .map_or(0, Vec::len);
    let unarmed = unverified(REASON_FAILED, String::from(WITHOUT_DEBUGGING));
    json!({ "breakpoints": vec![unarmed; asked] })
}

/// What a breakpoint or filter is answered with in a session that runs without debugging.
const WITHOUT_DEBUGGING: &str = "The program runs without debugging, so it stops at no breakpoint.";

/// The backend behind [`Debugger::refusing`]. There is no target: nothing starts, and every question
/// about a program has an empty answer.
struct Unstartable;

impl DebugBackend for Unstartable {
    fn launch(&mut self) -> Result<(), String> {
        Err(String::from("there is no target to start"))
    }
    fn resume(&mut self) -> Stop {
        Stop::Done
    }
    fn step(&mut self) -> Stop {
        Stop::Done
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
    fn variables(&self, _frame: usize, _scope: Scope) -> Vec<lamella_debug_backend::Variable> {
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

/// What this adapter tells the client it can do, for the backend it drives, in the `initialize`
/// response.
///
/// A client asks for nothing it has not been told about, so a capability left out here is a
/// feature that never gets exercised and leaves no trace of why: `supportsEvaluateForHovers` is
/// what makes an editor send an `evaluate` when the pointer rests on a variable, and without it
/// no hover request is made, no hover appears, and the server sees no request to explain it.
///
/// Every answer read from `backend` is one the adapter can honor only where the target can carry it
/// out: a "Set Value" offered to every backend is one every device refuses, and an exception filter
/// offered by a target that cannot stop on an exception is a checkbox that does nothing.
fn capabilities(backend: &dyn DebugBackend) -> Json {
    let mut offered = json!({
        "supportsConfigurationDoneRequest": true,
        "supportsInstructionBreakpoints": true,
        "supportsDisassembleRequest": true,
        "supportsSetVariable": backend.can_set_variables(),
        "supportsEvaluateForHovers": true,
        "supportsTerminateRequest": backend.can_terminate(),
        "supportTerminateDebuggee": backend.can_terminate(),
        "supportsRestartRequest": backend.can_restart(),
        "supportsFunctionBreakpoints": backend.can_break_on_functions(),
    });
    let filters = backend.exception_filters();
    if !filters.is_empty() {
        offered["exceptionBreakpointFilters"] =
            json!(filters.iter().map(|&filter| filter_offer(filter)).collect::<Vec<_>>());
        offered["supportsExceptionInfoRequest"] = json!(true);
    }
    offered
}

fn arg_u32(request: &Request, field: &str) -> u32 {
    arg_opt_u32(request, field).unwrap_or(0)
}

/// A numeric argument the client may have left out, which [`arg_u32`]'s default cannot express:
/// `frameId` 0 is a real frame -- the innermost one -- so "no frame was named" and "frame 0 was
/// named" are different requests and have to stay different values.
fn arg_opt_u32(request: &Request, field: &str) -> Option<u32> {
    request
        .arguments
        .as_ref()
        .and_then(|args| args.get(field))
        .and_then(Json::as_u64)
        .map(|value| value as u32)
}

fn arg_str<'r>(request: &'r Request, field: &str) -> &'r str {
    request
        .arguments
        .as_ref()
        .and_then(|args| args.get(field))
        .and_then(Json::as_str)
        .unwrap_or("")
}

/// A flag argument, false when the client left it out.
fn arg_bool(request: &Request, field: &str) -> bool {
    request
        .arguments
        .as_ref()
        .and_then(|args| args.get(field))
        .and_then(Json::as_bool)
        .unwrap_or(false)
}

#[cfg(all(test, feature = "interpreter"))]
mod tests {
    use super::*;
    use lamella_cil::{Instruction, MethodBodyImage, Opcode, Operand};
    use lamella_token::Token;

    fn body(code: Vec<Instruction>) -> MethodBodyImage {
        MethodBodyImage {
            max_stack: 8,
            init_locals: true,
            local_var_sig: None,
            code: code.into_boxed_slice(),
            handlers: <Box<[lamella_cil::EhClause]>>::default(),
        }
    }

    fn request(seq: i64, command: &str, arguments: Option<Json>) -> Request {
        Request {
            seq,
            command: command.to_owned(),
            arguments,
        }
    }

    fn add_program() -> (Module, u32) {
        let mut module = Module::new();
        let write_line = module.add_intrinsic(
            0,
            lamella_cil_runtime::intrinsics::console_write_line,
            lamella_cil_runtime::intrinsic_registry::intrinsic_id("console_write_line"),
            1,
        );
        let write_line_token = Token(0x0A00_0001);
        module.bind_token(0, write_line_token, write_line);
        let hi: Vec<u16> = "hi".encode_utf16().collect();
        let string_token = Token(0x7000_0001);
        module.bind_string(0, string_token, &hi);
        let main = module.add_method_image(
            0,
            body(vec![
                Instruction::new(Opcode::Ldstr, Operand::Token(string_token)),
                Instruction::new(Opcode::Call, Operand::Token(write_line_token)),
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::LdcI43),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        (module, main)
    }

    #[test]
    fn initialize_reports_capabilities_then_launch_emits_initialized() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(module, main);
        let out = dbg.handle(&request(1, "initialize", None));
        assert_eq!(out.len(), 1);
        match &out[0] {
            Message::Response(r) => {
                assert!(r.success);
                assert_eq!(r.request_seq, 1);
                assert_eq!(
                    r.body.as_ref().unwrap()["supportsConfigurationDoneRequest"],
                    json!(true)
                );
            }
            other => panic!("expected response, got {other:?}"),
        }
        let launched = dbg.handle(&request(2, "launch", None));
        assert!(
            launched
                .iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "initialized"))
        );
    }

    #[test]
    fn launch_then_continue_runs_to_termination() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(module, main);
        assert!(matches!(&dbg.handle(&request(1, "launch", None))[0],
            Message::Response(r) if r.success));
        let out = dbg.handle(&request(2, "continue", None));
        assert!(matches!(&out[0], Message::Response(r) if r.success));
        let exited = out
            .iter()
            .find_map(|m| match m {
                Message::Event(e) if e.event == "exited" => Some(e),
                _ => None,
            })
            .expect("an exited event");
        assert_eq!(exited.body.as_ref().expect("exited body")["exitCode"], json!(5));
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "terminated"))
        );
        assert_eq!(dbg.output_string(), "hi\n");
    }

    use lamella_debug_backend::{Disassembled, Frame, Register, SourceLocation, Variable};


    /// A backend with NO UNWINDER: it counts the steps it is asked for and reports a constant
    /// depth, which is exactly `lamella-dap-probe`'s shape on a real target.
    ///
    struct NoUnwinderBackend {
        steps: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    impl DebugBackend for NoUnwinderBackend {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Done
        }
        fn step(&mut self) -> Stop {
            self.steps.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Stop::Step
        }
        /// Constant, always -- there is no unwinder behind it.
        fn depth(&self) -> usize {
            1
        }
        fn tracks_depth(&self) -> bool {
            false
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn stack(&self) -> Vec<Frame> {
            Vec::new()
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    /// `stepOut` and `next` on a backend that cannot unwind take ONE step, not the whole budget.
    ///
    #[test]
    fn stepping_out_of_a_backend_that_cannot_unwind_takes_one_step_not_the_whole_budget() {
        for action in ["stepOut", "next"] {
            let steps = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let backend = NoUnwinderBackend { steps: std::sync::Arc::clone(&steps) };
            let mut debugger = Debugger::with_backend(Box::new(backend));
            let _ = debugger.handle(&request(1, "launch", None));
            steps.store(0, std::sync::atomic::Ordering::Relaxed);
            let _ = debugger.handle(&request(2, action, None));
            let taken = steps.load(std::sync::atomic::Ordering::Relaxed);
            assert_eq!(
                taken, 1,
                "{action} on a backend with no unwinder must degrade to ONE step; it took {taken}"
            );
        }
    }

    /// A minimal backend for capacity tests: it resolves source line N to the opaque address
    /// N and reports a fixed hardware-breakpoint limit. Everything else is an inert stub.
    struct CapBackend {
        max: usize,
        /// The reason this backend refuses to program a set, or `None` to accept every set --
        /// which is the difference between a target the adapter may report armed and one it
        /// may not.
        refuse: Option<&'static str>,
    }

    impl DebugBackend for CapBackend {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Done
        }
        fn step(&mut self) -> Stop {
            Stop::Step
        }
        fn depth(&self) -> usize {
            1
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            self.refuse.map_or(Ok(()), |reason| Err(reason.to_string()))
        }
        fn max_breakpoints(&self) -> Option<usize> {
            Some(self.max)
        }
        fn resolve_source_breakpoint(&self, _document: &str, line: u32) -> Option<u64> {
            Some(u64::from(line))
        }
        fn stack(&self) -> Vec<Frame> {
            Vec::new()
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    fn output_note(messages: &[Message]) -> Option<String> {
        messages.iter().find_map(|m| match m {
            Message::Event(e) if e.event == "output" => e
                .body
                .as_ref()
                .and_then(|b| b.get("output"))
                .and_then(Json::as_str)
                .map(str::to_owned),
            _ => None,
        })
    }

    /// A backend that surfaces a one-shot connect banner from `take_output` -- as the WireHostBackend
    /// does with the board/chip identity it sets on HELLO -- and produces nothing thereafter. Inert
    /// otherwise. Used to check the adapter drains that banner at `launch`, before the program runs.
    struct BannerBackend {
        banner: Option<String>,
    }

    impl DebugBackend for BannerBackend {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Done
        }
        fn step(&mut self) -> Stop {
            Stop::Step
        }
        fn depth(&self) -> usize {
            1
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn resolve_source_breakpoint(&self, _document: &str, _line: u32) -> Option<u64> {
            None
        }
        fn stack(&self) -> Vec<Frame> {
            Vec::new()
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            self.banner.take()
        }
    }

    /// A backend whose program FAULTS on resume -- the trap case -- with the code the target gave for
    /// it, or none.
    struct FaultingBackend(Option<i32>);

    impl DebugBackend for FaultingBackend {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Fault("call token 0x0A000003 resolved to no method".to_string())
        }
        fn step(&mut self) -> Stop {
            Stop::Step
        }
        fn depth(&self) -> usize {
            1
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn resolve_source_breakpoint(&self, _document: &str, _line: u32) -> Option<u64> {
            None
        }
        fn stack(&self) -> Vec<Frame> {
            Vec::new()
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
        fn fault_exit_code(&self) -> Option<i32> {
            self.0
        }
    }

    /// A target with no threads of its own -- a board debugged through its probe -- lists its one
    /// thread as .NET's debugger names a main thread, as the interpreter on this computer lists it.
    #[test]
    fn a_target_with_one_thread_names_it_as_dotnet_names_a_main_thread() {
        let mut dbg = Debugger::with_backend(Box::new(FaultingBackend(None)));
        dbg.handle(&request(1, "launch", None));
        let out = dbg.handle(&request(2, "threads", None));
        let Some(Message::Response(response)) = out.first() else {
            panic!("threads answered {out:?}");
        };
        assert_eq!(
            response.body,
            Some(json!({ "threads": [{ "id": 1, "name": "Main Thread" }] }))
        );
    }

    /// The `exited` event of a fault carries the code the TARGET gave for it, where it gave one: a
    /// board's 134 for an exception nothing caught, not the -1 that stands for an end no target
    /// accounted for.
    #[test]
    fn a_fault_reports_the_code_the_target_gave_for_it() {
        let mut dbg = Debugger::with_backend(Box::new(FaultingBackend(Some(134))));
        dbg.handle(&request(1, "launch", None));
        let out = dbg.handle(&request(2, "continue", None));
        let exited = out.iter().find_map(|m| match m {
            Message::Event(e) if e.event == "exited" => e.body.clone(),
            _ => None,
        });
        assert_eq!(
            exited.and_then(|b| b.get("exitCode").and_then(serde_json::Value::as_i64)),
            Some(134),
            "the board's own code, not -1"
        );
    }

    /// A FAULTED session must report a NON-ZERO exit, and this is the regression it exists to catch.
    ///
    /// **A fault arm emitting `terminated` with no `exited` leaves a host nothing to read an exit code
    /// from, so it shows the default -- success.** A trapped program then reports "Ready" and exit 0,
    /// with the only evidence of failure one line of stderr in a pane the user may not be looking at.
    ///
    /// The remedy belongs here rather than in each host: a host inferring failure by grepping stderr for
    /// trap text is a heuristic every consumer would reimplement slightly differently.
    #[test]
    fn a_faulted_session_reports_a_nonzero_exit_rather_than_a_silent_success() {
        let mut dbg = Debugger::with_backend(Box::new(FaultingBackend(None)));
        dbg.handle(&request(1, "launch", None));
        let out = dbg.handle(&request(2, "continue", None));

        let exited = out.iter().find_map(|m| match m {
            Message::Event(e) if e.event == "exited" => e.body.clone(),
            _ => None,
        });
        assert!(
            exited.is_some(),
            "a fault must emit an `exited` event -- without one a host reads success by default"
        );
        assert_eq!(
            exited.and_then(|b| b.get("exitCode").and_then(serde_json::Value::as_i64)),
            Some(-1),
            "and the code must be non-zero, matching what the non-debug run path reports for a trap"
        );
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "terminated")),
            "the session still terminates"
        );
        assert!(
            out.iter().any(|m| matches!(m, Message::Event(e)
                if e.event == "output"
                    && e.body.as_ref().is_some_and(|b| b
                        .get("output")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|s| s.contains("resolved to no method"))))),
            "and the fault text still rides out on stderr"
        );
    }

    #[test]
    fn launch_surfaces_the_connect_banner_before_the_program_runs() {
        let mut dbg = Debugger::with_backend(Box::new(BannerBackend {
            banner: Some("Lamella Link: SAM E54 Xplained Pro\n".to_string()),
        }));
        let out = dbg.handle(&request(1, "launch", None));
        assert_eq!(
            output_note(&out).as_deref(),
            Some("Lamella Link: SAM E54 Xplained Pro\n"),
        );
    }

    #[test]
    fn hit_condition_parsing() {
        assert!(hit_satisfied("3", 3));
        assert!(!hit_satisfied("3", 2));
        assert!(!hit_satisfied("3", 4));
        assert!(hit_satisfied("==3", 3));
        assert!(hit_satisfied(">2", 3));
        assert!(!hit_satisfied(">2", 2));
        assert!(hit_satisfied(">=3", 3));
        assert!(hit_satisfied("<3", 2));
        assert!(!hit_satisfied("<3", 3));
        assert!(hit_satisfied("%5", 10));
        assert!(!hit_satisfied("%5", 11));
        assert!(hit_satisfied("  > 2 ", 3));
        assert!(hit_satisfied("garbage", 1));
    }

    /// A backend that reports a breakpoint `total_hits` times (as a loop would), then `Done`.
    /// `stack` always sits at the single breakpoint address, so the adapter's hit-count /
    /// logpoint filter finds its metadata. Inert otherwise.
    struct LoopBackend {
        address: u64,
        total_hits: u32,
        seen: u32,
    }

    impl DebugBackend for LoopBackend {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            if self.seen < self.total_hits {
                self.seen += 1;
                Stop::Breakpoint
            } else {
                Stop::Done
            }
        }
        fn step(&mut self) -> Stop {
            Stop::Step
        }
        fn depth(&self) -> usize {
            1
        }
        fn set_breakpoints(&mut self, addresses: &[u64]) -> Result<(), String> {
            self.address = addresses.first().copied().unwrap_or(0);
            Ok(())
        }
        fn resolve_source_breakpoint(&self, _document: &str, line: u32) -> Option<u64> {
            Some(u64::from(line))
        }
        fn stack(&self) -> Vec<Frame> {
            vec![Frame {
                address: self.address,
                name: String::new(),
                line: 0,
            }]
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    /// A backend whose step command has no reachable end: it either claims line information for
    /// the image and never parks on a statement (`source`), or descends a frame per step so a
    /// depth condition is never met. Both are the shape of a target stepped into a region the
    /// line table does not cover -- a runtime helper, or code past the end of the program.
    ///
    /// It asserts rather than stepping forever, so an adapter that does not bound its step loop
    /// FAILS this test instead of hanging it, which is the only way a runaway loop can be
    /// red-proved without a timeout.
    struct NoEndBackend {
        budget: usize,
        steps: usize,
        source: bool,
    }

    impl DebugBackend for NoEndBackend {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Done
        }
        fn step(&mut self) -> Stop {
            self.steps += 1;
            assert!(
                self.steps <= self.budget,
                "the adapter stepped {} times against a step_budget of {}: the step loop is \
                 unbounded, and against a real target it would step until someone killed \
                 the session",
                self.steps,
                self.budget
            );
            Stop::Step
        }
        fn step_budget(&self) -> usize {
            self.budget
        }
        fn has_source(&self) -> bool {
            self.source
        }
        fn at_source_boundary(&self) -> bool {
            false
        }
        fn depth(&self) -> usize {
            if self.source { 1 } else { 1 + self.steps }
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn stack(&self) -> Vec<Frame> {
            Vec::new()
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    /// Every `output` event's text in one string.
    fn console_text(messages: &[Message]) -> String {
        messages
            .iter()
            .filter_map(|m| match m {
                Message::Event(event) if event.event == "output" => event
                    .body
                    .as_ref()
                    .and_then(|body| body["output"].as_str())
                    .map(str::to_owned),
                _ => None,
            })
            .collect()
    }

    fn no_end_debugger(source: bool) -> Debugger {
        let mut dbg = Debugger::with_backend(Box::new(NoEndBackend {
            budget: 8,
            steps: 0,
            source,
        }));
        dbg.handle(&request(1, "launch", None));
        dbg
    }

    #[test]
    fn a_source_step_that_never_reaches_a_statement_stops_instead_of_stepping_forever() {
        let mut dbg = no_end_debugger(true);
        let out = dbg.handle(&request(2, "stepIn", None));
        assert!(has_event(&out, "stopped"), "the session must come back");
        assert!(
            !has_event(&out, "terminated"),
            "the target is healthy, not done"
        );
        let console = console_text(&out);
        assert!(
            console.contains("gave up after 8 instructions")
                && console.contains("no source statement was reached"),
            "the give-up must say what it was waiting for, not stop silently: {console:?}"
        );
        assert!(
            !console.contains("[lamella]") && !console.contains("runtime helper"),
            "{console:?}"
        );
    }

    #[test]
    fn a_depth_step_whose_call_never_returns_stops_instead_of_stepping_forever() {
        let mut dbg = no_end_debugger(false);
        let out = dbg.handle(&request(2, "next", None));
        assert!(has_event(&out, "stopped"), "the session must come back");
        let console = console_text(&out);
        assert!(
            console.contains("has not returned"),
            "the two give-ups have different remedies and must not share one message: {console:?}"
        );
    }

    fn loop_debugger(total_hits: u32) -> Debugger {
        let mut dbg = Debugger::with_backend(Box::new(LoopBackend {
            address: 0,
            total_hits,
            seen: 0,
        }));
        dbg.handle(&request(1, "launch", None));
        dbg
    }

    fn set_one_breakpoint(dbg: &mut Debugger, extra: Json) {
        let mut breakpoint = json!({ "line": 10 });
        if let (Some(obj), Some(extra)) = (breakpoint.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                obj.insert(key.clone(), value.clone());
            }
        }
        dbg.handle(&request(
            2,
            "setBreakpoints",
            Some(json!({ "source": { "path": "x.cs" }, "breakpoints": [breakpoint] })),
        ));
    }

    fn has_event(messages: &[Message], event: &str) -> bool {
        messages
            .iter()
            .any(|m| matches!(m, Message::Event(e) if e.event == event))
    }

    #[test]
    fn a_hit_count_breakpoint_skips_until_its_count_then_stops() {
        let mut dbg = loop_debugger(5);
        set_one_breakpoint(&mut dbg, json!({ "hitCondition": "3" }));
        let out = dbg.handle(&request(3, "continue", None));
        assert!(has_event(&out, "stopped"));
        assert!(!has_event(&out, "exited"));
    }

    #[test]
    fn a_hit_count_never_reached_runs_to_completion() {
        let mut dbg = loop_debugger(2);
        set_one_breakpoint(&mut dbg, json!({ "hitCondition": "5" }));
        let out = dbg.handle(&request(3, "continue", None));
        assert!(has_event(&out, "exited"));
        assert!(!has_event(&out, "stopped"));
    }

    #[test]
    fn a_logpoint_logs_each_hit_and_never_stops() {
        let mut dbg = loop_debugger(3);
        set_one_breakpoint(&mut dbg, json!({ "logMessage": "loop hit" }));
        let out = dbg.handle(&request(3, "continue", None));
        let logs = out
            .iter()
            .filter(|m| {
                matches!(m, Message::Event(e) if e.event == "output"
                    && e.body.as_ref().and_then(|b| b.get("output")).and_then(Json::as_str)
                        == Some("loop hit\n"))
            })
            .count();
        assert_eq!(logs, 3);
        assert!(has_event(&out, "exited"));
        assert!(!has_event(&out, "stopped"));
    }

    #[test]
    fn a_backend_that_could_not_program_the_set_leaves_no_breakpoint_reported_armed() {
        let mut dbg = Debugger::with_backend(Box::new(CapBackend {
            max: 8,
            refuse: Some("the unit refused the write"),
        }));
        dbg.handle(&request(1, "launch", None));
        let out = dbg.handle(&request(
            2,
            "setBreakpoints",
            Some(json!({
                "source": { "path": "Program.cs" },
                "breakpoints": [ { "line": 10 }, { "line": 11 } ],
            })),
        ));
        let Message::Response(r) = &out[0] else {
            panic!("expected a response");
        };
        let bps = r.body.as_ref().unwrap()["breakpoints"].as_array().unwrap();
        assert_eq!(bps.len(), 2, "both breakpoints are still reported, greyed rather than dropped");
        for (index, breakpoint) in bps.iter().enumerate() {
            assert_eq!(
                breakpoint["verified"],
                json!(false),
                "breakpoint {index} was within the limit, so only the backend's answer can grey it"
            );
            assert_eq!(
                breakpoint["message"],
                json!("the unit refused the write"),
                "and the editor shows the backend's own reason, not a generic one"
            );
        }
    }

    #[test]
    fn a_backend_that_programmed_the_set_leaves_the_same_breakpoints_verified() {
        let mut dbg = Debugger::with_backend(Box::new(CapBackend { max: 8, refuse: None }));
        dbg.handle(&request(1, "launch", None));
        let out = dbg.handle(&request(
            2,
            "setBreakpoints",
            Some(json!({
                "source": { "path": "Program.cs" },
                "breakpoints": [ { "line": 10 }, { "line": 11 } ],
            })),
        ));
        let Message::Response(r) = &out[0] else {
            panic!("expected a response");
        };
        let bps = r.body.as_ref().unwrap()["breakpoints"].as_array().unwrap();
        assert_eq!(bps[0]["verified"], json!(true));
        assert_eq!(bps[1]["verified"], json!(true));
    }

    #[test]
    fn a_refusal_does_not_overwrite_the_more_specific_reason_a_breakpoint_already_carried() {
        let mut dbg = Debugger::with_backend(Box::new(CapBackend {
            max: 2,
            refuse: Some("the wire dropped"),
        }));
        dbg.handle(&request(1, "launch", None));
        let out = dbg.handle(&request(
            2,
            "setBreakpoints",
            Some(json!({
                "source": { "path": "Program.cs" },
                "breakpoints": [ { "line": 10 }, { "line": 11 }, { "line": 12 } ],
            })),
        ));
        let Message::Response(r) = &out[0] else {
            panic!("expected a response");
        };
        let bps = r.body.as_ref().unwrap()["breakpoints"].as_array().unwrap();
        assert_eq!(bps[0]["message"], json!("the wire dropped"));
        assert_eq!(bps[1]["message"], json!("the wire dropped"));
        assert!(
            bps[2]["message"].as_str().expect("a message").contains("hardware breakpoints"),
            "the one that never fit keeps the reason that explains why: {}",
            bps[2]["message"]
        );
    }

    #[test]
    fn source_breakpoints_past_the_hardware_limit_are_unverified_with_a_message() {
        let mut dbg = Debugger::with_backend(Box::new(CapBackend { max: 2, refuse: None }));
        dbg.handle(&request(1, "launch", None));
        let out = dbg.handle(&request(
            2,
            "setBreakpoints",
            Some(json!({
                "source": { "path": "Program.cs" },
                "breakpoints": [ { "line": 10 }, { "line": 11 }, { "line": 12 } ],
            })),
        ));
        let Message::Response(r) = &out[0] else {
            panic!("expected a response");
        };
        let bps = r.body.as_ref().unwrap()["breakpoints"].as_array().unwrap();
        assert_eq!(bps.len(), 3);
        assert_eq!(bps[0]["verified"], json!(true));
        assert_eq!(bps[1]["verified"], json!(true));
        assert_eq!(bps[2]["verified"], json!(false));
        assert!(
            bps[2]["message"]
                .as_str()
                .is_some_and(|m| m.contains("hardware breakpoints")),
            "the over-capacity breakpoint should carry an explanatory message"
        );
    }

    #[test]
    fn continue_notes_inactive_breakpoints_once() {
        let mut dbg = Debugger::with_backend(Box::new(CapBackend { max: 2, refuse: None }));
        dbg.handle(&request(1, "launch", None));
        dbg.handle(&request(
            2,
            "setBreakpoints",
            Some(json!({
                "source": { "path": "Program.cs" },
                "breakpoints": [ { "line": 1 }, { "line": 2 }, { "line": 3 } ],
            })),
        ));
        let first = dbg.handle(&request(3, "continue", None));
        assert!(
            output_note(&first).is_some_and(|note| note.contains("inactive")),
            "the first continue should note the inactive breakpoint"
        );
        let second = dbg.handle(&request(4, "continue", None));
        assert!(
            output_note(&second).is_none(),
            "a repeat continue with the same count should not warn again"
        );
    }

    #[test]
    fn stepping_emits_stopped_then_inspects_locals_and_stack() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(module, main);
        dbg.handle(&request(1, "launch", None));
        let out = dbg.handle(&request(2, "next", None));
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "stopped"))
        );

        let trace = dbg.handle(&request(3, "stackTrace", None));
        let frames = &trace[0].response_body()["stackFrames"];
        assert_eq!(frames.as_array().unwrap().len(), 1);

        let scopes = dbg.handle(&request(4, "scopes", Some(json!({ "frameId": 0 }))));
        let stack_ref = find_scope(&scopes[0].response_body(), "Stack");
        let vars = dbg.handle(&request(
            5,
            "variables",
            Some(json!({ "variablesReference": stack_ref })),
        ));
        let variables = vars[0].response_body()["variables"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(variables.len(), 1);
        assert_eq!(variables[0]["value"], json!("\"hi\""));
    }

    #[test]
    fn set_variable_edits_an_argument_and_echoes_the_rerendered_value() {
        let (module, main) = call_program();
        let mut dbg = Debugger::new(module, main);
        dbg.handle(&request(1, "launch", None));
        dbg.handle(&request(2, "stepIn", None));
        dbg.handle(&request(3, "stepIn", None));
        dbg.handle(&request(4, "stepIn", None));
        assert_eq!(frame_count(&mut dbg), 2);

        let frame = innermost_frame_id(&mut dbg);
        let scopes = dbg.handle(&request(5, "scopes", Some(json!({ "frameId": frame }))));
        let args_ref = find_scope(&scopes[0].response_body(), "Arguments");

        let before = dbg.handle(&request(
            6,
            "variables",
            Some(json!({ "variablesReference": args_ref })),
        ));
        let arg0 = before[0].response_body()["variables"][0].clone();
        let arg0_name = arg0["name"].as_str().unwrap().to_owned();
        assert_eq!(arg0["value"], json!("2"));
        assert_eq!(arg0["type"], json!("int"));

        let out = dbg.handle(&request(
            7,
            "setVariable",
            Some(json!({
                "variablesReference": args_ref,
                "name": arg0_name,
                "value": "42",
            })),
        ));
        let Message::Response(r) = &out[0] else {
            panic!("expected a response");
        };
        assert!(r.success);
        assert_eq!(r.body.as_ref().unwrap()["value"], json!("42"));
        assert_eq!(r.body.as_ref().unwrap()["variablesReference"], json!(0));

        let after = dbg.handle(&request(
            8,
            "variables",
            Some(json!({ "variablesReference": args_ref })),
        ));
        assert_eq!(after[0].response_body()["variables"][0]["value"], json!("42"));
    }

    #[test]
    fn set_variable_rejects_a_non_numeric_value_for_an_int_slot() {
        let (module, main) = call_program();
        let mut dbg = Debugger::new(module, main);
        dbg.handle(&request(1, "launch", None));
        dbg.handle(&request(2, "stepIn", None));
        dbg.handle(&request(3, "stepIn", None));
        dbg.handle(&request(4, "stepIn", None));
        let frame = innermost_frame_id(&mut dbg);
        let scopes = dbg.handle(&request(5, "scopes", Some(json!({ "frameId": frame }))));
        let args_ref = find_scope(&scopes[0].response_body(), "Arguments");
        let before = dbg.handle(&request(
            6,
            "variables",
            Some(json!({ "variablesReference": args_ref })),
        ));
        let arg0_name = before[0].response_body()["variables"][0]["name"]
            .as_str()
            .unwrap()
            .to_owned();
        let out = dbg.handle(&request(
            7,
            "setVariable",
            Some(json!({
                "variablesReference": args_ref,
                "name": arg0_name,
                "value": "oops",
            })),
        ));
        let Message::Response(r) = &out[0] else {
            panic!("expected a response");
        };
        assert!(!r.success);
        assert!(r.message.as_deref().is_some_and(|m| m.contains("cannot set")));
    }

    fn string_local_program() -> (Module, u32) {
        let mut module = Module::new();
        let hi: Vec<u16> = "hi".encode_utf16().collect();
        let string_token = Token(0x7000_0001);
        module.bind_string(0, string_token, &hi);
        let main = module.add_method_image(
            0,
            body(vec![
                Instruction::new(Opcode::Ldstr, Operand::Token(string_token)),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        (module, main)
    }

    #[test]
    fn set_variable_edits_a_string_local_and_echoes_the_quoted_new_value() {
        let (module, main) = string_local_program();
        let mut dbg = Debugger::new(module, main);
        dbg.handle(&request(1, "launch", None));
        dbg.handle(&request(2, "next", None));
        dbg.handle(&request(3, "next", None));

        let scopes = dbg.handle(&request(4, "scopes", Some(json!({ "frameId": 0 }))));
        let locals_ref = find_scope(&scopes[0].response_body(), "Locals");

        let before = dbg.handle(&request(
            5,
            "variables",
            Some(json!({ "variablesReference": locals_ref })),
        ));
        let local0 = before[0].response_body()["variables"][0].clone();
        let local0_name = local0["name"].as_str().unwrap().to_owned();
        assert_eq!(local0["value"], json!("\"hi\""));
        assert_eq!(local0["type"], json!("string"));

        let out = dbg.handle(&request(
            6,
            "setVariable",
            Some(json!({
                "variablesReference": locals_ref,
                "name": local0_name,
                "value": "world",
            })),
        ));
        let Message::Response(r) = &out[0] else {
            panic!("expected a response");
        };
        assert!(r.success);
        assert_eq!(r.body.as_ref().unwrap()["value"], json!("\"world\""));

        let after = dbg.handle(&request(
            7,
            "variables",
            Some(json!({ "variablesReference": locals_ref })),
        ));
        let reread = &after[0].response_body()["variables"][0];
        assert_eq!(reread["value"], json!("\"world\""));
        assert_eq!(reread["type"], json!("string"));

        let quoted = dbg.handle(&request(
            8,
            "setVariable",
            Some(json!({
                "variablesReference": locals_ref,
                "name": local0_name,
                "value": "\"hi\"",
            })),
        ));
        let Message::Response(r) = &quoted[0] else {
            panic!("expected a response");
        };
        assert!(r.success);
        assert_eq!(r.body.as_ref().unwrap()["value"], json!("\"hi\""));
    }

    #[test]
    fn an_unknown_request_is_unsuccessful() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(module, main);
        let out = dbg.handle(&request(1, "unheardOf", None));
        assert!(matches!(&out[0], Message::Response(r) if !r.success));
    }

    /// A backend whose source mapping resolves a requested line into a DIFFERENT FILE, which is what
    /// the device backend's documented line-only fallback does when a client spells a path the
    /// producer did not record. It resolves `line 7` of anything to one address, and reports that
    /// address as line 42 of `other.cs`.
    struct ResolvesIntoAnotherFile;

    impl DebugBackend for ResolvesIntoAnotherFile {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Done
        }
        fn step(&mut self) -> Stop {
            Stop::Step
        }
        fn depth(&self) -> usize {
            1
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn resolve_source_breakpoint(&self, _document: &str, line: u32) -> Option<u64> {
            (line == 7).then_some(0x100)
        }
        fn has_source(&self) -> bool {
            true
        }
        fn source_location(&self, address: u64) -> Option<SourceLocation> {
            (address == 0x100).then(|| SourceLocation {
                file: String::from("other.cs"),
                line: 42,
                column: 1,
                end_line: 42,
                end_column: 1,
            })
        }
        fn stack(&self) -> Vec<Frame> {
            Vec::new()
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    /// A RESOLVED LINE IS REPORTED WITH THE FILE IT IS A LINE IN, never as a bare number the client
    /// will read against the file it asked about.
    ///
    /// `Breakpoint.line` is "the start line of the actual range covered by the breakpoint" and
    /// `Breakpoint.source` is "the source where the breakpoint is located" (DAP), so the pair is the
    /// answer and the number alone is not. The adapter was fetching the location, taking its line and
    /// **discarding the file it came from** -- so a request about `app.cs` line 7 that bound into
    /// `other.cs` line 42 came back as "line 42", which an editor shows on line 42 of `app.cs`.
    #[test]
    fn a_breakpoint_that_bound_into_another_file_reports_that_file() {
        let mut dbg = Debugger::with_backend(Box::new(ResolvesIntoAnotherFile));
        dbg.handle(&request(1, "launch", None));
        let args = json!({ "source": { "path": "app.cs" }, "breakpoints": [{ "line": 7 }] });
        let out = dbg.handle(&request(2, "setBreakpoints", Some(args)));
        let breakpoint = out[0].response_body()["breakpoints"][0].clone();

        assert_eq!(breakpoint["verified"], json!(true), "{breakpoint}");
        assert_eq!(
            breakpoint["line"],
            json!(42),
            "the actual bound line: {breakpoint}"
        );
        assert_eq!(
            breakpoint["source"]["path"],
            json!("other.cs"),
            "and the file that line is in, which the client did not ask about: {breakpoint}"
        );
    }

    /// **AN UNVERIFIED BREAKPOINT'S MESSAGE READS AS ONE SENTENCE AFTER ANOTHER**, with no run of
    /// spaces where the source continued a line: the editor shows it as it is sent, on hover.
    #[test]
    fn no_unverified_breakpoints_message_carries_a_run_of_spaces() {
        let message_of = |out: &[Message]| -> String {
            let breakpoint = out[0].response_body()["breakpoints"][0].clone();
            breakpoint["message"].as_str().unwrap_or_else(|| panic!("a message: {breakpoint}")).to_owned()
        };
        let mut messages = Vec::new();
        let mut mapped = Debugger::with_backend(Box::new(ResolvesIntoAnotherFile));
        mapped.handle(&request(1, "launch", None));
        let no_code = json!({ "source": { "path": "app.cs" }, "breakpoints": [{ "line": 9 }] });
        messages.push(message_of(&mapped.handle(&request(2, "setBreakpoints", Some(no_code)))));
        let not_an_address = json!({ "breakpoints": [{ "instructionReference": "here" }] });
        messages.push(message_of(&mapped.handle(&request(3, "setInstructionBreakpoints", Some(not_an_address)))));
        let (module, main) = add_program();
        let mut unmapped = Debugger::new(module, main);
        let no_mapping = json!({ "source": { "path": "app.cs" }, "breakpoints": [{ "line": 9 }] });
        messages.push(message_of(&unmapped.handle(&request(1, "setBreakpoints", Some(no_mapping)))));
        for message in messages {
            assert!(!message.contains("  "), "{message:?}");
        }
    }

    /// AN UNVERIFIED BREAKPOINT SAYS WHY, AND SAYS IT TWICE -- once for the person and once for the
    /// client. The two `reason` values are not interchangeable: `pending` invites a client to wait,
    /// `failed` tells someone to change something, and the same refusal means both depending on
    /// whether a source mapping exists yet.
    #[test]
    fn an_unverified_breakpoint_carries_a_reason_and_a_message() {
        // A mapping EXISTS and the line has no code in it: nobody waiting will fix that.
        let mut mapped = Debugger::with_backend(Box::new(ResolvesIntoAnotherFile));
        mapped.handle(&request(1, "launch", None));
        let args = json!({ "source": { "path": "app.cs" }, "breakpoints": [{ "line": 9 }] });
        let out = mapped.handle(&request(2, "setBreakpoints", Some(args)));
        let refused = out[0].response_body()["breakpoints"][0].clone();
        assert_eq!(refused["verified"], json!(false), "{refused}");
        assert_eq!(refused["reason"], json!("failed"), "{refused}");
        assert!(
            refused["message"]
                .as_str()
                .is_some_and(|text| text.contains("line 9")),
            "the message names the line, for a person: {refused}"
        );

        // NO mapping yet, which is the state VS Code sends its first setBreakpoints in: it may bind
        // by itself once the program is loaded, and `pending` is how a client is told to wait.
        let (module, main) = add_program();
        let mut unmapped = Debugger::new(module, main);
        let args = json!({ "source": { "path": "app.cs" }, "breakpoints": [{ "line": 9 }] });
        let out = unmapped.handle(&request(1, "setBreakpoints", Some(args)));
        let waiting = out[0].response_body()["breakpoints"][0].clone();
        assert_eq!(waiting["verified"], json!(false), "{waiting}");
        assert_eq!(waiting["reason"], json!("pending"), "{waiting}");
        assert!(waiting["message"].as_str().is_some(), "{waiting}");
    }

    /// A breakpoint that IS verified carries no `reason` -- the specification says to omit it, and a
    /// client that switches on its presence would read a verified breakpoint as a qualified one.
    #[test]
    fn a_verified_breakpoint_carries_no_reason() {
        let mut dbg = Debugger::with_backend(Box::new(ResolvesIntoAnotherFile));
        dbg.handle(&request(1, "launch", None));
        let args = json!({ "source": { "path": "app.cs" }, "breakpoints": [{ "line": 7 }] });
        let out = dbg.handle(&request(2, "setBreakpoints", Some(args)));
        let breakpoint = out[0].response_body()["breakpoints"][0].clone();
        assert_eq!(breakpoint["verified"], json!(true), "{breakpoint}");
        assert!(breakpoint.get("reason").is_none(), "{breakpoint}");
    }

    /// **A FILE'S BREAKPOINTS REPLACE THAT FILE'S, AND NO OTHER FILE'S.** `setBreakpoints` names one
    /// source and "clears all previous breakpoints in that source" (DAP), so a client sends one per
    /// file, and the breakpoints of every file stay armed together.
    #[test]
    fn a_files_breakpoints_replace_that_files_and_leave_every_other_file_armed() {
        let record = std::rc::Rc::new(core::cell::RefCell::new(Record::default()));
        let mut dbg = Debugger::with_backend(Box::new(Recorded { record: record.clone() }));
        dbg.handle(&request(1, "launch", None));
        let armed = |record: &std::rc::Rc<core::cell::RefCell<Record>>| {
            let mut last = record.borrow().sets.last().cloned().unwrap_or_default();
            last.sort_unstable();
            last
        };
        let in_a = json!({ "source": { "path": "a.cs" }, "breakpoints": [{ "line": 3 }] });
        let in_b = json!({ "source": { "path": "b.cs" }, "breakpoints": [{ "line": 7 }] });
        dbg.handle(&request(2, "setBreakpoints", Some(in_a)));
        dbg.handle(&request(3, "setBreakpoints", Some(in_b)));
        assert_eq!(armed(&record), vec![3, 7], "both files' breakpoints are armed");
        let none_in_a = json!({ "source": { "path": "a.cs" }, "breakpoints": [] });
        dbg.handle(&request(4, "setBreakpoints", Some(none_in_a)));
        assert_eq!(armed(&record), vec![7], "b.cs's breakpoint is still armed");
    }

    #[test]
    fn set_breakpoints_reports_unverified_pending_source_mapping() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(module, main);
        let args = json!({ "breakpoints": [{ "line": 1 }, { "line": 2 }] });
        let out = dbg.handle(&request(1, "setBreakpoints", Some(args)));
        let bps = out[0].response_body()["breakpoints"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(bps.len(), 2);
        assert_eq!(bps[0]["verified"], json!(false));
    }

    fn call_program() -> (Module, u32) {
        let mut module = Module::new();
        let add = module.add_method_image(
            0,
            body(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            2,
        );
        let add_token = Token(0x0600_0002);
        module.bind_token(0, add_token, add);
        let main = module.add_method_image(
            0,
            body(vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::LdcI43),
                Instruction::new(Opcode::Call, Operand::Token(add_token)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        (module, main)
    }

    fn frame_count(dbg: &mut Debugger) -> u64 {
        let trace = dbg.handle(&request(99, "stackTrace", None));
        trace[0].response_body()["totalFrames"].as_u64().unwrap()
    }

    /// The id the adapter gives the innermost frame: the first `stackTrace` entry, which is the frame an
    /// editor selects at a stop.
    fn innermost_frame_id(dbg: &mut Debugger) -> u64 {
        let trace = dbg.handle(&request(98, "stackTrace", None));
        trace[0].response_body()["stackFrames"][0]["id"]
            .as_u64()
            .unwrap()
    }

    /// A backend stopped two calls deep that lists its stack innermost first, as
    /// [`DebugBackend::stack`] documents and the device and Link backends do. Each scope's one
    /// variable names the frame index it was read for.
    struct TwoFramesDeep;

    impl DebugBackend for TwoFramesDeep {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Done
        }
        fn step(&mut self) -> Stop {
            Stop::Step
        }
        fn depth(&self) -> usize {
            2
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn stack(&self) -> Vec<Frame> {
            vec![
                Frame {
                    address: 0x0807_76bc,
                    name: String::from("Sleep"),
                    line: 373,
                },
                Frame {
                    address: 0x0809_8a42,
                    name: String::from("Main"),
                    line: 18,
                },
            ]
        }
        fn variables(&self, frame: usize, _scope: Scope) -> Vec<Variable> {
            vec![Variable {
                name: String::from("frame"),
                value: frame.to_string(),
                kind: String::from("index"),
            }]
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    /// A backend whose first step from `Main` enters a call, into code that has source or not as the
    /// test chooses, and which counts what the adapter asked of it.
    struct CallsIn {
        callee_has_source: bool,
        depth: usize,
        returned: bool,
        steps: std::rc::Rc<core::cell::Cell<usize>>,
        returns: std::rc::Rc<core::cell::Cell<usize>>,
    }

    /// `Main`'s address, before and after the call; the callee's address.
    const MAIN: u64 = 0x100;
    const CALLEE: u64 = 0x900;

    impl DebugBackend for CallsIn {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Done
        }
        fn step(&mut self) -> Stop {
            self.steps.set(self.steps.get() + 1);
            self.depth = 2;
            Stop::Step
        }
        fn run_to_return(&mut self) -> Stop {
            self.returns.set(self.returns.get() + 1);
            self.depth = 1;
            self.returned = true;
            Stop::Step
        }
        fn depth(&self) -> usize {
            self.depth
        }
        fn step_budget(&self) -> usize {
            50
        }
        fn has_source(&self) -> bool {
            true
        }
        fn at_source_boundary(&self) -> bool {
            self.returned || (self.depth == 2 && self.callee_has_source)
        }
        fn source_location(&self, address: u64) -> Option<SourceLocation> {
            let covered = address == MAIN || (address == CALLEE && self.callee_has_source);
            covered.then(|| SourceLocation {
                file: String::from("Program.cs"),
                line: 1,
                column: 1,
                end_line: 1,
                end_column: 1,
            })
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn stack(&self) -> Vec<Frame> {
            let main = Frame { address: MAIN, name: String::from("Main"), line: 1 };
            if self.depth == 2 {
                vec![Frame { address: CALLEE, name: String::from("Callee"), line: 0 }, main]
            } else {
                vec![main]
            }
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    /// Steps in once over a [`CallsIn`] and reports `(steps, returns)` the adapter asked for.
    fn step_in_over(callee_has_source: bool) -> (usize, usize) {
        let steps = std::rc::Rc::new(core::cell::Cell::new(0));
        let returns = std::rc::Rc::new(core::cell::Cell::new(0));
        let backend = CallsIn {
            callee_has_source,
            depth: 1,
            returned: false,
            steps: std::rc::Rc::clone(&steps),
            returns: std::rc::Rc::clone(&returns),
        };
        let mut dbg = Debugger::with_backend(Box::new(backend));
        dbg.handle(&request(1, "launch", Some(json!({ "stopOnEntry": true }))));
        dbg.handle(&request(2, "stepIn", Some(json!({ "threadId": 1 }))));
        (steps.get(), returns.get())
    }

    /// Set Value is offered for the backend that can make an edit, and not for one that refuses every
    /// edit -- which is what a device backend does, and why the editor offered it one it always refused.
    #[test]
    fn set_value_is_offered_only_by_a_backend_that_can_edit() {
        let offered = |dbg: &mut Debugger| {
            let out = dbg.handle(&request(1, "initialize", None));
            out[0].response_body()["supportsSetVariable"].clone()
        };
        let (module, main) = add_program();
        assert_eq!(offered(&mut Debugger::new(module, main)), json!(true), "the interpreter edits");
        assert_eq!(
            offered(&mut Debugger::with_backend(Box::new(TwoFramesDeep))),
            json!(false),
            "a backend without an edit of its own is not offered one"
        );
    }

    /// A step in that enters code no source covers runs it to its return in one request, instead of
    /// single-stepping every instruction of it.
    #[test]
    fn a_step_in_runs_a_call_into_code_no_source_covers_to_its_return() {
        let (steps, returns) = step_in_over(false);
        assert_eq!(returns, 1, "the call is run to its return once");
        assert_eq!(steps, 1, "after the one step that entered it, not one per instruction inside");
    }

    /// The control: a step in that enters code WITH source stops in it, as a step in always has.
    #[test]
    fn a_step_in_to_a_call_with_source_stops_inside_it() {
        let (steps, returns) = step_in_over(true);
        assert_eq!(returns, 0, "nothing is run past");
        assert_eq!(steps, 1);
    }

    /// A backend stopped in one frame whose locals are an object with members and a number, and
    /// which would accept ANY edit -- so a test proving an edit was refused proves the adapter
    /// refused it.
    struct HoldsAnObject;

    impl HoldsAnObject {
        /// The locals, each with the reference the backend hands out for it.
        fn rows(scope: Scope) -> Vec<(Variable, ChildReference)> {
            if !matches!(scope, Scope::Locals) {
                return Vec::new();
            }
            let row = |name: &str, value: &str, kind: &str| Variable {
                name: name.to_string(),
                value: value.to_string(),
                kind: kind.to_string(),
            };
            vec![
                (row("counter", "object #3", "object"), ChildReference(4)),
                (row("round", "7", "int"), ChildReference::NONE),
            ]
        }
    }

    impl DebugBackend for HoldsAnObject {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Done
        }
        fn step(&mut self) -> Stop {
            Stop::Step
        }
        fn depth(&self) -> usize {
            1
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn stack(&self) -> Vec<Frame> {
            vec![Frame { address: 0x10, name: String::from("Main"), line: 44 }]
        }
        fn variables(&self, _frame: usize, scope: Scope) -> Vec<Variable> {
            Self::rows(scope).into_iter().map(|(variable, _)| variable).collect()
        }
        fn variables_with_children(&self, _frame: usize, scope: Scope) -> Vec<(Variable, ChildReference)> {
            Self::rows(scope)
        }
        fn children(&self, reference: ChildReference) -> Vec<(Variable, ChildReference)> {
            if reference != ChildReference(4) {
                return Vec::new();
            }
            vec![(
                Variable { name: String::from("field1"), value: String::from("33"), kind: String::from("int") },
                ChildReference::NONE,
            )]
        }
        fn set_variable(&mut self, _frame: usize, _scope: Scope, _name: &str, _value: &str) -> Option<String> {
            Some(String::from("edited"))
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    /// A row whose value has members carries a reference, and asking for that reference lists them.
    #[test]
    fn a_row_with_members_carries_a_reference_that_lists_them() {
        let mut dbg = Debugger::with_backend(Box::new(HoldsAnObject));
        dbg.handle(&request(1, "launch", None));
        let scopes = dbg.handle(&request(2, "scopes", Some(json!({ "frameId": 0 }))));
        let locals = find_scope(&scopes[0].response_body(), "Locals");
        let read = dbg.handle(&request(3, "variables", Some(json!({ "variablesReference": locals }))));
        let rows = read[0].response_body()["variables"].clone();
        assert_eq!(rows[0]["name"], json!("counter"));
        assert_eq!(rows[0]["variablesReference"], json!(CHILDREN_BASE + 4), "{rows}");
        assert_eq!(rows[1]["variablesReference"], json!(0), "a number opens onto nothing: {rows}");

        let opened = dbg.handle(&request(
            4,
            "variables",
            Some(json!({ "variablesReference": rows[0]["variablesReference"].clone() })),
        ));
        let members = opened[0].response_body()["variables"].clone();
        assert_eq!(members[0]["name"], json!("field1"), "{members}");
        assert_eq!(members[0]["value"], json!("33"), "{members}");
    }

    /// A hover or a Watch row over an object opens the same way its row in the pane does.
    #[test]
    fn a_hovered_object_can_be_opened_like_its_row() {
        let mut dbg = Debugger::with_backend(Box::new(HoldsAnObject));
        dbg.handle(&request(1, "launch", None));
        let hover = |dbg: &mut Debugger, seq: i64, name: &str| {
            let answer = dbg.handle(&request(
                seq,
                "evaluate",
                Some(json!({ "expression": name, "frameId": 0, "context": "hover" })),
            ));
            answer[0].response_body()["variablesReference"].clone()
        };
        assert_eq!(hover(&mut dbg, 2, "counter"), json!(CHILDREN_BASE + 4));
        assert_eq!(hover(&mut dbg, 3, "round"), json!(0));
    }

    /// An edit aimed at a member is refused before any backend sees it, rather than decoded as a
    /// scope of a frame thousands deep -- the backend here would have accepted anything.
    #[test]
    fn an_edit_aimed_at_a_member_is_refused_rather_than_decoded_as_a_frame() {
        let mut dbg = Debugger::with_backend(Box::new(HoldsAnObject));
        dbg.handle(&request(1, "launch", None));
        let answer = dbg.handle(&request(
            2,
            "setVariable",
            Some(json!({ "variablesReference": CHILDREN_BASE + 4, "name": "field1", "value": "5" })),
        ));
        let Message::Response(response) = &answer[0] else {
            panic!("a response");
        };
        assert!(!response.success, "an edit of a member reached the backend");
    }

    /// A backend's reference moves up past the scope references, and one that would pass DAP's bound
    /// is answered as a leaf rather than as a reference that names nothing.
    #[test]
    fn a_child_reference_stays_inside_the_range_dap_allows() {
        assert_eq!(dap_reference(ChildReference::NONE), 0);
        assert_eq!(dap_reference(ChildReference(1)), CHILDREN_BASE + 1);
        assert_eq!(dap_reference(ChildReference(i32::MAX as u32)), 0);
        assert_eq!(dap_reference(ChildReference(u32::MAX)), 0);
    }

    /// The editor's frame 0 is where the target stopped, and the id that frame carries reads that
    /// frame's variables, for a backend that lists its stack innermost first.
    #[test]
    fn a_stack_listed_innermost_first_reaches_the_editor_innermost_first() {
        let mut dbg = Debugger::with_backend(Box::new(TwoFramesDeep));
        dbg.handle(&request(1, "launch", None));
        let trace = dbg.handle(&request(2, "stackTrace", None));
        let frames = trace[0].response_body()["stackFrames"].clone();
        assert_eq!(
            frames[0]["name"],
            json!("Sleep"),
            "frame 0 is the stop: {frames}"
        );
        assert_eq!(frames[1]["name"], json!("Main"), "{frames}");

        let scopes = dbg.handle(&request(
            3,
            "scopes",
            Some(json!({ "frameId": frames[0]["id"].clone() })),
        ));
        let arguments = find_scope(&scopes[0].response_body(), "Arguments");
        let read = dbg.handle(&request(
            4,
            "variables",
            Some(json!({ "variablesReference": arguments })),
        ));
        assert_eq!(
            read[0].response_body()["variables"][0]["value"],
            json!("0"),
            "the id of frame 0 reads frame 0's variables"
        );
    }

    /// A frame the backend could not locate reports no position at all, which is the one encoding a
    /// client is told to ignore: `StackFrame.line` is "the line within the source of the frame. If
    /// the source attribute is missing or doesn't exist, `line` is 0 and should be ignored by the
    /// client", and `column` says the same (DAP, `StackFrame`). Passing the backend's own line
    /// through with no `source` beside it hands the editor a number it is required to believe.
    #[test]
    fn a_frame_with_no_source_reports_no_line_or_column() {
        let mut dbg = Debugger::with_backend(Box::new(TwoFramesDeep));
        dbg.handle(&request(1, "launch", None));
        let trace = dbg.handle(&request(2, "stackTrace", None));
        let frames = trace[0].response_body()["stackFrames"].clone();
        for index in 0..2 {
            let frame = &frames[index];
            assert!(
                frame.get("source").is_none(),
                "this backend maps no address to a source: {frame}"
            );
            assert_eq!(frame["line"], json!(0), "{frame}");
            assert_eq!(frame["column"], json!(0), "{frame}");
        }
    }

    /// A backend that places each frame by where it stands, apart from where its address alone would
    /// place it: frame `index` at line `100 + index`, while its address maps to line 999.
    struct PlacesItsFrames;

    impl DebugBackend for PlacesItsFrames {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Done
        }
        fn step(&mut self) -> Stop {
            Stop::Step
        }
        fn depth(&self) -> usize {
            2
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn stack(&self) -> Vec<Frame> {
            [0x0800_0200u64, 0x0800_0310]
                .into_iter()
                .map(|address| Frame { address, name: String::from("f"), line: 0 })
                .collect()
        }
        fn source_location(&self, _address: u64) -> Option<SourceLocation> {
            Some(SourceLocation { file: String::from("P.cs"), line: 999, column: 1, end_line: 999, end_column: 1 })
        }
        fn frame_source_location(&self, index: usize, _address: u64) -> Option<SourceLocation> {
            let line = 100 + index as u32;
            Some(SourceLocation { file: String::from("P.cs"), line, column: 1, end_line: line, end_column: 1 })
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    /// Every frame of a `stackTrace` is placed where the backend says that frame stands, never by its
    /// address alone: a native frame above the innermost stands at a call, and its address is the
    /// instruction after it, which for a call that ends a statement is the next statement's.
    #[test]
    fn a_frame_is_placed_where_the_backend_says_it_stands_not_by_its_address_alone() {
        let mut dbg = Debugger::with_backend(Box::new(PlacesItsFrames));
        dbg.handle(&request(1, "launch", None));
        let trace = dbg.handle(&request(2, "stackTrace", None));
        let frames = trace[0].response_body()["stackFrames"].clone();
        assert_eq!(frames[0]["line"], json!(100), "{frames}");
        assert_eq!(frames[1]["line"], json!(101), "frame 1 by its own place, not line 999: {frames}");
        assert_eq!(frames[1]["instructionPointerReference"], json!("134218512"), "its address unchanged");
    }

    #[test]
    fn step_in_descends_into_a_call_while_next_steps_over_it() {
        let (module, main) = call_program();
        let mut dbg = Debugger::new(module, main);
        dbg.handle(&request(1, "launch", None));
        dbg.handle(&request(2, "stepIn", None));
        dbg.handle(&request(3, "stepIn", None));
        assert_eq!(frame_count(&mut dbg), 1);
        dbg.handle(&request(4, "stepIn", None));
        assert_eq!(frame_count(&mut dbg), 2);

        let (module, main) = call_program();
        let mut dbg = Debugger::new(module, main);
        dbg.handle(&request(1, "launch", None));
        dbg.handle(&request(2, "stepIn", None));
        dbg.handle(&request(3, "stepIn", None));
        dbg.handle(&request(4, "next", None));
        assert_eq!(frame_count(&mut dbg), 1);
    }

    #[test]
    fn step_out_returns_to_the_caller() {
        let (module, main) = call_program();
        let mut dbg = Debugger::new(module, main);
        dbg.handle(&request(1, "launch", None));
        dbg.handle(&request(2, "stepIn", None));
        dbg.handle(&request(3, "stepIn", None));
        dbg.handle(&request(4, "stepIn", None));
        assert_eq!(frame_count(&mut dbg), 2);
        dbg.handle(&request(5, "stepOut", None));
        assert_eq!(frame_count(&mut dbg), 1);
    }

    #[test]
    fn an_instruction_breakpoint_stops_continue() {
        let (module, main) = call_program();
        let mut dbg = Debugger::new(module, main);
        dbg.handle(&request(1, "initialize", None));
        let address = encode_address(main, 2).to_string();
        let args = json!({ "breakpoints": [{ "instructionReference": address }] });
        let set = dbg.handle(&request(2, "setInstructionBreakpoints", Some(args)));
        assert_eq!(
            set[0].response_body()["breakpoints"][0]["verified"],
            json!(true)
        );

        dbg.handle(&request(3, "launch", None));
        let out = dbg.handle(&request(4, "continue", None));
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "stopped"))
        );
        assert!(
            !out.iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "terminated"))
        );
        let trace = dbg.handle(&request(5, "stackTrace", None));
        assert_eq!(
            trace[0].response_body()["stackFrames"][0]["instructionPointerReference"],
            json!(encode_address(main, 2).to_string()),
            "stopped at the instruction the breakpoint was set on"
        );

        let out = dbg.handle(&request(6, "continue", None));
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "terminated"))
        );
    }

    #[test]
    fn addresses_round_trip() {
        assert_eq!(decode_address(encode_address(7, 42)), (7, 42));
    }

    #[test]
    fn disassemble_lists_a_methods_instructions() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(module, main);
        let reference = encode_address(main, 0).to_string();
        let args = json!({
            "memoryReference": reference,
            "instructionOffset": 0,
            "instructionCount": 6,
        });
        let out = dbg.handle(&request(1, "disassemble", Some(args)));
        let listing = out[0].response_body()["instructions"]
            .as_array()
            .unwrap()
            .clone();

        assert_eq!(listing.len(), 6);
        assert!(
            listing[0]["instruction"]
                .as_str()
                .unwrap()
                .starts_with("ldstr")
        );
        assert!(
            listing
                .iter()
                .any(|entry| entry["instruction"] == json!("add"))
        );
        assert_eq!(listing[5]["instruction"], json!("ret"));
        let address: u64 = listing[0]["address"].as_str().unwrap().parse().unwrap();
        assert_eq!(decode_address(address), (main, 0));
    }

    impl Message {
        fn response_body(&self) -> Json {
            match self {
                Message::Response(r) => r.body.clone().unwrap_or(Json::Null),
                other => panic!("expected response, got {other:?}"),
            }
        }
    }

    /// A backend that stops, lists a frame, and reports no arguments and no locals -- what a
    /// target whose debug information carries no variable locations does.
    struct ReportsNoVariables;

    impl DebugBackend for ReportsNoVariables {
        fn launch(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Breakpoint
        }
        fn step(&mut self) -> Stop {
            Stop::Step
        }
        fn depth(&self) -> usize {
            1
        }
        fn set_breakpoints(&mut self, _addresses: &[u64]) -> Result<(), String> {
            Ok(())
        }
        fn stack(&self) -> Vec<Frame> {
            vec![Frame { address: 0x2000_0100, name: String::from("Main"), line: 1 }]
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    /// The response to one `evaluate`, which is always the first message back -- anything after it
    /// is a follow-up event (see [`Debugger::absent_variables_note`]).
    fn evaluate_request(dbg: &mut Debugger, seq: i64, arguments: Json) -> Response {
        let mut out = dbg.handle(&request(seq, "evaluate", Some(arguments))).into_iter();
        let answer = match out.next().expect("an evaluate is answered") {
            Message::Response(response) => response,
            other => panic!("the response comes first, got {other:?}"),
        };
        for trailing in out {
            assert!(
                matches!(trailing, Message::Event(_)),
                "only events follow a response, got {trailing:?}"
            );
        }
        answer
    }

    /// A session stopped inside `add`, far enough into its body that the innermost frame has both
    /// arguments and an evaluation-stack slot.
    fn stopped_inside_add() -> Debugger {
        let (module, main) = call_program();
        let mut dbg = Debugger::new(module, main);
        dbg.handle(&request(1, "launch", None));
        for seq in 2..=5 {
            dbg.handle(&request(seq, "stepIn", None));
        }
        assert_eq!(frame_count(&mut dbg), 2, "stopped in the callee");
        dbg
    }

    #[test]
    fn a_hover_reads_a_variables_value_and_type_out_of_the_paused_frame() {
        let mut dbg = stopped_inside_add();
        let frame = innermost_frame_id(&mut dbg);
        let answer = evaluate_request(
            &mut dbg,
            10,
            json!({ "expression": "arg0", "frameId": frame, "context": "hover" }),
        );
        assert!(answer.success, "a hover over an argument is answerable: {:?}", answer.message);
        let body = answer.body.expect("a body");
        assert_eq!(body["result"], json!("2"));
        assert_eq!(body["type"], json!("int"));
    }

    #[test]
    fn a_stack_slot_is_not_reachable_by_name_though_an_argument_in_the_same_frame_is() {
        let mut dbg = stopped_inside_add();
        let frame = innermost_frame_id(&mut dbg);
        let scopes = dbg.handle(&request(10, "scopes", Some(json!({ "frameId": frame }))));
        let stack_ref = find_scope(&scopes[0].response_body(), "Stack");
        let slots = dbg.handle(&request(
            11,
            "variables",
            Some(json!({ "variablesReference": stack_ref })),
        ));
        let slot = slots[0].response_body()["variables"][0]["name"]
            .as_str()
            .expect("the frame has an evaluation-stack slot to hide")
            .to_owned();

        let hidden = evaluate_request(
            &mut dbg,
            12,
            json!({ "expression": slot, "frameId": frame, "context": "hover" }),
        );
        assert!(!hidden.success, "a stack slot is not read by name: {:?}", hidden.body);

        let control = evaluate_request(
            &mut dbg,
            13,
            json!({ "expression": "arg0", "frameId": frame, "context": "hover" }),
        );
        assert!(control.success, "but the frame does answer its arguments");
    }

    #[test]
    fn a_name_the_frame_does_not_have_is_refused_without_consulting_the_session() {
        let mut dbg = stopped_inside_add();
        dbg.repl = Some(crate::repl_eval::ReplCell::Unavailable(
            "the session answered a frame-scoped submission".to_owned(),
        ));
        let frame = innermost_frame_id(&mut dbg);
        let answer = evaluate_request(
            &mut dbg,
            10,
            json!({ "expression": "elsewhere", "frameId": frame, "context": "watch" }),
        );
        assert!(!answer.success, "a name the frame does not have is refused");
        let said = answer.message.unwrap_or_default();
        assert!(said.starts_with('<'), "a watch gets the inline form: {said}");
        assert!(
            !said.contains("the session answered"),
            "the session was consulted, which is the defect this closes: {said}"
        );
    }

    #[test]
    fn an_unscoped_console_line_still_reaches_the_session() {
        let mut dbg = stopped_inside_add();
        dbg.repl = Some(crate::repl_eval::ReplCell::Unavailable("no references".to_owned()));
        let answer =
            evaluate_request(&mut dbg, 10, json!({ "expression": "1 + 1", "context": "repl" }));
        assert!(answer.success, "a console line is answered as console output");
        assert!(
            answer.body.expect("a body")["result"]
                .as_str()
                .is_some_and(|said| said.contains("no references")),
            "and it is the session that answered it"
        );
    }

    #[test]
    fn a_frame_with_no_variables_reads_differently_from_a_name_that_is_absent() {
        let mut empty = Debugger::with_backend(Box::new(ReportsNoVariables));
        empty.handle(&request(1, "launch", None));
        let nothing = evaluate_request(
            &mut empty,
            2,
            json!({ "expression": "count", "frameId": 0, "context": "repl" }),
        );
        assert!(!nothing.success);
        let nothing = nothing.message.unwrap_or_default();

        let mut some = stopped_inside_add();
        let frame = innermost_frame_id(&mut some);
        let absent = evaluate_request(
            &mut some,
            10,
            json!({ "expression": "count", "frameId": frame, "context": "repl" }),
        );
        assert!(!absent.success);
        let absent = absent.message.unwrap_or_default();

        assert_ne!(nothing, absent, "the same submission, two causes, one answer");
        assert!(nothing.contains("no arguments and no locals"), "got {nothing}");
        assert!(absent.contains("not an argument or a local"), "got {absent}");
    }

    #[test]
    fn initialize_advertises_evaluate_for_hovers() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(module, main);
        let out = dbg.handle(&request(1, "initialize", None));
        assert_eq!(out[0].response_body()["supportsEvaluateForHovers"], json!(true));
    }

    fn find_scope(scopes_body: &Json, name: &str) -> u32 {
        scopes_body["scopes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|scope| scope["name"] == json!(name))
            .and_then(|scope| scope["variablesReference"].as_u64())
            .unwrap() as u32
    }

    /// What a [`Recorded`] backend was asked to do, readable after the adapter has taken the backend.
    #[derive(Default)]
    struct Record {
        /// Every breakpoint set the adapter programmed, in order.
        sets: Vec<Vec<u64>>,
        /// How many exception filters each set the adapter armed held, in order.
        filter_sets: Vec<usize>,
        launched: bool,
        /// Whether the target was handed back.
        released: bool,
        /// Why its `launch` and `attach` fail, while this is set.
        refuses: Option<String>,
        /// How many breakpoints it holds at once, for a target with a hardware limit.
        max: Option<usize>,
        /// Every source a line was resolved in, as the backend was handed it.
        documents: Vec<String>,
        /// The source the program records every line in, for a target that maps addresses to source.
        recorded_file: Option<String>,
        /// Where the program is stopped, for a target that shows a frame.
        frame_at: Option<u64>,
    }

    /// A backend for the sessions a [`Chooser`] starts: it resolves source line N to address N, offers
    /// Set Value, and records what it is asked into a [`Record`] the test keeps.
    struct Recorded {
        record: std::rc::Rc<core::cell::RefCell<Record>>,
    }

    impl DebugBackend for Recorded {
        fn launch(&mut self) -> Result<(), String> {
            if let Some(reason) = self.record.borrow().refuses.clone() {
                return Err(reason);
            }
            self.record.borrow_mut().launched = true;
            Ok(())
        }
        fn attach(&mut self) -> Result<(), String> {
            self.launch()
        }
        fn release(&mut self) -> Result<(), String> {
            self.record.borrow_mut().released = true;
            Ok(())
        }
        fn resume(&mut self) -> Stop {
            Stop::Done
        }
        fn step(&mut self) -> Stop {
            Stop::Step
        }
        fn depth(&self) -> usize {
            1
        }
        fn set_breakpoints(&mut self, addresses: &[u64]) -> Result<(), String> {
            self.record.borrow_mut().sets.push(addresses.to_vec());
            Ok(())
        }
        fn max_breakpoints(&self) -> Option<usize> {
            self.record.borrow().max
        }
        fn set_exception_filters(&mut self, filters: &[ExceptionFilter]) -> Result<(), String> {
            self.record.borrow_mut().filter_sets.push(filters.len());
            Ok(())
        }
        fn resolve_source_breakpoint(&self, document: &str, line: u32) -> Option<u64> {
            self.record.borrow_mut().documents.push(document.to_owned());
            Some(u64::from(line))
        }
        fn source_location(&self, address: u64) -> Option<SourceLocation> {
            let file = self.record.borrow().recorded_file.clone()?;
            let line = u32::try_from(address).ok()?;
            Some(SourceLocation { file, line, column: 1, end_line: line, end_column: 1 })
        }
        fn can_set_variables(&self) -> bool {
            true
        }
        fn stack(&self) -> Vec<Frame> {
            self.record
                .borrow()
                .frame_at
                .map(|address| Frame { address, name: String::from("Program.Main"), line: 0 })
                .into_iter()
                .collect()
        }
        fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
            Vec::new()
        }
        fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
            Vec::new()
        }
        fn read_registers(&self) -> Vec<Register> {
            Vec::new()
        }
        fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
            Vec::new()
        }
        fn take_output(&mut self) -> Option<String> {
            None
        }
    }

    /// A session with no target until its `launch`, whose chooser hands back a [`Recorded`] backend
    /// with `stop_at_entry`, `no_debug` and `notes`, and keeps every `(command, arguments)` it is given.
    fn chosen_session(
        stop_at_entry: bool,
        no_debug: bool,
        notes: Vec<String>,
    ) -> (
        Debugger,
        std::rc::Rc<core::cell::RefCell<Record>>,
        std::rc::Rc<core::cell::RefCell<Vec<(String, Json)>>>,
    ) {
        let record = std::rc::Rc::new(core::cell::RefCell::new(Record::default()));
        let asked = std::rc::Rc::new(core::cell::RefCell::new(Vec::new()));
        let (backend_record, chooser_asked) = (record.clone(), asked.clone());
        let mut notes = Some(notes);
        let debugger = Debugger::choosing(Box::new(move |command: &str, arguments: &Json| {
            chooser_asked.borrow_mut().push((command.to_owned(), arguments.clone()));
            Ok(Chosen {
                backend: Box::new(Recorded { record: backend_record.clone() }),
                stop_at_entry,
                no_debug,
                notes: notes.take().unwrap_or_default(),
                path_map: PathMap::default(),
            })
        }));
        (debugger, record, asked)
    }

    /// The events in `messages`, by name, in the order they were sent.
    fn event_names(messages: &[Message]) -> Vec<String> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Event(event) => Some(event.event.clone()),
                _ => None,
            })
            .collect()
    }

    /// The body of the first event named `name` in `messages`.
    fn event_body(messages: &[Message], name: &str) -> Option<Json> {
        messages.iter().find_map(|message| match message {
            Message::Event(event) if event.event == name => event.body.clone(),
            _ => None,
        })
    }

    /// The `breakpoint` events in `messages`: each one's `breakpoint`, in the order they were sent.
    fn breakpoint_events(messages: &[Message]) -> Vec<Json> {
        messages
            .iter()
            .filter_map(|message| match message {
                Message::Event(event) if event.event == "breakpoint" => event.body.clone(),
                _ => None,
            })
            .map(|body| {
                assert_eq!(body["reason"], json!("changed"), "{body}");
                body["breakpoint"].clone()
            })
            .collect()
    }

    /// **WHAT A CLIENT CONFIGURES BEFORE THE SESSION HAS A TARGET IS KEPT, AND SET WHEN THE TARGET
    /// STARTS.** Each answer is pending and carries an id, and once the launch has started the target
    /// a `breakpoint` event with that id says how it was set -- so the promise a pending answer makes is
    /// kept, for a line, a method's name and an exception filter alike.
    #[test]
    fn configuration_sent_before_the_target_is_chosen_is_set_when_it_starts() {
        let (mut dbg, record, _) = chosen_session(false, false, Vec::new());
        dbg.handle(&request(1, "initialize", None));
        let early = [
            ("setBreakpoints", json!({ "source": { "path": "a.cs" }, "breakpoints": [{ "line": 5 }] })),
            ("setFunctionBreakpoints", json!({ "breakpoints": [{ "name": "Program.Main" }] })),
            ("setExceptionBreakpoints", json!({ "filters": ["uncaught"] })),
        ];
        let mut ids = Vec::new();
        for (seq, (command, arguments)) in early.into_iter().enumerate() {
            let out = dbg.handle(&request(seq as i64 + 2, command, Some(arguments)));
            let answer = out[0].response_body()["breakpoints"][0].clone();
            assert_eq!(answer["verified"], json!(false), "{command}: {answer}");
            assert_eq!(answer["reason"], json!("pending"), "{command}: {answer}");
            assert!(
                answer["message"].as_str().is_some_and(|text| !text.contains("  ")),
                "{command}: {answer}"
            );
            ids.push(answer["id"].as_i64().unwrap_or_else(|| panic!("{command}: an id: {answer}")));
        }
        assert!(record.borrow().sets.is_empty(), "nothing is armed on a target not yet chosen");

        let launched = dbg.handle(&request(5, "launch", Some(json!({}))));
        let changed = breakpoint_events(&launched);
        let reported: Vec<i64> = changed.iter().filter_map(|breakpoint| breakpoint["id"].as_i64()).collect();
        assert_eq!(reported, ids, "one event for each early answer, by its id: {changed:?}");
        assert_eq!(changed[0]["verified"], json!(true), "the line is set: {}", changed[0]);
        assert_eq!(changed[0]["line"], json!(5), "{}", changed[0]);
        assert!(
            record.borrow().sets.last().is_some_and(|armed| armed.contains(&5)),
            "and armed on the target: {:?}",
            record.borrow().sets
        );
        assert_eq!(changed[1]["verified"], json!(false), "{}", changed[1]);
        assert_eq!(changed[2]["verified"], json!(false), "{}", changed[2]);
    }

    /// **A RUN WITHOUT DEBUGGING THAT DOES NOT START LEAVES THE SESSION AS IT WAS**: the next
    /// breakpoint is set, rather than answered as though the program were running without debugging.
    #[test]
    fn a_run_without_debugging_that_does_not_start_leaves_breakpoints_settable() {
        let record = std::rc::Rc::new(core::cell::RefCell::new(Record {
            refuses: Some(String::from("the board is not attached")),
            ..Record::default()
        }));
        let mut dbg = Debugger::with_backend(Box::new(Recorded { record: record.clone() }));
        let failed = dbg.handle(&request(1, "launch", Some(json!({ "noDebug": true }))));
        assert!(
            failed.iter().any(|message| matches!(message, Message::Response(r) if !r.success)),
            "the launch failed: {failed:?}"
        );
        record.borrow_mut().refuses = None;
        let args = json!({ "source": { "path": "a.cs" }, "breakpoints": [{ "line": 3 }] });
        let out = dbg.handle(&request(2, "setBreakpoints", Some(args)));
        let answer = out[0].response_body()["breakpoints"][0].clone();
        assert_eq!(answer["verified"], json!(true), "{answer}");
    }

    #[test]
    fn a_session_started_without_a_target_takes_the_one_its_launch_names() {
        let (mut dbg, record, asked) = chosen_session(false, false, Vec::new());
        let initialized = dbg.handle(&request(1, "initialize", None));
        assert_eq!(
            initialized[0].response_body()["supportsSetVariable"],
            json!(false),
            "nothing is offered for a target that has not been named yet"
        );
        let arguments = json!({ "device": "local", "projectPath": "p.csproj" });
        let out = dbg.handle(&request(2, "launch", Some(arguments.clone())));
        assert!(matches!(&out[0], Message::Response(response) if response.success), "{out:?}");
        assert_eq!(*asked.borrow(), vec![(String::from("launch"), arguments)]);
        assert!(record.borrow().launched, "the chosen backend is the one launched");
        let names = event_names(&out);
        let capabilities = names.iter().position(|name| name == "capabilities");
        let initialized_at = names.iter().position(|name| name == "initialized");
        assert!(
            capabilities.is_some() && capabilities < initialized_at,
            "the chosen target's capabilities reach the client before it configures: {names:?}"
        );
        let body = event_body(&out, "capabilities").expect("a capabilities event");
        assert_eq!(body["capabilities"]["supportsSetVariable"], json!(true));
    }

    #[test]
    fn a_launch_the_chooser_refuses_fails_with_its_reason_for_the_user() {
        let mut dbg = Debugger::choosing(Box::new(|_: &str, _: &Json| {
            Err(String::from("no device answers to the id usb:0001"))
        }));
        dbg.handle(&request(1, "initialize", None));
        let out = dbg.handle(&request(2, "launch", Some(json!({ "device": "usb:0001" }))));
        let Message::Response(response) = &out[0] else { panic!("{out:?}") };
        assert!(!response.success);
        let error = &response.body.as_ref().expect("a structured error")["error"];
        assert_eq!(error["showUser"], json!(true));
        let shown = error["variables"]["reason"].as_str().unwrap_or_default();
        assert!(shown.contains("no device answers to the id usb:0001"), "{shown}");
        assert!(!event_names(&out).contains(&String::from("initialized")));
    }

    #[test]
    fn the_choosers_notes_reach_the_console_before_the_program_starts() {
        let note = String::from("lamella-debug: launch.json names \"stopOnEntery\", which is no field of a Lamella launch");
        let (mut dbg, _, _) = chosen_session(false, false, vec![note.clone()]);
        dbg.handle(&request(1, "initialize", None));
        let out = dbg.handle(&request(2, "launch", Some(json!({}))));
        let names = event_names(&out);
        let output = names.iter().position(|name| name == "output");
        assert!(output.is_some() && output < names.iter().position(|name| name == "initialized"), "{names:?}");
        let body = event_body(&out, "output").expect("the note");
        assert_eq!(body["category"], json!("console"));
        assert!(body["output"].as_str().unwrap_or_default().contains(&note));
    }

    #[test]
    fn stop_at_entry_is_the_choosers_answer_not_a_field_the_adapter_reads() {
        let (mut dbg, _, _) = chosen_session(true, false, Vec::new());
        dbg.handle(&request(1, "initialize", None));
        dbg.handle(&request(2, "launch", Some(json!({ "stopOnEntry": false }))));
        let out = dbg.handle(&request(3, "configurationDone", None));
        let stopped = event_body(&out, "stopped").expect("a stop at the entry");
        assert_eq!(stopped["reason"], json!("entry"));
    }

    #[test]
    fn run_without_debugging_arms_no_breakpoint_and_says_why_each_is_greyed() {
        let (mut dbg, record, _) = chosen_session(true, true, Vec::new());
        dbg.handle(&request(1, "initialize", None));
        dbg.handle(&request(2, "launch", Some(json!({ "noDebug": true }))));
        let out = dbg.handle(&request(
            3,
            "setBreakpoints",
            Some(json!({ "source": { "path": "Program.cs" }, "breakpoints": [{ "line": 5 }, { "line": 9 }] })),
        ));
        let answered = out[0].response_body()["breakpoints"].as_array().cloned().unwrap_or_default();
        assert_eq!(answered.len(), 2, "one answer per breakpoint asked for");
        for breakpoint in &answered {
            assert_eq!(breakpoint["verified"], json!(false));
            assert!(breakpoint["message"].as_str().unwrap_or_default().contains("without debugging"), "{breakpoint}");
        }
        assert!(
            record.borrow().sets.iter().all(Vec::is_empty),
            "nothing is armed on the target: {:?}",
            record.borrow().sets
        );
        let ran = dbg.handle(&request(4, "configurationDone", None));
        assert!(event_body(&ran, "stopped").is_none(), "no stop at the entry either: {ran:?}");
    }

    #[test]
    fn a_session_given_its_backend_up_front_still_honors_no_debug_from_the_request() {
        let record = std::rc::Rc::new(core::cell::RefCell::new(Record::default()));
        let mut dbg = Debugger::with_backend(Box::new(Recorded { record: record.clone() }));
        dbg.handle(&request(1, "initialize", None));
        dbg.handle(&request(2, "launch", Some(json!({ "noDebug": true, "stopOnEntry": true }))));
        let out = dbg.handle(&request(
            3,
            "setBreakpoints",
            Some(json!({ "source": { "path": "Program.cs" }, "breakpoints": [{ "line": 5 }] })),
        ));
        assert_eq!(out[0].response_body()["breakpoints"][0]["verified"], json!(false));
        assert!(record.borrow().sets.iter().all(Vec::is_empty));
        let ran = dbg.handle(&request(4, "configurationDone", None));
        assert!(event_body(&ran, "stopped").is_none(), "{ran:?}");
    }

    /// The session a debugger that serves every target runs: this build carries the evaluation
    /// session, and the target its launch chose is not in this process, so the console line is
    /// refused rather than answered by the session.
    #[test]
    fn a_console_line_on_a_chosen_target_elsewhere_is_refused_though_this_build_can_evaluate() {
        let (mut dbg, _, _) = chosen_session(false, false, Vec::new());
        dbg.handle(&request(1, "initialize", None));
        dbg.handle(&request(2, "launch", Some(json!({}))));
        dbg.repl = Some(crate::repl_eval::ReplCell::Unavailable(
            "the session answered a line for a target elsewhere".to_owned(),
        ));
        let answer =
            evaluate_request(&mut dbg, 3, json!({ "expression": "1 + 1", "context": "repl" }));
        assert!(!answer.success, "{:?}", answer.body);
        let said = answer.message.unwrap_or_default();
        assert!(!said.contains("the session answered"), "{said}");
        assert!(said.contains("Hover") && said.contains("Watch"), "{said}");
    }

    /// **A CHOSEN TARGET IS THE SESSION'S ONLY ONCE IT HAS STARTED.** One that does not start is
    /// handed back, no capabilities are offered for it, and the session has no target again, so its
    /// next `launch` is put to the chooser as the first was.
    #[test]
    fn a_chosen_target_that_does_not_start_is_handed_back_and_the_next_launch_chooses_again() {
        let (mut dbg, record, asked) = chosen_session(true, false, Vec::new());
        record.borrow_mut().refuses = Some(String::from("the board did not answer the write"));
        dbg.handle(&request(1, "initialize", None));
        let out = dbg.handle(&request(2, "launch", Some(json!({ "stopAtEntry": true }))));
        let Message::Response(response) = &out[0] else { panic!("{out:?}") };
        assert!(!response.success, "{out:?}");
        let names = event_names(&out);
        assert!(
            !names.contains(&String::from("capabilities")) && !names.contains(&String::from("initialized")),
            "nothing is offered for a target that did not start: {names:?}"
        );
        assert!(record.borrow().released, "the target that did not start is handed back");

        record.borrow_mut().refuses = None;
        let again = dbg.handle(&request(3, "launch", Some(json!({ "stopAtEntry": true }))));
        assert!(matches!(&again[0], Message::Response(response) if response.success), "{again:?}");
        assert_eq!(asked.borrow().len(), 2, "the second launch is the chooser's to answer too");
        let names = event_names(&again);
        assert!(names.contains(&String::from("capabilities")), "{names:?}");
        let ran = dbg.handle(&request(4, "configurationDone", None));
        let stopped = event_body(&ran, "stopped").expect("the chooser's entry stop, as the launch asked");
        assert_eq!(stopped["reason"], json!("entry"));
    }

    /// The host is told each launch's outcome once: a refusal, a target that was chosen and did not
    /// start, and one that started, in that order here.
    #[test]
    fn the_host_is_told_each_launch_that_started_and_why_each_other_did_not() {
        let record = std::rc::Rc::new(core::cell::RefCell::new(Record::default()));
        let told = std::rc::Rc::new(core::cell::RefCell::new(Vec::<Result<(), String>>::new()));
        let (backend_record, mut answers) = (record.clone(), 0);
        let report = told.clone();
        let mut dbg = Debugger::choosing(Box::new(move |_: &str, _: &Json| {
            answers += 1;
            if answers == 1 {
                return Err(String::from("no device answers to the id usb:0001"));
            }
            Ok(Chosen {
                backend: Box::new(Recorded { record: backend_record.clone() }),
                stop_at_entry: false,
                no_debug: false,
                notes: Vec::new(),
                path_map: PathMap::default(),
            })
        }))
        .reporting_starts(Box::new(move |outcome: Result<(), &str>| {
            report.borrow_mut().push(outcome.map_err(str::to_owned));
        }));
        dbg.handle(&request(1, "initialize", None));
        dbg.handle(&request(2, "launch", Some(json!({}))));
        record.borrow_mut().refuses = Some(String::from("the board did not answer the write"));
        dbg.handle(&request(3, "launch", Some(json!({}))));
        record.borrow_mut().refuses = None;
        dbg.handle(&request(4, "launch", Some(json!({}))));
        assert_eq!(
            *told.borrow(),
            vec![
                Err(String::from("no device answers to the id usb:0001")),
                Err(String::from("the board did not answer the write")),
                Ok(()),
            ]
        );
    }

    /// **RUN WITHOUT DEBUGGING ARMS NOTHING, INCLUDING WHAT WAS SET BEFORE THE LAUNCH SAID SO.** A
    /// client may configure before it launches, and a breakpoint or filter armed then would stop a
    /// program the user asked to run without stopping.
    #[test]
    fn breakpoints_and_filters_set_before_a_launch_without_debugging_are_disarmed() {
        let record = std::rc::Rc::new(core::cell::RefCell::new(Record::default()));
        let mut dbg = Debugger::with_backend(Box::new(Recorded { record: record.clone() }));
        dbg.handle(&request(1, "initialize", None));
        dbg.handle(&request(
            2,
            "setBreakpoints",
            Some(json!({ "source": { "path": "Program.cs" }, "breakpoints": [{ "line": 5 }] })),
        ));
        assert_eq!(record.borrow().sets.last(), Some(&vec![5]), "armed before the launch");
        dbg.handle(&request(3, "launch", Some(json!({ "noDebug": true }))));
        assert_eq!(
            record.borrow().sets.last(),
            Some(&Vec::new()),
            "the launch that runs without debugging disarms it: {:?}",
            record.borrow().sets
        );
        assert_eq!(record.borrow().filter_sets.last(), Some(&0), "and every exception filter");
    }

    /// A breakpoint set before the launch named a target was answered against no target, so the
    /// target the launch chooses is given it: it is armed there before the program runs.
    #[test]
    fn an_instruction_breakpoint_set_before_the_target_is_chosen_is_armed_on_it() {
        let (mut dbg, record, _) = chosen_session(false, false, Vec::new());
        dbg.handle(&request(1, "initialize", None));
        dbg.handle(&request(
            2,
            "setInstructionBreakpoints",
            Some(json!({ "breakpoints": [{ "instructionReference": "4096" }] })),
        ));
        dbg.handle(&request(3, "launch", Some(json!({}))));
        assert!(
            record.borrow().sets.iter().any(|set| set == &vec![4096]),
            "the chosen target holds the breakpoint set before it was chosen: {:?}",
            record.borrow().sets
        );
    }

    /// The breakpoints the response first in `messages` answered, in the order its request named them.
    fn answered(messages: &[Message]) -> Vec<Json> {
        let Some(Message::Response(response)) = messages.first() else {
            panic!("expected a response first");
        };
        response
            .body
            .as_ref()
            .and_then(|body| body["breakpoints"].as_array().cloned())
            .unwrap_or_default()
    }

    /// Whether each of `answers` is verified.
    fn verified(answers: &[Json]) -> Vec<bool> {
        answers.iter().map(|answer| answer["verified"] == json!(true)).collect()
    }

    /// A `setBreakpoints` for the source `path` at `lines`.
    fn set_lines(dbg: &mut Debugger, seq: i64, path: &str, lines: &[u32]) -> Vec<Message> {
        let breakpoints: Vec<Json> = lines.iter().map(|line| json!({ "line": line })).collect();
        dbg.handle(&request(
            seq,
            "setBreakpoints",
            Some(json!({ "source": { "path": path }, "breakpoints": breakpoints })),
        ))
    }

    /// A `setInstructionBreakpoints` at `addresses`.
    fn set_instructions(dbg: &mut Debugger, seq: i64, addresses: &[u64]) -> Vec<Message> {
        let breakpoints: Vec<Json> = addresses
            .iter()
            .map(|address| json!({ "instructionReference": address.to_string() }))
            .collect();
        dbg.handle(&request(
            seq,
            "setInstructionBreakpoints",
            Some(json!({ "breakpoints": breakpoints })),
        ))
    }

    /// A launched session over a [`Recorded`] target that holds `max` breakpoints at once.
    fn capped_session(max: usize) -> (Debugger, std::rc::Rc<core::cell::RefCell<Record>>) {
        let record = std::rc::Rc::new(core::cell::RefCell::new(Record {
            max: Some(max),
            ..Record::default()
        }));
        let mut dbg = Debugger::with_backend(Box::new(Recorded { record: record.clone() }));
        dbg.handle(&request(1, "launch", None));
        (dbg, record)
    }

    /// **ONE FILE'S UPDATE KEEPS THE COMPARATORS WHERE THEY ARE, AND THE CLIENT IS TOLD WHAT A FREED
    /// ONE ARMED.** On a target holding two, `a.cs` has both and `b.cs`'s two wait. Taking one of
    /// `a.cs`'s away leaves its other armed and frees a comparator; the first of `b.cs`'s takes it,
    /// and a `breakpoint` event names it by the id it was answered with.
    #[test]
    fn a_files_update_keeps_its_armed_breakpoints_and_says_what_a_freed_comparator_armed() {
        let (mut dbg, record) = capped_session(2);
        let a = answered(&set_lines(&mut dbg, 2, "a.cs", &[1, 2]));
        assert_eq!(verified(&a), [true, true]);
        let b = answered(&set_lines(&mut dbg, 3, "b.cs", &[10, 20]));
        assert_eq!(verified(&b), [false, false], "the target is full");
        assert!(b[0]["id"].is_i64(), "every answer carries an id a later event can name: {b:?}");
        let out = set_lines(&mut dbg, 4, "a.cs", &[1]);
        assert_eq!(verified(&answered(&out)), [true], "a.cs keeps the comparator it held");
        let changed = breakpoint_events(&out);
        assert_eq!(changed.len(), 1, "only what the freed comparator armed changed: {changed:?}");
        assert_eq!(changed[0]["id"], b[0]["id"], "b.cs's first");
        assert_eq!(changed[0]["verified"], json!(true));
        assert_eq!(
            record.borrow().sets.last(),
            Some(&vec![1, 10, 20]),
            "the target holds the breakpoints the client shows armed first"
        );
    }

    /// **A KIND ARMED LATER IN THE SET IS TOLD WHEN IT LOSES ITS COMPARATOR, AND WHEN IT GETS ONE
    /// BACK.** Source breakpoints come first in the set a target is given, so two set after an
    /// instruction breakpoint on a target holding two push it past the limit, and taking them away
    /// gives its comparator back: each is told in a `breakpoint` event.
    #[test]
    fn an_instruction_breakpoint_pushed_past_the_limit_and_back_is_told_both_times() {
        let (mut dbg, record) = capped_session(2);
        let instruction = answered(&set_instructions(&mut dbg, 2, &[4096]));
        assert_eq!(verified(&instruction), [true]);
        let out = set_lines(&mut dbg, 3, "a.cs", &[1, 2]);
        assert_eq!(verified(&answered(&out)), [true, true]);
        let changed = breakpoint_events(&out);
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert_eq!(changed[0]["id"], instruction[0]["id"]);
        assert_eq!(changed[0]["verified"], json!(false));
        assert!(
            changed[0]["message"].as_str().is_some_and(|m| m.contains("hardware breakpoints")),
            "{changed:?}"
        );
        assert_eq!(record.borrow().sets.last(), Some(&vec![1, 2, 4096]));
        let out = set_lines(&mut dbg, 4, "a.cs", &[]);
        let changed = breakpoint_events(&out);
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert_eq!(changed[0]["id"], instruction[0]["id"]);
        assert_eq!(changed[0]["verified"], json!(true));
        assert!(changed[0].get("message").is_none(), "an armed breakpoint carries no reason: {changed:?}");
    }

    /// **AN INSTRUCTION BREAKPOINT SENT BEFORE THE LAUNCH WAITS FOR THE TARGET, AS EVERY OTHER KIND
    /// DOES.** It is answered pending, with an id, and settled by a `breakpoint` event once the target
    /// has started: here inactive, as the two source breakpoints set before it fill the target.
    #[test]
    fn an_instruction_breakpoint_set_before_a_chosen_launch_is_settled_when_the_target_starts() {
        let (mut dbg, record, _) = chosen_session(false, false, Vec::new());
        record.borrow_mut().max = Some(2);
        dbg.handle(&request(1, "initialize", None));
        let source = answered(&set_lines(&mut dbg, 2, "a.cs", &[1, 2]));
        let instruction = answered(&set_instructions(&mut dbg, 3, &[4096]));
        assert_eq!(verified(&instruction), [false], "no target yet: {instruction:?}");
        assert_eq!(instruction[0]["reason"], json!("pending"));
        let started = dbg.handle(&request(4, "launch", Some(json!({}))));
        let changed = breakpoint_events(&started);
        let settled = |answer: &Json| {
            changed
                .iter()
                .find(|breakpoint| breakpoint["id"] == answer["id"])
                .cloned()
                .unwrap_or_else(|| panic!("no event settles {answer}: {changed:?}"))
        };
        assert_eq!(settled(&source[0])["verified"], json!(true));
        assert_eq!(settled(&source[1])["verified"], json!(true));
        assert_eq!(settled(&instruction[0])["verified"], json!(false), "past the two comparators");
        assert_eq!(record.borrow().sets.last(), Some(&vec![1, 2, 4096]));
    }

    /// The same instruction breakpoint before a launch that runs without debugging: settled unarmed,
    /// and never given to the target.
    #[test]
    fn an_instruction_breakpoint_set_before_a_chosen_launch_without_debugging_is_settled_unarmed() {
        let (mut dbg, record, _) = chosen_session(false, true, Vec::new());
        dbg.handle(&request(1, "initialize", None));
        let instruction = answered(&set_instructions(&mut dbg, 2, &[4096]));
        assert_eq!(verified(&instruction), [false]);
        let started = dbg.handle(&request(3, "launch", Some(json!({}))));
        let changed = breakpoint_events(&started);
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert_eq!(changed[0]["id"], instruction[0]["id"]);
        assert_eq!(changed[0]["verified"], json!(false));
        assert!(
            record.borrow().sets.iter().all(Vec::is_empty),
            "nothing reached the target: {:?}",
            record.borrow().sets
        );
    }

    /// A map of one pair, as MSBuild's `PathMap` gives it for a project: the project's folder on this
    /// computer, and the folder its build records in its place.
    fn project_map() -> PathMap {
        PathMap::new(vec![(String::from("C:\\w\\App\\"), String::from("/rhu/src/App/"))])
    }

    /// A path maps by its prefix both ways, ignoring case and which separator it is written with,
    /// and comes back in the separators of the side it is mapped to; a path no prefix starts, or that
    /// a prefix starts only partway into a folder's name, passes unchanged.
    #[test]
    fn a_path_maps_by_its_prefix_both_ways() {
        let map = project_map();
        assert_eq!(map.to_program("c:\\w\\app\\Sub\\Program.cs"), "/rhu/src/App/Sub/Program.cs");
        assert_eq!(map.to_program("C:/w/App/Program.cs"), "/rhu/src/App/Program.cs");
        assert_eq!(map.to_local("/rhu/src/App/Sub/Program.cs"), "C:\\w\\App\\Sub\\Program.cs");
        assert_eq!(map.to_program("C:\\w\\Apps\\Program.cs"), "C:\\w\\Apps\\Program.cs");
        assert_eq!(map.to_local("/usr/lib/runtime/Thread.cs"), "/usr/lib/runtime/Thread.cs");
        let first = PathMap::new(vec![
            (String::from("C:\\w\\App\\obj\\"), String::from("/rhu/obj/App/")),
            (String::from("C:\\w\\App\\"), String::from("/rhu/src/App/")),
        ]);
        assert_eq!(first.to_program("C:\\w\\App\\obj\\G.cs"), "/rhu/obj/App/G.cs", "the first pair that starts it");
    }

    /// **A BREAKPOINT IN A SOURCE WHOSE BUILD REWROTE ITS PATH** is resolved in the source as the
    /// program records it, and answered at the path the client has it at; a frame stopped in that
    /// source is shown at the client's path too.
    #[test]
    fn a_mapped_source_is_resolved_as_the_program_records_it_and_shown_as_the_client_has_it() {
        let record = std::rc::Rc::new(core::cell::RefCell::new(Record {
            recorded_file: Some(String::from("/rhu/src/App/Program.cs")),
            frame_at: Some(7),
            ..Record::default()
        }));
        let mut dbg = Debugger::with_backend(Box::new(Recorded { record: record.clone() }))
            .mapping_paths(project_map());
        dbg.handle(&request(1, "launch", None));
        let answers = answered(&set_lines(&mut dbg, 2, "c:\\w\\App\\Program.cs", &[7]));
        assert_eq!(record.borrow().documents.last().map(String::as_str), Some("/rhu/src/App/Program.cs"));
        assert_eq!(answers[0]["source"]["path"], json!("C:\\w\\App\\Program.cs"), "{answers:?}");
        let out = dbg.handle(&request(3, "stackTrace", Some(json!({ "threadId": 1 }))));
        let Some(Message::Response(trace)) = out.first() else {
            panic!("expected a response");
        };
        let frames = trace.body.as_ref().expect("a body")["stackFrames"].clone();
        assert_eq!(frames[0]["source"]["path"], json!("C:\\w\\App\\Program.cs"), "{frames}");
    }

    /// A session with no map, or a source its map does not cover, resolves and shows paths as they
    /// are.
    #[test]
    fn an_unmapped_source_is_resolved_and_shown_as_it_is() {
        let record = std::rc::Rc::new(core::cell::RefCell::new(Record {
            recorded_file: Some(String::from("C:\\w\\Lib\\Util.cs")),
            ..Record::default()
        }));
        let mut dbg = Debugger::with_backend(Box::new(Recorded { record: record.clone() }))
            .mapping_paths(project_map());
        dbg.handle(&request(1, "launch", None));
        let answers = answered(&set_lines(&mut dbg, 2, "C:\\w\\Lib\\Util.cs", &[3]));
        assert_eq!(record.borrow().documents.last().map(String::as_str), Some("C:\\w\\Lib\\Util.cs"));
        assert_eq!(answers[0]["source"]["path"], json!("C:\\w\\Lib\\Util.cs"));
    }

    /// The same breakpoint under a chosen launch that runs without debugging: nothing reaches the
    /// target, and nothing is counted as an inactive breakpoint of a session that arms none.
    #[test]
    fn a_breakpoint_set_before_a_chosen_launch_without_debugging_is_never_armed() {
        let (mut dbg, record, _) = chosen_session(false, true, Vec::new());
        dbg.handle(&request(1, "initialize", None));
        dbg.handle(&request(
            2,
            "setInstructionBreakpoints",
            Some(json!({ "breakpoints": [{ "instructionReference": "4096" }] })),
        ));
        dbg.handle(&request(3, "launch", Some(json!({ "noDebug": true }))));
        assert!(
            record.borrow().sets.iter().all(Vec::is_empty),
            "nothing is armed on the target: {:?}",
            record.borrow().sets
        );
        assert!(dbg.instruction_breakpoints.is_empty(), "and the session holds none");
    }
}
