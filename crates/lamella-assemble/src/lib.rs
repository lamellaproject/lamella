#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! CIL emission for C# 1.0 (ECMA-335 1st edition, Partition III).

extern crate alloc;

pub mod compile;
pub mod expr;
pub mod frame;
pub mod method;

pub use compile::{Compilation, compile_unit};
pub use expr::{EmitError, emit_expression};
pub use frame::{Frame, Slot};
pub use method::{emit_method, max_stack};
