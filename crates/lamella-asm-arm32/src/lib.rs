#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Machine-code encoder for the Cortex-M (M-profile Thumb) instruction set.

extern crate alloc;

pub mod cond;
pub mod encoder;
pub mod register;
pub mod target;

pub use cond::Cond;
pub use encoder::{AssembleError, Assembled, Encoder, Label, Reloc, RelocKind};
pub use register::Reg;
pub use target::Profile;
