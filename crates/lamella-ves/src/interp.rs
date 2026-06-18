//! A tree-of-frames CIL interpreter over a hand-built method body.

use crate::module::{Method, MethodId, Module};
use crate::object::Heap;
use crate::trap::Trap;
use crate::value::Value;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::vec::Vec;
use lamella_cil::{Instruction, MethodBodyImage, Opcode, Operand};
use lamella_token::Token;

/// The maximum depth of the call stack before [`Trap::CallStackOverflow`]. Bounds
/// runaway recursion in the absence of a configured stack size.
const MAX_CALL_DEPTH: usize = 4096;

/// The runtime context an execution shares across frames and exposes to
/// intrinsics: the managed heap and the console output.
///
/// This is the `Vm` an [`crate::module::IntrinsicFn`] receives. It deliberately
/// does *not* hold the call frames or the program, so an intrinsic can borrow it
/// mutably while the interpreter holds the frame stack. The console is an
/// in-memory buffer for now; a device console transport replaces it later.
#[derive(Debug, Default)]
pub struct Vm {
    heap: Heap,
    output: Vec<u16>,
}

impl Vm {
    /// Creates a fresh runtime context with an empty heap and no output.
    #[must_use]
    pub fn new() -> Vm {
        Vm::default()
    }

    /// The managed heap.
    #[must_use]
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// The managed heap, mutably (to allocate objects).
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// Appends UTF-16 code units to the console output.
    pub fn write(&mut self, chars: &[u16]) {
        self.output.extend_from_slice(chars);
    }

    /// The console output so far, as UTF-16 code units.
    #[must_use]
    pub fn output(&self) -> &[u16] {
        &self.output
    }

    /// The console output so far, decoded to a `String` (lossily, for display
    /// and tests).
    #[must_use]
    pub fn output_string(&self) -> String {
        String::from_utf16_lossy(&self.output)
    }
}

/// One activation frame: the evaluation stack, the local variables, the
/// arguments, the instruction pointer, and which method is running, for a single
/// method invocation.
struct Frame<'a> {
    code: &'a [Instruction],
    method: MethodId,
    ip: usize,
    stack: Vec<Value>,
    locals: Vec<Value>,
    args: Vec<Value>,
}

/// What executing one instruction decided.
enum Flow {
    /// Continue with the next instruction (or wherever a branch set `ip`).
    Next,
    /// The method returned, with its result if any.
    Return(Option<Value>),
    /// The method called another; its frame must be pushed.
    Call {
        /// The callee.
        method: MethodId,
        /// The arguments taken from the caller's stack, in declaration order.
        args: Vec<Value>,
    },
}

/// Runs a single method that makes no calls, returning the value its `ret` leaves
/// on the stack (`None` for a void return).
///
/// For methods that call others, use [`run`] with a [`Module`]. A `call` here is
/// [`Trap::Unsupported`], since there is no module to resolve it against.
///
/// # Errors
/// Returns a [`Trap`] for malformed or unsupported CIL, a stack imbalance, an
/// out-of-range local, argument, or branch, or integer division by zero.
pub fn run_method(body: &MethodBodyImage, args: Vec<Value>) -> Result<Option<Value>, Trap> {
    let mut vm = Vm::new();
    let mut frame = Frame::new(&body.code, 0, args);
    loop {
        let instruction = frame.code.get(frame.ip).ok_or(Trap::FellThroughEnd)?;
        frame.ip += 1;
        match step(&mut frame, None, &mut vm, instruction)? {
            Flow::Next => {}
            Flow::Return(result) => return Ok(result),
            Flow::Call { .. } => return Err(Trap::Unsupported(Opcode::Call)),
        }
    }
}

/// Runs `entry` in `module` with `args`, following static calls, and returns the
/// value the entry method ultimately returns. This is [`Session::run`] without
/// stopping at breakpoints.
///
/// # Errors
/// Returns a [`Trap`] as [`run_method`] does, plus [`Trap::UnresolvedCall`] for a
/// call token that names no method and [`Trap::CallStackOverflow`] for runaway
/// recursion.
pub fn run(
    module: &Module,
    vm: &mut Vm,
    entry: MethodId,
    args: Vec<Value>,
) -> Result<Option<Value>, Trap> {
    Session::new(module, entry, args)?.run(vm)
}

/// A steppable, inspectable execution: the foundation the debugger drives.
///
/// A `Session` owns the call stack and advances one CIL instruction at a time,
/// which is what instruction-level (source-free) debugging needs. It can run to
/// completion ([`Session::run`]), single-step ([`Session::step`]), or resume until
/// a breakpoint ([`Session::resume`]); the call stack is open to inspection
/// throughout ([`Session::frame`]). The runtime context ([`Vm`]) is passed in on
/// each advance rather than owned, so the heap and console outlive a session.
pub struct Session<'a> {
    module: &'a Module,
    frames: Vec<Frame<'a>>,
    breakpoints: BTreeSet<(MethodId, u32)>,
    result: Option<Option<Value>>,
}

/// The state of a [`Session`] after a step or resume.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Execution has not finished; more instructions remain.
    Running,
    /// Execution paused at a breakpoint, before the instruction there ran.
    Paused,
    /// Execution finished; the entry method returned this value.
    Done(Option<Value>),
}

/// A read-only view of one activation frame, for inspection such as a debugger's
/// stack trace and variables.
#[derive(Debug, Clone, Copy)]
pub struct FrameView<'s> {
    /// The method running in this frame.
    pub method: MethodId,
    /// The index of the instruction about to execute.
    pub ip: u32,
    /// The evaluation stack, bottom first.
    pub stack: &'s [Value],
    /// The local variables.
    pub locals: &'s [Value],
    /// The arguments.
    pub args: &'s [Value],
}

impl<'a> Session<'a> {
    /// Starts a session at `entry` with `args`.
    ///
    /// # Errors
    /// Returns [`Trap::NoSuchMethod`] if `entry` is not a managed method.
    pub fn new(module: &'a Module, entry: MethodId, args: Vec<Value>) -> Result<Session<'a>, Trap> {
        Ok(Session {
            module,
            frames: alloc::vec![new_frame(module, entry, args)?],
            breakpoints: BTreeSet::new(),
            result: None,
        })
    }

    /// Sets a breakpoint before instruction `instruction` of `method`.
    pub fn add_breakpoint(&mut self, method: MethodId, instruction: u32) {
        self.breakpoints.insert((method, instruction));
    }

    /// Clears a breakpoint set by [`Session::add_breakpoint`].
    pub fn remove_breakpoint(&mut self, method: MethodId, instruction: u32) {
        self.breakpoints.remove(&(method, instruction));
    }

    /// Removes all breakpoints (e.g. when a debugger replaces the whole set).
    pub fn clear_breakpoints(&mut self) {
        self.breakpoints.clear();
    }

    /// Whether the session is currently sitting at a breakpoint -- the next
    /// [`Session::resume`] would pause immediately. A debugger uses this to step
    /// off a breakpoint before continuing.
    #[must_use]
    pub fn is_at_breakpoint(&self) -> bool {
        self.at_breakpoint()
    }

    /// Executes exactly one instruction, ignoring breakpoints.
    ///
    /// # Errors
    /// Returns a [`Trap`] if the instruction faults.
    pub fn step(&mut self, vm: &mut Vm) -> Result<Status, Trap> {
        if let Some(result) = &self.result {
            return Ok(Status::Done(*result));
        }
        self.advance(vm)
    }

    /// Runs until a breakpoint is reached or the program finishes. A breakpoint
    /// pauses *before* its instruction runs; to continue past one the session is
    /// sitting on, [`Session::step`] once, then resume again.
    ///
    /// # Errors
    /// Returns a [`Trap`] if an instruction faults.
    pub fn resume(&mut self, vm: &mut Vm) -> Result<Status, Trap> {
        loop {
            if let Some(result) = &self.result {
                return Ok(Status::Done(*result));
            }
            if self.at_breakpoint() {
                return Ok(Status::Paused);
            }
            if let Status::Done(value) = self.advance(vm)? {
                return Ok(Status::Done(value));
            }
        }
    }

    /// Runs to completion, ignoring breakpoints, returning the entry's result.
    ///
    /// # Errors
    /// Returns a [`Trap`] if an instruction faults.
    pub fn run(&mut self, vm: &mut Vm) -> Result<Option<Value>, Trap> {
        loop {
            if let Some(result) = &self.result {
                return Ok(*result);
            }
            self.advance(vm)?;
        }
    }

    /// The number of frames on the call stack (0 once finished).
    #[must_use]
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// A view of frame `index`, with 0 the outermost (entry) frame and the last
    /// the innermost (currently executing).
    #[must_use]
    pub fn frame(&self, index: usize) -> Option<FrameView<'_>> {
        let frame = self.frames.get(index)?;
        Some(FrameView {
            method: frame.method,
            ip: frame.ip as u32,
            stack: &frame.stack,
            locals: &frame.locals,
            args: &frame.args,
        })
    }

    fn at_breakpoint(&self) -> bool {
        match self.frames.last() {
            Some(frame) => self.breakpoints.contains(&(frame.method, frame.ip as u32)),
            None => false,
        }
    }

    /// Executes one instruction of the top frame and applies its effect to the
    /// call stack: a return pops (handing the result back, or finishing); a call
    /// pushes a managed frame or invokes an intrinsic.
    fn advance(&mut self, vm: &mut Vm) -> Result<Status, Trap> {
        let Session {
            module,
            frames,
            result,
            ..
        } = self;
        let module = *module;
        let Some(top) = frames.last_mut() else {
            return Ok(Status::Done(None));
        };
        let instruction = top.code.get(top.ip).ok_or(Trap::FellThroughEnd)?;
        top.ip += 1;
        match step(top, Some(module), vm, instruction)? {
            Flow::Next => Ok(Status::Running),
            Flow::Return(value) => {
                frames.pop();
                match frames.last_mut() {
                    Some(caller) => {
                        if let Some(value) = value {
                            caller.stack.push(value);
                        }
                        Ok(Status::Running)
                    }
                    None => {
                        *result = Some(value);
                        Ok(Status::Done(value))
                    }
                }
            }
            Flow::Call { method, args } => match module.method(method) {
                Some(Method::Managed { .. }) => {
                    if frames.len() >= MAX_CALL_DEPTH {
                        return Err(Trap::CallStackOverflow);
                    }
                    frames.push(new_frame(module, method, args)?);
                    Ok(Status::Running)
                }
                Some(Method::Intrinsic { func, .. }) => {
                    let func = *func;
                    if let Some(value) = func(vm, &args)? {
                        frames
                            .last_mut()
                            .ok_or(Trap::CallStackOverflow)?
                            .stack
                            .push(value);
                    }
                    Ok(Status::Running)
                }
                None => Err(Trap::NoSuchMethod(method)),
            },
        }
    }
}

fn new_frame<'a>(module: &'a Module, id: MethodId, args: Vec<Value>) -> Result<Frame<'a>, Trap> {
    match module.method(id) {
        Some(Method::Managed { body, .. }) => Ok(Frame::new(&body.code, id, args)),
        _ => Err(Trap::NoSuchMethod(id)),
    }
}

fn step(
    frame: &mut Frame<'_>,
    module: Option<&Module>,
    vm: &mut Vm,
    instruction: &Instruction,
) -> Result<Flow, Trap> {
    let opcode = instruction.opcode;
    match opcode {
        Opcode::Nop => {}
        Opcode::Pop => {
            frame.pop()?;
        }
        Opcode::Dup => {
            let top = *frame.stack.last().ok_or(Trap::StackUnderflow)?;
            frame.stack.push(top);
        }

        Opcode::LdcI4M1 => frame.stack.push(Value::Int32(-1)),
        Opcode::LdcI40 => frame.stack.push(Value::Int32(0)),
        Opcode::LdcI41 => frame.stack.push(Value::Int32(1)),
        Opcode::LdcI42 => frame.stack.push(Value::Int32(2)),
        Opcode::LdcI43 => frame.stack.push(Value::Int32(3)),
        Opcode::LdcI44 => frame.stack.push(Value::Int32(4)),
        Opcode::LdcI45 => frame.stack.push(Value::Int32(5)),
        Opcode::LdcI46 => frame.stack.push(Value::Int32(6)),
        Opcode::LdcI47 => frame.stack.push(Value::Int32(7)),
        Opcode::LdcI48 => frame.stack.push(Value::Int32(8)),
        Opcode::LdcI4S => frame
            .stack
            .push(Value::Int32(int8_operand(instruction)? as i32)),
        Opcode::LdcI4 => frame.stack.push(Value::Int32(int32_operand(instruction)?)),
        Opcode::LdcI8 => frame.stack.push(Value::Int64(int64_operand(instruction)?)),
        Opcode::LdcR4 => frame
            .stack
            .push(Value::Float(f64::from(float32_operand(instruction)?))),
        Opcode::LdcR8 => frame
            .stack
            .push(Value::Float(float64_operand(instruction)?)),
        Opcode::Ldnull => frame.stack.push(Value::Null),

        Opcode::Ldarg0 => frame.load_arg(0)?,
        Opcode::Ldarg1 => frame.load_arg(1)?,
        Opcode::Ldarg2 => frame.load_arg(2)?,
        Opcode::Ldarg3 => frame.load_arg(3)?,
        Opcode::LdargS | Opcode::Ldarg => frame.load_arg(var_operand(instruction)?)?,
        Opcode::Ldloc0 => frame.load_local(0),
        Opcode::Ldloc1 => frame.load_local(1),
        Opcode::Ldloc2 => frame.load_local(2),
        Opcode::Ldloc3 => frame.load_local(3),
        Opcode::LdlocS | Opcode::Ldloc => frame.load_local(var_operand(instruction)?),
        Opcode::Stloc0 => frame.store_local(0)?,
        Opcode::Stloc1 => frame.store_local(1)?,
        Opcode::Stloc2 => frame.store_local(2)?,
        Opcode::Stloc3 => frame.store_local(3)?,
        Opcode::StlocS | Opcode::Stloc => frame.store_local(var_operand(instruction)?)?,
        Opcode::StargS | Opcode::Starg => frame.store_arg(var_operand(instruction)?)?,

        Opcode::Add | Opcode::Sub | Opcode::Mul | Opcode::Div | Opcode::Rem => {
            let (a, b) = frame.pop2()?;
            frame.stack.push(binary_numeric(opcode, a, b)?);
        }
        Opcode::DivUn | Opcode::RemUn | Opcode::And | Opcode::Or | Opcode::Xor => {
            let (a, b) = frame.pop2()?;
            frame.stack.push(binary_integer(opcode, a, b)?);
        }
        Opcode::Shl | Opcode::Shr | Opcode::ShrUn => {
            let (value, amount) = frame.pop2()?;
            frame.stack.push(shift(opcode, value, amount)?);
        }
        Opcode::Neg => {
            let value = frame.pop()?;
            frame.stack.push(negate(value)?);
        }
        Opcode::Not => {
            let value = frame.pop()?;
            frame.stack.push(bitwise_not(value)?);
        }

        Opcode::ConvI1
        | Opcode::ConvI2
        | Opcode::ConvI4
        | Opcode::ConvI8
        | Opcode::ConvU1
        | Opcode::ConvU2
        | Opcode::ConvU4
        | Opcode::ConvU8
        | Opcode::ConvI
        | Opcode::ConvU
        | Opcode::ConvR4
        | Opcode::ConvR8
        | Opcode::ConvRUn => {
            let value = frame.pop()?;
            frame.stack.push(convert(opcode, value)?);
        }

        Opcode::Ceq | Opcode::Cgt | Opcode::CgtUn | Opcode::Clt | Opcode::CltUn => {
            let (a, b) = frame.pop2()?;
            let result = compare(opcode, a, b)?;
            frame.stack.push(Value::Int32(i32::from(result)));
        }

        Opcode::Br | Opcode::BrS => frame.ip = branch_target(instruction, frame.code.len())?,
        Opcode::Brtrue | Opcode::BrtrueS => {
            let value = frame.pop()?;
            if value.is_truthy() {
                frame.ip = branch_target(instruction, frame.code.len())?;
            }
        }
        Opcode::Brfalse | Opcode::BrfalseS => {
            let value = frame.pop()?;
            if !value.is_truthy() {
                frame.ip = branch_target(instruction, frame.code.len())?;
            }
        }
        Opcode::Beq
        | Opcode::BeqS
        | Opcode::BneUn
        | Opcode::BneUnS
        | Opcode::Bge
        | Opcode::BgeS
        | Opcode::BgeUn
        | Opcode::BgeUnS
        | Opcode::Bgt
        | Opcode::BgtS
        | Opcode::BgtUn
        | Opcode::BgtUnS
        | Opcode::Ble
        | Opcode::BleS
        | Opcode::BleUn
        | Opcode::BleUnS
        | Opcode::Blt
        | Opcode::BltS
        | Opcode::BltUn
        | Opcode::BltUnS => {
            let (a, b) = frame.pop2()?;
            if compare(opcode, a, b)? {
                frame.ip = branch_target(instruction, frame.code.len())?;
            }
        }
        Opcode::Switch => {
            let index = unsigned_index(frame.pop()?).ok_or(Trap::TypeMismatch(Opcode::Switch))?;
            let Operand::Switch(targets) = &instruction.operand else {
                return Err(Trap::MalformedInstruction(Opcode::Switch));
            };
            if let Some(&target) = targets.get(index) {
                if target as usize >= frame.code.len() {
                    return Err(Trap::BranchOutOfRange(target));
                }
                frame.ip = target as usize;
            }
        }

        Opcode::Ldstr => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldstr))?;
            let token = token_operand(instruction)?;
            let chars = module
                .resolve_string(token)
                .ok_or(Trap::UnresolvedString(token))?;
            let reference = vm.heap_mut().alloc_string(chars);
            frame.stack.push(Value::Object(reference));
        }

        Opcode::Call | Opcode::Callvirt => {
            let module = module.ok_or(Trap::Unsupported(instruction.opcode))?;
            let token = token_operand(instruction)?;
            let method = module.resolve(token).ok_or(Trap::UnresolvedCall(token))?;
            let arg_count = module
                .method(method)
                .ok_or(Trap::NoSuchMethod(method))?
                .arg_count();
            let args = frame.take_args(arg_count)?;
            return Ok(Flow::Call { method, args });
        }

        Opcode::Ret => return Ok(Flow::Return(frame.stack.pop())),

        other => return Err(Trap::Unsupported(other)),
    }
    Ok(Flow::Next)
}

impl<'a> Frame<'a> {
    fn new(code: &'a [Instruction], method: MethodId, args: Vec<Value>) -> Frame<'a> {
        Frame {
            code,
            method,
            ip: 0,
            stack: Vec::new(),
            locals: Vec::new(),
            args,
        }
    }

    fn pop(&mut self) -> Result<Value, Trap> {
        self.stack.pop().ok_or(Trap::StackUnderflow)
    }

    /// Pops the two operands of a binary instruction, returning them in source
    /// order `(deeper, shallower)` -- the CLI's `value1, value2`.
    fn pop2(&mut self) -> Result<(Value, Value), Trap> {
        let second = self.pop()?;
        let first = self.pop()?;
        Ok((first, second))
    }

    /// Takes the top `count` values as call arguments, in declaration order:
    /// the deepest of the popped values is argument 0 (it was pushed first).
    fn take_args(&mut self, count: u16) -> Result<Vec<Value>, Trap> {
        let count = count as usize;
        if self.stack.len() < count {
            return Err(Trap::StackUnderflow);
        }
        Ok(self.stack.split_off(self.stack.len() - count))
    }

    fn load_arg(&mut self, slot: u16) -> Result<(), Trap> {
        let value = *self
            .args
            .get(slot as usize)
            .ok_or(Trap::ArgumentOutOfRange(slot))?;
        self.stack.push(value);
        Ok(())
    }

    fn load_local(&mut self, slot: u16) {
        let value = self
            .locals
            .get(slot as usize)
            .copied()
            .unwrap_or(Value::Int32(0));
        self.stack.push(value);
    }

    fn store_local(&mut self, slot: u16) -> Result<(), Trap> {
        let value = self.pop()?;
        if self.locals.len() <= slot as usize {
            self.locals.resize(slot as usize + 1, Value::Int32(0));
        }
        self.locals[slot as usize] = value;
        Ok(())
    }

    fn store_arg(&mut self, slot: u16) -> Result<(), Trap> {
        let value = self.pop()?;
        let target = self
            .args
            .get_mut(slot as usize)
            .ok_or(Trap::ArgumentOutOfRange(slot))?;
        *target = value;
        Ok(())
    }
}

/// The unsigned index a `switch` pops, or `None` if the value is not an integer
/// (III.3.66 compares the value as an unsigned integer).
fn unsigned_index(value: Value) -> Option<usize> {
    match value {
        Value::Int32(x) => Some(x as u32 as usize),
        Value::Int64(x) | Value::NativeInt(x) => Some(x as usize),
        _ => None,
    }
}

/// The integer category of a stack value, for the operand-type rules of III.1.5.
#[derive(Clone, Copy, PartialEq)]
enum IntKind {
    I32,
    I64,
    Native,
}

fn int_parts(value: Value) -> Option<(i64, IntKind)> {
    match value {
        Value::Int32(value) => Some((i64::from(value), IntKind::I32)),
        Value::Int64(value) => Some((value, IntKind::I64)),
        Value::NativeInt(value) => Some((value, IntKind::Native)),
        _ => None,
    }
}

/// Combines the operand categories of a binary integer op into the result
/// category, or `None` if the pairing is invalid (III.1.5, Tables 11 and 14):
/// `int32` and `int64` may not mix, but either may combine with native `int`.
fn combine(a: IntKind, b: IntKind) -> Option<IntKind> {
    match (a, b) {
        (IntKind::I32, IntKind::I32) => Some(IntKind::I32),
        (IntKind::I64, IntKind::I64) => Some(IntKind::I64),
        (IntKind::Native, IntKind::Native) => Some(IntKind::Native),
        (IntKind::I32, IntKind::Native) | (IntKind::Native, IntKind::I32) => Some(IntKind::Native),
        _ => None,
    }
}

fn wrap_int(kind: IntKind, value: i64) -> Value {
    match kind {
        IntKind::I32 => Value::Int32(value as i32),
        IntKind::I64 => Value::Int64(value),
        IntKind::Native => Value::NativeInt(value),
    }
}

/// `add`, `sub`, `mul`, `div`, `rem`: floating point when both operands are
/// floats, otherwise integer.
fn binary_numeric(opcode: Opcode, a: Value, b: Value) -> Result<Value, Trap> {
    if let (Value::Float(x), Value::Float(y)) = (a, b) {
        let result = match opcode {
            Opcode::Add => x + y,
            Opcode::Sub => x - y,
            Opcode::Mul => x * y,
            Opcode::Div => x / y,
            Opcode::Rem => x % y,
            _ => return Err(Trap::Unsupported(opcode)),
        };
        return Ok(Value::Float(result));
    }
    binary_integer(opcode, a, b)
}

/// The integer-only binary operations, computed at the result category's width.
fn binary_integer(opcode: Opcode, a: Value, b: Value) -> Result<Value, Trap> {
    let (av, ak) = int_parts(a).ok_or(Trap::TypeMismatch(opcode))?;
    let (bv, bk) = int_parts(b).ok_or(Trap::TypeMismatch(opcode))?;
    let kind = combine(ak, bk).ok_or(Trap::TypeMismatch(opcode))?;
    let result = if kind == IntKind::I32 {
        i64::from(integer_op_32(opcode, av as i32, bv as i32)?)
    } else {
        integer_op_64(opcode, av, bv)?
    };
    Ok(wrap_int(kind, result))
}

fn integer_op_32(opcode: Opcode, x: i32, y: i32) -> Result<i32, Trap> {
    Ok(match opcode {
        Opcode::Add => x.wrapping_add(y),
        Opcode::Sub => x.wrapping_sub(y),
        Opcode::Mul => x.wrapping_mul(y),
        Opcode::And => x & y,
        Opcode::Or => x | y,
        Opcode::Xor => x ^ y,
        Opcode::Div => x.checked_div(y).ok_or(Trap::DivideByZero)?,
        Opcode::Rem => x.checked_rem(y).ok_or(Trap::DivideByZero)?,
        Opcode::DivUn => (checked_div_u32(x as u32, y as u32)?) as i32,
        Opcode::RemUn => (checked_rem_u32(x as u32, y as u32)?) as i32,
        _ => return Err(Trap::Unsupported(opcode)),
    })
}

fn integer_op_64(opcode: Opcode, x: i64, y: i64) -> Result<i64, Trap> {
    Ok(match opcode {
        Opcode::Add => x.wrapping_add(y),
        Opcode::Sub => x.wrapping_sub(y),
        Opcode::Mul => x.wrapping_mul(y),
        Opcode::And => x & y,
        Opcode::Or => x | y,
        Opcode::Xor => x ^ y,
        Opcode::Div => x.checked_div(y).ok_or(Trap::DivideByZero)?,
        Opcode::Rem => x.checked_rem(y).ok_or(Trap::DivideByZero)?,
        Opcode::DivUn => (checked_div_u64(x as u64, y as u64)?) as i64,
        Opcode::RemUn => (checked_rem_u64(x as u64, y as u64)?) as i64,
        _ => return Err(Trap::Unsupported(opcode)),
    })
}

fn checked_div_u32(x: u32, y: u32) -> Result<u32, Trap> {
    x.checked_div(y).ok_or(Trap::DivideByZero)
}

fn checked_rem_u32(x: u32, y: u32) -> Result<u32, Trap> {
    x.checked_rem(y).ok_or(Trap::DivideByZero)
}

fn checked_div_u64(x: u64, y: u64) -> Result<u64, Trap> {
    x.checked_div(y).ok_or(Trap::DivideByZero)
}

fn checked_rem_u64(x: u64, y: u64) -> Result<u64, Trap> {
    x.checked_rem(y).ok_or(Trap::DivideByZero)
}

/// `shl`, `shr`, `shr.un`: the result has the first operand's category; the
/// second operand is the shift amount (III.1.5, Table 15).
fn shift(opcode: Opcode, value: Value, amount: Value) -> Result<Value, Trap> {
    let (raw, kind) = int_parts(value).ok_or(Trap::TypeMismatch(opcode))?;
    let (shift_by, _) = int_parts(amount).ok_or(Trap::TypeMismatch(opcode))?;
    let shift_by = shift_by as u32;
    let result = match kind {
        IntKind::I32 => {
            let x = raw as i32;
            let shifted = match opcode {
                Opcode::Shl => x.wrapping_shl(shift_by),
                Opcode::Shr => x.wrapping_shr(shift_by),
                Opcode::ShrUn => (x as u32).wrapping_shr(shift_by) as i32,
                _ => return Err(Trap::Unsupported(opcode)),
            };
            i64::from(shifted)
        }
        _ => match opcode {
            Opcode::Shl => raw.wrapping_shl(shift_by),
            Opcode::Shr => raw.wrapping_shr(shift_by),
            Opcode::ShrUn => (raw as u64).wrapping_shr(shift_by) as i64,
            _ => return Err(Trap::Unsupported(opcode)),
        },
    };
    Ok(wrap_int(kind, result))
}

fn negate(value: Value) -> Result<Value, Trap> {
    match value {
        Value::Int32(x) => Ok(Value::Int32(x.wrapping_neg())),
        Value::Int64(x) => Ok(Value::Int64(x.wrapping_neg())),
        Value::NativeInt(x) => Ok(Value::NativeInt(x.wrapping_neg())),
        Value::Float(x) => Ok(Value::Float(-x)),
        Value::Object(_) | Value::Null => Err(Trap::TypeMismatch(Opcode::Neg)),
    }
}

fn bitwise_not(value: Value) -> Result<Value, Trap> {
    match value {
        Value::Int32(x) => Ok(Value::Int32(!x)),
        Value::Int64(x) => Ok(Value::Int64(!x)),
        Value::NativeInt(x) => Ok(Value::NativeInt(!x)),
        _ => Err(Trap::TypeMismatch(Opcode::Not)),
    }
}

/// Converts the top-of-stack value per a `conv.*` opcode (III.3.27). Float to
/// integer truncates toward zero; integer narrowing truncates the high bits;
/// `conv.i*` widen by sign-extension and `conv.u*` by zero-extension; results
/// narrower than `int32` fill the slot. `conv.r.un` reads the source as unsigned.
fn convert(opcode: Opcode, value: Value) -> Result<Value, Trap> {
    if opcode == Opcode::ConvRUn {
        let unsigned = match value {
            Value::Int32(x) => u64::from(x as u32),
            Value::Int64(x) | Value::NativeInt(x) => x as u64,
            _ => return Err(Trap::TypeMismatch(opcode)),
        };
        return Ok(Value::Float(unsigned as f64));
    }

    if matches!(opcode, Opcode::ConvR4 | Opcode::ConvR8) {
        let float = match value {
            Value::Int32(x) => f64::from(x),
            Value::Int64(x) | Value::NativeInt(x) => x as f64,
            Value::Float(f) => f,
            _ => return Err(Trap::TypeMismatch(opcode)),
        };
        let float = if opcode == Opcode::ConvR4 {
            f64::from(float as f32)
        } else {
            float
        };
        return Ok(Value::Float(float));
    }

    let (source, from_32) = match value {
        Value::Int32(x) => (i64::from(x), true),
        Value::Int64(x) => (x, false),
        Value::NativeInt(x) => (x, false),
        Value::Float(f) => (f as i64, false),
        Value::Object(_) | Value::Null => return Err(Trap::TypeMismatch(opcode)),
    };
    let zero_extended = if from_32 {
        i64::from(source as u32)
    } else {
        source
    };
    Ok(match opcode {
        Opcode::ConvI1 => Value::Int32(i32::from(source as i8)),
        Opcode::ConvU1 => Value::Int32(i32::from(source as u8)),
        Opcode::ConvI2 => Value::Int32(i32::from(source as i16)),
        Opcode::ConvU2 => Value::Int32(i32::from(source as u16)),
        Opcode::ConvI4 => Value::Int32(source as i32),
        Opcode::ConvU4 => Value::Int32(source as u32 as i32),
        Opcode::ConvI8 => Value::Int64(source),
        Opcode::ConvU8 => Value::Int64(zero_extended),
        Opcode::ConvI => Value::NativeInt(source),
        Opcode::ConvU => Value::NativeInt(zero_extended),
        _ => return Err(Trap::Unsupported(opcode)),
    })
}

/// The relation a comparison or conditional branch tests.
#[derive(Clone, Copy)]
enum Relation {
    Equal,
    NotEqual,
    Less,
    LessOrEqual,
    Greater,
    GreaterOrEqual,
}

/// Decodes a comparison or conditional-branch opcode into its relation and
/// whether it is the unsigned/unordered variant.
fn relation_of(opcode: Opcode) -> Option<(Relation, bool)> {
    Some(match opcode {
        Opcode::Ceq | Opcode::Beq | Opcode::BeqS => (Relation::Equal, false),
        Opcode::BneUn | Opcode::BneUnS => (Relation::NotEqual, true),
        Opcode::Cgt | Opcode::Bgt | Opcode::BgtS => (Relation::Greater, false),
        Opcode::CgtUn | Opcode::BgtUn | Opcode::BgtUnS => (Relation::Greater, true),
        Opcode::Clt | Opcode::Blt | Opcode::BltS => (Relation::Less, false),
        Opcode::CltUn | Opcode::BltUn | Opcode::BltUnS => (Relation::Less, true),
        Opcode::Bge | Opcode::BgeS => (Relation::GreaterOrEqual, false),
        Opcode::BgeUn | Opcode::BgeUnS => (Relation::GreaterOrEqual, true),
        Opcode::Ble | Opcode::BleS => (Relation::LessOrEqual, false),
        Opcode::BleUn | Opcode::BleUnS => (Relation::LessOrEqual, true),
        _ => return None,
    })
}

/// Evaluates a comparison or conditional branch (III.1.5, Table 13). Integers
/// compare signed unless the unsigned variant is used; floats compare ordered,
/// with the unordered (NaN) result chosen by the `.un` variant.
fn compare(opcode: Opcode, a: Value, b: Value) -> Result<bool, Trap> {
    let (relation, unordered_or_unsigned) = relation_of(opcode).ok_or(Trap::Unsupported(opcode))?;
    if let (Value::Float(x), Value::Float(y)) = (a, b) {
        if x.is_nan() || y.is_nan() {
            return Ok(unordered_or_unsigned && !matches!(relation, Relation::Equal));
        }
        return Ok(apply_relation(relation, x.partial_cmp(&y)));
    }
    let (av, ak) = int_parts(a).ok_or(Trap::TypeMismatch(opcode))?;
    let (bv, bk) = int_parts(b).ok_or(Trap::TypeMismatch(opcode))?;
    let _ = (ak, bk);
    let ordering = if unordered_or_unsigned {
        (av as u64).cmp(&(bv as u64))
    } else {
        av.cmp(&bv)
    };
    Ok(apply_relation(relation, Some(ordering)))
}

fn apply_relation(relation: Relation, ordering: Option<core::cmp::Ordering>) -> bool {
    use core::cmp::Ordering::{Equal, Greater, Less};
    let Some(ordering) = ordering else {
        return false;
    };
    match relation {
        Relation::Equal => ordering == Equal,
        Relation::NotEqual => ordering != Equal,
        Relation::Less => ordering == Less,
        Relation::LessOrEqual => ordering != Greater,
        Relation::Greater => ordering == Greater,
        Relation::GreaterOrEqual => ordering != Less,
    }
}

fn branch_target(instruction: &Instruction, code_len: usize) -> Result<usize, Trap> {
    let target = match instruction.operand {
        Operand::Target(index) => index,
        _ => return Err(Trap::MalformedInstruction(instruction.opcode)),
    };
    if target as usize >= code_len {
        return Err(Trap::BranchOutOfRange(target));
    }
    Ok(target as usize)
}

fn int8_operand(instruction: &Instruction) -> Result<i8, Trap> {
    match instruction.operand {
        Operand::Int8(value) => Ok(value),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

fn int32_operand(instruction: &Instruction) -> Result<i32, Trap> {
    match instruction.operand {
        Operand::Int32(value) => Ok(value),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

fn int64_operand(instruction: &Instruction) -> Result<i64, Trap> {
    match instruction.operand {
        Operand::Int64(value) => Ok(value),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

fn float32_operand(instruction: &Instruction) -> Result<f32, Trap> {
    match instruction.operand {
        Operand::Float32(value) => Ok(value),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

fn float64_operand(instruction: &Instruction) -> Result<f64, Trap> {
    match instruction.operand {
        Operand::Float64(value) => Ok(value),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

fn var_operand(instruction: &Instruction) -> Result<u16, Trap> {
    match instruction.operand {
        Operand::Variable(slot) => Ok(slot),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

fn token_operand(instruction: &Instruction) -> Result<Token, Trap> {
    match instruction.operand {
        Operand::Token(token) => Ok(token),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cil::{EhClause, Instruction, Operand};

    fn method(code: Vec<Instruction>) -> MethodBodyImage {
        MethodBodyImage {
            max_stack: 8,
            init_locals: true,
            local_var_sig: None,
            code: code.into_boxed_slice(),
            handlers: <Box<[EhClause]>>::default(),
        }
    }

    fn run(code: Vec<Instruction>) -> Result<Option<Value>, Trap> {
        run_method(&method(code), Vec::new())
    }

    #[test]
    fn evaluates_two_plus_two() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI42),
            Instruction::simple(Opcode::LdcI42),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(4))));
    }

    #[test]
    fn subtracts_and_multiplies() {
        let result = run(vec![
            Instruction::new(Opcode::LdcI4S, Operand::Int8(10)),
            Instruction::simple(Opcode::LdcI43),
            Instruction::simple(Opcode::Sub),
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::Mul),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(28))));
    }

    #[test]
    fn integer_divide_by_zero_traps() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::LdcI40),
            Instruction::simple(Opcode::Div),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Err(Trap::DivideByZero));
    }

    #[test]
    fn add_wraps_around_like_twos_complement() {
        let result = run(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(i32::MAX)),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(i32::MIN))));
    }

    #[test]
    fn arguments_and_locals_round_trip() {
        let body = method(vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::Ldarg1),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Ret),
        ]);
        let result = run_method(&body, vec![Value::Int32(40), Value::Int32(2)]);
        assert_eq!(result, Ok(Some(Value::Int32(42))));
    }

    #[test]
    fn sums_one_to_five_with_a_loop() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI40),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Stloc1),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::Ldloc1),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(Opcode::Ldloc1),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Stloc1),
            Instruction::simple(Opcode::Ldloc1),
            Instruction::simple(Opcode::LdcI45),
            Instruction::new(Opcode::BleS, Operand::Target(4)),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(15))));
    }

    #[test]
    fn compares_with_clt() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI43),
            Instruction::simple(Opcode::LdcI45),
            Instruction::simple(Opcode::Clt),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(1))));
    }

    #[test]
    fn unsigned_compare_differs_from_signed() {
        let signed = run(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(-1)),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Clt),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(signed, Ok(Some(Value::Int32(1))));
        let unsigned = run(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(-1)),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::CltUn),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(unsigned, Ok(Some(Value::Int32(0))));
    }

    #[test]
    fn float_arithmetic() {
        let result = run(vec![
            Instruction::new(Opcode::LdcR8, Operand::Float64(1.5)),
            Instruction::new(Opcode::LdcR8, Operand::Float64(2.0)),
            Instruction::simple(Opcode::Mul),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Float(3.0))));
    }

    fn run_convert(opcode: Opcode, value: Operand, load: Opcode) -> Result<Option<Value>, Trap> {
        run(vec![
            Instruction::new(load, value),
            Instruction::simple(opcode),
            Instruction::simple(Opcode::Ret),
        ])
    }

    #[test]
    fn conv_i4_truncates_a_float_toward_zero() {
        let result = run_convert(Opcode::ConvI4, Operand::Float64(3.9), Opcode::LdcR8);
        assert_eq!(result, Ok(Some(Value::Int32(3))));
    }

    #[test]
    fn conv_i1_sign_extends_but_conv_u1_zero_extends() {
        let signed = run_convert(Opcode::ConvI1, Operand::Int32(0x1FF), Opcode::LdcI4);
        assert_eq!(signed, Ok(Some(Value::Int32(-1))));
        let unsigned = run_convert(Opcode::ConvU1, Operand::Int32(0x1FF), Opcode::LdcI4);
        assert_eq!(unsigned, Ok(Some(Value::Int32(255))));
    }

    #[test]
    fn conv_i8_sign_extends_but_conv_u8_zero_extends_int32() {
        let signed = run_convert(Opcode::ConvI8, Operand::Int32(-1), Opcode::LdcI4);
        assert_eq!(signed, Ok(Some(Value::Int64(-1))));
        let unsigned = run_convert(Opcode::ConvU8, Operand::Int32(-1), Opcode::LdcI4);
        assert_eq!(unsigned, Ok(Some(Value::Int64(0xFFFF_FFFF))));
    }

    #[test]
    fn conv_r8_is_signed_but_conv_r_un_is_unsigned() {
        let signed = run_convert(Opcode::ConvR8, Operand::Int32(-1), Opcode::LdcI4);
        assert_eq!(signed, Ok(Some(Value::Float(-1.0))));
        let unsigned = run_convert(Opcode::ConvRUn, Operand::Int32(-1), Opcode::LdcI4);
        assert_eq!(unsigned, Ok(Some(Value::Float(4_294_967_295.0))));
    }

    #[test]
    fn converting_a_reference_traps() {
        let result = run(vec![
            Instruction::simple(Opcode::Ldnull),
            Instruction::simple(Opcode::ConvI4),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Err(Trap::TypeMismatch(Opcode::ConvI4)));
    }

    #[test]
    fn switch_selects_a_case_or_falls_through() {
        let code = vec![
            Instruction::simple(Opcode::Ldarg0),
            Instruction::new(
                Opcode::Switch,
                Operand::Switch(vec![4, 6].into_boxed_slice()),
            ),
            Instruction::new(Opcode::LdcI4, Operand::Int32(100)),
            Instruction::simple(Opcode::Ret),
            Instruction::new(Opcode::LdcI4, Operand::Int32(10)),
            Instruction::simple(Opcode::Ret),
            Instruction::new(Opcode::LdcI4, Operand::Int32(20)),
            Instruction::simple(Opcode::Ret),
        ];
        let body = method(code);
        assert_eq!(
            run_method(&body, vec![Value::Int32(0)]),
            Ok(Some(Value::Int32(10)))
        );
        assert_eq!(
            run_method(&body, vec![Value::Int32(1)]),
            Ok(Some(Value::Int32(20)))
        );
        assert_eq!(
            run_method(&body, vec![Value::Int32(5)]),
            Ok(Some(Value::Int32(100)))
        );
    }

    #[test]
    fn starg_overwrites_an_argument() {
        let body = method(vec![
            Instruction::new(Opcode::LdcI4, Operand::Int32(99)),
            Instruction::new(Opcode::StargS, Operand::Variable(0)),
            Instruction::simple(Opcode::Ldarg0),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(
            run_method(&body, vec![Value::Int32(1)]),
            Ok(Some(Value::Int32(99)))
        );
    }

    #[test]
    fn stack_underflow_traps() {
        assert_eq!(
            run(vec![Instruction::simple(Opcode::Add)]),
            Err(Trap::StackUnderflow)
        );
    }

    #[test]
    fn unsupported_opcode_traps() {
        let result = run(vec![Instruction::new(
            Opcode::Newobj,
            Operand::Token(lamella_token::Token(0x0A00_0001)),
        )]);
        assert_eq!(result, Err(Trap::Unsupported(Opcode::Newobj)));
    }

    #[test]
    fn falling_off_the_end_traps() {
        assert_eq!(
            run(vec![Instruction::simple(Opcode::Nop)]),
            Err(Trap::FellThroughEnd)
        );
    }

    use crate::module::Module;
    use lamella_token::Token;

    const ADD_TOKEN: Token = Token(0x0600_0002);
    const SELF_TOKEN: Token = Token(0x0600_0003);

    #[test]
    fn static_call_adds_two_arguments() {
        let mut module = Module::new();
        let add = module.add_method(
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            2,
        );
        let main = module.add_method(
            method(vec![
                Instruction::new(Opcode::LdcI4S, Operand::Int8(40)),
                Instruction::simple(Opcode::LdcI42),
                Instruction::new(Opcode::Call, Operand::Token(ADD_TOKEN)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        module.bind_token(ADD_TOKEN, add);

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(42)))
        );
    }

    #[test]
    fn recursion_sums_one_to_n_across_frames() {
        let mut module = Module::new();
        let sum = module.add_method(
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::LdcI40),
                Instruction::new(Opcode::BgtS, Operand::Target(5)),
                Instruction::simple(Opcode::LdcI40),
                Instruction::simple(Opcode::Ret),
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::Sub),
                Instruction::new(Opcode::Call, Operand::Token(SELF_TOKEN)),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            1,
        );
        module.bind_token(SELF_TOKEN, sum);

        assert_eq!(
            super::run(&module, &mut Vm::new(), sum, alloc::vec![Value::Int32(5)]),
            Ok(Some(Value::Int32(15)))
        );
    }

    #[test]
    fn an_unbound_call_token_traps() {
        let mut module = Module::new();
        let main = module.add_method(
            method(vec![Instruction::new(
                Opcode::Call,
                Operand::Token(ADD_TOKEN),
            )]),
            0,
        );
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Err(Trap::UnresolvedCall(ADD_TOKEN))
        );
    }

    #[test]
    fn runaway_recursion_traps_instead_of_crashing() {
        let mut module = Module::new();
        let loop_method = module.add_method(
            method(vec![
                Instruction::new(Opcode::Call, Operand::Token(SELF_TOKEN)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        module.bind_token(SELF_TOKEN, loop_method);

        assert_eq!(
            super::run(&module, &mut Vm::new(), loop_method, Vec::new()),
            Err(Trap::CallStackOverflow)
        );
    }

    #[test]
    fn session_single_steps_and_inspects_the_stack() {
        let mut module = Module::new();
        let main = module.add_method(
            method(vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::LdcI43),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        let mut vm = Vm::new();
        let mut session = Session::new(&module, main, Vec::new()).unwrap();

        assert_eq!(session.frame(0).unwrap().ip, 0);
        assert!(session.frame(0).unwrap().stack.is_empty());
        assert_eq!(session.step(&mut vm), Ok(Status::Running));
        assert_eq!(
            session.frame(0).unwrap().stack,
            [Value::Int32(2)].as_slice()
        );
        assert_eq!(session.step(&mut vm), Ok(Status::Running));
        assert_eq!(session.step(&mut vm), Ok(Status::Running));
        assert_eq!(
            session.frame(0).unwrap().stack,
            [Value::Int32(5)].as_slice()
        );
        assert_eq!(
            session.step(&mut vm),
            Ok(Status::Done(Some(Value::Int32(5))))
        );
    }

    #[test]
    fn session_pauses_at_a_breakpoint() {
        let mut module = Module::new();
        let main = module.add_method(
            method(vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::LdcI43),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        let mut vm = Vm::new();
        let mut session = Session::new(&module, main, Vec::new()).unwrap();
        session.add_breakpoint(main, 2);

        assert_eq!(session.resume(&mut vm), Ok(Status::Paused));
        assert_eq!(session.frame(0).unwrap().ip, 2);
        assert_eq!(
            session.frame(0).unwrap().stack,
            [Value::Int32(2), Value::Int32(3)].as_slice()
        );
        assert_eq!(session.step(&mut vm), Ok(Status::Running));
        assert_eq!(
            session.resume(&mut vm),
            Ok(Status::Done(Some(Value::Int32(5))))
        );
    }

    #[test]
    fn session_exposes_the_call_stack_at_a_breakpoint() {
        let mut module = Module::new();
        let add = module.add_method(
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            2,
        );
        let main = module.add_method(
            method(vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::LdcI43),
                Instruction::new(Opcode::Call, Operand::Token(ADD_TOKEN)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        module.bind_token(ADD_TOKEN, add);
        let mut vm = Vm::new();
        let mut session = Session::new(&module, main, Vec::new()).unwrap();
        session.add_breakpoint(add, 0);

        assert_eq!(session.resume(&mut vm), Ok(Status::Paused));
        assert_eq!(session.depth(), 2);
        assert_eq!(session.frame(0).unwrap().method, main);
        assert_eq!(session.frame(1).unwrap().method, add);
        assert_eq!(
            session.frame(1).unwrap().args,
            [Value::Int32(2), Value::Int32(3)].as_slice()
        );
        assert_eq!(session.step(&mut vm), Ok(Status::Running));
        assert_eq!(
            session.resume(&mut vm),
            Ok(Status::Done(Some(Value::Int32(5))))
        );
    }
}
