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
    /// A float was needed and this build has none -- the `float` capability is compiled out
    /// (the no-float tier, for parts too small to carry soft float at all).
    ///
    /// **Its own variant rather than [`Trap::Unsupported`] or [`Trap::TypeError`], and the
    /// reason is what the other two would tell the person reading the failure.** `TypeError`
    /// says the OPERANDS are wrong, which is false and sends its author to fix working code;
    /// `Unsupported` renders as the bare word "Unsupported" on a device serve, which names
    /// nothing. This one exists so the refusal can say that FLOAT IS ABSENT FROM THIS BUILD --
    /// a capability answer, which is what it is.
    ///
    /// Reached through `ObjectModel::new_float`, the one choke point every float-producing path
    /// passes -- so paths nobody enumerated refuse too, rather than silently yielding a wrong
    /// number. In Python that matters more than it looks: `1 / 3` is a float with no float
    /// literal in it, and `x ** n` is a float exactly when `n` is negative.
    FloatUnavailable,
    /// An integer result overflowed the fixnum range. Python's `int` has an unlimited
    /// range (data model, Numbers); the interpreter traps the overflow rather than
    /// wrapping silently.
    Overflow,
    /// A heap allocation failed after collection -- out of memory.
    OutOfMemory,
    /// The bytecode was malformed: an out-of-range pool index, jump target, local slot,
    /// inline-cache slot, or argument count. A well-formed front end never emits this.
    Malformed,
    /// Every live thread is blocked waiting for another one, so no thread can ever run again --
    /// a join cycle.
    ///
    /// **It is a trap and not a Python exception because there is no thread left to raise it in.**
    /// An exception is delivered to a running frame and searched for a handler; here every frame
    /// belongs to a thread that is by definition not running, so there is nowhere to deliver it.
    ///
    /// **And the alternative -- ending the program quietly -- is the one outcome this design will
    /// not ship.** A deadlocked program that exits 0 with no message reads to its author as "my
    /// code did nothing", which sends them to look at the code that ran rather than at the wait
    /// that never ended. CPython hangs forever instead, which at least never claims success; a
    /// green-thread scheduler can see the whole cycle and say so. The ahead-of-time tier reached
    /// the same answer first and prints `DEADLOCK` on the console before halting
    /// (`runtime-support`'s `sched_deadlock_trap`), so the two tiers report the same event.
    ///
    /// The scheduler writes WHICH thread is waiting on WHICH to `sys.stderr` before returning this,
    /// because the bare word names the failure without locating it.
    Deadlock,
    /// A Python exception is in flight (a `raise`, a `Reraise`, or a propagated exception):
    /// the exception object lives in the model's pending slot, and the interpreter's
    /// exception-table search routes it to a handler. It only surfaces as an "uncaught
    /// exception" when it escapes the top frame; it is never a bytecode/VM fault.
    Raised,
}
