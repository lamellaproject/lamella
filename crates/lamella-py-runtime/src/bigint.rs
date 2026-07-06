//! Arbitrary-precision integers -- Python's `int` beyond the i128 `long` range.

use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

/// A signed arbitrary-precision integer. `mag` is the magnitude in base-2^32 little-endian limbs
/// with no trailing zeros; `negative` is the sign (always `false` for zero).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BigInt {
    negative: bool,
    mag: Vec<u32>,
}

impl BigInt {
    /// The value zero.
    #[must_use]
    pub fn zero() -> BigInt {
        BigInt { negative: false, mag: Vec::new() }
    }

    /// Whether this is zero.
    #[must_use]
    pub fn is_zero(&self) -> bool {
        self.mag.is_empty()
    }

    /// Whether this is negative (zero is not).
    #[must_use]
    pub fn is_negative(&self) -> bool {
        self.negative
    }

    /// Builds a `BigInt` from an `i128` -- the promotion path from the fixnum / i128-`long` tiers.
    #[must_use]
    pub fn from_i128(n: i128) -> BigInt {
        if n == 0 {
            return BigInt::zero();
        }
        let mut magnitude = n.unsigned_abs();
        let mut mag = Vec::new();
        while magnitude != 0 {
            mag.push((magnitude & 0xFFFF_FFFF) as u32);
            magnitude >>= 32;
        }
        BigInt { negative: n < 0, mag }
    }

    /// Narrows to an `i128` if the value fits (so a result that fell back to `i128` range
    /// re-normalizes to the `long` tier); `None` if the magnitude exceeds the `i128` range.
    #[must_use]
    pub fn to_i128(&self) -> Option<i128> {
        if self.mag.len() > 4 {
            return None;
        }
        let mut magnitude: u128 = 0;
        for (i, &limb) in self.mag.iter().enumerate() {
            magnitude |= u128::from(limb) << (i * 32);
        }
        if self.negative {
            if magnitude <= (i128::MAX as u128) + 1 {
                Some((magnitude as i128).wrapping_neg())
            } else {
                None
            }
        } else if magnitude <= i128::MAX as u128 {
            Some(magnitude as i128)
        } else {
            None
        }
    }

    /// The nearest `f64` (for mixing with floats and float comparison). Exact within 53 bits; a
    /// larger magnitude rounds (or overflows to infinity), as CPython's int->float does.
    #[must_use]
    pub fn to_f64(&self) -> f64 {
        let mut value = 0.0f64;
        for &limb in self.mag.iter().rev() {
            value = value * 4_294_967_296.0 + f64::from(limb);
        }
        if self.negative {
            -value
        } else {
            value
        }
    }

    /// Orders two values (sign first, then magnitude).
    #[must_use]
    pub fn cmp(&self, other: &BigInt) -> Ordering {
        match (self.negative, other.negative) {
            (false, true) => Ordering::Greater,
            (true, false) => Ordering::Less,
            (false, false) => mag_cmp(&self.mag, &other.mag),
            (true, true) => mag_cmp(&other.mag, &self.mag),
        }
    }

    /// The negation (`-self`); negating zero stays zero.
    #[must_use]
    pub fn neg(&self) -> BigInt {
        if self.is_zero() {
            BigInt::zero()
        } else {
            BigInt { negative: !self.negative, mag: self.mag.clone() }
        }
    }

    /// The absolute value.
    #[must_use]
    pub fn abs(&self) -> BigInt {
        BigInt { negative: false, mag: self.mag.clone() }
    }

    /// `self + other`.
    #[must_use]
    pub fn add(&self, other: &BigInt) -> BigInt {
        if self.negative == other.negative {
            normalized(self.negative, mag_add(&self.mag, &other.mag))
        } else {
            match mag_cmp(&self.mag, &other.mag) {
                Ordering::Equal => BigInt::zero(),
                Ordering::Greater => normalized(self.negative, mag_sub(&self.mag, &other.mag)),
                Ordering::Less => normalized(other.negative, mag_sub(&other.mag, &self.mag)),
            }
        }
    }

    /// `self - other`.
    #[must_use]
    pub fn sub(&self, other: &BigInt) -> BigInt {
        self.add(&other.neg())
    }

    /// `self * other`.
    #[must_use]
    pub fn mul(&self, other: &BigInt) -> BigInt {
        normalized(self.negative != other.negative, mag_mul(&self.mag, &other.mag))
    }

    /// Python's `divmod`: the pair `(self // other, self % other)`, where `//` floors toward
    /// negative infinity and `%` takes the DIVISOR's sign (`-7 // 2 == -4`, `-7 % 2 == 1`). `None`
    /// when `other` is zero (the caller raises `ZeroDivisionError`).
    #[must_use]
    pub fn divmod(&self, other: &BigInt) -> Option<(BigInt, BigInt)> {
        if other.is_zero() {
            return None;
        }
        let (q_mag, r_mag) = mag_divmod(&self.mag, &other.mag);
        let quotient_negative = self.negative != other.negative;
        let quotient = normalized(quotient_negative, q_mag);
        let remainder = normalized(self.negative, r_mag);
        if remainder.is_zero() {
            return Some((quotient, remainder));
        }
        if self.negative != other.negative {
            Some((quotient.sub(&BigInt::from_i128(1)), remainder.add(other)))
        } else {
            Some((quotient, remainder))
        }
    }

    /// `self << bits` (multiply by `2^bits`); the sign is unchanged.
    #[must_use]
    pub fn shl(&self, bits: u64) -> BigInt {
        if self.is_zero() || bits == 0 {
            return self.clone();
        }
        let limb_shift = (bits / 32) as usize;
        let bit_shift = (bits % 32) as u32;
        let mut mag = alloc::vec![0u32; limb_shift];
        mag.extend_from_slice(&mag_shl_bits(&self.mag, bit_shift));
        normalized(self.negative, mag)
    }

    /// `self >> bits` -- an ARITHMETIC right shift (floor division by `2^bits`, so a negative value
    /// rounds toward negative infinity: `-7 >> 1 == -4`).
    #[must_use]
    pub fn shr(&self, bits: u64) -> BigInt {
        if bits == 0 {
            return self.clone();
        }
        let limb_shift = (bits / 32) as usize;
        let bit_shift = (bits % 32) as u32;
        if limb_shift >= self.mag.len() {
            return if self.negative { BigInt::from_i128(-1) } else { BigInt::zero() };
        }
        let shifted = mag_shr_bits(&self.mag[limb_shift..], bit_shift);
        if !self.negative {
            return normalized(false, shifted);
        }
        let lost = self.mag[..limb_shift].iter().any(|&limb| limb != 0)
            || (bit_shift > 0 && self.mag[limb_shift] & ((1u32 << bit_shift) - 1) != 0);
        let result = normalized(true, shifted);
        if lost {
            result.sub(&BigInt::from_i128(1))
        } else {
            result
        }
    }

    /// `self & other`, over Python's infinite two's-complement model (`-5 & 3 == 3`).
    #[must_use]
    pub fn bitand(&self, other: &BigInt) -> BigInt {
        self.bitwise(other, |a, b| a & b)
    }

    /// `self | other`, over Python's infinite two's-complement model (`-5 | 3 == -5`).
    #[must_use]
    pub fn bitor(&self, other: &BigInt) -> BigInt {
        self.bitwise(other, |a, b| a | b)
    }

    /// `self ^ other`, over Python's infinite two's-complement model.
    #[must_use]
    pub fn bitxor(&self, other: &BigInt) -> BigInt {
        self.bitwise(other, |a, b| a ^ b)
    }

    /// Applies a bitwise operator over the two operands' infinite two's-complement forms, then
    /// converts the result back to sign-magnitude. The `+1` guard limb ensures a positive value
    /// keeps a clear sign bit (and a negative one a set sign bit) after sign extension.
    fn bitwise(&self, other: &BigInt, op: impl Fn(u32, u32) -> u32) -> BigInt {
        let len = self.mag.len().max(other.mag.len()) + 1;
        let a = self.twos_complement(len);
        let b = other.twos_complement(len);
        let result: Vec<u32> = (0..len).map(|i| op(a[i], b[i])).collect();
        if result[len - 1] & 0x8000_0000 != 0 {
            normalized(true, twos_complement_to_magnitude(&result))
        } else {
            normalized(false, result)
        }
    }

    /// This value's two's-complement over exactly `len` limbs (`len` exceeds the magnitude length, so
    /// the sign is captured): the magnitude as-is when non-negative, its negation when negative.
    fn twos_complement(&self, len: usize) -> Vec<u32> {
        let mut limbs = alloc::vec![0u32; len];
        limbs[..self.mag.len()].copy_from_slice(&self.mag);
        if self.negative {
            let mut carry = 1u64;
            for limb in &mut limbs {
                let value = u64::from(!*limb) + carry;
                *limb = value as u32;
                carry = value >> 32;
            }
        }
        limbs
    }

    /// The decimal string (`-` prefix when negative), e.g. `"123456789012345678901234567890"`.
    #[must_use]
    pub fn to_decimal_string(&self) -> String {
        if self.is_zero() {
            return String::from("0");
        }
        let mut mag = self.mag.clone();
        let mut chunks = Vec::new();
        while !mag.is_empty() {
            let (quotient, remainder) = mag_divmod_small(&mag, 1_000_000_000);
            chunks.push(remainder);
            mag = quotient;
        }
        let mut out = String::new();
        if self.negative {
            out.push('-');
        }
        let mut chunks = chunks.iter().rev();
        if let Some(&most_significant) = chunks.next() {
            out.push_str(&alloc::format!("{most_significant}"));
        }
        for &chunk in chunks {
            out.push_str(&alloc::format!("{chunk:09}"));
        }
        out
    }

    /// Parses a decimal string: an optional leading `+`/`-` then ASCII digits. The caller strips
    /// surrounding whitespace and `_` separators first. `None` for any non-digit content.
    #[must_use]
    pub fn from_decimal_str(s: &str) -> Option<BigInt> {
        let (negative, digits) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut mag: Vec<u32> = Vec::new();
        let bytes = digits.as_bytes();
        let first_len = match bytes.len() % 9 {
            0 => 9,
            n => n,
        };
        let mut pos = 0;
        while pos < bytes.len() {
            let len = if pos == 0 { first_len } else { 9 };
            let chunk: u32 = digits[pos..pos + len].parse().ok()?;
            mag = mag_add_small(&mag_mul_small(&mag, 10u32.pow(len as u32)), chunk);
            pos += len;
        }
        Some(normalized(negative, mag))
    }
}

/// Trims trailing-zero limbs, and forces a zero magnitude to a non-negative sign (one form per value).
fn normalized(negative: bool, mut mag: Vec<u32>) -> BigInt {
    while mag.last() == Some(&0) {
        mag.pop();
    }
    BigInt { negative: negative && !mag.is_empty(), mag }
}

/// Orders two normalized magnitudes: the longer is larger; equal lengths compare from the top limb.
fn mag_cmp(a: &[u32], b: &[u32]) -> Ordering {
    a.len().cmp(&b.len()).then_with(|| a.iter().rev().cmp(b.iter().rev()))
}

/// Adds two magnitudes.
fn mag_add(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(a.len().max(b.len()) + 1);
    let mut carry = 0u64;
    for i in 0..a.len().max(b.len()) {
        let sum = carry + u64::from(a.get(i).copied().unwrap_or(0)) + u64::from(b.get(i).copied().unwrap_or(0));
        result.push((sum & 0xFFFF_FFFF) as u32);
        carry = sum >> 32;
    }
    if carry != 0 {
        result.push(carry as u32);
    }
    result
}

/// Subtracts magnitude `b` from `a`; the precondition is `a >= b`.
fn mag_sub(a: &[u32], b: &[u32]) -> Vec<u32> {
    let mut result = Vec::with_capacity(a.len());
    let mut borrow = 0i64;
    for i in 0..a.len() {
        let diff = i64::from(a[i]) - i64::from(b.get(i).copied().unwrap_or(0)) - borrow;
        if diff < 0 {
            result.push((diff + (1i64 << 32)) as u32);
            borrow = 1;
        } else {
            result.push(diff as u32);
            borrow = 0;
        }
    }
    while result.last() == Some(&0) {
        result.pop();
    }
    result
}

/// Multiplies two magnitudes (schoolbook long multiplication).
fn mag_mul(a: &[u32], b: &[u32]) -> Vec<u32> {
    if a.is_empty() || b.is_empty() {
        return Vec::new();
    }
    let mut result = alloc::vec![0u32; a.len() + b.len()];
    for (i, &ai) in a.iter().enumerate() {
        let mut carry = 0u64;
        for (j, &bj) in b.iter().enumerate() {
            let cur = u64::from(result[i + j]) + u64::from(ai) * u64::from(bj) + carry;
            result[i + j] = (cur & 0xFFFF_FFFF) as u32;
            carry = cur >> 32;
        }
        result[i + b.len()] = carry as u32;
    }
    while result.last() == Some(&0) {
        result.pop();
    }
    result
}

/// Multiplies a magnitude by a small (`u32`) factor.
fn mag_mul_small(mag: &[u32], factor: u32) -> Vec<u32> {
    let mut result = Vec::with_capacity(mag.len() + 1);
    let mut carry = 0u64;
    for &limb in mag {
        let cur = u64::from(limb) * u64::from(factor) + carry;
        result.push((cur & 0xFFFF_FFFF) as u32);
        carry = cur >> 32;
    }
    if carry != 0 {
        result.push(carry as u32);
    }
    result
}

/// Adds a small (`u32`) addend to a magnitude.
fn mag_add_small(mag: &[u32], addend: u32) -> Vec<u32> {
    let mut result = mag.to_vec();
    let mut carry = u64::from(addend);
    let mut i = 0;
    while carry != 0 {
        if i < result.len() {
            let cur = u64::from(result[i]) + carry;
            result[i] = (cur & 0xFFFF_FFFF) as u32;
            carry = cur >> 32;
        } else {
            result.push(carry as u32);
            carry = 0;
        }
        i += 1;
    }
    result
}

/// Converts a NEGATIVE two's-complement limb sequence (its high bit set) back to a magnitude by
/// negating it (`~limbs + 1`).
fn twos_complement_to_magnitude(limbs: &[u32]) -> Vec<u32> {
    let mut mag = alloc::vec![0u32; limbs.len()];
    let mut carry = 1u64;
    for (i, &limb) in limbs.iter().enumerate() {
        let value = u64::from(!limb) + carry;
        mag[i] = value as u32;
        carry = value >> 32;
    }
    while mag.last() == Some(&0) {
        mag.pop();
    }
    mag
}

/// Shifts a magnitude LEFT by `bits` (0..32), growing it by at most one limb.
fn mag_shl_bits(mag: &[u32], bits: u32) -> Vec<u32> {
    if bits == 0 {
        return mag.to_vec();
    }
    let mut result = Vec::with_capacity(mag.len() + 1);
    let mut carry = 0u32;
    for &limb in mag {
        result.push((limb << bits) | carry);
        carry = limb >> (32 - bits);
    }
    if carry != 0 {
        result.push(carry);
    }
    result
}

/// Shifts a magnitude RIGHT by `bits` (0..32).
fn mag_shr_bits(mag: &[u32], bits: u32) -> Vec<u32> {
    if bits == 0 {
        return mag.to_vec();
    }
    let mut result = alloc::vec![0u32; mag.len()];
    let mut carry = 0u32;
    for i in (0..mag.len()).rev() {
        result[i] = (mag[i] >> bits) | carry;
        carry = mag[i] << (32 - bits);
    }
    while result.last() == Some(&0) {
        result.pop();
    }
    result
}

/// Divides magnitude `a` by non-zero magnitude `b` (Knuth's Algorithm D, TAOCP 4.3.1), returning
/// `(quotient, remainder)`, both normalized. A single-limb divisor uses the small path.
fn mag_divmod(a: &[u32], b: &[u32]) -> (Vec<u32>, Vec<u32>) {
    if mag_cmp(a, b) == Ordering::Less {
        return (Vec::new(), a.to_vec());
    }
    if b.len() == 1 {
        let (quotient, remainder) = mag_divmod_small(a, b[0]);
        return (quotient, if remainder == 0 { Vec::new() } else { alloc::vec![remainder] });
    }
    let n = b.len();
    let m = a.len() - n;
    let base = 1u64 << 32;
    let shift = b[n - 1].leading_zeros();
    let v = mag_shl_bits(b, shift);
    let mut u = mag_shl_bits(a, shift);
    u.resize(a.len() + 1, 0);
    let mut q = alloc::vec![0u32; m + 1];
    for j in (0..=m).rev() {
        let numerator = (u64::from(u[j + n]) << 32) | u64::from(u[j + n - 1]);
        let mut qhat = numerator / u64::from(v[n - 1]);
        let mut rhat = numerator % u64::from(v[n - 1]);
        while qhat >= base
            || u128::from(qhat) * u128::from(v[n - 2]) > (u128::from(rhat) << 32) | u128::from(u[j + n - 2])
        {
            qhat -= 1;
            rhat += u64::from(v[n - 1]);
            if rhat >= base {
                break;
            }
        }
        let mut borrow: i64 = 0;
        let mut carry: u64 = 0;
        for i in 0..n {
            let product = qhat * u64::from(v[i]) + carry;
            carry = product >> 32;
            let diff = i64::from(u[j + i]) + borrow - (product & 0xFFFF_FFFF) as i64;
            u[j + i] = (diff as u64 & 0xFFFF_FFFF) as u32;
            borrow = diff >> 32;
        }
        let diff = i64::from(u[j + n]) + borrow - carry as i64;
        u[j + n] = (diff as u64 & 0xFFFF_FFFF) as u32;
        if diff < 0 {
            qhat -= 1;
            let mut carry = 0u64;
            for i in 0..n {
                let sum = u64::from(u[j + i]) + u64::from(v[i]) + carry;
                u[j + i] = sum as u32;
                carry = sum >> 32;
            }
            u[j + n] = (u64::from(u[j + n]) + carry) as u32;
        }
        q[j] = qhat as u32;
    }
    while q.last() == Some(&0) {
        q.pop();
    }
    let remainder = mag_shr_bits(&u[0..n], shift);
    (q, remainder)
}

/// Divides a magnitude by a small (`u32`) divisor, returning `(quotient, remainder)`.
fn mag_divmod_small(mag: &[u32], divisor: u32) -> (Vec<u32>, u32) {
    let mut quotient = alloc::vec![0u32; mag.len()];
    let mut remainder = 0u64;
    for i in (0..mag.len()).rev() {
        let cur = (remainder << 32) | u64::from(mag[i]);
        quotient[i] = (cur / u64::from(divisor)) as u32;
        remainder = cur % u64::from(divisor);
    }
    while quotient.last() == Some(&0) {
        quotient.pop();
    }
    (quotient, remainder as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(s: &str) -> BigInt {
        BigInt::from_decimal_str(s).unwrap()
    }

    #[test]
    fn i128_round_trips_and_narrows() {
        for &n in &[0i128, 1, -1, 42, -42, i128::MAX, i128::MIN, 1 << 100, -(1 << 100)] {
            let b = BigInt::from_i128(n);
            assert_eq!(b.to_i128(), Some(n), "round trip {n}");
        }
        let over = BigInt::from_i128(i128::MAX).add(&BigInt::from_i128(1));
        assert_eq!(over.to_i128(), None);
    }

    #[test]
    fn decimal_round_trips() {
        let samples = [
            "0", "1", "-1", "9", "10", "255", "4294967296", "-4294967296",
            "1000000000", "999999999", "123456789012345678901234567890",
            "-123456789012345678901234567890",
            "170141183460469231731687303715884105728",
        ];
        for s in samples {
            assert_eq!(big(s).to_decimal_string(), s, "decimal round trip {s}");
        }
        assert_eq!(big("007").to_decimal_string(), "7");
        assert_eq!(big("-0").to_decimal_string(), "0");
        assert_eq!(BigInt::from_decimal_str("12x"), None);
        assert_eq!(BigInt::from_decimal_str(""), None);
    }

    #[test]
    fn add_sub_mul_match_reference() {
        let a = big("123456789012345678901234567890");
        let b = big("987654321098765432109876543210");
        assert_eq!(a.add(&b).to_decimal_string(), "1111111110111111111011111111100");
        assert_eq!(b.sub(&a).to_decimal_string(), "864197532086419753208641975320");
        assert_eq!(a.sub(&b).to_decimal_string(), "-864197532086419753208641975320");
        assert_eq!(
            a.mul(&b).to_decimal_string(),
            "121932631137021795226185032733622923332237463801111263526900"
        );
        assert_eq!(a.neg().mul(&b).to_decimal_string(), a.mul(&b).neg().to_decimal_string());
        assert_eq!(a.neg().mul(&b.neg()).to_decimal_string(), a.mul(&b).to_decimal_string());
        assert_eq!(a.add(&a.neg()), BigInt::zero());
        assert!(a.add(&a.neg()).is_zero());
        let mut two_pow = BigInt::from_i128(1);
        let two = BigInt::from_i128(2);
        for _ in 0..128 {
            two_pow = two_pow.mul(&two);
        }
        assert_eq!(two_pow.to_decimal_string(), "340282366920938463463374607431768211456");
    }

    #[test]
    fn divmod_matches_python_floor_semantics() {
        let cases = [
            ("100000000000000000000000000000000000000000", "7", "14285714285714285714285714285714285714285", "5"),
            ("1000000000000000000000000000000", "999999999999999", "1000000000000001", "1"),
            ("-100000000000000000000000000000000000000000", "7", "-14285714285714285714285714285714285714286", "2"),
            ("100000000000000000000000000000000000000000", "-7", "-14285714285714285714285714285714285714286", "-2"),
            ("-100000000000000000000000000000000000000000", "-7", "14285714285714285714285714285714285714285", "-5"),
            ("42", "5", "8", "2"),
            ("12345678901234567890123456789", "12345678901234567890123456789", "1", "0"),
        ];
        for (a, b, fdiv, m) in cases {
            let (q, r) = big(a).divmod(&big(b)).unwrap();
            assert_eq!(q.to_decimal_string(), fdiv, "{a} // {b}");
            assert_eq!(r.to_decimal_string(), m, "{a} % {b}");
            assert_eq!(q.mul(&big(b)).add(&r), big(a), "identity for {a} / {b}");
        }
        assert!(big("5").divmod(&BigInt::zero()).is_none());
    }

    #[test]
    fn shifts_and_bitwise_match_python() {
        let big_pos = big("1000000000000000000000000000000");
        assert_eq!(big_pos.shl(64).to_decimal_string(), "18446744073709551616000000000000000000000000000000");
        assert_eq!(big_pos.shr(50).to_decimal_string(), "888178419700125");
        assert_eq!(big_pos.neg().shr(50).to_decimal_string(), "-888178419700126");
        assert_eq!(BigInt::from_i128(1).shl(200).to_decimal_string(), "1606938044258990275541962092341162602522202993782792835301376");
        let a = big("123456789012345678901234567890");
        let b = big("987654321098765432109876543210");
        assert_eq!(a.bitand(&b).to_decimal_string(), bitref(&a, &b, '&'));
        assert_eq!(a.bitor(&b).to_decimal_string(), bitref(&a, &b, '|'));
        assert_eq!(a.bitxor(&b).to_decimal_string(), bitref(&a, &b, '^'));
        assert_eq!(BigInt::from_i128(-5).bitand(&BigInt::from_i128(3)), BigInt::from_i128(3));
        assert_eq!(BigInt::from_i128(-5).bitor(&BigInt::from_i128(3)), BigInt::from_i128(-5));
        assert_eq!(BigInt::from_i128(-5).bitxor(&BigInt::from_i128(3)), BigInt::from_i128(-8));
    }

    fn bitref(a: &BigInt, b: &BigInt, op: char) -> String {
        let (x, y) = (a.to_i128().unwrap(), b.to_i128().unwrap());
        let r = match op {
            '&' => x & y,
            '|' => x | y,
            _ => x ^ y,
        };
        BigInt::from_i128(r).to_decimal_string()
    }

    #[test]
    fn ordering_is_correct() {
        assert_eq!(big("100").cmp(&big("99")), Ordering::Greater);
        assert_eq!(big("-100").cmp(&big("-99")), Ordering::Less);
        assert_eq!(big("-100").cmp(&big("100")), Ordering::Less);
        assert_eq!(big("0").cmp(&big("-1")), Ordering::Greater);
        assert_eq!(big("0").cmp(&big("1")), Ordering::Less);
        assert_eq!(
            big("123456789012345678901234567890").cmp(&big("123456789012345678901234567890")),
            Ordering::Equal
        );
    }
}
