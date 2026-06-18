//! The seam between the middle IR and a target: the lowering trait every

use alloc::vec::Vec;

use lamella_ir::Function;

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
