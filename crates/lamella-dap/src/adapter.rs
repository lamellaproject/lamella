//! The debug adapter: translates DAP requests into actions on a
//! [`lamella_ves::Session`] and produces the responses and events DAP expects.

use crate::protocol::{Event, Message, Request, Response};
use lamella_ves::{Module, Status, Value, Vm};
use serde_json::{Value as Json, json};

/// A debug session over one program: the module being debugged, the runtime
/// context, and the running [`lamella_ves::Session`] once launched.
pub struct Debugger<'a> {
    module: &'a Module,
    entry: u32,
    vm: Vm,
    session: Option<lamella_ves::Session<'a>>,
    /// The current instruction breakpoints, as `(method, instruction)`. Held here
    /// so they survive across `launch` (which creates the session) and re-apply.
    breakpoints: Vec<(u32, u32)>,
    out_seq: i64,
}

impl<'a> Debugger<'a> {
    /// Creates a debugger for `module`, whose program entry point is `entry`.
    ///
    /// (A real `launch` would load the program named in its arguments; until the
    /// metadata reader lands, the program is the hand-built module supplied here.)
    #[must_use]
    pub fn new(module: &'a Module, entry: u32) -> Debugger<'a> {
        Debugger {
            module,
            entry,
            vm: Vm::new(),
            session: None,
            breakpoints: Vec::new(),
            out_seq: 0,
        }
    }

    /// The runtime context, for inspecting console output after a run.
    #[must_use]
    pub fn vm(&self) -> &Vm {
        &self.vm
    }

    /// Handles one DAP request, returning the response followed by any events.
    pub fn handle(&mut self, request: &Request) -> Vec<Message> {
        let mut events: Vec<(&str, Option<Json>)> = Vec::new();
        let (success, body) = match request.command.as_str() {
            "initialize" => {
                events.push(("initialized", None));
                (true, Some(capabilities()))
            }
            "launch" => (self.launch(), None),
            "configurationDone" => (true, None),
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
                self.resume(&mut events);
                (true, Some(json!({ "allThreadsContinued": true })))
            }
            "stepIn" => {
                self.step_in(&mut events);
                (true, None)
            }
            "next" => {
                self.step_over(&mut events);
                (true, None)
            }
            "stepOut" => {
                self.step_out(&mut events);
                (true, None)
            }
            "setBreakpoints" => (true, Some(unverified_breakpoints(request))),
            "setInstructionBreakpoints" => (true, Some(self.set_instruction_breakpoints(request))),
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

    fn launch(&mut self) -> bool {
        match lamella_ves::Session::new(self.module, self.entry, Vec::new()) {
            Ok(session) => {
                self.session = Some(session);
                self.apply_breakpoints();
                true
            }
            Err(_) => false,
        }
    }

    /// Replaces the program's instruction breakpoints. Each breakpoint addresses a
    /// CIL instruction by `(method, instruction)`, encoded in the
    /// `instructionReference` (see [`encode_address`]); no source mapping needed.
    fn set_instruction_breakpoints(&mut self, request: &Request) -> Json {
        let specs = request
            .arguments
            .as_ref()
            .and_then(|args| args.get("breakpoints"))
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        let mut parsed = Vec::new();
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
                    let (method, instruction) = decode_address(address);
                    parsed.push((method, instruction));
                    results.push(json!({ "verified": true, "instructionReference": alloc_int(address as i64) }));
                }
                None => results.push(json!({ "verified": false })),
            }
        }
        self.breakpoints = parsed;
        self.apply_breakpoints();
        json!({ "breakpoints": results })
    }

    fn apply_breakpoints(&mut self) {
        let Debugger {
            session,
            breakpoints,
            ..
        } = self;
        if let Some(session) = session.as_mut() {
            session.clear_breakpoints();
            for (method, instruction) in breakpoints.iter() {
                session.add_breakpoint(*method, *instruction);
            }
        }
    }

    fn resume(&mut self, events: &mut Vec<(&'static str, Option<Json>)>) {
        let Debugger { session, vm, .. } = self;
        let Some(session) = session.as_mut() else {
            return;
        };
        if session.is_at_breakpoint() {
            match session.step(vm) {
                Ok(Status::Done(_)) => return terminate(events),
                Ok(_) => {}
                Err(trap) => return fault(events, &trap),
            }
        }
        match session.resume(vm) {
            Ok(Status::Paused) => events.push(("stopped", Some(stopped("breakpoint")))),
            Ok(Status::Done(_)) => terminate(events),
            Ok(Status::Running) => {}
            Err(trap) => fault(events, &trap),
        }
    }

    /// `stepIn`: execute exactly one instruction, descending into any call.
    fn step_in(&mut self, events: &mut Vec<(&'static str, Option<Json>)>) {
        let Debugger { session, vm, .. } = self;
        let Some(session) = session.as_mut() else {
            return;
        };
        match session.step(vm) {
            Ok(Status::Done(_)) => terminate(events),
            Ok(Status::Running | Status::Paused) => events.push(("stopped", Some(stopped("step")))),
            Err(trap) => fault(events, &trap),
        }
    }

    /// `next`: step one instruction, but run any call it makes to completion
    /// (stop once the stack is no deeper than it began).
    fn step_over(&mut self, events: &mut Vec<(&'static str, Option<Json>)>) {
        let Debugger { session, vm, .. } = self;
        let Some(session) = session.as_mut() else {
            return;
        };
        let target_depth = session.depth();
        loop {
            match session.step(vm) {
                Ok(Status::Done(_)) => return terminate(events),
                Ok(Status::Running | Status::Paused) => {
                    if session.depth() <= target_depth {
                        return events.push(("stopped", Some(stopped("step"))));
                    }
                }
                Err(trap) => return fault(events, &trap),
            }
        }
    }

    /// `stepOut`: run until the current method returns to its caller.
    fn step_out(&mut self, events: &mut Vec<(&'static str, Option<Json>)>) {
        let Debugger { session, vm, .. } = self;
        let Some(session) = session.as_mut() else {
            return;
        };
        let target_depth = session.depth();
        loop {
            match session.step(vm) {
                Ok(Status::Done(_)) => return terminate(events),
                Ok(Status::Running | Status::Paused) => {
                    if session.depth() < target_depth {
                        return events.push(("stopped", Some(stopped("step"))));
                    }
                }
                Err(trap) => return fault(events, &trap),
            }
        }
    }

    fn stack_trace(&self) -> Json {
        let Some(session) = self.session.as_ref() else {
            return json!({ "stackFrames": [], "totalFrames": 0 });
        };
        let mut frames = Vec::new();
        for index in (0..session.depth()).rev() {
            if let Some(frame) = session.frame(index) {
                frames.push(json!({
                    "id": index,
                    "name": alloc_method_name(frame.method, frame.ip),
                    "line": frame.ip + 1,
                    "column": 1,
                }));
            }
        }
        let total = frames.len();
        json!({ "stackFrames": frames, "totalFrames": total })
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
        let Some(session) = self.session.as_ref() else {
            return json!({ "variables": [] });
        };
        if reference == 0 {
            return json!({ "variables": [] });
        }
        let frame_index = ((reference - 1) / 3) as usize;
        let kind = (reference - 1) % 3;
        let Some(frame) = session.frame(frame_index) else {
            return json!({ "variables": [] });
        };
        let (slot_prefix, values) = match kind {
            0 => ("arg", frame.args),
            1 => ("local", frame.locals),
            _ => ("stack", frame.stack),
        };
        let variables: Vec<Json> = values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let (text, kind) = format_value(&self.vm, *value);
                json!({
                    "name": alloc_slot_name(slot_prefix, index),
                    "value": text,
                    "type": kind,
                    "variablesReference": 0,
                })
            })
            .collect();
        json!({ "variables": variables })
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

fn terminate(events: &mut Vec<(&'static str, Option<Json>)>) {
    events.push(("exited", Some(json!({ "exitCode": 0 }))));
    events.push(("terminated", None));
}

fn fault(events: &mut Vec<(&'static str, Option<Json>)>, trap: &lamella_ves::Trap) {
    events.push((
        "output",
        Some(json!({ "category": "stderr", "output": alloc_trap_line(trap) })),
    ));
    events.push(("terminated", None));
}

fn stopped(reason: &str) -> Json {
    json!({ "reason": reason, "threadId": 1, "allThreadsStopped": true })
}

fn capabilities() -> Json {
    json!({
        "supportsConfigurationDoneRequest": true,
        "supportsInstructionBreakpoints": true,
    })
}

/// Encodes a CIL location `(method, instruction)` as a single instruction address
/// for DAP: the method in the high 32 bits, the instruction index in the low 32.
#[must_use]
pub fn encode_address(method: lamella_ves::MethodId, instruction: u32) -> u64 {
    (u64::from(method) << 32) | u64::from(instruction)
}

/// The inverse of [`encode_address`].
#[must_use]
pub fn decode_address(address: u64) -> (u32, u32) {
    ((address >> 32) as u32, (address & 0xFFFF_FFFF) as u32)
}

fn unverified_breakpoints(request: &Request) -> Json {
    let count = request
        .arguments
        .as_ref()
        .and_then(|args| args.get("breakpoints"))
        .and_then(Json::as_array)
        .map_or(0, Vec::len);
    let breakpoints: Vec<Json> = (0..count)
        .map(|_| json!({ "verified": false, "message": "source breakpoints await CIL-to-source mapping" }))
        .collect();
    json!({ "breakpoints": breakpoints })
}

fn format_value(vm: &Vm, value: Value) -> (String, String) {
    match value {
        Value::Int32(n) => (alloc_int(i64::from(n)), "int".to_owned()),
        Value::Int64(n) => (alloc_int(n), "long".to_owned()),
        Value::NativeInt(n) => (alloc_int(n), "native int".to_owned()),
        Value::Float(f) => (alloc_float(f), "double".to_owned()),
        Value::Object(reference) => match vm.heap().as_string(reference) {
            Some(chars) => (alloc_quoted(chars), "string".to_owned()),
            None => ("object".to_owned(), "object".to_owned()),
        },
        Value::Null => ("null".to_owned(), "object".to_owned()),
    }
}

fn arg_u32(request: &Request, field: &str) -> u32 {
    request
        .arguments
        .as_ref()
        .and_then(|args| args.get(field))
        .and_then(Json::as_u64)
        .unwrap_or(0) as u32
}


fn alloc_method_name(method: lamella_ves::MethodId, ip: u32) -> String {
    format!("method#{method} @{ip}")
}

fn alloc_slot_name(prefix: &str, index: usize) -> String {
    format!("{prefix}{index}")
}

fn alloc_int(value: i64) -> String {
    value.to_string()
}

fn alloc_float(value: f64) -> String {
    value.to_string()
}

fn alloc_quoted(chars: &[u16]) -> String {
    format!("\"{}\"", String::from_utf16_lossy(chars))
}

fn alloc_trap_line(trap: &lamella_ves::Trap) -> String {
    format!("{trap}\n")
}

#[cfg(test)]
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
        let write_line = module.add_intrinsic(lamella_ves::intrinsics::console_write_line, 1);
        let write_line_token = Token(0x0A00_0001);
        module.bind_token(write_line_token, write_line);
        let hi: Vec<u16> = "hi".encode_utf16().collect();
        let string_token = Token(0x7000_0001);
        module.bind_string(string_token, &hi);
        let main = module.add_method(
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
    fn initialize_reports_capabilities_and_the_initialized_event() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(&module, main);
        let out = dbg.handle(&request(1, "initialize", None));
        assert_eq!(out.len(), 2);
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
        assert!(matches!(&out[1], Message::Event(e) if e.event == "initialized"));
    }

    #[test]
    fn launch_then_continue_runs_to_termination() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(&module, main);
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
        assert_eq!(dbg.vm().output_string(), "hi\n");
    }

    #[test]
    fn stepping_emits_stopped_then_inspects_locals_and_stack() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(&module, main);
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
        let mut dbg = Debugger::new(&module, main);
        let out = dbg.handle(&request(1, "unheardOf", None));
        assert!(matches!(&out[0], Message::Response(r) if !r.success));
    }

    #[test]
    fn set_breakpoints_reports_unverified_pending_source_mapping() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(&module, main);
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
            body(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            2,
        );
        let add_token = Token(0x0600_0002);
        module.bind_token(add_token, add);
        let main = module.add_method(
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
        let mut dbg = Debugger::new(&module, main);
        dbg.handle(&request(1, "launch", None));
        dbg.handle(&request(2, "stepIn", None));
        dbg.handle(&request(3, "stepIn", None));
        assert_eq!(frame_count(&mut dbg), 1);
        dbg.handle(&request(4, "stepIn", None));
        assert_eq!(frame_count(&mut dbg), 2);

        let mut dbg = Debugger::new(&module, main);
        dbg.handle(&request(1, "launch", None));
        dbg.handle(&request(2, "stepIn", None));
        dbg.handle(&request(3, "stepIn", None));
        dbg.handle(&request(4, "next", None));
        assert_eq!(frame_count(&mut dbg), 1);
    }

    #[test]
    fn step_out_returns_to_the_caller() {
        let (module, main) = call_program();
        let mut dbg = Debugger::new(&module, main);
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
        let mut dbg = Debugger::new(&module, main);
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
