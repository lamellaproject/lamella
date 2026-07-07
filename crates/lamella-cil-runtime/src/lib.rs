#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The Lamella virtual execution system: a CIL interpreter.

extern crate alloc;

#[cfg(feature = "exceptions")]
pub mod exception;
pub mod interp;
#[cfg(feature = "bcl")]
pub mod intrinsic_registry;
pub mod intrinsics;
pub mod memory;
pub mod module;
pub mod net;
pub mod object;
pub mod tls;
pub mod trap;
pub mod value;

#[cfg(feature = "exceptions")]
pub use exception::{exception_tag, tag_is_exact, tag_is_subtype};
pub use interp::{
    CodeLocation, FrameView, NamedValue, PInvokeArg, PInvokeHostFn, Session, Status, Stop,
    StopReason, Vm, run, run_method,
};
pub use module::{
    IntrinsicFn, Method, MethodId, Module, PInvokeParam, PInvokeReturn, PInvokeTarget, TypeId,
    baked_image_checksum,
};
pub use object::{ArrayStorage, Heap, Object, ObjectRef, PrimKind};
pub use trap::Trap;
pub use value::Value;
