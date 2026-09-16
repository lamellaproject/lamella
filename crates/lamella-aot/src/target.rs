//! The seam between the middle IR and a target: the lowering trait every
//! per-target code generator implements.

use alloc::vec::Vec;

use lamella_ir::{BlockId, Function};

/// Whether control leaving block `from` for block `to` reaches it without a jump.
///
/// A lowering that emits a function's blocks in index order, binding each block's label before
/// anything else in it, starts the block with the next index at the very next instruction. A jump
/// or branch edge to that block can be omitted: execution arrives there anyway.
#[must_use]
pub(crate) fn falls_through(from: usize, to: BlockId) -> bool {
    to.index() == from + 1
}

/// A target code generator: lowers a verified MIR [`Function`] to machine code
/// for one target, or reports why it could not.
///
/// Implementors should treat the input as untrusted and never panic: an
/// unsupported or malformed function is an [`TargetLowering::Error`], not a crash.
pub trait TargetLowering {
    /// Why a function could not be lowered for this target.
    type Error;

    /// Lowers `func` to this target's machine code.
    fn lower(&self, func: &Function) -> Result<Vec<u8>, Self::Error>;
}
