//! The abstract operations, which are the ONLY arithmetic path.

use crate::{format, String, ToString};
use crate::string_value::JsString;
use crate::value::JsValue;

/// The operation needs the object protocol, which lives in the interpreter.
///
/// A distinct type rather than a `None`, so a caller cannot mistake "ask the object" for "the answer
/// is undefined" -- which is how a conversion quietly becomes wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NeedsObjectProtocol;

/// The hint `ToPrimitive` was called with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hint {
    Default,
    Number,
    String,
}


/// `ToBoolean`.
///
/// The falsy set is exactly `undefined`, `null`, `false`, `+0`, `-0`, `NaN`, and the empty string.
/// **Everything else is true, including `"0"`, `"false"` and every object.** A host-language
/// truthiness test gets `"0"` wrong, and it is one of the most common ways an engine drifts.
#[must_use]
pub fn to_boolean(value: &JsValue) -> bool {
    match value {
        JsValue::Undefined | JsValue::Null => false,
        JsValue::Boolean(b) => *b,
        JsValue::Number(n) => *n != 0.0 && !n.is_nan(),
        JsValue::String(s) => !s.is_empty(),
        JsValue::Symbol(_) => true,
        JsValue::Object(_) => true,
    }
}

/// `ToNumber`.
pub fn to_number(value: &JsValue) -> Result<f64, NeedsObjectProtocol> {
    Ok(match value {
        JsValue::Undefined => f64::NAN,
        JsValue::Null => 0.0,
        JsValue::Boolean(true) => 1.0,
        JsValue::Boolean(false) => 0.0,
        JsValue::Number(n) => *n,
        JsValue::String(s) => string_to_number(s),
        JsValue::Symbol(_) => return Err(NeedsObjectProtocol),
        JsValue::Object(_) => return Err(NeedsObjectProtocol),
    })
}

/// `StringNumericValue`: the grammar a string must match to be a number.
///
/// # IT IS NOT THE NUMERIC LITERAL GRAMMAR, AND THE DIFFERENCES ARE LOAD-BEARING
///
/// - **The empty string, and any string of only whitespace, is `0`** -- not `NaN`. `Number("")` is
///   `0`, `Number(" ")` is `0`, and `+[]` is `0` for this reason.
/// - **Numeric separators are NOT allowed**: `Number("1_000")` is `NaN`, though `1_000` is a
///   perfectly good literal. A conversion that reuses the lexer's number scanner gets this wrong in
///   the direction of accepting too much.
/// - **A leading `+`/`-` is allowed**, which a literal's grammar does not have -- the sign is a
///   unary operator there.
/// - `"Infinity"`, `"+Infinity"` and `"-Infinity"` are accepted by NAME.
/// - **Legacy octal is not accepted**: `Number("010")` is `10`, not `8`.
/// - Anything left over after the number is `NaN`: `Number("12abc")` is `NaN`, where C's `strtod`
///   would happily return 12 and stop. **That is the single most common way this conversion is got
///   wrong**, because the host function does the wrong thing by default.
#[must_use]
pub fn string_to_number(value: &JsString) -> f64 {
    let text = value.to_lossy_string();
    let trimmed = text.trim_matches(is_string_numeric_whitespace);
    if trimmed.is_empty() {
        return 0.0;
    }

    if let Some(digits) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
        return radix_digits(digits, 16);
    }
    if let Some(digits) = trimmed.strip_prefix("0o").or_else(|| trimmed.strip_prefix("0O")) {
        return radix_digits(digits, 8);
    }
    if let Some(digits) = trimmed.strip_prefix("0b").or_else(|| trimmed.strip_prefix("0B")) {
        return radix_digits(digits, 2);
    }

    let (sign, rest) = match trimmed.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, trimmed.strip_prefix('+').unwrap_or(trimmed)),
    };
    if rest == "Infinity" {
        return sign * f64::INFINITY;
    }
    if !rest.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-')) {
        return f64::NAN;
    }
    match rest.parse::<f64>() {
        Ok(number) => sign * number,
        Err(_) => f64::NAN,
    }
}

/// The whitespace `ToNumber` strips: `WhiteSpace` and `LineTerminator` together.
fn is_string_numeric_whitespace(ch: char) -> bool {
    crate::source::is_whitespace(ch) || crate::source::is_line_terminator(ch)
}

fn radix_digits(digits: &str, radix: u32) -> f64 {
    if digits.is_empty() {
        return f64::NAN;
    }
    let mut value = 0.0f64;
    for ch in digits.chars() {
        match ch.to_digit(radix) {
            Some(digit) => value = value * f64::from(radix) + f64::from(digit),
            None => return f64::NAN,
        }
    }
    value
}

/// `ToInt32`.
///
/// # THE OPERATION THE LANDSCAPE REPORT FOUND EVERY SMALL ENGINE GETTING WRONG
///
/// Bitwise operators are defined to convert through **exactly 32 bits, modulo 2^32, with a truncating
/// (toward-zero) integer step first**. An engine that casts an f64 to the host's `int` inherits the
/// host's width and its undefined behaviour on overflow, so `2**31 | 0` gives different answers on
/// different targets -- silently.
///
/// `NaN`, the infinities and `-0` all become `+0`, which a host cast does not guarantee either.
#[must_use]
pub fn to_int32(number: f64) -> i32 {
    to_uint32(number) as i32
}

/// `ToUint32`. The shared core of both: truncate, then reduce modulo 2^32.
#[must_use]
pub fn to_uint32(number: f64) -> u32 {
    if !number.is_finite() || number == 0.0 {
        return 0;
    }
    let truncated = crate::math::trunc(number);
    let modulo = truncated - crate::math::floor(truncated / 4_294_967_296.0) * 4_294_967_296.0;
    modulo as u32
}

/// `ToString` for a Number.
///
/// # NOT THE HOST'S FLOAT FORMATTER, AND THE DIFFERENCES ARE OBSERVABLE
///
/// The standard specifies the result exactly: the SHORTEST decimal that round-trips, then a
/// five-case rule for where the point goes and when to use exponential form. Rust's `{}` also gives
/// shortest-round-trip digits -- the hard half -- but formats them differently at the extremes, so
/// the digits are taken from it and the placement is done here.
///
/// The cases that separate a real implementation from a printf call:
/// - **`-0` prints as `"0"`**, with no sign, though it is a distinct value.
/// - `1e21` prints as `"1e+21"`, and `1e20` prints as `"100000000000000000000"` -- the switch is at
///   21 digits, not at whatever the host chose.
/// - `1e-7` prints as `"1e-7"` and `1e-6` as `"0.000001"`.
#[must_use]
pub fn number_to_string(number: f64) -> String {
    if number.is_nan() {
        return "NaN".to_string();
    }
    if number == 0.0 {
        return "0".to_string();
    }
    if number < 0.0 {
        return format!("-{}", number_to_string(-number));
    }
    if number.is_infinite() {
        return "Infinity".to_string();
    }

    let (digits, exponent10) = shortest_digits(number);
    let k = digits.len() as i32;
    let n = exponent10 + 1;

    if k <= n && n <= 21 {
        let mut out = digits;
        out.push_str(&"0".repeat((n - k) as usize));
        out
    } else if 0 < n && n <= 21 {
        format!("{}.{}", &digits[..n as usize], &digits[n as usize..])
    } else if -6 < n && n <= 0 {
        format!("0.{}{}", "0".repeat((-n) as usize), digits)
    } else if k == 1 {
        format!("{}e{}{}", digits, if n - 1 >= 0 { "+" } else { "-" }, (n - 1).abs())
    } else {
        format!(
            "{}.{}e{}{}",
            &digits[..1],
            &digits[1..],
            if n - 1 >= 0 { "+" } else { "-" },
            (n - 1).abs()
        )
    }
}

/// The shortest round-tripping decimal digits of a positive finite `number`, and the base-10
/// exponent of its first digit.
fn shortest_digits(number: f64) -> (String, i32) {
    let formatted = format!("{number:e}");
    let (mantissa, exponent) = formatted.split_once('e').expect("`{:e}` always has an exponent");
    let exponent: i32 = exponent.parse().expect("`{:e}` writes a decimal exponent");
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let digits = digits.trim_end_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    (even_at_a_tie(number, digits), exponent)
}

/// The standard's LAST tie-break, which Rust's shortest-round-trip does not share.
///
/// # TWO DIGIT STRINGS OF THE SAME LENGTH CAN BOTH ROUND-TRIP, AND THE STANDARD SAYS WHICH
///
/// `Number::toString` asks for the shortest `s` that round-trips, then: "If there are multiple
/// possibilities for `s`, choose the value of `s` for which `s * 10^(n-k)` is closest in value to
/// `x`. **If there are two such possible values of `s`, choose the one that is even.**" Rust's
/// formatter answers the first two questions and is free about the third; at a real tie it hands
/// back the ODD candidate, so `String(0.0017900466918945312)` printed `...313`.
///
/// The values it happens on are exactly those whose EXACT expansion is one digit longer than the
/// shortest form and ends in a 5 -- so `x` sits precisely halfway between the two candidates, and
/// the exact expansion is what says so. Guessing from a few extra digits cannot tell a real tie from
/// a near one, which is the same reason [`crate::decimal`] carries the full expansion.
///
/// THE SWAP IS VERIFIED BY PARSING IT BACK. At seventeen digits the neighbouring candidate is
/// about one part in 10^16 away and a `f64`'s spacing is about one in 10^16 too, so "both
/// round-trip" is nearly true rather than always true. A candidate that does not read back as the
/// same number is not a candidate at all, and the original stands.
fn even_at_a_tie(number: f64, digits: &str) -> String {
    let last = digits.as_bytes().last().copied().unwrap_or(b'0');
    if last % 2 == 0 {
        return digits.to_string();
    }
    let keep = digits.len();
    let (exact, point) = crate::decimal::exact_digits(number);
    if exact.len() != keep + 1 || exact[keep] != b'5' {
        return digits.to_string();
    }
    let mut raised = exact[..keep].to_vec();
    for slot in raised.iter_mut().rev() {
        if *slot == b'9' {
            *slot = b'0';
        } else {
            *slot += 1;
            break;
        }
    }
    if raised.first() == Some(&b'0') {
        return digits.to_string();
    }
    let even = if exact[keep - 1] % 2 == 0 { &exact[..keep] } else { &raised[..] };
    let even: String = even.iter().map(|digit| *digit as char).collect();
    if even == digits {
        return digits.to_string();
    }
    match crate::format!("{even}e{}", point - keep as i32).parse::<f64>() {
        Ok(read) if read == number => even,
        _ => digits.to_string(),
    }
}

/// `ToString`.
pub fn to_string(value: &JsValue) -> Result<JsString, NeedsObjectProtocol> {
    Ok(match value {
        JsValue::Undefined => JsString::from("undefined"),
        JsValue::Null => JsString::from("null"),
        JsValue::Boolean(true) => JsString::from("true"),
        JsValue::Boolean(false) => JsString::from("false"),
        JsValue::Number(n) => JsString::from(number_to_string(*n).as_str()),
        JsValue::String(s) => s.clone(),
        JsValue::Symbol(_) => return Err(NeedsObjectProtocol),
        JsValue::Object(_) => return Err(NeedsObjectProtocol),
    })
}

/// `IsStrictlyEqual` -- the `===` operator.
///
/// Two rules a host comparison does not have: **`NaN` is not equal to itself**, and **`+0` equals
/// `-0`**. Both matter, and they pull in opposite directions from "compare the bits".
#[must_use]
/// `SameValueZero`: how a `Map` key and a `Set` member are compared, and what
/// `Array.prototype.includes` uses.
///
/// # IT DIFFERS FROM `===` IN EXACTLY ONE PLACE, AND FROM `Object.is` IN EXACTLY ONE OTHER
///
/// | | `NaN` vs `NaN` | `+0` vs `-0` |
/// |---|---|---|
/// | `===` (`strict_equals`) | false | **true** |
/// | `Object.is` (SameValue) | **true** | false |
/// | **SameValueZero** | **true** | **true** |
///
/// So `new Set([NaN, NaN]).size` is 1 -- a set built on `===` would hold two -- and
/// `new Set([0, -0]).size` is also 1, where a set built on `Object.is` would hold two. **Reaching
/// for either of the two operations that already exist gets one of those wrong**, and each is a
/// single line in a test nobody writes by accident.
pub fn same_value_zero(left: &JsValue, right: &JsValue) -> bool {
    if let (JsValue::Number(a), JsValue::Number(b)) = (left, right) {
        return a == b || (a.is_nan() && b.is_nan());
    }
    strict_equals(left, right)
}

/// `SameValue`, which differs from `===` on exactly two inputs and is why `Object.is` exists.
///
/// See [`same_value_zero`]'s table for all three at once: this is the row that separates `NaN`
/// (equal here) from `-0` against `+0` (NOT equal here, and equal under both of the others).
pub fn same_value(a: &JsValue, b: &JsValue) -> bool {
    match (a, b) {
        (JsValue::Number(a), JsValue::Number(b)) => {
            if a.is_nan() && b.is_nan() {
                return true;
            }
            a == b && a.is_sign_negative() == b.is_sign_negative()
        }
        _ => strict_equals(a, b),
    }
}

pub fn strict_equals(left: &JsValue, right: &JsValue) -> bool {
    match (left, right) {
        (JsValue::Undefined, JsValue::Undefined) | (JsValue::Null, JsValue::Null) => true,
        (JsValue::Number(a), JsValue::Number(b)) => a == b,
        (JsValue::String(a), JsValue::String(b)) => a == b,
        (JsValue::Boolean(a), JsValue::Boolean(b)) => a == b,
        (JsValue::Object(a), JsValue::Object(b)) => a == b,
        (JsValue::Symbol(a), JsValue::Symbol(b)) => a == b,
        _ => false,
    }
}

/// `IsLooselyEqual` -- the `==` operator, for primitives.
///
/// **`null == undefined` is TRUE and `null == 0` is FALSE.** `null` and `undefined` are equal to
/// each other and to nothing else, which is not what "coerce and compare" would produce, and is the
/// rule most often lost when this operator is reimplemented as a coercion.
pub fn loose_equals(left: &JsValue, right: &JsValue) -> Result<bool, NeedsObjectProtocol> {
    use JsValue::*;
    Ok(match (left, right) {
        (Object(_), _) | (_, Object(_)) => return Err(NeedsObjectProtocol),
        (Undefined | Null, Undefined | Null) => true,
        (Undefined | Null, _) | (_, Undefined | Null) => false,
        (Number(_), Number(_)) | (String(_), String(_)) | (Boolean(_), Boolean(_)) => {
            strict_equals(left, right)
        }
        (Boolean(b), other) => {
            return loose_equals(&Number(if *b { 1.0 } else { 0.0 }), other);
        }
        (other, Boolean(b)) => {
            return loose_equals(other, &Number(if *b { 1.0 } else { 0.0 }));
        }
        (Number(n), String(s)) => *n == string_to_number(s),
        (String(s), Number(n)) => string_to_number(s) == *n,
        (Symbol(a), Symbol(b)) => a == b,
        (Symbol(_), _) | (_, Symbol(_)) => false,
    })
}

/// The result of `IsLessThan`, which has three outcomes rather than two.
///
/// **`undefined` is a real answer**, produced whenever either side is `NaN`, and it is why every
/// relational operator is defined in terms of this one operation with the arguments swapped and the
/// result negated in specific combinations. `a < b` being false does NOT mean `a >= b`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comparison {
    True,
    False,
    Undefined,
}

/// `IsLessThan(left, right)` for primitives.
///
/// **Two strings compare by CODE UNIT, not numerically and not by locale.** `"10" < "9"` is true.
/// Anything else compares as numbers.
pub fn less_than(left: &JsValue, right: &JsValue) -> Result<Comparison, NeedsObjectProtocol> {
    if left.is_object() || right.is_object() {
        return Err(NeedsObjectProtocol);
    }
    if let (JsValue::String(a), JsValue::String(b)) = (left, right) {
        return Ok(if a.units() < b.units() { Comparison::True } else { Comparison::False });
    }
    let a = to_number(left)?;
    let b = to_number(right)?;
    Ok(if a.is_nan() || b.is_nan() {
        Comparison::Undefined
    } else if a < b {
        Comparison::True
    } else {
        Comparison::False
    })
}

/// The `%` operator's numeric step.
///
/// # THE HEADLINE DEFECT OF EVERY SMALL ENGINE SURVEYED
///
/// ECMAScript's `%` is a **floating-point remainder that truncates toward zero**, so `5.5 % 2.5` is
/// `0.5`. An engine that reaches for the host's integer `%` returns `1`, because it truncated the
/// operands first -- and nothing about the program looks wrong.
///
/// It is also NOT IEEE `remainder`, which rounds to nearest and would give `0.5` here but `-0.5` for
/// `5.5 % 2.5` with different operands; Rust's `%` on `f64` is the truncating one the standard wants.
#[must_use]
pub fn remainder(left: f64, right: f64) -> f64 {
    left % right
}

/// `Number::exponentiate`, which is what BOTH `**` and `Math.pow` are defined to be.
///
/// # IT LIVES HERE BECAUSE TWO IMPLEMENTATIONS OF IT DRIFT
///
/// Carrying the standard's overrides inside `Math.pow` while the `**` operator calls the bare
/// `libm` power makes `Math.pow(-1, Infinity)` `NaN` and `(-1) ** Infinity` `1` -- the same rule,
/// answered two ways, in one engine. Adding the missing case beside the operator leaves the pair to
/// drift again on the next one, so the rule is extracted and both sites call it.
///
/// IEEE 754 `pow` differs from the standard in exactly two places, and both are the same refusal to
/// take a limit:
///
/// - `pow(x, NaN)` is `1` for `x` of 1 and NaN otherwise; the standard makes a NaN exponent NaN
///   whatever the base, so `1 ** NaN` is NaN.
/// - `pow(±1, ±Infinity)` is `1`, because every even exponent gives 1; the standard makes it NaN.
///
/// An exponent of `±0` is `1` for EVERY base including NaN, in both, and that ordering is why
/// the NaN-exponent test comes first and the zero-exponent case needs no line of its own.
#[must_use]
pub fn exponentiate(base: f64, exponent: f64) -> f64 {
    if exponent.is_nan() {
        return f64::NAN;
    }
    if crate::math::abs(base) == 1.0 && exponent.is_infinite() {
        return f64::NAN;
    }
    crate::math::powf(base, exponent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(value: f64) -> JsValue {
        JsValue::Number(value)
    }
    fn s(text: &str) -> JsValue {
        JsValue::string(text)
    }

    /// THE HEADLINE. Every small engine surveyed returns `1` here, by reaching for the host's
    /// integer remainder. The answer is `0.5`.
    #[test]
    fn the_remainder_operator_is_floating_point_and_truncates_toward_zero() {
        assert_eq!(remainder(5.5, 2.5), 0.5);
        assert_eq!(remainder(-5.5, 2.5), -0.5, "the sign follows the DIVIDEND");
        assert_eq!(remainder(5.5, -2.5), 0.5);
        assert!(remainder(5.0, 0.0).is_nan());
        assert_eq!(remainder(5.0, 3.0), 2.0);
    }

    /// Bitwise operators go through exactly 32 bits, modulo 2^32, truncating first. A host cast
    /// inherits the host's width and its overflow behaviour.
    #[test]
    fn to_int32_wraps_at_exactly_thirty_two_bits() {
        assert_eq!(to_int32(0.0), 0);
        assert_eq!(to_int32(-0.0), 0);
        assert_eq!(to_int32(f64::NAN), 0, "NaN is +0, not a trap and not the host's INT_MIN");
        assert_eq!(to_int32(f64::INFINITY), 0);
        assert_eq!(to_int32(f64::NEG_INFINITY), 0);
        assert_eq!(to_int32(1.9), 1, "truncates toward zero");
        assert_eq!(to_int32(-1.9), -1, "toward zero, NOT floor");
        assert_eq!(to_int32(4_294_967_296.0), 0, "2^32 wraps to 0");
        assert_eq!(to_int32(2_147_483_648.0), -2_147_483_648, "2^31 wraps to the minimum");
        assert_eq!(to_int32(-1.0), -1);
        assert_eq!(to_int32(4_294_967_295.0), -1, "2^32-1 is -1 as an int32");
    }

    #[test]
    fn to_uint32_lands_in_the_unsigned_range() {
        assert_eq!(to_uint32(-1.0), 4_294_967_295);
        assert_eq!(to_uint32(4_294_967_296.0), 0);
        assert_eq!(to_uint32(-4_294_967_297.0), 4_294_967_295);
    }

    /// The single most common way ToNumber is got wrong: the host's `strtod` returns 12 for
    /// `"12abc"` and stops. The standard says the WHOLE string must be a number or the answer is
    /// NaN. And an empty or whitespace-only string is `0`, not NaN.
    #[test]
    fn string_to_number_takes_the_whole_string_or_nothing() {
        assert_eq!(string_to_number(&JsString::from("")), 0.0, "the empty string is 0");
        assert_eq!(string_to_number(&JsString::from("   \t\n ")), 0.0, "and so is whitespace");
        assert_eq!(string_to_number(&JsString::from("12")), 12.0);
        assert_eq!(string_to_number(&JsString::from("  12  ")), 12.0);
        assert!(string_to_number(&JsString::from("12abc")).is_nan(), "not 12");
        assert_eq!(string_to_number(&JsString::from("-1.5")), -1.5);
        assert_eq!(string_to_number(&JsString::from("+2")), 2.0, "a leading sign IS allowed here");
        assert_eq!(string_to_number(&JsString::from("Infinity")), f64::INFINITY);
        assert_eq!(string_to_number(&JsString::from("-Infinity")), f64::NEG_INFINITY);
        assert_eq!(string_to_number(&JsString::from("0x10")), 16.0);
        assert_eq!(string_to_number(&JsString::from("0b11")), 3.0);
        assert_eq!(string_to_number(&JsString::from("0o17")), 15.0);
        assert_eq!(string_to_number(&JsString::from("010")), 10.0, "NOT 8 -- no legacy octal here");
    }

    /// Numeric separators are a LITERAL feature and not a ToNumber one, so a conversion that
    /// reuses the lexer's scanner accepts a string the standard says is NaN.
    #[test]
    fn numeric_separators_are_not_allowed_in_a_converted_string() {
        assert!(string_to_number(&JsString::from("1_000")).is_nan());
        assert!(string_to_number(&JsString::from("0x_10")).is_nan());
    }

    /// The falsy set is exact, and `"0"` is NOT in it. A host truthiness test gets that wrong.
    #[test]
    fn the_falsy_set_is_exactly_seven_values() {
        for value in [
            JsValue::Undefined,
            JsValue::Null,
            JsValue::Boolean(false),
            n(0.0),
            n(-0.0),
            n(f64::NAN),
            s(""),
        ] {
            assert!(!to_boolean(&value), "{value:?} is falsy");
        }
        for value in [s("0"), s("false"), s(" "), n(-1.0), n(f64::INFINITY), JsValue::Boolean(true)]
        {
            assert!(to_boolean(&value), "{value:?} is TRUTHY");
        }
    }

    /// Two rules a bit-comparison does not have, pulling in opposite directions.
    #[test]
    fn strict_equality_says_nan_differs_from_itself_and_minus_zero_does_not() {
        assert!(!strict_equals(&n(f64::NAN), &n(f64::NAN)), "NaN !== NaN");
        assert!(strict_equals(&n(0.0), &n(-0.0)), "+0 === -0");
        assert!(strict_equals(&s("a"), &s("a")));
        assert!(!strict_equals(&n(1.0), &s("1")), "no coercion in ===");
        assert!(strict_equals(&JsValue::Null, &JsValue::Null));
        assert!(!strict_equals(&JsValue::Null, &JsValue::Undefined));
    }

    /// `null == undefined` is TRUE and `null == 0` is FALSE. The pair is equal to each other and
    /// to nothing else -- not what "coerce both sides" would give.
    #[test]
    fn loose_equality_treats_null_and_undefined_as_a_closed_pair() {
        assert_eq!(loose_equals(&JsValue::Null, &JsValue::Undefined), Ok(true));
        assert_eq!(loose_equals(&JsValue::Null, &n(0.0)), Ok(false));
        assert_eq!(loose_equals(&JsValue::Undefined, &n(0.0)), Ok(false));
        assert_eq!(loose_equals(&JsValue::Null, &s("")), Ok(false));
    }

    /// A Boolean converts to a Number FIRST and is then compared again -- which is why `"1" == true`
    /// and `"true" == true` differ.
    #[test]
    fn loose_equality_converts_a_boolean_to_a_number_before_comparing() {
        assert_eq!(loose_equals(&s("1"), &JsValue::Boolean(true)), Ok(true));
        assert_eq!(loose_equals(&s("true"), &JsValue::Boolean(true)), Ok(false));
        assert_eq!(loose_equals(&n(1.0), &s("1")), Ok(true));
        assert_eq!(loose_equals(&n(f64::NAN), &n(f64::NAN)), Ok(false));
    }

    /// THREE outcomes, not two: `undefined` whenever either side is NaN. `a < b` being false does
    /// not mean `a >= b`, and an engine with a two-valued comparison cannot express that.
    #[test]
    fn relational_comparison_has_a_third_outcome() {
        assert_eq!(less_than(&n(1.0), &n(2.0)), Ok(Comparison::True));
        assert_eq!(less_than(&n(2.0), &n(1.0)), Ok(Comparison::False));
        assert_eq!(less_than(&n(f64::NAN), &n(1.0)), Ok(Comparison::Undefined));
        assert_eq!(less_than(&n(1.0), &n(f64::NAN)), Ok(Comparison::Undefined));
    }

    /// Two strings compare by CODE UNIT, so `"10" < "9"` is true. Comparing them numerically --
    /// or by locale -- is a different language.
    #[test]
    fn two_strings_compare_by_code_unit_and_everything_else_numerically() {
        assert_eq!(less_than(&s("10"), &s("9")), Ok(Comparison::True), "lexicographic");
        assert_eq!(less_than(&n(10.0), &n(9.0)), Ok(Comparison::False));
        assert_eq!(less_than(&s("10"), &n(9.0)), Ok(Comparison::False), "mixed goes numeric");
        assert_eq!(less_than(&s("a"), &s("b")), Ok(Comparison::True));
    }

    /// THE LAST TIE-BREAK, WHICH RUST'S SHORTEST-ROUND-TRIP DOES NOT SHARE.
    ///
    /// Two seventeen-digit strings can both round-trip to the same `f64`, and the standard says
    /// which one to print: the closest, and at a tie the EVEN one. Rust hands back the odd
    /// candidate. Each value below has an exact expansion one digit longer than its shortest form
    /// ending in a 5 -- which is what makes it a real tie rather than one that looks like one --
    /// and the whole of the difference is the final digit.
    #[test]
    fn a_shortest_form_that_ties_prints_the_even_digit() {
        assert_eq!(number_to_string(0.001_790_046_691_894_531_2), "0.0017900466918945312");
        assert_eq!(number_to_string(0.001_584_053_039_550_781_2), "0.0015840530395507812");
        assert_eq!(number_to_string(0.000_029_802_322_387_695_312), "0.000029802322387695312");
        assert_eq!(number_to_string(-0.001_790_046_691_894_531_2), "-0.0017900466918945312");
        assert_eq!(number_to_string(0.1), "0.1");
        assert_eq!(number_to_string(0.3), "0.3");
        assert_eq!(number_to_string(1.0 / 3.0), "0.3333333333333333");
        assert_eq!(number_to_string(0.125), "0.125");
        assert_eq!(number_to_string(5e-324), "5e-324");
        assert_eq!(number_to_string(f64::MAX), "1.7976931348623157e+308");
        assert_eq!(number_to_string(9_007_199_254_740_991.0), "9007199254740991");
    }

    /// NumberToString is a specified algorithm, not the host's float formatter. The switch to
    /// exponential form is at 21 digits and at 1e-7, wherever the host would have put it.
    #[test]
    fn number_to_string_follows_the_standards_placement_rules() {
        assert_eq!(number_to_string(0.0), "0");
        assert_eq!(number_to_string(-0.0), "0", "-0 prints WITHOUT a sign");
        assert_eq!(number_to_string(f64::NAN), "NaN");
        assert_eq!(number_to_string(f64::INFINITY), "Infinity");
        assert_eq!(number_to_string(f64::NEG_INFINITY), "-Infinity");
        assert_eq!(number_to_string(1.0), "1");
        assert_eq!(number_to_string(-1.5), "-1.5");
        assert_eq!(number_to_string(100.0), "100");
        assert_eq!(number_to_string(0.1), "0.1");
        assert_eq!(number_to_string(1e20), "100000000000000000000");
        assert_eq!(number_to_string(1e21), "1e+21", "the switch is at 21 digits");
        assert_eq!(number_to_string(1e-6), "0.000001");
        assert_eq!(number_to_string(1e-7), "1e-7");
        assert_eq!(number_to_string(1.5e300), "1.5e+300");
    }

    /// The classic float-printing check: the shortest round-tripping digits, not a fixed precision.
    #[test]
    fn number_to_string_prints_the_shortest_round_tripping_digits() {
        assert_eq!(number_to_string(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(number_to_string(1.0 / 3.0), "0.3333333333333333");
    }

    /// The object protocol is a SEAM, and a caller must not be able to mistake it for an answer.
    #[test]
    fn an_object_asks_for_the_protocol_rather_than_getting_a_guess() {
        let object = JsValue::Object(crate::value::ObjectId(1));
        assert_eq!(to_number(&object), Err(NeedsObjectProtocol));
        assert_eq!(to_string(&object), Err(NeedsObjectProtocol));
        assert_eq!(less_than(&object, &n(1.0)), Err(NeedsObjectProtocol));
        assert!(to_boolean(&object));
    }
}
