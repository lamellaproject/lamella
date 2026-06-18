#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's ahead-of-time backend: lowering the middle IR to target machine code.

extern crate alloc;

pub mod cil;
pub mod target;

mod regalloc;

#[cfg(feature = "arm32")]
pub mod arm32;
