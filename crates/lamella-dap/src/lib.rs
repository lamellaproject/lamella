#![forbid(unsafe_code)]

//! A Debug Adapter Protocol server over the Lamella interpreter.

pub mod adapter;
pub mod protocol;
pub mod serve;

pub use adapter::Debugger;
pub use protocol::{Event, Message, Request, Response, read_message, write_message};
pub use serve::serve;
