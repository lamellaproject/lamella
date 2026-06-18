#![forbid(unsafe_code)]

//! A Debug Adapter Protocol server over the Lamella interpreter.

pub mod protocol;

pub use protocol::{Event, Message, Request, Response, read_message, write_message};
