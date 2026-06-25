#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's Python runtime.

extern crate alloc;

pub mod builtins;
pub mod interp;
pub mod object;
pub mod trap;
pub mod value;

pub use builtins::Builtin;
pub use interp::{run, Frame};
pub use lamella_py_bytecode::{BinOp, CmpOp, CodeObject, Const, Op};
pub use object::{InlineCache, ObjectModel, PyType};
pub use trap::Trap;
pub use value::Value;
