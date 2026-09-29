//! Register access, so a family driver runs against the part itself or against a test's model of
//! it.

/// A part's 32-bit registers, by address.
pub trait Registers {
    /// Reads the register at `address`.
    fn read(&mut self, address: u32) -> u32;
    /// Writes `value` to the register at `address`.
    fn write(&mut self, address: u32, value: u32);
}

/// The part's own registers, reached by volatile access at their addresses.
#[derive(Debug)]
pub struct Mmio {
    _private: (),
}

impl Mmio {
    /// The accessor for the part this code runs on.
    ///
    /// # Safety
    ///
    /// Every address it is handed must be a register of the part the code is running on. That holds
    /// for the family drivers here when the library was built for the board it runs on.
    #[allow(unsafe_code)]
    #[must_use]
    pub const unsafe fn new() -> Self {
        Mmio { _private: () }
    }
}

impl Registers for Mmio {
    #[allow(unsafe_code)]
    fn read(&mut self, address: u32) -> u32 {
        // SAFETY: `Mmio::new`'s caller promised every address is one of this part's registers.
        unsafe { core::ptr::read_volatile(address as usize as *const u32) }
    }

    #[allow(unsafe_code)]
    fn write(&mut self, address: u32, value: u32) {
        // SAFETY: as for `read`.
        unsafe { core::ptr::write_volatile(address as usize as *mut u32, value) }
    }
}
