//! Traps: controlled execution failures reported instead of panicking.

use core::fmt;
use lamella_cil::Opcode;
use lamella_token::Token;

/// A controlled execution failure.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Trap {
    /// An instruction needed more values than the evaluation stack held.
    StackUnderflow,
    /// Execution ran off the end of the method without a `ret`.
    FellThroughEnd,
    /// The interpreter does not implement this opcode yet.
    Unsupported(Opcode),
    /// An operation was applied to evaluation-stack types it does not accept
    /// (ECMA-335 1st ed, III.1.5 operand-type tables).
    TypeMismatch(Opcode),
    /// An instruction carried an operand of the wrong shape -- a malformed
    /// instruction that should not survive decoding.
    MalformedInstruction(Opcode),
    /// A local-variable slot was out of range for the method.
    LocalOutOfRange(u16),
    /// An argument slot was out of range for the call.
    ArgumentOutOfRange(u16),
    /// A branch named an instruction index outside the method.
    BranchOutOfRange(u32),
    /// A string or array index was outside its bounds.
    IndexOutOfRange(i32),
    /// A field access, method call, or unbox dereferenced the null reference (the
    /// `NullReferenceException` site, until exceptions exist).
    NullReference,
    /// A `castclass` to a type the object is not an instance of (the
    /// `InvalidCastException` site, until exceptions exist).
    InvalidCast,
    /// An argument was invalid (the `ArgumentException` site) -- e.g. `Enum.Parse` of a
    /// name that names no constant of the enum.
    InvalidArgument,
    /// `Monitor.Wait`/`Pulse`/`PulseAll` by a thread that does not own the object's lock (the
    /// `SynchronizationLockException` site).
    SynchronizationLock,
    /// A checked arithmetic operation or conversion overflowed (the `OverflowException`
    /// site) -- `add.ovf` / `sub.ovf` / `mul.ovf` and `conv.ovf.*`.
    Overflow,
    /// A field token (`ldfld`/`stfld`) resolved to no field slot in the module.
    UnresolvedField(Token),
    /// Integer division or remainder by zero (`div`, `rem`, and unsigned forms).
    DivideByZero,
    /// A `call` token resolved to no method in the module.
    UnresolvedCall(Token),
    /// An `ldstr` token resolved to no string in the module's user-string heap.
    UnresolvedString(Token),
    /// A resolved method id did not exist in the module.
    NoSuchMethod(u32),
    /// The call stack grew past the interpreter's depth limit (runaway recursion).
    CallStackOverflow,
    /// An exception propagated out of the entry method with no matching handler.
    UnhandledException,
}

impl fmt::Display for Trap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Trap::StackUnderflow => f.write_str("evaluation stack underflow"),
            Trap::FellThroughEnd => f.write_str("execution fell off the end of the method"),
            Trap::Unsupported(opcode) => write!(f, "unsupported instruction {}", opcode.mnemonic()),
            Trap::TypeMismatch(opcode) => {
                write!(f, "operand types invalid for {}", opcode.mnemonic())
            }
            Trap::MalformedInstruction(opcode) => {
                write!(f, "malformed operand for {}", opcode.mnemonic())
            }
            Trap::LocalOutOfRange(slot) => write!(f, "local variable {slot} out of range"),
            Trap::ArgumentOutOfRange(slot) => write!(f, "argument {slot} out of range"),
            Trap::BranchOutOfRange(target) => write!(f, "branch target {target} out of range"),
            Trap::IndexOutOfRange(index) => write!(f, "index {index} out of range"),
            Trap::NullReference => f.write_str("dereferenced a null reference"),
            Trap::InvalidCast => f.write_str("invalid cast"),
            Trap::InvalidArgument => f.write_str("invalid argument"),
            Trap::SynchronizationLock => {
                f.write_str("monitor wait/pulse by a thread that does not own the lock")
            }
            Trap::Overflow => f.write_str("arithmetic overflow"),
            Trap::UnresolvedField(token) => {
                write!(f, "field token 0x{:08X} resolved to no field", token.0)
            }
            Trap::DivideByZero => f.write_str("integer divide by zero"),
            Trap::UnresolvedCall(token) => {
                write!(f, "call token 0x{:08X} resolved to no method", token.0)
            }
            Trap::UnresolvedString(token) => {
                write!(f, "ldstr token 0x{:08X} resolved to no string", token.0)
            }
            Trap::NoSuchMethod(id) => write!(f, "method id {id} does not exist"),
            Trap::CallStackOverflow => f.write_str("call stack overflow"),
            Trap::UnhandledException => f.write_str("unhandled exception"),
        }
    }
}
