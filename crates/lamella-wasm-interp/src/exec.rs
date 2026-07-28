//! The executor: instantiate a decoded [`Module`] against a granted [`World`] and run it.

use crate::ops::{LabelKind, Op};
use crate::{num, ConstExpr, ExportKind, FuncType, ImportKind, Module, ValType};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// One wasm value. Floats are stored as raw IEEE-754 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Value {
    /// An `i32` bit pattern (signedness belongs to operations).
    I32(u32),
    /// An `i64` bit pattern.
    I64(u64),
    /// An `f32`, as raw bits.
    F32(u32),
    /// An `f64`, as raw bits.
    F64(u64),
}

impl Value {
    /// The value's type.
    #[must_use]
    pub fn ty(&self) -> ValType {
        match self {
            Value::I32(_) => ValType::I32,
            Value::I64(_) => ValType::I64,
            Value::F32(_) => ValType::F32,
            Value::F64(_) => ValType::F64,
        }
    }

    fn zero(ty: ValType) -> Value {
        match ty {
            ValType::I32 => Value::I32(0),
            ValType::I64 => Value::I64(0),
            ValType::F32 => Value::F32(0),
            ValType::F64 => Value::F64(0),
        }
    }
}

/// Why execution stopped abnormally. Guest-caused traps and engine-boundary refusals share
/// the enum; the distinction that matters is that NONE of these are convertible to guest
/// control flow -- a guest can never catch one and carry on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    /// The `unreachable` instruction executed.
    Unreachable,
    /// A linear-memory access left bounds.
    MemOutOfBounds,
    /// A `call_indirect` index left the table.
    TableOutOfBounds,
    /// A `call_indirect` hit an uninitialized table slot.
    NullFunction,
    /// A `call_indirect` callee's signature did not match the declared type.
    SignatureMismatch,
    /// Integer division or remainder by zero.
    DivByZero,
    /// `MIN / -1`, or a trapping float-to-int truncation out of range.
    IntOverflow,
    /// A trapping float-to-int truncation of NaN.
    InvalidConversion,
    /// A value/label/frame/locals budget was exhausted.
    StackExhausted,
    /// The validation-lite backstop: an operation met the wrong operand type or a stack
    /// underflow -- the module was invalid, caught dynamically.
    ModuleInvalid,
    /// `run` was called with a missing export or mismatched arguments.
    EntryMismatch,
    /// A host function failed; the message names the host-side reason.
    Host(&'static str),
}

/// Why instantiation refused. Every variant names its cause; there is no partial instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstantiateError {
    /// The module imports something the granted world does not carry.
    UngrantedImport {
        /// The import's namespace.
        module: String,
        /// The import's name.
        name: String,
    },
    /// A granted name exists but at a different function type -- a capability violation,
    /// not a link error (the type is part of the capability).
    ImportTypeMismatch {
        /// The import's namespace.
        module: String,
        /// The import's name.
        name: String,
    },
    /// The module's minimum memory exceeds the engine budget. Counts are in the MODULE's
    /// own page units (64 KiB classically; 1 B under the custom-page-sizes experiment).
    MemoryBudget {
        /// Pages the module requires.
        min_pages: u32,
        /// Pages the engine allows.
        budget: u32,
    },
    /// The module uses a gated experimental feature the engine config has disabled; the
    /// label names it.
    ExperimentDisabled(&'static str),
    /// An active data segment falls outside the initial memory.
    DataOutOfRange,
    /// An active element segment falls outside the table.
    ElementOutOfRange,
    /// A global initializer used `global.get` (imported globals are outside this scope).
    UnsupportedGlobalInit,
    /// The start function trapped.
    StartTrap(Trap),
}

/// Engine budgets. Defaults suit host and emulator runs; a device build tightens
/// them to its RAM honesty.
#[derive(Debug, Clone, Copy)]
pub struct EngineConfig {
    /// Operand-stack entries across all live frames.
    pub max_values: usize,
    /// Live call frames.
    pub max_frames: usize,
    /// Live labels across all frames.
    pub max_labels: usize,
    /// Locals-arena entries across all live frames.
    pub max_locals: usize,
    /// The linear-memory ceiling in 64 KiB pages (`memory.grow` refuses past it). Stated
    /// in 64 KiB units regardless of the module's own page size: the byte budget is what
    /// RAM honesty is about, and a custom-page-size module gets the SAME byte ceiling
    /// counted in its own pages.
    pub max_memory_pages: u32,
    /// EXPERIMENT (ON by default): admit memories declaring the
    /// phase-3 `custom-page-sizes` encoding (1-byte pages). The proposal is
    /// pre-standardization -- the encoding and semantics MAY CHANGE before phase 4, and
    /// this knob (or the feature's shape) may change with it. Set false to refuse such
    /// modules at instantiation, named.
    pub experimental_custom_page_sizes: bool,
}

impl Default for EngineConfig {
    fn default() -> EngineConfig {
        EngineConfig {
            max_values: 64 * 1024,
            max_frames: 1024,
            max_labels: 16 * 1024,
            max_locals: 64 * 1024,
            max_memory_pages: 64,
            experimental_custom_page_sizes: true,
        }
    }
}

/// A granted host function's implementation: linear memory in, arguments in, at most one
/// result out. The same shape as the CIL interpreter's intrinsic ABI, so the driver bridge
/// needs no adaptation layer.
pub type HostFn = Box<dyn FnMut(&mut [u8], &[Value]) -> Result<Option<Value>, Trap>>;

/// One granted capability: a named, TYPED host function.
pub struct HostFunc {
    /// The import namespace this grant answers (e.g. `lamella_i2c`).
    pub module: String,
    /// The name within the namespace.
    pub name: String,
    /// The granted signature; an import naming this function at any other type is refused.
    pub ty: FuncType,
    /// The implementation.
    pub call: HostFn,
}

/// The granted world: the COMPLETE set of capabilities an instance may touch. Anything a
/// module imports beyond this set fails instantiation, named.
#[derive(Default)]
pub struct World {
    /// The granted functions.
    pub funcs: Vec<HostFunc>,
}

/// A runtime label: where a branch to it lands, what it carries, and the operand height it
/// restores.
#[derive(Clone, Copy)]
struct Label {
    kind: LabelKind,
    target: u32,
    keep: u8,
    height: usize,
}

/// A suspended caller (the running frame lives in a local, not the vec).
#[derive(Clone, Copy)]
struct Frame {
    /// Index into `module.code` (defined-function space).
    func: usize,
    pc: usize,
    locals_base: usize,
    labels_base: usize,
    opnds_base: usize,
    ret: u8,
}

/// An instantiated module: its memory, globals, table, and bound world, ready to run.
pub struct Instance {
    module: Module,
    config: EngineConfig,
    memory: Vec<u8>,
    /// The grow ceiling, in the memory's OWN page units.
    memory_max_pages: u32,
    /// The memory's page size in bytes (65536 classically; 1 under the experiment).
    memory_page_size: u32,
    globals: Vec<Value>,
    table: Vec<Option<u32>>,
    world: World,
    /// World index per function import, in import order.
    bindings: Vec<usize>,
}

impl Instance {
    /// Builds an instance: gate the imports against the grant, size memory against the budget,
    /// evaluate globals, apply active segments, then run the start function if declared.
    pub fn instantiate(
        module: Module,
        world: World,
        config: EngineConfig,
    ) -> Result<Instance, InstantiateError> {
        let mut bindings = Vec::new();
        for import in &module.imports {
            let ImportKind::Func { type_index } = import.kind else {
                return Err(InstantiateError::UngrantedImport {
                    module: import.module.clone(),
                    name: import.name.clone(),
                });
            };
            let position = world
                .funcs
                .iter()
                .position(|f| f.module == import.module && f.name == import.name)
                .ok_or_else(|| InstantiateError::UngrantedImport {
                    module: import.module.clone(),
                    name: import.name.clone(),
                })?;
            let declared = &module.types[type_index as usize];
            if *declared != world.funcs[position].ty {
                return Err(InstantiateError::ImportTypeMismatch {
                    module: import.module.clone(),
                    name: import.name.clone(),
                });
            }
            bindings.push(position);
        }

        let (memory, memory_max_pages, memory_page_size) = match module.memory {
            Some(mem) => {
                if mem.page_size_log2 != 16 && !config.experimental_custom_page_sizes {
                    return Err(InstantiateError::ExperimentDisabled("custom-page-sizes"));
                }
                let page_size = mem.page_size();
                let budget_bytes = u64::from(config.max_memory_pages) * 65536;
                let budget_pages =
                    (budget_bytes / u64::from(page_size)).min(u64::from(u32::MAX)) as u32;
                let budget = match mem.limits.max {
                    Some(max) => max.min(budget_pages),
                    None => budget_pages,
                };
                if mem.limits.min > budget {
                    return Err(InstantiateError::MemoryBudget {
                        min_pages: mem.limits.min,
                        budget,
                    });
                }
                let bytes = u64::from(mem.limits.min) * u64::from(page_size);
                (vec![0u8; bytes as usize], budget, page_size)
            }
            None => (Vec::new(), 0, 65536),
        };

        let mut globals = Vec::new();
        for global in &module.globals {
            globals.push(match global.init {
                ConstExpr::I32(v) => Value::I32(v),
                ConstExpr::I64(v) => Value::I64(v),
                ConstExpr::F32(v) => Value::F32(v),
                ConstExpr::F64(v) => Value::F64(v),
                ConstExpr::GlobalGet(_) => {
                    return Err(InstantiateError::UnsupportedGlobalInit);
                }
            });
        }

        let mut table = vec![None; module.table.map_or(0, |t| t.min as usize)];

        let mut instance = Instance {
            module,
            config,
            memory,
            memory_max_pages,
            memory_page_size,
            globals,
            table: Vec::new(),
            world,
            bindings,
        };

        for segment in &instance.module.data {
            let ConstExpr::I32(offset) = segment.offset else {
                return Err(InstantiateError::UnsupportedGlobalInit);
            };
            let end = offset as u64 + segment.bytes.len() as u64;
            if end > instance.memory.len() as u64 {
                return Err(InstantiateError::DataOutOfRange);
            }
            instance.memory[offset as usize..end as usize].copy_from_slice(&segment.bytes);
        }
        for segment in &instance.module.elements {
            let ConstExpr::I32(offset) = segment.offset else {
                return Err(InstantiateError::UnsupportedGlobalInit);
            };
            let end = offset as u64 + segment.funcs.len() as u64;
            if end > table.len() as u64 {
                return Err(InstantiateError::ElementOutOfRange);
            }
            for (i, func) in segment.funcs.iter().enumerate() {
                table[offset as usize + i] = Some(*func);
            }
        }
        instance.table = table;

        if let Some(start) = instance.module.start {
            instance.exec(start, &[]).map_err(InstantiateError::StartTrap)?;
        }
        Ok(instance)
    }

    /// The instance's linear memory (the exported `memory`, when the module exports one).
    #[must_use]
    pub fn memory(&self) -> &[u8] {
        &self.memory
    }

    /// Runs an exported function with `args`, returning its result.
    pub fn run(&mut self, export: &str, args: &[Value]) -> Result<Option<Value>, Trap> {
        let Some(ExportKind::Func(index)) = self.module.export(export).map(|e| e.kind) else {
            return Err(Trap::EntryMismatch);
        };
        let ty = self.module.func_type(index).ok_or(Trap::EntryMismatch)?;
        if ty.params.len() != args.len()
            || ty.params.iter().zip(args).any(|(p, a)| *p != a.ty())
        {
            return Err(Trap::EntryMismatch);
        }
        self.exec(index, args)
    }

    /// The iterative interpreter loop.
    #[allow(clippy::too_many_lines)]
    fn exec(&mut self, entry: u32, args: &[Value]) -> Result<Option<Value>, Trap> {
        let Instance {
            module,
            config,
            memory,
            memory_max_pages,
            memory_page_size,
            globals,
            table,
            world,
            bindings,
        } = self;
        let module: &Module = module;
        let imported = module.imported_func_count();
        let max_values = config.max_values;
        let max_frames = config.max_frames;
        let max_labels = config.max_labels;
        let max_locals = config.max_locals;

        let mut opnds: Vec<Value> = Vec::new();
        let mut labels: Vec<Label> = Vec::new();
        let mut locals: Vec<Value> = Vec::new();
        let mut frames: Vec<Frame> = Vec::new();

        if entry < imported {
            let host = &mut world.funcs[bindings[entry as usize]];
            return (host.call)(memory.as_mut_slice(), args);
        }

        macro_rules! enter {
            ($func_index:expr, $argc:expr) => {{
                let defined = ($func_index - imported) as usize;
                let ty = &module.types[module.functions[defined] as usize];
                if frames.len() >= max_frames {
                    return Err(Trap::StackExhausted);
                }
                let locals_base = locals.len();
                if opnds.len() < $argc {
                    return Err(Trap::ModuleInvalid);
                }
                let args_at = opnds.len() - $argc;
                if locals_base + $argc + module.code[defined].locals.len() > max_locals {
                    return Err(Trap::StackExhausted);
                }
                locals.extend_from_slice(&opnds[args_at..]);
                opnds.truncate(args_at);
                for ty in &module.code[defined].locals {
                    locals.push(Value::zero(*ty));
                }
                Frame {
                    func: defined,
                    pc: 0,
                    locals_base,
                    labels_base: labels.len(),
                    opnds_base: opnds.len(),
                    ret: ty.results.len() as u8,
                }
            }};
        }

        for a in args {
            opnds.push(*a);
        }
        let mut cur = enter!(entry, args.len());

        macro_rules! push_opnd {
            ($v:expr) => {{
                if opnds.len() >= max_values {
                    return Err(Trap::StackExhausted);
                }
                opnds.push($v);
            }};
        }

        macro_rules! pop_opnd {
            () => {
                opnds.pop().ok_or(Trap::ModuleInvalid)?
            };
        }

        macro_rules! pop_i32 {
            () => {
                match pop_opnd!() {
                    Value::I32(v) => v,
                    _ => return Err(Trap::ModuleInvalid),
                }
            };
        }

        macro_rules! branch {
            ($depth:expr) => {{
                let ix = labels
                    .len()
                    .checked_sub(1 + $depth as usize)
                    .ok_or(Trap::ModuleInvalid)?;
                let label = labels[ix];
                let kept = if label.keep == 1 { Some(pop_opnd!()) } else { None };
                if opnds.len() < label.height {
                    return Err(Trap::ModuleInvalid);
                }
                opnds.truncate(label.height);
                if let Some(v) = kept {
                    opnds.push(v);
                }
                match label.kind {
                    LabelKind::Loop => labels.truncate(ix + 1),
                    LabelKind::Block => labels.truncate(ix),
                }
                cur.pc = label.target as usize;
            }};
        }

        loop {
            let op = module.code[cur.func]
                .ops
                .get(cur.pc)
                .ok_or(Trap::ModuleInvalid)?;
            cur.pc += 1;
            match op {
                Op::PushLabel { kind, keep, target } => {
                    if labels.len() >= max_labels {
                        return Err(Trap::StackExhausted);
                    }
                    labels.push(Label {
                        kind: *kind,
                        target: *target,
                        keep: *keep,
                        height: opnds.len(),
                    });
                }
                Op::PopLabel => {
                    if labels.len() <= cur.labels_base {
                        return Err(Trap::ModuleInvalid);
                    }
                    labels.pop();
                }
                Op::BrIfZero { target } => {
                    if pop_i32!() == 0 {
                        cur.pc = *target as usize;
                    }
                }
                Op::Goto { target } => cur.pc = *target as usize,
                Op::Br { depth } => branch!(*depth),
                Op::BrIf { depth } => {
                    if pop_i32!() != 0 {
                        branch!(*depth);
                    }
                }
                Op::BrTable { depths, default } => {
                    let i = pop_i32!() as usize;
                    let depth = depths.get(i).copied().unwrap_or(*default);
                    branch!(depth);
                }
                Op::Return => {
                    let arity = cur.ret as usize;
                    if opnds.len() < cur.opnds_base + arity {
                        return Err(Trap::ModuleInvalid);
                    }
                    let result = if arity == 1 { Some(opnds.pop().unwrap_or(Value::I32(0))) } else { None };
                    opnds.truncate(cur.opnds_base);
                    locals.truncate(cur.locals_base);
                    labels.truncate(cur.labels_base);
                    match frames.pop() {
                        Some(frame) => {
                            if let Some(v) = result {
                                push_opnd!(v);
                            }
                            cur = frame;
                        }
                        None => return Ok(result),
                    }
                }
                Op::Unreachable => return Err(Trap::Unreachable),
                Op::Nop => {}
                Op::Call { func } => {
                    let func = *func;
                    if func < imported {
                        let ty = module.func_type(func).ok_or(Trap::ModuleInvalid)?;
                        let argc = ty.params.len();
                        let has_result = !ty.results.is_empty();
                        if opnds.len() < argc {
                            return Err(Trap::ModuleInvalid);
                        }
                        let args_at = opnds.len() - argc;
                        let host = &mut world.funcs[bindings[func as usize]];
                        let result = (host.call)(memory.as_mut_slice(), &opnds[args_at..])?;
                        opnds.truncate(args_at);
                        match (has_result, result) {
                            (true, Some(v)) => push_opnd!(v),
                            (false, None) => {}
                            _ => return Err(Trap::Host("host result arity mismatch")),
                        }
                    } else {
                        let ty = module.func_type(func).ok_or(Trap::ModuleInvalid)?;
                        let argc = ty.params.len();
                        frames.push(cur);
                        cur = enter!(func, argc);
                    }
                }
                Op::CallIndirect { type_index } => {
                    let i = pop_i32!() as usize;
                    let slot = *table.get(i).ok_or(Trap::TableOutOfBounds)?;
                    let func = slot.ok_or(Trap::NullFunction)?;
                    let expected = &module.types[*type_index as usize];
                    let actual = module.func_type(func).ok_or(Trap::ModuleInvalid)?;
                    if actual != expected {
                        return Err(Trap::SignatureMismatch);
                    }
                    if func < imported {
                        let argc = expected.params.len();
                        let has_result = !expected.results.is_empty();
                        if opnds.len() < argc {
                            return Err(Trap::ModuleInvalid);
                        }
                        let args_at = opnds.len() - argc;
                        let host = &mut world.funcs[bindings[func as usize]];
                        let result = (host.call)(memory.as_mut_slice(), &opnds[args_at..])?;
                        opnds.truncate(args_at);
                        match (has_result, result) {
                            (true, Some(v)) => push_opnd!(v),
                            (false, None) => {}
                            _ => return Err(Trap::Host("host result arity mismatch")),
                        }
                    } else {
                        let argc = expected.params.len();
                        frames.push(cur);
                        cur = enter!(func, argc);
                    }
                }
                Op::Drop => {
                    let _ = pop_opnd!();
                }
                Op::Select => {
                    let cond = pop_i32!();
                    let b = pop_opnd!();
                    let a = pop_opnd!();
                    push_opnd!(if cond != 0 { a } else { b });
                }
                Op::LocalGet(i) => {
                    let v = locals[cur.locals_base + *i as usize];
                    push_opnd!(v);
                }
                Op::LocalSet(i) => {
                    let v = pop_opnd!();
                    locals[cur.locals_base + *i as usize] = v;
                }
                Op::LocalTee(i) => {
                    let v = *opnds.last().ok_or(Trap::ModuleInvalid)?;
                    locals[cur.locals_base + *i as usize] = v;
                }
                Op::GlobalGet(i) => {
                    let v = *globals.get(*i as usize).ok_or(Trap::ModuleInvalid)?;
                    push_opnd!(v);
                }
                Op::GlobalSet(i) => {
                    let v = pop_opnd!();
                    *globals.get_mut(*i as usize).ok_or(Trap::ModuleInvalid)? = v;
                }
                Op::Load { ty, width, signed, offset } => {
                    let addr = pop_i32!();
                    let bytes = mem_range(memory, addr, *offset, *width)?;
                    let raw = read_le(&memory[bytes.0..bytes.1]);
                    let v = extend_load(*ty, *width, *signed, raw);
                    push_opnd!(v);
                }
                Op::Store { ty, width, offset } => {
                    let raw = match (pop_opnd!(), *ty) {
                        (Value::I32(v), ValType::I32) => u64::from(v),
                        (Value::I64(v), ValType::I64) => v,
                        (Value::F32(v), ValType::F32) => u64::from(v),
                        (Value::F64(v), ValType::F64) => v,
                        _ => return Err(Trap::ModuleInvalid),
                    };
                    let addr = pop_i32!();
                    let (start, end) = mem_range(memory, addr, *offset, *width)?;
                    let le = raw.to_le_bytes();
                    memory[start..end].copy_from_slice(&le[..(end - start)]);
                }
                Op::MemorySize => {
                    push_opnd!(Value::I32(
                        (memory.len() as u64 / u64::from(*memory_page_size)) as u32
                    ));
                }
                Op::MemoryGrow => {
                    let delta = pop_i32!();
                    let old = (memory.len() as u64 / u64::from(*memory_page_size)) as u32;
                    let new = u64::from(old) + u64::from(delta);
                    if new > u64::from(*memory_max_pages) {
                        push_opnd!(Value::I32(u32::MAX));
                    } else {
                        memory.resize((new * u64::from(*memory_page_size)) as usize, 0);
                        push_opnd!(Value::I32(old));
                    }
                }
                Op::MemoryCopy => {
                    let n = u64::from(pop_i32!());
                    let src = u64::from(pop_i32!());
                    let dst = u64::from(pop_i32!());
                    let len = memory.len() as u64;
                    if src + n > len || dst + n > len {
                        return Err(Trap::MemOutOfBounds);
                    }
                    memory.copy_within(src as usize..(src + n) as usize, dst as usize);
                }
                Op::MemoryFill => {
                    let n = u64::from(pop_i32!());
                    let val = pop_i32!() as u8;
                    let dst = u64::from(pop_i32!());
                    if dst + n > memory.len() as u64 {
                        return Err(Trap::MemOutOfBounds);
                    }
                    memory[dst as usize..(dst + n) as usize].fill(val);
                }
                Op::I32Const(v) => push_opnd!(Value::I32(*v)),
                Op::I64Const(v) => push_opnd!(Value::I64(*v)),
                Op::F32Const(v) => push_opnd!(Value::F32(*v)),
                Op::F64Const(v) => push_opnd!(Value::F64(*v)),
                Op::Num(op) => num::eval(*op, &mut opnds)?,
            }
        }
    }
}

/// The overflow-proof bounds law: `addr + offset + width` computed in u64 against the
/// current memory length; a failing access traps before any byte moves.
fn mem_range(memory: &[u8], addr: u32, offset: u32, width: u8) -> Result<(usize, usize), Trap> {
    let start = u64::from(addr) + u64::from(offset);
    let end = start + u64::from(width);
    if end > memory.len() as u64 {
        return Err(Trap::MemOutOfBounds);
    }
    Ok((start as usize, end as usize))
}

/// Little-endian assembly of 1/2/4/8 bytes into a u64 (zero-filled high bits).
fn read_le(bytes: &[u8]) -> u64 {
    let mut raw = [0u8; 8];
    raw[..bytes.len()].copy_from_slice(bytes);
    u64::from_le_bytes(raw)
}

/// Applies a load's extension rule: sub-width integers sign- or zero-extend into their
/// result type; floats pass bits through.
fn extend_load(ty: ValType, width: u8, signed: bool, raw: u64) -> Value {
    match ty {
        ValType::I32 => {
            let v = match (width, signed) {
                (1, true) => raw as u8 as i8 as i32 as u32,
                (2, true) => raw as u16 as i16 as i32 as u32,
                _ => raw as u32,
            };
            Value::I32(v)
        }
        ValType::I64 => {
            let v = match (width, signed) {
                (1, true) => raw as u8 as i8 as i64 as u64,
                (2, true) => raw as u16 as i16 as i64 as u64,
                (4, true) => raw as u32 as i32 as i64 as u64,
                _ => raw,
            };
            Value::I64(v)
        }
        ValType::F32 => Value::F32(raw as u32),
        ValType::F64 => Value::F64(raw),
    }
}
