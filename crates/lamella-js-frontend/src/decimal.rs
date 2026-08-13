//! Turning an `f64` into the decimal digits the standard asks for, including at a tie.

use crate::{format, String, Vec};

/// Enough fraction digits that the expansion is EXACT for every finite `f64`. The smallest
/// subnormal is `2^-1074`, whose last significant digit sits at the 1,074th decimal place.
const EXACT_FRACTION_DIGITS: usize = 1080;

/// The exact decimal digits of a positive finite `value`, and where the point sits.
///
/// The digits have no leading and no trailing zero, and `value == 0.<digits> * 10^point`. So `123.5`
/// gives `("1235", 3)` and `0.0025` gives `("25", -2)`.
///
/// A ZERO INPUT HAS NO SIGNIFICANT DIGITS AT ALL and is the caller's case to handle: there is no
/// `point` that makes the identity above true for it, and inventing one -- `("0", 1)`, say -- makes
/// every caller carry a special digit that is not significant.
pub(crate) fn exact_digits(value: f64) -> (Vec<u8>, i32) {
    debug_assert!(value > 0.0 && value.is_finite());
    let rendered = format!("{value:.EXACT_FRACTION_DIGITS$}");
    let point_at = rendered.find('.').unwrap_or(rendered.len());
    let mut digits: Vec<u8> =
        rendered.bytes().filter(u8::is_ascii_digit).collect();
    let mut point = point_at as i32;
    let leading = digits.iter().take_while(|d| **d == b'0').count();
    digits.drain(..leading);
    point -= leading as i32;
    while digits.last() == Some(&b'0') {
        digits.pop();
    }
    (digits, point)
}

/// `digits`/`point` rounded to `keep` significant digits, **ties away from zero**.
///
/// THE CARRY CAN LENGTHEN THE NUMBER: rounding `999` to two digits is `10` at one place further
/// left, not `00`. Dropping that case gives an answer that is right except when every digit is a 9,
/// which is exactly where a reader stops checking.
fn round_significant(digits: &[u8], point: i32, keep: usize) -> (Vec<u8>, i32) {
    if digits.len() <= keep {
        let mut padded = digits.to_vec();
        padded.resize(keep, b'0');
        return (padded, point);
    }
    let mut kept = digits[..keep].to_vec();
    if digits[keep] < b'5' {
        return (kept, point);
    }
    for index in (0..keep).rev() {
        if kept[index] == b'9' {
            kept[index] = b'0';
            continue;
        }
        kept[index] += 1;
        return (kept, point);
    }
    kept.insert(0, b'1');
    kept.pop();
    (kept, point + 1)
}

/// The significant digits and decimal exponent of `value` at `keep` significant digits.
///
/// The exponent is the power of ten of the LEADING digit, so `123.5` at 2 digits gives
/// `("12", 2)` meaning `1.2e2`.
///
/// Zero answers `keep` zeros at exponent 0, which is what every caller wants and what the digit
/// routine above deliberately refuses to invent.
pub(crate) fn significant(value: f64, keep: usize) -> (String, i32) {
    if value == 0.0 {
        return ("0".repeat(keep), 0);
    }
    let (digits, point) = exact_digits(value);
    let (rounded, point) = round_significant(&digits, point, keep);
    (String::from_utf8(rounded).unwrap_or_default(), point - 1)
}

/// `value` in fixed notation with exactly `places` digits after the point, ties away from zero.
///
/// The digits kept are `point + places`, which can be ZERO or NEGATIVE for a small value:
/// `(0.5).toFixed(0)` keeps none, and the answer is still `"1"` because the first discarded digit
/// is a 5. A `usize` count would have made that case unrepresentable and the rounding would
/// silently disappear.
pub(crate) fn fixed(value: f64, places: usize) -> String {
    if value == 0.0 {
        return with_point(&"0".repeat(places + 1), 1, places);
    }
    let (digits, point) = exact_digits(value);
    let keep = point + places as i32;
    if keep < 0 {
        return with_point(&"0".repeat(places + 1), 1, places);
    }
    let (rounded, point) = if keep == 0 {
        if digits[0] >= b'5' {
            (b"1".to_vec(), point + 1)
        } else {
            return with_point(&"0".repeat(places + 1), 1, places);
        }
    } else {
        round_significant(&digits, point, keep as usize)
    };
    let text = String::from_utf8(rounded).unwrap_or_default();
    with_point(&text, point, places)
}

/// Places the decimal point in a digit string whose value is `0.<digits> * 10^point`, padding with
/// zeros on whichever side is short.
fn with_point(digits: &str, point: i32, places: usize) -> String {
    let mut out = String::new();
    if point <= 0 {
        out.push('0');
        if places > 0 {
            out.push('.');
            for _ in 0..(-point).min(places as i32) {
                out.push('0');
            }
            out.push_str(digits);
        }
    } else {
        let point = point as usize;
        if digits.len() <= point {
            out.push_str(digits);
            for _ in 0..(point - digits.len()) {
                out.push('0');
            }
            if places > 0 {
                out.push('.');
                for _ in 0..places {
                    out.push('0');
                }
            }
        } else {
            out.push_str(&digits[..point]);
            if places > 0 {
                out.push('.');
                out.push_str(&digits[point..]);
            }
        }
    }
    if places > 0 {
        let fraction = out.split('.').nth(1).map_or(0, str::len);
        if fraction < places {
            for _ in 0..(places - fraction) {
                out.push('0');
            }
        }
    }
    out
}

/// The exponent part the standard spells out: `e`, an explicit sign, then the digits.
///
/// THE `+` IS NOT OPTIONAL. `(1).toExponential(0)` is `"1e+0"`, and a formatter that omits it
/// for a non-negative exponent produces a string that no test in the corpus expects.
pub(crate) fn exponent_suffix(exponent: i32) -> String {
    if exponent < 0 {
        format!("e-{}", exponent.unsigned_abs())
    } else {
        format!("e+{exponent}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToString;

    /// THE HALF OF THE TIE RULE THE HOST GETS RIGHT IS `1.5` AND `3.5`. Testing only those
    /// agrees with round-half-to-even and proves nothing.
    #[test]
    fn a_tie_goes_away_from_zero_rather_than_to_even() {
        assert_eq!(fixed(0.5, 0), "1");
        assert_eq!(fixed(1.5, 0), "2");
        assert_eq!(fixed(2.5, 0), "3", "the host gives 2");
        assert_eq!(fixed(3.5, 0), "4");
        assert_eq!(fixed(4.5, 0), "5", "the host gives 4");
        assert_eq!(fixed(0.125, 2), "0.13", "exactly representable, so a real tie");
        assert_eq!(fixed(0.375, 2), "0.38");
    }

    /// AND THE CASE THAT LOOKS LIKE A TIE AND IS NOT. The `f64` nearest `1.005` is below the
    /// midpoint, so the answer is `"1.00"` -- a rounder that reads a few extra digits and guesses
    /// cannot tell this from the cases above.
    #[test]
    fn a_value_that_only_looks_like_a_tie_is_decided_by_its_exact_expansion() {
        assert_eq!(fixed(1.005, 2), "1.00");
        assert_eq!(fixed(1.045, 2), "1.04");
        assert_eq!(fixed(0.615, 2), "0.61");
        assert_eq!(fixed(1.015, 2), "1.01");
        assert_eq!(fixed(0.035, 2), "0.04");
        assert_eq!(fixed(0.005, 2), "0.01");
    }

    #[test]
    fn the_point_lands_where_the_places_say() {
        assert_eq!(fixed(0.0, 0), "0");
        assert_eq!(fixed(0.0, 2), "0.00");
        assert_eq!(fixed(1.0, 3), "1.000");
        assert_eq!(fixed(123.456, 2), "123.46");
        assert_eq!(fixed(123.456, 0), "123");
        assert_eq!(fixed(0.000_001, 3), "0.000");
        assert_eq!(fixed(1e20, 2), "100000000000000000000.00");
        assert_eq!(fixed(0.09, 1), "0.1");
    }

    /// THE CARRY OUT OF THE FRONT, which is right except when every digit is a 9.
    #[test]
    fn a_carry_past_the_leading_digit_lengthens_the_number() {
        assert_eq!(fixed(9.99, 1), "10.0");
        assert_eq!(fixed(0.999, 2), "1.00");
        assert_eq!(fixed(99.5, 0), "100");
        assert_eq!(significant(999.0, 2), ("10".to_string(), 3));
        assert_eq!(significant(0.999_9, 2), ("10".to_string(), 0));
    }

    #[test]
    fn significant_digits_carry_their_own_exponent() {
        assert_eq!(significant(123.5, 2), ("12".to_string(), 2));
        assert_eq!(significant(123.5, 4), ("1235".to_string(), 2));
        assert_eq!(significant(123.5, 6), ("123500".to_string(), 2), "padded, not truncated");
        assert_eq!(significant(0.001_234, 2), ("12".to_string(), -3));
        assert_eq!(significant(0.0, 3), ("000".to_string(), 0));
        assert_eq!(significant(1.0, 1), ("1".to_string(), 0));
    }

    #[test]
    fn the_exponent_suffix_always_carries_a_sign() {
        assert_eq!(exponent_suffix(0), "e+0");
        assert_eq!(exponent_suffix(21), "e+21");
        assert_eq!(exponent_suffix(-7), "e-7");
    }
}
