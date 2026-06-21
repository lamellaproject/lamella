#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's ahead-of-time backend: lowering the middle IR to target machine code.

extern crate alloc;

pub mod cil;
pub mod debugmap;
pub mod resolver;
pub mod target;

mod regalloc;

#[cfg(any(feature = "arm32", feature = "wasm"))]
mod stringgen;

#[cfg(feature = "arm32")]
pub mod arm32;

#[cfg(feature = "riscv32")]
pub mod riscv32;

#[cfg(feature = "wasm")]
pub mod wasm;

#[cfg(feature = "wasm")]
pub mod build;
