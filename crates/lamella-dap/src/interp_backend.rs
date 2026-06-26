//! The interpreter debug backend: drives a [`lamella_cil_runtime::Session`] behind the
//! [`DebugBackend`] seam, so the adapter debugs interpreted code through the same
//! interface an on-device target uses.

use lamella_cil::{Instruction, Operand};
use lamella_debug_backend::{
    DebugBackend, Disassembled, Frame, Register, Scope, SourceLocation, Stop, Variable,
};
use lamella_metadata::PortablePdb;
use lamella_token::Token;
use lamella_cil_runtime::{MethodId, Module, Session, Status, Value, Vm};
use std::collections::{BTreeMap, BTreeSet};

/// The `MethodDef` metadata table tag, for rebuilding a method token to map a PDB
/// `method_rid` to the interpreter's `MethodId`.
const METHOD_DEF: u8 = 0x06;

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
    /// The standalone Portable PDB bytes, when source mapping is available.
    pdb: Option<Vec<u8>>,
    /// `MethodId` -> `MethodDef` rid: the reverse of the module's token binding, so a
    /// stack frame's method maps back to its PDB row for `source_location`.
    method_rid: BTreeMap<MethodId, u32>,
    /// Per method, the CIL byte offsets carrying a (non-hidden) sequence point: the
    /// statement boundaries a source-level step may land on. Empty without a PDB.
    seq_boundaries: BTreeMap<MethodId, BTreeSet<u32>>,
    /// True when execution is parked on a breakpoint already reported to the client, so
    /// the next `resume` steps off it before running on. False at launch -- so a
    /// breakpoint on the entry instruction fires rather than being stepped over.
    at_reported_breakpoint: bool,
    /// The program's exit code: the entry method's `int` return (else 0), captured when the
    /// session reports `Status::Done`. Reported in the adapter's `exited` event, so a debug run
    /// surfaces the real exit code (not a hardcoded 0).
    exit_code: i32,
}

/// The process exit code an entry method's return maps to: its `int` return, or 0 for a `void` (or
/// non-`int`) entry -- matching the non-debug run path's convention (`run_bytes`, `run-with-corlib`).
fn exit_code_of(returned: &Option<Value>) -> i32 {
    match returned {
        Some(Value::Int32(code)) => *code,
        _ => 0,
    }
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
            pdb: None,
            method_rid: BTreeMap::new(),
            seq_boundaries: BTreeMap::new(),
            at_reported_breakpoint: false,
            exit_code: 0,
        }
    }

    /// Creates a backend with source mapping from a standalone Portable PDB
    /// (`pdb_bytes`), building the `MethodId` -> `method_rid` reverse map from the
    /// module's token binding so frames and breakpoints map to and from source.
    #[must_use]
    pub fn with_pdb(module: Module, entry: u32, pdb_bytes: Vec<u8>) -> InterpreterBackend {
        let mut method_rid = BTreeMap::new();
        let mut seq_boundaries = BTreeMap::new();
        if let Ok(pdb) = PortablePdb::read(&pdb_bytes) {
            for rid in 1..=pdb.method_count() {
                if let Some(id) = module.resolve(0, Token::new(METHOD_DEF, rid)) {
                    method_rid.insert(id, rid);
                    let offsets: BTreeSet<u32> = pdb
                        .sequence_points(rid)
                        .iter()
                        .filter(|point| !point.is_hidden)
                        .map(|point| point.il_offset)
                        .collect();
                    if !offsets.is_empty() {
                        seq_boundaries.insert(id, offsets);
                    }
                }
            }
        }
        InterpreterBackend {
            module,
            entry,
            vm: Vm::new(),
            session: None,
            breakpoints: Vec::new(),
            output_sent: 0,
            pdb: Some(pdb_bytes),
            method_rid,
            seq_boundaries,
            at_reported_breakpoint: false,
            exit_code: 0,
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

    /// The CIL of a loaded method, or `None` for an intrinsic or unknown method. Decodes the body
    /// lazily on first access (shared with the interpreter's own lazy decode).
    fn method_code(&self, method: u32) -> Option<&[Instruction]> {
        self.module.method_body(method).map(|body| &body.code[..])
    }

    /// The CIL byte offset of each instruction in `method` (index -> offset), recomputed
    /// from the decoded body to align with the Portable PDB's sequence points.
    fn offsets(&self, method: MethodId) -> Option<Vec<u32>> {
        lamella_cil::instruction_offsets(self.method_code(method)?)
    }

    /// The CIL byte offset of `method`'s instruction `index`.
    fn index_to_il_offset(&self, method: MethodId, index: u32) -> Option<u32> {
        self.offsets(method)?.get(index as usize).copied()
    }

    /// The instruction index at CIL byte offset `il_offset` in `method`: the boundary the
    /// offset names, else the last instruction at or before it.
    fn il_offset_to_index(&self, method: MethodId, il_offset: u32) -> Option<u32> {
        let offsets = self.offsets(method)?;
        let slice = offsets.get(..offsets.len().saturating_sub(1))?;
        let index = slice
            .iter()
            .position(|&offset| offset == il_offset)
            .or_else(|| slice.iter().rposition(|&offset| offset <= il_offset))?;
        u32::try_from(index).ok()
    }

    /// Source names for `method`'s locals by slot, from the PDB, when available.
    fn local_names(&self, method: MethodId) -> Option<BTreeMap<u16, String>> {
        let pdb = PortablePdb::read(self.pdb.as_ref()?).ok()?;
        let method_rid = *self.method_rid.get(&method)?;
        let mut names = BTreeMap::new();
        for variable in pdb.local_variables(method_rid) {
            names.insert(variable.index, String::from(variable.name));
        }
        Some(names)
    }
}

impl DebugBackend for InterpreterBackend {
    fn launch(&mut self) -> bool {
        match Session::new(&self.module, self.entry, Vec::new()) {
            Ok(session) => {
                self.session = Some(session);
                self.at_reported_breakpoint = false;
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
            at_reported_breakpoint,
            exit_code,
            ..
        } = self;
        let Some(session) = session.as_mut() else {
            return Stop::Done;
        };
        if *at_reported_breakpoint && session.is_at_breakpoint() {
            *at_reported_breakpoint = false;
            match session.step(module, vm) {
                Ok(Status::Done(value)) => {
                    *exit_code = exit_code_of(&value);
                    return Stop::Done;
                }
                Ok(_) => {}
                Err(trap) => return Stop::Fault(format!("{trap}")),
            }
        }
        match session.resume(module, vm) {
            Ok(Status::Paused | Status::Running) => {
                *at_reported_breakpoint = true;
                Stop::Breakpoint
            }
            Ok(Status::Done(value)) => {
                *exit_code = exit_code_of(&value);
                Stop::Done
            }
            Err(trap) => Stop::Fault(format!("{trap}")),
        }
    }

    fn step(&mut self) -> Stop {
        let InterpreterBackend {
            session,
            vm,
            module,
            at_reported_breakpoint,
            exit_code,
            ..
        } = self;
        let Some(session) = session.as_mut() else {
            return Stop::Done;
        };
        *at_reported_breakpoint = false;
        match session.step(module, vm) {
            Ok(Status::Done(value)) => {
                *exit_code = exit_code_of(&value);
                Stop::Done
            }
            Ok(Status::Running | Status::Paused) if session.is_at_breakpoint() => {
                *at_reported_breakpoint = true;
                Stop::Breakpoint
            }
            Ok(Status::Running | Status::Paused) => Stop::Step,
            Err(trap) => Stop::Fault(format!("{trap}")),
        }
    }

    fn exit_code(&self) -> i32 {
        self.exit_code
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
                    name: self
                        .module
                        .method_name(frame.method)
                        .map(String::from)
                        .unwrap_or_else(|| alloc_method_name(frame.method, frame.ip)),
                    line: frame.ip + 1,
                })
            })
            .collect()
    }

    fn resolve_source_breakpoint(&self, document: &str, line: u32) -> Option<u64> {
        let pdb = PortablePdb::read(self.pdb.as_ref()?).ok()?;
        let basename: fn(&str) -> &str = |path| path.rsplit(['/', '\\']).next().unwrap_or(path);
        let (method_rid, il_offset) = pdb.resolve_breakpoint(document, line).or_else(|| {
            let target = basename(document);
            let document = (1..=pdb.method_count())
                .filter_map(|rid| pdb.method_document(rid))
                .find(|candidate| basename(candidate) == target)?;
            pdb.resolve_breakpoint(&document, line)
        })?;
        let method = self.module.resolve(0, Token::new(METHOD_DEF, method_rid))?;
        let index = self.il_offset_to_index(method, il_offset)?;
        Some(encode_address(method, index))
    }

    fn source_location(&self, address: u64) -> Option<SourceLocation> {
        let (method, index) = decode_address(address);
        let method_rid = *self.method_rid.get(&method)?;
        let il_offset = self.index_to_il_offset(method, index)?;
        let pdb = PortablePdb::read(self.pdb.as_ref()?).ok()?;
        let point = pdb.source_location(method_rid, il_offset)?;
        Some(SourceLocation {
            file: pdb.method_document(method_rid)?,
            line: point.start_line,
            column: point.start_column,
            end_line: point.end_line,
            end_column: point.end_column,
        })
    }

    fn has_source(&self) -> bool {
        self.pdb.is_some()
    }

    fn at_source_boundary(&self) -> bool {
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        let Some(frame) = session
            .depth()
            .checked_sub(1)
            .and_then(|innermost| session.frame(innermost))
        else {
            return false;
        };
        let Some(boundaries) = self.seq_boundaries.get(&frame.method) else {
            return false;
        };
        self.index_to_il_offset(frame.method, frame.ip)
            .is_some_and(|il_offset| boundaries.contains(&il_offset))
    }

    fn variables(&self, frame_index: usize, scope: Scope) -> Vec<Variable> {
        let Some(session) = self.session.as_ref() else {
            return Vec::new();
        };
        let Some(frame) = session.frame(frame_index) else {
            return Vec::new();
        };
        let method = frame.method;
        let (prefix, values) = match scope {
            Scope::Arguments => ("arg", frame.args),
            Scope::Locals => ("local", frame.locals),
            Scope::Stack => ("stack", frame.stack),
        };
        let names = matches!(scope, Scope::Locals)
            .then(|| self.local_names(method))
            .flatten();
        let arguments = matches!(scope, Scope::Arguments);
        values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let (text, kind) = format_value(&self.vm, value.clone());
                let name = names
                    .as_ref()
                    .and_then(|names| names.get(&(index as u16)))
                    .cloned()
                    .or_else(|| {
                        arguments
                            .then(|| self.module.arg_name(method, index).map(String::from))
                            .flatten()
                    })
                    .unwrap_or_else(|| format!("{prefix}{index}"));
                Variable {
                    name,
                    value: text,
                    kind,
                }
            })
            .collect()
    }

    fn set_variable(
        &mut self,
        frame_index: usize,
        scope: Scope,
        name: &str,
        value: &str,
    ) -> Option<String> {
        if matches!(scope, Scope::Stack) {
            return None;
        }
        let slot = self
            .variables(frame_index, scope)
            .iter()
            .position(|variable| variable.name == name)?;
        let frame = self.session.as_ref()?.frame(frame_index)?;
        let current = match scope {
            Scope::Arguments => frame.args.get(slot),
            Scope::Locals => frame.locals.get(slot),
            Scope::Stack => None,
        }?
        .clone();
        let new_value = match &current {
            Value::Object(reference) if self.vm.heap().as_string(*reference).is_some() => {
                let text = strip_one_quote_pair(value);
                let chars: Vec<u16> = text.encode_utf16().collect();
                Value::Object(self.vm.heap_mut().alloc_string(&chars))
            }
            _ => parse_value(&current, value)?,
        };
        let session = self.session.as_mut()?;
        let written = match scope {
            Scope::Arguments => session.set_arg(frame_index, slot, new_value.clone()),
            Scope::Locals => session.set_local(frame_index, slot, new_value.clone()),
            Scope::Stack => false,
        };
        if !written {
            return None;
        }
        Some(format_value(&self.vm, new_value).0)
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
        Value::Single(f) => (f.to_string(), "float".to_owned()),
        Value::Object(reference) => match vm.heap().as_string(reference) {
            Some(chars) => (
                format!("\"{}\"", String::from_utf16_lossy(&chars)),
                "string".to_owned(),
            ),
            None => ("object".to_owned(), "object".to_owned()),
        },
        Value::Null => ("null".to_owned(), "object".to_owned()),
        Value::Struct(fields) => (format!("struct[{}]", fields.len()), "struct".to_owned()),
        Value::ByRef(_) => ("&".to_owned(), "byref".to_owned()),
        Value::TypedRef { .. } => ("typedref".to_owned(), "typedref".to_owned()),
    }
}

/// Parses `text` into a [`Value`] of the SAME kind as `current` -- the inverse of
/// [`format_value`]'s rendering, so a `setVariable` round-trips. Returns `None` when the
/// text does not parse as that kind, or when the kind is one this editor does not support
/// (null / struct / byref / typedref, plus a NON-string object -- see the trailing arms):
/// those render as a description, not an editable literal, so there is nothing to parse back
/// into. A managed-`String` object IS editable, but `set_variable` handles it before reaching
/// here (it mints a new String rather than parsing), so the `Value::Object` arm here only ever
/// covers non-string objects.
///
/// `bool` and `char` are not distinguished here: the runtime widens both to [`Value::Int32`]
/// (see `Value`), and `format_value` renders that slot as a plain decimal, so the inverse is
/// an integer parse -- typing `1` into a `bool` local sets it to 1, matching how it reads back.
fn parse_value(current: &Value, text: &str) -> Option<Value> {
    let text = text.trim();
    match current {
        Value::Int32(_) => text.parse::<i32>().ok().map(Value::Int32),
        Value::Int64(_) => text.parse::<i64>().ok().map(Value::Int64),
        Value::NativeInt(_) => text.parse::<i64>().ok().map(Value::NativeInt),
        Value::Float(_) => text.parse::<f64>().ok().map(Value::Float),
        Value::Single(_) => text.parse::<f32>().ok().map(Value::Single),
        Value::Object(_)
        | Value::Null
        | Value::Struct(_)
        | Value::ByRef(_)
        | Value::TypedRef { .. } => None,
    }
}

/// Strips one matched pair of surrounding double-quotes from `text`, if present, returning
/// the inner content; otherwise returns `text` unchanged. This is the inverse of
/// [`format_value`]'s `"hi"` rendering of a string, so a `setVariable` round-trips whether the
/// client sends the rendered `"hi"` or the bare `hi`. Only the outermost pair is removed and
/// the content is taken verbatim (no trimming or unescaping): `"a b"` -> `a b`, `"` -> `"`
/// (a lone quote has no pair), `""` -> `` (empty string).
fn strip_one_quote_pair(text: &str) -> &str {
    text.strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(text)
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
