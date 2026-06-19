#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The debug backend seam: the interface a debug target implements so a Debug Adapter
//! Protocol adapter can drive it.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// A debug target the adapter drives. Object-safe so the adapter can hold a
/// `Box<dyn DebugBackend>` chosen at launch (interpreter, or device over CMSIS-DAP).
pub trait DebugBackend {
    /// Begins (or restarts) execution, leaving the target stopped at its first
    /// location, ready for breakpoints and `configurationDone`. Returns `false` if
    /// the target could not be started. Interpreter: create the `Session` at the
    /// entry point. Device: reset-and-halt at the entry (a fresh `launch`), or attach
    /// to a running target and halt it (`attach`).
    fn launch(&mut self) -> bool;

    /// Resumes until a breakpoint, completion, or fault. Interpreter: run the
    /// `Session`. Device: clear the halt and run until a BPU match / the program ends.
    fn resume(&mut self) -> Stop;

    /// Executes one instruction, descending into a call. Interpreter: one CIL
    /// instruction. Device: one native instruction step (the adapter composes
    /// `step-over` / `step-out` from this plus [`DebugBackend::depth`]).
    fn step(&mut self) -> Stop;

    /// Polls a target a previous `resume`/`step` left [`Stop::Running`]: returns
    /// `Running` while it is still going, or the eventual stop (so the adapter can then
    /// emit the stopped event). The default suits a synchronous backend -- the
    /// interpreter finishes inside `resume`/`step` and never returns `Running` -- so it
    /// is never polled; a free-running device overrides this to read its halt state.
    fn poll(&mut self) -> Stop {
        Stop::Done
    }

    /// The current call depth, so the adapter can express depth-relative stepping
    /// (`next` stays at or above the start depth, `stepOut` runs until below it). A
    /// backend without unwinding may report `1` (then `next`/`stepOut` degrade to step).
    fn depth(&self) -> usize;

    /// Replaces the breakpoints, each an opaque code address (see the module docs).
    /// Interpreter: `(method, instruction)` pairs. Device: native code addresses, set
    /// as hardware BPU comparators.
    fn set_breakpoints(&mut self, addresses: &[u64]);

    /// The call stack, innermost frame first (DAP's order). Each frame carries its
    /// opaque address, a display name, and a 1-based line (the CIL index until the
    /// compiler's sequence points map it to source).
    fn stack(&self) -> Vec<Frame>;

    /// The variables in one scope of frame `index`. Interpreter: the frame's
    /// arguments / locals / evaluation-stack slots. Device: locals/arguments recovered
    /// from the AOT debug-info (register or stack-slot homes), read via memory/regs.
    fn variables(&self, frame: usize, scope: Scope) -> Vec<Variable>;

    /// Reads `len` bytes of target memory at `address`. Device: an ADIv5 MEM-AP read.
    /// Interpreter: the managed heap is not flat addressable, so this is typically
    /// empty -- inspection goes through [`DebugBackend::variables`] instead.
    fn read_memory(&self, address: u64, len: usize) -> Vec<u8>;

    /// The target's registers. Device: the Cortex-M core registers. Interpreter: a
    /// synthetic view (the instruction pointer; an empty set is also valid).
    fn read_registers(&self) -> Vec<Register>;

    /// Disassembles `count` locations starting `offset` locations from `address` (the
    /// offset may be negative, for context before it), each with its own opaque address
    /// so a client can place instruction breakpoints. The backend owns what "one
    /// location away" means (interpreter: the next CIL index; device: the next native
    /// instruction). Interpreter: CIL; device: native instructions (or empty).
    fn disassemble(&self, address: u64, offset: i64, count: usize) -> Vec<Disassembled>;

    /// Program output produced since the previous call (so the adapter can forward it
    /// incrementally as `output` events). Interpreter: new console output. Device: new
    /// bytes from the target's UART / semihosting channel.
    fn take_output(&mut self) -> Option<String>;
}

/// Why execution stopped after a `resume` or `step`.
pub enum Stop {
    /// Paused at a breakpoint.
    Breakpoint,
    /// Paused after completing a step.
    Step,
    /// The program ran to completion.
    Done,
    /// The target is now running and has not stopped yet: a resume-now backend (a
    /// free-running device) returns this rather than blocking. The adapter emits no
    /// stopped event and polls ([`DebugBackend::poll`]) for the eventual stop; a
    /// synchronous backend (the interpreter, which finishes inside `resume`) never
    /// returns it.
    Running,
    /// A fault ended the run, with a human-readable description.
    Fault(String),
}

/// One call-stack frame: an opaque code address, a display name, and a 1-based line.
pub struct Frame {
    /// The frame's current code location (opaque; see the module docs).
    pub address: u64,
    /// A display name for the frame (a method name once metadata names are wired).
    pub name: String,
    /// The 1-based line shown in the editor (the CIL index until source mapping).
    pub line: u32,
}

/// A variable scope of a frame.
pub enum Scope {
    /// The method's arguments.
    Arguments,
    /// The method's local variables.
    Locals,
    /// The method's evaluation stack (interpreter-specific; empty on a native target).
    Stack,
}

/// One inspected variable: a name, a rendered value, and a type name.
pub struct Variable {
    /// The variable's name (e.g. `arg0`, `local2`).
    pub name: String,
    /// The value rendered for display.
    pub value: String,
    /// The type name shown beside the value.
    pub kind: String,
}

/// One target register and its value.
pub struct Register {
    /// The register name (e.g. `r0`, `pc`).
    pub name: String,
    /// Its current value.
    pub value: u64,
}

/// One disassembled location: its opaque address and rendered text.
pub struct Disassembled {
    /// The location's opaque address.
    pub address: u64,
    /// The rendered instruction text.
    pub text: String,
}
