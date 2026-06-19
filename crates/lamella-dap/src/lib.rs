#![forbid(unsafe_code)]

//! A Debug Adapter Protocol server over the Lamella interpreter.

pub mod adapter;
pub mod interp_backend;
pub mod protocol;
pub mod serve;

pub use adapter::Debugger;
pub use interp_backend::{InterpreterBackend, decode_address, encode_address};
pub use lamella_debug_backend::{
    DebugBackend, Disassembled, Frame, Register, Scope, Stop, Variable,
};
pub use protocol::{Event, Message, Request, Response, read_message, write_message};
pub use serve::serve;
