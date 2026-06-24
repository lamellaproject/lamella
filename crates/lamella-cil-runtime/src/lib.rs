#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The Lamella virtual execution system: a CIL interpreter.

extern crate alloc;

#[cfg(feature = "exceptions")]
pub mod exception;
pub mod interp;
#[cfg(feature = "bcl")]
pub mod intrinsics;
pub mod module;
pub mod object;
pub mod trap;
pub mod value;

#[cfg(feature = "exceptions")]
pub use exception::{exception_tag, tag_is_exact, tag_is_subtype};
pub use interp::{
    CodeLocation, FrameView, NamedValue, Session, Status, Stop, StopReason, Vm, run, run_method,
};
pub use module::{IntrinsicFn, Method, MethodId, Module, TypeId};
pub use object::{Heap, Object, ObjectRef};
pub use trap::Trap;
pub use value::Value;
