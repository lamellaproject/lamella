#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! CIL emission for C# 1.0 (ECMA-335 1st edition, Partition III).
//!
//! The back of the front end: it lowers the binder's typed bound tree
//! ([`lamella_binder`]) to CIL -- the stack-based instruction stream
//! ([`lamella_cil`]) that the runtime executes and the backend lowers to native.
//! Emission walks each [`BoundExpr`](lamella_binder::BoundExpr) and
//! [`BoundStmt`](lamella_binder::BoundStmt), pushing values onto the evaluation
//! stack the way the bound tree's shape dictates.
//!
//! The crate is `no_std` + `alloc`, so the same emitter runs on a host and, for the
//! on-device REPL, on a microcontroller.
//!
//! **One thing differs by target.** On a host, every compile entry point runs its work on a thread
//! with a 64 MiB stack, so deeply nested source cannot overflow the ~1 MiB a main thread starts
//! with. A target without threads -- WebAssembly, or a microcontroller -- compiles on the stack it
//! has, and how deeply source may nest there depends on that stack.

extern crate alloc;
#[cfg(all(not(test), any(unix, windows)))]
extern crate std;

pub mod awaitlower;
pub mod compile;
pub mod debug;
pub mod expr;
pub mod frame;
pub(crate) mod interop;
pub(crate) mod lambdalower;
pub mod method;
pub mod session;
pub mod tokens;

pub use compile::{
    Compilation, Diagnostic, MultiCompilation, compile_source, compile_source_with,
    compile_sources_with, compile_unit, compile_unit_with_debug, compile_unit_with_references,
};
pub use debug::{LineMap, SpanLines};
pub use expr::{EmitError, emit_expression};
pub use frame::{Frame, Slot};
/// The types [`Diagnostic`]'s own fields are made of.
///
/// Re-exported so a caller reaching [`Diagnostic`] through this crate can NAME its fields -- build
/// one, and match on `namespace` to learn whether a code is a statement about the language or about
/// this build's coverage. A type re-exported without the types it is made of can be read and never
/// constructed.
pub use lamella_syntax::diagnostic::{CodeNamespace, Severity};
pub use lamella_syntax::span::Span;
pub use method::{EmittedBody, SequencePoint, emit_method, max_stack};
pub use session::{Session, SubmissionResult};
pub use tokens::Tokens;
