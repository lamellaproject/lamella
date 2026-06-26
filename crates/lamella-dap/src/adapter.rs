//! The debug adapter: translates DAP requests into calls on a [`DebugBackend`] and
//! produces the responses and events DAP expects.

#[cfg(feature = "interpreter")]
use crate::interp_backend::InterpreterBackend;
use crate::protocol::{Event, Message, Request, Response};
use lamella_debug_backend::{DebugBackend, Scope, Stop};
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
    out_seq: i64,
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
            running: false,
            source_breakpoints: Vec::new(),
            instruction_breakpoints: Vec::new(),
            breakpoint_meta: Vec::new(),
            last_inactive_note: 0,
            out_seq: 0,
        }
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

    /// Handles one DAP request, returning the response followed by any events.
    pub fn handle(&mut self, request: &Request) -> Vec<Message> {
        let mut events: Vec<(&str, Option<Json>)> = Vec::new();
        let (success, body) = match request.command.as_str() {
            "initialize" => (true, Some(capabilities())),
            "launch" => {
                let launched = self.launch(request);
                events.push(("initialized", None));
                (launched, None)
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
            "disconnect" => (true, None),
            _ => (false, None),
        };

        let mut out = Vec::with_capacity(1 + events.len());
        out.push(self.response(request, success, body));
        for (event, body) in events {
            out.push(self.event(event, body));
        }
        out
    }

    fn launch(&mut self, request: &Request) -> bool {
        self.stop_on_entry = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("stopOnEntry"))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        self.launched = self.backend.launch();
        self.launched
    }

    /// Programs the backend with the union of source and instruction breakpoints. They share
    /// the backend's single comparator set, so both kinds must be re-applied together;
    /// applying only one would erase the other (VS Code sends an empty
    /// `setInstructionBreakpoints` right after `setBreakpoints`, which otherwise wipes them).
    fn apply_breakpoints(&mut self) {
        let mut all = self.source_breakpoints.clone();
        all.extend_from_slice(&self.instruction_breakpoints);
        self.backend.set_breakpoints(&all);
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
                    }
                    results.push(breakpoint);
                }
                None => results.push(json!({ "verified": false })),
            }
        }
        self.instruction_breakpoints = addresses;
        self.apply_breakpoints();
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
                    let line = self
                        .backend
                        .source_location(address)
                        .map_or(requested.unwrap_or(0), |location| u64::from(location.line));
                    let mut breakpoint = json!({
                        "verified": armed,
                        "line": line,
                        "instructionReference": address.to_string(),
                    });
                    if !armed {
                        breakpoint["message"] = json!(over_capacity_message(cap));
                    }
                    results.push(breakpoint);
                }
                None => results.push(json!({ "verified": false })),
            }
        }
        self.source_breakpoints = addresses;
        self.breakpoint_meta = meta;
        self.apply_breakpoints();
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
                self.source_step(action)
            }
            Action::StepIn => self.backend.step(),
            Action::StepOver => self.step_to_depth(|depth, start| depth <= start),
            Action::StepOut => self.step_to_depth(|depth, start| depth < start),
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
    fn step_to_depth(&mut self, reached: impl Fn(usize, usize) -> bool) -> Stop {
        let start = self.backend.depth();
        loop {
            match self.backend.step() {
                Stop::Done => break Stop::Done,
                Stop::Fault(message) => break Stop::Fault(message),
                Stop::Running => break Stop::Running,
                Stop::Breakpoint => break Stop::Breakpoint,
                _ if reached(self.backend.depth(), start) => break Stop::Step,
                _ => {}
            }
        }
    }

    /// Single-steps to the next source statement (sequence point) at the call depth the
    /// step implies: `stepIn` stops at the next boundary anywhere (descending into a
    /// call), `next` at the next boundary in this frame or a caller (running a called
    /// method to completion), `stepOut` at the next boundary after the current method
    /// returns. Used when the backend has source info; otherwise stepping is per-CIL-op.
    fn source_step(&mut self, action: Action) -> Stop {
        if matches!(action, Action::StepOut) {
            if let Some(stop) = self.backend.step_out() {
                return stop;
            }
        }
        let start = self.backend.depth();
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
                events.push(("exited", Some(json!({ "exitCode": 0 }))));
                events.push(("terminated", None));
            }
            Stop::Running => {}
            Stop::Fault(message) => {
                events.push((
                    "output",
                    Some(json!({ "category": "stderr", "output": format!("{message}\n") })),
                ));
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
    }

    fn stack_trace(&self) -> Json {
        let frames = self.backend.stack();
        let mut out = Vec::with_capacity(frames.len());
        for (index, frame) in frames.iter().enumerate().rev() {
            let source = self.backend.source_location(frame.address);
            let mut entry = json!({
                "id": index,
                "name": frame.name,
                "line": source.as_ref().map_or(frame.line, |location| location.line),
                "column": source.as_ref().map_or(1, |location| location.column),
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

    fn event(&mut self, event: &str, body: Option<Json>) -> Message {
        self.out_seq += 1;
        Message::Event(Event {
            seq: self.out_seq,
            event: event.to_owned(),
            body,
        })
    }
}

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

fn capabilities() -> Json {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsInstructionBreakpoints": true,
        "supportsDisassembleRequest": true,
        "supportsSetVariable": true,
    })
}

fn arg_u32(request: &Request, field: &str) -> u32 {
    request
        .arguments
        .as_ref()
        .and_then(|args| args.get(field))
        .and_then(Json::as_u64)
        .unwrap_or(0) as u32
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
        let write_line = module.add_intrinsic(0, lamella_cil_runtime::intrinsics::console_write_line, 1);
        let write_line_token = Token(0x0A00_0001);
        module.bind_token(0, write_line_token, write_line);
        let hi: Vec<u16> = "hi".encode_utf16().collect();
        let string_token = Token(0x7000_0001);
        module.bind_string(0, string_token, &hi);
        let main = module.add_method(
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
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "exited"))
        );
        assert!(
            out.iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "terminated"))
        );
        assert_eq!(dbg.output_string(), "hi\n");
    }

    use lamella_debug_backend::{Disassembled, Frame, Register, Variable};

    /// A minimal backend for capacity tests: it resolves source line N to the opaque address
    /// N and reports a fixed hardware-breakpoint limit. Everything else is an inert stub.
    struct CapBackend {
        max: usize,
    }

    impl DebugBackend for CapBackend {
        fn launch(&mut self) -> bool {
            true
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
        fn set_breakpoints(&mut self, _addresses: &[u64]) {}
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
        fn launch(&mut self) -> bool {
            true
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
        fn set_breakpoints(&mut self, addresses: &[u64]) {
            self.address = addresses.first().copied().unwrap_or(0);
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
    fn source_breakpoints_past_the_hardware_limit_are_unverified_with_a_message() {
        let mut dbg = Debugger::with_backend(Box::new(CapBackend { max: 2 }));
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
        let mut dbg = Debugger::with_backend(Box::new(CapBackend { max: 2 }));
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

        let scopes = dbg.handle(&request(5, "scopes", Some(json!({ "frameId": 1 }))));
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
        let scopes = dbg.handle(&request(5, "scopes", Some(json!({ "frameId": 1 }))));
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
        let main = module.add_method(
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
        let add = module.add_method(
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
        let main = module.add_method(
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
        assert_eq!(trace[0].response_body()["stackFrames"][0]["line"], json!(3));

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
