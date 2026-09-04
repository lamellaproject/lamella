//! The on-device firmware store behind the DEVICE / FIRMWARE block (`0x90`-`0x9F`): the flash
//! primitives a family supplies, and the boot locator record that says which installed image runs
//! next.

#![no_std]

/// What a family supplies so its flash can hold a locator.
///
/// **THE UNITS ARE THE FAMILY'S AND THE REGION IS A LINK-TIME FACT.** A part decides how many bytes
/// one program commits and how many one erase clears, and those come from its reference manual.
/// Where the locator sits comes from the LINKER SCRIPT -- read as a symbol at boot, the way
/// `set_board_identity` and the LIVE window already take theirs -- because the linker script is
/// what carves this firmware's flash, and anything else stating the same carve is a second source
/// for one fact.
///
/// A deploy window makes the same argument about its own boundary symbol: the firmware reads that
/// symbol instead of declaring its own copy of the number, so the address the linker keeps this
/// firmware out of and the address a DEPLOY erases from are one fact rather than two that agree
/// today. A locator region is the same shape.
///
/// A menu of where a locator MAY go is a board fact; where it IS in a given build is not, and
/// modelling a flash region in a board file would put the second in the first's place.
///
/// **`write_unit` MUST come from the part's own manual and NEVER from a sibling family.** That
/// rule is not caution: an STM32L4 and an STM32C0 share this controller's entire register block and
/// differ in the page numbering, and two SAM parts four years apart differ 4x in this number.
/// EVERY OFFSET IS RELATIVE TO THE REGION THIS IMPLEMENTOR WAS CONSTRUCTED FOR, never an
/// absolute flash address. A locator region and an image region are two implementors, and the
/// arithmetic that turns an offset into an address is the one place the region's base appears.
pub trait FirmwareFlash {
    /// Bytes committed by one program operation. Every write below is a whole number of these and
    /// starts on a multiple of one.
    fn write_unit(&self) -> usize;

    /// Erases `[offset, offset + len)`. After this every byte in it reads [`Self::erased_byte`].
    ///
    /// THE RANGE IS ROUNDED OUT TO WHOLE ERASE UNITS BY THE IMPLEMENTOR, and an implementor whose
    /// erase unit reaches outside the region it was given cannot honour this contract -- it would
    /// clear something that is not its own. Refusing to construct such an implementor is cheaper
    /// than discovering it when the something else disappears.
    fn erase(&mut self, offset: usize, len: usize) -> Result<(), FlashError>;

    /// Programs `data` at `offset`. Both are multiples of [`Self::write_unit`], and the target is
    /// always erased.
    fn write(&mut self, offset: usize, data: &[u8]) -> Result<(), FlashError>;

    /// Reads `out.len()` bytes from `offset`.
    fn read(&self, offset: usize, out: &mut [u8]) -> Result<(), FlashError>;

    /// What one erased byte reads as.
    ///
    /// **DEFAULTED TO `0xFF` BECAUSE ALMOST EVERY PART AGREES, AND OVERRIDABLE BECAUSE ONE DOES
    /// NOT.** An STM32L0 erases to `0x00`, so a blank check spelled `== 0xFF` is exactly backwards
    /// there -- a correctly erased part reads full and a full part reads erased. Every comparison
    /// in this crate goes through this method for that reason.
    fn erased_byte(&self) -> u8 {
        0xFF
    }
}

/// Why a flash operation did not happen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashError {
    /// The controller refused or the read-back did not match.
    Refused,
    /// The region is not big enough for a record on this part's write unit.
    RegionTooSmall,
}

/// The boot locator: which installed image runs next, written torn-write safe.
pub mod locator;

/// Programming a firmware image into an update region, and the checksum over what landed.
pub mod image;

/// The checksum the firmware block carries, named rather than assumed.
pub mod crc;
