//! Constant values (ECMA-335 1st ed, II.22.9).

use crate::signature::element;
use alloc::vec::Vec;

/// A decoded constant value (II.22.9).
#[derive(Debug, Clone, PartialEq)]
pub enum ConstantValue {
    /// A `bool`.
    Bool(bool),
    /// A `char` (a UTF-16 code unit).
    Char(u16),
    /// An `sbyte`.
    I1(i8),
    /// A `byte`.
    U1(u8),
    /// A `short`.
    I2(i16),
    /// A `ushort`.
    U2(u16),
    /// An `int`.
    I4(i32),
    /// A `uint`.
    U4(u32),
    /// A `long`.
    I8(i64),
    /// A `ulong`.
    U8(u64),
    /// A `float`.
    R4(f32),
    /// A `double`.
    R8(f64),
    /// A `string` as UTF-16 code units.
    String(Vec<u16>),
    /// A null reference.
    Null,
}

/// Decodes a constant from its element type (II.22.9 `Type`) and value blob.
#[must_use]
pub fn decode_constant(element_type: u8, blob: &[u8]) -> Option<ConstantValue> {
    Some(match element_type {
        element::BOOLEAN => ConstantValue::Bool(*blob.first()? != 0),
        element::CHAR => ConstantValue::Char(u16::from_le_bytes(take(blob)?)),
        element::I1 => ConstantValue::I1(*blob.first()? as i8),
        element::U1 => ConstantValue::U1(*blob.first()?),
        element::I2 => ConstantValue::I2(i16::from_le_bytes(take(blob)?)),
        element::U2 => ConstantValue::U2(u16::from_le_bytes(take(blob)?)),
        element::I4 => ConstantValue::I4(i32::from_le_bytes(take(blob)?)),
        element::U4 => ConstantValue::U4(u32::from_le_bytes(take(blob)?)),
        element::I8 => ConstantValue::I8(i64::from_le_bytes(take(blob)?)),
        element::U8 => ConstantValue::U8(u64::from_le_bytes(take(blob)?)),
        element::R4 => ConstantValue::R4(f32::from_le_bytes(take(blob)?)),
        element::R8 => ConstantValue::R8(f64::from_le_bytes(take(blob)?)),
        element::STRING => {
            let mut units = Vec::with_capacity(blob.len() / 2);
            for pair in blob.chunks_exact(2) {
                units.push(u16::from_le_bytes([pair[0], pair[1]]));
            }
            ConstantValue::String(units)
        }
        element::CLASS => ConstantValue::Null,
        _ => return None,
    })
}

/// Reads a fixed-size little-endian field from the front of `blob`.
fn take<const N: usize>(blob: &[u8]) -> Option<[u8; N]> {
    blob.get(..N)?.try_into().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_integer_and_bool_constants() {
        assert_eq!(
            decode_constant(element::I4, &42i32.to_le_bytes()),
            Some(ConstantValue::I4(42))
        );
        assert_eq!(
            decode_constant(element::BOOLEAN, &[1]),
            Some(ConstantValue::Bool(true))
        );
        assert_eq!(
            decode_constant(element::CHAR, &b'Z'.to_le_bytes_u16()),
            Some(ConstantValue::Char(u16::from(b'Z')))
        );
    }

    #[test]
    fn decodes_string_and_null_constants() {
        let blob = [b'H', 0, b'i', 0];
        assert_eq!(
            decode_constant(element::STRING, &blob),
            Some(ConstantValue::String(alloc::vec![
                u16::from(b'H'),
                u16::from(b'i')
            ]))
        );
        assert_eq!(
            decode_constant(element::CLASS, &[0, 0, 0, 0]),
            Some(ConstantValue::Null)
        );
    }

    #[test]
    fn a_truncated_or_unknown_constant_is_none() {
        assert_eq!(decode_constant(element::I4, &[0, 0]), None);
        assert_eq!(decode_constant(element::VOID, &[]), None);
    }

    trait U16Bytes {
        fn to_le_bytes_u16(self) -> [u8; 2];
    }
    impl U16Bytes for u8 {
        fn to_le_bytes_u16(self) -> [u8; 2] {
            u16::from(self).to_le_bytes()
        }
    }
}
