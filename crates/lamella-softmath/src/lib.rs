//! The `double` rounding kernel, without libm: truncate, floor, ceiling, round-half-to-even, and the
//! magnitude, comparison and NaN rules `System.Math`'s floating-point group is defined by.

#![no_std]

/// The magnitude at or above which a double has no fractional bits left. A value there is already
/// integral, so every rounding below returns it unchanged, and the `i64` round trip that the others
/// use would be lossy rather than exact.
const NO_FRACTION_ABOVE: f64 = 4_503_599_627_370_496.0;

/// The IEEE-754 sign bit of a double.
const SIGN_BIT: u64 = 0x8000_0000_0000_0000;

/// The magnitude of `value`: the sign bit cleared.
///
/// Branch-free, and correct for the signed-zero case that a select gets wrong: `-0.0 < 0.0` is
/// false, so `if value < 0.0 { -value } else { value }` hands back `-0.0` where this answers `+0.0`.
/// NaN stays NaN, with its payload; an infinity becomes positive infinity.
#[must_use]
pub fn abs(value: f64) -> f64 {
    f64::from_bits(value.to_bits() & !SIGN_BIT)
}

/// Whether `value` is NaN, an infinity, or already integral by magnitude -- the three cases every
/// rounding here returns unchanged.
///
/// Written as a NEGATED less-than, which is what puts NaN on the correct side: every comparison
/// against a NaN is false, so `!(magnitude < limit)` is true for it. `magnitude >= limit` would be
/// false for NaN and would send it down the rounding path, where the `i64` cast saturates and turns
/// it into a very large integer.
#[must_use]
pub fn already_integral(value: f64) -> bool {
    !(abs(value) < NO_FRACTION_ABOVE)
}

/// The integer part of `value`, toward zero -- `System.Math.Truncate`.
#[must_use]
pub fn truncate(value: f64) -> f64 {
    if already_integral(value) {
        return value;
    }
    (value as i64) as f64
}

/// The largest integer not greater than `value` -- `System.Math.Floor`.
///
/// Truncation goes toward zero, which is already the floor of a positive value and one too high for
/// a negative one with a fractional part. So the correction is exactly the case where truncating
/// moved the value up.
#[must_use]
pub fn floor(value: f64) -> f64 {
    let truncated = truncate(value);
    if truncated > value {
        truncated - 1.0
    } else {
        truncated
    }
}

/// The smallest integer not less than `value` -- `System.Math.Ceiling`. The mirror of [`floor`].
#[must_use]
pub fn ceiling(value: f64) -> f64 {
    let truncated = truncate(value);
    if truncated < value {
        truncated + 1.0
    } else {
        truncated
    }
}

/// `value` rounded to the nearest integer, ties to EVEN -- `System.Math.Round(double)`, whose
/// default is `MidpointRounding.ToEven`, and Python's `round(<float>)`, which specifies the same
/// rule. So `2.5` rounds to `2` and `3.5` to `4`, not away from zero.
///
/// Adding and then subtracting 2^52 is what does the rounding: the sum has no fractional bits to
/// spare, so the addition itself must round, and IEEE-754's default mode is round-to-nearest-EVEN --
/// the rule this function wants. Subtracting the same constant then returns the value to its own
/// magnitude. The constant carries `value`'s sign so negatives round symmetrically rather than
/// toward zero.
#[must_use]
pub fn round_half_to_even(value: f64) -> f64 {
    if already_integral(value) {
        return value;
    }
    let magic = f64::from_bits(NO_FRACTION_ABOVE.to_bits() | (value.to_bits() & SIGN_BIT));
    (value + magic) - magic
}

/// The larger of two doubles, or NaN when either is NaN -- `System.Math.Max(double, double)`.
///
/// **NaN PROPAGATES, and that is what makes this a different function from IEEE `fmax` or Rust's
/// `f64::max`, both of which return the non-NaN operand.** .NET propagates, so a maximum taken over
/// data containing a NaN is NaN rather than the largest real value in it. That answer is the visible
/// one; the other silently reports a maximum over a subset.
///
/// KNOWN DEVIATION from .NET, and it is deliberate rather than undiscovered: the signed-zero
/// tie-break is not modeled. `.NET`'s `Max(-0.0, 0.0)` is `+0.0` and this answers `-0.0`, because
/// `-0.0 >= 0.0` holds in IEEE. `System.Math`'s own source records the same boundary.
#[must_use]
pub fn max(a: f64, b: f64) -> f64 {
    if a != a || b != b {
        return f64::NAN;
    }
    if a >= b {
        a
    } else {
        b
    }
}

/// The smaller of two doubles, or NaN when either is NaN -- `System.Math.Min(double, double)`.
/// The mirror of [`max`], with its NaN propagation and its signed-zero deviation.
#[must_use]
pub fn min(a: f64, b: f64) -> f64 {
    if a != a || b != b {
        return f64::NAN;
    }
    if a <= b {
        a
    } else {
        b
    }
}

/// The sign of `value` as -1, 0 or 1, or `None` for NaN.
///
/// NaN is `None` rather than a sign because `System.Math.Sign(double)` throws `ArithmeticException`
/// for it: there is no sign to report, and answering 0 would be a wrong answer rather than a
/// missing one. The caller decides how to raise, which is the one part of this that differs between
/// a tier that can throw and one that cannot.
#[must_use]
pub fn sign(value: f64) -> Option<i32> {
    if value != value {
        return None;
    }
    Some(if value > 0.0 {
        1
    } else if value < 0.0 {
        -1
    } else {
        0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnitude_clears_the_sign_bit_including_on_zero() {
        assert_eq!(abs(-3.5), 3.5);
        assert_eq!(abs(3.5), 3.5);
        assert_eq!(abs(-0.0).to_bits(), 0.0f64.to_bits(), "-0.0 becomes +0.0");
        assert!(abs(f64::NAN).is_nan());
        assert_eq!(abs(f64::NEG_INFINITY), f64::INFINITY);
    }

    #[test]
    fn truncate_discards_the_fraction_toward_zero() {
        assert_eq!(truncate(2.9), 2.0);
        assert_eq!(truncate(-2.9), -2.0);
        assert_eq!(truncate(2.0), 2.0);
    }

    #[test]
    fn a_result_of_zero_loses_its_sign_and_that_is_the_recorded_deviation() {
        assert_eq!(truncate(-0.5).to_bits(), 0.0f64.to_bits());
        assert_eq!(ceiling(-0.5).to_bits(), 0.0f64.to_bits());
        assert_eq!(round_half_to_even(-0.4).to_bits(), 0.0f64.to_bits());
        assert_eq!(floor(-0.5), -1.0);
    }

    #[test]
    fn floor_and_ceiling_round_toward_the_infinities() {
        assert_eq!(floor(2.7), 2.0);
        assert_eq!(floor(-2.7), -3.0);
        assert_eq!(floor(2.0), 2.0);
        assert_eq!(ceiling(2.1), 3.0);
        assert_eq!(ceiling(-2.1), -2.0);
        assert_eq!(ceiling(2.0), 2.0);
    }

    #[test]
    fn round_breaks_ties_to_even_rather_than_away_from_zero() {
        assert_eq!(round_half_to_even(2.5), 2.0);
        assert_eq!(round_half_to_even(3.5), 4.0);
        assert_eq!(round_half_to_even(-2.5), -2.0);
        assert_eq!(round_half_to_even(-3.5), -4.0);
        assert_eq!(round_half_to_even(2.4), 2.0);
        assert_eq!(round_half_to_even(2.6), 3.0);
    }

    #[test]
    fn a_value_with_no_fractional_bits_is_returned_unchanged() {
        assert_eq!(truncate(NO_FRACTION_ABOVE), NO_FRACTION_ABOVE);
        assert_eq!(round_half_to_even(NO_FRACTION_ABOVE), NO_FRACTION_ABOVE);
        assert_eq!(floor(1e300), 1e300);
        assert!(truncate(f64::NAN).is_nan(), "NaN must not reach the i64 cast");
        assert!(round_half_to_even(f64::NAN).is_nan());
        assert_eq!(ceiling(f64::INFINITY), f64::INFINITY);
    }

    #[test]
    fn max_and_min_propagate_nan_in_both_operand_positions() {
        assert_eq!(max(2.5, 3.5), 3.5);
        assert_eq!(min(2.5, 3.5), 2.5);
        assert_eq!(max(-2.5, -3.5), -2.5);
        assert_eq!(min(-2.5, -3.5), -3.5);
        assert!(max(f64::NAN, 1.0).is_nan());
        assert!(max(1.0, f64::NAN).is_nan());
        assert!(min(f64::NAN, 1.0).is_nan());
        assert!(min(1.0, f64::NAN).is_nan());
    }

    #[test]
    fn sign_reports_nan_as_absent_rather_than_as_zero() {
        assert_eq!(sign(-7.2), Some(-1));
        assert_eq!(sign(0.0), Some(0));
        assert_eq!(sign(-0.0), Some(0));
        assert_eq!(sign(9.9), Some(1));
        assert_eq!(sign(f64::NAN), None, "there is no sign, and 0 would be a wrong one");
    }
}
