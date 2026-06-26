#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! CIL emission for C# 1.0 (ECMA-335 1st edition, Partition III).

extern crate alloc;

pub mod compile;
pub mod debug;
pub mod expr;
pub mod frame;
pub mod method;
pub mod session;
pub mod tokens;

pub use compile::{
    Compilation, Diagnostic, compile_source, compile_source_with, compile_unit,
    compile_unit_with_debug, compile_unit_with_references,
};
pub use debug::{LineMap, SpanLines};
pub use expr::{EmitError, emit_expression};
pub use frame::{Frame, Slot};
pub use method::{EmittedBody, SequencePoint, emit_method, max_stack};
pub use session::{Session, SubmissionResult};
pub use tokens::Tokens;
