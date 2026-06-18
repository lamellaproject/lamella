//! The debug adapter: translates DAP requests into actions on a
//! [`lamella_ves::Session`] and produces the responses and events DAP expects.

use crate::protocol::{Event, Message, Request, Response};
use lamella_cil::{Instruction, Operand};
use lamella_ves::{Module, Status, Trap, Value, Vm};
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
    /// How many UTF-16 code units of the program's console output have already been
    /// forwarded to the client as `output` events.
    output_sent: usize,
    out_seq: i64,
}

impl<'a> Debugger<'a> {
    /// Creates a debugger for `module`, whose program entry point is `entry`.
    ///
    /// The caller provides the already-loaded program: the server binary loads it
    /// (via `lamella-load`) from the command line; reading it from the DAP `launch`
    /// request instead is a later refinement.
    #[must_use]
    pub fn new(module: &'a Module, entry: u32) -> Debugger<'a> {
        Debugger {
            module,
            entry,
            vm: Vm::new(),
            session: None,
            breakpoints: Vec::new(),
            output_sent: 0,
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
            "pause" => {
                if self.session.is_some() {
                    events.push(("stopped", Some(stopped("pause"))));
                }
                (true, None)
            }
            "setBreakpoints" => (true, Some(unverified_breakpoints(request))),
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
        let Debugger {
            session,
            vm,
            output_sent,
            ..
        } = self;
        let Some(session) = session.as_mut() else {
            return;
        };
        if session.is_at_breakpoint() {
            match session.step(vm) {
                Ok(Status::Done(_)) => return finish(Run::Done, vm, output_sent, events),
                Ok(_) => {}
                Err(trap) => return finish(Run::Fault(trap), vm, output_sent, events),
            }
        }
        let run = match session.resume(vm) {
            Ok(Status::Paused | Status::Running) => Run::Stopped("breakpoint"),
            Ok(Status::Done(_)) => Run::Done,
            Err(trap) => Run::Fault(trap),
        };
        finish(run, vm, output_sent, events);
    }

    /// `stepIn`: execute exactly one instruction, descending into any call.
    fn step_in(&mut self, events: &mut Vec<(&'static str, Option<Json>)>) {
        let Debugger {
            session,
            vm,
            output_sent,
            ..
        } = self;
        let Some(session) = session.as_mut() else {
            return;
        };
        let run = match session.step(vm) {
            Ok(Status::Done(_)) => Run::Done,
            Ok(Status::Running | Status::Paused) => Run::Stopped("step"),
            Err(trap) => Run::Fault(trap),
        };
        finish(run, vm, output_sent, events);
    }

    /// `next`: step one instruction, but run any call it makes to completion
    /// (stop once the stack is no deeper than it began).
    fn step_over(&mut self, events: &mut Vec<(&'static str, Option<Json>)>) {
        let Debugger {
            session,
            vm,
            output_sent,
            ..
        } = self;
        let Some(session) = session.as_mut() else {
            return;
        };
        let target_depth = session.depth();
        let run = loop {
            match session.step(vm) {
                Ok(Status::Done(_)) => break Run::Done,
                Ok(Status::Running | Status::Paused) if session.depth() <= target_depth => {
                    break Run::Stopped("step");
                }
                Ok(_) => {}
                Err(trap) => break Run::Fault(trap),
            }
        };
        finish(run, vm, output_sent, events);
    }

    /// `stepOut`: run until the current method returns to its caller.
    fn step_out(&mut self, events: &mut Vec<(&'static str, Option<Json>)>) {
        let Debugger {
            session,
            vm,
            output_sent,
            ..
        } = self;
        let Some(session) = session.as_mut() else {
            return;
        };
        let target_depth = session.depth();
        let run = loop {
            match session.step(vm) {
                Ok(Status::Done(_)) => break Run::Done,
                Ok(Status::Running | Status::Paused) if session.depth() < target_depth => {
                    break Run::Stopped("step");
                }
                Ok(_) => {}
                Err(trap) => break Run::Fault(trap),
            }
        };
        finish(run, vm, output_sent, events);
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
                    "instructionPointerReference": encode_address(frame.method, frame.ip).to_string(),
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

    /// Lists CIL instructions starting at the `memoryReference` address, returning
    /// each with its own address so the client can set instruction breakpoints.
    fn disassemble(&self, request: &Request) -> Json {
        let arguments = request.arguments.as_ref();
        let reference = arguments
            .and_then(|args| args.get("memoryReference"))
            .and_then(Json::as_str)
            .and_then(|reference| reference.parse::<u64>().ok());
        let Some(reference) = reference else {
            return json!({ "instructions": [] });
        };
        let (method, base_ip) = decode_address(reference);
        let instruction_offset = arguments
            .and_then(|args| args.get("instructionOffset"))
            .and_then(Json::as_i64)
            .unwrap_or(0);
        let count = arguments
            .and_then(|args| args.get("instructionCount"))
            .and_then(Json::as_u64)
            .unwrap_or(0)
            .min(4096);

        let code = self.method_code(method);
        let mut instructions = Vec::new();
        for step in 0..count {
            let ip = i64::from(base_ip) + instruction_offset + step as i64;
            let address = if ip >= 0 {
                encode_address(method, ip as u32)
            } else {
                0
            };
            let text = match (code, usize::try_from(ip)) {
                (Some(code), Ok(index)) if index < code.len() => format_instruction(&code[index]),
                _ => "(out of range)".to_owned(),
            };
            instructions.push(json!({ "address": address.to_string(), "instruction": text }));
        }
        json!({ "instructions": instructions })
    }

    /// The CIL of a loaded method, or `None` for an intrinsic or unknown method.
    fn method_code(&self, method: u32) -> Option<&[Instruction]> {
        match self.module.method(method)? {
            lamella_ves::Method::Managed { body, .. } => Some(&body.code[..]),
            lamella_ves::Method::Intrinsic { .. } => None,
        }
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

/// The outcome of running the interpreter for one command, before output is
/// flushed and the matching event emitted.
enum Run {
    /// Paused, with the DAP stop reason (`breakpoint` or `step`).
    Stopped(&'static str),
    /// The program ran to completion.
    Done,
    /// A trap ended the run.
    Fault(Trap),
}

/// Flushes any new program output as an `output` event, then emits the event for
/// `run` -- so console output precedes the stop or terminate it accompanies.
fn finish(
    run: Run,
    vm: &Vm,
    output_sent: &mut usize,
    events: &mut Vec<(&'static str, Option<Json>)>,
) {
    flush_output(vm, output_sent, events);
    match run {
        Run::Stopped(reason) => events.push(("stopped", Some(stopped(reason)))),
        Run::Done => terminate(events),
        Run::Fault(trap) => fault(events, &trap),
    }
}

/// Emits an `output` event for console output produced since the last flush.
fn flush_output(vm: &Vm, output_sent: &mut usize, events: &mut Vec<(&'static str, Option<Json>)>) {
    let output = vm.output();
    if output.len() > *output_sent {
        let text = String::from_utf16_lossy(&output[*output_sent..]);
        events.push((
            "output",
            Some(json!({ "category": "stdout", "output": text })),
        ));
        *output_sent = output.len();
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
        "supportsDisassembleRequest": true,
    })
}

/// Renders one CIL instruction as a mnemonic and its operand, for disassembly.
fn format_instruction(instruction: &Instruction) -> String {
    let mnemonic = instruction.opcode.mnemonic();
    match &instruction.operand {
        Operand::None => mnemonic.to_owned(),
        Operand::Int8(value) => format!("{mnemonic} {value}"),
        Operand::Int32(value) => format!("{mnemonic} {value}"),
        Operand::Int64(value) => format!("{mnemonic} {value}"),
        Operand::Float32(value) => format!("{mnemonic} {value}"),
        Operand::Float64(value) => format!("{mnemonic} {value}"),
        Operand::Variable(slot) => format!("{mnemonic} {slot}"),
        Operand::Target(target) => format!("{mnemonic} -> {target}"),
        Operand::Switch(targets) => format!("{mnemonic} ({} targets)", targets.len()),
        Operand::Token(token) => format!("{mnemonic} 0x{:08X}", token.0),
        Operand::Alignment(align) => format!("{mnemonic} {align}"),
    }
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

    #[test]
    fn disassemble_lists_a_methods_instructions() {
        let (module, main) = add_program();
        let mut dbg = Debugger::new(&module, main);
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
