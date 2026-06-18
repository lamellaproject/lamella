//! The ARM core registers.

/// One of the sixteen ARM core registers, identified by its 4-bit number.
///
/// The number is what instruction encodings embed, so a `Reg` maps directly
/// onto the register fields of an encoding. Construction goes through
/// [`Reg::new`], which rejects numbers outside `0..=15`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Reg(u8);

impl Reg {
    /// Register R0.
    pub const R0: Reg = Reg(0);
    /// Register R1.
    pub const R1: Reg = Reg(1);
    /// Register R2.
    pub const R2: Reg = Reg(2);
    /// Register R3.
    pub const R3: Reg = Reg(3);
    /// Register R4.
    pub const R4: Reg = Reg(4);
    /// Register R5.
    pub const R5: Reg = Reg(5);
    /// Register R6.
    pub const R6: Reg = Reg(6);
    /// Register R7.
    pub const R7: Reg = Reg(7);
    /// Register R8.
    pub const R8: Reg = Reg(8);
    /// Register R9.
    pub const R9: Reg = Reg(9);
    /// Register R10.
    pub const R10: Reg = Reg(10);
    /// Register R11.
    pub const R11: Reg = Reg(11);
    /// Register R12.
    pub const R12: Reg = Reg(12);
    /// The stack pointer, R13.
    pub const SP: Reg = Reg(13);
    /// The link register, R14, which holds the return address after a call.
    pub const LR: Reg = Reg(14);
    /// The program counter, R15.
    pub const PC: Reg = Reg(15);

    /// Creates a register from its number, or returns `None` if `number > 15`.
    #[must_use]
    pub const fn new(number: u8) -> Option<Reg> {
        if number <= 15 {
            Some(Reg(number))
        } else {
            None
        }
    }

    /// The 4-bit register number, in `0..=15`.
    #[must_use]
    pub const fn number(self) -> u8 {
        self.0
    }

    /// Whether this is a Thumb "low" register, R0 through R7.
    ///
    /// The 16-bit Thumb encodings address only the low registers, in 3-bit
    /// fields; the high registers R8 through R15 need a 32-bit encoding or one
    /// of the few 16-bit forms that carry an extra high bit (Armv6-M ARM
    /// (DDI 0419E), A5.2).
    #[must_use]
    pub const fn is_low(self) -> bool {
        self.0 <= 7
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_registers_have_the_expected_numbers() {
        assert_eq!(Reg::SP.number(), 13);
        assert_eq!(Reg::LR.number(), 14);
        assert_eq!(Reg::PC.number(), 15);
    }

    #[test]
    fn new_rejects_out_of_range_numbers() {
        assert_eq!(Reg::new(15), Some(Reg::PC));
        assert_eq!(Reg::new(16), None);
    }

    #[test]
    fn low_registers_are_r0_through_r7() {
        assert!(Reg::R7.is_low());
        assert!(!Reg::R8.is_low());
    }
}
