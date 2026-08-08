#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's Python runtime.

extern crate alloc;

pub mod bigint;
pub mod builtins;
pub mod gpio;
pub mod uart;
pub mod spi;
pub mod i2c;
pub mod adc;
pub mod fileio;
pub(crate) mod board_binding;
pub mod pystdlib;
pub(crate) mod shims;
pub(crate) mod tables;
pub mod interp;
pub mod object;
pub mod reactor;
pub mod stdlib;
pub mod trap;
pub mod value;

pub use builtins::Builtin;
pub use gpio::Board;
pub use interp::{run, run_bundle, run_module, Frame};
pub use lamella_py_bytecode::{BinOp, Bundle, CmpOp, CodeObject, Const, Module, Op};
pub use object::{Arena, FinalizerSkips, Footprint, InlineCache, ObjectModel, PyType};
pub use trap::Trap;
pub use value::Value;
