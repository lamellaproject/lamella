//! Emit the firmware file formats a bootloader accepts by drag-and-drop.

#![forbid(unsafe_code)]

pub mod hex;
pub mod uf2;

use core::fmt;

/// Why an image could not be turned into a firmware file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitError {
    /// The image had no bytes in it.
    ///
    /// Almost always this means the build upstream produced nothing loadable rather than that a
    /// caller passed an empty slice deliberately. Emitting the file anyway would produce a valid
    /// but empty artifact -- see the crate documentation for why that is worth refusing.
    EmptyImage,
    /// The image does not fit in the 32-bit address space at the requested base address.
    AddressOverflow {
        /// The base address the image was to be placed at.
        base: u32,
        /// The length of the image, in bytes.
        len: usize,
    },
}

impl fmt::Display for EmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyImage => f.write_str(
                "the image is empty, so the firmware file would flash nothing; \
                 check that the build produced a binary with loadable sections",
            ),
            Self::AddressOverflow { base, len } => write!(
                f,
                "an image of {len} bytes based at {base:#010x} runs past the end of the \
                 32-bit address space"
            ),
        }
    }
}

impl std::error::Error for EmitError {}
