//! The STM32U5 boards' half of a deployed image: the startup validates the separately supplied I/O
//! rail the board powers, so the pads on it answer a program.
//!
//! An STM32U5 isolates its VDDIO2 rail's pads, PG2 to PG15, at reset, logically and electrically:
//! RM0456 Rev 7 section 10.4 makes setting PWR_SVMCR's IO2SV bit mandatory before they are used, and
//! PWR's registers answer only while RCC_AHB3ENR's PWREN gate is open. Every address and bit comes
//! from the board's generated bindings, which name the rail's gate and its valid bit.

use crate::registers::Registers;

/// A rail the part isolates at reset and the board powers: the RCC register and bit that gate the
/// controller holding the rail's valid bit, and the register and bit that declare it valid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rail {
    /// The RCC enable register of the controller that holds the valid bit.
    pub clock_register: u32,
    /// That controller's enable bit.
    pub clock_mask: u32,
    /// The register holding the rail's valid bit.
    pub valid_register: u32,
    /// The valid bit.
    pub valid_mask: u32,
}

/// Validates `rail`: opens its controller's clock gate, reads the gate back, then sets the valid
/// bit, each a read-modify-write that leaves every other bit as it was.
///
/// The read-back is the wait RM0456 Rev 7 section 11.4.24 asks for, two cycles of the peripheral's
/// clock after its enable is set before its registers are reached: the read cannot complete before
/// the write it follows has reached the RCC.
pub fn apply(registers: &mut impl Registers, rail: Rail) {
    let gate = registers.read(rail.clock_register);
    registers.write(rail.clock_register, gate | rail.clock_mask);
    let _ = registers.read(rail.clock_register);
    let valid = registers.read(rail.valid_register);
    registers.write(rail.valid_register, valid | rail.valid_mask);
}
