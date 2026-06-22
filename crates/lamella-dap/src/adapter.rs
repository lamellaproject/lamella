//! The debug adapter: translates DAP requests into calls on a [`DebugBackend`] and
//! produces the responses and events DAP expects.

#[cfg(feature = "interpreter")]
use crate::interp_backend::InterpreterBackend;
use crate::protocol::{Event, Message, Request, Response};
use lamella_debug_backend::{DebugBackend, Scope, Stop};
#[cfg(feature = "interpreter")]
use lamella_ves::Module;
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
    out_seq: i64,
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
                    addresses.push(address);
                    results.push(
                        json!({ "verified": true, "instructionReference": address.to_string() }),
                    );
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
        let mut addresses = Vec::new();
        let mut results = Vec::new();
        for spec in &specs {
            let requested = spec.get("line").and_then(Json::as_u64);
            let address = requested
                .and_then(|line| u32::try_from(line).ok())
                .and_then(|line| self.backend.resolve_source_breakpoint(document, line));
            match address {
                Some(address) => {
                    addresses.push(address);
                    let line = self
                        .backend
                        .source_location(address)
                        .map_or(requested.unwrap_or(0), |location| u64::from(location.line));
                    results.push(json!({
                        "verified": true,
                        "line": line,
                        "instructionReference": address.to_string(),
                    }));
                }
                None => results.push(json!({ "verified": false })),
            }
        }
        self.source_breakpoints = addresses;
        self.apply_breakpoints();
        json!({ "breakpoints": results })
    }

    /// Runs the target for one command and emits the resulting output + stop/terminate
    /// events.
    fn run(&mut self, action: Action, events: &mut Vec<(&'static str, Option<Json>)>) {
        let stop = match action {
            Action::Resume => self.backend.resume(),
            Action::StepIn | Action::StepOver | Action::StepOut if self.backend.has_source() => {
                self.source_step(action)
            }
            Action::StepIn => self.backend.step(),
            Action::StepOver => self.step_to_depth(|depth, start| depth <= start),
            Action::StepOut => self.step_to_depth(|depth, start| depth < start),
        };
        if matches!(stop, Stop::Running) {
            self.running = true;
            self.flush_output(events);
        } else {
            self.finish(stop, events);
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

    fn event(&mut self, event: &str, body: Option<Json>) -> Message {
        self.out_seq += 1;
        Message::Event(Event {
            seq: self.out_seq,
            event: event.to_owned(),
            body,
        })
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
        let write_line = module.add_intrinsic(0, lamella_ves::intrinsics::console_write_line, 1);
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
