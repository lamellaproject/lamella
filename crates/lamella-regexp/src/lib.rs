//! **A backtracking regular-expression engine for constrained devices.** A pattern is parsed by a
//! per-language front end into a shared syntax tree, compiled to a flat program, and executed by
//! one matcher that no language owns.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub(crate) use alloc::boxed::Box;
pub(crate) use alloc::string::String;
pub(crate) use alloc::vec::Vec;
pub(crate) use alloc::{format, vec};

pub mod ast;
pub mod compile;
pub mod haystack;
pub mod matcher;
pub mod program;

/// The ECMA-262 pattern grammar, its flag set, and its early errors.
#[cfg(feature = "js")]
pub mod js;

pub use ast::{Assertion, ClassEntry, Greed, Node};
pub use compile::{compile, Options};
pub use haystack::{CodePointInput, CodeUnitInput, Haystack};
pub use matcher::{run, Fuel, Match, Outcome};
pub use program::{Direction, Instruction, Program};
