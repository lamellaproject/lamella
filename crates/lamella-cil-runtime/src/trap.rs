//! Traps: controlled execution failures reported instead of panicking.

use crate::object::UnencodableChar;
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
    /// A `stelem.ref` whose value is not assignable to the array's ELEMENT type -- a
    /// covariant store (I.8.7.1, the `ArrayTypeMismatchException` site).
    ArrayTypeMismatch,
    /// An argument was invalid (the `ArgumentException` site) -- e.g. `Enum.Parse` of a
    /// name that names no constant of the enum.
    InvalidArgument,
    /// `Monitor.Wait`/`Pulse`/`PulseAll` by a thread that does not own the object's lock (the
    /// `SynchronizationLockException` site).
    SynchronizationLock,
    /// A checked arithmetic operation or conversion overflowed (the `OverflowException`
    /// site) -- `add.ovf` / `sub.ovf` / `mul.ovf` and `conv.ovf.*`.
    Overflow,
    /// A field token (`ldfld`/`stfld`) resolved to no field slot in the module -- the field is not
    /// REGISTERED. A loading or binding question: the token names a field this module does not know.
    UnresolvedField(Token),
    /// The field resolved fine, and the STORAGE does not have that slot -- an instance, a struct
    /// value, or the static area, depending on the instruction.
    ///
    /// A different defect from [`UnresolvedField`](Self::UnresolvedField) with a different remedy,
    /// which is why it is a different trap: the field IS registered, so nothing about loading or
    /// binding is wrong -- the storage was allocated with a layout that does not match the type
    /// whose field is being read. **Look at how it was ALLOCATED, not at the field.**
    FieldSlotMissing {
        /// The field token the instruction named.
        field: Token,
        /// The slot it resolved to, which the storage does not have.
        slot: u32,
    },
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
    /// The managed heap reached its configured budget and a collection could not free enough
    /// (the `OutOfMemoryException` site) -- raised before the underlying allocator would fail
    /// hard, so it is catchable like the other runtime faults.
    OutOfMemory,
    /// An exception propagated out of the entry method with no matching handler.
    UnhandledException,
    /// A BAKED module's entry point was run before its static constructors. A baked image
    /// carries the ordered `.cctor` list but not the lazy-trigger map a loaded module uses,
    /// so no static would ever initialize on first access and every `static readonly` would
    /// read its zero value. Boot the module with [`crate::boot_baked`] first.
    StaticCtorsNotRun,
    /// A `System.String` could not be constructed because the build's string storage cannot
    /// hold one of its code units -- a lone surrogate on the well-formed UTF-8 tier (the
    /// `System.Text.EncoderFallbackException` site). Carries the offending unit and its
    /// UTF-16-unit index, which is what the message names.
    EncoderFallback {
        /// The code unit that could not be encoded.
        char_unknown: u16,
        /// Its position in the input, in UTF-16 code units.
        index: u32,
    },
}

impl From<UnencodableChar> for Trap {
    /// Lifts the heap encoder's refusal to the trap the interpreter raises, so a string
    /// allocation propagates with a plain `?` at every construction site rather than each one
    /// deciding what an unencodable unit means.
    fn from(refusal: UnencodableChar) -> Self {
        Trap::EncoderFallback {
            char_unknown: refusal.char_unknown,
            index: refusal.index,
        }
    }
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
            Trap::ArrayTypeMismatch => f.write_str("array element type mismatch"),
            Trap::InvalidArgument => f.write_str("invalid argument"),
            Trap::SynchronizationLock => {
                f.write_str("monitor wait/pulse by a thread that does not own the lock")
            }
            Trap::Overflow => f.write_str("arithmetic overflow"),
            Trap::UnresolvedField(token) => {
                write!(f, "field token 0x{:08X} resolved to no field", token.0)
            }
            Trap::FieldSlotMissing { field, slot } => write!(
                f,
                "field token 0x{:08X} resolved to slot {slot}, which this instance does not have \
                 (the field is fine; the object was allocated with a different layout)",
                field.0
            ),
            Trap::DivideByZero => f.write_str("integer divide by zero"),
            Trap::UnresolvedCall(token) => {
                write!(f, "call token 0x{:08X} resolved to no method", token.0)
            }
            Trap::UnresolvedString(token) => {
                write!(f, "ldstr token 0x{:08X} resolved to no string", token.0)
            }
            Trap::NoSuchMethod(id) => write!(f, "method id {id} does not exist"),
            Trap::CallStackOverflow => f.write_str("call stack overflow"),
            Trap::OutOfMemory => f.write_str("out of memory"),
            Trap::UnhandledException => f.write_str("unhandled exception"),
            Trap::StaticCtorsNotRun => {
                f.write_str("baked module run before its static constructors")
            }
            Trap::EncoderFallback {
                char_unknown,
                index,
            } => write!(
                f,
                "code unit U+{char_unknown:04X} at index {index} cannot be encoded by this build's string storage"
            ),
        }
    }
}
