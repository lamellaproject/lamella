//! ARM condition codes.

/// A 4-bit ARM condition code, as tested by a conditional branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Cond {
    /// Equal (`Z == 1`).
    Eq,
    /// Not equal (`Z == 0`).
    Ne,
    /// Carry set, i.e. unsigned higher or same (`C == 1`).
    CarrySet,
    /// Carry clear, i.e. unsigned lower (`C == 0`).
    CarryClear,
    /// Minus / negative (`N == 1`).
    Minus,
    /// Plus / positive or zero (`N == 0`).
    Plus,
    /// Overflow set (`V == 1`).
    OverflowSet,
    /// Overflow clear (`V == 0`).
    OverflowClear,
    /// Unsigned higher (`C == 1 && Z == 0`).
    Higher,
    /// Unsigned lower or same (`C == 0 || Z == 1`).
    LowerOrSame,
    /// Signed greater than or equal (`N == V`).
    GreaterOrEqual,
    /// Signed less than (`N != V`).
    LessThan,
    /// Signed greater than (`Z == 0 && N == V`).
    GreaterThan,
    /// Signed less than or equal (`Z == 1 || N != V`).
    LessOrEqual,
}

impl Cond {
    /// The 4-bit encoding of this condition (Armv6-M ARM (DDI 0419E), Table A6-1).
    #[must_use]
    pub const fn encoding(self) -> u8 {
        match self {
            Cond::Eq => 0b0000,
            Cond::Ne => 0b0001,
            Cond::CarrySet => 0b0010,
            Cond::CarryClear => 0b0011,
            Cond::Minus => 0b0100,
            Cond::Plus => 0b0101,
            Cond::OverflowSet => 0b0110,
            Cond::OverflowClear => 0b0111,
            Cond::Higher => 0b1000,
            Cond::LowerOrSame => 0b1001,
            Cond::GreaterOrEqual => 0b1010,
            Cond::LessThan => 0b1011,
            Cond::GreaterThan => 0b1100,
            Cond::LessOrEqual => 0b1101,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings_follow_table_a6_1() {
        assert_eq!(Cond::Eq.encoding(), 0b0000);
        assert_eq!(Cond::Ne.encoding(), 0b0001);
        assert_eq!(Cond::GreaterOrEqual.encoding(), 0b1010);
        assert_eq!(Cond::LessThan.encoding(), 0b1011);
        assert_eq!(Cond::LessOrEqual.encoding(), 0b1101);
    }
}
