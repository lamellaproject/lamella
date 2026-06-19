#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The Lamella virtual execution system: a CIL interpreter.

extern crate alloc;

pub mod interp;
#[cfg(feature = "bcl")]
pub mod intrinsics;
pub mod module;
pub mod object;
pub mod trap;
pub mod value;

pub use interp::{FrameView, Session, Status, Vm, run, run_method};
pub use module::{IntrinsicFn, Method, MethodId, Module, TypeId};
pub use object::{Heap, Object, ObjectRef};
pub use trap::Trap;
pub use value::Value;
