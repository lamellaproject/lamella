//! The 96-bit `System.Decimal` kernel: the operations whose intermediates exceed 96 bits.

#![no_std]

/// Why a decimal operation could not produce a value.
///
/// The two variants are the two exceptions .NET raises from decimal arithmetic, and a caller maps
/// them straight across: [`Fault::Overflow`] is `OverflowException` and [`Fault::DivideByZero`]
/// is `DivideByZeroException`. Deciding which one applies belongs here, so that both execution
/// tiers raise the same exception for the same operands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The exact result cannot be represented: a magnitude past 96 bits with no fractional
    /// places left to round away, or a value outside the range entirely.
    Overflow,
    /// The divisor is zero, in `divide` or `remainder`.
    DivideByZero,
}

/// The status a C-ABI caller reads when the operation produced a value.
///
/// A compiled program cannot receive a `Result`, so the entry points it links return one of
/// these small integers and write the result through a pointer. The numbering lives here, next
/// to the faults it encodes, so that a caller in either instruction set reads the same word: two
/// copies of the mapping would be two places for a renumbering to land in one of.
pub const STATUS_OK: i32 = 0;

impl Fault {
    /// The status word for this fault, the counterpart of [`STATUS_OK`].
    ///
    /// `1` is `OverflowException` and `2` is `DivideByZeroException`. The value is deliberately
    /// not zero for either, so a caller that only tests for success cannot read a fault as one.
    #[must_use]
    pub fn status(self) -> i32 {
        match self {
            Fault::Overflow => 1,
            Fault::DivideByZero => 2,
        }
    }
}

/// The maximum scale (number of fractional decimal digits) a `Decimal` can carry.
const MAX_SCALE: u32 = 28;
/// The number of decimal places past which a 64-bit intermediate cannot survive: `10^20` is
/// larger than `u64::MAX`, so dropping twenty places from a value that fits 64 bits is zero
/// whatever the value was. .NET's multiply short-circuits there rather than dividing, and the
/// difference is observable -- not in the value, which is zero either way, but in the scale it
/// carries. Dropping nineteen places to zero leaves a zero with 28 places; dropping twenty
/// leaves a bare `0`.
const PLACES_PAST_64_BITS: u32 = 20;
/// The number of 32-bit limbs in the working magnitude: enough for a 96x96 product (192
/// bits = 6 limbs) plus headroom for a rounding carry and intermediate scale-up shifts.
const LIMBS: usize = 8;

/// A `Decimal` decoded into its parts: a 96-bit magnitude (held in the low three limbs of a
/// wider buffer), the base-ten scale, and the sign.
#[derive(Clone, Copy)]
pub struct Dec {
    /// The mantissa magnitude, little-endian 32-bit limbs (only the low three are the value;
    /// the rest are working headroom, zero on a decoded value).
    mag: [u32; LIMBS],
    /// The base-ten scale (`0..=28`): the value is `mag * 10^-scale`.
    scale: u32,
    /// The sign: true for negative.
    negative: bool,
}

impl Dec {
    /// Flips the sign, a zero's included, which is how subtraction reaches [`add`]. .NET's unary
    /// minus flips the sign bit of a zero too (ECMA-335 6th ed IV.5.7 gives a zero result no sign),
    /// so `a - b` and `a + (-b)` agree only if this does: `0m - 0.0m` is a negative zero there, as
    /// `0m + (-0.0m)` is.
    fn negate(&mut self) {
        self.negative = !self.negative;
    }
}

/// Decodes the four inline words of a `Decimal` -- `[lo, mid, hi, flags]`, in the field
/// declaration order the type carries -- into a [`Dec`]. The scale and sign come from
/// `flags` (bits 16..23 and bit 31). An out-of-spec scale (greater than 28) is clamped
/// defensively, though a well-formed `Decimal` never carries one.
pub fn decode(words: [u32; 4]) -> Dec {
    let mut mag = [0u32; LIMBS];
    mag[0] = words[0];
    mag[1] = words[1];
    mag[2] = words[2];
    let scale = (words[3] >> 16) & 0xFF;
    let negative = words[3] & 0x8000_0000 != 0;
    Dec {
        mag,
        scale: scale.min(MAX_SCALE),
        negative,
    }
}

/// Whether the magnitude is zero.
fn is_zero(mag: &[u32; LIMBS]) -> bool {
    mag.iter().all(|&w| w == 0)
}

/// Whether the magnitude needs more than one 32-bit word. .NET's multiply takes a different
/// route for operands that fit a single word, and the two routes disagree about what a zero
/// product looks like.
fn wider_than_32(mag: &[u32; LIMBS]) -> bool {
    mag[1..].iter().any(|&w| w != 0)
}

/// Whether the magnitude exceeds 96 bits (any limb above the low three is set).
fn exceeds_96(mag: &[u32; LIMBS]) -> bool {
    mag[3..].iter().any(|&w| w != 0)
}

/// `mag += other` (limb-wise with carry). Returns the carry out of the top limb (nonzero
/// only on a true overflow past the buffer width, which the callers size against).
fn add_into(mag: &mut [u32; LIMBS], other: &[u32; LIMBS]) -> u32 {
    let mut carry = 0u64;
    for i in 0..LIMBS {
        let sum = u64::from(mag[i]) + u64::from(other[i]) + carry;
        mag[i] = sum as u32;
        carry = sum >> 32;
    }
    carry as u32
}

/// `mag -= other`, assuming `mag >= other` (the caller orders the operands by magnitude).
fn sub_into(mag: &mut [u32; LIMBS], other: &[u32; LIMBS]) {
    let mut borrow = 0i64;
    for i in 0..LIMBS {
        let diff = i64::from(mag[i]) - i64::from(other[i]) - borrow;
        if diff < 0 {
            mag[i] = (diff + (1i64 << 32)) as u32;
            borrow = 1;
        } else {
            mag[i] = diff as u32;
            borrow = 0;
        }
    }
}

/// Compares two magnitudes (`-1`/`0`/`1`), high limb first.
fn cmp_mag(a: &[u32; LIMBS], b: &[u32; LIMBS]) -> i32 {
    for i in (0..LIMBS).rev() {
        if a[i] != b[i] {
            return if a[i] > b[i] { 1 } else { -1 };
        }
    }
    0
}

/// `mag *= factor` (a single 32-bit multiplier). Returns the carry out of the top limb.
fn mul_small(mag: &mut [u32; LIMBS], factor: u32) -> u32 {
    let mut carry = 0u64;
    for limb in mag.iter_mut() {
        let product = u64::from(*limb) * u64::from(factor) + carry;
        *limb = product as u32;
        carry = product >> 32;
    }
    carry as u32
}

/// `mag /= 10`, returning the remainder (`0..=9`). High limb first so the running remainder
/// threads down through the limbs.
fn div10(mag: &mut [u32; LIMBS]) -> u32 {
    let mut remainder = 0u64;
    for i in (0..LIMBS).rev() {
        let cur = (remainder << 32) | u64::from(mag[i]);
        mag[i] = (cur / 10) as u32;
        remainder = cur % 10;
    }
    remainder as u32
}

/// Powers of ten that fit one 32-bit limb (`10^0 .. 10^9`), for scaling a magnitude up by a
/// known number of decimal places in chunks.
const POW10_U32: [u32; 10] = [
    1,
    10,
    100,
    1000,
    10000,
    100_000,
    1_000_000,
    10_000_000,
    100_000_000,
    1_000_000_000,
];

/// Multiplies `mag` by `10^power` in single-limb chunks. Returns false if the product
/// overflows the working buffer (the caller treats that as out of range).
fn scale_up(mag: &mut [u32; LIMBS], mut power: u32) -> bool {
    while power > 0 {
        let chunk = power.min(9);
        if mul_small(mag, POW10_U32[chunk as usize]) != 0 {
            return false;
        }
        power -= chunk;
    }
    true
}

/// Rounds a magnitude down by `drop` decimal places, half-to-even (banker's rounding, what
/// .NET's `DecCalc` uses): divide by ten `drop` times, tracking whether anything below the
/// final digit was nonzero so a tie is broken to even. Returns the carry of a round-up that
/// could grow the magnitude (e.g. 9.5 -> 10).
fn round_off(mag: &mut [u32; LIMBS], drop: u32) {
    if drop == 0 {
        return;
    }
    let mut sticky = false;
    let mut last = 0u32;
    for _ in 0..drop {
        sticky |= last != 0;
        last = div10(mag);
    }
    let round_up = last > 5 || (last == 5 && (sticky || mag[0] & 1 == 1));
    if round_up {
        let mut one = [0u32; LIMBS];
        one[0] = 1;
        add_into(mag, &one);
    }
}

/// Builds the result words `[lo, mid, hi, flags]` from a magnitude, scale, and sign, after
/// rescaling it into the 96-bit / `scale<=28` range half-to-even. A magnitude that still
/// will not fit, or a scale that cannot be reduced enough, is [`Fault::Overflow`].
///
/// The sign is used as given, including for a zero magnitude: .NET's decimal has a signed zero
/// and each operation decides its own, so normalizing one here would overwrite an answer the
/// caller had already got right. The standard says otherwise, and this kernel answers as .NET does:
/// ECMA-335 6th ed IV.5.7's `System.Decimal(Int32[])` remarks give "any zero result" a sign of 0,
/// and ECMA-334 2nd ed 11.1.6 gives decimal no signed zero (4th ed 11.1.7: it "does not
/// necessarily support" one).
fn finish(mut mag: [u32; LIMBS], mut scale: u32, negative: bool) -> Result<[u32; 4], Fault> {
    while exceeds_96(&mag) || scale > MAX_SCALE {
        if scale == 0 {
            return Err(Fault::Overflow);
        }
        let mut drop = scale.saturating_sub(MAX_SCALE);
        let mut probe = mag;
        for _ in 0..drop {
            div10(&mut probe);
        }
        while exceeds_96(&probe) && drop < scale {
            div10(&mut probe);
            drop += 1;
        }
        round_off(&mut mag, drop);
        scale -= drop;
    }
    Ok(encode(&mag, scale, negative))
}

/// Packs a fit magnitude (low three limbs) + scale + sign into the four inline words.
fn encode(mag: &[u32; LIMBS], scale: u32, negative: bool) -> [u32; 4] {
    let flags = (scale << 16) | if negative { 0x8000_0000 } else { 0 };
    [mag[0], mag[1], mag[2], flags]
}

/// Aligns two decoded decimals to a common scale by scaling the lower-scale magnitude up.
/// Returns the common scale, or `None` if scaling up overflowed the working buffer (the
/// callers map that to `OverflowException`).
fn align(a: &mut Dec, b: &mut Dec) -> Option<u32> {
    if a.scale < b.scale {
        if !scale_up(&mut a.mag, b.scale - a.scale) {
            return None;
        }
        a.scale = b.scale;
    } else if b.scale < a.scale {
        if !scale_up(&mut b.mag, a.scale - b.scale) {
            return None;
        }
        b.scale = a.scale;
    }
    Some(a.scale)
}

/// The signed sum `a + b` (used for both addition and subtraction; [`subtract`] flips `b`'s
/// sign first). Aligns scales, then adds same-sign magnitudes or subtracts the smaller from the
/// larger for opposite signs, and finishes (rescaling/rounding) the result.
///
/// An EXACT CANCELLATION -- equal magnitudes with opposite signs -- still has a sign, because
/// .NET's decimal zero carries one, and the sign it carries is the LOWER-SCALED operand's:
/// `-1m + 1m` is a negative zero, `1m + -1m` a positive one, and
/// `1.0000000000000000000000000000m + -1m` a negative one even though the negative operand is on
/// the right. Equal scales make that the left operand's sign, which is the same rule with nothing
/// to choose between.
///
/// A ZERO with fewer places than the other operand is not a cancellation: the sum is the other
/// operand as it stands, its sign and scale included, a zero's too. `0m + -0.0m` is `-0.0`, and
/// `-0.0m + 0m` is `-0.0` as well, where the cancellation rule would take the zero `0m`'s sign.
/// Those are .NET's zero signs; ECMA-335 6th ed IV.5.7 gives a zero result no sign.
pub fn add(mut a: Dec, mut b: Dec) -> Result<[u32; 4], Fault> {
    if a.scale < b.scale && is_zero(&a.mag) {
        return Ok(encode(&b.mag, b.scale, b.negative));
    }
    if b.scale < a.scale && is_zero(&b.mag) {
        return Ok(encode(&a.mag, a.scale, a.negative));
    }
    let cancelled = if a.scale <= b.scale {
        a.negative
    } else {
        b.negative
    };
    let scale = align(&mut a, &mut b).ok_or(Fault::Overflow)?;
    if a.negative == b.negative {
        let mut mag = a.mag;
        add_into(&mut mag, &b.mag);
        let negative = if is_zero(&mag) { cancelled } else { a.negative };
        finish(mag, scale, negative)
    } else {
        match cmp_mag(&a.mag, &b.mag) {
            0 => finish([0u32; LIMBS], scale, cancelled),
            1 => {
                let mut mag = a.mag;
                sub_into(&mut mag, &b.mag);
                finish(mag, scale, a.negative)
            }
            _ => {
                let mut mag = b.mag;
                sub_into(&mut mag, &a.mag);
                finish(mag, scale, b.negative)
            }
        }
    }
}

/// The product `a * b`: the magnitudes multiply (a full 192-bit schoolbook product over the
/// working buffer) and the scales add; `finish` then rescales/rounds into range. .NET caps
/// the product scale at 28, rounding away extra fractional digits, which `finish` does.
pub fn multiply(a: Dec, b: Dec) -> Result<[u32; 4], Fault> {
    let mut product = [0u32; LIMBS];
    for i in 0..3 {
        if a.mag[i] == 0 {
            continue;
        }
        let mut carry = 0u64;
        for j in 0..3 {
            let pos = i + j;
            let cur = u64::from(a.mag[i]) * u64::from(b.mag[j])
                + u64::from(product[pos])
                + carry;
            product[pos] = cur as u32;
            carry = cur >> 32;
        }
        let mut pos = i + 3;
        while carry != 0 && pos < LIMBS {
            let cur = u64::from(product[pos]) + carry;
            product[pos] = cur as u32;
            carry = cur >> 32;
            pos += 1;
        }
        if carry != 0 {
            return Err(Fault::Overflow);
        }
    }
    let negative = a.negative != b.negative;
    let scale = a.scale + b.scale;
    let narrow = !wider_than_32(&a.mag) && !wider_than_32(&b.mag);
    if narrow && !is_zero(&product) && scale.saturating_sub(MAX_SCALE) >= PLACES_PAST_64_BITS {
        return Ok(encode(&[0u32; LIMBS], 0, false));
    }
    if is_zero(&product) {
        if !narrow || scale.saturating_sub(MAX_SCALE) >= PLACES_PAST_64_BITS {
            return Ok(encode(&product, 0, false));
        }
        return Ok(encode(&product, scale.min(MAX_SCALE), negative));
    }
    finish(product, scale, negative)
}

/// The quotient `a / b`, at the scale .NET gives it.
///
/// The result starts at the NATURAL scale, `a.scale - b.scale` (the "preferred scale" of ECMA-334
/// 4th ed 14.7.2), which is what makes an exact division keep its places: `100.00m / 1m` is
/// `100.00`, not `100`. A negative natural scale means the dividend has fewer places than the
/// divisor, and the dividend is scaled up by the difference so the quotient starts at zero places.
///
/// Fractional digits are then appended one at a time -- quotient times ten plus the next digit --
/// until the division terminates, another digit would leave 96 bits, or the scale reaches 28.
/// Whatever tail is left is rounded half-to-even.
///
/// Finally, a quotient that was NOT exact at the natural scale gives up every trailing zero, the
/// operands' own places included, down to none: `2m / 1.15m` is `1.739130434782608695652173913`
/// (27 places) rather than the same value with a 28th place of zero, and
/// `0.9999999999999999999999999999m / 2m`, which rounds to a half, is `0.5`. A quotient that was
/// exact there keeps the natural scale: `1.00m / 2m` is `0.50`. Read literally, ECMA-334 4th ed
/// 14.7.2 and 11.1.7 (2nd ed 14.7.2 and 11.1.6) give the scale of the exact quotient "before any
/// rounding" and then round "to fit the representation", which keeps every place that rounding
/// leaves: `0.9999999999999999999999999999m / 2.0m` would be `0.5000000000000000000000000000`.
/// .NET drops those trailing zeros, and so does this kernel.
///
/// A ZERO DIVIDEND gives a zero at the natural scale, or at none if that is negative, with the sign
/// of the division: `0.000m / 2.5m` is `0.00`, and `-0.0m / 2m` is `-0.0`. A nonzero quotient that
/// underflows past the 28th place is a different zero, with no places at all and only the sign of
/// the division. Those are .NET's zero signs; ECMA-335 6th ed IV.5.7 gives a zero result no sign.
pub fn divide(mut a: Dec, b: Dec) -> Result<[u32; 4], Fault> {
    if is_zero(&b.mag) {
        return Err(Fault::DivideByZero);
    }
    let natural = a.scale as i32 - b.scale as i32;
    if is_zero(&a.mag) {
        let scale = if natural > 0 { natural as u32 } else { 0 };
        return Ok(encode(&a.mag, scale, a.negative != b.negative));
    }
    let mut result_scale = if natural < 0 {
        if !scale_up(&mut a.mag, natural.unsigned_abs()) {
            return Err(Fault::Overflow);
        }
        0
    } else {
        natural as u32
    };
    let divisor = b.mag;
    let (mut quotient, mut remainder) = divmod(&a.mag, &divisor);
    if exceeds_96(&quotient) {
        return Err(Fault::Overflow);
    }
    let exact = is_zero(&remainder);
    while !is_zero(&remainder) && result_scale < MAX_SCALE {
        let mut scaled = remainder;
        if mul_small(&mut scaled, 10) != 0 {
            break;
        }
        let (digit, next) = divmod(&scaled, &divisor);
        let mut shifted = quotient;
        if mul_small(&mut shifted, 10) != 0 {
            break;
        }
        add_into(&mut shifted, &digit);
        if exceeds_96(&shifted) {
            break;
        }
        quotient = shifted;
        remainder = next;
        result_scale += 1;
    }
    if !is_zero(&remainder) {
        let mut twice = remainder;
        let overflow = mul_small(&mut twice, 2) != 0;
        let round_up = if overflow {
            true
        } else {
            let cmp = cmp_mag(&twice, &divisor);
            cmp > 0 || (cmp == 0 && quotient[0] & 1 == 1)
        };
        if round_up {
            let mut one = [0u32; LIMBS];
            one[0] = 1;
            add_into(&mut quotient, &one);
            if exceeds_96(&quotient) {
                if result_scale == 0 {
                    return Err(Fault::Overflow);
                }
                if div10(&mut quotient) >= 5 {
                    add_into(&mut quotient, &one);
                }
                result_scale -= 1;
            }
        }
    }
    let negative = a.negative != b.negative;
    if is_zero(&quotient) {
        return Ok(encode(&quotient, 0, negative));
    }
    let floor = if exact && natural > 0 { natural as u32 } else { 0 };
    while result_scale > floor {
        let mut reduced = quotient;
        if div10(&mut reduced) != 0 {
            break;
        }
        quotient = reduced;
        result_scale -= 1;
    }
    Ok(encode(&quotient, result_scale, negative))
}

/// The remainder `a % b` (.NET's `Decimal.Remainder`): the result has the sign of the dividend,
/// including when it is zero (ECMA-334 2nd ed and 4th ed 14.7.3 give a nonzero result the
/// dividend's sign; a zero's is .NET's, and ECMA-335 6th ed IV.5.7 gives a zero result no sign),
/// and `|a % b| < |b|`.
///
/// A dividend SMALLER in magnitude than the divisor is already the remainder, and .NET returns it
/// untouched -- so `1m % 1.5m` is `1`, not the numerically equal `1.0` that computing it at the
/// common scale would give. That distinction is invisible to a comparison and visible in
/// `ToString`, which is why it is worth the early return rather than a rescale afterwards.
///
/// Otherwise both are brought to the common scale and the magnitude remainder of the integer
/// division is the answer, at that scale.
pub fn remainder(a: Dec, b: Dec) -> Result<[u32; 4], Fault> {
    if is_zero(&b.mag) {
        return Err(Fault::DivideByZero);
    }
    let (mut wide_a, mut wide_b) = (a, b);
    let scale = align(&mut wide_a, &mut wide_b).ok_or(Fault::Overflow)?;
    if cmp_mag(&wide_a.mag, &wide_b.mag) < 0 {
        return Ok(encode(&a.mag, a.scale, a.negative));
    }
    let (_, r) = divmod(&wide_a.mag, &wide_b.mag);
    finish(r, scale, a.negative)
}

/// Compares two decimals by value (`-1`/`0`/`1`), scale-independent: a zero is equal
/// regardless of sign, opposite signs order by sign, and same signs align scales then
/// compare magnitudes (the sign flips the order for negatives). Fails only if a scale
/// alignment overflows the working buffer.
pub fn compare(mut a: Dec, mut b: Dec) -> Result<i32, Fault> {
    let a_zero = is_zero(&a.mag);
    let b_zero = is_zero(&b.mag);
    if a_zero && b_zero {
        return Ok(0);
    }
    if a_zero {
        return Ok(if b.negative { 1 } else { -1 });
    }
    if b_zero {
        return Ok(if a.negative { -1 } else { 1 });
    }
    if a.negative != b.negative {
        return Ok(if a.negative { -1 } else { 1 });
    }
    align(&mut a, &mut b).ok_or(Fault::Overflow)?;
    let mag_cmp = cmp_mag(&a.mag, &b.mag);
    Ok(if a.negative { -mag_cmp } else { mag_cmp })
}

/// Integer division of magnitudes: returns `(quotient, remainder)` with
/// `dividend = quotient*divisor + remainder` and `remainder < divisor`. A restoring
/// bit-at-a-time long division over the working buffer -- exact and simple (the operands are
/// small enough that performance is a non-issue for the differential corpus).
fn divmod(dividend: &[u32; LIMBS], divisor: &[u32; LIMBS]) -> ([u32; LIMBS], [u32; LIMBS]) {
    let mut quotient = [0u32; LIMBS];
    let mut remainder = [0u32; LIMBS];
    let total_bits = LIMBS * 32;
    for bit in (0..total_bits).rev() {
        shl1(&mut remainder);
        let word = bit / 32;
        let off = bit % 32;
        if (dividend[word] >> off) & 1 == 1 {
            remainder[0] |= 1;
        }
        if cmp_mag(&remainder, divisor) >= 0 {
            sub_into(&mut remainder, divisor);
            quotient[word] |= 1 << off;
        }
    }
    (quotient, remainder)
}

/// `mag <<= 1`.
fn shl1(mag: &mut [u32; LIMBS]) {
    let mut carry = 0u32;
    for limb in mag.iter_mut() {
        let new_carry = *limb >> 31;
        *limb = (*limb << 1) | carry;
        carry = new_carry;
    }
}

/// Converts an `f64` to a [`Dec`] by the standard's rule (ECMA-334 2nd ed 13.2.1: "the source value is converted
/// to decimal representation and rounded to the nearest number after the 28th decimal place if required"; 4th ed
/// 13.2.1 says the same). The double's exact binary value keeps its own number of places when that is at most 28
/// and its integer fits 96 bits; otherwise it is rounded once, to the nearest value at the largest scale that
/// fits, a tie going to the even neighbor. A result of zero is `0`, with no places and no sign. NaN, an infinity,
/// and a value whose integer part needs more than 96 bits are [`Fault::Overflow`], the `OverflowException` the
/// managed constructor raises.
#[cfg(feature = "float")]
pub fn from_double(value: f64) -> Result<[u32; 4], Fault> {
    if !value.is_finite() {
        return Err(Fault::Overflow);
    }
    let bits = value.to_bits();
    let negative = bits >> 63 == 1;
    let biased = ((bits >> 52) & 0x7FF) as i32;
    let fraction = bits & ((1u64 << 52) - 1);
    let (mut significand, mut exponent) =
        if biased == 0 { (fraction, -1074) } else { (fraction | (1u64 << 52), biased - 1075) };
    if significand == 0 {
        return Ok([0, 0, 0, 0]);
    }
    while significand & 1 == 0 && exponent < 0 {
        significand >>= 1;
        exponent += 1;
    }
    if exponent >= 0 {
        let width = 64 - significand.leading_zeros() + exponent as u32;
        if width > 96 {
            return Err(Fault::Overflow);
        }
        let whole = u128::from(significand) << exponent;
        let mut mag = [0u32; LIMBS];
        mag[0] = whole as u32;
        mag[1] = (whole >> 32) as u32;
        mag[2] = (whole >> 64) as u32;
        return Ok(encode(&mag, 0, negative));
    }
    let places = (-exponent) as u32;
    if places > 147 {
        return Ok([0, 0, 0, 0]);
    }
    let mut scale = places.min(MAX_SCALE);
    loop {
        let mut mag = [0u32; LIMBS];
        mag[0] = significand as u32;
        mag[1] = (significand >> 32) as u32;
        let fits = scale_up(&mut mag, scale);
        debug_assert!(fits, "53 bits times 10^28 fit the buffer");
        round_shift_right(&mut mag, places);
        if !exceeds_96(&mag) {
            if is_zero(&mag) {
                return Ok([0, 0, 0, 0]);
            }
            return Ok(encode(&mag, scale, negative));
        }
        if scale == 0 {
            return Err(Fault::Overflow);
        }
        scale -= 1;
    }
}

/// Shifts a magnitude right by `bits`, rounding to the nearest, a tie to even (`bits` below the buffer's width).
#[cfg(feature = "float")]
fn round_shift_right(mag: &mut [u32; LIMBS], bits: u32) {
    if bits == 0 {
        return;
    }
    let bit = |m: &[u32; LIMBS], i: u32| (m[(i / 32) as usize] >> (i % 32)) & 1 == 1;
    let half = bit(mag, bits - 1);
    let mut below = false;
    for i in 0..bits - 1 {
        if bit(mag, i) {
            below = true;
            break;
        }
    }
    let words = (bits / 32) as usize;
    let shift = bits % 32;
    for i in 0..LIMBS {
        let low = if i + words < LIMBS { mag[i + words] } else { 0 };
        let high = if i + words + 1 < LIMBS { mag[i + words + 1] } else { 0 };
        mag[i] = if shift == 0 { low } else { (low >> shift) | (high << (32 - shift)) };
    }
    if half && (below || mag[0] & 1 == 1) {
        let mut one = [0u32; LIMBS];
        one[0] = 1;
        add_into(mag, &one);
    }
}

/// Converts a decoded decimal to the nearest `f64`, a tie going to the even neighbor (ECMA-334 2nd ed 13.2.1:
/// "the decimal value is rounded to the nearest double or float value"; 4th ed 13.2.1 says the same).
#[cfg(feature = "float")]
pub fn to_double(a: Dec) -> f64 {
    let sign = if a.negative { 1u64 << 63 } else { 0 };
    match nearest_binary(&a, 53) {
        Some((significand, exponent)) => {
            let biased = (exponent + 52 + 1023) as u64;
            f64::from_bits(sign | (biased << 52) | (significand as u64 & ((1u64 << 52) - 1)))
        }
        None => f64::from_bits(sign),
    }
}

/// The nearest binary floating-point value to a decimal, with `width` significant bits: `Some((q, e))` where
/// `q` has exactly `width` bits and the value is `q * 2^e`, a tie going to even; `None` for a zero.
#[cfg(feature = "float")]
fn nearest_binary(a: &Dec, width: u32) -> Option<(u128, i32)> {
    let numerator =
        u128::from(a.mag[0]) | (u128::from(a.mag[1]) << 32) | (u128::from(a.mag[2]) << 64);
    if numerator == 0 {
        return None;
    }
    let divisor = 10u128.pow(a.scale);
    let bit_length = |x: u128| 128 - x.leading_zeros();
    let wanted = width + 1;
    let mut quotient = numerator / divisor;
    let mut remainder = numerator % divisor;
    let mut exponent = 0i32;
    let sticky;
    if bit_length(quotient) > wanted {
        let drop = bit_length(quotient) - wanted;
        sticky = quotient & ((1u128 << drop) - 1) != 0 || remainder != 0;
        quotient >>= drop;
        exponent = drop as i32;
    } else {
        while bit_length(quotient) < wanted {
            remainder <<= 1;
            quotient <<= 1;
            if remainder >= divisor {
                remainder -= divisor;
                quotient |= 1;
            }
            exponent -= 1;
        }
        sticky = remainder != 0;
    }
    let round = quotient & 1 == 1;
    quotient >>= 1;
    exponent += 1;
    if round && (sticky || quotient & 1 == 1) {
        quotient += 1;
        if bit_length(quotient) > width {
            quotient >>= 1;
            exponent += 1;
        }
    }
    Some((quotient, exponent))
}

/// The signed difference `a - b`, which is `a + (-b)`: the sign of the subtrahend is flipped and
/// the result goes through [`add`], so subtraction and addition share one alignment, one rounding
/// decision, and one overflow test rather than agreeing by construction.
pub fn subtract(a: Dec, mut b: Dec) -> Result<[u32; 4], Fault> {
    b.negate();
    add(a, b)
}

#[cfg(all(test, feature = "float"))]
mod tests {
    use super::{Fault, from_double};

    #[test]
    fn from_double_refuses_two_to_the_96th_and_accepts_the_double_below() {
        let boundary = (1u128 << 96) as f64;
        for value in [boundary, f64::from_bits(boundary.to_bits() + 1)] {
            for signed in [value, -value] {
                assert_eq!(from_double(signed), Err(Fault::Overflow), "{signed:e}");
            }
        }
        let below = f64::from_bits(boundary.to_bits() - 1);
        assert!(from_double(below).is_ok());
        assert!(from_double(-below).is_ok());
    }
}
