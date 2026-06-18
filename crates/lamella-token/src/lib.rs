#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Metadata tokens: the 4-byte references CIL instructions use to name metadata.

/// A metadata token: a table tag in the most significant byte and a 1-based row
/// index in the low three bytes (ECMA-335 1st ed, II.24.2.6).
///
/// The token does not interpret its table tag; mapping a tag to a metadata table
/// is the metadata model's job. A token whose row is 0 is the nil token, which
/// names no row.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct Token(pub u32);

impl Token {
    /// The largest representable row index: the low 24 bits all set.
    pub const MAX_ROW: u32 = 0x00FF_FFFF;

    /// Builds a token from a table tag and a 1-based row index. The row is masked
    /// to its low 24 bits; a caller passing a larger value has a bug.
    #[must_use]
    pub const fn new(table: u8, row: u32) -> Token {
        Token(((table as u32) << 24) | (row & Token::MAX_ROW))
    }

    /// The table tag: the most significant byte, naming which metadata table or
    /// heap the row refers to.
    #[must_use]
    pub const fn table(self) -> u8 {
        (self.0 >> 24) as u8
    }

    /// The 1-based row index within the table: the low three bytes.
    #[must_use]
    pub const fn row(self) -> u32 {
        self.0 & Token::MAX_ROW
    }

    /// Whether this is the nil token, whose row is 0 and which names no row.
    #[must_use]
    pub const fn is_nil(self) -> bool {
        self.row() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_and_row_round_trip() {
        let token = Token::new(0x02, 1234);
        assert_eq!(token.table(), 0x02);
        assert_eq!(token.row(), 1234);
        assert!(!token.is_nil());
    }

    #[test]
    fn the_raw_value_packs_the_table_then_the_row() {
        assert_eq!(Token::new(0x02, 1).0, 0x0200_0001);
    }

    #[test]
    fn a_zero_row_is_the_nil_token() {
        assert!(Token::new(0x01, 0).is_nil());
        assert_eq!(Token::new(0x01, 0).row(), 0);
    }

    #[test]
    fn a_row_is_masked_to_twenty_four_bits() {
        let token = Token::new(0x70, 0xFFFF_FFFF);
        assert_eq!(token.table(), 0x70);
        assert_eq!(token.row(), Token::MAX_ROW);
    }
}
