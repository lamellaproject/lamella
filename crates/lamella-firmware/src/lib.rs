//! Compose the AOT compiler and the linker into a flashable bare-metal image: a CIL assembly ->
//! `build_object` (the relocating object pipeline, so float / GC / P-Invoke / CallNative are all
//! REACHABLE, not just the flat fast path) -> link at the target's flash base -> prepend the
//! `[initial SP][reset]` vector header. This is the layer that runs the linker for a device target,
//! keeping `lamella-aot`'s `build_object` itself linker-free.

use lamella_aot::build::{build_object, BuildError};
use lamella_elf::{read_archive, read_object, ElfError};
use lamella_link::{LinkError, link_with_archives};

/// A supported Cortex-M flash target: its initial stack pointer (the top of SRAM) and flash base.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// BBC micro:bit v1 -- nRF51822, 16 KiB SRAM at 0x2000_0000, code flashed from address 0.
    Microbit,
}

impl Target {
    fn initial_sp(self) -> u32 {
        match self {
            Target::Microbit => 0x2000_4000,
        }
    }

    fn flash_base(self) -> u32 {
        match self {
            Target::Microbit => 0,
        }
    }
}

/// The vector header laid before the code: `[initial SP][reset vector]`, two words.
const VECTOR_BYTES: u32 = 8;

/// The entry symbol `build_object` emits for function 0 -- the startup that runs `.cctor`s (and the
/// board init hook) before `Main`.
const ENTRY_SYMBOL: &str = "f0";

/// Anything that can stop a flashable image from being produced.
#[derive(Debug)]
pub enum FirmwareError {
    /// The AOT could not lower the assembly to a relocatable object.
    Build(BuildError),
    /// The emitted object was malformed.
    Object(ElfError),
    /// Linking failed (an unresolved symbol, an unsupported relocation, a too-large image, ...).
    Link(LinkError),
}

impl From<BuildError> for FirmwareError {
    fn from(error: BuildError) -> Self {
        FirmwareError::Build(error)
    }
}

impl From<ElfError> for FirmwareError {
    fn from(error: ElfError) -> Self {
        FirmwareError::Object(error)
    }
}

impl From<LinkError> for FirmwareError {
    fn from(error: LinkError) -> Self {
        FirmwareError::Link(error)
    }
}

impl core::fmt::Display for FirmwareError {
    /// Defers to each cause's own rendering. Without this a caller's only option is the derived
    /// `Debug`, which for a link failure prints a mangled symbol and no cause -- the whole reason
    /// the linker gained a `Display`.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FirmwareError::Build(error) => write!(f, "the assembly did not lower: {error:?}"),
            FirmwareError::Object(error) => write!(f, "the emitted object was malformed: {error:?}"),
            FirmwareError::Link(error) => write!(f, "{error}"),
        }
    }
}

/// Compile a CIL assembly to a flashable bare-metal image for `target`, resolving runtime seams
/// against `archives` -- the runtime-support archive built for this target's ISA, whose members are
/// pulled on demand, so an image carries only the seams its program actually reaches.
///
/// The entry method IS the program and should loop forever -- a reset handler never returns. The
/// result is written directly to the chip's flash from address 0: `[initial SP][reset -> entry][code]`.
///
/// Supply the archive whenever the program may allocate or compute with floats. Passing none is
/// legitimate for a program that calls no seam (a pure MMIO loop), and is what
/// [`build_cortex_m_image`] does.
pub fn build_cortex_m_image_with_archives(
    cil: &[u8],
    target: Target,
    archives: &[&[u8]],
) -> Result<Vec<u8>, FirmwareError> {
    let object = build_object(cil)?;
    let code_base = target.flash_base() + VECTOR_BYTES;
    let parsed = archives
        .iter()
        .map(|bytes| read_archive(bytes))
        .collect::<Result<Vec<_>, _>>()?;
    let image = link_with_archives(
        &[read_object(&object)?],
        &parsed,
        ENTRY_SYMBOL,
        Some(code_base),
    )?;
    let reset = (code_base + image.entry_offset) | 1;
    let mut out = Vec::with_capacity(VECTOR_BYTES as usize + image.text.len());
    out.extend_from_slice(&target.initial_sp().to_le_bytes());
    out.extend_from_slice(&reset.to_le_bytes());
    out.extend_from_slice(&image.text);
    Ok(out)
}

/// [`build_cortex_m_image_with_archives`] with no archive, for a program that calls no runtime seam.
///
/// A program that allocates or uses floats needs the archive and fails here at the link; the error
/// says which symbol and what defines it.
pub fn build_cortex_m_image(cil: &[u8], target: Target) -> Result<Vec<u8>, FirmwareError> {
    build_cortex_m_image_with_archives(cil, target, &[])
}
