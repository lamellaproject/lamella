//! Emit the firmware file formats a bootloader accepts by drag-and-drop.
//!
//! Both formats exist for the same reason: the board exposes a USB mass-storage volume, and
//! copying one file onto it flashes the device. That is the entire deployment step -- no probe, no
//! vendor tool, no driver install. DAPLink boards such as the micro:bit take [Intel HEX](hex);
//! an RP2040 or RP2350 held in BOOTSEL takes [UF2](uf2).
//!
//! ```no_run
//! # fn main() -> Result<(), lamella_flash_format::EmitError> {
//! let image = std::fs::read("firmware.bin").unwrap();
//! std::fs::write("firmware.hex", lamella_flash_format::hex::to_intel_hex(&image, 0)?).unwrap();
//! # Ok(())
//! # }
//! ```
//!
//! # Why the emitters return errors
//!
//! An emitter refuses an empty image. A link that produces no loadable sections does not fail, and
//! the Intel HEX for it is a lone end-of-file record: a 13-byte file that copies onto the volume
//! without complaint and leaves the board running whatever was flashed before. Each emitter
//! therefore checks that its output contains data rather than trusting the build that produced it.

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
