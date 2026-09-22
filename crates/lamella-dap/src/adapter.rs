//! The debug adapter: translates DAP requests into calls on a [`DebugBackend`] and
//! produces the responses and events DAP expects.

#[cfg(feature = "interpreter")]
use crate::interp_backend::InterpreterBackend;
use crate::frame_eval;
use crate::protocol::{Event, Message, Request, Response};
use lamella_debug_backend::{DebugBackend, Scope, Stop, Variable};
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
    /// Whether a resume-now backend left the target running, so the serve loop polls for
    /// the async stop. Always false for the synchronous interpreter backend.
    running: bool,
    /// Source and instruction breakpoints, kept apart so each `setBreakpoints` /
    /// `setInstructionBreakpoints` updates only its own kind. The backend has a single
    /// breakpoint set, so the *union* is what gets programmed -- otherwise an empty
    /// `setInstructionBreakpoints` (which VS Code sends right after `setBreakpoints`) would
    /// wipe the source breakpoints.
    source_breakpoints: Vec<u64>,
    instruction_breakpoints: Vec<u64>,
    /// Per-source-breakpoint hit-count / logpoint behavior, by code address. Rebuilt on each
    /// `setBreakpoints`; an address with no entry is an ordinary breakpoint that always stops.
    breakpoint_meta: Vec<BreakpointMeta>,
    /// The inactive (over-capacity) breakpoint count last reported to the user, so the
    /// run-time "N inactive" note fires only when that count changes -- not on every continue.
    last_inactive_note: usize,
    /// Whether the session has already said in the console that the frame it was asked about
    /// reports no variables, so the note is made once rather than on every hover.
    noted_absent_variables: bool,
    out_seq: i64,
    /// The Debug Console REPL, built on the first `evaluate` so a session that never uses the
    /// console pays nothing. See [`crate::repl_eval`].
    #[cfg(feature = "interpreter")]
    repl: Option<crate::repl_eval::ReplCell>,
}

/// Per-breakpoint behavior for a source breakpoint: a hit-count condition and/or a logpoint
/// message, with the running hit count. Both fields empty means an ordinary breakpoint that
/// always stops.
struct BreakpointMeta {
    address: u64,
    hit_condition: Option<String>,
    log_message: Option<String>,
    hits: u32,
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
            running: false,
            source_breakpoints: Vec::new(),
            instruction_breakpoints: Vec::new(),
            breakpoint_meta: Vec::new(),
            last_inactive_note: 0,
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

    /// All console output the program has produced and the adapter has forwarded.
    #[must_use]
    pub fn output_string(&self) -> &str {
        &self.output
    }

    /// Whether a resume-now backend left the target running, so the serve loop should
    /// [`poll`](Debugger::poll) for the eventual stop. Always false for the synchronous
    /// interpreter backend (which finishes inside the request that resumed it).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Polls a target a resume-now backend left running: the output + stop events once
    /// it halts, or empty while it is still going. The serve loop calls this while
    /// [`is_running`](Debugger::is_running) holds; the backend's `poll` may block until
    /// the target halts, so this does not busy-wait.
    #[must_use]
    pub fn poll(&mut self) -> Vec<Message> {
        let mut events: Vec<(&'static str, Option<Json>)> = Vec::new();
        if self.running {
            match self.backend.poll() {
                Stop::Running => self.flush_output(&mut events),
                stop => {
                    self.running = false;
                    self.finish(stop, &mut events);
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
    /// # Errors
    /// The backend's reason, when the target could not be released and may still be stopped.
    pub fn release_target(&mut self) -> Result<(), String> {
        self.running = false;
        self.backend.release()
    }

    /// Handles one DAP request, returning the response followed by any events.
    pub fn handle(&mut self, request: &Request) -> Vec<Message> {
        let mut events: Vec<(&str, Option<Json>)> = Vec::new();
        let (success, body) = match request.command.as_str() {
            "initialize" => (true, Some(capabilities())),
            "launch" => {
                if let Some(reason) = self.refusal.clone() {
                    return self.fail_for_user(request, ERROR_SESSION_REFUSED, &reason);
                }
                let launched = self.launch(request);
                self.flush_output(&mut events);
                if let Err(reason) = launched {
                    let mut out: Vec<Message> =
                        events.into_iter().map(|(event, body)| self.event(event, body)).collect();
                    out.extend(self.fail_for_user(
                        request,
                        ERROR_LAUNCH_FAILED,
                        &format!("The session could not start: {reason}"),
                    ));
                    return out;
                }
                events.push(("initialized", None));
                (true, None)
            }
            "configurationDone" => {
                if self.stop_on_entry {
                    events.push(("stopped", Some(stopped("entry"))));
                } else {
                    self.run(Action::Resume, &mut events);
                }
                (true, None)
            }
            "threads" => (
                true,
                Some(json!({ "threads": [{ "id": 1, "name": "main" }] })),
            ),
            "stackTrace" => (true, Some(self.stack_trace())),
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
                self.run(Action::StepIn, &mut events);
                (true, None)
            }
            "next" => {
                self.run(Action::StepOver, &mut events);
                (true, None)
            }
            "stepOut" => {
                self.run(Action::StepOut, &mut events);
                (true, None)
            }
            "pause" => {
                if self.running {
                    self.backend.pause();
                    self.running = false;
                }
                if self.launched {
                    events.push(("stopped", Some(stopped("pause"))));
                }
                (true, None)
            }
            "setBreakpoints" => (true, Some(self.set_source_breakpoints(request))),
            "setInstructionBreakpoints" => (true, Some(self.set_instruction_breakpoints(request))),
            "disassemble" => (true, Some(self.disassemble(request))),
            "evaluate" => return self.evaluate(request),
            "disconnect" => match self.release_target() {
                Ok(()) => (true, None),
                Err(reason) => {
                    return self.fail_for_user(
                        request,
                        ERROR_TARGET_NOT_RELEASED,
                        &format!(
                            "The session has ended, but the target could not be released and may \
                             still be stopped: {reason}"
                        ),
                    );
                }
            },
            _ => (false, None),
        };

        let mut out = Vec::with_capacity(1 + events.len());
        out.push(self.response(request, success, body));
        for (event, body) in events {
            out.push(self.event(event, body));
        }
        out
    }

    fn launch(&mut self, request: &Request) -> Result<(), String> {
        self.stop_on_entry = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("stopOnEntry"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        let launched = self.backend.launch();
        self.launched = launched.is_ok();
        launched
    }

    /// Programs the backend with the union of source and instruction breakpoints. They share
    /// the backend's single comparator set, so both kinds must be re-applied together;
    /// applying only one would erase the other (VS Code sends an empty
    /// `setInstructionBreakpoints` right after `setBreakpoints`, which otherwise wipes them).
    fn apply_breakpoints(&mut self) -> Result<(), String> {
        let mut all = self.source_breakpoints.clone();
        all.extend_from_slice(&self.instruction_breakpoints);
        self.backend.set_breakpoints(&all)
    }

    /// Replaces the instruction breakpoints. Each is an opaque code address carried in
    /// the `instructionReference` (the backend interprets it); no source mapping needed.
    fn set_instruction_breakpoints(&mut self, request: &Request) -> Json {
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
        for spec in &specs {
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
                        json!({ "verified": armed, "instructionReference": address.to_string() });
                    if !armed {
                        breakpoint["message"] = json!(over_capacity_message(cap));
                        breakpoint["reason"] = json!(REASON_PENDING);
                    }
                    results.push(breakpoint);
                }
                None => results.push(unverified(
                    REASON_FAILED,
                    String::from(
                        "This breakpoint's instruction reference is not an address. A client sends                          one it was given by a stack frame or a disassembly.",
                    ),
                )),
            }
        }
        self.instruction_breakpoints = addresses;
        if let Err(reason) = self.apply_breakpoints() {
            withdraw_verification(&mut results, &reason);
        }
        json!({ "breakpoints": results })
    }

    /// Replaces the source breakpoints. Each is a `line` in the request's `source`,
    /// resolved to a code address by the backend's source mapping; a line with no code
    /// resolves to nothing and is reported unverified.
    fn set_source_breakpoints(&mut self, request: &Request) -> Json {
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
        let mut addresses = Vec::new();
        let mut meta = Vec::new();
        let mut results = Vec::new();
        for spec in &specs {
            let requested = spec.get("line").and_then(Json::as_u64);
            let address = requested
                .and_then(|line| u32::try_from(line).ok())
                .and_then(|line| self.backend.resolve_source_breakpoint(document, line));
            match address {
                Some(address) => {
                    let armed = cap.map_or(true, |max| addresses.len() < max);
                    addresses.push(address);
                    meta.push(BreakpointMeta {
                        address,
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
                    });
                    let located = self.backend.source_location(address);
                    let (line, file) = match &located {
                        Some(location) => (u64::from(location.line), location.file.as_str()),
                        None => (requested.unwrap_or(0), document),
                    };
                    let mut breakpoint = json!({
                        "verified": armed,
                        "line": line,
                        "instructionReference": address.to_string(),
                    });
                    if !file.is_empty() {
                        breakpoint["source"] = json!({ "path": file });
                    }
                    if !armed {
                        breakpoint["message"] = json!(over_capacity_message(cap));
                        breakpoint["reason"] = json!(REASON_PENDING);
                    }
                    results.push(breakpoint);
                }
                None if !self.backend.has_source() => results.push(unverified(
                    REASON_PENDING,
                    String::from(
                        "No source mapping is loaded yet, so this line cannot be resolved. It                          binds once the program is running.",
                    ),
                )),
                None => results.push(unverified(
                    REASON_FAILED,
                    match requested {
                        Some(line) => format!(
                            "There is no code on line {line} of this file, so no breakpoint can be                              set there. Move it to a statement."
                        ),
                        None => String::from("This breakpoint names no line."),
                    },
                )),
            }
        }
        self.source_breakpoints = addresses;
        self.breakpoint_meta = meta;
        if let Err(reason) = self.apply_breakpoints() {
            withdraw_verification(&mut results, &reason);
        }
        json!({ "breakpoints": results })
    }

    /// Runs the target for one command and emits the resulting output + stop/terminate
    /// events.
    fn run(&mut self, action: Action, events: &mut Vec<(&'static str, Option<Json>)>) {
        if matches!(action, Action::Resume) {
            self.note_inactive_breakpoints(events);
        }
        let mut stop = match action {
            Action::Resume => self.backend.resume(),
            Action::StepIn | Action::StepOver | Action::StepOut if self.backend.has_source() => {
                self.source_step(action, events)
            }
            Action::StepIn => self.backend.step(),
            Action::StepOver | Action::StepOut if !self.backend.tracks_depth() => {
                self.backend.step()
            }
            Action::StepOver => self.step_to_depth(|depth, start| depth <= start, events),
            Action::StepOut => self.step_to_depth(|depth, start| depth < start, events),
        };
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
        let total = self.source_breakpoints.len() + self.instruction_breakpoints.len();
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
                         breakpoints; {inactive} inactive (shown greyed).\n"
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
    fn step_to_depth(
        &mut self,
        reached: impl Fn(usize, usize) -> bool,
        events: &mut Vec<(&'static str, Option<Json>)>,
    ) -> Stop {
        let start = self.backend.depth();
        let mut budget = self.backend.step_budget().max(1);
        loop {
            match self.backend.step() {
                Stop::Done => break Stop::Done,
                Stop::Fault(message) => break Stop::Fault(message),
                Stop::Running => break Stop::Running,
                Stop::Breakpoint => break Stop::Breakpoint,
                _ if reached(self.backend.depth(), start) => break Stop::Step,
                _ => {
                    budget -= 1;
                    if budget == 0 {
                        break self.give_up_stepping(
                            "the call being stepped over has not returned",
                            events,
                        );
                    }
                }
            }
        }
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
                    "[lamella] Step gave up after {budget} instructions: {waiting_for}. \
                     Execution is most likely in code with no line information -- a runtime \
                     helper, or past the end of the program. Stopped where it is; use Continue, \
                     or set a breakpoint where you want to land.\n"
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
        let start = self.backend.depth();
        let mut budget = self.backend.step_budget().max(1);
        loop {
            match self.backend.step() {
                Stop::Done => break Stop::Done,
                Stop::Fault(message) => break Stop::Fault(message),
                Stop::Running => break Stop::Running,
                Stop::Breakpoint => break Stop::Breakpoint,
                _ => {
                    if matches!(action, Action::StepOver | Action::StepOut)
                        && self.backend.depth() > start
                    {
                        match self.backend.run_to_return() {
                            Stop::Step => {}
                            other => break other,
                        }
                    }
                    let reached = match action {
                        Action::StepIn | Action::Resume => true,
                        Action::StepOver => self.backend.depth() <= start,
                        Action::StepOut => self.backend.depth() < start,
                    };
                    if reached && self.backend.at_source_boundary() {
                        break Stop::Step;
                    }
                    budget -= 1;
                    if budget == 0 {
                        break self
                            .give_up_stepping("no source statement was reached", events);
                    }
                }
            }
        }
    }

    /// Flushes new program output as an `output` event, then emits the event for the
    /// stop -- so console output precedes the stop or terminate it accompanies.
    fn finish(&mut self, stop: Stop, events: &mut Vec<(&'static str, Option<Json>)>) {
        self.flush_output(events);
        match stop {
            Stop::Breakpoint => events.push(("stopped", Some(stopped("breakpoint")))),
            Stop::Step => events.push(("stopped", Some(stopped("step")))),
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
                events.push(("exited", Some(json!({ "exitCode": -1 }))));
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

    fn stack_trace(&self) -> Json {
        let frames = self.backend.stack();
        let mut out = Vec::with_capacity(frames.len());
        for (index, frame) in frames.iter().enumerate() {
            let source = self.backend.source_location(frame.address);
            let mut entry = json!({
                "id": index,
                "name": frame.name,
                "line": source.as_ref().map_or(0, |location| location.line),
                "column": source.as_ref().map_or(0, |location| location.column),
                "instructionPointerReference": frame.address.to_string(),
            });
            if let Some(location) = &source {
                entry["source"] = json!({ "path": location.file.clone() });
                entry["endLine"] = json!(location.end_line);
                entry["endColumn"] = json!(location.end_column);
            }
            out.push(entry);
        }
        let total = out.len();
        json!({ "stackFrames": out, "totalFrames": total })
    }

    fn scopes(&self, frame_id: u32) -> Json {
        let reference = |kind: u32| frame_id * 3 + kind + 1;
        json!({ "scopes": [
            { "name": "Arguments", "variablesReference": reference(0), "expensive": false },
            { "name": "Locals",    "variablesReference": reference(1), "expensive": false },
            { "name": "Stack",     "variablesReference": reference(2), "expensive": false },
        ]})
    }

    fn variables(&self, reference: u32) -> Json {
        if reference == 0 {
            return json!({ "variables": [] });
        }
        let frame_index = ((reference - 1) / 3) as usize;
        let scope = match (reference - 1) % 3 {
            0 => Scope::Arguments,
            1 => Scope::Locals,
            _ => Scope::Stack,
        };
        let variables: Vec<Json> = self
            .backend
            .variables(frame_index, scope)
            .iter()
            .map(|variable| {
                json!({
                    "name": variable.name,
                    "value": variable.value,
                    "type": variable.kind,
                    "variablesReference": 0,
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
        match frame_eval::route(arg_opt_u32(request, "frameId"), arg_str(request, "context")) {
            frame_eval::Target::Frame { index, room } => {
                let visible = self.frame_variables(index);
                match frame_eval::resolve(&visible, &expression) {
                    Ok(variable) => {
                        let body = json!({
                            "result": variable.value,
                            "type": variable.kind,
                            "variablesReference": 0,
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
    fn frame_variables(&self, index: usize) -> Vec<Variable> {
        let mut visible = self.backend.variables(index, Scope::Locals);
        visible.extend(self.backend.variables(index, Scope::Arguments));
        visible
    }

    /// Answers an unscoped Debug Console line from the debugger's own evaluation session.
    #[cfg(feature = "interpreter")]
    fn session_evaluate(&mut self, request: &Request, expression: &str) -> Vec<Message> {
        let body = crate::repl_eval::evaluate(&mut self.repl, expression);
        vec![self.response(request, true, Some(body))]
    }

    /// Answers an unscoped Debug Console line in a build that carries no evaluation session of its
    /// own -- a server whose target is a board, where a submission evaluated here would run in this
    /// process rather than on the thing being debugged.
    ///
    /// The refusal names the two ways to read the target's own values, because a console line is
    /// usually someone reaching for a value they can have: a refusal that only says no leaves them
    /// with the impression that the session cannot show them anything.
    #[cfg(not(feature = "interpreter"))]
    fn session_evaluate(&mut self, request: &Request, expression: &str) -> Vec<Message> {
        let _ = expression;
        self.fail(
            request,
            "This debug server evaluates nothing of its own: a submission typed here would run \
             in this process rather than on the target being debugged. Hover a variable, or add \
             its name to Watch, to read it from the paused frame.",
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
        if reference == 0 {
            return None;
        }
        let frame_index = ((reference - 1) / 3) as usize;
        let scope = match (reference - 1) % 3 {
            0 => Scope::Arguments,
            1 => Scope::Locals,
            _ => Scope::Stack,
        };
        let name = arg_str(request, "name");
        let value = arg_str(request, "value");
        self.backend.set_variable(frame_index, scope, name, value)
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

fn stopped(reason: &str) -> Json {
    json!({ "reason": reason, "threadId": 1, "allThreadsStopped": true })
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

/// What this adapter tells the client it can do, in the `initialize` response.
///
/// A client asks for nothing it has not been told about, so a capability left out here is a
/// feature that never gets exercised and leaves no trace of why: `supportsEvaluateForHovers` is
/// what makes an editor send an `evaluate` when the pointer rests on a variable, and without it
/// no hover request is made, no hover appears, and the server sees no request to explain it.
fn capabilities() -> Json {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsInstructionBreakpoints": true,
        "supportsDisassembleRequest": true,
        "supportsSetVariable": true,
        "supportsEvaluateForHovers": true,
    })
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

    /// A backend whose program FAULTS on resume -- the trap case.
    struct FaultingBackend;

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
        let mut dbg = Debugger::with_backend(Box::new(FaultingBackend));
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
}
