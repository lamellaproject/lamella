#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The Lamella virtual execution system: a CIL interpreter.

extern crate alloc;

pub mod block;
#[cfg(feature = "exceptions")]
pub mod exception;
pub mod fs;
pub mod interp;
#[cfg(feature = "bcl")]
pub mod intrinsic_registry;
pub mod intrinsics;
pub mod memory;
pub mod module;
pub mod mount;
pub use lamella_net_core as net;
pub mod object;
pub use lamella_reactor as reactor;
pub mod serial;
pub mod aead;
pub mod tls;
pub mod trap;
pub mod value;

#[cfg(feature = "exceptions")]
pub use exception::{exception_tag, tag_is_exact, tag_is_subtype};
pub use interp::{
    CodeLocation, FrameView, NamedValue, PInvokeArg, PInvokeHostFn, PendingOp, PinEventSource, Ran,
    Session, Status, Stop, StopReason, Vm, boot_baked, run, run_interruptible, run_method,
    run_serviced, set_wall_clock, take_pending_op, wall_clock_source,
};
/// The pin-change event a board's interrupt handler queues, re-exported from the crate that owns
/// the queue's RULE so an embedder installing a [`PinEventSource`] can name it without taking a
/// second dependency for one two-field struct.
pub use lamella_pin_events::PinEvent;
pub use module::{
    CastElem, CastPrim, IntrinsicFn, Method, MethodId, Module, PInvokeParam, PInvokeReturn,
    PInvokeTarget, TypeId, asm_key, baked_image_checksum,
};
#[cfg(feature = "code-in-place")]
pub use module::verified_image_checksum;
pub use mount::{DriveType, MountTable, StorageProvider};
pub use object::{ArrayStorage, Heap, Object, ObjectRef, PrimKind, UnencodableChar};
pub use trap::Trap;
pub use value::{Location, Value};
