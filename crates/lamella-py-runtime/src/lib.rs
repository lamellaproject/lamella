#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's Python runtime.

extern crate alloc;

/// The universal tagged-value representation.
pub mod value {
    //! Placeholder. A machine word that is either a small immediate (fixnum,
    //! `None`/`True`/`False`) or a pointer to a heap object, distinguished by tag
    //! bits and scannable by the precise GC.
}

/// The bytecode interpreter dispatch loop for the first-light subset.
pub mod interp {
    //! Placeholder for the interpreter.
}

/// The dynamic object model and intrinsics (the abstract object protocol).
pub mod object {
    //! Placeholder. First light needs only `py_getattr` plus an inline-cache slot.
}
