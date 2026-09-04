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
    /// Flips the sign, which is how subtraction reaches [`add`].
    fn negate(&mut self) {
        if !is_zero(&self.mag) {
            self.negative = !self.negative;
        }
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
/// caller had already got right.
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
        let dropped_past_64_bits = drop >= PLACES_PAST_64_BITS;
        round_off(&mut mag, drop);
        scale -= drop;
        if is_zero(&mag) && dropped_past_64_bits {
            return Ok(encode(&[0u32; LIMBS], 0, false));
        }
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
/// `-1m + 1m` is a negative zero, `1m + -1m` a positive one, and `1.0000000000000000000000000000m
/// + -1m` a negative one even though the negative operand is on the right. Equal scales make that
/// the left operand's sign, which is the same rule with nothing to choose between.
pub fn add(mut a: Dec, mut b: Dec) -> Result<[u32; 4], Fault> {
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
    if is_zero(&product) {
        if wider_than_32(&a.mag) || wider_than_32(&b.mag) {
            return Ok(encode(&product, 0, false));
        }
        let scale = (a.scale + b.scale).min(MAX_SCALE);
        return Ok(encode(&product, scale, negative));
    }
    finish(product, a.scale + b.scale, negative)
}

/// The quotient `a / b`, at the scale .NET gives it.
///
/// The result starts at the NATURAL scale, `a.scale - b.scale`, which is what makes an exact
/// division keep its places: `100.00m / 1m` is `100.00`, not `100`. A negative natural scale
/// means the dividend has fewer places than the divisor, and the dividend is scaled up by the
/// difference so the quotient starts at zero places.
///
/// Fractional digits are then appended one at a time -- quotient times ten plus the next digit --
/// until the division terminates, another digit would leave 96 bits, or the scale reaches 28.
/// Whatever tail is left is rounded half-to-even.
///
/// Finally the APPENDED digits are unwound while they are zeros, which is why `2m / 1.15m` is
/// `1.739130434782608695652173913` (27 places) rather than the same value with a 28th place of
/// zero. The unwinding stops at the natural scale, so it never eats a place the operands had.
///
/// A quotient that comes out zero -- an exact zero dividend, or a value that underflows past the
/// 28th place -- carries no places at all, only the sign of the division.
pub fn divide(mut a: Dec, b: Dec) -> Result<[u32; 4], Fault> {
    if is_zero(&b.mag) {
        return Err(Fault::DivideByZero);
    }
    let natural = a.scale as i32 - b.scale as i32;
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
                return Err(Fault::Overflow);
            }
        }
    }
    let negative = a.negative != b.negative;
    if is_zero(&quotient) {
        return Ok(encode(&quotient, 0, negative));
    }
    let floor = if natural > 0 { natural as u32 } else { 0 };
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
/// including when it is zero, and `|a % b| < |b|`.
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

/// Converts an `f64` to a [`Dec`] the way .NET's `Decimal(double)` ctor does: round the
/// value to 15 significant decimal digits (double's reliable precision), then express that as
/// a 96-bit mantissa with the matching scale. NaN, an infinity, or a magnitude outside the
/// `Decimal` range is [`Fault::Overflow`], which is the `OverflowException` the managed
/// constructor raises.
#[cfg(feature = "float")]
pub fn from_double(value: f64) -> Result<[u32; 4], Fault> {
    if !value.is_finite() {
        return Err(Fault::Overflow);
    }
    if value == 0.0 {
        return finish([0u32; LIMBS], 0, false);
    }
    let negative = value < 0.0;
    let magnitude = value.abs();
    let mut exp = floor_log10(magnitude);
    let mut digits15 = magnitude / pow10_f64(exp - 14);
    if digits15 >= 1e15 {
        exp += 1;
        digits15 = magnitude / pow10_f64(exp - 14);
    } else if digits15 < 1e14 {
        exp -= 1;
        digits15 = magnitude / pow10_f64(exp - 14);
    }
    let rounded = round_half_even_f64(digits15);
    let mut int_digits = rounded as u128;
    if int_digits == 0 {
        return finish([0u32; LIMBS], 0, false);
    }
    let mut scale_pow = exp - 14;
    while int_digits % 10 == 0 {
        int_digits /= 10;
        scale_pow += 1;
    }
    let mut mag = [0u32; LIMBS];
    mag[0] = int_digits as u32;
    mag[1] = (int_digits >> 32) as u32;
    mag[2] = (int_digits >> 64) as u32;
    let words = if scale_pow >= 0 {
        if !scale_up(&mut mag, scale_pow as u32) {
            return Err(Fault::Overflow);
        }
        finish(mag, 0, negative)?
    } else {
        finish(mag, (-scale_pow) as u32, negative)?
    };
    if words[0] | words[1] | words[2] == 0 {
        return Ok([0, 0, 0, 0]);
    }
    Ok(words)
}

/// `floor(log10(x))` for a finite positive `x`, without a math library: bracket the value
/// between consecutive integer powers of ten (the range of magnitudes is small).
#[cfg(feature = "float")]
fn floor_log10(x: f64) -> i32 {
    let mut exp = 0i32;
    let mut v = x;
    while v >= 10.0 {
        v /= 10.0;
        exp += 1;
    }
    while v < 1.0 {
        v *= 10.0;
        exp -= 1;
    }
    exp
}

/// `10^n` as an `f64` for a small signed exponent, by repeated multiply/divide (exact for the
/// |n| <= ~22 range where 10^n is representable exactly in a double; good enough beyond).
#[cfg(feature = "float")]
fn pow10_f64(n: i32) -> f64 {
    let mut result = 1.0f64;
    let mut k = n.abs();
    while k > 0 {
        result *= 10.0;
        k -= 1;
    }
    if n < 0 { 1.0 / result } else { result }
}

/// Rounds an `f64` to the nearest integer, ties to even.
#[cfg(feature = "float")]
fn round_half_even_f64(x: f64) -> f64 {
    let floor = floor_f64(x);
    let frac = x - floor;
    if frac < 0.5 {
        floor
    } else if frac > 0.5 {
        floor + 1.0
    } else if (floor as i64) & 1 == 0 {
        floor
    } else {
        floor + 1.0
    }
}

/// `floor(x)` for a non-negative `x` within the i64 range (the 15-digit integers here),
/// without a math library.
#[cfg(feature = "float")]
fn floor_f64(x: f64) -> f64 {
    let truncated = x as i64 as f64;
    if truncated > x { truncated - 1.0 } else { truncated }
}

/// Converts a decoded decimal to the nearest `f64` (.NET's `(double)dec` operator): the
/// 96-bit mantissa as a float divided by `10^scale`. Double's rounding gives the same
/// result as .NET here for the values the corpus exercises.
#[cfg(feature = "float")]
pub fn to_double(a: Dec) -> f64 {
    let mantissa =
        u128::from(a.mag[0]) | (u128::from(a.mag[1]) << 32) | (u128::from(a.mag[2]) << 64);
    let mut value = mantissa as f64;
    value /= pow10_f64(a.scale as i32);
    if a.negative {
        -value
    } else {
        value
    }
}

/// The signed difference `a - b`, which is `a + (-b)`: the sign of the subtrahend is flipped and
/// the result goes through [`add`], so subtraction and addition share one alignment, one rounding
/// decision, and one overflow test rather than agreeing by construction.
pub fn subtract(a: Dec, mut b: Dec) -> Result<[u32; 4], Fault> {
    b.negate();
    add(a, b)
}
