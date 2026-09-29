//! Integral constant arithmetic in the operands' own C# types (ECMA-334 14.5.12, 14.6, 14.7, 14.8,
//! 14.15).
//!
//! A [`Literal::Integer`] holds the low 64 bits of its value: a negative value is sign-extended and
//! an unsigned one zero-extended. The same bits are therefore different numbers at different types
//! (`0xFFFFFFFF` is `-1` as an `int` and 4294967295 as a `uint`), and only the type a constant is
//! held at says which. Every operation here decodes its operands at the types they are held at,
//! computes the exact result, and encodes it at the result's type, wrapped to that type's width the
//! way an unchecked operation wraps it.
//!
//! Wrapping is the value an operation has wherever it is permitted. An overflow in a checked
//! context is a compile-time error that the binder reports where it binds the operation, so no
//! program that reaches emission depends on what this module computes for one.

use crate::bound::integer_literal;
use crate::special::SpecialType;
use lamella_syntax::ast::{BinaryOperator, Literal, UnaryOperator};

/// The width in bits and the signedness of an integral type's values, with `char` as the unsigned
/// 16-bit type it is (11.1.4). `None` for every other type.
pub(crate) fn layout(ty: SpecialType) -> Option<(u32, bool)> {
    use SpecialType as S;
    Some(match ty {
        S::SByte => (8, true),
        S::Byte => (8, false),
        S::Int16 => (16, true),
        S::UInt16 | S::Char => (16, false),
        S::Int32 => (32, true),
        S::UInt32 => (32, false),
        S::Int64 => (64, true),
        S::UInt64 => (64, false),
        _ => return None,
    })
}

/// The value of an integral or `char` constant held at `ty`: its low bits, as many as `ty` has, read
/// as signed or unsigned. `None` for any other literal, or when `ty` is not integral.
pub(crate) fn value(literal: &Literal, ty: SpecialType) -> Option<i128> {
    let bits = match literal {
        Literal::Integer { value, .. } => *value,
        Literal::Character(unit) => u64::from(*unit),
        _ => return None,
    };
    let (width, signed) = layout(ty)?;
    Some(wrap(i128::from(bits), width, signed))
}

/// `value` as a constant of the integral type `ty`, wrapped to its width: a `char` is a
/// [`Literal::Character`], and every other type an integer literal holding the value's low 64 bits.
/// `None` when `ty` is not integral.
pub(crate) fn literal(value: i128, ty: SpecialType) -> Option<Literal> {
    let (width, signed) = layout(ty)?;
    let wrapped = wrap(value, width, signed);
    Some(if ty == SpecialType::Char {
        Literal::Character(wrapped as u16)
    } else {
        integer_literal(wrapped as i64)
    })
}

/// A unary `+`, `-` or `~` over an integral constant (14.6.1-14.6.4): `operand` is its value and
/// `result` the type the operation produces, which after unary numeric promotion is also the type
/// its operand is computed at. `None` for any other operator.
pub(crate) fn unary(operator: UnaryOperator, operand: i128, result: SpecialType) -> Option<Literal> {
    match operator {
        UnaryOperator::Plus => literal(operand, result),
        UnaryOperator::Minus => literal(-operand, result),
        UnaryOperator::Complement => literal(!operand, result),
        _ => None,
    }
}

/// A binary operator over integral constants (14.7-14.10): `left` and `right` are the operands'
/// values and `result` the type the operation produces. A comparison needs no result type and
/// answers a `bool`; every other operator answers `None` without one, and so does a division or
/// remainder by zero, which has no value.
pub(crate) fn binary(
    operator: BinaryOperator,
    left: i128,
    right: i128,
    result: Option<SpecialType>,
) -> Option<Literal> {
    use BinaryOperator as Op;
    let value = match operator {
        Op::Equal => return Some(Literal::Boolean(left == right)),
        Op::NotEqual => return Some(Literal::Boolean(left != right)),
        Op::LessThan => return Some(Literal::Boolean(left < right)),
        Op::LessThanOrEqual => return Some(Literal::Boolean(left <= right)),
        Op::GreaterThan => return Some(Literal::Boolean(left > right)),
        Op::GreaterThanOrEqual => return Some(Literal::Boolean(left >= right)),
        Op::LogicalAnd | Op::LogicalOr => return None,
        Op::Add => left + right,
        Op::Subtract => left - right,
        Op::Multiply => left.wrapping_mul(right),
        Op::Divide => left.checked_div(right)?,
        Op::Modulo => left.checked_rem(right)?,
        Op::LeftShift | Op::RightShift => {
            let (width, _) = layout(result?)?;
            let count = (right as u32) & (width.max(32) - 1);
            if operator == Op::LeftShift {
                left << count
            } else {
                left >> count
            }
        }
        Op::BitwiseAnd => left & right,
        Op::BitwiseOr => left | right,
        Op::BitwiseXor => left ^ right,
    };
    literal(value, result?)
}

/// `value` reduced to `width` bits and read back as signed or unsigned.
fn wrap(value: i128, width: u32, signed: bool) -> i128 {
    let modulus = 1i128 << width;
    let low = value & (modulus - 1);
    if signed && low >= modulus / 2 {
        low - modulus
    } else {
        low
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(value: i64) -> Literal {
        integer_literal(value)
    }

    #[test]
    fn the_same_bits_are_read_at_the_type_they_are_held_at() {
        let all_ones = int(-1);
        assert_eq!(value(&all_ones, SpecialType::Int32), Some(-1));
        assert_eq!(value(&all_ones, SpecialType::UInt32), Some(4_294_967_295));
        assert_eq!(value(&all_ones, SpecialType::UInt64), Some(i128::from(u64::MAX)));
        assert_eq!(value(&all_ones, SpecialType::Byte), Some(255));
        assert_eq!(value(&Literal::Character(0xFFFF), SpecialType::Char), Some(65_535));
        assert_eq!(value(&Literal::Boolean(true), SpecialType::Int32), None);
    }

    #[test]
    fn a_result_wraps_to_its_own_type() {
        let shl = |left: i128, right: i128, ty| binary(BinaryOperator::LeftShift, left, right, Some(ty));
        assert_eq!(shl(1, 33, SpecialType::Int32), Some(int(2)));
        assert_eq!(shl(1, 31, SpecialType::Int32), Some(int(i64::from(i32::MIN))));
        assert_eq!(shl(1, 64, SpecialType::Int64), Some(int(1)));
        assert_eq!(shl(1, -1, SpecialType::Int32), Some(int(i64::from(i32::MIN))));
        assert_eq!(shl(0x8000_0000, 1, SpecialType::UInt32), Some(int(0)));
        assert_eq!(
            unary(UnaryOperator::Complement, 0, SpecialType::UInt32),
            Some(int(4_294_967_295))
        );
        assert_eq!(unary(UnaryOperator::Complement, 0, SpecialType::Int32), Some(int(-1)));
    }

    #[test]
    fn an_unsigned_right_shift_brings_in_zeros() {
        let top = i128::from(u64::MAX) - i128::from(i64::MAX);
        assert_eq!(
            binary(BinaryOperator::RightShift, top, 1, Some(SpecialType::UInt64)),
            Some(int(4_611_686_018_427_387_904))
        );
        assert_eq!(
            binary(BinaryOperator::RightShift, -1, 1, Some(SpecialType::Int32)),
            Some(int(-1))
        );
    }

    #[test]
    fn a_comparison_of_unsigned_values_is_unsigned() {
        let max = i128::from(u64::MAX);
        assert_eq!(
            binary(BinaryOperator::GreaterThan, max, 1, None),
            Some(Literal::Boolean(true))
        );
        assert_eq!(
            binary(BinaryOperator::Divide, max, 3, Some(SpecialType::UInt64)),
            Some(int(6_148_914_691_236_517_205))
        );
        assert_eq!(binary(BinaryOperator::Divide, 1, 0, Some(SpecialType::Int32)), None);
    }
}
