#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The Lamella virtual execution system: a CIL interpreter.

extern crate alloc;

/// The width of a native pointer, in bytes, for the tier this interpreter is executing on.
///
/// It is a property of the active target rather than a fixed number: 8 on an x64 or arm64 host, 4
/// on ARMv6-M and on wasm32. The interpreter executes in the process it was built for, so the
/// build's own pointer width is the answer, which is why this reads `usize` rather than naming a
/// tier. `System.IntPtr.Size` and the value-type layout the loader measures structs with are the
/// same fact asked twice, and both read it here.
///
/// The ahead-of-time tier does not use this. `lamella-aot` cross-compiles -- it runs on a 64-bit
/// host and emits for a 32-bit part -- so its explicit `TargetLayout::ilp32()` is the correct
/// choice there. This answers "what am I running on", which is the same question only for a tier
/// that executes where it was built.
#[must_use]
pub const fn native_pointer_size() -> u32 {
    core::mem::size_of::<usize>() as u32
}

pub mod block;
#[cfg(feature = "exceptions")]
pub mod exception;
pub mod fs;
pub mod interp;
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
