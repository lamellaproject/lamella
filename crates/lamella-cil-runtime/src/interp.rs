//! A tree-of-frames CIL interpreter over a hand-built method body.

#[cfg(feature = "bcl")]
use crate::module::IntrinsicFn;
use crate::module::{Method, MethodId, Module, asm_key};
use crate::object::{Heap, ObjectRef};
use crate::trap::Trap;
use crate::value::{Location, Value};
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use lamella_cil::{
    EhClause, EhKind, Instruction, InstructionRange, MethodBodyImage, Opcode, Operand,
};
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
    /// The DEBUG channel -- the bytes `System.Diagnostics.Debug`'s `DefaultTraceListener`
    /// emits, kept separate from `output` (the Console). Conceptually the developer's debug
    /// sink: a host renders it to STDERR, distinct from Console.Out's STDOUT, so a bare
    /// `Debug.WriteLine` (no listener config) is seen without polluting program output. Empty
    /// unless `debug_write` is called.
    debug_output: Vec<u16>,
    statics: Vec<Value>,
    /// The message string of each exception object, kept as runtime side-state so
    /// `Exception.Message` works without modeling mscorlib's field layout (and so the
    /// message-stripping knob has one place to act). Keyed by the exception object.
    exception_messages: BTreeMap<ObjectRef, ObjectRef>,
    /// The base-chain TAG vector of a runtime-fault exception object -- one whose managed
    /// type is external to the loaded module (`EXTERNAL_TYPE_ID`), so it has no live base
    /// chain to walk. A catchable fault (`catchable_fault`) records the vector of its .NET
    /// exception type here (leaf-first up to `System.Object`), so the handler search tests a
    /// `catch`'s tag against it exactly as it does a managed exception's
    /// [`Module::exception_base_chain`]. Keyed by the fault object; empty/absent without the
    /// `exceptions` feature.
    #[cfg(feature = "exceptions")]
    exception_chains: BTreeMap<ObjectRef, Vec<u32>>,
    /// Whether a finalizer is currently running: the collector is paused (a GC triggered
    /// from within a finalizer is a no-op) so the in-flight f-reachable list stays valid.
    #[cfg(feature = "finalizers")]
    finalizing: bool,
    /// Set by `GC.Collect`: the next safepoint collects regardless of the heap threshold.
    #[cfg(feature = "gc")]
    force_collect: bool,
    /// A nesting depth that suspends collection while non-zero. A reflective intrinsic that
    /// builds objects across a nested interpreter run (`GetCustomAttributes` instantiates each
    /// attribute by running its constructor) raises this for the duration: the moving collector
    /// remaps only roots it can reach through the running session's frames, so an instance the
    /// intrinsic holds in a Rust local -- or the result array it accumulates into -- would
    /// otherwise dangle if a nested constructor crossed a GC safepoint. Balanced by
    /// [`Vm::suspend_collection`] / [`Vm::resume_collection`].
    #[cfg(feature = "gc")]
    collect_suspend: u32,
    /// The exception currently propagating without (yet) a handler: set the moment a
    /// `throw`/`rethrow` begins its search and cleared the instant a `catch`/`filter`
    /// accepts it, so if the search exhausts the call stack this still holds the culprit.
    /// A [`Session`] armed with [`Session::set_pause_on_unhandled_exception`] reads it to
    /// report the object on a [`StopReason::Exception`] pause. Always `None` (and never set)
    /// without the `exceptions` feature.
    #[cfg(feature = "exceptions")]
    unhandled: Option<ObjectRef>,
    /// The current time, as a count of 100-nanosecond ticks since the .NET epoch
    /// (0001-01-01 00:00:00). The interpreter core is no_std and has no clock of its own:
    /// the embedder sets this (the host from `std::time`, a device from its RTC) via
    /// [`Vm::set_now_ticks`], and `DateTime.Now`/`UtcNow`/`Today` read it through the
    /// `datetime_now_ticks` intrinsic. It defaults to 0 (the epoch) when the embedder
    /// never sets it, so an unconfigured host reads a defined value rather than garbage.
    now_ticks: i64,
}

impl Vm {
    /// Creates a fresh runtime context with an empty heap and no output.
    #[must_use]
    pub fn new() -> Vm {
        Vm::default()
    }

    /// Requests a collection at the next safepoint (`GC.Collect`).
    #[cfg(feature = "gc")]
    pub fn request_collect(&mut self) {
        self.force_collect = true;
    }

    /// Takes and clears any pending forced-collection request.
    #[cfg(feature = "gc")]
    pub fn take_force_collect(&mut self) -> bool {
        core::mem::take(&mut self.force_collect)
    }

    /// Suspends collection until the matching [`Vm::resume_collection`] (nestable). A
    /// reflective intrinsic that constructs objects across a nested interpreter run holds this
    /// so an in-progress instance it roots only in a Rust local cannot be relocated/reclaimed by
    /// a GC the nested run triggers.
    #[cfg(feature = "gc")]
    pub fn suspend_collection(&mut self) {
        self.collect_suspend = self.collect_suspend.saturating_add(1);
    }

    /// Ends one [`Vm::suspend_collection`]. Collection resumes once every suspension is lifted.
    #[cfg(feature = "gc")]
    pub fn resume_collection(&mut self) {
        self.collect_suspend = self.collect_suspend.saturating_sub(1);
    }

    /// Whether collection is currently suspended (a non-zero suspension depth).
    #[cfg(feature = "gc")]
    #[must_use]
    pub fn collection_suspended(&self) -> bool {
        self.collect_suspend != 0
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

    /// Appends UTF-16 code units to the DEBUG channel (the `Debug.WriteLine` sink). Kept
    /// separate from [`Vm::write`] so the host can route it to STDERR, distinct from the
    /// Console's STDOUT.
    pub fn debug_write(&mut self, chars: &[u16]) {
        self.debug_output.extend_from_slice(chars);
    }

    /// The DEBUG-channel output so far, as UTF-16 code units.
    #[must_use]
    pub fn debug_output(&self) -> &[u16] {
        &self.debug_output
    }

    /// The DEBUG-channel output so far, decoded to a `String` (lossily, for display
    /// and tests).
    #[must_use]
    pub fn debug_output_string(&self) -> String {
        String::from_utf16_lossy(&self.debug_output)
    }

    /// The console output so far, decoded to a `String` (lossily, for display
    /// and tests).
    #[must_use]
    pub fn output_string(&self) -> String {
        String::from_utf16_lossy(&self.output)
    }

    /// Initializes the static-field storage from `defaults` on first use; idempotent
    /// once sized, so it never clobbers values written by `stsfld`.
    pub fn init_statics(&mut self, defaults: &[Value]) {
        if self.statics.len() < defaults.len() {
            self.statics = defaults.to_vec();
        }
    }

    /// The value of static field `slot`, if storage holds it.
    #[must_use]
    pub fn static_field(&self, slot: usize) -> Option<Value> {
        self.statics.get(slot).cloned()
    }

    /// Stores `value` into static field `slot` (a no-op if out of range).
    pub fn set_static_field(&mut self, slot: usize, value: Value) {
        if let Some(target) = self.statics.get_mut(slot) {
            *target = value;
        }
    }

    /// Sets the current time the host clock seam reports, as 100-nanosecond ticks since
    /// the .NET epoch (0001-01-01 00:00:00). The embedder supplies this -- the host from
    /// `std::time`, a device from its RTC -- and `DateTime.Now`/`UtcNow`/`Today` read it.
    /// For v1 these are all UTC-based (no timezone), so `Now` and `UtcNow` report the same
    /// value.
    pub fn set_now_ticks(&mut self, ticks: i64) {
        self.now_ticks = ticks;
    }

    /// The current time in 100-nanosecond ticks since the .NET epoch, as last set by
    /// [`Vm::set_now_ticks`] (0 -- the epoch -- if never set). The `datetime_now_ticks`
    /// intrinsic returns this to the managed `DateTime.Now`/`UtcNow`.
    #[must_use]
    pub fn now_ticks(&self) -> i64 {
        self.now_ticks
    }

    /// Records `message` (a heap string) as `exception`'s message, for
    /// `Exception.Message`.
    pub fn set_exception_message(&mut self, exception: ObjectRef, message: ObjectRef) {
        self.exception_messages.insert(exception, message);
    }

    /// The recorded message string of `exception`, if any.
    #[must_use]
    pub fn exception_message(&self, exception: ObjectRef) -> Option<ObjectRef> {
        self.exception_messages.get(&exception).copied()
    }

    /// Records the base-chain TAG vector of a runtime-fault `exception` (a type external to the
    /// loaded module), so the handler search can match a `catch` against it. The `chain` is
    /// leaf-first up to `System.Object`.
    #[cfg(feature = "exceptions")]
    fn set_exception_chain(&mut self, exception: ObjectRef, chain: Vec<u32>) {
        self.exception_chains.insert(exception, chain);
    }

    /// The recorded base-chain tag vector of a runtime-fault `exception`, if one was set.
    #[cfg(feature = "exceptions")]
    #[must_use]
    fn exception_chain(&self, exception: ObjectRef) -> Option<&[u32]> {
        self.exception_chains.get(&exception).map(Vec::as_slice)
    }

    /// Records `exception` as the one currently propagating (set as a throw/fault begins its
    /// search), so an unhandled-exception pause can report it if the search exhausts the
    /// stack. The stepping loop discards it the moment a handler accepts (the unwind returns
    /// without a trap), so it cannot mis-attribute a later exception.
    #[cfg(feature = "exceptions")]
    fn note_unhandled(&mut self, exception: ObjectRef) {
        self.unhandled = Some(exception);
    }

    /// Takes the exception that escaped without a handler, if one did.
    #[cfg(feature = "exceptions")]
    fn take_unhandled(&mut self) -> Option<ObjectRef> {
        self.unhandled.take()
    }
}

/// A multicast delegate's bound method: a `(target, method)` pair.
type Invocation = (Value, u32);

/// A multicast `Invoke` in progress: the remaining invocations and the arguments each
/// is called with.
type Multicast = (Vec<Invocation>, Vec<Value>);

/// One activation frame: the evaluation stack, the local variables, the
/// arguments, the instruction pointer, and which method is running, for a single
/// method invocation.
struct Frame {
    method: MethodId,
    ip: usize,
    stack: Vec<Value>,
    locals: Vec<Value>,
    args: Vec<Value>,
    /// Set on a constructor frame created by `newobj`: the new object to leave on
    /// the caller's stack when the frame returns (a ctor returns `void`, but the
    /// object reference is `newobj`'s result).
    new_object: Option<ObjectRef>,
    /// Set on a value-type constructor frame (`newobj` of a struct): the location of the
    /// temporary the ctor built in place, whose struct VALUE -- not a heap reference -- is
    /// left on the caller's stack when the frame returns.
    new_value: Option<Location>,
    /// The exception currently being handled in a catch block of this frame, so
    /// `rethrow` can re-propagate it.
    current_exception: Option<ObjectRef>,
    /// An in-progress `finally` chain (from a `leave` or an exception unwind).
    pending: Option<PendingFinally>,
    /// A `filter` expression being evaluated mid-unwind: the exception, the handler to
    /// enter if it accepts, and where to resume the search if it rejects.
    pending_filter: Option<PendingFilter>,
    /// A multicast-delegate invocation in progress: the remaining `(target, method)`
    /// invocations and the shared arguments, so each is called as the previous returns.
    multicast: Option<Multicast>,
    /// A `constrained.` prefix awaiting the next `callvirt`: the type to resolve the call
    /// against (the receiver stays a managed pointer to the value type).
    pending_constraint: Option<Token>,
    /// The frame's `localloc` (`stackalloc`) buffers: flat zeroed byte blocks a managed
    /// pointer (`Location::Stack`) indexes into. They live as long as the frame and are
    /// dropped with it on return, giving `stackalloc`'s lifetime. Empty for the common
    /// method that allocates none.
    buffers: Vec<Vec<u8>>,
}

/// What executing one instruction decided.
#[cfg_attr(not(feature = "exceptions"), allow(dead_code))]
enum Flow {
    /// Continue with the next instruction (or wherever a branch set `ip`).
    Next,
    /// The method returned, with its result if any.
    Return(Option<Value>),
    /// `jmp`: replace the current frame with a tail call to this method, reusing the
    /// current frame's arguments.
    Jmp(MethodId),
    /// The method called another; its frame must be pushed.
    Call {
        /// The callee.
        method: MethodId,
        /// The arguments taken from the caller's stack, in declaration order.
        args: Vec<Value>,
    },
    /// `newobj` allocated an object and must run its constructor: push a ctor frame
    /// with `this` (the new `object`) ahead of `args`, then leave `object` on the
    /// caller's stack when it returns.
    NewObj {
        /// The constructor to run.
        ctor: MethodId,
        /// The freshly allocated, zero-initialized object.
        object: ObjectRef,
        /// The constructor arguments (without `this`), from the caller's stack.
        args: Vec<Value>,
    },
    /// `newobj` of a value type: run its constructor against a managed pointer to a
    /// zero-initialized struct temporary, then leave that struct's VALUE (not a heap
    /// reference) on the caller's stack when it returns.
    NewValueObj {
        /// The constructor to run.
        ctor: MethodId,
        /// The managed pointer the ctor receives as `this` -- a temporary holding the
        /// zero struct, whose value is read back on return.
        location: Location,
        /// The constructor arguments (without `this`), from the caller's stack.
        args: Vec<Value>,
    },
    /// A `throw` or `rethrow` is propagating an exception; the call stack must be
    /// searched for a handler.
    Throw(ObjectRef),
    /// A `leave`: exit a protected/handler region to this instruction index, after
    /// running any `finally` blocks being exited.
    Leave(usize),
    /// An `endfinally`: the current `finally` block is done; resume the chain.
    EndFinally,
    /// `endfilter`: a filter expression finished; the bool is whether it accepts (catch)
    /// or rejects (continue the handler search).
    EndFilter(bool),
    /// `ldfld` through a managed pointer (`&`): read a field of the value-type instance
    /// at `location`, which lives in the call stack `step` cannot reach.
    LoadField {
        /// The struct's location (a frame local/arg, or heap storage).
        location: Location,
        /// The field token (resolved to a slot by `advance`).
        field: Token,
    },
    /// `stfld` through a managed pointer (`&`): write a field of the value-type instance
    /// at `location`, materializing the struct if the slot is still empty.
    StoreField {
        /// The struct's location.
        location: Location,
        /// The field token.
        field: Token,
        /// The value to store.
        value: Value,
    },
    /// `initobj`: zero-initialize the value-type instance at `location` (a default
    /// struct).
    InitObj {
        /// The location to initialize.
        location: Location,
        /// The value type's token (giving its zero fields).
        kind: Token,
    },
    /// `ldobj`: load the value-type instance at `location` onto the evaluation stack.
    LoadObj {
        /// The location to read.
        location: Location,
    },
    /// `stobj`: store a value-type instance through a managed pointer to `location`.
    StoreObj {
        /// The location to write.
        location: Location,
        /// The value to store.
        value: Value,
    },
    /// `cpobj`: copy the value-type instance at `src` to `dest` (both managed pointers).
    CopyObj {
        /// The destination location to write.
        dest: Location,
        /// The source location to read.
        src: Location,
    },
    /// A multicast delegate's `Invoke`: call each `(target, method)` in turn (each with
    /// `params`); the delegate's result is the last one's.
    InvokeMulticast {
        /// The bound methods to call, in order.
        invocations: Vec<(Value, u32)>,
        /// The arguments shared by every call (the delegate's parameters).
        params: Vec<Value>,
    },
}

/// What to do once a chain of `finally` blocks (run by a `leave` or an exception)
/// finishes.
enum AfterFinally {
    /// A `leave`: branch to this instruction index.
    Goto(usize),
    /// The exception was caught in this frame: enter the catch handler with it.
    Catch {
        /// The catch handler's first instruction.
        handler: usize,
        /// The exception to hand the handler.
        exception: ObjectRef,
    },
    /// The exception was not caught in this frame: pop the frame and keep unwinding.
    Unwind(ObjectRef),
}

/// A frame's in-progress `finally` chain: the remaining handler starts to run
/// (innermost first, via `pop`) and what to do once they are all done.
struct PendingFinally {
    finallys: Vec<usize>,
    then: AfterFinally,
}

/// A `filter` expression being evaluated during the handler search for an exception.
struct PendingFilter {
    /// The exception being filtered.
    exception: ObjectRef,
    /// The handler to enter if the filter accepts (leaves a non-zero result).
    handler: usize,
    /// The filter's try region, for the finallys to run before entering its handler.
    filter_try: InstructionRange,
    /// The clause index to resume the search from if the filter rejects.
    resume: usize,
    /// The original fault site, preserved across the filter's evaluation.
    fault_ip: usize,
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
    let mut frame = Frame::new(0, args);
    loop {
        let instruction = body.code.get(frame.ip).ok_or(Trap::FellThroughEnd)?;
        frame.ip += 1;
        match step(&mut frame, 0, &body.code, None, &mut vm, instruction)? {
            Flow::Next => {}
            Flow::Return(result) => return Ok(result),
            Flow::Call { .. } => return Err(Trap::Unsupported(Opcode::Call)),
            Flow::NewObj { .. } | Flow::NewValueObj { .. } => {
                return Err(Trap::Unsupported(Opcode::Newobj));
            }
            Flow::Throw(_) => return Err(Trap::Unsupported(Opcode::Throw)),
            Flow::Leave(_) => return Err(Trap::Unsupported(Opcode::Leave)),
            Flow::EndFinally => return Err(Trap::Unsupported(Opcode::Endfinally)),
            Flow::EndFilter(_) => return Err(Trap::Unsupported(Opcode::Endfilter)),
            Flow::Jmp(_) => return Err(Trap::Unsupported(Opcode::Jmp)),
            Flow::LoadField { .. } => return Err(Trap::Unsupported(Opcode::Ldfld)),
            Flow::StoreField { .. } => return Err(Trap::Unsupported(Opcode::Stfld)),
            Flow::InitObj { .. } => return Err(Trap::Unsupported(Opcode::Initobj)),
            Flow::LoadObj { .. } => return Err(Trap::Unsupported(Opcode::Ldobj)),
            Flow::StoreObj { .. } => return Err(Trap::Unsupported(Opcode::Stobj)),
            Flow::CopyObj { .. } => return Err(Trap::Unsupported(Opcode::Cpobj)),
            Flow::InvokeMulticast { .. } => return Err(Trap::Unsupported(Opcode::Callvirt)),
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
    for &cctor in module.static_ctors() {
        Session::new(module, cctor, Vec::new())?.run(module, vm)?;
    }
    Session::new(module, entry, args)?.run(module, vm)
}

/// A steppable, inspectable execution: the foundation the debugger drives.
///
/// A `Session` owns the call stack and advances one CIL instruction at a time,
/// which is what instruction-level (source-free) debugging needs. It can run to
/// completion ([`Session::run`]), single-step ([`Session::step`]), or resume until
/// a breakpoint ([`Session::resume`]); the call stack is open to inspection
/// throughout ([`Session::frame`]). The module and the runtime context ([`Vm`]) are
/// passed in on each call rather than owned -- the session borrows neither, so a
/// debugger can own the module it steps, and the heap and console outlive a session.
pub struct Session {
    frames: Vec<Frame>,
    /// Instruction breakpoints keyed by `(method, instruction-index)`, each mapped to
    /// whether it is enabled. A disabled breakpoint is remembered (so a debugger can
    /// toggle it back on) but does not pause execution. A `BTreeMap` rather than a set so
    /// enable/disable need not re-send the address.
    breakpoints: BTreeMap<(MethodId, u32), bool>,
    result: Option<Option<Value>>,
    /// When set, an exception that unwinds off the bottom of the call stack pauses the
    /// session ([`StopReason::Exception`]) with the thrown object reported, rather than
    /// ending the run with [`Trap::UnhandledException`]. Off by default, so
    /// [`Session::run`]/[`Session::resume`]/[`Session::step`] keep their abort-on-unhandled
    /// behaviour; a debugger opts in via [`Session::set_pause_on_unhandled_exception`].
    #[cfg(feature = "exceptions")]
    pause_on_exception: bool,
    /// The exception object an unhandled-exception pause parked on, so
    /// [`Session::stopped_exception`] can report it. The call stack has unwound by then (an
    /// unhandled exception is terminal), so the object -- not the frames -- is the
    /// inspectable artifact. Cleared when the run advances again; always `None` without
    /// `pause_on_exception`.
    #[cfg(feature = "exceptions")]
    unhandled_exception: Option<ObjectRef>,
}

/// A code location: a method and the index of an instruction within it -- the unit a
/// breakpoint addresses and a [`Stop`] reports. This is the interpreter-side ("CIL
/// offset") location the device-agnostic debug seam carries; a driver maps it to source
/// (via a PDB) or to a wire address as it sees fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeLocation {
    /// The method the location is in.
    pub method: MethodId,
    /// The index (not byte offset) of the instruction within the method.
    pub instruction: u32,
}

/// Why a [`Session`] run loop ([`Session::continue_`], [`Session::step_into`],
/// [`Session::step_over`], [`Session::step_out`]) stopped.
///
/// This is the device-agnostic stop seam: the on-device wireline stub and the host
/// `lamella-dap` backend both read it. It is the [`Session`]-level counterpart of the
/// host adapter's `Stop`, kept in the no_std core so a device driver shares it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Paused at the program's first instruction (the explicit stop-at-entry a debugger
    /// requests before any code runs). Reserved for a driver that begins paused; the run
    /// loops here never originate it (they report `Breakpoint` / `Step` / `Exited`).
    EntryPoint,
    /// Paused before an enabled breakpoint's instruction.
    Breakpoint,
    /// A single-step (into/over/out) completed.
    Step,
    /// The program ran to completion. The [`Stop`]'s `location` is then `None` and its
    /// `returned` holds the entry method's result.
    Exited,
    /// An exception unwound off the bottom of the call stack and the session was
    /// configured to pause on it ([`Session::set_pause_on_unhandled_exception`]); the call
    /// stack has unwound, and [`Session::stopped_exception`] gives the thrown object.
    #[cfg(feature = "exceptions")]
    Exception,
}

/// The outcome of a [`Session`] run-loop call: why it stopped, and where (the location of
/// the top frame when it stopped, or `None` once the program has finished and the call
/// stack is empty).
#[derive(Debug, Clone, PartialEq)]
pub struct Stop {
    /// Why the run loop stopped.
    pub reason: StopReason,
    /// The location execution is paused at (the top frame's method + instruction), or
    /// `None` if the program ran to completion.
    pub location: Option<CodeLocation>,
    /// The value the entry method returned, present only when the program ran to
    /// completion (`location` is then `None`).
    pub returned: Option<Value>,
}

/// One named, inspectable value -- a field or element a debugger drilled into via
/// [`Session::expand`]. A driver renders/types the `value` itself (e.g. `lamella-dap`'s
/// `format_value`); the core supplies the name and the raw [`Value`].
#[derive(Debug, Clone)]
pub struct NamedValue {
    /// The display name (`field0` for an instance field by slot, `[2]` for an array
    /// element, `value` for a box's content).
    pub name: String,
    /// The value itself, for display or further expansion.
    pub value: Value,
}

/// Whether `value` can be drilled into by [`Session::expand`] -- an object instance with
/// fields, an array with elements, or a box. A bare scalar cannot.
#[must_use]
fn is_expandable(vm: &Vm, value: &Value) -> bool {
    match value {
        Value::Object(reference) => {
            vm.heap().array_len(*reference).is_some()
                || vm.heap().boxed_value(*reference).is_some()
                || matches!(vm.heap().get(*reference), Some(crate::object::Object::Instance { .. }))
        }
        _ => false,
    }
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

impl Session {
    /// Starts a session at `entry` with `args`. The `module` is used to set up the
    /// first frame but not retained; pass it again to each step/resume/run.
    ///
    /// # Errors
    /// Returns [`Trap::NoSuchMethod`] if `entry` is not a managed method.
    pub fn new(module: &Module, entry: MethodId, args: Vec<Value>) -> Result<Session, Trap> {
        Ok(Session {
            frames: alloc::vec![new_frame(module, entry, args)?],
            breakpoints: BTreeMap::new(),
            result: None,
            #[cfg(feature = "exceptions")]
            pause_on_exception: false,
            #[cfg(feature = "exceptions")]
            unhandled_exception: None,
        })
    }

    /// Sets (and enables) a breakpoint before instruction `instruction` of `method`. Setting
    /// one that already exists re-enables it.
    pub fn add_breakpoint(&mut self, method: MethodId, instruction: u32) {
        self.breakpoints.insert((method, instruction), true);
    }

    /// Clears a breakpoint set by [`Session::add_breakpoint`] (forgotten entirely, unlike
    /// [`Session::set_breakpoint_enabled`] with `false`, which keeps it disabled).
    pub fn remove_breakpoint(&mut self, method: MethodId, instruction: u32) {
        self.breakpoints.remove(&(method, instruction));
    }

    /// Enables or disables the breakpoint at `(method, instruction)`, creating it (in the
    /// requested state) if it does not exist. A disabled breakpoint is remembered but does
    /// not pause execution -- the toggle a DAP `setBreakpoints` with `enabled: false`, or a
    /// wireline `Disable`, maps to.
    pub fn set_breakpoint_enabled(&mut self, method: MethodId, instruction: u32, enabled: bool) {
        self.breakpoints.insert((method, instruction), enabled);
    }

    /// Whether an enabled breakpoint is set at `(method, instruction)`.
    #[must_use]
    pub fn is_breakpoint_enabled(&self, method: MethodId, instruction: u32) -> bool {
        self.breakpoints
            .get(&(method, instruction))
            .copied()
            .unwrap_or(false)
    }

    /// Removes all breakpoints (e.g. when a debugger replaces the whole set).
    pub fn clear_breakpoints(&mut self) {
        self.breakpoints.clear();
    }

    /// Whether the session is currently sitting on an enabled breakpoint -- the next
    /// [`Session::resume`] / [`Session::continue_`] would pause immediately. A debugger uses
    /// this to step off a breakpoint before continuing.
    #[must_use]
    pub fn is_at_breakpoint(&self) -> bool {
        self.at_breakpoint()
    }

    /// Makes an exception that unwinds off the bottom of the call stack pause the session
    /// ([`StopReason::Exception`], the faulting frames still inspectable) instead of ending
    /// the run with [`Trap::UnhandledException`]. Only the new [`Session::continue_`] /
    /// `step_*` loop honours it; [`Session::run`]/[`Session::resume`]/[`Session::step`] keep
    /// aborting, so existing callers (and the differential) are unaffected. No-op without the
    /// `exceptions` feature (nothing throws).
    #[cfg(feature = "exceptions")]
    pub fn set_pause_on_unhandled_exception(&mut self, pause: bool) {
        self.pause_on_exception = pause;
    }

    /// The exception object the session is parked on after a [`StopReason::Exception`] stop,
    /// if any. The faulting frames sit beneath it ([`Session::frame`]), and
    /// [`Session::expand`] reads its fields. `None` at any other stop.
    #[cfg(feature = "exceptions")]
    #[must_use]
    pub fn stopped_exception(&self) -> Option<ObjectRef> {
        self.unhandled_exception
    }

    /// Executes exactly one instruction, ignoring breakpoints.
    ///
    /// # Errors
    /// Returns a [`Trap`] if the instruction faults.
    pub fn step(&mut self, module: &Module, vm: &mut Vm) -> Result<Status, Trap> {
        if let Some(result) = &self.result {
            return Ok(Status::Done(result.clone()));
        }
        self.advance(module, vm)
    }

    /// Runs until a breakpoint is reached or the program finishes. A breakpoint
    /// pauses *before* its instruction runs; to continue past one the session is
    /// sitting on, [`Session::step`] once, then resume again.
    ///
    /// # Errors
    /// Returns a [`Trap`] if an instruction faults.
    pub fn resume(&mut self, module: &Module, vm: &mut Vm) -> Result<Status, Trap> {
        loop {
            if let Some(result) = &self.result {
                return Ok(Status::Done(result.clone()));
            }
            if self.at_breakpoint() {
                return Ok(Status::Paused);
            }
            if let Status::Done(value) = self.advance(module, vm)? {
                return Ok(Status::Done(value));
            }
        }
    }

    /// Runs to completion, ignoring breakpoints, returning the entry's result.
    ///
    /// # Errors
    /// Returns a [`Trap`] if an instruction faults.
    pub fn run(&mut self, module: &Module, vm: &mut Vm) -> Result<Option<Value>, Trap> {
        loop {
            if let Some(result) = &self.result {
                return Ok(result.clone());
            }
            self.advance(module, vm)?;
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
            Some(frame) => {
                self.breakpoints
                    .get(&(frame.method, frame.ip as u32))
                    .copied()
                    == Some(true)
            }
            None => false,
        }
    }

    /// The location execution is paused at -- the top (innermost) frame's method and
    /// instruction index -- or `None` once the program has finished. This is the location a
    /// [`Stop`] reports, exposed on its own so a driver can read "where am I" without a step.
    #[must_use]
    pub fn location(&self) -> Option<CodeLocation> {
        self.frames.last().map(|frame| CodeLocation {
            method: frame.method,
            instruction: frame.ip as u32,
        })
    }

    /// The `this` reference of the innermost frame -- argument slot 0 when `module` records
    /// that method's first argument as `this` (i.e. it is an instance method) -- or `None`
    /// for a static method (or when no debug names are loaded). A debugger surfaces this
    /// distinctly from the other arguments.
    #[must_use]
    pub fn this(&self, module: &Module) -> Option<Value> {
        let frame = self.frames.last()?;
        module
            .arg_name(frame.method, 0)
            .filter(|name| *name == "this")
            .and_then(|_| frame.args.first())
            .cloned()
    }

    /// Resumes until an enabled breakpoint, an unhandled exception
    /// ([`Session::set_pause_on_unhandled_exception`]), or completion -- the device-agnostic
    /// "continue" the wireline and `lamella-dap` both drive. Unlike [`Session::resume`], it
    /// reports *why* it stopped and *where* via a [`Stop`], and (when enabled) pauses on an
    /// unhandled exception rather than trapping. It steps off a breakpoint it is already
    /// sitting on before running, so a repeated `continue_` makes progress.
    ///
    /// # Errors
    /// Returns a [`Trap`] if an instruction faults in a way that is not a catchable,
    /// pausable exception (malformed CIL, stack overflow, an unresolved token, ...).
    pub fn continue_(&mut self, module: &Module, vm: &mut Vm) -> Result<Stop, Trap> {
        if let Some(result) = &self.result {
            return Ok(self.done(result.clone()));
        }
        if self.at_breakpoint() {
            if let Some(stop) = self.tick(module, vm)? {
                return Ok(stop);
            }
        }
        loop {
            if self.at_breakpoint() {
                return Ok(self.stop(StopReason::Breakpoint));
            }
            if let Some(stop) = self.tick(module, vm)? {
                return Ok(stop);
            }
        }
    }

    /// Single-steps one instruction, descending into a call -- "step into". Stops at the
    /// next instruction wherever it lands (the callee's first instruction across a call, or
    /// the next instruction in this frame), or earlier on an enabled breakpoint / a pausable
    /// unhandled exception.
    ///
    /// # Errors
    /// Returns a [`Trap`] as [`Session::continue_`] does.
    pub fn step_into(&mut self, module: &Module, vm: &mut Vm) -> Result<Stop, Trap> {
        if let Some(result) = &self.result {
            return Ok(self.done(result.clone()));
        }
        match self.tick(module, vm)? {
            Some(stop) => Ok(stop),
            None if self.at_breakpoint() => Ok(self.stop(StopReason::Breakpoint)),
            None => Ok(self.stop(StopReason::Step)),
        }
    }

    /// Steps over a call -- "step over" / DAP `next`. A call at the current instruction runs
    /// to completion (back to this frame or a caller) before stopping; otherwise it behaves
    /// like [`Session::step_into`]. An enabled breakpoint hit inside the stepped-over call
    /// takes priority and pauses there.
    ///
    /// # Errors
    /// Returns a [`Trap`] as [`Session::continue_`] does.
    pub fn step_over(&mut self, module: &Module, vm: &mut Vm) -> Result<Stop, Trap> {
        let start = self.depth();
        self.step_until(module, vm, |session| session.depth() <= start)
    }

    /// Steps out of the current method -- "step out" / DAP `stepOut`. Runs until the current
    /// method returns (the call stack is shallower than it is now), then stops at the next
    /// instruction in the caller, or earlier on an enabled breakpoint / a pausable unhandled
    /// exception.
    ///
    /// # Errors
    /// Returns a [`Trap`] as [`Session::continue_`] does.
    pub fn step_out(&mut self, module: &Module, vm: &mut Vm) -> Result<Stop, Trap> {
        let start = self.depth();
        self.step_until(module, vm, |session| session.depth() < start)
    }

    /// Single-steps until `reached` holds (after a step that completes), a breakpoint is hit,
    /// an exception pauses, or the program finishes -- the shared engine of `step_over` /
    /// `step_out`.
    fn step_until(
        &mut self,
        module: &Module,
        vm: &mut Vm,
        reached: impl Fn(&Session) -> bool,
    ) -> Result<Stop, Trap> {
        if let Some(result) = &self.result {
            return Ok(self.done(result.clone()));
        }
        loop {
            if let Some(stop) = self.tick(module, vm)? {
                return Ok(stop);
            }
            if self.at_breakpoint() {
                return Ok(self.stop(StopReason::Breakpoint));
            }
            if reached(self) {
                return Ok(self.stop(StopReason::Step));
            }
        }
    }

    /// Advances exactly one instruction for the stepping/continue loop, mapping its outcome
    /// to an early-stop [`Stop`] (completion, or -- when armed -- a pausable unhandled
    /// exception) or `None` to keep going. A breakpoint is *not* detected here (the callers
    /// check `at_breakpoint` at the right boundary); this only runs the instruction.
    fn tick(&mut self, module: &Module, vm: &mut Vm) -> Result<Option<Stop>, Trap> {
        #[cfg(feature = "exceptions")]
        {
            self.unhandled_exception = None;
        }
        let outcome = self.advance(module, vm);
        match outcome {
            Ok(Status::Done(value)) => {
                #[cfg(feature = "exceptions")]
                let _ = vm.take_unhandled();
                Ok(Some(self.done(value)))
            }
            Ok(Status::Running | Status::Paused) => {
                #[cfg(feature = "exceptions")]
                let _ = vm.take_unhandled();
                Ok(None)
            }
            #[cfg(feature = "exceptions")]
            Err(Trap::UnhandledException) if self.pause_on_exception => {
                self.unhandled_exception = vm.take_unhandled();
                Ok(Some(self.stop(StopReason::Exception)))
            }
            Err(trap) => Err(trap),
        }
    }

    /// Builds a [`Stop`] for stopping (still executing) at the current top-frame location.
    fn stop(&self, reason: StopReason) -> Stop {
        Stop {
            reason,
            location: self.location(),
            returned: None,
        }
    }

    /// Builds the completion [`Stop`] (no location; the call stack is empty).
    fn done(&self, returned: Option<Value>) -> Stop {
        Stop {
            reason: StopReason::Exited,
            location: None,
            returned,
        }
    }

    /// Expands an inspectable `value` into its constituents for a debugger's variable tree:
    /// an object instance's fields (`fieldN` by slot), an array's elements (`[i]`), or a
    /// box's single inner value. A scalar (or any value that is not a heap aggregate) expands
    /// to nothing. `vm` owns the heap the references point into. This is the read side of
    /// variable drill-down the wireline and the DAP `variables` request both use. (Instance
    /// field *names* live in metadata a driver can layer on; the core names them by slot.)
    #[must_use]
    pub fn expand(&self, vm: &Vm, value: &Value) -> Vec<NamedValue> {
        let Value::Object(reference) = value else {
            return Vec::new();
        };
        let reference = *reference;
        if let Some(len) = vm.heap().array_len(reference) {
            return (0..len)
                .filter_map(|index| {
                    vm.heap().array_get(reference, index).map(|value| NamedValue {
                        name: alloc::format!("[{index}]"),
                        value,
                    })
                })
                .collect();
        }
        if let Some(inner) = vm.heap().boxed_value(reference) {
            return alloc::vec![NamedValue {
                name: "value".to_string(),
                value: inner,
            }];
        }
        if let Some(crate::object::Object::Instance { fields, .. }) = vm.heap().get(reference) {
            return fields
                .iter()
                .enumerate()
                .map(|(slot, value)| NamedValue {
                    name: alloc::format!("field{slot}"),
                    value: value.clone(),
                })
                .collect();
        }
        Vec::new()
    }

    /// Whether `value` can be drilled into with [`Session::expand`] (so a driver knows to
    /// offer it as expandable). `vm` owns the heap.
    #[must_use]
    pub fn is_expandable(&self, vm: &Vm, value: &Value) -> bool {
        is_expandable(vm, value)
    }

    /// Executes one instruction of the top frame and applies its effect to the
    /// call stack: a return pops (handing the result back, or finishing); a call
    /// pushes a managed frame or invokes an intrinsic.
    fn advance(&mut self, module: &Module, vm: &mut Vm) -> Result<Status, Trap> {
        #[cfg(feature = "gc")]
        {
            let forced = vm.take_force_collect();
            if vm.collection_suspended() {
                if forced {
                    vm.request_collect();
                }
            } else if forced || vm.heap().should_collect() {
                self.collect_garbage(module, vm);
            }
        }
        let Session { frames, result, .. } = self;
        let current = match frames.len() {
            0 => return Ok(Status::Done(None)),
            len => len - 1,
        };
        let top = &mut frames[current];
        let asm = module.method_asm(top.method);
        let code = method_code(module, top.method)?;
        let instruction = code.get(top.ip).ok_or(Trap::FellThroughEnd)?;
        top.ip += 1;
        let flow = match step(top, current, code, Some(module), vm, instruction) {
            Ok(flow) => flow,
            #[cfg(feature = "exceptions")]
            Err(trap) => match catchable_fault(&trap, vm) {
                Some(exception) => Flow::Throw(exception),
                None => return Err(trap),
            },
            #[cfg(not(feature = "exceptions"))]
            Err(trap) => return Err(trap),
        };
        match flow {
            Flow::Next => Ok(Status::Running),
            Flow::Return(value) => {
                let returned = frames.pop();
                if let Some((rest, params)) = returned.as_ref().and_then(|f| f.multicast.as_ref()) {
                    if let Some(((target, method), remaining)) = rest.split_first() {
                        let call_args = delegate_call_args(target, params);
                        let (method, remaining, params) =
                            (*method, remaining.to_vec(), params.clone());
                        if frames.len() >= MAX_CALL_DEPTH {
                            return Err(Trap::CallStackOverflow);
                        }
                        let mut next = new_frame(module, method, call_args)?;
                        next.multicast = Some((remaining, params));
                        frames.push(next);
                        return Ok(Status::Running);
                    }
                }
                let returned_object = returned.as_ref().and_then(|frame| frame.new_object);
                let returned_value_location = returned.and_then(|frame| frame.new_value);
                let returned_struct = returned_value_location
                    .map(|location| read_byref(frames, vm, location));
                match frames.last_mut() {
                    Some(caller) => {
                        if let Some(object) = returned_object {
                            caller.stack.push(Value::Object(object));
                        } else if let Some(structure) = returned_struct {
                            caller.stack.push(structure);
                        } else if let Some(value) = value {
                            caller.stack.push(value);
                        }
                        Ok(Status::Running)
                    }
                    None => {
                        *result = Some(value.clone());
                        Ok(Status::Done(value))
                    }
                }
            }
            Flow::Jmp(target) => {
                let args = frames.last().ok_or(Trap::StackUnderflow)?.args.clone();
                frames.pop();
                let frame = new_frame(module, target, args)?;
                frames.push(frame);
                Ok(Status::Running)
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
                    let args = if is_compare_exchange(module, method) {
                        args
                    } else {
                        deref_byref_args(frames, vm, args)
                    };
                    match func(vm, module, &args) {
                        Ok(result) => {
                            if let Some(value) = result {
                                frames
                                    .last_mut()
                                    .ok_or(Trap::CallStackOverflow)?
                                    .stack
                                    .push(value);
                            }
                            Ok(Status::Running)
                        }
                        #[cfg(feature = "exceptions")]
                        Err(trap) => match catchable_fault(&trap, vm) {
                            Some(exception) => {
                                vm.note_unhandled(exception);
                                raise(frames, module, vm, exception)
                            }
                            None => Err(trap),
                        },
                        #[cfg(not(feature = "exceptions"))]
                        Err(trap) => Err(trap),
                    }
                }
                None => Err(Trap::NoSuchMethod(method)),
            },
            Flow::NewObj { ctor, object, args } => match module.method(ctor) {
                Some(Method::Managed { .. }) => {
                    if frames.len() >= MAX_CALL_DEPTH {
                        return Err(Trap::CallStackOverflow);
                    }
                    let mut full_args = Vec::with_capacity(args.len() + 1);
                    full_args.push(Value::Object(object));
                    full_args.extend(args);
                    let mut frame = new_frame(module, ctor, full_args)?;
                    frame.new_object = Some(object);
                    frames.push(frame);
                    Ok(Status::Running)
                }
                Some(Method::Intrinsic { func, .. }) => {
                    let func = *func;
                    let mut full_args = Vec::with_capacity(args.len() + 1);
                    full_args.push(Value::Object(object));
                    full_args.extend(args);
                    func(vm, module, &full_args)?;
                    frames
                        .last_mut()
                        .ok_or(Trap::CallStackOverflow)?
                        .stack
                        .push(Value::Object(object));
                    Ok(Status::Running)
                }
                None => Err(Trap::NoSuchMethod(ctor)),
            },
            Flow::NewValueObj {
                ctor,
                location,
                args,
            } => match module.method(ctor) {
                Some(Method::Managed { .. }) => {
                    if frames.len() >= MAX_CALL_DEPTH {
                        return Err(Trap::CallStackOverflow);
                    }
                    let mut full_args = Vec::with_capacity(args.len() + 1);
                    full_args.push(Value::ByRef(location.clone()));
                    full_args.extend(args);
                    let mut frame = new_frame(module, ctor, full_args)?;
                    frame.new_value = Some(location);
                    frames.push(frame);
                    Ok(Status::Running)
                }
                Some(Method::Intrinsic { .. }) => {
                    let value = read_byref(frames, vm, location);
                    frames
                        .last_mut()
                        .ok_or(Trap::CallStackOverflow)?
                        .stack
                        .push(value);
                    Ok(Status::Running)
                }
                None => Err(Trap::NoSuchMethod(ctor)),
            },
            Flow::Throw(exception) => {
                #[cfg(feature = "exceptions")]
                vm.note_unhandled(exception);
                raise(frames, module, vm, exception)
            }
            Flow::Leave(target) => {
                let finallys = {
                    let method = frames.last().ok_or(Trap::StackUnderflow)?.method;
                    let handlers = method_handlers(module, method)?;
                    let frame = frames.last_mut().ok_or(Trap::StackUnderflow)?;
                    frame.stack.clear();
                    let leave_ip = frame.ip.saturating_sub(1);
                    finallys_exited(handlers, leave_ip, target)
                };
                begin_finallys(frames, module, vm, finallys, AfterFinally::Goto(target))
            }
            Flow::EndFinally => {
                let pending = frames
                    .last_mut()
                    .ok_or(Trap::StackUnderflow)?
                    .pending
                    .take();
                match pending {
                    Some(PendingFinally { finallys, then }) => {
                        begin_finallys(frames, module, vm, finallys, then)
                    }
                    None => Err(Trap::Unsupported(Opcode::Endfinally)),
                }
            }
            Flow::EndFilter(accept) => {
                let pending = frames
                    .last_mut()
                    .ok_or(Trap::StackUnderflow)?
                    .pending_filter
                    .take();
                match pending {
                    Some(filter) if accept => {
                        let method = frames.last().ok_or(Trap::StackUnderflow)?.method;
                        let handlers = method_handlers(module, method)?;
                        let finallys =
                            finallys_inside(handlers, filter.fault_ip, filter.filter_try);
                        begin_finallys(
                            frames,
                            module,
                            vm,
                            finallys,
                            AfterFinally::Catch {
                                handler: filter.handler,
                                exception: filter.exception,
                            },
                        )
                    }
                    Some(filter) => raise_from(
                        frames,
                        module,
                        vm,
                        filter.exception,
                        filter.resume,
                        filter.fault_ip,
                    ),
                    None => Err(Trap::Unsupported(Opcode::Endfilter)),
                }
            }
            Flow::LoadField { location, field } => {
                let slot = module
                    .field_slot(asm, field)
                    .ok_or(Trap::UnresolvedField(field))?;
                let value = read_field_at(frames, vm, location, slot);
                frames
                    .get_mut(current)
                    .ok_or(Trap::StackUnderflow)?
                    .stack
                    .push(value);
                Ok(Status::Running)
            }
            Flow::StoreField {
                location,
                field,
                value,
            } => {
                let slot = module
                    .field_slot(asm, field)
                    .ok_or(Trap::UnresolvedField(field))?;
                let type_id = module
                    .field_type(asm, field)
                    .ok_or(Trap::UnresolvedField(field))?;
                let shape = module
                    .type_field_defaults(type_id)
                    .ok_or(Trap::UnresolvedField(field))?
                    .to_vec();
                write_field_at(frames, vm, location, slot, &shape, value)?;
                Ok(Status::Running)
            }
            Flow::InitObj { location, kind } => {
                let value = match module
                    .type_id_of(asm, kind)
                    .and_then(|type_id| module.type_field_defaults(type_id))
                {
                    Some(defaults) => Value::Struct(defaults.to_vec().into_boxed_slice()),
                    None => Value::Int32(0),
                };
                write_location_value(frames, vm, location, value)?;
                Ok(Status::Running)
            }
            Flow::LoadObj { location } => {
                let value = read_byref(frames, vm, location);
                frames
                    .get_mut(current)
                    .ok_or(Trap::StackUnderflow)?
                    .stack
                    .push(value);
                Ok(Status::Running)
            }
            Flow::StoreObj { location, value } => {
                write_location_value(frames, vm, location, value)?;
                Ok(Status::Running)
            }
            Flow::CopyObj { dest, src } => {
                let value = read_byref(frames, vm, src);
                write_location_value(frames, vm, dest, value)?;
                Ok(Status::Running)
            }
            Flow::InvokeMulticast {
                mut invocations,
                params,
            } => {
                if invocations.is_empty() {
                    return Ok(Status::Running);
                }
                let (target, method) = invocations.remove(0);
                let call_args = delegate_call_args(&target, &params);
                if frames.len() >= MAX_CALL_DEPTH {
                    return Err(Trap::CallStackOverflow);
                }
                let mut frame = new_frame(module, method, call_args)?;
                frame.multicast = Some((invocations, params));
                frames.push(frame);
                Ok(Status::Running)
            }
        }
    }
}

/// The array element index a `Location::Element` addresses: its base `index` advanced by the
/// raw `byte_offset` a pinned-array pointer accumulated (`fixed (int* p = arr)` then `p[i]`).
/// The offset is a whole multiple of the element width for the indexing csc emits; with a
/// zero offset (a plain `ldelema` pointer) this is just the base index. Falls back to the
/// base index if the element width is unknown (an empty array -- not dereferenceable anyway).
fn array_element_index(vm: &Vm, array: ObjectRef, index: usize, byte_offset: u32) -> usize {
    if byte_offset == 0 {
        return index;
    }
    match vm.heap().array_element_width(array) {
        Some(width) if width != 0 => index + (byte_offset as usize) / width,
        _ => index,
    }
}

/// The whole value at a managed-pointer `location` -- a frame local/arg, a heap
/// object's field, an array element, or a static field -- if present.
fn read_location_value(frames: &[Frame], vm: &Vm, location: Location) -> Option<Value> {
    match location {
        Location::Local { frame, slot } => {
            frames.get(frame).and_then(|f| f.locals.get(slot)).cloned()
        }
        Location::Arg { frame, slot } => frames.get(frame).and_then(|f| f.args.get(slot)).cloned(),
        Location::Field { object, slot } => vm.heap().instance_field(object, slot),
        Location::Element {
            array,
            index,
            byte_offset,
        } => vm
            .heap()
            .array_get(array, array_element_index(vm, array, index, byte_offset)),
        Location::Static { slot } => vm.static_field(slot),
        Location::Boxed { object } => vm.heap().boxed_value(object),
        Location::Nested { base, slot } => match read_location_value(frames, vm, (*base).clone()) {
            Some(Value::Struct(fields)) => fields.get(slot as usize).cloned(),
            _ => None,
        },
        Location::Stack { .. } => None,
    }
}

/// Writes the whole `value` at a managed-pointer `location`.
fn write_location_value(
    frames: &mut [Frame],
    vm: &mut Vm,
    location: Location,
    value: Value,
) -> Result<(), Trap> {
    match location {
        Location::Local { frame, slot } => {
            let frame = frames
                .get_mut(frame)
                .ok_or(Trap::Unsupported(Opcode::Stobj))?;
            set_slot(&mut frame.locals, slot, value);
            Ok(())
        }
        Location::Arg { frame, slot } => {
            let frame = frames
                .get_mut(frame)
                .ok_or(Trap::Unsupported(Opcode::Stobj))?;
            set_slot(&mut frame.args, slot, value);
            Ok(())
        }
        Location::Field { object, slot } => vm
            .heap_mut()
            .set_instance_field(object, slot, value)
            .then_some(())
            .ok_or(Trap::NullReference),
        Location::Element {
            array,
            index,
            byte_offset,
        } => {
            let element = array_element_index(vm, array, index, byte_offset);
            vm.heap_mut()
                .array_set(array, element, value)
                .then_some(())
                .ok_or(Trap::IndexOutOfRange(element as i32))
        }
        Location::Static { slot } => {
            vm.set_static_field(slot, value);
            Ok(())
        }
        Location::Boxed { object } => vm
            .heap_mut()
            .set_boxed_value(object, value)
            .then_some(())
            .ok_or(Trap::NullReference),
        Location::Nested { base, slot } => {
            let mut fields = match read_location_value(frames, vm, (*base).clone()) {
                Some(Value::Struct(fields)) => fields,
                _ => return Err(Trap::Unsupported(Opcode::Stfld)),
            };
            if let Some(target) = fields.get_mut(slot as usize) {
                *target = value;
            }
            write_location_value(frames, vm, *base, Value::Struct(fields))
        }
        Location::Stack { .. } => Err(Trap::Unsupported(Opcode::Stobj)),
    }
}

/// Reads field `slot` of the value-type instance at `location` (zero if the slot has
/// not been materialized into a struct yet -- an `init_locals` zero).
fn read_field_at(frames: &[Frame], vm: &Vm, location: Location, slot: u32) -> Value {
    match read_location_value(frames, vm, location) {
        Some(Value::Struct(fields)) => fields
            .get(slot as usize)
            .cloned()
            .unwrap_or(Value::Int32(0)),
        _ => Value::Int32(0),
    }
}

/// Dereferences any managed-pointer argument to the value it points at, so an intrinsic
/// (which works on values -- e.g. `Int32.ToString` on `&int`) sees the value, not the
/// pointer.
fn deref_byref_args(frames: &[Frame], vm: &Vm, args: Vec<Value>) -> Vec<Value> {
    args.into_iter()
        .map(|arg| match arg {
            Value::ByRef(location) => read_byref(frames, vm, location),
            other => other,
        })
        .collect()
}

/// Normalizes the value `stind.i1` / `stind.i2` stores through a byref to a VALUE slot (a local /
/// argument / field -- NOT raw localloc memory, which takes the byte-accurate path). C# always
/// pre-narrows with `conv.u1`/`conv.i1` (or the i2 pair) before the store, so the `int32` reaching
/// the slot is ALREADY the final byte/short value -- signed for an sbyte/short, UNSIGNED for a
/// byte/ushort, the `conv` having chosen. The slot is read back sign-AGNOSTICALLY (`ldloc` / `ldfld`
/// / a value-slot `ldind` return it as-is), so the value must be PRESERVED here: re-narrowing cannot
/// tell a byte from an sbyte and would corrupt an unsigned store -- `(byte)200` is `0xC8`, which must
/// read back 200, not the sign-extended -56. A `NativeInt` collapses to the `int32` the slot holds.
fn narrow_stored_int(value: Value, opcode: Opcode) -> Value {
    match (opcode, value) {
        (Opcode::StindI1 | Opcode::StindI2, Value::NativeInt(n)) => Value::Int32(n as i32),
        (_, value) => value,
    }
}

/// The value a managed pointer refers to (for `ldobj` and intrinsic-argument deref);
/// `Null` if the location holds nothing.
fn read_byref(frames: &[Frame], vm: &Vm, location: Location) -> Value {
    read_location_value(frames, vm, location).unwrap_or(Value::Null)
}

/// The underlying integer of an enum value at a managed pointer this frame can reach (a
/// local/argument of this frame, or a heap field/element/static) -- for Enum.ToString.
#[cfg(feature = "bcl")]
fn read_enum_value(frame: &Frame, frame_index: usize, vm: &Vm, location: Location) -> Option<i64> {
    let value = match location {
        Location::Local { frame: f, slot } if f == frame_index => frame.locals.get(slot).cloned(),
        Location::Arg { frame: f, slot } if f == frame_index => frame.args.get(slot).cloned(),
        Location::Field { object, slot } => vm.heap().instance_field(object, slot),
        Location::Element {
            array,
            index,
            byte_offset,
        } => vm
            .heap()
            .array_get(array, array_element_index(vm, array, index, byte_offset)),
        Location::Static { slot } => vm.static_field(slot),
        _ => None,
    }?;
    match value {
        Value::Int32(n) => Some(i64::from(n)),
        Value::Int64(n) => Some(n),
        _ => None,
    }
}

/// Whether `method` is the `Object.ToString` intrinsic, so a `constrained.` Enum.ToString
/// can be rendered as the constant name rather than the boxed value's text.
#[cfg(feature = "bcl")]
fn is_object_to_string(module: &Module, method: MethodId) -> bool {
    if let Some(Method::Intrinsic { func, .. }) = module.method(method) {
        core::ptr::fn_addr_eq(*func, crate::intrinsics::object_to_string as IntrinsicFn)
    } else {
        false
    }
}

/// Whether `method` is the `Interlocked.CompareExchange` intrinsic, so its first argument is
/// passed as the raw managed pointer (to write back through it) rather than dereferenced like
/// an ordinary intrinsic's by-ref argument. Always `false` without the `bcl` intrinsics.
#[cfg(feature = "bcl")]
fn is_compare_exchange(module: &Module, method: MethodId) -> bool {
    if let Some(Method::Intrinsic { func, .. }) = module.method(method) {
        core::ptr::fn_addr_eq(
            *func,
            crate::intrinsics::interlocked_compare_exchange as IntrinsicFn,
        )
    } else {
        false
    }
}

#[cfg(not(feature = "bcl"))]
fn is_compare_exchange(_module: &Module, _method: MethodId) -> bool {
    false
}

/// Stores `value` at `slot`, growing `slots` with `Null` placeholders to reach it.
fn set_slot(slots: &mut Vec<Value>, slot: usize, value: Value) {
    while slots.len() <= slot {
        slots.push(Value::Null);
    }
    slots[slot] = value;
}

/// Builds a delegate invocation's arguments: the bound target (if any) ahead of the
/// shared parameters -- an instance method receives `this`, a static method does not.
fn delegate_call_args(target: &Value, params: &[Value]) -> Vec<Value> {
    let mut call_args = Vec::with_capacity(params.len() + 1);
    if !matches!(target, Value::Null) {
        call_args.push(target.clone());
    }
    call_args.extend_from_slice(params);
    call_args
}

/// Writes `value` into field `slot` of the value-type instance at `location`,
/// materializing it from `shape` (the declaring type's zero fields) if it is not yet a
/// struct. Reads the container, sets the field, writes it back -- so it serves a frame
/// local/arg or a heap/static struct alike.
fn write_field_at(
    frames: &mut [Frame],
    vm: &mut Vm,
    location: Location,
    slot: u32,
    shape: &[Value],
    value: Value,
) -> Result<(), Trap> {
    let mut container = read_location_value(frames, vm, location.clone()).unwrap_or(Value::Null);
    if !matches!(container, Value::Struct(_)) {
        container = Value::Struct(shape.to_vec().into_boxed_slice());
    }
    if let Value::Struct(fields) = &mut container {
        if let Some(target) = fields.get_mut(slot as usize) {
            *target = value;
        }
    }
    write_location_value(frames, vm, location, container)
}

fn new_frame(module: &Module, id: MethodId, args: Vec<Value>) -> Result<Frame, Trap> {
    match module.method(id) {
        Some(Method::Managed { .. }) => Ok(Frame::new(id, args)),
        _ => Err(Trap::NoSuchMethod(id)),
    }
}

/// The CIL of a managed method -- looked up per advance now that a frame no longer
/// borrows it. Errors if `id` names an intrinsic or no method.
fn method_code(module: &Module, id: MethodId) -> Result<&[Instruction], Trap> {
    match module.method(id) {
        Some(Method::Managed { body, .. }) => Ok(&body.code[..]),
        _ => Err(Trap::NoSuchMethod(id)),
    }
}

/// The exception-handling clauses of a managed method.
fn method_handlers(module: &Module, id: MethodId) -> Result<&[EhClause], Trap> {
    match module.method(id) {
        Some(Method::Managed { body, .. }) => Ok(&body.handlers[..]),
        _ => Err(Trap::NoSuchMethod(id)),
    }
}

/// A reserved type id for objects whose type is external to this module: a runtime-fault
/// exception (divide-by-zero, etc.) or a `new` of an external BCL type whose constructor
/// is an intrinsic (e.g. `System.Exception`). No loaded type has this id, so it has no
/// field layout or vtable, and `sig_dispatch` finds nothing for it (callvirt falls back to
/// the bound intrinsic). A runtime-fault exception records its base-chain tag vector
/// separately (`Vm::set_exception_chain`) so `catch` still matches it by type.
const EXTERNAL_TYPE_ID: u32 = u32::MAX;

/// The .NET exception type a catchable runtime fault surfaces as: a default message and the
/// type's base-chain full names leaf-first up to `System.Object`. The handler search needs the
/// WHOLE chain (not just the leaf) so that, e.g., a div-by-zero is caught by
/// `catch (DivideByZeroException)`, `catch (ArithmeticException)`, `catch (Exception)`, or a
/// typeless `catch {}` (== Object) alike. `None` for traps that should still abort (a stack
/// overflow, an unresolved token, malformed CIL, ...). The chains mirror .NET's hierarchy so
/// the tags equal those a managed `throw` of the same type produces.
#[cfg(feature = "exceptions")]
fn fault_exception(trap: &Trap) -> Option<(&'static str, &'static [&'static str])> {
    const ARITHMETIC: &[&str] = &[
        "System.DivideByZeroException",
        "System.ArithmeticException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const OVERFLOW: &[&str] = &[
        "System.OverflowException",
        "System.ArithmeticException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const NULL_REF: &[&str] = &[
        "System.NullReferenceException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const INDEX_OOB: &[&str] = &[
        "System.IndexOutOfRangeException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const ARG_OOR: &[&str] = &[
        "System.ArgumentOutOfRangeException",
        "System.ArgumentException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const INVALID_CAST: &[&str] = &[
        "System.InvalidCastException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    const ARGUMENT: &[&str] = &[
        "System.ArgumentException",
        "System.SystemException",
        "System.Exception",
        "System.Object",
    ];
    let (text, chain): (&str, &[&str]) = match trap {
        Trap::DivideByZero => ("Attempted to divide by zero.", ARITHMETIC),
        Trap::NullReference => (
            "Object reference not set to an instance of an object.",
            NULL_REF,
        ),
        Trap::IndexOutOfRange(_) => ("Index was outside the bounds of the array.", INDEX_OOB),
        Trap::ArgumentOutOfRange(_) => (
            "Specified argument was out of the range of valid values.",
            ARG_OOR,
        ),
        Trap::InvalidCast => ("Unable to cast object to the target type.", INVALID_CAST),
        Trap::InvalidArgument => ("Requested value was not found.", ARGUMENT),
        Trap::Overflow => ("Arithmetic operation resulted in an overflow.", OVERFLOW),
        _ => return None,
    };
    Some((text, chain))
}

/// Converts a catchable runtime fault into a thrown exception object (carrying a default
/// message and its base-chain tag vector so `catch` matches it by type), or returns `None`
/// for traps that should still abort execution (a stack overflow, an unresolved token,
/// malformed CIL, ...).
#[cfg(feature = "exceptions")]
fn catchable_fault(trap: &Trap, vm: &mut Vm) -> Option<ObjectRef> {
    let (text, chain_names) = fault_exception(trap)?;
    let chain: Vec<u32> = chain_names
        .iter()
        .copied()
        .map(crate::exception::exception_tag)
        .collect();
    let exception = vm.heap_mut().alloc_instance(EXTERNAL_TYPE_ID, Vec::new());
    let chars: Vec<u16> = text.bytes().map(u16::from).collect();
    let message = vm.heap_mut().alloc_string(&chars);
    vm.set_exception_message(exception, message);
    vm.set_exception_chain(exception, chain);
    Some(exception)
}

/// Propagates `exception`: searches the call stack from the top for a catch handler
/// whose try region covers the faulting instruction and whose type matches, entering
/// it (eval stack cleared, exception pushed). Frames with no handler are unwound.
/// Returns [`Trap::UnhandledException`] if the stack is exhausted.
fn raise(
    frames: &mut Vec<Frame>,
    module: &Module,
    vm: &Vm,
    exception: ObjectRef,
) -> Result<Status, Trap> {
    let Some(frame) = frames.last() else {
        return Err(Trap::UnhandledException);
    };
    let fault_ip = frame.ip.saturating_sub(1);
    raise_from(frames, module, vm, exception, 0, fault_ip)
}

/// Searches this frame's handler clauses, from index `from`, for one that catches
/// `exception` -- a type-matching `catch`, or a `filter` whose expression evaluates to
/// true. A filter is evaluated inline: the frame runs its expression and `endfilter`
/// resumes the search here (a rejecting filter continuing from the next clause). With no
/// catcher, the finallys and faults covering the fault run and the frame unwinds.
fn raise_from(
    frames: &mut Vec<Frame>,
    module: &Module,
    vm: &Vm,
    exception: ObjectRef,
    from: usize,
    fault_ip: usize,
) -> Result<Status, Trap> {
    let Some(frame) = frames.last() else {
        return Err(Trap::UnhandledException);
    };
    let handlers = method_handlers(module, frame.method)?;
    for (index, clause) in handlers.iter().enumerate().skip(from) {
        if !covers(clause.try_range, fault_ip) {
            continue;
        }
        match &clause.kind {
            EhKind::Catch(type_token) => {
                if catch_matches(
                    module,
                    module.method_asm(frame.method),
                    vm,
                    *type_token,
                    exception,
                ) {
                    let finallys = finallys_inside(handlers, fault_ip, clause.try_range);
                    return begin_finallys(
                        frames,
                        module,
                        vm,
                        finallys,
                        AfterFinally::Catch {
                            handler: clause.handler_range.start as usize,
                            exception,
                        },
                    );
                }
            }
            EhKind::Filter { filter_start } => {
                let pending = PendingFilter {
                    exception,
                    handler: clause.handler_range.start as usize,
                    filter_try: clause.try_range,
                    resume: index + 1,
                    fault_ip,
                };
                let start = *filter_start as usize;
                let frame = frames.last_mut().ok_or(Trap::StackUnderflow)?;
                frame.stack.clear();
                frame.stack.push(Value::Object(exception));
                frame.ip = start;
                frame.pending_filter = Some(pending);
                return Ok(Status::Running);
            }
            EhKind::Finally | EhKind::Fault => {}
        }
    }
    let finallys = finallys_covering(handlers, fault_ip);
    begin_finallys(
        frames,
        module,
        vm,
        finallys,
        AfterFinally::Unwind(exception),
    )
}

/// Runs the next pending `finally` (if any), else performs the chain's continuation.
fn begin_finallys(
    frames: &mut Vec<Frame>,
    module: &Module,
    vm: &Vm,
    mut finallys: Vec<usize>,
    then: AfterFinally,
) -> Result<Status, Trap> {
    match finallys.pop() {
        Some(next) => {
            let frame = frames.last_mut().ok_or(Trap::StackUnderflow)?;
            frame.ip = next;
            frame.pending = Some(PendingFinally { finallys, then });
            Ok(Status::Running)
        }
        None => complete_finally(frames, module, vm, then),
    }
}

/// Performs a finished finally chain's continuation: branch (`leave`), enter the
/// catch, or pop the frame and keep unwinding.
fn complete_finally(
    frames: &mut Vec<Frame>,
    module: &Module,
    vm: &Vm,
    then: AfterFinally,
) -> Result<Status, Trap> {
    match then {
        AfterFinally::Goto(target) => {
            frames.last_mut().ok_or(Trap::StackUnderflow)?.ip = target;
            Ok(Status::Running)
        }
        AfterFinally::Catch { handler, exception } => {
            let frame = frames.last_mut().ok_or(Trap::StackUnderflow)?;
            frame.stack.clear();
            frame.stack.push(Value::Object(exception));
            frame.current_exception = Some(exception);
            frame.ip = handler;
            Ok(Status::Running)
        }
        AfterFinally::Unwind(exception) => {
            frames.pop();
            raise(frames, module, vm, exception)
        }
    }
}

/// Whether `ip` lies in the half-open `[start, end)` instruction range.
fn covers(range: InstructionRange, ip: usize) -> bool {
    (range.start as usize) <= ip && ip < (range.end as usize)
}

/// The finally handlers a `leave` from `from_ip` to `target` exits: those whose try
/// covers `from_ip` but not `target`. Ordered so `pop` yields innermost first.
fn finallys_exited(handlers: &[EhClause], from_ip: usize, target: usize) -> Vec<usize> {
    finally_handlers(handlers, false, |clause| {
        covers(clause.try_range, from_ip) && !covers(clause.try_range, target)
    })
}

/// The finally handlers in this frame covering `fault_ip` (run as the frame unwinds
/// when it has no matching catch).
fn finallys_covering(handlers: &[EhClause], fault_ip: usize) -> Vec<usize> {
    finally_handlers(handlers, true, |clause| covers(clause.try_range, fault_ip))
}

/// The finally handlers nested between `fault_ip` and a catch -- covering the fault
/// and lying within the catch's try region -- run before entering the catch.
fn finallys_inside(
    handlers: &[EhClause],
    fault_ip: usize,
    catch_try: InstructionRange,
) -> Vec<usize> {
    finally_handlers(handlers, true, |clause| {
        covers(clause.try_range, fault_ip)
            && clause.try_range.start >= catch_try.start
            && clause.try_range.end <= catch_try.end
    })
}

/// The handler starts of the finally clauses -- and, when `include_fault` (an exception
/// unwind, not a `leave`), the fault clauses -- kept by `keep`, ordered outermost-first so
/// that `pop` runs them innermost-first. A fault handler runs like a finally during unwind
/// and ends with `endfault` (the same opcode as `endfinally`).
fn finally_handlers(
    handlers: &[EhClause],
    include_fault: bool,
    keep: impl Fn(&EhClause) -> bool,
) -> Vec<usize> {
    let mut clauses: Vec<&EhClause> = handlers
        .iter()
        .filter(|clause| {
            let runs = matches!(clause.kind, EhKind::Finally)
                || (include_fault && matches!(clause.kind, EhKind::Fault));
            runs && keep(clause)
        })
        .collect();
    clauses.sort_by_key(|clause| clause.try_range.start);
    clauses
        .into_iter()
        .map(|clause| clause.handler_range.start as usize)
        .collect()
}

/// Whether a `catch` of `type_token` catches `exception` -- a TYPE test: the catch matches
/// only when the thrown exception's runtime type IS-A the catch's declared type. It compares
/// the catch type's exception TAG against the thrown exception's base-chain tag VECTOR
/// (`exception::tag_is_subtype`, the membership the AOT tag contract relies on): a managed
/// exception's vector is its live base chain ([`Module::exception_base_chain`]); a runtime-fault
/// exception's was recorded when the fault was raised ([`Vm::set_exception_chain`]).
///
/// Two cases stay catch-all (matching any in-flight exception), exactly as before: a typeless
/// `catch {}` whose clause carries no catch-type tag, and an exception whose runtime type cannot
/// be identified (no base-chain vector -- e.g. a legacy intrinsic-seam `new Exception()` with no
/// live type), which there is no way to discriminate by type.
fn catch_matches(
    module: &Module,
    asm: u8,
    vm: &Vm,
    type_token: Token,
    exception: ObjectRef,
) -> bool {
    #[cfg(feature = "exceptions")]
    {
        let Some(catch_tag) = module.catch_type_tag(asm, type_token) else {
            return true;
        };
        if crate::exception::is_universal_catch(catch_tag) {
            return true;
        }
        let managed_chain;
        let thrown_chain: &[u32] = match vm.heap().type_of(exception) {
            Some(EXTERNAL_TYPE_ID) | None => vm.exception_chain(exception).unwrap_or(&[]),
            Some(type_id) => {
                managed_chain = module.exception_base_chain(type_id);
                &managed_chain
            }
        };
        if thrown_chain.is_empty() {
            return true;
        }
        crate::exception::tag_is_subtype(catch_tag, thrown_chain)
    }
    #[cfg(not(feature = "exceptions"))]
    {
        let _ = (module, asm, vm, type_token, exception);
        true
    }
}

fn step(
    frame: &mut Frame,
    frame_index: usize,
    code: &[Instruction],
    module: Option<&Module>,
    vm: &mut Vm,
    instruction: &Instruction,
) -> Result<Flow, Trap> {
    let opcode = instruction.opcode;
    let asm = module.map_or(0, |module| module.method_asm(frame.method));
    match opcode {
        Opcode::Nop => {}
        Opcode::Pop => {
            frame.pop()?;
        }
        Opcode::Dup => {
            let top = frame.stack.last().ok_or(Trap::StackUnderflow)?.clone();
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
        #[cfg(feature = "float")]
        Opcode::LdcR4 => frame
            .stack
            .push(Value::Single(float32_operand(instruction)?)),
        #[cfg(feature = "float")]
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
        Opcode::LdlocaS | Opcode::Ldloca => frame.stack.push(Value::ByRef(Location::Local {
            frame: frame_index,
            slot: var_operand(instruction)? as usize,
        })),
        Opcode::LdargaS | Opcode::Ldarga => frame.stack.push(Value::ByRef(Location::Arg {
            frame: frame_index,
            slot: var_operand(instruction)? as usize,
        })),

        Opcode::Add
        | Opcode::Sub
        | Opcode::Mul
        | Opcode::Div
        | Opcode::Rem
        | Opcode::AddOvf
        | Opcode::AddOvfUn
        | Opcode::SubOvf
        | Opcode::SubOvfUn
        | Opcode::MulOvf
        | Opcode::MulOvfUn => {
            let (a, b) = frame.pop2()?;
            let result = match stack_pointer_arithmetic(opcode, &a, &b)? {
                Some(pointer) => pointer,
                None => binary_numeric(opcode, a, b)?,
            };
            frame.stack.push(result);
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
        #[cfg(feature = "float")]
        Opcode::Ckfinite => {
            let value = frame.pop()?;
            match value {
                Value::Float(x) if x.is_finite() => frame.stack.push(value),
                Value::Single(x) if x.is_finite() => frame.stack.push(value),
                Value::Float(_) | Value::Single(_) => return Err(Trap::Overflow),
                _ => return Err(Trap::TypeMismatch(Opcode::Ckfinite)),
            }
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
            let converted = match value {
                Value::ByRef(_)
                    if matches!(
                        opcode,
                        Opcode::ConvU | Opcode::ConvI | Opcode::ConvU8 | Opcode::ConvI8
                    ) =>
                {
                    value
                }
                other => convert(opcode, other)?,
            };
            frame.stack.push(converted);
        }
        Opcode::ConvOvfI1
        | Opcode::ConvOvfI1Un
        | Opcode::ConvOvfU1
        | Opcode::ConvOvfU1Un
        | Opcode::ConvOvfI2
        | Opcode::ConvOvfI2Un
        | Opcode::ConvOvfU2
        | Opcode::ConvOvfU2Un
        | Opcode::ConvOvfI4
        | Opcode::ConvOvfI4Un
        | Opcode::ConvOvfU4
        | Opcode::ConvOvfU4Un
        | Opcode::ConvOvfI8
        | Opcode::ConvOvfI8Un
        | Opcode::ConvOvfU8
        | Opcode::ConvOvfU8Un
        | Opcode::ConvOvfI
        | Opcode::ConvOvfIUn
        | Opcode::ConvOvfU
        | Opcode::ConvOvfUUn => {
            let value = frame.pop()?;
            frame.stack.push(convert_checked(opcode, value)?);
        }

        Opcode::Ceq | Opcode::Cgt | Opcode::CgtUn | Opcode::Clt | Opcode::CltUn => {
            let (a, b) = frame.pop2()?;
            let result = compare(opcode, a, b)?;
            frame.stack.push(Value::Int32(i32::from(result)));
        }

        Opcode::Br | Opcode::BrS => frame.ip = branch_target(instruction, code.len())?,
        Opcode::Brtrue | Opcode::BrtrueS => {
            let value = frame.pop()?;
            if value.is_truthy() {
                frame.ip = branch_target(instruction, code.len())?;
            }
        }
        Opcode::Brfalse | Opcode::BrfalseS => {
            let value = frame.pop()?;
            if !value.is_truthy() {
                frame.ip = branch_target(instruction, code.len())?;
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
                frame.ip = branch_target(instruction, code.len())?;
            }
        }
        Opcode::Switch => {
            let index = unsigned_index(frame.pop()?).ok_or(Trap::TypeMismatch(Opcode::Switch))?;
            let Operand::Switch(targets) = &instruction.operand else {
                return Err(Trap::MalformedInstruction(Opcode::Switch));
            };
            if let Some(&target) = targets.get(index) {
                if target as usize >= code.len() {
                    return Err(Trap::BranchOutOfRange(target));
                }
                frame.ip = target as usize;
            }
        }

        Opcode::Ldstr => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldstr))?;
            let token = token_operand(instruction)?;
            let chars = module
                .resolve_string(asm, token)
                .ok_or(Trap::UnresolvedString(token))?;
            let reference = vm.heap_mut().alloc_string(chars);
            frame.stack.push(Value::Object(reference));
        }

        Opcode::Call => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Call))?;
            let token = token_operand(instruction)?;
            let method = module
                .resolve(asm, token)
                .ok_or(Trap::UnresolvedCall(token))?;
            let arg_count = module
                .method(method)
                .ok_or(Trap::NoSuchMethod(method))?
                .arg_count();
            let args = frame.take_args(arg_count)?;
            return Ok(Flow::Call { method, args });
        }

        Opcode::Callvirt => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Callvirt))?;
            let token = token_operand(instruction)?;
            if let Some(param_count) = module.delegate_invoke(asm, token) {
                let args = frame.take_args(param_count + 1)?;
                let delegate = object_ref(
                    args.first().ok_or(Trap::StackUnderflow)?.clone(),
                    Opcode::Callvirt,
                )?;
                let invocations = vm
                    .heap()
                    .delegate_invocations(delegate)
                    .ok_or(Trap::TypeMismatch(Opcode::Callvirt))?
                    .to_vec();
                let params = args.get(1..).unwrap_or_default().to_vec();
                return match invocations.split_first() {
                    Some(((target, method), [])) => Ok(Flow::Call {
                        method: *method,
                        args: delegate_call_args(target, &params),
                    }),
                    Some(_) => Ok(Flow::InvokeMulticast {
                        invocations,
                        params,
                    }),
                    None => Err(Trap::TypeMismatch(Opcode::Callvirt)),
                };
            }
            let static_method = module.resolve(asm, token);
            let target_info = module.call_target(asm, token);
            let arg_count = match target_info {
                Some((_, count)) => count,
                None => {
                    let method = static_method.ok_or(Trap::UnresolvedCall(token))?;
                    module
                        .method(method)
                        .ok_or(Trap::NoSuchMethod(method))?
                        .arg_count()
                }
            };
            let mut args = frame.take_args(arg_count)?;
            let sig_key = target_info.map(|(key, _)| key);
            let constraint = frame.pending_constraint.take();
            let runtime_type = match constraint {
                Some(constraint) => module.type_id_of(asm, constraint),
                None => match args.first().ok_or(Trap::StackUnderflow)? {
                    Value::Object(this) => receiver_type_id(module, vm, *this),
                    Value::Null => return Err(Trap::NullReference),
                    _ => None,
                },
            };
            let explicit_override =
                runtime_type.and_then(|type_id| module.explicit_override(asm, type_id, token));
            let method =
                resolve_callvirt(module, static_method, sig_key, runtime_type, explicit_override)
                    .ok_or(Trap::UnresolvedCall(token))?;
            #[cfg(feature = "bcl")]
            if let Some(constraint) = constraint {
                if is_object_to_string(module, method) {
                    let handle = asm_key(asm, constraint.0);
                    let enum_value = match args.first() {
                        Some(Value::ByRef(location)) => {
                            read_enum_value(frame, frame_index, vm, location.clone())
                        }
                        _ => None,
                    };
                    if let Some(value) = enum_value {
                        if let Some(name) = module.enum_name_or_flags(handle, value, false) {
                            let chars: Vec<u16> = name.encode_utf16().collect();
                            let string = vm.heap_mut().alloc_string(&chars);
                            frame.stack.push(Value::Object(string));
                            return Ok(Flow::Next);
                        }
                        if module.is_enum_by_handle(handle) {
                            let chars: Vec<u16> = value.to_string().encode_utf16().collect();
                            let string = vm.heap_mut().alloc_string(&chars);
                            frame.stack.push(Value::Object(string));
                            return Ok(Flow::Next);
                        }
                    }
                    if let Some(name) = module.type_name_by_handle(handle) {
                        let chars: Vec<u16> = name.encode_utf16().collect();
                        let string = vm.heap_mut().alloc_string(&chars);
                        frame.stack.push(Value::Object(string));
                        return Ok(Flow::Next);
                    }
                }
            }
            if module.method_declares_value_type(method) {
                if let Some(&Value::Object(reference)) = args.first() {
                    if vm.heap().boxed_type_token(reference).is_some() {
                        args[0] = Value::ByRef(Location::Boxed { object: reference });
                    }
                }
            }
            return Ok(Flow::Call { method, args });
        }

        Opcode::Calli => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Calli))?;
            let pointer = function_pointer(frame.pop()?)?;
            let arg_count = module
                .method(pointer)
                .ok_or(Trap::NoSuchMethod(pointer))?
                .arg_count();
            let args = frame.take_args(arg_count)?;
            return Ok(Flow::Call {
                method: pointer,
                args,
            });
        }
        Opcode::Jmp => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Jmp))?;
            let token = token_operand(instruction)?;
            let target = module
                .resolve(asm, token)
                .ok_or(Trap::UnresolvedCall(token))?;
            return Ok(Flow::Jmp(target));
        }
        Opcode::Ldftn => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldftn))?;
            let token = token_operand(instruction)?;
            let method = module
                .resolve(asm, token)
                .ok_or(Trap::UnresolvedCall(token))?;
            frame.stack.push(Value::NativeInt(i64::from(method)));
        }
        Opcode::Ldvirtftn => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldvirtftn))?;
            let token = token_operand(instruction)?;
            let this = object_ref(frame.pop()?, Opcode::Ldvirtftn)?;
            let runtime_type = receiver_type_id(module, vm, this);
            let sig_key = module.call_target(asm, token).map(|(key, _)| key);
            let explicit_override =
                runtime_type.and_then(|type_id| module.explicit_override(asm, type_id, token));
            let method = resolve_callvirt(
                module,
                module.resolve(asm, token),
                sig_key,
                runtime_type,
                explicit_override,
            )
            .ok_or(Trap::UnresolvedCall(token))?;
            frame.stack.push(Value::NativeInt(i64::from(method)));
        }

        Opcode::Newobj => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Newobj))?;
            let token = token_operand(instruction)?;
            if module.is_delegate_ctor(asm, token) {
                let method = function_pointer(frame.pop()?)?;
                let target = frame.pop()?;
                let delegate = vm.heap_mut().alloc_delegate(target, method);
                frame.stack.push(Value::Object(delegate));
                return Ok(Flow::Next);
            }
            if let Some(rank) = module.md_array_ctor_rank(asm, token) {
                let lengths = frame.take_args(rank)?;
                let dims: Vec<i32> = lengths
                    .iter()
                    .map(|value| match value {
                        Value::Int32(n) => *n,
                        Value::Int64(n) | Value::NativeInt(n) => *n as i32,
                        _ => 0,
                    })
                    .collect();
                let array = vm.heap_mut().alloc_md_array(dims);
                frame.stack.push(Value::Object(array));
                return Ok(Flow::Next);
            }
            if let Some(params) = module.string_builder_ctor_params(asm, token) {
                const DEFAULT_CAPACITY: usize = 16;
                let args = frame.take_args(params)?;
                let (initial, capacity) = match args.first() {
                    Some(&Value::Object(reference)) => {
                        let units = vm
                            .heap()
                            .as_string(reference)
                            .map(|units| units.into_owned())
                            .unwrap_or_default();
                        (units, DEFAULT_CAPACITY)
                    }
                    Some(&Value::Int32(requested)) => {
                        let capacity = if requested <= 0 {
                            DEFAULT_CAPACITY
                        } else {
                            requested as usize
                        };
                        (Vec::new(), capacity)
                    }
                    _ => (Vec::new(), DEFAULT_CAPACITY),
                };
                let builder = vm.heap_mut().alloc_string_builder(initial, capacity);
                frame.stack.push(Value::Object(builder));
                return Ok(Flow::Next);
            }
            if let Some(params) = module.list_ctor_params(asm, token) {
                frame.take_args(params)?;
                let list = vm.heap_mut().alloc_array(Vec::new());
                frame.stack.push(Value::Object(list));
                return Ok(Flow::Next);
            }
            let ctor = module
                .resolve(asm, token)
                .ok_or(Trap::UnresolvedCall(token))?;
            let is_intrinsic = matches!(module.method(ctor), Some(Method::Intrinsic { .. }));
            let (type_id, defaults) = if is_intrinsic {
                (EXTERNAL_TYPE_ID, Vec::new())
            } else {
                let type_id = module.method_type(ctor).ok_or(Trap::NoSuchMethod(ctor))?;
                let defaults = module
                    .type_field_defaults(type_id)
                    .ok_or(Trap::NoSuchMethod(ctor))?
                    .to_vec();
                (type_id, defaults)
            };
            let param_count = module
                .method(ctor)
                .ok_or(Trap::NoSuchMethod(ctor))?
                .arg_count()
                .saturating_sub(1);
            let args = frame.take_args(param_count)?;
            if module.is_value_type_ctor(asm, token) {
                let zero = Value::Struct(defaults.into_boxed_slice());
                let temporary = vm.heap_mut().alloc_boxed(asm_key(asm, token.0), zero);
                return Ok(Flow::NewValueObj {
                    ctor,
                    location: Location::Boxed { object: temporary },
                    args,
                });
            }
            let object = vm.heap_mut().alloc_instance(type_id, defaults);
            #[cfg(feature = "finalizers")]
            if module.finalizer_of(type_id).is_some() {
                vm.heap_mut().register_finalizer(object);
            }
            return Ok(Flow::NewObj { ctor, object, args });
        }

        Opcode::Ldfld => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldfld))?;
            let token = token_operand(instruction)?;
            let slot = module
                .field_slot(asm, token)
                .ok_or(Trap::UnresolvedField(token))?;
            match frame.pop()? {
                Value::Object(object) => {
                    let value = vm
                        .heap()
                        .instance_field(object, slot)
                        .ok_or(Trap::UnresolvedField(token))?;
                    frame.stack.push(value);
                }
                Value::ByRef(location) => {
                    return Ok(Flow::LoadField {
                        location,
                        field: token,
                    });
                }
                Value::Struct(fields) => {
                    let value = fields
                        .get(slot as usize)
                        .cloned()
                        .ok_or(Trap::UnresolvedField(token))?;
                    frame.stack.push(value);
                }
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Ldfld)),
            }
        }
        Opcode::Stfld => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Stfld))?;
            let token = token_operand(instruction)?;
            let slot = module
                .field_slot(asm, token)
                .ok_or(Trap::UnresolvedField(token))?;
            let value = frame.pop()?;
            match frame.pop()? {
                Value::Object(object) => {
                    if !vm.heap_mut().set_instance_field(object, slot, value) {
                        return Err(Trap::UnresolvedField(token));
                    }
                }
                Value::ByRef(location) => {
                    return Ok(Flow::StoreField {
                        location,
                        field: token,
                        value,
                    });
                }
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Stfld)),
            }
        }
        Opcode::Ldflda => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldflda))?;
            let token = token_operand(instruction)?;
            let slot = module
                .field_slot(asm, token)
                .ok_or(Trap::UnresolvedField(token))?;
            match frame.pop()? {
                Value::Object(object) => {
                    frame
                        .stack
                        .push(Value::ByRef(Location::Field { object, slot }));
                }
                Value::Null => return Err(Trap::NullReference),
                Value::ByRef(base) => {
                    frame.stack.push(Value::ByRef(Location::Nested {
                        base: alloc::boxed::Box::new(base),
                        slot,
                    }));
                }
                _ => return Err(Trap::Unsupported(Opcode::Ldflda)),
            }
        }
        Opcode::Ldsflda => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldsflda))?;
            let token = token_operand(instruction)?;
            let slot = module
                .static_field_slot(asm, token)
                .ok_or(Trap::UnresolvedField(token))?;
            vm.init_statics(module.static_field_defaults());
            frame.stack.push(Value::ByRef(Location::Static { slot }));
        }
        Opcode::Ldelema => {
            let index = array_index(frame.pop()?, Opcode::Ldelema)?;
            let array = object_ref(frame.pop()?, Opcode::Ldelema)?;
            let len = vm
                .heap()
                .array_len(array)
                .ok_or(Trap::TypeMismatch(Opcode::Ldelema))?;
            let index = bounded_index(index, len)?;
            frame.stack.push(Value::ByRef(Location::Element {
                array,
                index,
                byte_offset: 0,
            }));
        }
        Opcode::Initobj => {
            let token = token_operand(instruction)?;
            match frame.pop()? {
                Value::ByRef(location) => {
                    return Ok(Flow::InitObj {
                        location,
                        kind: token,
                    });
                }
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Initobj)),
            }
        }
        Opcode::Ldobj => match frame.pop()? {
            Value::ByRef(location) => return Ok(Flow::LoadObj { location }),
            other @ (Value::Struct(_) | Value::Object(_)) => frame.stack.push(other),
            Value::Null => return Err(Trap::NullReference),
            _ => return Err(Trap::TypeMismatch(Opcode::Ldobj)),
        },
        Opcode::Stobj => {
            let value = frame.pop()?;
            match frame.pop()? {
                Value::ByRef(location) => return Ok(Flow::StoreObj { location, value }),
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Stobj)),
            }
        }
        Opcode::Cpobj => {
            let src = match frame.pop()? {
                Value::ByRef(location) => location,
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Cpobj)),
            };
            match frame.pop()? {
                Value::ByRef(dest) => return Ok(Flow::CopyObj { dest, src }),
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Cpobj)),
            }
        }
        Opcode::Constrained => {
            frame.pending_constraint = Some(token_operand(instruction)?);
        }
        Opcode::Readonly => {}
        Opcode::Volatile | Opcode::Unaligned | Opcode::Tail => {}
        Opcode::LdindI1
        | Opcode::LdindU1
        | Opcode::LdindI2
        | Opcode::LdindU2
        | Opcode::LdindI4
        | Opcode::LdindU4
        | Opcode::LdindI8
        | Opcode::LdindI
        | Opcode::LdindR4
        | Opcode::LdindR8
        | Opcode::LdindRef => match frame.pop()? {
            Value::ByRef(Location::Stack {
                frame: owner,
                buffer,
                offset,
            }) => {
                let width = indirect_width(opcode).ok_or(Trap::TypeMismatch(opcode))?;
                if owner != frame_index {
                    return Err(Trap::Unsupported(opcode));
                }
                let raw = frame
                    .read_stack(buffer, offset, width)
                    .ok_or(Trap::NullReference)?;
                frame.stack.push(stack_loaded_value(opcode, raw)?);
            }
            Value::ByRef(location) => return Ok(Flow::LoadObj { location }),
            Value::Null => return Err(Trap::NullReference),
            _ => return Err(Trap::TypeMismatch(Opcode::LdindI4)),
        },
        Opcode::StindI1 | Opcode::StindI2 => {
            let value = frame.pop()?;
            match frame.pop()? {
                Value::ByRef(location @ Location::Stack { .. }) => {
                    store_stack_indirect(frame, frame_index, opcode, location, value)?;
                }
                Value::ByRef(location) => {
                    let value = narrow_stored_int(value, opcode);
                    return Ok(Flow::StoreObj { location, value });
                }
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::StindI4)),
            }
        }
        Opcode::StindI4
        | Opcode::StindI8
        | Opcode::StindI
        | Opcode::StindR4
        | Opcode::StindR8
        | Opcode::StindRef => {
            let value = frame.pop()?;
            match frame.pop()? {
                Value::ByRef(location @ Location::Stack { .. }) => {
                    store_stack_indirect(frame, frame_index, opcode, location, value)?;
                }
                Value::ByRef(location) => return Ok(Flow::StoreObj { location, value }),
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::StindI4)),
            }
        }
        Opcode::Ldtoken => {
            let token = token_operand(instruction)?;
            frame
                .stack
                .push(Value::NativeInt(asm_key(asm, token.0) as i64));
        }

        Opcode::Ldsfld => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Ldsfld))?;
            let token = token_operand(instruction)?;
            let slot = module
                .static_field_slot(asm, token)
                .ok_or(Trap::UnresolvedField(token))?;
            vm.init_statics(module.static_field_defaults());
            let value = vm.static_field(slot).ok_or(Trap::UnresolvedField(token))?;
            frame.stack.push(value);
        }
        Opcode::Stsfld => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Stsfld))?;
            let token = token_operand(instruction)?;
            let slot = module
                .static_field_slot(asm, token)
                .ok_or(Trap::UnresolvedField(token))?;
            let value = frame.pop()?;
            vm.init_statics(module.static_field_defaults());
            vm.set_static_field(slot, value);
        }

        Opcode::Castclass => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Castclass))?;
            let token = token_operand(instruction)?;
            let value = frame.pop()?;
            if !cast_matches(module, asm, vm, &value, token) {
                return Err(Trap::InvalidCast);
            }
            frame.stack.push(value);
        }

        Opcode::Isinst => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Isinst))?;
            let token = token_operand(instruction)?;
            let value = frame.pop()?;
            let matched =
                !matches!(value, Value::Null) && cast_matches(module, asm, vm, &value, token);
            frame.stack.push(if matched { value } else { Value::Null });
        }

        Opcode::Box => {
            let token = token_operand(instruction)?;
            let value = frame.pop()?;
            let reference = vm.heap_mut().alloc_boxed(asm_key(asm, token.0), value);
            frame.stack.push(Value::Object(reference));
        }
        Opcode::Unbox => {
            let reference = object_ref(frame.pop()?, Opcode::Unbox)?;
            frame
                .stack
                .push(Value::ByRef(Location::Boxed { object: reference }));
        }
        Opcode::UnboxAny => {
            let reference = object_ref(frame.pop()?, Opcode::UnboxAny)?;
            let value = vm.heap().boxed_value(reference).ok_or(Trap::InvalidCast)?;
            frame.stack.push(value);
        }

        Opcode::Newarr => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Newarr))?;
            let token = token_operand(instruction)?;
            let default = match module.array_default(asm, token) {
                Some(default) => default,
                None => module
                    .type_id_of(asm, token)
                    .and_then(|type_id| module.type_field_defaults(type_id))
                    .map_or(Value::Null, |fields| {
                        Value::Struct(fields.to_vec().into_boxed_slice())
                    }),
            };
            let element_size = module.array_element_size(asm, token);
            let length = array_length(frame.pop()?)?;
            let object = vm
                .heap_mut()
                .alloc_array_sized(alloc::vec![default; length], element_size);
            frame.stack.push(Value::Object(object));
        }

        Opcode::Ldlen => {
            let array = object_ref(frame.pop()?, Opcode::Ldlen)?;
            let length = vm.heap().array_len(array).ok_or(Trap::NullReference)?;
            frame.stack.push(Value::NativeInt(length as i64));
        }

        Opcode::LdelemI1
        | Opcode::LdelemU1
        | Opcode::LdelemI2
        | Opcode::LdelemU2
        | Opcode::LdelemI4
        | Opcode::LdelemU4
        | Opcode::LdelemI8
        | Opcode::LdelemI
        | Opcode::LdelemR4
        | Opcode::LdelemR8
        | Opcode::LdelemRef
        | Opcode::Ldelem => {
            let index = array_index(frame.pop()?, instruction.opcode)?;
            let array = object_ref(frame.pop()?, instruction.opcode)?;
            let len = vm.heap().array_len(array).ok_or(Trap::NullReference)?;
            let index = bounded_index(index, len)?;
            let value = vm
                .heap()
                .array_get(array, index)
                .ok_or(Trap::IndexOutOfRange(index as i32))?;
            let value = match (instruction.opcode, value) {
                (Opcode::LdelemU1, Value::Int32(raw)) => Value::Int32(raw & 0xFF),
                (Opcode::LdelemI1, Value::Int32(raw)) => Value::Int32(i32::from(raw as u8 as i8)),
                (Opcode::LdelemU2, Value::Int32(raw)) => Value::Int32(raw & 0xFFFF),
                (Opcode::LdelemI2, Value::Int32(raw)) => Value::Int32(i32::from(raw as u16 as i16)),
                (_, value) => value,
            };
            frame.stack.push(value);
        }

        Opcode::StelemI1
        | Opcode::StelemI2
        | Opcode::StelemI4
        | Opcode::StelemI8
        | Opcode::StelemI
        | Opcode::StelemR4
        | Opcode::StelemR8
        | Opcode::StelemRef
        | Opcode::Stelem => {
            let value = frame.pop()?;
            let index = array_index(frame.pop()?, instruction.opcode)?;
            let array = object_ref(frame.pop()?, instruction.opcode)?;
            let len = vm.heap().array_len(array).ok_or(Trap::NullReference)?;
            let index = bounded_index(index, len)?;
            if !vm.heap_mut().array_set(array, index, value) {
                return Err(Trap::IndexOutOfRange(index as i32));
            }
        }

        #[cfg(feature = "exceptions")]
        Opcode::Throw => {
            let exception = object_ref(frame.pop()?, Opcode::Throw)?;
            return Ok(Flow::Throw(exception));
        }
        #[cfg(feature = "exceptions")]
        Opcode::Rethrow => {
            let exception = frame
                .current_exception
                .ok_or(Trap::Unsupported(Opcode::Rethrow))?;
            return Ok(Flow::Throw(exception));
        }
        #[cfg(feature = "exceptions")]
        Opcode::Leave | Opcode::LeaveS => {
            return Ok(Flow::Leave(branch_target(instruction, code.len())?));
        }
        #[cfg(feature = "exceptions")]
        Opcode::Endfinally => return Ok(Flow::EndFinally),
        #[cfg(feature = "exceptions")]
        Opcode::Endfilter => {
            let accept = matches!(frame.pop()?, Value::Int32(n) if n != 0);
            return Ok(Flow::EndFilter(accept));
        }

        Opcode::Sizeof => {
            let module = module.ok_or(Trap::Unsupported(Opcode::Sizeof))?;
            let token = token_operand(instruction)?;
            let size = module
                .type_size(asm, token)
                .ok_or(Trap::Unsupported(Opcode::Sizeof))?;
            frame.stack.push(Value::Int32(size as i32));
        }

        Opcode::Localloc => {
            let size = match frame.pop()? {
                Value::Int32(n) => n as u32 as usize,
                Value::Int64(n) | Value::NativeInt(n) => n as usize,
                _ => return Err(Trap::TypeMismatch(Opcode::Localloc)),
            };
            let pointer = frame.localloc(frame_index, size);
            frame.stack.push(pointer);
        }

        Opcode::Ret => return Ok(Flow::Return(frame.stack.pop())),

        #[cfg(feature = "typed-references")]
        Opcode::Mkrefany => {
            let token = token_operand(instruction)?;
            match frame.pop()? {
                Value::ByRef(location) => frame.stack.push(Value::TypedRef {
                    location,
                    type_token: asm_key(asm, token.0),
                }),
                Value::Null => return Err(Trap::NullReference),
                _ => return Err(Trap::TypeMismatch(Opcode::Mkrefany)),
            }
        }
        #[cfg(feature = "typed-references")]
        Opcode::Refanyval => {
            let token = token_operand(instruction)?;
            match frame.pop()? {
                Value::TypedRef {
                    location,
                    type_token,
                } => {
                    if type_token != asm_key(asm, token.0) {
                        return Err(Trap::InvalidCast);
                    }
                    frame.stack.push(Value::ByRef(location));
                }
                _ => return Err(Trap::TypeMismatch(Opcode::Refanyval)),
            }
        }
        #[cfg(feature = "typed-references")]
        Opcode::Refanytype => match frame.pop()? {
            Value::TypedRef { type_token, .. } => {
                frame.stack.push(Value::NativeInt(type_token as i64));
            }
            _ => return Err(Trap::TypeMismatch(Opcode::Refanytype)),
        },
        #[cfg(feature = "varargs")]
        Opcode::Arglist => frame.stack.push(Value::NativeInt(0)),

        #[cfg(not(feature = "typed-references"))]
        op @ (Opcode::Mkrefany | Opcode::Refanyval | Opcode::Refanytype) => {
            return Err(Trap::Unsupported(op));
        }
        #[cfg(not(feature = "varargs"))]
        Opcode::Arglist => return Err(Trap::Unsupported(Opcode::Arglist)),

        other => return Err(Trap::Unsupported(other)),
    }
    Ok(Flow::Next)
}

impl Frame {
    fn new(method: MethodId, args: Vec<Value>) -> Frame {
        Frame {
            method,
            ip: 0,
            stack: Vec::new(),
            locals: Vec::new(),
            args,
            new_object: None,
            new_value: None,
            current_exception: None,
            pending: None,
            pending_filter: None,
            multicast: None,
            pending_constraint: None,
            buffers: Vec::new(),
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
        let value = self
            .args
            .get(slot as usize)
            .ok_or(Trap::ArgumentOutOfRange(slot))?
            .clone();
        self.stack.push(value);
        Ok(())
    }

    fn load_local(&mut self, slot: u16) {
        let value = self
            .locals
            .get(slot as usize)
            .cloned()
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

    /// `localloc` (III.3.47): allocate a zeroed buffer of `size` bytes in this frame and
    /// return a managed pointer to its start. C# zero-initializes `stackalloc`, so the
    /// block is zeroed; it is freed when the frame returns.
    fn localloc(&mut self, frame_index: usize, size: usize) -> Value {
        let buffer = self.buffers.len();
        self.buffers.push(alloc::vec![0u8; size]);
        Value::ByRef(Location::Stack {
            frame: frame_index,
            buffer,
            offset: 0,
        })
    }

    /// Reads `width` bytes (little-endian) at `offset` of localloc buffer `buffer`, or
    /// `None` if the access is out of range or the buffer does not exist. Reads beyond the
    /// last written byte see the zero-initialized fill.
    fn read_stack(&self, buffer: usize, offset: u32, width: usize) -> Option<[u8; 8]> {
        let bytes = self.buffers.get(buffer)?;
        let start = offset as usize;
        let end = start.checked_add(width)?;
        let slice = bytes.get(start..end)?;
        let mut out = [0u8; 8];
        out[..width].copy_from_slice(slice);
        Some(out)
    }

    /// Writes `width` little-endian bytes of `value` at `offset` of localloc buffer
    /// `buffer`. Errors (a wild-pointer trap) if the access is out of range or the buffer
    /// does not exist.
    fn write_stack(
        &mut self,
        buffer: usize,
        offset: u32,
        value: [u8; 8],
        width: usize,
    ) -> Result<(), Trap> {
        let bytes = self.buffers.get_mut(buffer).ok_or(Trap::NullReference)?;
        let start = offset as usize;
        let end = start.checked_add(width).ok_or(Trap::NullReference)?;
        let slice = bytes.get_mut(start..end).ok_or(Trap::NullReference)?;
        slice.copy_from_slice(&value[..width]);
        Ok(())
    }
}

/// The byte width an `ldind.*` / `stind.*` opcode transfers (III.3.42-43), or `None`
/// for `ldind.ref` / `stind.ref`, which move an object reference rather than raw bytes
/// (a `Location::Stack` pointer carries no object reference, so those do not apply to it).
fn indirect_width(opcode: Opcode) -> Option<usize> {
    match opcode {
        Opcode::LdindI1 | Opcode::LdindU1 | Opcode::StindI1 => Some(1),
        Opcode::LdindI2 | Opcode::LdindU2 | Opcode::StindI2 => Some(2),
        Opcode::LdindI4 | Opcode::LdindU4 | Opcode::StindI4 => Some(4),
        Opcode::LdindI8 | Opcode::StindI8 => Some(8),
        Opcode::LdindI | Opcode::StindI => Some(8),
        #[cfg(feature = "float")]
        Opcode::LdindR4 | Opcode::StindR4 => Some(4),
        #[cfg(feature = "float")]
        Opcode::LdindR8 | Opcode::StindR8 => Some(8),
        _ => None,
    }
}

/// Decodes the little-endian bytes an `ldind.*` read from a `Location::Stack` into the
/// stack value the typed opcode yields -- sign- or zero-extending a sub-`int32` integer
/// to `int32` (III.1.6), and producing the wider integer / native / float types directly.
fn stack_loaded_value(opcode: Opcode, raw: [u8; 8]) -> Result<Value, Trap> {
    let low8 = u64::from_le_bytes(raw);
    Ok(match opcode {
        Opcode::LdindI1 => Value::Int32(i32::from(raw[0] as i8)),
        Opcode::LdindU1 => Value::Int32(i32::from(raw[0])),
        Opcode::LdindI2 => Value::Int32(i32::from(i16::from_le_bytes([raw[0], raw[1]]))),
        Opcode::LdindU2 => Value::Int32(i32::from(u16::from_le_bytes([raw[0], raw[1]]))),
        Opcode::LdindI4 => Value::Int32(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])),
        Opcode::LdindU4 => Value::Int32(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as i32),
        Opcode::LdindI8 => Value::Int64(low8 as i64),
        Opcode::LdindI => Value::NativeInt(low8 as i64),
        #[cfg(feature = "float")]
        Opcode::LdindR4 => Value::Single(f32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]])),
        #[cfg(feature = "float")]
        Opcode::LdindR8 => Value::Float(f64::from_le_bytes(raw)),
        _ => return Err(Trap::TypeMismatch(opcode)),
    })
}

/// Encodes the stack value a `stind.*` stores into its low `width` little-endian bytes
/// (III.3.62) for a write into a `Location::Stack` buffer. The store opcode fixes the
/// width; the value is taken at that width (a wider integer is truncated, matching how
/// `stind.i1`/`i2` narrow).
fn stack_stored_bytes(opcode: Opcode, value: Value) -> Result<[u8; 8], Trap> {
    let raw = match opcode {
        #[cfg(feature = "float")]
        Opcode::StindR4 => match value {
            Value::Single(f) => u64::from(f.to_bits()),
            Value::Float(f) => u64::from(f32::to_bits(f as f32)),
            _ => return Err(Trap::TypeMismatch(opcode)),
        },
        #[cfg(feature = "float")]
        Opcode::StindR8 => match value {
            Value::Single(f) => f64::from(f).to_bits(),
            Value::Float(f) => f.to_bits(),
            _ => return Err(Trap::TypeMismatch(opcode)),
        },
        _ => match value {
            Value::Int32(n) => n as u32 as u64,
            Value::Int64(n) | Value::NativeInt(n) => n as u64,
            _ => return Err(Trap::TypeMismatch(opcode)),
        },
    };
    Ok(raw.to_le_bytes())
}

/// Performs a `stind.*` through a `localloc` pointer: writes the value's low bytes into
/// the owning frame's buffer at the opcode's width (little-endian). The pointer must name
/// this frame's buffer (the common same-frame `stackalloc` case); a pointer that escaped
/// to another frame is unsupported (it cannot reach that frame's storage from here).
fn store_stack_indirect(
    frame: &mut Frame,
    frame_index: usize,
    opcode: Opcode,
    location: Location,
    value: Value,
) -> Result<(), Trap> {
    let Location::Stack {
        frame: owner,
        buffer,
        offset,
    } = location
    else {
        return Err(Trap::TypeMismatch(opcode));
    };
    let width = indirect_width(opcode).ok_or(Trap::TypeMismatch(opcode))?;
    if owner != frame_index {
        return Err(Trap::Unsupported(opcode));
    }
    let bytes = stack_stored_bytes(opcode, value)?;
    frame.write_stack(buffer, offset, bytes, width)
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
///
/// `float32 op float32` computes at `f32` precision (so the rounded single-precision result
/// matches .NET); any operand of double precision lifts the other to `f64` and computes there
/// (ECMA-335 III.1.5: the F type carries implementation-defined precision, and C# inserts the
/// `conv.r8` for a mixed `float`/`double` expression, so widening here yields the same value).
fn binary_numeric(opcode: Opcode, a: Value, b: Value) -> Result<Value, Trap> {
    #[cfg(feature = "float")]
    if let (Value::Single(x), Value::Single(y)) = (&a, &b) {
        let (x, y) = (*x, *y);
        let result = match opcode {
            Opcode::Add => x + y,
            Opcode::Sub => x - y,
            Opcode::Mul => x * y,
            Opcode::Div => x / y,
            Opcode::Rem => x % y,
            _ => return Err(Trap::Unsupported(opcode)),
        };
        return Ok(Value::Single(result));
    }
    #[cfg(feature = "float")]
    if let (Some(x), Some(y)) = (as_double(&a), as_double(&b)) {
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

/// The `f64` value of a floating-point stack value (a `Single` widened, a `Float` directly), or
/// `None` for a non-float -- the seam mixed-precision float arithmetic and comparison share.
#[cfg(feature = "float")]
fn as_double(value: &Value) -> Option<f64> {
    match value {
        Value::Float(x) => Some(*x),
        Value::Single(x) => Some(f64::from(*x)),
        _ => None,
    }
}

/// The integer offset of an `add`/`sub` operand, if it is one (for stepping a localloc
/// pointer). A managed pointer is not an offset.
fn pointer_offset(value: &Value) -> Option<i64> {
    match value {
        Value::Int32(n) => Some(i64::from(*n)),
        Value::Int64(n) | Value::NativeInt(n) => Some(*n),
        _ => None,
    }
}

/// `add`/`sub` of a raw byte-addressed pointer and an integer byte offset, yielding the
/// adjusted pointer. This is the pointer walk behind C# `stackalloc` indexing (a
/// `Location::Stack` localloc pointer) and behind `fixed`-array indexing (a `Location::Element`
/// pointer into a pinned array, whose byte offset an eventual `ldind`/`stind` divides by the
/// element width). Returns `Ok(None)` when neither operand is such a pointer, so the caller
/// falls back to ordinary numeric arithmetic; `add` accepts the offset on either side (it is
/// commutative), `sub` as `pointer - offset` and as `pointer - pointer` (III.1.5). The latter
/// yields the signed BYTE difference as a native int (C# then divides by `sizeof(T)` for the
/// element count), defined only for two pointers into the same buffer/array.
fn stack_pointer_arithmetic(opcode: Opcode, a: &Value, b: &Value) -> Result<Option<Value>, Trap> {
    let add = matches!(opcode, Opcode::Add | Opcode::AddOvf | Opcode::AddOvfUn);
    let sub = matches!(opcode, Opcode::Sub | Opcode::SubOvf | Opcode::SubOvfUn);
    if !add && !sub {
        return Ok(None);
    }
    if sub {
        if let (Value::ByRef(la), Value::ByRef(lb)) = (a, b) {
            if is_raw_pointer(la) && is_raw_pointer(lb) {
                return Ok(raw_pointer_byte_difference(la, lb).map(Value::NativeInt));
            }
        }
    }
    let (location, offset) = match (a, b) {
        (Value::ByRef(location), other) if is_raw_pointer(location) => {
            let Some(delta) = pointer_offset(other) else {
                return Ok(None);
            };
            (location, delta)
        }
        (other, Value::ByRef(location)) if add && is_raw_pointer(location) => {
            let Some(delta) = pointer_offset(other) else {
                return Ok(None);
            };
            (location, delta)
        }
        _ => return Ok(None),
    };
    let signed = if sub { -offset } else { offset };
    let walked = |base: u32| i64::from(base).wrapping_add(signed) as u32;
    let stepped = match location {
        Location::Stack {
            frame,
            buffer,
            offset: base,
        } => Location::Stack {
            frame: *frame,
            buffer: *buffer,
            offset: walked(*base),
        },
        Location::Element {
            array,
            index,
            byte_offset,
        } => Location::Element {
            array: *array,
            index: *index,
            byte_offset: walked(*byte_offset),
        },
        _ => return Ok(None),
    };
    Ok(Some(Value::ByRef(stepped)))
}

/// Whether a managed pointer is one of the raw byte-addressed kinds that pointer arithmetic
/// walks: a `localloc` (`stackalloc`) buffer pointer or a pinned-array element pointer. The
/// typed-slot kinds (a frame local/arg, a field, a static, a box) are not byte-addressed.
fn is_raw_pointer(location: &Location) -> bool {
    matches!(location, Location::Stack { .. } | Location::Element { .. })
}

/// The signed BYTE difference `a - b` of two raw pointers into the SAME buffer/array, for
/// `pointer - pointer` (III.1.5). `None` for pointers into different allocations (or different
/// element bases) -- subtracting those is undefined, and returning `None` lets the op trap, as
/// in .NET.
fn raw_pointer_byte_difference(a: &Location, b: &Location) -> Option<i64> {
    match (a, b) {
        (
            Location::Stack {
                frame: fa,
                buffer: ba,
                offset: oa,
            },
            Location::Stack {
                frame: fb,
                buffer: bb,
                offset: ob,
            },
        ) if fa == fb && ba == bb => Some(i64::from(*oa) - i64::from(*ob)),
        (
            Location::Element {
                array: aa,
                index: ia,
                byte_offset: oa,
            },
            Location::Element {
                array: ab,
                index: ib,
                byte_offset: ob,
            },
        ) if aa == ab && ia == ib => Some(i64::from(*oa) - i64::from(*ob)),
        _ => None,
    }
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
        Opcode::AddOvf => x.checked_add(y).ok_or(Trap::Overflow)?,
        Opcode::SubOvf => x.checked_sub(y).ok_or(Trap::Overflow)?,
        Opcode::MulOvf => x.checked_mul(y).ok_or(Trap::Overflow)?,
        Opcode::AddOvfUn => (x as u32).checked_add(y as u32).ok_or(Trap::Overflow)? as i32,
        Opcode::SubOvfUn => (x as u32).checked_sub(y as u32).ok_or(Trap::Overflow)? as i32,
        Opcode::MulOvfUn => (x as u32).checked_mul(y as u32).ok_or(Trap::Overflow)? as i32,
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
        Opcode::AddOvf => x.checked_add(y).ok_or(Trap::Overflow)?,
        Opcode::SubOvf => x.checked_sub(y).ok_or(Trap::Overflow)?,
        Opcode::MulOvf => x.checked_mul(y).ok_or(Trap::Overflow)?,
        Opcode::AddOvfUn => (x as u64).checked_add(y as u64).ok_or(Trap::Overflow)? as i64,
        Opcode::SubOvfUn => (x as u64).checked_sub(y as u64).ok_or(Trap::Overflow)? as i64,
        Opcode::MulOvfUn => (x as u64).checked_mul(y as u64).ok_or(Trap::Overflow)? as i64,
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
        #[cfg(feature = "float")]
        Value::Float(x) => Ok(Value::Float(-x)),
        #[cfg(feature = "float")]
        Value::Single(x) => Ok(Value::Single(-x)),
        Value::Object(_) | Value::Null | Value::Struct(_) | Value::ByRef(_) => {
            Err(Trap::TypeMismatch(Opcode::Neg))
        }
        #[cfg(feature = "typed-references")]
        Value::TypedRef { .. } => Err(Trap::TypeMismatch(Opcode::Neg)),
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
    #[cfg(feature = "float")]
    if opcode == Opcode::ConvRUn {
        let unsigned = match value {
            Value::Int32(x) => u64::from(x as u32),
            Value::Int64(x) | Value::NativeInt(x) => x as u64,
            _ => return Err(Trap::TypeMismatch(opcode)),
        };
        return Ok(Value::Float(unsigned as f64));
    }

    #[cfg(feature = "float")]
    if matches!(opcode, Opcode::ConvR4 | Opcode::ConvR8) {
        let float = match value {
            Value::Int32(x) => f64::from(x),
            Value::Int64(x) | Value::NativeInt(x) => x as f64,
            Value::Float(f) => f,
            Value::Single(f) => f64::from(f),
            _ => return Err(Trap::TypeMismatch(opcode)),
        };
        return Ok(if opcode == Opcode::ConvR4 {
            Value::Single(float as f32)
        } else {
            Value::Float(float)
        });
    }

    let (source, from_32) = match value {
        Value::Int32(x) => (i64::from(x), true),
        Value::Int64(x) => (x, false),
        Value::NativeInt(x) => (x, false),
        #[cfg(feature = "float")]
        Value::Float(f) => (f as i64, false),
        #[cfg(feature = "float")]
        Value::Single(f) => (f as i64, false),
        Value::Object(_) | Value::Null | Value::Struct(_) | Value::ByRef(_) => {
            return Err(Trap::TypeMismatch(opcode));
        }
        #[cfg(feature = "typed-references")]
        Value::TypedRef { .. } => return Err(Trap::TypeMismatch(opcode)),
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

/// The checked conversions `conv.ovf.*`: like [`convert`] but yielding [`Trap::Overflow`]
/// (the `OverflowException` site) when the source does not fit the target type. The `.un`
/// forms read the source as unsigned.
fn convert_checked(opcode: Opcode, value: Value) -> Result<Value, Trap> {
    let unsigned_source = matches!(
        opcode,
        Opcode::ConvOvfI1Un
            | Opcode::ConvOvfI2Un
            | Opcode::ConvOvfI4Un
            | Opcode::ConvOvfI8Un
            | Opcode::ConvOvfU1Un
            | Opcode::ConvOvfU2Un
            | Opcode::ConvOvfU4Un
            | Opcode::ConvOvfU8Un
            | Opcode::ConvOvfIUn
            | Opcode::ConvOvfUUn
    );
    let source: i128 = match value {
        Value::Int32(x) if unsigned_source => i128::from(x as u32),
        Value::Int32(x) => i128::from(x),
        Value::Int64(x) | Value::NativeInt(x) if unsigned_source => i128::from(x as u64),
        Value::Int64(x) | Value::NativeInt(x) => i128::from(x),
        #[cfg(feature = "float")]
        Value::Float(f) if f.is_nan() || f.is_infinite() => return Err(Trap::Overflow),
        #[cfg(feature = "float")]
        Value::Float(f) => f as i128,
        #[cfg(feature = "float")]
        Value::Single(f) if f.is_nan() || f.is_infinite() => return Err(Trap::Overflow),
        #[cfg(feature = "float")]
        Value::Single(f) => f as i128,
        _ => return Err(Trap::TypeMismatch(opcode)),
    };
    let (min, max): (i128, i128) = match opcode {
        Opcode::ConvOvfI1 | Opcode::ConvOvfI1Un => (i128::from(i8::MIN), i128::from(i8::MAX)),
        Opcode::ConvOvfU1 | Opcode::ConvOvfU1Un => (0, i128::from(u8::MAX)),
        Opcode::ConvOvfI2 | Opcode::ConvOvfI2Un => (i128::from(i16::MIN), i128::from(i16::MAX)),
        Opcode::ConvOvfU2 | Opcode::ConvOvfU2Un => (0, i128::from(u16::MAX)),
        Opcode::ConvOvfI4 | Opcode::ConvOvfI4Un => (i128::from(i32::MIN), i128::from(i32::MAX)),
        Opcode::ConvOvfU4 | Opcode::ConvOvfU4Un => (0, i128::from(u32::MAX)),
        Opcode::ConvOvfI8 | Opcode::ConvOvfI8Un | Opcode::ConvOvfI | Opcode::ConvOvfIUn => {
            (i128::from(i64::MIN), i128::from(i64::MAX))
        }
        Opcode::ConvOvfU8 | Opcode::ConvOvfU8Un | Opcode::ConvOvfU | Opcode::ConvOvfUUn => {
            (0, i128::from(u64::MAX))
        }
        _ => return Err(Trap::Unsupported(opcode)),
    };
    if source < min || source > max {
        return Err(Trap::Overflow);
    }
    Ok(match opcode {
        Opcode::ConvOvfI8 | Opcode::ConvOvfI8Un | Opcode::ConvOvfU8 | Opcode::ConvOvfU8Un => {
            Value::Int64(source as i64)
        }
        Opcode::ConvOvfI | Opcode::ConvOvfIUn | Opcode::ConvOvfU | Opcode::ConvOvfUUn => {
            Value::NativeInt(source as i64)
        }
        _ => Value::Int32(source as i32),
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
    #[cfg(feature = "float")]
    if let (Some(x), Some(y)) = (as_double(&a), as_double(&b)) {
        if x.is_nan() || y.is_nan() {
            return Ok(unordered_or_unsigned && !matches!(relation, Relation::Equal));
        }
        return Ok(apply_relation(relation, x.partial_cmp(&y)));
    }
    if matches!(a, Value::Object(_) | Value::Null) || matches!(b, Value::Object(_) | Value::Null) {
        let equal = reference_equal(a, b);
        return match (relation, unordered_or_unsigned) {
            (Relation::Equal, _) => Ok(equal),
            (Relation::NotEqual, _) | (Relation::Greater, true) => Ok(!equal),
            _ => Err(Trap::TypeMismatch(opcode)),
        };
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

#[cfg(feature = "float")]
fn float32_operand(instruction: &Instruction) -> Result<f32, Trap> {
    match instruction.operand {
        Operand::Float32(value) => Ok(value),
        _ => Err(Trap::MalformedInstruction(instruction.opcode)),
    }
}

#[cfg(feature = "float")]
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

/// The object reference a field or instance instruction expects on the stack: an
/// object, [`Trap::NullReference`] for null, or [`Trap::TypeMismatch`] otherwise.
fn object_ref(value: Value, opcode: Opcode) -> Result<ObjectRef, Trap> {
    match value {
        Value::Object(reference) => Ok(reference),
        Value::Null => Err(Trap::NullReference),
        _ => Err(Trap::TypeMismatch(opcode)),
    }
}

/// Extracts a method id from a function pointer: `ldftn` / `ldvirtftn` push the method
/// id as a native int, and a delegate constructor consumes it.
fn function_pointer(value: Value) -> Result<MethodId, Trap> {
    match value {
        Value::NativeInt(method) => Ok(method as MethodId),
        _ => Err(Trap::TypeMismatch(Opcode::Newobj)),
    }
}

/// The runtime [`crate::module::TypeId`] a `callvirt` / `ldvirtftn` dispatches on for an
/// object receiver: a field-carrying instance's own type id; else `System.String`'s for a
/// heap string (which has no per-object type id) so the call reaches String's overrides;
/// else a boxed value type's declared type id (resolved from the box's tag) so an interface
/// method `callvirt` on a boxed struct reaches the struct's implementation. `None` for any
/// receiver with no resolvable declared type (an array / delegate / builder).
fn receiver_type_id(module: &Module, vm: &Vm, this: ObjectRef) -> Option<u32> {
    vm.heap().type_of(this).or_else(|| {
        if vm.heap().is_string(this) {
            module.string_type_id()
        } else {
            vm.heap()
                .boxed_type_token(this)
                .and_then(|token| module.type_id_by_handle(token))
        }
    })
}

/// Resolves a `callvirt` target on a `this` of `runtime_type`: an explicit interface
/// implementation (`MethodImpl`) wins outright -- it is the only way to reach a private,
/// interface-named body and to tell two same-signature interface methods apart -- then a
/// class virtual via the runtime type's vtable slot, else an interface/abstract method by
/// signature key, else the static target (a non-virtual instance method, or a string/array
/// `this`).
fn resolve_callvirt(
    module: &Module,
    static_method: Option<MethodId>,
    sig_key: Option<&str>,
    runtime_type: Option<u32>,
    explicit_override: Option<MethodId>,
) -> Option<MethodId> {
    if explicit_override.is_some() {
        return explicit_override;
    }
    if let Some(method) = static_method {
        if let Some(slot) = module.method_slot(method) {
            return Some(
                runtime_type
                    .and_then(|type_id| module.vtable_entry(type_id, slot))
                    .unwrap_or(method),
            );
        }
    }
    if let (Some(key), Some(type_id)) = (sig_key, runtime_type) {
        if let Some(method) = module.sig_dispatch(type_id, key) {
            return Some(method);
        }
    }
    static_method
}

/// Whether `value` can be `castclass`/`isinst` to `token`'s type: null matches; a
/// reference matches when its runtime type is a subtype of the target OR implements the
/// target interface.
///
/// The receiver's heap kind makes the test precise where the target is otherwise
/// unverifiable (an external core type with no module [`crate::module::TypeId`]):
/// - a declared **instance** matches via the subtype relation, or when its runtime type
///   (or a base) implements the target interface; an unresolved non-core target is treated
///   as a match (unverified -- an interface this module cannot see);
/// - a **boxed** value type matches `System.Object`, its own exact value type, or an
///   interface its value type declares -- so a boxed `int` is precisely *not* a `string` /
///   an unrelated class, but IS castable to an interface it implements;
/// - a heap **string** matches `System.String`, `System.Object`, or an interface the
///   `System.String` type declares, and nothing else.
fn cast_matches(module: &Module, asm: u8, vm: &Vm, value: &Value, token: Token) -> bool {
    let reference = match value {
        Value::Null => return true,
        Value::Object(reference) => *reference,
        _ => return false,
    };
    let target_type_id = module.type_id_of(asm, token);
    if let Some(runtime) = vm.heap().type_of(reference) {
        return match target_type_id {
            Some(target) => {
                module.is_subtype(runtime, target) || module.implements_interface(runtime, target)
            }
            None if module.is_object_type_token(asm, token) => true,
            None if module.is_string_type_token(asm, token) => false,
            None => true,
        };
    }
    if let Some(box_token) = vm.heap().boxed_type_token(reference) {
        if module.is_object_type_token(asm, token) {
            return true;
        }
        if box_token == asm_key(asm, token.0) {
            return true;
        }
        if let Some(box_type) = module.type_id_by_handle(box_token) {
            if let Some(target) = target_type_id {
                return box_type == target || module.implements_interface(box_type, target);
            }
        }
        return false;
    }
    if vm.heap().is_string(reference) {
        if module.is_string_type_token(asm, token) || module.is_object_type_token(asm, token) {
            return true;
        }
        if let (Some(string_type), Some(target)) = (module.string_type_id(), target_type_id) {
            return module.implements_interface(string_type, target);
        }
        return false;
    }
    true
}

/// Reference equality for `ceq` / `cgt.un`: two nulls are equal, two objects are
/// equal iff they are the same reference, and a null and an object differ.
fn reference_equal(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Null, Value::Null) => true,
        (Value::Object(x), Value::Object(y)) => x == y,
        _ => false,
    }
}

/// The non-negative length operand of `newarr`, as a `usize`.
fn array_length(value: Value) -> Result<usize, Trap> {
    let length = match value {
        Value::Int32(n) => i64::from(n),
        Value::Int64(n) | Value::NativeInt(n) => n,
        _ => return Err(Trap::TypeMismatch(Opcode::Newarr)),
    };
    usize::try_from(length).map_err(|_| Trap::IndexOutOfRange(length as i32))
}

/// The index operand of an array access, kept signed so a negative index reports as
/// out of range rather than wrapping.
fn array_index(value: Value, opcode: Opcode) -> Result<i32, Trap> {
    match value {
        Value::Int32(index) => Ok(index),
        Value::Int64(index) | Value::NativeInt(index) => {
            i32::try_from(index).map_err(|_| Trap::IndexOutOfRange(index as i32))
        }
        _ => Err(Trap::TypeMismatch(opcode)),
    }
}

/// Bounds-checks a signed array index against `len`, returning the `usize` index.
fn bounded_index(index: i32, len: usize) -> Result<usize, Trap> {
    usize::try_from(index)
        .ok()
        .filter(|&index| index < len)
        .ok_or(Trap::IndexOutOfRange(index))
}

/// Garbage-collection integration: enumerating the interpreter's roots for the heap's
/// mark-compact collector (the `gc` feature).
#[cfg(feature = "gc")]
impl Session {
    /// Reclaims unreachable objects and compacts the heap, relocating every live
    /// reference. Enumerates all roots -- each frame's eval stack, locals, arguments, and
    /// continuation state (`new_object`, `current_exception`, a pending `finally` chain's
    /// exception, an in-flight multicast), the entry's result, the statics, and the
    /// exception-message table. Called at an instruction boundary, where the frame state
    /// is consistent, so anything still live is reachable from these roots.
    fn collect_garbage(&mut self, module: &Module, vm: &mut Vm) {
        #[cfg(feature = "finalizers")]
        if vm.finalizing {
            return;
        }
        #[cfg(not(feature = "finalizers"))]
        let _ = module;
        let mut messages: Vec<Value> = Vec::with_capacity(vm.exception_messages.len() * 2);
        for (&exception, &message) in &vm.exception_messages {
            messages.push(Value::Object(exception));
            messages.push(Value::Object(message));
        }

        let Vm { heap, statics, .. } = vm;
        let frames = &mut self.frames;
        let result = &mut self.result;
        let finalizable = heap.collect(|visit| {
            for frame in frames.iter_mut() {
                for value in frame.stack.iter_mut() {
                    visit(value);
                }
                for value in frame.locals.iter_mut() {
                    visit(value);
                }
                for value in frame.args.iter_mut() {
                    visit(value);
                }
                visit_optional_ref(&mut frame.new_object, visit);
                visit_optional_location(&mut frame.new_value, visit);
                visit_optional_ref(&mut frame.current_exception, visit);
                if let Some(pending) = &mut frame.pending {
                    match &mut pending.then {
                        AfterFinally::Catch { exception, .. } | AfterFinally::Unwind(exception) => {
                            visit_ref(exception, visit);
                        }
                        AfterFinally::Goto(_) => {}
                    }
                }
                if let Some(filter) = &mut frame.pending_filter {
                    visit_ref(&mut filter.exception, visit);
                }
                if let Some((invocations, params)) = &mut frame.multicast {
                    for (target, _) in invocations.iter_mut() {
                        visit(target);
                    }
                    for value in params.iter_mut() {
                        visit(value);
                    }
                }
            }
            if let Some(Some(value)) = result.as_mut() {
                visit(value);
            }
            for value in statics.iter_mut() {
                visit(value);
            }
            for value in messages.iter_mut() {
                visit(value);
            }
        });

        vm.exception_messages = messages
            .chunks_exact(2)
            .filter_map(|pair| match (&pair[0], &pair[1]) {
                (Value::Object(key), Value::Object(value)) => Some((*key, *value)),
                _ => None,
            })
            .collect();

        #[cfg(not(feature = "finalizers"))]
        let _ = finalizable;
        #[cfg(feature = "finalizers")]
        if !finalizable.is_empty() {
            vm.finalizing = true;
            for object in finalizable {
                let Some(type_id) = vm.heap().type_of(object) else {
                    continue;
                };
                let Some(finalize) = module.finalizer_of(type_id) else {
                    continue;
                };
                if let Ok(mut session) =
                    Session::new(module, finalize, alloc::vec![Value::Object(object)])
                {
                    let _ = session.run(module, vm);
                }
            }
            vm.finalizing = false;
        }
    }
}

/// Relocates an optional heap-reference root through the collector's value visitor.
#[cfg(feature = "gc")]
fn visit_optional_ref(slot: &mut Option<ObjectRef>, visit: &mut dyn FnMut(&mut Value)) {
    if let Some(reference) = slot {
        visit_ref(reference, visit);
    }
}

/// Relocates a bare heap-reference root by mirroring it through a temporary `Value`; the
/// visitor marks it, and on the relocation pass rewrites the contained `ObjectRef`.
#[cfg(feature = "gc")]
fn visit_ref(reference: &mut ObjectRef, visit: &mut dyn FnMut(&mut Value)) {
    let mut wrapped = Value::Object(*reference);
    visit(&mut wrapped);
    if let Value::Object(new) = wrapped {
        *reference = new;
    }
}

/// Relocates a managed-pointer root (a [`Location`]) by mirroring it through a temporary
/// `Value::ByRef`; the visitor marks the heap object the pointer reaches and, on the
/// relocation pass, rewrites the contained `ObjectRef`. Used for `frame.new_value` -- the
/// pointer to the under-construction value type's box, which a GC during the constructor
/// body must relocate or the struct read back on return dangles to a stale (relocated)
/// object.
#[cfg(feature = "gc")]
fn visit_location(location: &mut Location, visit: &mut dyn FnMut(&mut Value)) {
    let mut wrapped = Value::ByRef(location.clone());
    visit(&mut wrapped);
    if let Value::ByRef(new) = wrapped {
        *location = new;
    }
}

#[cfg(feature = "gc")]
fn visit_optional_location(slot: &mut Option<Location>, visit: &mut dyn FnMut(&mut Value)) {
    if let Some(location) = slot {
        visit_location(location, visit);
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
    fn localloc_zero_inits_and_round_trips_an_int() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::Dup),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0x1234_5678)),
            Instruction::simple(Opcode::StindI4),
            Instruction::simple(Opcode::LdindI4),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(0x1234_5678))));
    }

    #[test]
    fn localloc_unwritten_bytes_read_as_zero() {
        let result = run(vec![
            Instruction::new(Opcode::LdcI4S, Operand::Int8(8)),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::LdindI4),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(0))));
    }

    #[test]
    fn localloc_pointer_add_indexes_bytes_little_endian() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::Stloc0),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0xCD)),
            Instruction::simple(Opcode::StindI1),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::Add),
            Instruction::new(Opcode::LdcI4, Operand::Int32(0xAB)),
            Instruction::simple(Opcode::StindI1),
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::LdindU2),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(0xABCD))));
    }

    #[test]
    fn localloc_out_of_range_store_traps() {
        let result = run(vec![
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::ConvU),
            Instruction::simple(Opcode::Localloc),
            Instruction::simple(Opcode::LdcI44),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::LdcI41),
            Instruction::simple(Opcode::StindI4),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Err(Trap::NullReference));
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
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            2,
        );
        let main = module.add_method(
            0,
            method(vec![
                Instruction::new(Opcode::LdcI4S, Operand::Int8(40)),
                Instruction::simple(Opcode::LdcI42),
                Instruction::new(Opcode::Call, Operand::Token(ADD_TOKEN)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        module.bind_token(0, ADD_TOKEN, add);

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(42)))
        );
    }

    #[test]
    fn recursion_sums_one_to_n_across_frames() {
        let mut module = Module::new();
        let sum = module.add_method(
            0,
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
        module.bind_token(0, SELF_TOKEN, sum);

        assert_eq!(
            super::run(&module, &mut Vm::new(), sum, alloc::vec![Value::Int32(5)]),
            Ok(Some(Value::Int32(15)))
        );
    }

    #[test]
    fn an_unbound_call_token_traps() {
        let mut module = Module::new();
        let main = module.add_method(
            0,
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
            0,
            method(vec![
                Instruction::new(Opcode::Call, Operand::Token(SELF_TOKEN)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        module.bind_token(0, SELF_TOKEN, loop_method);

        assert_eq!(
            super::run(&module, &mut Vm::new(), loop_method, Vec::new()),
            Err(Trap::CallStackOverflow)
        );
    }

    #[test]
    fn session_single_steps_and_inspects_the_stack() {
        let mut module = Module::new();
        let main = module.add_method(
            0,
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
        assert_eq!(session.step(&module, &mut vm), Ok(Status::Running));
        assert_eq!(
            session.frame(0).unwrap().stack,
            [Value::Int32(2)].as_slice()
        );
        assert_eq!(session.step(&module, &mut vm), Ok(Status::Running));
        assert_eq!(session.step(&module, &mut vm), Ok(Status::Running));
        assert_eq!(
            session.frame(0).unwrap().stack,
            [Value::Int32(5)].as_slice()
        );
        assert_eq!(
            session.step(&module, &mut vm),
            Ok(Status::Done(Some(Value::Int32(5))))
        );
    }

    #[test]
    fn session_pauses_at_a_breakpoint() {
        let mut module = Module::new();
        let main = module.add_method(
            0,
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

        assert_eq!(session.resume(&module, &mut vm), Ok(Status::Paused));
        assert_eq!(session.frame(0).unwrap().ip, 2);
        assert_eq!(
            session.frame(0).unwrap().stack,
            [Value::Int32(2), Value::Int32(3)].as_slice()
        );
        assert_eq!(session.step(&module, &mut vm), Ok(Status::Running));
        assert_eq!(
            session.resume(&module, &mut vm),
            Ok(Status::Done(Some(Value::Int32(5))))
        );
    }

    #[test]
    fn session_exposes_the_call_stack_at_a_breakpoint() {
        let mut module = Module::new();
        let add = module.add_method(
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::simple(Opcode::Add),
                Instruction::simple(Opcode::Ret),
            ]),
            2,
        );
        let main = module.add_method(
            0,
            method(vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::simple(Opcode::LdcI43),
                Instruction::new(Opcode::Call, Operand::Token(ADD_TOKEN)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        module.bind_token(0, ADD_TOKEN, add);
        let mut vm = Vm::new();
        let mut session = Session::new(&module, main, Vec::new()).unwrap();
        session.add_breakpoint(add, 0);

        assert_eq!(session.resume(&module, &mut vm), Ok(Status::Paused));
        assert_eq!(session.depth(), 2);
        assert_eq!(session.frame(0).unwrap().method, main);
        assert_eq!(session.frame(1).unwrap().method, add);
        assert_eq!(
            session.frame(1).unwrap().args,
            [Value::Int32(2), Value::Int32(3)].as_slice()
        );
        assert_eq!(session.step(&module, &mut vm), Ok(Status::Running));
        assert_eq!(
            session.resume(&module, &mut vm),
            Ok(Status::Done(Some(Value::Int32(5))))
        );
    }

    #[test]
    fn newobj_constructs_then_instance_calls_read_and_write_a_field() {
        let count_field = Token(0x0400_0001);
        let ctor_token = Token(0x0600_0010);
        let inc_token = Token(0x0600_0011);
        let get_token = Token(0x0600_0012);

        let mut module = Module::new();
        let counter = module.add_type(vec![Value::Int32(0)]);
        module.bind_field(0, count_field, 0);

        let ctor = module.add_method(
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg1),
                Instruction::new(Opcode::Stfld, Operand::Token(count_field)),
                Instruction::simple(Opcode::Ret),
            ]),
            2,
        );
        module.set_method_type(ctor, counter);
        module.bind_token(0, ctor_token, ctor);

        let inc = module.add_method(
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::simple(Opcode::Ldarg0),
                Instruction::new(Opcode::Ldfld, Operand::Token(count_field)),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::Add),
                Instruction::new(Opcode::Stfld, Operand::Token(count_field)),
                Instruction::simple(Opcode::Ret),
            ]),
            1,
        );
        module.set_method_type(inc, counter);
        module.bind_token(0, inc_token, inc);

        let get = module.add_method(
            0,
            method(vec![
                Instruction::simple(Opcode::Ldarg0),
                Instruction::new(Opcode::Ldfld, Operand::Token(count_field)),
                Instruction::simple(Opcode::Ret),
            ]),
            1,
        );
        module.set_method_type(get, counter);
        module.bind_token(0, get_token, get);

        let main = module.add_method(
            0,
            method(vec![
                Instruction::new(Opcode::LdcI4S, Operand::Int8(10)),
                Instruction::new(Opcode::Newobj, Operand::Token(ctor_token)),
                Instruction::simple(Opcode::Stloc0),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::new(Opcode::Call, Operand::Token(inc_token)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::new(Opcode::Call, Operand::Token(inc_token)),
                Instruction::simple(Opcode::Ldloc0),
                Instruction::new(Opcode::Call, Operand::Token(get_token)),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(12)))
        );
    }

    #[test]
    fn arrays_allocate_store_load_and_measure_length() {
        let elem = Token(0x0100_0005);
        let mut module = Module::new();
        module.bind_array_default(0, elem, Value::Int32(0));

        let store = |index: Opcode, value: i8| {
            [
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(index),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(value)),
                Instruction::simple(Opcode::StelemI4),
            ]
        };
        let load = |index: Opcode| {
            [
                Instruction::simple(Opcode::Ldloc0),
                Instruction::simple(index),
                Instruction::simple(Opcode::LdelemI4),
            ]
        };
        let mut code = alloc::vec![
            Instruction::simple(Opcode::LdcI43),
            Instruction::new(Opcode::Newarr, Operand::Token(elem)),
            Instruction::simple(Opcode::Stloc0),
        ];
        code.extend(store(Opcode::LdcI40, 10));
        code.extend(store(Opcode::LdcI41, 20));
        code.extend(store(Opcode::LdcI42, 30));
        code.extend(load(Opcode::LdcI40));
        code.extend(load(Opcode::LdcI41));
        code.push(Instruction::simple(Opcode::Add));
        code.extend(load(Opcode::LdcI42));
        code.push(Instruction::simple(Opcode::Add));
        code.extend([
            Instruction::simple(Opcode::Ldloc0),
            Instruction::simple(Opcode::Ldlen),
            Instruction::simple(Opcode::ConvI4),
            Instruction::simple(Opcode::Add),
            Instruction::simple(Opcode::Ret),
        ]);
        let main = module.add_method(0, method(code), 0);

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(63)))
        );
    }

    #[test]
    fn array_index_out_of_range_throws() {
        let elem = Token(0x0100_0005);
        let mut module = Module::new();
        module.bind_array_default(0, elem, Value::Int32(0));
        let main = module.add_method(
            0,
            method(vec![
                Instruction::simple(Opcode::LdcI42),
                Instruction::new(Opcode::Newarr, Operand::Token(elem)),
                Instruction::simple(Opcode::LdcI45),
                Instruction::simple(Opcode::LdelemI4),
                Instruction::simple(Opcode::Ret),
            ]),
            0,
        );
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Err(Trap::UnhandledException)
        );
    }

    #[test]
    fn callvirt_dispatches_on_the_runtime_type() {
        let ctor_token = Token(0x0600_0020);
        let speak_token = Token(0x0600_0021);

        let mut module = Module::new();
        let base = module.add_type(vec![]);
        let derived = module.add_type(vec![]);

        let base_speak = module.add_method(
            0,
            method(vec![Instruction::simple(Opcode::LdcI41), ret()]),
            1,
        );
        module.set_method_type(base_speak, base);
        let derived_speak = module.add_method(
            0,
            method(vec![Instruction::simple(Opcode::LdcI42), ret()]),
            1,
        );
        module.set_method_type(derived_speak, derived);

        module.set_vtable(base, vec![base_speak]);
        module.set_vtable(derived, vec![derived_speak]);
        module.bind_method_slot(base_speak, 0);
        module.bind_method_slot(derived_speak, 0);

        let ctor = module.add_method(0, method(vec![ret()]), 1);
        module.set_method_type(ctor, derived);
        module.bind_token(0, ctor_token, ctor);
        module.bind_token(0, speak_token, base_speak);

        let main = module.add_method(
            0,
            method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(ctor_token)),
                Instruction::new(Opcode::Callvirt, Operand::Token(speak_token)),
                ret(),
            ]),
            0,
        );

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(2)))
        );
    }

    #[test]
    fn static_fields_persist_across_calls() {
        let field = Token(0x0400_0009);
        let bump_token = Token(0x0600_0030);

        let mut module = Module::new();
        module.bind_static_field(0, field, Value::Int32(0));
        let bump = module.add_method(
            0,
            method(vec![
                Instruction::new(Opcode::Ldsfld, Operand::Token(field)),
                Instruction::simple(Opcode::LdcI41),
                Instruction::simple(Opcode::Add),
                Instruction::new(Opcode::Stsfld, Operand::Token(field)),
                ret(),
            ]),
            0,
        );
        module.bind_token(0, bump_token, bump);
        let main = module.add_method(
            0,
            method(vec![
                Instruction::new(Opcode::Call, Operand::Token(bump_token)),
                Instruction::new(Opcode::Call, Operand::Token(bump_token)),
                Instruction::new(Opcode::Ldsfld, Operand::Token(field)),
                ret(),
            ]),
            0,
        );

        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(2)))
        );
    }

    #[test]
    fn castclass_to_an_unrelated_type_throws() {
        let (module, main) = cast_program(Opcode::Castclass);
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Err(Trap::UnhandledException)
        );
    }

    #[test]
    fn isinst_of_an_unrelated_type_is_null() {
        let (module, main) = cast_program(Opcode::Isinst);
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Null))
        );
    }

    #[test]
    fn isinst_matches_a_boxed_value_type_via_a_cross_assembly_token() {
        let box_token = Token(0x0100_0007);
        let test_token = Token(0x0100_0009);
        let vt_ctor = Token(0x0600_0041);
        let mut module = Module::new();
        let vt = module.add_type(vec![Value::Int32(0)]);
        module.set_type_is_value_type(vt, true);
        module.bind_type_token(0, box_token, vt);
        module.bind_type_token(0, test_token, vt);
        let ctor = module.add_method(0, method(vec![ret()]), 1);
        module.set_method_type(ctor, vt);
        module.bind_token(0, vt_ctor, ctor);
        module.mark_value_type_ctor(0, vt_ctor);
        let main = module.add_method(
            0,
            method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(vt_ctor)),
                Instruction::new(Opcode::Box, Operand::Token(box_token)),
                Instruction::new(Opcode::Isinst, Operand::Token(test_token)),
                Instruction::new(Opcode::BrtrueS, Operand::Target(6)),
                Instruction::simple(Opcode::LdcI40),
                ret(),
                Instruction::simple(Opcode::LdcI41),
                ret(),
            ]),
            0,
        );
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Ok(Some(Value::Int32(1)))
        );
    }

    fn cast_program(op: Opcode) -> (Module, MethodId) {
        let b_token = Token(0x0200_0002);
        let a_ctor = Token(0x0600_0040);
        let mut module = Module::new();
        let a = module.add_type(vec![]);
        let b = module.add_type(vec![]);
        module.bind_type_token(0, b_token, b);
        let ctor = module.add_method(0, method(vec![ret()]), 1);
        module.set_method_type(ctor, a);
        module.bind_token(0, a_ctor, ctor);
        let main = module.add_method(
            0,
            method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(a_ctor)),
                Instruction::new(op, Operand::Token(b_token)),
                ret(),
            ]),
            0,
        );
        (module, main)
    }

    #[test]
    fn an_unhandled_exception_traps() {
        let e_ctor = Token(0x0600_0050);
        let mut module = Module::new();
        let e = module.add_type(vec![]);
        let ctor = module.add_method(0, method(vec![ret()]), 1);
        module.set_method_type(ctor, e);
        module.bind_token(0, e_ctor, ctor);
        let main = module.add_method(
            0,
            method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(e_ctor)),
                Instruction::simple(Opcode::Throw),
                ret(),
            ]),
            0,
        );
        assert_eq!(
            super::run(&module, &mut Vm::new(), main, Vec::new()),
            Err(Trap::UnhandledException)
        );
    }

    #[cfg(feature = "typed-references")]
    mod typed_references {
        use super::*;

        const INT_TOKEN: Token = Token(0x0100_0001);
        const OTHER_TOKEN: Token = Token(0x0100_0002);

        #[test]
        fn refvalue_round_trips_the_value_through_a_typedref() {
            let mut module = Module::new();
            let main = module.add_method(
                0,
                method(vec![
                    Instruction::simple(Opcode::LdcI45),
                    Instruction::simple(Opcode::Stloc0),
                    Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                    Instruction::new(Opcode::Mkrefany, Operand::Token(INT_TOKEN)),
                    Instruction::new(Opcode::Refanyval, Operand::Token(INT_TOKEN)),
                    Instruction::simple(Opcode::LdindI4),
                    ret(),
                ]),
                0,
            );
            assert_eq!(
                super::super::run(&module, &mut Vm::new(), main, Vec::new()),
                Ok(Some(Value::Int32(5)))
            );
        }

        #[test]
        fn refvalue_with_a_mismatched_type_throws() {
            let mut module = Module::new();
            let main = module.add_method(
                0,
                method(vec![
                    Instruction::simple(Opcode::LdcI45),
                    Instruction::simple(Opcode::Stloc0),
                    Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                    Instruction::new(Opcode::Mkrefany, Operand::Token(INT_TOKEN)),
                    Instruction::new(Opcode::Refanyval, Operand::Token(OTHER_TOKEN)),
                    Instruction::simple(Opcode::LdindI4),
                    ret(),
                ]),
                0,
            );
            assert_eq!(
                super::super::run(&module, &mut Vm::new(), main, Vec::new()),
                Err(Trap::UnhandledException)
            );
        }

        #[test]
        fn reftype_yields_the_referent_type_handle() {
            let mut module = Module::new();
            let main = module.add_method(
                0,
                method(vec![
                    Instruction::simple(Opcode::LdcI45),
                    Instruction::simple(Opcode::Stloc0),
                    Instruction::new(Opcode::LdlocaS, Operand::Variable(0)),
                    Instruction::new(Opcode::Mkrefany, Operand::Token(INT_TOKEN)),
                    Instruction::simple(Opcode::Refanytype),
                    Instruction::simple(Opcode::ConvI4),
                    ret(),
                ]),
                0,
            );
            let expected = asm_key(0, INT_TOKEN.0) as i32;
            assert_eq!(
                super::super::run(&module, &mut Vm::new(), main, Vec::new()),
                Ok(Some(Value::Int32(expected)))
            );
        }

        #[test]
        fn mkrefany_on_a_non_pointer_is_a_type_mismatch() {
            let result = run(vec![
                Instruction::simple(Opcode::LdcI45),
                Instruction::new(Opcode::Mkrefany, Operand::Token(INT_TOKEN)),
                ret(),
            ]);
            assert_eq!(result, Err(Trap::TypeMismatch(Opcode::Mkrefany)));
        }
    }

    #[cfg(feature = "varargs")]
    #[test]
    fn arglist_pushes_a_placeholder_handle() {
        let result = run(vec![
            Instruction::simple(Opcode::Arglist),
            Instruction::simple(Opcode::ConvI4),
            Instruction::simple(Opcode::Ret),
        ]);
        assert_eq!(result, Ok(Some(Value::Int32(0))));
    }

    fn ret() -> Instruction {
        Instruction::simple(Opcode::Ret)
    }

    mod debug_core {
        use super::*;

        fn call_program() -> (Module, MethodId, MethodId) {
            let mut module = Module::new();
            let add = module.add_method(
                0,
                method(vec![
                    Instruction::simple(Opcode::Ldarg0),
                    Instruction::simple(Opcode::Ldarg1),
                    Instruction::simple(Opcode::Add),
                    ret(),
                ]),
                2,
            );
            module.bind_token(0, ADD_TOKEN, add);
            let main = module.add_method(
                0,
                method(vec![
                    Instruction::simple(Opcode::LdcI42),
                    Instruction::simple(Opcode::LdcI43),
                    Instruction::new(Opcode::Call, Operand::Token(ADD_TOKEN)),
                    ret(),
                ]),
                0,
            );
            (module, main, add)
        }

        #[test]
        fn continue_stops_at_an_enabled_breakpoint_then_runs_to_completion() {
            let (module, main, _add) = call_program();
            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.add_breakpoint(main, 2);

            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Breakpoint);
            assert_eq!(
                stop.location,
                Some(CodeLocation {
                    method: main,
                    instruction: 2,
                })
            );
            assert_eq!(
                session.frame(0).unwrap().stack,
                [Value::Int32(2), Value::Int32(3)].as_slice()
            );
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.location, None);
            assert_eq!(stop.returned, Some(Value::Int32(5)));
        }

        #[test]
        fn a_disabled_breakpoint_does_not_pause_and_re_enabling_restores_it() {
            let (module, main, _add) = call_program();
            let mut vm = Vm::new();

            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.add_breakpoint(main, 2);
            session.set_breakpoint_enabled(main, 2, false);
            assert!(!session.is_breakpoint_enabled(main, 2));
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.returned, Some(Value::Int32(5)));
            assert_eq!(stop.location, None);

            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.set_breakpoint_enabled(main, 2, false);
            session.set_breakpoint_enabled(main, 2, true);
            assert!(session.is_breakpoint_enabled(main, 2));
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Breakpoint);
            assert_eq!(stop.location.unwrap().instruction, 2);
        }

        #[test]
        fn step_into_descends_into_a_call_while_step_over_runs_it_to_completion() {
            let (module, main, add) = call_program();
            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            assert_eq!(session.depth(), 1);
            let stop = session.step_into(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Step);
            assert_eq!(session.depth(), 2);
            assert_eq!(stop.location.unwrap().method, add);
            assert_eq!(stop.location.unwrap().instruction, 0);

            let (module, main, _add) = call_program();
            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            let stop = session.step_over(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Step);
            assert_eq!(session.depth(), 1);
            assert_eq!(stop.location.unwrap().method, main);
        }

        #[test]
        fn step_out_returns_to_the_caller() {
            let (module, main, _add) = call_program();
            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            assert_eq!(session.depth(), 2);
            let stop = session.step_out(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Step);
            assert_eq!(session.depth(), 1);
            assert_eq!(stop.location.unwrap().method, main);
        }

        #[test]
        fn a_breakpoint_inside_a_stepped_over_call_takes_priority() {
            let (module, main, add) = call_program();
            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.add_breakpoint(add, 2);
            session.step_into(&module, &mut vm).unwrap();
            session.step_into(&module, &mut vm).unwrap();
            let stop = session.step_over(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Breakpoint);
            assert_eq!(stop.location.unwrap().method, add);
            assert_eq!(session.depth(), 2);
        }

        #[test]
        fn location_and_this_report_the_innermost_frame() {
            let v_field = Token(0x0400_00A1);
            let read_token = Token(0x0600_00A1);
            let ctor_token = Token(0x0600_00A2);
            let mut module = Module::new();
            let c = module.add_type(vec![Value::Int32(0)]);
            module.bind_field(0, v_field, 0);
            let ctor = module.add_method(0, method(vec![ret()]), 1);
            module.set_method_type(ctor, c);
            module.bind_token(0, ctor_token, ctor);
            let read = module.add_method(
                0,
                method(vec![
                    Instruction::simple(Opcode::Ldarg0),
                    Instruction::new(Opcode::Ldfld, Operand::Token(v_field)),
                    ret(),
                ]),
                1,
            );
            module.set_method_type(read, c);
            module.bind_token(0, read_token, read);
            module.set_method_debug(read, "C.Read".into(), alloc::vec!["this".into()]);
            let main = module.add_method(
                0,
                method(vec![
                    Instruction::new(Opcode::Newobj, Operand::Token(ctor_token)),
                    Instruction::new(Opcode::Callvirt, Operand::Token(read_token)),
                    ret(),
                ]),
                0,
            );

            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.add_breakpoint(read, 0);
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Breakpoint);
            assert_eq!(session.location().unwrap().method, read);
            assert!(matches!(session.this(&module), Some(Value::Object(_))));
            assert!(session.this(&module).is_some());
            assert_eq!(module.arg_name(main, 0), None);
        }

        #[test]
        fn expand_reads_array_elements_and_object_fields() {
            let mut vm = Vm::new();
            let array = vm
                .heap_mut()
                .alloc_array(alloc::vec![Value::Int32(10), Value::Int32(20), Value::Int32(30)]);
            let instance = vm
                .heap_mut()
                .alloc_instance(0, alloc::vec![Value::Int32(7), Value::Null]);

            let mut module = Module::new();
            let main = module.add_method(0, method(vec![ret()]), 0);
            let session = Session::new(&module, main, Vec::new()).unwrap();

            assert!(session.expand(&vm, &Value::Int32(5)).is_empty());
            assert!(!session.is_expandable(&vm, &Value::Int32(5)));

            assert!(session.is_expandable(&vm, &Value::Object(array)));
            let elements = session.expand(&vm, &Value::Object(array));
            assert_eq!(elements.len(), 3);
            assert_eq!(elements[0].name, "[0]");
            assert_eq!(elements[0].value, Value::Int32(10));
            assert_eq!(elements[2].value, Value::Int32(30));

            assert!(session.is_expandable(&vm, &Value::Object(instance)));
            let fields = session.expand(&vm, &Value::Object(instance));
            assert_eq!(fields.len(), 2);
            assert_eq!(fields[0].name, "field0");
            assert_eq!(fields[0].value, Value::Int32(7));
            assert_eq!(fields[1].value, Value::Null);
        }

        #[cfg(feature = "exceptions")]
        #[test]
        fn pause_on_unhandled_exception_reports_the_exception_instead_of_trapping() {
            let e_ctor = Token(0x0600_00B1);
            let mut module = Module::new();
            let e = module.add_type(vec![]);
            let ctor = module.add_method(0, method(vec![ret()]), 1);
            module.set_method_type(ctor, e);
            module.bind_token(0, e_ctor, ctor);
            let main = module.add_method(
                0,
                method(vec![
                    Instruction::new(Opcode::Newobj, Operand::Token(e_ctor)),
                    Instruction::simple(Opcode::Throw),
                    ret(),
                ]),
                0,
            );

            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            assert_eq!(
                session.continue_(&module, &mut vm),
                Err(Trap::UnhandledException)
            );

            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.set_pause_on_unhandled_exception(true);
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.reason, StopReason::Exception);
            assert!(session.stopped_exception().is_some());
        }

        #[cfg(feature = "exceptions")]
        #[test]
        fn a_caught_exception_does_not_trigger_the_exception_pause() {
            use lamella_cil::{EhClause, EhKind, InstructionRange};
            let e_ctor = Token(0x0600_00C1);
            let catch_type = Token(0x0100_00C2);
            let mut module = Module::new();
            let e = module.add_type(vec![]);
            let ctor = module.add_method(0, method(vec![ret()]), 1);
            module.set_method_type(ctor, e);
            module.bind_token(0, e_ctor, ctor);
            let mut body = method(vec![
                Instruction::new(Opcode::Newobj, Operand::Token(e_ctor)),
                Instruction::simple(Opcode::Throw),
                Instruction::simple(Opcode::Pop),
                Instruction::new(Opcode::LdcI4S, Operand::Int8(42)),
                ret(),
            ]);
            body.handlers = alloc::vec![EhClause {
                try_range: InstructionRange { start: 0, end: 2 },
                handler_range: InstructionRange { start: 2, end: 5 },
                kind: EhKind::Catch(catch_type),
            }]
            .into_boxed_slice();
            let main = module.add_method(0, body, 0);

            let mut vm = Vm::new();
            let mut session = Session::new(&module, main, Vec::new()).unwrap();
            session.set_pause_on_unhandled_exception(true);
            let stop = session.continue_(&module, &mut vm).unwrap();
            assert_eq!(stop.returned, Some(Value::Int32(42)));
            assert_eq!(session.stopped_exception(), None);
        }
    }
}
