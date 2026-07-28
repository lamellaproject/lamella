#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's typed middle IR, lowered from CIL toward native code.

extern crate alloc;

pub mod function;
pub mod inst;
pub mod types;
pub mod verify;

pub use function::{BasicBlock, BlockId, Function, Terminator, ValueId};
pub use inst::{BinOp, CmpOp, ConvKind, Inst, PyBinOp, PyCmpOp, PyOp, StaticOwner};
pub use types::{
    ARRAY_HANDLE_TABLE_OFFSET, MirType, SYNTHETIC_ARRAY_HANDLE_TABLE, TypeHandle, array_handle,
    synthetic_array_handle,
};
pub use verify::{VerifyError, verify};
