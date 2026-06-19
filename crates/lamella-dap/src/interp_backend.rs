//! The interpreter debug backend: drives a [`lamella_ves::Session`] behind the
//! [`DebugBackend`] seam, so the adapter debugs interpreted code through the same
//! interface an on-device target uses.

use lamella_cil::{Instruction, Operand};
use lamella_debug_backend::{DebugBackend, Disassembled, Frame, Register, Scope, Stop, Variable};
use lamella_ves::{Method, MethodId, Module, Session, Status, Value, Vm};

/// A [`DebugBackend`] over the interpreter: it owns the module being debugged, the
/// runtime context, and the running `Session` once launched.
pub struct InterpreterBackend {
    module: Module,
    entry: u32,
    vm: Vm,
    session: Option<Session>,
    /// Instruction breakpoints as `(method, instruction)`, kept so they survive the
    /// `launch` that (re)creates the session.
    breakpoints: Vec<(u32, u32)>,
    /// UTF-16 code units of console output already drained by [`Self::take_output`].
    output_sent: usize,
}

impl InterpreterBackend {
    /// Creates a backend that owns `module`, entered at `entry`.
    #[must_use]
    pub fn new(module: Module, entry: u32) -> InterpreterBackend {
        InterpreterBackend {
            module,
            entry,
            vm: Vm::new(),
            session: None,
            breakpoints: Vec::new(),
            output_sent: 0,
        }
    }

    fn apply_breakpoints(&mut self) {
        let InterpreterBackend {
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

    /// The CIL of a loaded method, or `None` for an intrinsic or unknown method.
    fn method_code(&self, method: u32) -> Option<&[Instruction]> {
        match self.module.method(method)? {
            Method::Managed { body, .. } => Some(&body.code[..]),
            Method::Intrinsic { .. } => None,
        }
    }
}

impl DebugBackend for InterpreterBackend {
    fn launch(&mut self) -> bool {
        match Session::new(&self.module, self.entry, Vec::new()) {
            Ok(session) => {
                self.session = Some(session);
                self.apply_breakpoints();
                true
            }
            Err(_) => false,
        }
    }

    fn resume(&mut self) -> Stop {
        let InterpreterBackend {
            session,
            vm,
            module,
            ..
        } = self;
        let Some(session) = session.as_mut() else {
            return Stop::Done;
        };
        if session.is_at_breakpoint() {
            match session.step(module, vm) {
                Ok(Status::Done(_)) => return Stop::Done,
                Ok(_) => {}
                Err(trap) => return Stop::Fault(format!("{trap}")),
            }
        }
        match session.resume(module, vm) {
            Ok(Status::Paused | Status::Running) => Stop::Breakpoint,
            Ok(Status::Done(_)) => Stop::Done,
            Err(trap) => Stop::Fault(format!("{trap}")),
        }
    }

    fn step(&mut self) -> Stop {
        let InterpreterBackend {
            session,
            vm,
            module,
            ..
        } = self;
        let Some(session) = session.as_mut() else {
            return Stop::Done;
        };
        match session.step(module, vm) {
            Ok(Status::Done(_)) => Stop::Done,
            Ok(Status::Running | Status::Paused) => Stop::Step,
            Err(trap) => Stop::Fault(format!("{trap}")),
        }
    }

    fn depth(&self) -> usize {
        self.session.as_ref().map_or(0, Session::depth)
    }

    fn set_breakpoints(&mut self, addresses: &[u64]) {
        self.breakpoints = addresses
            .iter()
            .map(|&address| decode_address(address))
            .collect();
        self.apply_breakpoints();
    }

    fn stack(&self) -> Vec<Frame> {
        let Some(session) = self.session.as_ref() else {
            return Vec::new();
        };
        (0..session.depth())
            .filter_map(|index| {
                session.frame(index).map(|frame| Frame {
                    address: encode_address(frame.method, frame.ip),
                    name: alloc_method_name(frame.method, frame.ip),
                    line: frame.ip + 1,
                })
            })
            .collect()
    }

    fn variables(&self, frame_index: usize, scope: Scope) -> Vec<Variable> {
        let Some(session) = self.session.as_ref() else {
            return Vec::new();
        };
        let Some(frame) = session.frame(frame_index) else {
            return Vec::new();
        };
        let (prefix, values) = match scope {
            Scope::Arguments => ("arg", frame.args),
            Scope::Locals => ("local", frame.locals),
            Scope::Stack => ("stack", frame.stack),
        };
        values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let (text, kind) = format_value(&self.vm, value.clone());
                Variable {
                    name: format!("{prefix}{index}"),
                    value: text,
                    kind,
                }
            })
            .collect()
    }

    fn read_memory(&self, _address: u64, _len: usize) -> Vec<u8> {
        Vec::new()
    }

    fn read_registers(&self) -> Vec<Register> {
        Vec::new()
    }

    fn disassemble(&self, address: u64, offset: i64, count: usize) -> Vec<Disassembled> {
        let (method, base_ip) = decode_address(address);
        let code = self.method_code(method);
        (0..count)
            .map(|step| {
                let ip = i64::from(base_ip) + offset + step as i64;
                let address = if ip >= 0 {
                    encode_address(method, ip as u32)
                } else {
                    0
                };
                let text = match (code, usize::try_from(ip)) {
                    (Some(code), Ok(index)) if index < code.len() => {
                        format_instruction(&code[index])
                    }
                    _ => "(out of range)".to_owned(),
                };
                Disassembled { address, text }
            })
            .collect()
    }

    fn take_output(&mut self) -> Option<String> {
        let output = self.vm.output();
        if output.len() > self.output_sent {
            let text = String::from_utf16_lossy(&output[self.output_sent..]);
            self.output_sent = output.len();
            Some(text)
        } else {
            None
        }
    }
}

/// Encodes a CIL location `(method, instruction)` as one opaque address: the method id
/// in the high 32 bits, the instruction index in the low 32.
#[must_use]
pub fn encode_address(method: MethodId, instruction: u32) -> u64 {
    (u64::from(method) << 32) | u64::from(instruction)
}

/// The inverse of [`encode_address`].
#[must_use]
pub fn decode_address(address: u64) -> (u32, u32) {
    ((address >> 32) as u32, (address & 0xFFFF_FFFF) as u32)
}

fn alloc_method_name(method: MethodId, ip: u32) -> String {
    format!("method#{method} @{ip}")
}

/// Renders a value as `(display text, type name)` for the variables view.
fn format_value(vm: &Vm, value: Value) -> (String, String) {
    match value {
        Value::Int32(n) => (n.to_string(), "int".to_owned()),
        Value::Int64(n) => (n.to_string(), "long".to_owned()),
        Value::NativeInt(n) => (n.to_string(), "native int".to_owned()),
        Value::Float(f) => (f.to_string(), "double".to_owned()),
        Value::Object(reference) => match vm.heap().as_string(reference) {
            Some(chars) => (
                format!("\"{}\"", String::from_utf16_lossy(chars)),
                "string".to_owned(),
            ),
            None => ("object".to_owned(), "object".to_owned()),
        },
        Value::Null => ("null".to_owned(), "object".to_owned()),
        Value::Struct(fields) => (format!("struct[{}]", fields.len()), "struct".to_owned()),
        Value::ByRef(_) => ("&".to_owned(), "byref".to_owned()),
    }
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
