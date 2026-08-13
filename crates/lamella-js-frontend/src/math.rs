//! Every floating-point operation this engine performs, in one place and on one implementation.

/// The largest integer not greater than `x`.
pub fn floor(x: f64) -> f64 {
    libm::floor(x)
}

/// The smallest integer not less than `x`.
pub fn ceil(x: f64) -> f64 {
    libm::ceil(x)
}

/// `x` with its fractional part removed, toward zero.
pub fn trunc(x: f64) -> f64 {
    libm::trunc(x)
}

/// The fractional part of `x`, keeping its sign.
pub fn fract(x: f64) -> f64 {
    x - libm::trunc(x)
}

pub fn abs(x: f64) -> f64 {
    libm::fabs(x)
}

pub fn sqrt(x: f64) -> f64 {
    libm::sqrt(x)
}

pub fn cbrt(x: f64) -> f64 {
    libm::cbrt(x)
}

/// `x` raised to `y`.
///
/// The caller supplies the standard's overrides -- `Math.pow(1, NaN)` is `NaN` where IEEE 754
/// says 1. This is the arithmetic, not the language rule.
pub fn powf(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

/// `Math.f16round`: `x` rounded to the nearest IEEE 754 binary16 value, back as a `f64`.
///
/// # IT IS NOT `fround` WITH A SMALLER TYPE, AND IT CANNOT GO THROUGH `f32`
///
/// `Math.fround` is one `as f32` because Rust has the type. There is no `f16` here, and the obvious
/// substitute -- `f64 -> f32 -> f16` -- **rounds twice**, which is wrong for every value that sits
/// near a binary16 tie: the first rounding can move a value across the halfway point the second one
/// is deciding on. The conversion is therefore a single step, done on the bits.
///
/// # THE THREE REGIMES, AND THE TWO PLACES THEY MEET
///
/// binary16 has a 5-bit exponent and a 10-bit significand, so a finite value is one of:
///
/// - **normal**, `m * 2^(e - 10)` with `m` in 1024..=2047 and `e` in -14..=15;
/// - **subnormal**, `m * 2^-24` with `m` in 1..=1023, reaching down to 2^-24;
/// - **zero**, which a value below half of 2^-24 rounds to.
///
/// Both boundaries are carried by the SAME arithmetic rather than by a branch, which is the part
/// worth writing down: rounding a subnormal's significand up to 1024 gives `1024 * 2^-24`, and that
/// IS the smallest normal, 2^-14; rounding a normal's up to 2048 carries into the exponent, and at
/// `e` of 15 that leaves 16, which is the overflow. So 65520 becomes infinity and 65519 becomes
/// 65504, with no comparison against either constant.
#[must_use]
pub fn f16round(x: f64) -> f64 {
    if x.is_nan() || x == 0.0 || x.is_infinite() {
        return x;
    }
    let bits = x.to_bits();
    let negative = bits >> 63 == 1;
    let biased = ((bits >> 52) & 0x7FF) as i32;
    if biased == 0 {
        return if negative { -0.0 } else { 0.0 };
    }
    let exponent = biased - 1023;
    let significand = (1u64 << 52) | (bits & 0x000F_FFFF_FFFF_FFFF);
    let sign = if negative { -1.0 } else { 1.0 };

    let round = |value: u64, shift: u32| -> u64 {
        if shift >= 64 {
            return 0;
        }
        let kept = value >> shift;
        let dropped = value & ((1u64 << shift) - 1);
        let half = 1u64 << (shift - 1);
        if dropped > half || (dropped == half && kept & 1 == 1) {
            kept + 1
        } else {
            kept
        }
    };
    let scale = |exponent: i32| f64::from_bits(((exponent + 1023) as u64) << 52);

    if exponent >= 16 {
        return sign * f64::INFINITY;
    }
    if exponent >= -14 {
        let mantissa = round(significand, 42);
        let (mantissa, exponent) =
            if mantissa == 2048 { (1024, exponent + 1) } else { (mantissa, exponent) };
        if exponent > 15 {
            return sign * f64::INFINITY;
        }
        return sign * (mantissa as f64) * scale(exponent - 10);
    }
    let mantissa = round(significand, (28 - exponent) as u32);
    sign * (mantissa as f64) * scale(-24)
}

pub fn ln(x: f64) -> f64 {
    libm::log(x)
}

pub fn log2(x: f64) -> f64 {
    libm::log2(x)
}

pub fn log10(x: f64) -> f64 {
    libm::log10(x)
}

pub fn exp(x: f64) -> f64 {
    libm::exp(x)
}

pub fn sin(x: f64) -> f64 {
    libm::sin(x)
}

pub fn cos(x: f64) -> f64 {
    libm::cos(x)
}

pub fn tan(x: f64) -> f64 {
    libm::tan(x)
}

pub fn asin(x: f64) -> f64 {
    libm::asin(x)
}

pub fn acos(x: f64) -> f64 {
    libm::acos(x)
}

pub fn atan(x: f64) -> f64 {
    libm::atan(x)
}

pub fn atan2(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

pub fn sinh(x: f64) -> f64 {
    libm::sinh(x)
}

pub fn cosh(x: f64) -> f64 {
    libm::cosh(x)
}

pub fn tanh(x: f64) -> f64 {
    libm::tanh(x)
}

pub fn asinh(x: f64) -> f64 {
    libm::asinh(x)
}

/// `acosh(x)` IS NaN FOR EVERY `x` BELOW 1, which is a domain rather than an error: the inverse
/// hyperbolic cosine has no real value there and the standard says NaN rather than throwing.
pub fn acosh(x: f64) -> f64 {
    libm::acosh(x)
}

/// `atanh(±1)` IS ±INFINITY and `|x| > 1` is NaN -- two different answers either side of the
/// same boundary, so a domain check that lumps them together gets one of them wrong.
pub fn atanh(x: f64) -> f64 {
    libm::atanh(x)
}

/// `ln(1 + x)`, accurate for small `x` where `ln(1 + x)` computed directly loses every digit.
pub fn log1p(x: f64) -> f64 {
    libm::log1p(x)
}

/// `exp(x) - 1`, accurate for small `x` for the same reason [`log1p`] exists.
pub fn expm1(x: f64) -> f64 {
    libm::expm1(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A THIN WRAPPER STILL DESERVES A ROW, because the failure it guards is a WRONG FUNCTION
    /// wired to a right name -- `libm::log` is the natural log and `libm::log10` is not, and
    /// nothing but a value catches a swap between them.
    #[test]
    fn each_name_is_wired_to_the_function_it_claims() {
        assert!((ln(core::f64::consts::E) - 1.0).abs() < 1e-12, "ln is the NATURAL log");
        assert!((log10(1000.0) - 3.0).abs() < 1e-12);
        assert!((log2(8.0) - 3.0).abs() < 1e-12);
        assert!((sqrt(9.0) - 3.0).abs() < 1e-12);
        assert!((cbrt(27.0) - 3.0).abs() < 1e-12);
        assert!((powf(2.0, 10.0) - 1024.0).abs() < 1e-12);
        assert!((exp(0.0) - 1.0).abs() < 1e-12);
        assert!((atan2(1.0, 0.0) - core::f64::consts::FRAC_PI_2).abs() < 1e-12);
    }

    /// THE HYPERBOLIC FAMILY IS SIX NAMES THAT ALL LOOK ALIKE, and a swap between any two is
    /// silent at 0 -- `sinh(0)`, `tanh(0)`, `asinh(0)` and `atanh(0)` are all +0, so a table of
    /// zeros proves nothing. Each row here is a value only its own function produces.
    #[test]
    fn each_hyperbolic_name_is_wired_to_its_own_function() {
        assert!((sinh(1.0) - 1.175_201_193_643_801_4).abs() < 1e-12);
        assert!((cosh(1.0) - 1.543_080_634_815_243_7).abs() < 1e-12);
        assert!((tanh(1.0) - 0.761_594_155_955_764_9).abs() < 1e-12);
        assert!((asinh(1.0) - 0.881_373_587_019_543).abs() < 1e-12);
        assert!((acosh(2.0) - 1.316_957_896_924_816_7).abs() < 1e-12);
        assert!((atanh(0.5) - 0.549_306_144_334_054_8).abs() < 1e-12);
        assert!((log1p(1e-16) - 1e-16).abs() < 1e-30, "log1p keeps digits ln(1 + x) loses");
        assert!((expm1(1e-16) - 1e-16).abs() < 1e-30, "and so does expm1");
    }

    /// THE DOMAIN EDGES, which are answers rather than errors: `acosh` below 1 is NaN, `atanh`
    /// is ±INFINITY exactly at ±1 and NaN outside -- two different answers either side of one
    /// boundary.
    #[test]
    fn the_hyperbolic_domains_answer_rather_than_fail() {
        assert!(acosh(0.5).is_nan());
        assert_eq!(acosh(1.0), 0.0);
        assert_eq!(atanh(1.0), f64::INFINITY);
        assert_eq!(atanh(-1.0), f64::NEG_INFINITY);
        assert!(atanh(1.5).is_nan());
        assert_eq!(tanh(f64::INFINITY), 1.0);
        assert_eq!(tanh(f64::NEG_INFINITY), -1.0);
        assert_eq!(cosh(0.0), 1.0);
        assert!(sinh(-0.0).is_sign_negative(), "sinh(-0) is -0");
        assert!(asinh(-0.0).is_sign_negative(), "asinh(-0) is -0");
        assert!(atanh(-0.0).is_sign_negative(), "atanh(-0) is -0");
        assert!(expm1(-0.0).is_sign_negative(), "expm1(-0) is -0");
        assert!(log1p(-0.0).is_sign_negative(), "log1p(-0) is -0");
        assert_eq!(log1p(-1.0), f64::NEG_INFINITY);
        assert!(log1p(-2.0).is_nan());
    }

    /// TRUNCATION IS TOWARD ZERO, which differs from `floor` for every negative non-integer --
    /// the two are interchangeable on positives, which is where a wrong wiring hides.
    #[test]
    fn truncation_and_flooring_part_company_on_negatives() {
        assert_eq!(trunc(2.7), 2.0);
        assert_eq!(floor(2.7), 2.0);
        assert_eq!(trunc(-2.7), -2.0, "toward zero");
        assert_eq!(floor(-2.7), -3.0, "toward negative infinity");
        assert_eq!(ceil(-2.7), -2.0);
        assert!((fract(-2.75) - -0.75).abs() < 1e-12, "and the fraction keeps the sign");
    }
}
