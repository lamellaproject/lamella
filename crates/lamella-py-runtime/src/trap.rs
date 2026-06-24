//! Interpreter traps

/// A reason an interpreter run aborted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trap {
    /// An operation popped from an empty evaluation stack -- a malformed instruction
    /// stream (the verifier will rule this out; surfaced defensively for now).
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
    /// An argument of the right type had an inappropriate value -- Python's `ValueError`
    /// (here: a negative shift count, `x << -1` / `x >> -1`).
    ValueError,
    /// The second operand of `//` or `%` was zero -- Python's `ZeroDivisionError`.
    ZeroDivisionError,
    /// A name was not found -- Python's `NameError` (here: a `LoadGlobal` of a name that
    /// is not an intra-module function; first light resolves no builtins or imports).
    NameError,
    /// Call nesting exceeded the interpreter's depth limit -- Python's `RecursionError`.
    RecursionError,
    /// An opcode or operand the bytecode defines but the first-light interpreter does not
    /// implement yet -- currently a string constant (there is no string object yet). A
    /// clean "not yet", distinct from malformed input.
    Unsupported,
    /// An integer result overflowed the fixnum range. Python's `int` has an unlimited
    /// range (data model, Numbers), so full Python promotes to a bignum; the first-light
    /// subset has no bignums yet, so this is a trap rather than a silent wrap.
    Overflow,
    /// A heap allocation failed after collection -- out of memory.
    OutOfMemory,
    /// The bytecode was malformed: an out-of-range pool index, jump target, local slot,
    /// inline-cache slot, or argument count. A well-formed front end never emits this.
    Malformed,
}
