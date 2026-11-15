//! Traps: controlled execution failures reported instead of panicking.

use crate::object::UnencodableChar;
use alloc::borrow::Cow;
use alloc::string::String;
use core::fmt;
use lamella_cil::Opcode;
use lamella_token::Token;

/// What the running tier can say about an exception that escaped without a handler.
///
/// Each `Option` means "this tier may not have this", never "this was empty", and the distinction
/// is the point of the type: an absent message is a tier that cannot tell you, while an empty one
/// is an exception constructed without a message. A caller that needs to tell them apart can.
///
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnhandledException {
    /// The exception type's TAG: FNV-1a over `namespace.name` with the high bit set.
    ///
    /// The one fact BOTH tiers have, verified against `lamella_metadata::exception_tag_for_name`
    /// rather than assumed: FNV-1a is a streaming fold, so folding `"Ns"`, `"."`, `"Name"` is
    /// byte-identical to folding `"Ns.Name"`, and both sides fold the BARE name for a type in the
    /// global namespace. Not invertible -- it names the type without naming it.
    pub tag: u32,
    /// The exception type's full name, when this tier can name it.
    ///
    pub type_name: Option<String>,
    /// The exception's message, when this tier has an object to read one from.
    ///
    pub message: Option<String>,
    /// The type whose method was executing when the runtime raised this exception. `None` for an
    /// exception the program threw itself, and on a tier that does not record it.
    pub raised_in: Option<String>,
    /// The exception this one wraps, as its `InnerException` holds it: what a static constructor
    /// threw, inside the `TypeInitializationException` raised at its type's first access.
    pub inner: Option<alloc::boxed::Box<UnhandledException>>,
}

impl fmt::Display for UnhandledException {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("unhandled exception")?;
        f.write_str(if self.type_name.is_none() && self.message.is_none() { " " } else { ": " })?;
        self.write_described(f)
    }
}

impl UnhandledException {
    /// This exception's type and message, where it was raised, and then each exception it wraps,
    /// after .NET's ` ---> `.
    fn write_described(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.type_name, &self.message) {
            (Some(name), Some(message)) => write!(f, "{name}: {message}")?,
            (Some(name), None) => f.write_str(name)?,
            (None, Some(message)) => f.write_str(message)?,
            (None, None) => write!(f, "(type tag {:#010x})", self.tag)?,
        }
        if let Some(method) = &self.raised_in {
            write!(f, " (raised in {method})")?;
        }
        match &self.inner {
            Some(inner) => {
                f.write_str(" ---> ")?;
                inner.write_described(f)
            }
            None => Ok(()),
        }
    }
}

/// The exception a [`Trap::Refused`] raises.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefusedWith {
    /// `System.ArgumentException`.
    Argument,
    /// `System.ArrayTypeMismatchException`.
    ArrayTypeMismatch,
    /// `System.InvalidCastException`.
    InvalidCast,
    /// `System.MissingMethodException`.
    MissingMethod,
    /// `System.MissingFieldException`.
    MissingField,
}

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
    /// A library method refused its arguments with .NET's exception and message for the case.
    ///
    /// For the cases where .NET's message differs from the one the same exception carries when
    /// an instruction raises it: `Array.Copy` raises ArrayTypeMismatchException "Source array type
    /// cannot be assigned to destination array type.", where a `stelem.ref` that stores a wrong
    /// element raises the same type with a different message. A message that names its argument
    /// is built, as `Activator.CreateInstance`'s names the type it could not construct.
    Refused(RefusedWith, Cow<'static, str>),
    /// `Monitor.Wait`/`Pulse`/`PulseAll` by a thread that does not own the object's lock (the
    /// `SynchronizationLockException` site).
    SynchronizationLock,
    /// A type whose `.cctor` already THREW was accessed again (ECMA-335 II.10.5.3.1 guarantee 3:
    /// an initializer that threw has been executed and did not complete, so the type stays
    /// inaccessible and every later access fails the way the first one did).
    ///
    /// It carries the [`TypeId`](crate::module::TypeId) rather than the exception because the
    /// exception is VM state: .NET caches the `TypeInitializationException` it raised and rethrows
    /// that same instance, so the fault-to-exception conversion looks the recorded one up instead
    /// of minting a second.
    ///
    TypeInitializationFailed(crate::module::TypeId),
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
    ///
    /// Reporting the two alike sends the reader to the wrong subsystem: a `new ArrayList()` that
    /// allocates a fieldless native instance while the managed body reads `size` is a defect in the
    /// ALLOCATOR, and "resolved to no field" -- said of a field that resolves perfectly well --
    /// points at the loader instead.
    FieldSlotMissing {
        /// The field token the instruction named.
        field: Token,
        /// The slot it resolved to, which the storage does not have.
        slot: u32,
    },
    /// Pointer arithmetic on the address of a field reached a byte this interpreter cannot
    /// address, or started from a field address it cannot walk at all. The message says which.
    ///
    /// An object's fields are typed slots, so a pointer walked from one field reaches another only
    /// where that field starts. Not catchable: like [`Unsupported`](Self::Unsupported), it names
    /// something the interpreter does not do, not an exception .NET would raise.
    FieldPointerWalk(Cow<'static, str>),
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
    /// An exception propagated out of the entry method with no matching handler, and what the
    /// running tier could say about it.
    UnhandledException(UnhandledException),
    /// A BAKED module's entry point was run before its static constructors. An image baked
    /// before the trigger tables carries the ordered `.cctor` list but not the lazy-trigger map
    /// a loaded module uses, so no static would ever initialize on first access and every
    /// `static readonly` would read its zero value. Boot the module with [`crate::boot_baked`]
    /// first.
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
            Trap::Refused(_, message) => f.write_str(message),
            Trap::SynchronizationLock => {
                f.write_str("monitor wait/pulse by a thread that does not own the lock")
            }
            Trap::Overflow => f.write_str("arithmetic overflow"),
            Trap::TypeInitializationFailed(type_id) => write!(
                f,
                "type {type_id}'s initializer already threw, so the type stays uninitialized"
            ),
            Trap::UnresolvedField(token) => {
                write!(f, "field token 0x{:08X} resolved to no field", token.0)
            }
            Trap::FieldSlotMissing { field, slot } => write!(
                f,
                "field token 0x{:08X} resolved to slot {slot}, which this instance does not have \
                 (the field is fine; the object was allocated with a different layout)",
                field.0
            ),
            Trap::FieldPointerWalk(message) => f.write_str(message),
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
            Trap::UnhandledException(detail) => detail.fmt(f),
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

/// The codes a run reports when it ends any way other than its entry point returning, and what each
/// one means: the one table the runtimes, the boards, the command line and the debugger all read.
///
/// A program that returns from its entry point reports what it returned, or 0 for an entry point
/// that returns nothing. A run that ends any other way reports one of these, so a host can tell
/// from the code alone what ended it: an exception the program did not catch, the runtime stopping
/// the program itself, or the processor faulting.
pub use lamella_exit_codes as exit_code;

impl Trap {
    /// The code a run that ended in this trap reports: [`exit_code::UNHANDLED_EXCEPTION`] for an
    /// exception that propagated out of the entry method, and [`exit_code::FATAL_STOP`] for every
    /// other trap, each of which is the interpreter stopping the program itself.
    ///
    /// No trap is an [`exit_code::PROCESSOR_FAULT`]: the interpreter refuses an access it cannot make
    /// rather than letting the processor attempt it. On a build without exception handling, a fault
    /// such as a null dereference ends the run as a trap of its own, and so reports
    /// [`exit_code::FATAL_STOP`]: that build stops a program where another raises an exception.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Trap::UnhandledException(_) => exit_code::UNHANDLED_EXCEPTION,
            _ => exit_code::FATAL_STOP,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Trap, UnhandledException, exit_code};
    use lamella_cil::Opcode;

    /// An exception nothing caught, as a run that ended in one reports it.
    fn unhandled(type_name: &str) -> Trap {
        Trap::UnhandledException(UnhandledException {
            tag: 0,
            type_name: Some(type_name.into()),
            message: None,
            raised_in: None,
            inner: None,
        })
    }

    #[test]
    fn an_exception_nothing_caught_reports_134_as_on_dotnet() {
        assert_eq!(unhandled("System.InvalidOperationException").exit_code(), 134);
        assert_eq!(exit_code::UNHANDLED_EXCEPTION, 134);
    }

    #[test]
    fn a_trap_of_the_interpreters_own_reports_the_runtimes_fatal_stop() {
        for trap in [
            Trap::TypeMismatch(Opcode::StindI4),
            Trap::Unsupported(Opcode::Arglist),
            Trap::UnresolvedCall(lamella_token::Token(0x0A00_0001)),
            Trap::CallStackOverflow,
            Trap::StaticCtorsNotRun,
        ] {
            assert_eq!(trap.exit_code(), 1, "{trap:?}");
        }
        assert_eq!(exit_code::FATAL_STOP, 1);
    }

    #[test]
    fn a_processor_fault_has_a_code_of_its_own() {
        assert_eq!(exit_code::PROCESSOR_FAULT, 2);
        assert_ne!(exit_code::PROCESSOR_FAULT, exit_code::FATAL_STOP);
        assert_ne!(exit_code::PROCESSOR_FAULT, exit_code::UNHANDLED_EXCEPTION);
    }

    #[test]
    fn no_trap_reports_the_code_a_kernel_gives_when_it_refuses_to_start_a_program() {
        for trap in [
            unhandled("System.InvalidOperationException"),
            Trap::TypeMismatch(Opcode::StindI4),
            Trap::Unsupported(Opcode::Arglist),
            Trap::UnresolvedCall(lamella_token::Token(0x0A00_0001)),
            Trap::CallStackOverflow,
            Trap::OutOfMemory,
            Trap::StaticCtorsNotRun,
        ] {
            assert_ne!(trap.exit_code(), exit_code::KERNEL_REFUSED_ENTRY, "{trap:?}");
        }
        assert_eq!(exit_code::KERNEL_REFUSED_ENTRY, 4);
    }
}
