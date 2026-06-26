//! Interpreter traps -- the ways executing the bytecode can fail.

/// A reason an interpreter run aborted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    /// An operation popped from an empty evaluation stack -- a malformed instruction
    /// stream (a verifier rules this out; surfaced defensively).
    StackUnderflow,
    /// A `LoadFast` read a local before any value was bound to it -- Python's
    /// `UnboundLocalError` (a subclass of `NameError`).
    UnboundLocal,
    /// An operation or function was applied to an object of inappropriate type --
    /// Python's `TypeError` (e.g. arithmetic on a value that is not a number). NOT used
    /// for a missing attribute: a value that supports attribute references but lacks the
    /// name raises `AttributeError`, not this.
    TypeError,
    /// An attribute reference failed -- Python's `AttributeError`.
    AttributeError,
    /// A sequence index was out of range -- Python's `IndexError` (here: a `str`/`list`/
    /// `tuple` index outside `[-len, len)`).
    IndexError,
    /// A mapping key was not found -- Python's `KeyError` (here: a missing `dict` key).
    KeyError,
    /// An argument of the right type had an inappropriate value -- Python's `ValueError`
    /// (here: a negative shift count, `x << -1` / `x >> -1`).
    ValueError,
    /// The second operand of `//` or `%` was zero -- Python's `ZeroDivisionError`.
    ZeroDivisionError,
    /// A name was not found -- Python's `NameError` (here: a `LoadGlobal` of a name that
    /// is neither an intra-module function nor a built-in).
    NameError,
    /// Call nesting exceeded the interpreter's depth limit -- Python's `RecursionError`.
    RecursionError,
    /// An opcode or operand the bytecode defines that is outside the interpreter's
    /// implemented set (e.g. a string constant outside the supported forms). Distinct
    /// from malformed input.
    Unsupported,
    /// An integer result overflowed the fixnum range. Python's `int` has an unlimited
    /// range (data model, Numbers); the interpreter traps the overflow rather than
    /// wrapping silently.
    Overflow,
    /// A heap allocation failed after collection -- out of memory.
    OutOfMemory,
    /// The bytecode was malformed: an out-of-range pool index, jump target, local slot,
    /// inline-cache slot, or argument count. A well-formed front end never emits this.
    Malformed,
    /// A Python exception is in flight (a `raise`, a `Reraise`, or a propagated exception):
    /// the exception object lives in the model's pending slot, and the interpreter's
    /// exception-table search routes it to a handler. It only surfaces as an "uncaught
    /// exception" when it escapes the top frame; it is never a bytecode/VM fault.
    Raised,
}
