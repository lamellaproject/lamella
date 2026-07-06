//! The dynamic object model and its intrinsics.

use alloc::string::String;
use alloc::vec::Vec;

use core::cmp::Ordering;

use lamella_gc::{Heap, Ref, TypeDesc};
use lamella_py_bytecode::{BinOp, CmpOp};

use crate::bigint::BigInt;
use crate::builtins::Builtin;
use crate::interp::Frame;
use crate::trap::Trap;
use crate::value::{Value, FIXNUM_MAX, FIXNUM_MIN};

/// The `str` method ids stored in a bound method's payload (Python 3.14.6 "String
/// Methods"). The set grows as methods are added.
const STR_UPPER: u32 = 0;
const STR_LOWER: u32 = 1;
const STR_STARTSWITH: u32 = 2;
const STR_ENDSWITH: u32 = 3;
const STR_FIND: u32 = 4;
const STR_STRIP: u32 = 5;
const STR_LSTRIP: u32 = 6;
const STR_RSTRIP: u32 = 7;
const STR_REPLACE: u32 = 8;
const STR_COUNT: u32 = 9;
const STR_ISDIGIT: u32 = 10;
const STR_ISALPHA: u32 = 11;
const STR_ISALNUM: u32 = 12;
const STR_ISSPACE: u32 = 13;
const STR_ISUPPER: u32 = 14;
const STR_ISLOWER: u32 = 15;
const STR_SPLIT: u32 = 16;
const STR_ISDECIMAL: u32 = 17;
const STR_ISNUMERIC: u32 = 18;
const STR_JOIN: u32 = 19;
const STR_RFIND: u32 = 20;
const STR_INDEX: u32 = 21;
const STR_RINDEX: u32 = 22;
const STR_CAPITALIZE: u32 = 23;
const STR_TITLE: u32 = 24;
const STR_SWAPCASE: u32 = 25;
const STR_SPLITLINES: u32 = 26;
const STR_REMOVEPREFIX: u32 = 27;
const STR_REMOVESUFFIX: u32 = 28;
const STR_ZFILL: u32 = 29;
const STR_LJUST: u32 = 30;
const STR_RJUST: u32 = 31;
const STR_CENTER: u32 = 32;
const STR_PARTITION: u32 = 33;
const STR_RPARTITION: u32 = 34;
const STR_EXPANDTABS: u32 = 35;
const STR_ISASCII: u32 = 36;
const STR_ISIDENTIFIER: u32 = 37;
const STR_FORMAT: u32 = 38;
const STR_RSPLIT: u32 = 39;
const STR_CASEFOLD: u32 = 40;
const STR_TRANSLATE: u32 = 41;
const STR_FORMAT_MAP: u32 = 42;
const STR_ENCODE: u32 = 43;

/// The id of the `str` method `name`, or `None` if `str` has no such method.
/// Renders `n` in `radix` (2/8/10/16) with no sign or prefix; `upper` uppercases the hex digits.
fn format_radix(mut n: u32, radix: u32, upper: bool) -> String {
    if n == 0 {
        return String::from("0");
    }
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut buf = Vec::new();
    while n > 0 {
        let digit = DIGITS[(n % radix) as usize];
        buf.push(if upper { digit.to_ascii_uppercase() } else { digit });
        n /= radix;
    }
    buf.reverse();
    String::from_utf8(buf).unwrap_or_default()
}

/// Pads `s` to `width` code points with `fill`, per `align` (`<` left, `>`/`=` right, `^` centre).
fn pad_field(s: &str, width: usize, fill: char, align: char) -> String {
    let len = s.chars().count();
    if len >= width {
        return String::from(s);
    }
    let total = width - len;
    let mut out = String::new();
    match align {
        '<' => {
            out.push_str(s);
            (0..total).for_each(|_| out.push(fill));
        }
        '^' => {
            let left = total / 2;
            (0..left).for_each(|_| out.push(fill));
            out.push_str(s);
            (0..total - left).for_each(|_| out.push(fill));
        }
        _ => {
            (0..total).for_each(|_| out.push(fill));
            out.push_str(s);
        }
    }
    out
}

fn str_method_id(name: &str) -> Option<u32> {
    match name {
        "upper" => Some(STR_UPPER),
        "lower" => Some(STR_LOWER),
        "startswith" => Some(STR_STARTSWITH),
        "endswith" => Some(STR_ENDSWITH),
        "find" => Some(STR_FIND),
        "strip" => Some(STR_STRIP),
        "lstrip" => Some(STR_LSTRIP),
        "rstrip" => Some(STR_RSTRIP),
        "replace" => Some(STR_REPLACE),
        "count" => Some(STR_COUNT),
        "isdigit" => Some(STR_ISDIGIT),
        "isalpha" => Some(STR_ISALPHA),
        "isalnum" => Some(STR_ISALNUM),
        "isspace" => Some(STR_ISSPACE),
        "isupper" => Some(STR_ISUPPER),
        "islower" => Some(STR_ISLOWER),
        "split" => Some(STR_SPLIT),
        "isdecimal" => Some(STR_ISDECIMAL),
        "isnumeric" => Some(STR_ISNUMERIC),
        "join" => Some(STR_JOIN),
        "rfind" => Some(STR_RFIND),
        "index" => Some(STR_INDEX),
        "rindex" => Some(STR_RINDEX),
        "capitalize" => Some(STR_CAPITALIZE),
        "title" => Some(STR_TITLE),
        "swapcase" => Some(STR_SWAPCASE),
        "splitlines" => Some(STR_SPLITLINES),
        "removeprefix" => Some(STR_REMOVEPREFIX),
        "removesuffix" => Some(STR_REMOVESUFFIX),
        "zfill" => Some(STR_ZFILL),
        "ljust" => Some(STR_LJUST),
        "rjust" => Some(STR_RJUST),
        "center" => Some(STR_CENTER),
        "partition" => Some(STR_PARTITION),
        "rpartition" => Some(STR_RPARTITION),
        "expandtabs" => Some(STR_EXPANDTABS),
        "isascii" => Some(STR_ISASCII),
        "isidentifier" => Some(STR_ISIDENTIFIER),
        "format" => Some(STR_FORMAT),
        "rsplit" => Some(STR_RSPLIT),
        "casefold" => Some(STR_CASEFOLD),
        "translate" => Some(STR_TRANSLATE),
        "format_map" => Some(STR_FORMAT_MAP),
        "encode" => Some(STR_ENCODE),
        _ => None,
    }
}

const LIST_APPEND: u32 = 0;
const LIST_POP: u32 = 1;
const LIST_SORT: u32 = 2;
const LIST_REVERSE: u32 = 3;
const LIST_INSERT: u32 = 4;
const LIST_REMOVE: u32 = 5;
const LIST_INDEX: u32 = 6;
const LIST_COUNT: u32 = 7;
const LIST_EXTEND: u32 = 8;
const LIST_CLEAR: u32 = 9;
const LIST_COPY: u32 = 10;
const DICT_GET: u32 = 0;
const DICT_KEYS: u32 = 1;
const DICT_VALUES: u32 = 2;
const DICT_ITEMS: u32 = 3;
const DICT_UPDATE: u32 = 4;
const DICT_POP: u32 = 5;
const DICT_SETDEFAULT: u32 = 6;
const DICT_CLEAR: u32 = 7;
const DICT_COPY: u32 = 8;
const DICT_POPITEM: u32 = 9;
const SET_UNION: u32 = 0;
const SET_INTERSECTION: u32 = 1;
const SET_DIFFERENCE: u32 = 2;
const SET_SYMMETRIC_DIFFERENCE: u32 = 3;
const SET_ISSUBSET: u32 = 4;
const SET_ISSUPERSET: u32 = 5;
const SET_ISDISJOINT: u32 = 6;
const SET_COPY: u32 = 7;
const SET_ADD: u32 = 8;
const SET_DISCARD: u32 = 9;
const SET_REMOVE: u32 = 10;
const SET_CLEAR: u32 = 11;
const SET_POP: u32 = 12;
const SET_UPDATE: u32 = 13;
const TUPLE_INDEX: u32 = 0;
const TUPLE_COUNT: u32 = 1;

/// The `list`-method id for `name`, or `None`.
fn list_method_id(name: &str) -> Option<u32> {
    match name {
        "append" => Some(LIST_APPEND),
        "pop" => Some(LIST_POP),
        "sort" => Some(LIST_SORT),
        "reverse" => Some(LIST_REVERSE),
        "insert" => Some(LIST_INSERT),
        "remove" => Some(LIST_REMOVE),
        "index" => Some(LIST_INDEX),
        "count" => Some(LIST_COUNT),
        "extend" => Some(LIST_EXTEND),
        "clear" => Some(LIST_CLEAR),
        "copy" => Some(LIST_COPY),
        _ => None,
    }
}

/// The `dict`-method id for `name`, or `None`.
fn dict_method_id(name: &str) -> Option<u32> {
    match name {
        "get" => Some(DICT_GET),
        "keys" => Some(DICT_KEYS),
        "values" => Some(DICT_VALUES),
        "items" => Some(DICT_ITEMS),
        "update" => Some(DICT_UPDATE),
        "pop" => Some(DICT_POP),
        "setdefault" => Some(DICT_SETDEFAULT),
        "clear" => Some(DICT_CLEAR),
        "copy" => Some(DICT_COPY),
        "popitem" => Some(DICT_POPITEM),
        _ => None,
    }
}

/// The method id for a `set` method `name` -- the full mutable surface.
fn set_method_id(name: &str) -> Option<u32> {
    match name {
        "union" => Some(SET_UNION),
        "intersection" => Some(SET_INTERSECTION),
        "difference" => Some(SET_DIFFERENCE),
        "symmetric_difference" => Some(SET_SYMMETRIC_DIFFERENCE),
        "issubset" => Some(SET_ISSUBSET),
        "issuperset" => Some(SET_ISSUPERSET),
        "isdisjoint" => Some(SET_ISDISJOINT),
        "copy" => Some(SET_COPY),
        "add" => Some(SET_ADD),
        "discard" => Some(SET_DISCARD),
        "remove" => Some(SET_REMOVE),
        "clear" => Some(SET_CLEAR),
        "pop" => Some(SET_POP),
        "update" => Some(SET_UPDATE),
        _ => None,
    }
}

/// The method id for a `tuple` method `name` -- the immutable sequence queries.
fn tuple_method_id(name: &str) -> Option<u32> {
    match name {
        "index" => Some(TUPLE_INDEX),
        "count" => Some(TUPLE_COUNT),
        _ => None,
    }
}

/// `complex.conjugate()` -- the one complex method (`.real`/`.imag` are plain float attributes,
/// resolved in `getattr` directly, not bound methods).
#[cfg(feature = "complex")]
const COMPLEX_CONJUGATE: u32 = 0;

/// The method id for a `complex` method `name`.
#[cfg(feature = "complex")]
fn complex_method_id(name: &str) -> Option<u32> {
    match name {
        "conjugate" => Some(COMPLEX_CONJUGATE),
        _ => None,
    }
}

const BYTES_HEX: u32 = 0;
const BYTES_DECODE: u32 = 1;
const BYTEARRAY_APPEND: u32 = 2;
const BYTEARRAY_EXTEND: u32 = 3;
const BYTES_STARTSWITH: u32 = 4;
const BYTES_ENDSWITH: u32 = 5;
const BYTES_FIND: u32 = 6;
const BYTES_COUNT: u32 = 7;
const BYTES_REPLACE: u32 = 8;
const BYTES_UPPER: u32 = 9;
const BYTES_LOWER: u32 = 10;

/// The method id for a `bytes`/`bytearray` method `name` (`mutating` allows the bytearray-only ones).
fn bytes_method_id(name: &str, mutating: bool) -> Option<u32> {
    match name {
        "hex" => Some(BYTES_HEX),
        "decode" => Some(BYTES_DECODE),
        "startswith" => Some(BYTES_STARTSWITH),
        "endswith" => Some(BYTES_ENDSWITH),
        "find" => Some(BYTES_FIND),
        "count" => Some(BYTES_COUNT),
        "replace" => Some(BYTES_REPLACE),
        "upper" => Some(BYTES_UPPER),
        "lower" => Some(BYTES_LOWER),
        "append" if mutating => Some(BYTEARRAY_APPEND),
        "extend" if mutating => Some(BYTEARRAY_EXTEND),
        _ => None,
    }
}

/// The method id for a `frozenset` method `name` -- the read-only subset only (a frozenset is
/// immutable, so `add`/`discard`/`pop`/... are not attributes).
fn frozenset_method_id(name: &str) -> Option<u32> {
    match name {
        "union" => Some(SET_UNION),
        "intersection" => Some(SET_INTERSECTION),
        "difference" => Some(SET_DIFFERENCE),
        "symmetric_difference" => Some(SET_SYMMETRIC_DIFFERENCE),
        "issubset" => Some(SET_ISSUBSET),
        "issuperset" => Some(SET_ISSUPERSET),
        "isdisjoint" => Some(SET_ISDISJOINT),
        "copy" => Some(SET_COPY),
        _ => None,
    }
}

/// Whether `s` satisfies a `str` predicate (`isdigit`/`isalpha`/`isalnum`/`isspace`/
/// `isupper`/`islower`, Python 3.14.6 "String Methods"). The category predicates require
/// at least one character; `isupper`/`islower` require at least one CASED character and
/// that every cased character has that case. Classification is exact vs CPython: the
/// predicates derive from the shared [`lamella_unicode`] UCD properties (validated against
/// CPython's `unicodedata` + `str` methods over every code point), not from Rust's `char`
/// classification (which uses the broader `Alphabetic`/`White_Space` properties and diverges
/// on combining marks, superscript digits, CJK numerics, and the separator controls).
fn str_predicate(method_id: u32, s: &str) -> bool {
    use lamella_unicode::{
        general_category, is_lowercase, is_uppercase, is_white_space, is_xid_continue,
        is_xid_start, numeric_level, GeneralCategory,
    };
    let is_titlecase = |cp: u32| general_category(cp) == GeneralCategory::TitlecaseLetter;
    match method_id {
        STR_ISDIGIT => !s.is_empty() && s.chars().all(|c| numeric_level(c as u32) >= 2),
        STR_ISDECIMAL => !s.is_empty() && s.chars().all(|c| numeric_level(c as u32) >= 3),
        STR_ISNUMERIC => !s.is_empty() && s.chars().all(|c| numeric_level(c as u32) >= 1),
        STR_ISALPHA => !s.is_empty() && s.chars().all(|c| general_category(c as u32).is_letter()),
        STR_ISALNUM => {
            !s.is_empty()
                && s.chars().all(|c| {
                    let cp = c as u32;
                    general_category(cp).is_letter() || numeric_level(cp) >= 1
                })
        }
        STR_ISSPACE => {
            !s.is_empty()
                && s.chars().all(|c| {
                    let cp = c as u32;
                    is_white_space(cp) || (0x1c..=0x1f).contains(&cp)
                })
        }
        STR_ISUPPER => {
            let mut cased = false;
            for c in s.chars() {
                let cp = c as u32;
                if is_lowercase(cp) || is_titlecase(cp) {
                    return false;
                }
                cased |= is_uppercase(cp);
            }
            cased
        }
        STR_ISLOWER => {
            let mut cased = false;
            for c in s.chars() {
                let cp = c as u32;
                if is_uppercase(cp) || is_titlecase(cp) {
                    return false;
                }
                cased |= is_lowercase(cp);
            }
            cased
        }
        STR_ISASCII => s.chars().all(|c| (c as u32) < 0x80),
        STR_ISIDENTIFIER => match s.chars().next() {
            None => false,
            Some(first) => {
                (first == '_' || is_xid_start(first as u32))
                    && s.chars().skip(1).all(|c| is_xid_continue(c as u32))
            }
        },
        _ => false,
    }
}

/// Parses a `(affix_or_sub[, start[, end]])` argument list for `startswith`/`endswith`/
/// `find`: the first argument is the str to match (checked by the caller); the optional
/// `start`/`end` are slice bounds -- an `int` (a slice index) or `None` for the default.
/// A wrong count, or a non-int / non-`None` bound, is a `TypeError`.
fn affix_and_bounds(args: &[Value]) -> Result<(Value, Option<i64>, Option<i64>), Trap> {
    fn bound(v: Value) -> Result<Option<i64>, Trap> {
        if v.is_none() {
            Ok(None)
        } else {
            Ok(Some(v.as_int().ok_or(Trap::TypeError)?))
        }
    }
    match args {
        [affix] => Ok((*affix, None, None)),
        [affix, start] => Ok((*affix, bound(*start)?, None)),
        [affix, start, end] => Ok((*affix, bound(*start)?, bound(*end)?)),
        _ => Err(Trap::TypeError),
    }
}

/// Normalizes Python slice bounds `[start:end]` over `len` code points: a negative bound
/// counts from the end (`+ len`), then both clamp to `[0, len]`; an absent bound defaults
/// to `0` (start) or `len` (end). The returned `(start, end)` may have `start > end`,
/// which denotes an empty range.
fn normalize_bounds(start: Option<i64>, end: Option<i64>, len: i64) -> (i64, i64) {
    fn norm(i: i64, len: i64) -> i64 {
        (if i < 0 { i + len } else { i }).clamp(0, len)
    }
    (
        start.map_or(0, |i| norm(i, len)),
        end.map_or(len, |i| norm(i, len)),
    )
}

/// The substring spanning code points `[a, b)` of `s` (empty if `a >= b`). Indexing is by
/// code point -- `s[a:b]` in Python terms, not a byte slice.
fn cp_slice(s: &str, a: i64, b: i64) -> &str {
    if a >= b {
        return "";
    }
    let byte = |cp: i64| s.char_indices().nth(cp as usize).map_or(s.len(), |(i, _)| i);
    &s[byte(a)..byte(b)]
}

/// Python slice-bound adjustment for `[start:stop:step]` over `len` code points
/// (`PySlice_Unpack` + `PySlice_AdjustIndices`, Python 3.14.6 `slice.indices`): a `None`
/// bound takes its default for the step direction, a negative bound counts from the end,
/// and an out-of-range bound CLAMPS. Returns the `(start, stop)` to iterate with `step`. A
/// non-int, non-`None` bound is a `TypeError`.
fn adjust_slice(start_v: Value, stop_v: Value, step: i64, len: i64) -> Result<(i64, i64), Trap> {
    let clamp = |bound: i64| {
        if bound < 0 {
            let shifted = bound + len;
            if shifted < 0 {
                if step < 0 {
                    -1
                } else {
                    0
                }
            } else {
                shifted
            }
        } else if bound >= len {
            if step < 0 {
                len - 1
            } else {
                len
            }
        } else {
            bound
        }
    };
    let start = if start_v.is_none() {
        if step < 0 {
            len - 1
        } else {
            0
        }
    } else {
        clamp(start_v.as_int().ok_or(Trap::TypeError)?)
    };
    let stop = if stop_v.is_none() {
        if step < 0 {
            -1
        } else {
            len
        }
    } else {
        clamp(stop_v.as_int().ok_or(Trap::TypeError)?)
    };
    Ok((start, stop))
}

/// The Python `repr()` of a string: single quotes, switching to double quotes if the string
/// contains a `'` but no `"`; backslash, the quote, and the common control chars are escaped.
/// (Escaping of exotic non-printables is an ASCII-faithful refinement.)
fn str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&alloc::format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Renders a `float` exactly as CPython 3.14's `repr(float)`/`str(float)` does (they are identical
/// for a float): the SHORTEST decimal string that round-trips to the same double, in CPython's
/// choice of fixed vs exponential notation. This is deliberately NOT .NET's `Double.ToString`
/// (csharp's `G`-switch), whose thresholds and digit choices differ.
///
/// The rules, from CPython's `format_float_short` (`Python/pystrtod.c`, format code `'r'`):
/// - `nan`, `inf`, `-inf` render as those literals (lowercase; a NaN never carries a sign).
/// - Otherwise take the shortest round-trip digits `d0 d1 ...` and the scientific exponent `E`
///   (value = `d0.d1d2... x 10^E`). Rust's `{:e}` gives exactly this pair.
/// - Use EXPONENTIAL notation when `E < -4` (i.e. `E <= -5`) or `E >= 16`; the exponent is written
///   with an explicit sign and at least two digits (`1e-05`, `1e+16`, `1e+308`). A single
///   significant digit omits the point (`1e+16`, not `1.0e+16`).
/// - Else FIXED notation, with the decimal point placed after `E + 1` digits; a value with no
///   fractional part still gets a trailing `.0` (`1.0`, `100.0`) -- CPython's `Py_DTSF_ADD_DOT_0`.
///
/// The shortest round-trip scientific form of `value` (`"<mantissa>e<exp>"`, one digit before the
/// point) with CPython's tie-breaking. Rust's `{:e}` gives the shortest round-trip length but breaks
/// an exactly-halfway tie AWAY from even, whereas CPython (David Gay's `dtoa`) breaks it TO even. So
/// re-round to that same number of significant digits with `{:.*e}` (which rounds half-to-even on
/// the exact value): when the even-tie result still round-trips, it is the one CPython emits (either
/// the unique shortest, or the even member of a genuine tie); when it does not round-trip (the
/// correctly-rounded value falls outside the double's interval), Rust's shortest is already correct.
/// Verified to match CPython 3.14 on a 250k-double random differential, ties included.
fn shortest_scientific(value: f64) -> String {
    let shortest = alloc::format!("{value:e}");
    let mantissa = shortest.split('e').next().unwrap_or(&shortest);
    let sig = mantissa.chars().filter(char::is_ascii_digit).count();
    let even = alloc::format!("{value:.*e}", sig.saturating_sub(1));
    if even.parse::<f64>() == Ok(value) {
        even
    } else {
        shortest
    }
}

/// The digit sequence is the shortest that round-trips, with equidistant ties broken to EVEN --
/// see [`shortest_scientific`] for why Rust's `{:e}` alone is not enough. A 250k-double random
/// differential confirms it matches CPython 3.14 exactly (see the float corpus).
fn format_float(value: f64) -> String {
    format_float_impl(value, true)
}

/// The shared shortest-round-trip float renderer. `add_point_zero` controls CPython's
/// `Py_DTSF_ADD_DOT_0`: `true` for `float` (`1.0`, `100.0`), `false` for a `complex` PART (`(1+2j)`,
/// not `(1.0+2.0j)`) -- the only place the two formats differ (an integer-valued fixed result).
fn format_float_impl(value: f64, add_point_zero: bool) -> String {
    if value.is_nan() {
        return String::from("nan");
    }
    if value.is_infinite() {
        return String::from(if value < 0.0 { "-inf" } else { "inf" });
    }
    let scientific = shortest_scientific(value);
    let (negative, rest) = match scientific.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, scientific.as_str()),
    };
    let (mantissa, exponent) = rest.split_once('e').expect("`{:e}` always has an exponent");
    let exponent: i32 = exponent.parse().expect("`{:e}` exponent is a valid integer");
    let mut digits: String = mantissa.chars().filter(|&c| c != '.').collect();
    while digits.len() > 1 && digits.ends_with('0') {
        digits.pop();
    }
    let ndigits = digits.len() as i32;
    let decpt = exponent + 1;

    let body = if !(-4..16).contains(&exponent) {
        let mut mantissa_out = String::new();
        mantissa_out.push_str(&digits[..1]);
        if ndigits > 1 {
            mantissa_out.push('.');
            mantissa_out.push_str(&digits[1..]);
        }
        let sign = if exponent < 0 { '-' } else { '+' };
        alloc::format!("{mantissa_out}e{sign}{:02}", exponent.unsigned_abs())
    } else if decpt <= 0 {
        alloc::format!("0.{}{digits}", "0".repeat((-decpt) as usize))
    } else if decpt >= ndigits {
        let zeros = "0".repeat((decpt - ndigits) as usize);
        if add_point_zero {
            alloc::format!("{digits}{zeros}.0")
        } else {
            alloc::format!("{digits}{zeros}")
        }
    } else {
        alloc::format!("{}.{}", &digits[..decpt as usize], &digits[decpt as usize..])
    };
    if negative {
        alloc::format!("-{body}")
    } else {
        body
    }
}

/// Formats a non-negative double in scientific notation with a fixed number of fractional digits,
/// in CPython's `e`/`E` style (signed, >=2-digit exponent): `1.23e+04`. `mantissa` precision is the
/// fractional-digit count.
fn float_format_scientific(magnitude: f64, precision: usize, upper: bool) -> String {
    let raw = alloc::format!("{magnitude:.precision$e}");
    let (mantissa, exponent) = raw.split_once('e').expect("`{:e}` always has an exponent");
    let exponent: i32 = exponent.parse().expect("valid exponent");
    let e = if upper { 'E' } else { 'e' };
    let sign = if exponent < 0 { '-' } else { '+' };
    alloc::format!("{mantissa}{e}{sign}{:02}", exponent.unsigned_abs())
}

/// Formats a non-negative double in the general (`g`/`G`) style: `precision` significant digits, in
/// fixed notation when the exponent is in `[-4, precision)` else scientific, trailing zeros stripped
/// (kept under `#` alternate). With `keep_decimal`, a fixed result always keeps one fractional digit
/// (the default-format code's rule, `3.0` not `3`).
fn float_format_general(magnitude: f64, precision: usize, upper: bool, alternate: bool, keep_decimal: bool) -> String {
    let precision = precision.max(1);
    let sci = alloc::format!("{magnitude:.*e}", precision - 1);
    let (mantissa, exponent) = sci.split_once('e').expect("`{:e}` always has an exponent");
    let exponent: i32 = exponent.parse().expect("valid exponent");
    if (-4..precision as i32).contains(&exponent) {
        let fractional = (precision as i32 - 1 - exponent) as usize;
        let fixed = alloc::format!("{magnitude:.fractional$}");
        let stripped = if alternate { fixed } else { strip_float_trailing_zeros(&fixed) };
        if keep_decimal && !stripped.contains('.') {
            alloc::format!("{stripped}.0")
        } else {
            stripped
        }
    } else {
        let mantissa = if alternate {
            String::from(mantissa)
        } else {
            strip_float_trailing_zeros(mantissa)
        };
        let e = if upper { 'E' } else { 'e' };
        let sign = if exponent < 0 { '-' } else { '+' };
        alloc::format!("{mantissa}{e}{sign}{:02}", exponent.unsigned_abs())
    }
}

/// Strips trailing zeros after a decimal point (and a now-bare trailing point): `3.140` -> `3.14`,
/// `3.000` -> `3`.
fn strip_float_trailing_zeros(s: &str) -> String {
    if !s.contains('.') {
        return String::from(s);
    }
    let trimmed = s.trim_end_matches('0');
    String::from(trimmed.strip_suffix('.').unwrap_or(trimmed))
}

/// Inserts a `separator` every three digits of the INTEGER part (left of any `.`): `1234567.89` with
/// `,` -> `1,234,567.89`.
fn group_integer_digits(body: &str, separator: char) -> String {
    let (integer, rest) = match body.split_once('.') {
        Some((int, frac)) => (int, alloc::format!(".{frac}")),
        None => (body, String::new()),
    };
    let mut grouped = String::new();
    let digit_count = integer.len();
    for (index, digit) in integer.chars().enumerate() {
        if index > 0 && (digit_count - index) % 3 == 0 {
            grouped.push(separator);
        }
        grouped.push(digit);
    }
    grouped.push_str(&rest);
    grouped
}

/// Renders a `complex` as CPython's `repr`/`str` (identical). When the real part is a POSITIVE zero
/// only the imaginary term shows (`1j`, `-1j`, `0j`); otherwise `(real+imagj)` with the imaginary
/// term explicitly signed. Each part uses the shortest-round-trip float format WITHOUT a trailing
/// `.0` (`(1+2j)`, `(2.5-0.5j)`, `(1e+100+2e-05j)`, `(-0-1j)`).
#[cfg(feature = "complex")]
fn format_complex(real: f64, imag: f64) -> String {
    let imag_str = format_float_impl(imag, false);
    if real == 0.0 && real.is_sign_positive() {
        return alloc::format!("{imag_str}j");
    }
    let real_str = format_float_impl(real, false);
    let sign = if imag.is_sign_negative() { "" } else { "+" };
    alloc::format!("({real_str}{sign}{imag_str}j)")
}

/// CPython's `repr(bytes)` / `repr(bytearray)`: `b'...'` (or `bytearray(b'...')`), printable ASCII
/// shown literally, `\t\n\r\\` escaped, everything else as `\xNN`. The quote is `'` unless the data
/// has a `'` but no `"`.
fn bytes_repr(data: &[u8], is_bytearray: bool) -> String {
    let quote = if data.contains(&b'\'') && !data.contains(&b'"') { b'"' } else { b'\'' };
    let mut out = String::new();
    if is_bytearray {
        out.push_str("bytearray(");
    }
    out.push('b');
    out.push(quote as char);
    for &byte in data {
        match byte {
            b'\\' => out.push_str("\\\\"),
            b'\t' => out.push_str("\\t"),
            b'\n' => out.push_str("\\n"),
            b'\r' => out.push_str("\\r"),
            b if b == quote => {
                out.push('\\');
                out.push(b as char);
            }
            0x20..=0x7e => out.push(byte as char),
            _ => out.push_str(&alloc::format!("\\x{byte:02x}")),
        }
    }
    out.push(quote as char);
    if is_bytearray {
        out.push(')');
    }
    out
}

/// Replaces every non-overlapping occurrence of `old` with `new` in `data`. An empty `old` inserts
/// `new` between (and around) every byte, matching CPython.
fn replace_bytes(data: &[u8], old: &[u8], new: &[u8]) -> Vec<u8> {
    if old.is_empty() {
        let mut out = Vec::with_capacity(data.len() + new.len() * (data.len() + 1));
        out.extend_from_slice(new);
        for &byte in data {
            out.push(byte);
            out.extend_from_slice(new);
        }
        return out;
    }
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if i + old.len() <= data.len() && &data[i..i + old.len()] == old {
            out.extend_from_slice(new);
            i += old.len();
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// The number of elements in `range(start, stop, step)` (CPython's length formula).
fn range_len(start: i64, stop: i64, step: i64) -> i64 {
    if step > 0 {
        if start >= stop {
            0
        } else {
            (stop - start - 1) / step + 1
        }
    } else if start <= stop {
        0
    } else {
        (start - stop - 1) / (-step) + 1
    }
}

/// A Python type's metadata: a name and a fixed set of named attribute
/// slots. One Python type corresponds to one GC type-descriptor id (its index in the
/// [`ObjectModel`]'s type table), so an instance's header word names both.
///
/// This object model has no inheritance, descriptors, or instance `__dict__`: attributes are
/// a small fixed set resolved to slot indices. The MRO walk and the descriptor protocol
/// arrive with the full object model.
#[derive(Debug, Clone)]
pub struct PyType {
    name: String,
    attrs: Vec<(String, u16)>,
    num_slots: u16,
}

impl PyType {
    /// A type whose attributes are `attr_names`, assigned slots `0, 1, 2, ...` in order.
    #[must_use]
    pub fn with_slots(name: &str, attr_names: &[&str]) -> PyType {
        let attrs = attr_names
            .iter()
            .enumerate()
            .map(|(i, n)| (String::from(*n), i as u16))
            .collect();
        PyType {
            name: String::from(name),
            attrs,
            num_slots: attr_names.len() as u16,
        }
    }

    /// The type's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The number of attribute slots an instance reserves.
    #[must_use]
    pub fn num_slots(&self) -> u16 {
        self.num_slots
    }

    /// The slot index of attribute `name`, or `None` if the type has no such attribute.
    /// A linear scan -- the attribute sets are tiny; the inline cache
    /// keeps it off the hot path anyway.
    #[must_use]
    pub fn slot_of(&self, name: &str) -> Option<u16> {
        self.attrs
            .iter()
            .find(|(attr, _)| attr == name)
            .map(|&(_, slot)| slot)
    }
}

/// One call site's inline cache for attribute access (PEP 659 style).
///
/// A `LoadAttr` site always loads the *same* attribute name, so the cache keys on the
/// receiver's type id alone: on a type match the resolved slot is reused and the name
/// lookup is skipped. The cache stores no reference, so it survives a moving collection
/// untouched (type ids and slot offsets are stable across compaction).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InlineCache {
    type_id: u32,
    slot: u16,
    valid: bool,
}

impl Default for InlineCache {
    fn default() -> InlineCache {
        InlineCache::empty()
    }
}

impl InlineCache {
    /// A cold cache (no resolved type).
    #[must_use]
    pub const fn empty() -> InlineCache {
        InlineCache {
            type_id: 0,
            slot: 0,
            valid: false,
        }
    }

    /// The cached slot if `type_id` matches the last resolution (a cache *hit*), else
    /// `None` (a *miss*, which the caller resolves and records with [`InlineCache::fill`]).
    #[must_use]
    pub fn lookup(&self, type_id: u32) -> Option<u16> {
        if self.valid && self.type_id == type_id {
            Some(self.slot)
        } else {
            None
        }
    }

    /// Records a fresh resolution: a subsequent [`InlineCache::lookup`] of the same `type_id`
    /// will hit.
    pub fn fill(&mut self, type_id: u32, slot: u16) {
        self.type_id = type_id;
        self.slot = slot;
        self.valid = true;
    }
}

/// The dynamic object space: the shared heap plus the type table that gives each
/// heap object's header word a Python meaning.
///
/// The type table is indexed by GC type-descriptor id, so `heap.type_id_of(obj)` names
/// the [`PyType`] directly. The table is built once up front (the heap
/// needs its descriptors at construction); growing it dynamically (user-defined classes
/// at runtime) is a separate concern.
#[derive(Debug)]
pub struct ObjectModel {
    heap: Heap,
    types: Vec<PyType>,
    /// The runtime string arena: a `str`'s heap object holds an index into this, and the
    /// string bytes live here. The arena grows monotonically (strings are not reclaimed).
    strings: Vec<String>,
    /// The GC type-descriptor id of the `str` type; it follows the user types.
    str_type_id: u32,
    /// The GC type-descriptor id of `bytes` (immutable); its one-word payload indexes `byte_buffers`.
    bytes_type_id: u32,
    /// The GC type-descriptor id of `bytearray` (mutable); shares the `byte_buffers` arena.
    bytearray_type_id: u32,
    /// The GC type-descriptor id of a `long` (a big int, an i128 payload).
    long_type_id: u32,
    /// The GC type-descriptor id of a `bigint` (an arbitrary-precision int beyond i128); its
    /// one-word payload indexes the `bigints` arena. The third `int` representation (after fixnum
    /// and the i128 `long`); all three present as Python `int`.
    bigint_type_id: u32,
    /// The GC type-descriptor id of a `float` (an IEEE-754 double, an f64 payload). A heap object
    /// because `Value` is a 32-bit word -- an f64 cannot be an immediate (mirrors `long`).
    float_type_id: u32,
    /// The GC type-descriptor id of a `complex` (two f64 -- the real + imaginary parts, a 16-byte
    /// payload). Behind the `complex` capability knob.
    #[cfg(feature = "complex")]
    complex_type_id: u32,
    /// The GC type-descriptor id of a provided module (a namespace-dict wrapper).
    module_type_id: u32,
    /// The GC type-descriptor id of a bound method (`str.method`); it follows `str`.
    bound_method_type_id: u32,
    /// The GC type-descriptor id of a `slice(start, stop, step)`; it follows the bound method.
    slice_type_id: u32,
    /// The runtime backing for `list`/`tuple`: each container's heap object holds an index
    /// into this, and its elements (tagged Values) live in the indexed `Vec`. A list mutates
    /// its `Vec` in place; a tuple never does. (str-arena pattern; the GC-faithful
    /// variable-size container object is a follow-up on the tagged-trace seam.)
    seqs: Vec<Vec<Value>>,
    /// The runtime backing for `dict`: insertion-ordered key/value pairs (Python dicts
    /// preserve insertion order). A dict's heap object holds an index into this.
    dicts: Vec<Vec<(Value, Value)>>,
    /// The runtime backing for `bigint`: each arbitrary-precision int's heap object holds an index
    /// into this. Grows monotonically (str-arena pattern; the values are immutable).
    bigints: Vec<BigInt>,
    /// The runtime backing for `bytes`/`bytearray`: each object holds an index into this. A `bytes`
    /// buffer never mutates; a `bytearray` mutates its `Vec<u8>` in place (list-arena pattern).
    byte_buffers: Vec<Vec<u8>>,
    /// The runtime backing for `set`: deduped elements in insertion order. A set's heap object
    /// holds an index into this. (Iteration order is insertion, not CPython's hash order -- a
    /// documented divergence; differential tests compare sets as sets, e.g. via `sorted`.)
    sets: Vec<Vec<Value>>,
    /// The GC type-descriptor id of a `list`; it follows the slice.
    list_type_id: u32,
    /// The GC type-descriptor id of a `tuple`; it follows the list.
    tuple_type_id: u32,
    /// The GC type-descriptor id of a `dict`; it follows the tuple.
    dict_type_id: u32,
    /// The GC type-descriptor id of an iterator (over str/list/tuple/dict); follows dict.
    iter_type_id: u32,
    /// The GC type-descriptor id of a user CLASS object `[name, base, namespace-dict]`.
    class_type_id: u32,
    /// The GC type-descriptor id of a user class INSTANCE `[type, __dict__]`.
    instance_type_id: u32,
    /// The GC type-descriptor id of a bound Python method `[self, func-ref]`.
    py_bound_type_id: u32,
    /// The GC type-descriptor id of a `range(start, stop, step)` (a lazy int sequence).
    range_type_id: u32,
    /// The GC type-descriptor id of a `set` (a deduped collection); follows range.
    set_type_id: u32,
    /// The GC type-descriptor id of a `super` object `[class, self]`; follows set.
    super_type_id: u32,
    /// The GC type-descriptor id of a `frozenset` (an immutable set); shares the `sets` arena.
    frozenset_type_id: u32,
    /// The built-in exception classes (name -> class object), built lazily on first use so a
    /// program that never touches exceptions never allocates them. `BaseException` down to the
    /// concrete leaves; a raised interpreter [`Trap`] instantiates the matching one.
    exception_classes: Vec<(&'static str, Value)>,
    /// The exception currently in flight while it propagates to a handler (set by a `raise`
    /// and carried across call frames -- a [`Trap::Raised`] is the signal, this is the object).
    pending_exception: Option<Value>,
    /// The module namespace (top-level name -> value): classes and other top-level bindings the
    /// module body produces, which a function reaches by `LoadGlobal`. The body mirrors its locals
    /// here as it binds them.
    globals: Vec<(String, Value)>,
    /// Captured `print(...)` output (the interpreter is `no_std`, so it buffers rather than
    /// writing a stream; the host drains it).
    stdout: String,
    /// The GC type-descriptor id of the `gpio` module singleton (the clean hardware API).
    gpio_type_id: u32,
    /// The GC type-descriptor id of the `board` pin-name singleton.
    board_type_id: u32,
    /// The GC type-descriptor id of a `Pin` handle (a GC leaf of raw register words).
    pin_type_id: u32,
    /// The GC type-descriptor id of the `machine` module singleton.
    machine_type_id: u32,
    /// The GC type-descriptor id of a `machine.Pin` factory (a callable, carrying OUT/IN).
    pin_factory_type_id: u32,
    /// The GC type-descriptor id of the `digitalio` module singleton.
    digitalio_type_id: u32,
    /// The GC type-descriptor id of a `digitalio.DigitalInOut` factory (a callable).
    dio_factory_type_id: u32,
    /// The GC type-descriptor id of the `digitalio.Direction` enum singleton (OUTPUT/INPUT).
    direction_type_id: u32,
    /// The GC type-descriptor id of a `DigitalInOut` instance -- wraps a clean gpio `Pin` (one
    /// tagged slot), its `value`/`direction` exposed as properties.
    dio_type_id: u32,
    /// The GC type-descriptor id of a `PyFunction` -- a DEFAULTED function object
    /// `[func_index: raw u32 @0, defaults: tagged tuple|None @4, kwdefaults: tagged dict|None @8]`.
    /// A plain (non-defaulted) function stays a stateless `function_ref` immediate; only a `def`
    /// with default arguments allocates one.
    py_function_type_id: u32,
    /// The GC type-descriptor id of a generator object -- a leaf holding an index into
    /// `generators` (the arena slot of its suspended frame).
    generator_type_id: u32,
    /// The GC type-descriptor id of a `Cell` -- a heap box holding one tagged `Value`, shared
    /// (mutably) between an enclosing function and a nested closure that captures the variable.
    cell_type_id: u32,
    /// The suspended frames of live generators, indexed by a generator object's payload word.
    /// `None` = the generator is exhausted (its body returned) or currently running; `Some(frame)`
    /// = it is fresh (ip 0) or suspended at a `yield`. A suspended frame holds tagged Values
    /// (locals, eval stack) that are GC roots, so a future moving collector must trace each frame
    /// here -- the same follow-on the `seqs`/`dicts` element arenas need (the interpreter never
    /// auto-collects today, so no live path exercises it yet).
    generators: Vec<Option<Frame>>,
    /// A pool of returned call frames, kept for their Vec allocations (locals/eval-stack/caches) so a
    /// hot call/return cycle reuses buffers instead of allocating a fresh frame each call. Bounded;
    /// every pooled frame is cleared of Values (holds nothing to trace).
    frame_pool: Vec<Frame>,
    /// App-claimed GPIO pins -- the one-owner-per-pin reservation. A second claim of a held pin
    /// fails LOUD (a `ValueError`, never a silent register
    /// race. Language-neutral; the shared BSP-level registry both interpreters consult is the
    /// coordinated follow-on.
    gpio_claimed: Vec<u32>,
    /// Firmware-reserved pins (seeded from the target profile): a claim of one fails loud, so an
    /// app-vs-firmware conflict is caught rather than silently colliding.
    gpio_reserved: Vec<u32>,
    /// The volatile MMIO write seam: on device the runner installs `lamella_mmio::write32`; on
    /// the host it is unset and writes fall to the simulated register file.
    mmio_write_fn: Option<fn(u32, u32)>,
    /// The volatile MMIO read seam (device: `lamella_mmio::read32`; host: the sim).
    mmio_read_fn: Option<fn(u32) -> u32>,
    /// The host-only simulated register file (the default MMIO target when no seam is installed),
    /// so a driver runs and its register writes are verifiable OFF-device.
    #[cfg(not(target_os = "none"))]
    mmio_sim: alloc::collections::BTreeMap<u32, u32>,
    /// The host-only ordered log of every MMIO write, so a test can assert the exact drive
    /// sequence (e.g. a blinky's alternating set/reset) that the last-value sim map cannot show.
    #[cfg(not(target_os = "none"))]
    mmio_trace: Vec<(u32, u32)>,
    /// The `sleep_ms` delay seam (device: a timer/spin; host: a no-op, so the differential is not
    /// slowed by real sleeps).
    delay_fn: Option<fn(u32)>,
    /// A one-shot argument for the NEXT trap-raised built-in exception, so a trap site can attach
    /// context (a `KeyError`'s key, an `IndexError`/`ValueError` message) that the bare `Trap` enum
    /// cannot carry. Set right before returning the bare trap; [`ObjectModel::trap_to_exception`]
    /// takes it as the exception's single arg. Transient: consumed on the immediately following
    /// conversion (a future safe-point collector would trace it as a root while set).
    pending_trap_arg: Option<Value>,
}

impl ObjectModel {
    /// Builds an object space for `types`, with a heap of `heap_capacity` bytes. Each
    /// type's GC descriptor reserves `num_slots` tagged-value words and lists no bare
    /// reference fields (the slots are traced by tag -- see the module note). The `str`
    /// type is appended after them.
    #[must_use]
    pub fn new(types: Vec<PyType>, heap_capacity: usize) -> ObjectModel {
        let mut descs: Vec<TypeDesc> = types
            .iter()
            .map(|t| TypeDesc {
                payload_size: u32::from(t.num_slots) * 4,
                ref_offsets: Vec::new(),
                tagged_offsets: (0..u32::from(t.num_slots)).map(|i| i * 4).collect(),
            })
            .collect();
        let str_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let bytes_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let bytearray_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let bound_method_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..1).map(|i| i * 4).collect(),
        });
        let slice_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..3).map(|i| i * 4).collect(),
        });
        let list_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let tuple_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let dict_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let iter_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..1).map(|i| i * 4).collect(),
        });
        let class_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..3).map(|i| i * 4).collect(),
        });
        let instance_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..2).map(|i| i * 4).collect(),
        });
        let py_bound_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..2).map(|i| i * 4).collect(),
        });
        let range_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..3).map(|i| i * 4).collect(),
        });
        let set_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let super_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..2).map(|i| i * 4).collect(),
        });
        let frozenset_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let gpio_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let board_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let pin_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: crate::gpio::PIN_WORDS * 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let machine_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let pin_factory_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let digitalio_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let dio_factory_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let direction_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let dio_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: (0..1).map(|i| i * 4).collect(),
        });
        let py_function_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: (1..3).map(|i| i * 4).collect(),
        });
        let generator_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let cell_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![0],
        });
        let long_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 16,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let bigint_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let float_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        #[cfg(feature = "complex")]
        let complex_type_id = descs.len() as u32;
        #[cfg(feature = "complex")]
        descs.push(TypeDesc {
            payload_size: 16,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        });
        let module_type_id = descs.len() as u32;
        descs.push(TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: alloc::vec![0],
        });
        ObjectModel {
            heap: Heap::new(heap_capacity, descs),
            types,
            strings: Vec::new(),
            seqs: Vec::new(),
            dicts: Vec::new(),
            bigints: Vec::new(),
            byte_buffers: Vec::new(),
            sets: Vec::new(),
            str_type_id,
            bytes_type_id,
            bytearray_type_id,
            long_type_id,
            bigint_type_id,
            float_type_id,
            #[cfg(feature = "complex")]
            complex_type_id,
            module_type_id,
            bound_method_type_id,
            slice_type_id,
            list_type_id,
            tuple_type_id,
            dict_type_id,
            iter_type_id,
            class_type_id,
            instance_type_id,
            py_bound_type_id,
            range_type_id,
            set_type_id,
            super_type_id,
            frozenset_type_id,
            exception_classes: Vec::new(),
            pending_exception: None,
            globals: Vec::new(),
            stdout: String::new(),
            gpio_type_id,
            board_type_id,
            pin_type_id,
            machine_type_id,
            pin_factory_type_id,
            digitalio_type_id,
            dio_factory_type_id,
            direction_type_id,
            dio_type_id,
            py_function_type_id,
            generator_type_id,
            cell_type_id,
            generators: Vec::new(),
            frame_pool: Vec::new(),
            gpio_claimed: Vec::new(),
            gpio_reserved: Vec::new(),
            mmio_write_fn: None,
            mmio_read_fn: None,
            #[cfg(not(target_os = "none"))]
            mmio_sim: alloc::collections::BTreeMap::new(),
            #[cfg(not(target_os = "none"))]
            mmio_trace: Vec::new(),
            delay_fn: None,
            pending_trap_arg: None,
        }
    }

    /// The single managed-allocation chokepoint: every `new_*` heap object is allocated here, so
    /// the GC / allocation tier is chosen in ONE place (a `gc(none|bump|collected)` knob --
    /// see [[gc-implementation-tiers]]). The default (allocation-capable) tier bumps the moving
    /// heap and returns `None` when full, at which point a collected tier drives `collect()` before
    /// retrying (the interpreter never auto-collects today, so it is allocate-only / bump). The
    /// `gc-none` tier has NO managed heap: every allocation is `None` -> a loud `OutOfMemory`, so a
    /// pure fixnum / mmio / control-flow driver runs (it never allocates) while an allocating
    /// program fails fast -- upholding "runs on interpreter-P => runs on device-P" for the tiniest
    /// micros. Dropping the collector code itself (binary size) is the coordinated `lamella-gc`
    /// allocation-mode follow-on (the `Allocator` trait's no-GC profile).
    #[must_use]
    fn alloc_object(&mut self, type_id: u32) -> Option<Ref> {
        #[cfg(feature = "gc-none")]
        {
            let _ = type_id;
            None
        }
        #[cfg(not(feature = "gc-none"))]
        {
            self.heap.alloc(type_id)
        }
    }

    /// Allocates a `str` from `s`, returning a heap-pointer Value. The content is interned
    /// in the string arena and the heap object holds its index.
    pub fn new_str(&mut self, s: &str) -> Result<Value, Trap> {
        let index = self.strings.len() as u32;
        self.strings.push(String::from(s));
        let reference = self.alloc_object(self.str_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// The string content if `value` is a `str`, else `None`.
    #[must_use]
    pub fn str_value(&self, value: Value) -> Option<&str> {
        let reference = value.as_ref()?;
        if self.heap.type_id_of(reference) != self.str_type_id {
            return None;
        }
        let index = self.heap.read_u32(reference.0) as usize;
        self.strings.get(index).map(String::as_str)
    }

    /// `len(value)` -- the built-in length. Handles `str` (its number of
    /// Unicode code points, per Python `len(str)`); containers and the `__len__` protocol
    /// arrive with the full object model. A value with no length is a `TypeError`.
    pub fn py_len(&self, value: Value) -> Result<Value, Trap> {
        let n = if let Some(s) = self.str_value(value) {
            s.chars().count()
        } else if let Some(data) = self.bytes_value(value) {
            data.len()
        } else if let Some(elems) = self.seq_value(value) {
            elems.len()
        } else if let Some(entries) = self.dict_value(value) {
            entries.len()
        } else if self.is_range(value) {
            let (start, stop, step) = self.range_bounds(value);
            range_len(start, stop, step).max(0) as usize
        } else if let Some(elements) = self.set_value(value) {
            elements.len()
        } else {
            return Err(Trap::TypeError);
        };
        Value::fixnum(n as i32).ok_or(Trap::Overflow)
    }

    /// Whether `value` is a `str`.
    #[must_use]
    pub fn is_str(&self, value: Value) -> bool {
        self.str_value(value).is_some()
    }

    /// A new immutable `bytes` object over `data`.
    pub fn new_bytes(&mut self, data: Vec<u8>) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.bytes_type_id).ok_or(Trap::OutOfMemory)?;
        let index = self.byte_buffers.len();
        self.byte_buffers.push(data);
        self.heap.write_u32(reference.0, index as u32);
        Ok(Value::from_ref(reference))
    }

    /// A new mutable `bytearray` object over `data`.
    pub fn new_bytearray(&mut self, data: Vec<u8>) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.bytearray_type_id).ok_or(Trap::OutOfMemory)?;
        let index = self.byte_buffers.len();
        self.byte_buffers.push(data);
        self.heap.write_u32(reference.0, index as u32);
        Ok(Value::from_ref(reference))
    }

    /// The bytes a `bytes` or `bytearray` holds, or `None` for any other value.
    #[must_use]
    pub fn bytes_value(&self, value: Value) -> Option<&[u8]> {
        let reference = value.as_ref()?;
        let type_id = self.heap.type_id_of(reference);
        if type_id != self.bytes_type_id && type_id != self.bytearray_type_id {
            return None;
        }
        let index = self.heap.read_u32(reference.0) as usize;
        self.byte_buffers.get(index).map(Vec::as_slice)
    }

    /// The `byte_buffers` arena slot of a `bytes`/`bytearray`, for in-place mutation of a bytearray.
    fn byte_buffer_slot(&self, value: Value) -> Option<usize> {
        let reference = value.as_ref()?;
        let type_id = self.heap.type_id_of(reference);
        if type_id != self.bytes_type_id && type_id != self.bytearray_type_id {
            return None;
        }
        Some(self.heap.read_u32(reference.0) as usize)
    }

    /// Whether `value` is a `bytes` (immutable).
    #[must_use]
    pub fn is_bytes(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.bytes_type_id)
    }

    /// Whether `value` is a `bytearray` (mutable).
    #[must_use]
    pub fn is_bytearray(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.bytearray_type_id)
    }

    /// The dynamic binary-op dispatch (`py_binop`) for object operands: `str` (`+` concatenates,
    /// `* int` repeats), `list`/`tuple` (`+` concatenates the SAME kind, `* int` repeats), and the
    /// set algebra (`| & - ^`). Any other operator, or a mismatched-kind pair (`"a" + 1`,
    /// `[1] + (1,)`, `"a" - "b"`), is a `TypeError` -- Python's rules. Returns `Ok(None)` when
    /// NEITHER operand is an object, so the caller falls back to the numeric path -- the
    /// one-source-of-truth dispatch both the interpreter and the AOT `py_binop` intrinsic consume.
    pub fn py_binary(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Result<Option<Value>, Trap> {
        if self.is_set(lhs) || self.is_frozenset(lhs) {
            return Ok(Some(self.set_binary_op(op, lhs, rhs)?));
        }
        if self.is_str(lhs) || self.is_str(rhs) {
            return Ok(Some(self.str_binary_op(op, lhs, rhs)?));
        }
        if self.bytes_value(lhs).is_some() || self.bytes_value(rhs).is_some() {
            return Ok(Some(self.bytes_binary_op(op, lhs, rhs)?));
        }
        if self.seq_value(lhs).is_some() || self.seq_value(rhs).is_some() {
            return Ok(Some(self.seq_binary_op(op, lhs, rhs)?));
        }
        Ok(None)
    }

    /// `bytes`/`bytearray` `+` (concatenate two byte strings; the result kind follows the LEFT
    /// operand) and `* int` (repeat; a non-positive count gives empty). Any other combination is a
    /// `TypeError`.
    fn bytes_binary_op(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Result<Value, Trap> {
        match op {
            BinOp::Add => {
                match (self.bytes_value(lhs).map(<[u8]>::to_vec), self.bytes_value(rhs).map(<[u8]>::to_vec)) {
                    (Some(mut a), Some(b)) => {
                        a.extend(b);
                        if self.is_bytearray(lhs) {
                            self.new_bytearray(a)
                        } else {
                            self.new_bytes(a)
                        }
                    }
                    _ => Err(Trap::TypeError),
                }
            }
            BinOp::Mul => {
                let (data, count, bytearray) = if let Some(d) = self.bytes_value(lhs).map(<[u8]>::to_vec) {
                    (d, rhs.as_int().ok_or(Trap::TypeError)?, self.is_bytearray(lhs))
                } else {
                    let d = self.bytes_value(rhs).map(<[u8]>::to_vec).ok_or(Trap::TypeError)?;
                    (d, lhs.as_int().ok_or(Trap::TypeError)?, self.is_bytearray(rhs))
                };
                let repeated = if count > 0 { data.repeat(count as usize) } else { Vec::new() };
                if bytearray {
                    self.new_bytearray(repeated)
                } else {
                    self.new_bytes(repeated)
                }
            }
            _ => Err(Trap::TypeError),
        }
    }

    /// `str + str` (concatenate) and `str * int` / `int * str` (repeat, a non-positive count gives
    /// `""`). Any other combination (`"a" + 1`, `"a" - "b"`, `"a" * "b"`) is a `TypeError`.
    fn str_binary_op(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Result<Value, Trap> {
        match op {
            BinOp::Add => {
                match (self.str_value(lhs).map(String::from), self.str_value(rhs).map(String::from)) {
                    (Some(mut a), Some(b)) => {
                        a.push_str(&b);
                        self.new_str(&a)
                    }
                    _ => Err(Trap::TypeError),
                }
            }
            BinOp::Mul => {
                let (text, count) = if let Some(text) = self.str_value(lhs).map(String::from) {
                    (text, rhs.as_int().ok_or(Trap::TypeError)?)
                } else {
                    let text = self.str_value(rhs).map(String::from).ok_or(Trap::TypeError)?;
                    (text, lhs.as_int().ok_or(Trap::TypeError)?)
                };
                let repeated = if count > 0 { text.repeat(count as usize) } else { String::new() };
                self.new_str(&repeated)
            }
            BinOp::Mod => {
                let template = self.str_value(lhs).map(String::from).ok_or(Trap::TypeError)?;
                let args = if self.is_tuple(rhs) {
                    self.seq_value(rhs).cloned().unwrap_or_default()
                } else {
                    alloc::vec![rhs]
                };
                let rendered = self.percent_format(&template, &args)?;
                self.new_str(&rendered)
            }
            _ => Err(Trap::TypeError),
        }
    }

    /// printf-style `%` formatting for `str % args`: `%d`/`%i` an int, `%s` str(), `%r` repr(),
    /// `%%` a literal `%`; the args are consumed left to right. A conversion with flags/width/
    /// precision (`%5d`) or an unhandled type (`%f`, `%x`) is `Unsupported` (never wrong output);
    /// too few or too many args is a `TypeError` (matching CPython's "not enough/all ... converted").
    fn percent_format(&self, template: &str, args: &[Value]) -> Result<String, Trap> {
        let mut out = String::new();
        let chars: Vec<char> = template.chars().collect();
        let mut i = 0;
        let mut next_arg = 0usize;
        while i < chars.len() {
            if chars[i] != '%' {
                out.push(chars[i]);
                i += 1;
                continue;
            }
            i += 1;
            if chars.get(i) == Some(&'%') {
                out.push('%');
                i += 1;
                continue;
            }
            let mut flags = String::new();
            while chars.get(i).is_some_and(|c| matches!(c, '-' | '+' | ' ' | '0' | '#')) {
                flags.push(chars[i]);
                i += 1;
            }
            let mut width = String::new();
            while chars.get(i).is_some_and(char::is_ascii_digit) {
                width.push(chars[i]);
                i += 1;
            }
            let mut precision = 0usize;
            let mut has_precision = false;
            if chars.get(i) == Some(&'.') {
                has_precision = true;
                i += 1;
                while chars.get(i).is_some_and(char::is_ascii_digit) {
                    precision = precision * 10 + (chars[i] as usize - '0' as usize);
                    i += 1;
                }
            }
            let ty = *chars.get(i).ok_or(Trap::ValueError)?;
            i += 1;
            let arg = *args.get(next_arg).ok_or(Trap::TypeError)?;
            next_arg += 1;
            let width_n = width.parse::<usize>().unwrap_or(0);
            match ty {
                's' | 'r' => {
                    let mut body = if ty == 's' { self.display(arg) } else { self.repr(arg) };
                    if has_precision {
                        body = body.chars().take(precision).collect();
                    }
                    let align = if flags.contains('-') { '<' } else { '>' };
                    out.push_str(&pad_field(&body, width_n, ' ', align));
                }
                _ => {
                    let mut spec = String::new();
                    if flags.contains('-') {
                        spec.push('<');
                    }
                    if flags.contains('+') {
                        spec.push('+');
                    } else if flags.contains(' ') {
                        spec.push(' ');
                    }
                    if flags.contains('#') {
                        spec.push('#');
                    }
                    if flags.contains('0') && !flags.contains('-') {
                        spec.push('0');
                    }
                    spec.push_str(&width);
                    spec.push(if ty == 'i' || ty == 'u' { 'd' } else { ty });
                    out.push_str(&self.format_value_spec(arg, &spec)?);
                }
            }
        }
        if next_arg != args.len() {
            return Err(Trap::TypeError);
        }
        Ok(out)
    }

    /// `list + list` / `tuple + tuple` (concatenate the SAME kind) and `seq * int` / `int * seq`
    /// (repeat, a non-positive count gives an empty sequence). A mismatched-kind `+` (`[1] + (2,)`,
    /// `[1] + 2`) or any other operator is a `TypeError`.
    fn seq_binary_op(&mut self, op: BinOp, lhs: Value, rhs: Value) -> Result<Value, Trap> {
        match op {
            BinOp::Add => {
                let both_lists = self.is_list(lhs) && self.is_list(rhs);
                let both_tuples = self.is_tuple(lhs) && self.is_tuple(rhs);
                if !both_lists && !both_tuples {
                    return Err(Trap::TypeError);
                }
                let mut elements = self.seq_value(lhs).cloned().ok_or(Trap::TypeError)?;
                let rhs_elements = self.seq_value(rhs).cloned().ok_or(Trap::TypeError)?;
                elements.extend(rhs_elements);
                if both_tuples {
                    self.new_tuple(elements)
                } else {
                    self.new_list(elements)
                }
            }
            BinOp::Mul => {
                let (sequence, count) = if self.seq_value(lhs).is_some() {
                    (lhs, rhs.as_int().ok_or(Trap::TypeError)?)
                } else {
                    (rhs, lhs.as_int().ok_or(Trap::TypeError)?)
                };
                let is_tuple = self.is_tuple(sequence);
                let base = self.seq_value(sequence).cloned().ok_or(Trap::TypeError)?;
                let mut elements = Vec::new();
                for _ in 0..count.max(0) {
                    elements.extend_from_slice(&base);
                }
                if is_tuple {
                    self.new_tuple(elements)
                } else {
                    self.new_list(elements)
                }
            }
            _ => Err(Trap::TypeError),
        }
    }

    /// The dynamic comparison dispatch (`py_compare`) for object operands. `str`/`str`
    /// compares by code point (Python 3.14.6, "Comparisons"); a `str` against a non-`str`
    /// is unequal for `==`/`!=` but a `TypeError` for the ordering operators (Python:
    /// `"a" == 1` is `False`, `"a" < 1` raises). `Ok(None)` when neither operand is an
    /// object, so the caller falls back to the numeric / identity path.
    pub fn py_compare(&self, op: CmpOp, lhs: Value, rhs: Value) -> Result<Option<Value>, Trap> {
        if self.is_set(lhs) || self.is_frozenset(lhs) {
            return Ok(Some(self.set_compare(op, lhs, rhs)?));
        }
        if self.bytes_value(lhs).is_some() || self.bytes_value(rhs).is_some() {
            return match (self.bytes_value(lhs), self.bytes_value(rhs)) {
                (Some(a), Some(b)) => {
                    let ord = a.cmp(b);
                    let holds = match op {
                        CmpOp::Eq => ord == Ordering::Equal,
                        CmpOp::Ne => ord != Ordering::Equal,
                        CmpOp::Lt => ord == Ordering::Less,
                        CmpOp::Le => ord != Ordering::Greater,
                        CmpOp::Gt => ord == Ordering::Greater,
                        CmpOp::Ge => ord != Ordering::Less,
                        CmpOp::Is | CmpOp::IsNot => {
                            unreachable!("is/is not handled in the Op::Compare path")
                        }
                    };
                    Ok(Some(Value::from_bool(holds)))
                }
                _ => match op {
                    CmpOp::Eq => Ok(Some(Value::FALSE)),
                    CmpOp::Ne => Ok(Some(Value::TRUE)),
                    _ => Err(Trap::TypeError),
                },
            };
        }
        match (self.str_value(lhs), self.str_value(rhs)) {
            (None, None) => Ok(None),
            (Some(a), Some(b)) => {
                let ord = a.cmp(b);
                let holds = match op {
                    CmpOp::Eq => ord == Ordering::Equal,
                    CmpOp::Ne => ord != Ordering::Equal,
                    CmpOp::Lt => ord == Ordering::Less,
                    CmpOp::Le => ord != Ordering::Greater,
                    CmpOp::Gt => ord == Ordering::Greater,
                    CmpOp::Ge => ord != Ordering::Less,
                    CmpOp::Is | CmpOp::IsNot => {
                        unreachable!("is/is not handled in the Op::Compare path")
                    }
                };
                Ok(Some(Value::from_bool(holds)))
            }
            _ => match op {
                CmpOp::Eq => Ok(Some(Value::FALSE)),
                CmpOp::Ne => Ok(Some(Value::TRUE)),
                _ => Err(Trap::TypeError),
            },
        }
    }

    /// Python truthiness of `value`: `None`/`False`/`0`/`""`/empty container/empty range are
    /// false; a non-empty str/container, a non-zero int, and any other object (e.g. a class
    /// instance) are true. Always `Ok(Some(_))` for the value subset we have (the `Option` keeps
    /// the seam for a future `__bool__`/`__len__` dispatch that could defer).
    pub fn py_truthy(&self, value: Value) -> Result<Option<bool>, Trap> {
        if value.is_none() || value == Value::FALSE {
            return Ok(Some(false));
        }
        if value == Value::TRUE {
            return Ok(Some(true));
        }
        if let Some(n) = value.as_fixnum() {
            return Ok(Some(n != 0));
        }
        if let Some(s) = self.str_value(value) {
            return Ok(Some(!s.is_empty()));
        }
        if let Some(data) = self.bytes_value(value) {
            return Ok(Some(!data.is_empty()));
        }
        if let Some(elems) = self.seq_value(value) {
            return Ok(Some(!elems.is_empty()));
        }
        if let Some(entries) = self.dict_value(value) {
            return Ok(Some(!entries.is_empty()));
        }
        if let Some(elements) = self.set_value(value) {
            return Ok(Some(!elements.is_empty()));
        }
        if self.is_range(value) {
            let (start, stop, step) = self.range_bounds(value);
            return Ok(Some(range_len(start, stop, step) > 0));
        }
        Ok(Some(true))
    }

    /// The dynamic subscript dispatch (`py_getitem`) for `container[index]` -- currently
    /// `str`. A `str` indexes by code point (Python 3.14.6, Common Sequence Operations):
    /// the index is an `int` (`bool` too, an int subtype), a negative index counts from
    /// the end (`len + i`), an index outside `[-len, len)` is an `IndexError`, and the
    /// result is a length-1 `str` (Python has no char type). A non-`int` index is a
    /// `TypeError`, as is subscripting a non-subscriptable value. (Slicing and
    /// store-subscript are separate operations; `str` is immutable.) Containers join this
    /// dispatch later -- the one-source-of-truth path the interpreter and the AOT
    /// `py_getitem` intrinsic both consume.
    pub fn py_getitem(&mut self, container: Value, index: Value) -> Result<Value, Trap> {
        if self.bytes_value(container).is_some() {
            if self.is_slice(index) {
                let selected = self.slice_bytes(container, index)?;
                return if self.is_bytearray(container) {
                    self.new_bytearray(selected)
                } else {
                    self.new_bytes(selected)
                };
            }
            let data = self.bytes_value(container).ok_or(Trap::TypeError)?;
            let len = data.len() as i64;
            let i = index.as_int().ok_or(Trap::TypeError)?;
            let at = if i < 0 { i + len } else { i };
            if at < 0 || at >= len {
                return Err(self.with_message(Trap::IndexError, "index out of range"));
            }
            return Value::fixnum(i32::from(data[at as usize])).ok_or(Trap::Overflow);
        }
        if self.str_value(container).is_some() {
            if self.is_slice(index) {
                return self.str_getitem_slice(container, index);
            }
            let resolved = {
                let s = self.str_value(container).ok_or(Trap::TypeError)?;
                let i = index.as_int().ok_or(Trap::TypeError)?;
                let len = s.chars().count() as i64;
                let at = if i < 0 { i + len } else { i };
                if at < 0 || at >= len {
                    None
                } else {
                    s.chars().nth(at as usize)
                }
            };
            let Some(ch) = resolved else {
                return Err(self.with_message(Trap::IndexError, "string index out of range"));
            };
            let mut buf = [0u8; 4];
            return self.new_str(ch.encode_utf8(&mut buf));
        }
        if self.seq_value(container).is_some() {
            if self.is_slice(index) {
                return self.seq_getitem_slice(container, index);
            }
            let (resolved, is_tuple) = {
                let elems = self.seq_value(container).ok_or(Trap::TypeError)?;
                let len = elems.len() as i64;
                let i = index.as_int().ok_or(Trap::TypeError)?;
                let at = if i < 0 { i + len } else { i };
                let resolved = if at < 0 || at >= len { None } else { Some(elems[at as usize]) };
                (resolved, self.is_tuple(container))
            };
            match resolved {
                Some(value) => return Ok(value),
                None => {
                    let message = if is_tuple {
                        "tuple index out of range"
                    } else {
                        "list index out of range"
                    };
                    return Err(self.with_message(Trap::IndexError, message));
                }
            }
        }
        if self.dict_value(container).is_some() {
            let found = {
                let entries = self.dict_value(container).ok_or(Trap::TypeError)?;
                entries
                    .iter()
                    .find(|(k, _)| self.key_eq(*k, index))
                    .map(|(_, v)| *v)
            };
            return match found {
                Some(value) => Ok(value),
                None => {
                    self.set_trap_arg(index);
                    Err(Trap::KeyError)
                }
            };
        }
        if self.is_range(container) {
            let (start, stop, step) = self.range_bounds(container);
            let len = range_len(start, stop, step);
            let i = index.as_int().ok_or(Trap::TypeError)?;
            let at = if i < 0 { i + len } else { i };
            if at < 0 || at >= len {
                return Err(self.with_message(Trap::IndexError, "range object index out of range"));
            }
            return Value::fixnum((start + at * step) as i32).ok_or(Trap::Overflow);
        }
        Err(Trap::TypeError)
    }

    /// `str` slicing -- `container[slice]`. Reads the slice's `[start, stop, step]` and
    /// builds the substring per Python 3.14.6 (`slice.indices`): a `None` start/stop takes
    /// its default for the step direction, a negative bound counts from the end, out-of-range
    /// bounds CLAMP (no IndexError, unlike integer indexing), and the step may be negative
    /// (reversing). `step == 0` is a `ValueError`; a non-int, non-`None` bound a `TypeError`.
    /// `seq[i:j:k]` -- a new `list` (from a list) or `tuple` (from a tuple) of the selected
    /// elements, with CPython slice semantics (clamping bounds, no IndexError, negative step).
    fn seq_getitem_slice(&mut self, container: Value, slice: Value) -> Result<Value, Trap> {
        let reference = slice.as_ref().ok_or(Trap::TypeError)?;
        let start_v = Value::from_bits(self.heap.read_u32(reference.0));
        let stop_v = Value::from_bits(self.heap.read_u32(reference.0 + 4));
        let step_v = Value::from_bits(self.heap.read_u32(reference.0 + 8));
        let step = if step_v.is_none() {
            1
        } else {
            let step = step_v.as_int().ok_or(Trap::TypeError)?;
            if step == 0 {
                return Err(Trap::ValueError);
            }
            step
        };
        let is_tuple = self.is_tuple(container);
        let selected: Vec<Value> = {
            let elems = self.seq_value(container).ok_or(Trap::TypeError)?;
            let len = elems.len() as i64;
            let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
            let mut out = Vec::new();
            let mut i = start;
            while (step > 0 && i < stop) || (step < 0 && i > stop) {
                if i >= 0 && i < len {
                    out.push(elems[i as usize]);
                }
                i += step;
            }
            out
        };
        if is_tuple {
            self.new_tuple(selected)
        } else {
            self.new_list(selected)
        }
    }

    /// The bytes a slice selects from a `bytes`/`bytearray` (clamping, negative bounds, any step).
    fn slice_bytes(&self, container: Value, slice: Value) -> Result<Vec<u8>, Trap> {
        let reference = slice.as_ref().ok_or(Trap::TypeError)?;
        let start_v = Value::from_bits(self.heap.read_u32(reference.0));
        let stop_v = Value::from_bits(self.heap.read_u32(reference.0 + 4));
        let step_v = Value::from_bits(self.heap.read_u32(reference.0 + 8));
        let step = if step_v.is_none() {
            1
        } else {
            let step = step_v.as_int().ok_or(Trap::TypeError)?;
            if step == 0 {
                return Err(Trap::ValueError);
            }
            step
        };
        let data = self.bytes_value(container).ok_or(Trap::TypeError)?;
        let len = data.len() as i64;
        let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
        let mut out = Vec::new();
        let mut i = start;
        while (step > 0 && i < stop) || (step < 0 && i > stop) {
            if i >= 0 && i < len {
                out.push(data[i as usize]);
            }
            i += step;
        }
        Ok(out)
    }

    fn str_getitem_slice(&mut self, container: Value, slice: Value) -> Result<Value, Trap> {
        let reference = slice.as_ref().ok_or(Trap::TypeError)?;
        let start_v = Value::from_bits(self.heap.read_u32(reference.0));
        let stop_v = Value::from_bits(self.heap.read_u32(reference.0 + 4));
        let step_v = Value::from_bits(self.heap.read_u32(reference.0 + 8));
        let step = if step_v.is_none() {
            1
        } else {
            let step = step_v.as_int().ok_or(Trap::TypeError)?;
            if step == 0 {
                return Err(Trap::ValueError);
            }
            step
        };
        let out = {
            let s = self.str_value(container).ok_or(Trap::TypeError)?;
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
            let mut out = String::new();
            let mut i = start;
            while (step > 0 && i < stop) || (step < 0 && i > stop) {
                if i >= 0 && i < len {
                    out.push(chars[i as usize]);
                }
                i += step;
            }
            out
        };
        self.new_str(&out)
    }

    /// Builds a `slice(start, stop, step)` object (each bound an int or `None`) -- the value
    /// `Op::BuildSlice` pushes and `Subscript` consumes. A small GC-leaf heap object.
    pub fn new_slice(&mut self, start: Value, stop: Value, step: Value) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.slice_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, start.bits());
        self.heap.write_u32(reference.0 + 4, stop.bits());
        self.heap.write_u32(reference.0 + 8, step.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a slice object (the value `Op::BuildSlice` produces).
    #[must_use]
    pub fn is_slice(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|reference| self.heap.type_id_of(reference) == self.slice_type_id)
    }

    /// Allocates a `range(start, stop, step)` -- a lazy int sequence (the bounds are fixnums,
    /// so an i32-range; a wider range would overflow, matching the corpus's needs).
    pub fn new_range(&mut self, start: i64, stop: i64, step: i64) -> Result<Value, Trap> {
        let s = Value::fixnum(i32::try_from(start).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)?;
        let e = Value::fixnum(i32::try_from(stop).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)?;
        let t = Value::fixnum(i32::try_from(step).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)?;
        let reference = self.alloc_object(self.range_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, s.bits());
        self.heap.write_u32(reference.0 + 4, e.bits());
        self.heap.write_u32(reference.0 + 8, t.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `range`.
    #[must_use]
    pub fn is_range(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|reference| self.heap.type_id_of(reference) == self.range_type_id)
    }

    /// Whether `value` is an iterator object (the value [`Self::new_iter`] produces).
    #[must_use]
    pub fn is_iter(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|reference| self.heap.type_id_of(reference) == self.iter_type_id)
    }

    /// The `(start, stop, step)` of a range (the caller has established `is_range`).
    fn range_bounds(&self, value: Value) -> (i64, i64, i64) {
        let reference = value.as_ref().expect("a range");
        let start = Value::from_bits(self.heap.read_u32(reference.0)).as_int().unwrap_or(0);
        let stop = Value::from_bits(self.heap.read_u32(reference.0 + 4)).as_int().unwrap_or(0);
        let step = Value::from_bits(self.heap.read_u32(reference.0 + 8)).as_int().unwrap_or(1);
        (start, stop, step)
    }

    /// The backing-arena index of `value` if its heap object has type `type_id`.
    fn container_slot(&self, value: Value, type_id: u32) -> Option<usize> {
        let reference = value.as_ref()?;
        (self.heap.type_id_of(reference) == type_id).then(|| self.heap.read_u32(reference.0) as usize)
    }

    /// The `seqs`-arena index if `value` is a `list` or `tuple`.
    fn seq_slot(&self, value: Value) -> Option<usize> {
        self.container_slot(value, self.list_type_id)
            .or_else(|| self.container_slot(value, self.tuple_type_id))
    }

    /// The elements if `value` is a `list` or `tuple`.
    fn seq_value(&self, value: Value) -> Option<&Vec<Value>> {
        self.seq_slot(value).and_then(|i| self.seqs.get(i))
    }

    /// The key/value pairs if `value` is a `dict`.
    fn dict_value(&self, value: Value) -> Option<&Vec<(Value, Value)>> {
        self.container_slot(value, self.dict_type_id)
            .and_then(|i| self.dicts.get(i))
    }

    /// A clone of a dict's `(key, value)` pairs, if `value` is a dict (so a caller can rebuild
    /// or copy the dict without holding a borrow on the model). `dict(other_dict)`.
    #[must_use]
    pub fn dict_entries(&self, value: Value) -> Option<Vec<(Value, Value)>> {
        self.dict_value(value).cloned()
    }

    /// Whether `value` is a `list`.
    #[must_use]
    pub fn is_list(&self, value: Value) -> bool {
        self.container_slot(value, self.list_type_id).is_some()
    }

    /// Whether `value` is a `tuple`.
    #[must_use]
    pub fn is_tuple(&self, value: Value) -> bool {
        self.container_slot(value, self.tuple_type_id).is_some()
    }

    /// Whether `value` is a `dict`.
    #[must_use]
    pub fn is_dict(&self, value: Value) -> bool {
        self.container_slot(value, self.dict_type_id).is_some()
    }

    /// Allocates a `list` over `elements` (a mutable sequence). The elements live in the
    /// backing arena; the heap object holds the index.
    pub fn new_list(&mut self, elements: Vec<Value>) -> Result<Value, Trap> {
        let index = self.seqs.len() as u32;
        self.seqs.push(elements);
        let reference = self.alloc_object(self.list_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Allocates a `tuple` over `elements` (an immutable sequence).
    pub fn new_tuple(&mut self, elements: Vec<Value>) -> Result<Value, Trap> {
        let index = self.seqs.len() as u32;
        self.seqs.push(elements);
        let reference = self.alloc_object(self.tuple_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Allocates a `dict` over `pairs`, in insertion order, collapsing duplicate keys with
    /// the last value winning (Python `{...}` display semantics; the key keeps its first
    /// position).
    pub fn new_dict(&mut self, pairs: Vec<(Value, Value)>) -> Result<Value, Trap> {
        let mut entries: Vec<(Value, Value)> = Vec::new();
        for (key, value) in pairs {
            match entries.iter().position(|(k, _)| self.key_eq(*k, key)) {
                Some(slot) => entries[slot].1 = value,
                None => entries.push((key, value)),
            }
        }
        let index = self.dicts.len() as u32;
        self.dicts.push(entries);
        let reference = self.alloc_object(self.dict_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Allocates a `set`/`frozenset` over `elements`, deduped by value equality in first-seen
    /// order, into the shared arena under `type_id`.
    fn alloc_set(&mut self, elements: Vec<Value>, type_id: u32) -> Result<Value, Trap> {
        let mut deduped: Vec<Value> = Vec::new();
        for element in elements {
            if !deduped.iter().any(|e| self.key_eq(*e, element)) {
                deduped.push(element);
            }
        }
        let index = self.sets.len() as u32;
        self.sets.push(deduped);
        let reference = self.alloc_object(type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Allocates a `set` over `elements`, deduped by value equality, in first-seen order.
    pub fn new_set(&mut self, elements: Vec<Value>) -> Result<Value, Trap> {
        let type_id = self.set_type_id;
        self.alloc_set(elements, type_id)
    }

    /// Allocates a `frozenset` over `elements` (an immutable set).
    pub fn new_frozenset(&mut self, elements: Vec<Value>) -> Result<Value, Trap> {
        let type_id = self.frozenset_type_id;
        self.alloc_set(elements, type_id)
    }

    /// Whether `value` is a `set`.
    #[must_use]
    pub fn is_set(&self, value: Value) -> bool {
        self.container_slot(value, self.set_type_id).is_some()
    }

    /// Whether `value` is a `frozenset`.
    #[must_use]
    pub fn is_frozenset(&self, value: Value) -> bool {
        self.container_slot(value, self.frozenset_type_id).is_some()
    }

    /// The elements if `value` is a `set` or `frozenset` (both back onto the shared arena, so
    /// every read op -- len, `in`, iteration, repr, truthiness -- works for either).
    fn set_value(&self, value: Value) -> Option<&Vec<Value>> {
        let slot = self
            .container_slot(value, self.set_type_id)
            .or_else(|| self.container_slot(value, self.frozenset_type_id))?;
        self.sets.get(slot)
    }

    /// Adds `value` to the set (a no-op if an equal element is present) -- `set.add` and the
    /// `SetAdd` comprehension op.
    pub fn set_add(&mut self, set: Value, value: Value) -> Result<(), Trap> {
        let index = self.container_slot(set, self.set_type_id).ok_or(Trap::TypeError)?;
        if !self.sets[index].iter().any(|e| self.key_eq(*e, value)) {
            self.sets[index].push(value);
        }
        Ok(())
    }

    /// Appends `value` to the list in place -- `list.append` and the `ListAppend` comprehension op.
    pub fn list_append(&mut self, list: Value, value: Value) -> Result<(), Trap> {
        let index = self.container_slot(list, self.list_type_id).ok_or(Trap::TypeError)?;
        self.seqs[index].push(value);
        Ok(())
    }

    /// Python value equality for container keys/membership over the value subset we have:
    /// `int`/`bool` compare numerically (so `True == 1`), `str` by content, everything else
    /// by identity (`None`, the same object). Enough for `in`, dict keys, and `==` on these.
    fn key_eq(&self, a: Value, b: Value) -> bool {
        if let (Some(x), Some(y)) = (self.as_i128(a), self.as_i128(b)) {
            return x == y;
        }
        if self.is_int(a) && self.is_int(b) {
            return self.as_bigint(a) == self.as_bigint(b);
        }
        if self.is_float(a) || self.is_float(b) {
            if let (Some(x), Some(y)) = (self.as_f64(a), self.as_f64(b)) {
                return x == y;
            }
        }
        if let (Some(x), Some(y)) = (self.str_value(a), self.str_value(b)) {
            return x == y;
        }
        if let (Some(x), Some(y)) = (self.bytes_value(a), self.bytes_value(b)) {
            return x == y;
        }
        a == b
    }

    /// `container[index] = value` (`Op::Setitem`): a `list` stores at an int index (negative
    /// from the end, `IndexError` out of range); a `dict` inserts or updates `index` as the
    /// key. A `tuple`/`str`/other is not assignable (`TypeError`).
    pub fn py_setitem(&mut self, container: Value, index: Value, value: Value) -> Result<(), Trap> {
        if self.is_bytearray(container) {
            let slot = self.byte_buffer_slot(container).ok_or(Trap::TypeError)?;
            let len = self.byte_buffers[slot].len() as i64;
            let at = index.as_int().ok_or(Trap::TypeError)?;
            let at = if at < 0 { at + len } else { at };
            if at < 0 || at >= len {
                return Err(self.with_message(Trap::IndexError, "bytearray index out of range"));
            }
            let byte = value.as_int().ok_or(Trap::TypeError)?;
            if !(0..=255).contains(&byte) {
                return Err(Trap::ValueError);
            }
            self.byte_buffers[slot][at as usize] = byte as u8;
            return Ok(());
        }
        if let Some(i) = self.container_slot(container, self.list_type_id) {
            let len = self.seqs[i].len() as i64;
            let at = index.as_int().ok_or(Trap::TypeError)?;
            let at = if at < 0 { at + len } else { at };
            if at < 0 || at >= len {
                return Err(Trap::IndexError);
            }
            self.seqs[i][at as usize] = value;
            return Ok(());
        }
        if let Some(i) = self.container_slot(container, self.dict_type_id) {
            match self.dicts[i].iter().position(|(k, _)| self.key_eq(*k, index)) {
                Some(slot) => self.dicts[i][slot].1 = value,
                None => self.dicts[i].push((index, value)),
            }
            return Ok(());
        }
        Err(Trap::TypeError)
    }

    /// `list[slice] = elements` (`Op::Setitem` with a slice index): replaces the slice with the
    /// already-collected RHS `elements`. A step-1 slice SPLICES (the list may change length --
    /// `xs[1:3] = [a, b, c]`); an extended slice (step != 1) assigns element-wise and requires the
    /// RHS length to equal the slice length (else a `ValueError`). Bounds resolve exactly like a
    /// slice read (clamping, negative indices). The RHS is collected by the caller (it may be any
    /// iterable, including a generator, which needs the interpreter).
    pub fn seq_setitem_slice(&mut self, container: Value, slice: Value, elements: Vec<Value>) -> Result<(), Trap> {
        let reference = slice.as_ref().ok_or(Trap::TypeError)?;
        let start_v = Value::from_bits(self.heap.read_u32(reference.0));
        let stop_v = Value::from_bits(self.heap.read_u32(reference.0 + 4));
        let step_v = Value::from_bits(self.heap.read_u32(reference.0 + 8));
        let step = if step_v.is_none() {
            1
        } else {
            let step = step_v.as_int().ok_or(Trap::TypeError)?;
            if step == 0 {
                return Err(Trap::ValueError);
            }
            step
        };
        let i = self.container_slot(container, self.list_type_id).ok_or(Trap::TypeError)?;
        let len = self.seqs[i].len() as i64;
        let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
        if step == 1 {
            let low = start.clamp(0, len) as usize;
            let high = stop.clamp(start, len) as usize;
            self.seqs[i].splice(low..high, elements);
        } else {
            let mut indices = Vec::new();
            let mut at = start;
            while (step > 0 && at < stop) || (step < 0 && at > stop) {
                if at >= 0 && at < len {
                    indices.push(at as usize);
                }
                at += step;
            }
            if indices.len() != elements.len() {
                let message = alloc::format!(
                    "attempt to assign sequence of size {} to extended slice of size {}",
                    elements.len(),
                    indices.len()
                );
                return Err(self.with_message(Trap::ValueError, &message));
            }
            for (index, value) in indices.into_iter().zip(elements) {
                self.seqs[i][index] = value;
            }
        }
        Ok(())
    }

    /// `del container[index]` (a future `Op::DeleteItem`): a `list` removes an int index (negative
    /// from the end, `IndexError` out of range) or the elements a slice selects (the list shrinks);
    /// a `dict` removes `index` as a key (`KeyError` if absent). A `tuple`/`str`/other is a
    /// `TypeError`. Ready for the frontend's `del x[i]` emission (co-design); an instance's
    /// `__delitem__` is dispatched by the interpreter before this.
    pub fn py_delitem(&mut self, container: Value, index: Value) -> Result<(), Trap> {
        if let Some(i) = self.container_slot(container, self.list_type_id) {
            if self.is_slice(index) {
                return self.seq_delitem_slice(container, index);
            }
            let len = self.seqs[i].len() as i64;
            let at = index.as_int().ok_or(Trap::TypeError)?;
            let at = if at < 0 { at + len } else { at };
            if at < 0 || at >= len {
                return Err(self.with_message(Trap::IndexError, "list assignment index out of range"));
            }
            self.seqs[i].remove(at as usize);
            return Ok(());
        }
        if let Some(i) = self.container_slot(container, self.dict_type_id) {
            return match self.dicts[i].iter().position(|(k, _)| self.key_eq(*k, index)) {
                Some(slot) => {
                    self.dicts[i].remove(slot);
                    Ok(())
                }
                None => {
                    self.set_trap_arg(index);
                    Err(Trap::KeyError)
                }
            };
        }
        Err(Trap::TypeError)
    }

    /// `del list[slice]` -- removes the elements the slice selects; the list shrinks. Bounds resolve
    /// exactly like a slice read.
    fn seq_delitem_slice(&mut self, container: Value, slice: Value) -> Result<(), Trap> {
        let reference = slice.as_ref().ok_or(Trap::TypeError)?;
        let start_v = Value::from_bits(self.heap.read_u32(reference.0));
        let stop_v = Value::from_bits(self.heap.read_u32(reference.0 + 4));
        let step_v = Value::from_bits(self.heap.read_u32(reference.0 + 8));
        let step = if step_v.is_none() {
            1
        } else {
            let step = step_v.as_int().ok_or(Trap::TypeError)?;
            if step == 0 {
                return Err(Trap::ValueError);
            }
            step
        };
        let i = self.container_slot(container, self.list_type_id).ok_or(Trap::TypeError)?;
        let len = self.seqs[i].len() as i64;
        let (start, stop) = adjust_slice(start_v, stop_v, step, len)?;
        if step == 1 {
            let low = start.clamp(0, len) as usize;
            let high = stop.clamp(start, len) as usize;
            self.seqs[i].drain(low..high);
        } else {
            let mut indices = Vec::new();
            let mut at = start;
            while (step > 0 && at < stop) || (step < 0 && at > stop) {
                if at >= 0 && at < len {
                    indices.push(at as usize);
                }
                at += step;
            }
            indices.sort_unstable();
            for index in indices.into_iter().rev() {
                self.seqs[i].remove(index);
            }
        }
        Ok(())
    }

    /// `element in container` (`Op::Contains`): substring for `str`, membership for a
    /// `list`/`tuple` (any element equals), key membership for a `dict`.
    pub fn py_contains(&self, container: Value, element: Value) -> Result<bool, Trap> {
        if let Some(s) = self.str_value(container) {
            let sub = self.str_value(element).ok_or(Trap::TypeError)?;
            return Ok(s.contains(sub));
        }
        if let Some(data) = self.bytes_value(container) {
            if let Some(byte) = element.as_int() {
                if !(0..=255).contains(&byte) {
                    return Err(Trap::ValueError);
                }
                return Ok(data.contains(&(byte as u8)));
            }
            let needle = self.bytes_value(element).ok_or(Trap::TypeError)?;
            return Ok(needle.is_empty() || data.windows(needle.len()).any(|w| w == needle));
        }
        if let Some(elems) = self.seq_value(container) {
            return Ok(elems.iter().any(|&e| self.key_eq(e, element)));
        }
        if let Some(entries) = self.dict_value(container) {
            return Ok(entries.iter().any(|(k, _)| self.key_eq(*k, element)));
        }
        if let Some(elements) = self.set_value(container) {
            return Ok(elements.iter().any(|&e| self.key_eq(e, element)));
        }
        Err(Trap::TypeError)
    }

    /// The Python `repr()` of `value` over the value subset we have, so a container (and its
    /// elements) prints as CPython does. A top-level `str` is printed raw by `print()`, but a
    /// `str` nested in a container is repr'd (quoted); this is the quoted form.
    #[must_use]
    pub fn repr(&self, value: Value) -> String {
        if value == Value::TRUE {
            return String::from("True");
        }
        if value == Value::FALSE {
            return String::from("False");
        }
        if value.is_none() {
            return String::from("None");
        }
        if let Some(n) = value.as_fixnum() {
            return alloc::format!("{n}");
        }
        if let Some(n) = self.long_value(value) {
            return alloc::format!("{n}");
        }
        if let Some(big) = self.bigint_value(value) {
            return big.to_decimal_string();
        }
        if let Some(f) = self.float_value(value) {
            return format_float(f);
        }
        #[cfg(feature = "complex")]
        if let Some((re, im)) = self.complex_value(value) {
            return format_complex(re, im);
        }
        if let Some(s) = self.str_value(value) {
            return str_repr(s);
        }
        if let Some(data) = self.bytes_value(value) {
            return bytes_repr(data, self.is_bytearray(value));
        }
        if self.is_range(value) {
            let (start, stop, step) = self.range_bounds(value);
            return if step == 1 {
                alloc::format!("range({start}, {stop})")
            } else {
                alloc::format!("range({start}, {stop}, {step})")
            };
        }
        if let Some(elems) = self.seq_value(value) {
            let is_tuple = self.is_tuple(value);
            let len = elems.len();
            let inner = elems
                .iter()
                .map(|&e| self.repr(e))
                .collect::<Vec<_>>()
                .join(", ");
            return if is_tuple {
                if len == 1 {
                    alloc::format!("({inner},)")
                } else {
                    alloc::format!("({inner})")
                }
            } else {
                alloc::format!("[{inner}]")
            };
        }
        if let Some(elements) = self.set_value(value) {
            let frozen = self.is_frozenset(value);
            if elements.is_empty() {
                return String::from(if frozen { "frozenset()" } else { "set()" });
            }
            let inner = elements
                .iter()
                .map(|&e| self.repr(e))
                .collect::<Vec<_>>()
                .join(", ");
            return if frozen {
                alloc::format!("frozenset({{{inner}}})")
            } else {
                alloc::format!("{{{inner}}}")
            };
        }
        if let Some(entries) = self.dict_value(value) {
            let inner = entries
                .iter()
                .map(|(k, v)| alloc::format!("{}: {}", self.repr(*k), self.repr(*v)))
                .collect::<Vec<_>>()
                .join(", ");
            return alloc::format!("{{{inner}}}");
        }
        if let Some(id) = value.as_builtin_id() {
            if let Some(builtin) = Builtin::from_id(id) {
                return if builtin.is_type() {
                    alloc::format!("<class '{}'>", builtin.python_name())
                } else {
                    alloc::format!("<built-in function {}>", builtin.python_name())
                };
            }
        }
        if self.is_class(value) {
            let name = self.str_value(self.read_slot(value, 0)).unwrap_or("?");
            return alloc::format!("<class '__main__.{name}'>");
        }
        if self.is_instance(value) {
            let name = self.instance_class_name(value).unwrap_or("object");
            return alloc::format!("<{name} object>");
        }
        alloc::format!("{value:?}")
    }

    /// `str(value)` (the Python builtin): a `str` is returned unchanged; an int/bool/None
    /// render as `print()` shows them; a container uses its `repr`. Allocates a new `str`
    /// (except when `value` is already one).
    pub fn py_str(&mut self, value: Value) -> Result<Value, Trap> {
        if self.is_str(value) {
            return Ok(value);
        }
        let rendered = if let Some(n) = value.as_fixnum() {
            alloc::format!("{n}")
        } else if value == Value::TRUE {
            String::from("True")
        } else if value == Value::FALSE {
            String::from("False")
        } else if value.is_none() {
            String::from("None")
        } else if self.is_instance(value) {
            self.instance_display(value)
        } else {
            self.repr(value)
        };
        self.new_str(&rendered)
    }

    /// `hash(value)` for a hashable value: int/bool `n` -> `n` (except `hash(-1) == -2`, CPython's
    /// error sentinel); `None` -> a stable constant; `str`/`tuple` -> a deterministic FNV-1a hash
    /// folded into the fixnum range (NOT CPython's randomized hash -- the value differs but is
    /// stable within a run). `list`/`dict`/`set` (and a tuple containing one) are unhashable ->
    /// `TypeError`, matching CPython.
    pub fn py_hash(&self, value: Value) -> Result<Value, Trap> {
        if let Some(n) = value.as_int() {
            let h = if n == -1 { -2 } else { n };
            return Value::fixnum(i32::try_from(h).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow);
        }
        if let Some(big) = self.long_value(value) {
            let bits = big as u128;
            let folded = (bits ^ (bits >> 64)) as u32;
            return Value::fixnum((folded & 0x3FFF_FFFF) as i32).ok_or(Trap::Overflow);
        }
        if let Some(bigint) = self.bigint_value(value) {
            let mut folded: u32 = if bigint.is_negative() { 2_166_136_261 } else { 16_777_619 };
            for byte in bigint.to_decimal_string().bytes() {
                folded = folded.wrapping_mul(16_777_619) ^ u32::from(byte);
            }
            return Value::fixnum((folded & 0x3FFF_FFFF) as i32).ok_or(Trap::Overflow);
        }
        if let Some(f) = self.float_value(value) {
            return Ok(self.py_hash_float(f));
        }
        #[cfg(feature = "complex")]
        if let Some((re, im)) = self.complex_value(value) {
            let hash_real = i64::from(self.py_hash_float(re).as_fixnum().unwrap_or(0));
            let hash_imag = i64::from(self.py_hash_float(im).as_fixnum().unwrap_or(0));
            let combined = hash_real.wrapping_add(1_000_003_i64.wrapping_mul(hash_imag));
            return Value::fixnum((combined & 0x3FFF_FFFF) as i32).ok_or(Trap::Overflow);
        }
        if value.is_none() {
            return Value::fixnum(0).ok_or(Trap::Overflow);
        }
        let folded = self.hash_nonint(value)?;
        Value::fixnum((folded & 0x3FFF_FFFF) as i32).ok_or(Trap::Overflow)
    }

    /// The deterministic FNV-1a hash of a `str`/`tuple` (recursive), or a `TypeError` for an
    /// unhashable value. Backs [`ObjectModel::py_hash`] for the non-int cases.
    fn hash_nonint(&self, value: Value) -> Result<u32, Trap> {
        if let Some(s) = self.str_value(value) {
            let mut hash: u32 = 2_166_136_261;
            for byte in s.bytes() {
                hash ^= u32::from(byte);
                hash = hash.wrapping_mul(16_777_619);
            }
            return Ok(hash);
        }
        if self.is_tuple(value) {
            let elements = self.seq_value(value).ok_or(Trap::TypeError)?;
            let mut hash: u32 = 2_166_136_261;
            for &element in elements {
                let element_hash = self.py_hash(element)?.as_fixnum().unwrap_or(0) as u32;
                hash ^= element_hash;
                hash = hash.wrapping_mul(16_777_619);
            }
            return Ok(hash);
        }
        Err(Trap::TypeError)
    }

    /// `hash(float)`, folded into the fixnum range. The load-bearing invariant is Python's numeric
    /// hash rule: a value that equals an int hashes EQUAL to that int (`hash(2.0) == hash(2)`), so
    /// mixed int/float dict and set keys collapse; `+0.0` and `-0.0` both equal int `0`, so they
    /// hash alike. `inf`/`-inf` use CPython's `+-314159` sentinels; a `NaN` gets a stable `0` (its
    /// value is never CPython's object-identity hash, but a NaN never matches anything anyway).
    /// A non-integral float uses a deterministic fold of its bits (stable within a run, not
    /// CPython's `_Py_HashDouble` value -- the same documented divergence as the str/tuple hash).
    #[must_use]
    fn py_hash_float(&self, value: f64) -> Value {
        let fixnum = |n: i32| Value::fixnum(n).unwrap_or(Value::fixnum(0).unwrap());
        if value.is_nan() {
            return fixnum(0);
        }
        if value.is_infinite() {
            return fixnum(if value > 0.0 { 314_159 } else { -314_159 });
        }
        if libm::floor(value) == value && (-9.223_372_036_854_776e18..9.223_372_036_854_776e18).contains(&value) {
            let n = i128::from(value as i64);
            if n >= i128::from(FIXNUM_MIN) && n <= i128::from(FIXNUM_MAX) {
                return fixnum(if n == -1 { -2 } else { n as i32 });
            }
            let bits = n as u128;
            return fixnum(((bits ^ (bits >> 64)) as u32 & 0x3FFF_FFFF) as i32);
        }
        let bits = value.to_bits();
        fixnum(((bits ^ (bits >> 32)) as u32 & 0x3FFF_FFFF) as i32)
    }

    /// An `int` value from an `i128`, normalized: one that fits the fixnum range stays a fixnum (so
    /// `10**10 == 10**10` and small results never allocate); a bigger one becomes a heap `long`.
    /// This is the i128-range first increment; a result outside i128 is a `Trap::Overflow` (true
    /// arbitrary precision -- limb arithmetic -- is the follow-on).
    pub fn new_long(&mut self, n: i128) -> Result<Value, Trap> {
        if n >= i128::from(FIXNUM_MIN) && n <= i128::from(FIXNUM_MAX) {
            return Value::fixnum(n as i32).ok_or(Trap::Overflow);
        }
        let reference = self.alloc_object(self.long_type_id).ok_or(Trap::OutOfMemory)?;
        let bits = n as u128;
        for word in 0..4u32 {
            self.heap.write_u32(reference.0 + word * 4, (bits >> (word * 32)) as u32);
        }
        Ok(Value::from_ref(reference))
    }

    /// The i128 a `long` holds, or `None` if `value` is not a long.
    #[must_use]
    pub fn long_value(&self, value: Value) -> Option<i128> {
        let reference = value.as_ref()?;
        if self.heap.type_id_of(reference) != self.long_type_id {
            return None;
        }
        let mut bits: u128 = 0;
        for word in 0..4u32 {
            bits |= u128::from(self.heap.read_u32(reference.0 + word * 4)) << (word * 32);
        }
        Some(bits as i128)
    }

    /// The integer value of `value` as an i128 -- a fixnum/bool or a `long`; `None` for a non-int.
    /// The single reader the arithmetic/comparison core uses so it treats both int kinds uniformly.
    #[must_use]
    pub fn as_i128(&self, value: Value) -> Option<i128> {
        if let Some(n) = value.as_int() {
            return Some(i128::from(n));
        }
        self.long_value(value)
    }

    /// Whether `value` is a `long` (a big int); a plain int-valued fixnum is NOT a long.
    #[must_use]
    pub fn is_long(&self, value: Value) -> bool {
        self.long_value(value).is_some()
    }

    /// An `int` from a [`BigInt`], normalized DOWN to the smallest representation: a value that fits
    /// the fixnum range stays a fixnum, one that fits `i128` becomes a `long`, and only a truly
    /// arbitrary-precision value allocates a `bigint`. So the three int tiers never overlap and a
    /// shrunk result (`big - big`) collapses back automatically.
    pub fn new_bigint(&mut self, value: BigInt) -> Result<Value, Trap> {
        if let Some(n) = value.to_i128() {
            return self.new_long(n);
        }
        let reference = self.alloc_object(self.bigint_type_id).ok_or(Trap::OutOfMemory)?;
        let index = self.bigints.len();
        self.bigints.push(value);
        self.heap.write_u32(reference.0, index as u32);
        Ok(Value::from_ref(reference))
    }

    /// The [`BigInt`] a `bigint` object holds, or `None` if `value` is not a bigint (a fixnum/long is
    /// NOT a bigint -- use [`ObjectModel::as_bigint`] to read any int as a `BigInt`).
    #[must_use]
    pub fn bigint_value(&self, value: Value) -> Option<&BigInt> {
        let reference = value.as_ref()?;
        if self.heap.type_id_of(reference) != self.bigint_type_id {
            return None;
        }
        let index = self.heap.read_u32(reference.0) as usize;
        self.bigints.get(index)
    }

    /// Any int -- a fixnum/bool, an i128 `long`, or a `bigint` -- as a [`BigInt`]; `None` for a
    /// non-int. The reader the arbitrary-precision arithmetic core uses so it treats all three int
    /// tiers uniformly (the `BigInt` sibling of [`ObjectModel::as_i128`]).
    #[must_use]
    pub fn as_bigint(&self, value: Value) -> Option<BigInt> {
        if let Some(n) = self.as_i128(value) {
            return Some(BigInt::from_i128(n));
        }
        self.bigint_value(value).cloned()
    }

    /// Whether `value` is a `bigint` (an int beyond the i128 range); a fixnum/`long` is NOT a bigint.
    #[must_use]
    pub fn is_bigint(&self, value: Value) -> bool {
        self.bigint_value(value).is_some()
    }

    /// Whether `value` is any Python `int` (a fixnum, a `bool`, an i128 `long`, or a `bigint`) --
    /// the three integer tiers plus `bool` (an int subtype). NOT a float/complex.
    #[must_use]
    pub fn is_int(&self, value: Value) -> bool {
        value.is_fixnum()
            || value == Value::TRUE
            || value == Value::FALSE
            || self.is_long(value)
            || self.is_bigint(value)
    }

    /// A heap `float` holding the IEEE-754 double `n`. Unlike `new_long`, a float is ALWAYS boxed
    /// (there is no immediate float form -- `Value` is a 32-bit word, an f64 does not fit), so even
    /// `0.0`/`1.0` allocate. Mirrors `new_long`'s heap-leaf storage (two u32 words of raw bits).
    pub fn new_float(&mut self, n: f64) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.float_type_id).ok_or(Trap::OutOfMemory)?;
        let bits = n.to_bits();
        self.heap.write_u32(reference.0, bits as u32);
        self.heap.write_u32(reference.0 + 4, (bits >> 32) as u32);
        Ok(Value::from_ref(reference))
    }

    /// The f64 a `float` holds, or `None` if `value` is not a float (an int/bool is NOT a float --
    /// use [`ObjectModel::as_f64`] for the numeric-coercion reader).
    #[must_use]
    pub fn float_value(&self, value: Value) -> Option<f64> {
        let reference = value.as_ref()?;
        if self.heap.type_id_of(reference) != self.float_type_id {
            return None;
        }
        let low = u64::from(self.heap.read_u32(reference.0));
        let high = u64::from(self.heap.read_u32(reference.0 + 4));
        Some(f64::from_bits((high << 32) | low))
    }

    /// The value of `value` as an f64 for MIXED int/float arithmetic and comparison: a fixnum/bool,
    /// a `long`, or a `float` all read here (an int widens to the nearest double); `None` for a
    /// non-number. The reader the float arithmetic/comparison core uses so `2 + 3.5` and `1 < 1.5`
    /// coerce the int operand -- the float sibling of [`ObjectModel::as_i128`].
    #[must_use]
    pub fn as_f64(&self, value: Value) -> Option<f64> {
        if let Some(n) = value.as_int() {
            return Some(n as f64);
        }
        if let Some(big) = self.long_value(value) {
            return Some(big as f64);
        }
        if let Some(bigint) = self.bigint_value(value) {
            return Some(bigint.to_f64());
        }
        self.float_value(value)
    }

    /// Whether `value` is a `float`; an int-valued fixnum/long is NOT a float (Python keeps the types
    /// distinct -- `1 is not 1.0`, `type(1) is not type(1.0)`).
    #[must_use]
    pub fn is_float(&self, value: Value) -> bool {
        self.float_value(value).is_some()
    }

    /// A heap `complex` from its real and imaginary parts. Always boxed (two f64 do not fit an
    /// immediate). Behind the `complex` knob.
    #[cfg(feature = "complex")]
    pub fn new_complex(&mut self, real: f64, imag: f64) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.complex_type_id).ok_or(Trap::OutOfMemory)?;
        for (word, part) in [real, imag].into_iter().enumerate() {
            let bits = part.to_bits();
            self.heap.write_u32(reference.0 + word as u32 * 8, bits as u32);
            self.heap.write_u32(reference.0 + word as u32 * 8 + 4, (bits >> 32) as u32);
        }
        Ok(Value::from_ref(reference))
    }

    /// The `(real, imag)` a `complex` holds, or `None` if `value` is not a complex.
    #[cfg(feature = "complex")]
    #[must_use]
    pub fn complex_value(&self, value: Value) -> Option<(f64, f64)> {
        let reference = value.as_ref()?;
        if self.heap.type_id_of(reference) != self.complex_type_id {
            return None;
        }
        let read = |offset: u32| {
            let low = u64::from(self.heap.read_u32(reference.0 + offset));
            let high = u64::from(self.heap.read_u32(reference.0 + offset + 4));
            f64::from_bits((high << 32) | low)
        };
        Some((read(0), read(8)))
    }

    /// The value of `value` as a `(real, imag)` pair for MIXED complex arithmetic: an int/bool, a
    /// `long`, or a `float` promotes to `(x, 0.0)`, and a `complex` reads directly; `None` for a
    /// non-number. Lets `2 + 3j` and `(1+2j) + 1` coerce the real operand -- the complex sibling of
    /// [`ObjectModel::as_f64`].
    #[cfg(feature = "complex")]
    #[must_use]
    pub fn as_complex(&self, value: Value) -> Option<(f64, f64)> {
        if let Some(re) = self.as_f64(value) {
            return Some((re, 0.0));
        }
        self.complex_value(value)
    }

    /// Whether `value` is a `complex`.
    #[cfg(feature = "complex")]
    #[must_use]
    pub fn is_complex(&self, value: Value) -> bool {
        self.complex_value(value).is_some()
    }

    /// Wraps a namespace dict in a module object; `module.member` resolves `member` in that dict.
    pub fn new_module(&mut self, namespace: Value) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.module_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, namespace.bits());
        Ok(Value::from_ref(reference))
    }

    /// The namespace dict a module wraps; the precondition is that `value` is a module object.
    #[must_use]
    pub fn module_namespace(&self, value: Value) -> Value {
        self.read_slot(value, 0)
    }

    /// Whether `value` is a provided module object.
    #[must_use]
    pub fn is_module_object(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.module_type_id)
    }

    /// Provides a module `name` backed by `namespace` (a dict of its members), bound as a global so
    /// a program reaches it as `name.member` -- the injection path the native machine/board modules
    /// use, here for a Python-authored namespace. (`import name` is the front-end follow-on; the
    /// loader that RUNS a `.py` module body to build `namespace` is the co-design follow-on.)
    pub fn provide_module(&mut self, name: &str, namespace: Value) -> Result<(), Trap> {
        let module = self.new_module(namespace)?;
        self.set_global(name, module);
        Ok(())
    }

    /// `iter(iterable)` (`Op::GetIter`): an iterator over a `str`/`list`/`tuple`/`dict` (a
    /// dict iterates its keys). A non-iterable value is a `TypeError`.
    /// A fresh `Cell` boxing `value` (the closure primitive: a shared mutable box for a variable
    /// captured by a nested function).
    pub fn new_cell(&mut self, value: Value) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.cell_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, value.bits());
        Ok(Value::from_ref(reference))
    }

    /// The value a `Cell` holds. `TypeError` if `cell` is not a Cell (an interpreter invariant).
    pub fn cell_get(&self, cell: Value) -> Result<Value, Trap> {
        let reference = cell.as_ref().ok_or(Trap::TypeError)?;
        if self.heap.type_id_of(reference) != self.cell_type_id {
            return Err(Trap::TypeError);
        }
        Ok(Value::from_bits(self.heap.read_u32(reference.0)))
    }

    /// Stores `value` into a `Cell` (a `nonlocal`/enclosing-scope write is visible to every holder).
    pub fn cell_set(&mut self, cell: Value, value: Value) -> Result<(), Trap> {
        let reference = cell.as_ref().ok_or(Trap::TypeError)?;
        if self.heap.type_id_of(reference) != self.cell_type_id {
            return Err(Trap::TypeError);
        }
        self.heap.write_u32(reference.0, value.bits());
        Ok(())
    }

    /// Whether `value` is a `Cell`.
    #[must_use]
    pub fn is_cell(&self, value: Value) -> bool {
        value.as_ref().is_some_and(|r| self.heap.type_id_of(r) == self.cell_type_id)
    }

    pub fn new_iter(&mut self, iterable: Value) -> Result<Value, Trap> {
        if self.is_iter(iterable) {
            return Ok(iterable);
        }
        let iterable_ok = self.str_value(iterable).is_some()
            || self.bytes_value(iterable).is_some()
            || self.seq_value(iterable).is_some()
            || self.dict_value(iterable).is_some()
            || self.is_range(iterable)
            || self.set_value(iterable).is_some();
        if !iterable_ok {
            return Err(Trap::TypeError);
        }
        let reference = self.alloc_object(self.iter_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, iterable.bits());
        self.heap.write_u32(reference.0 + 4, 0);
        Ok(Value::from_ref(reference))
    }

    /// Advances an iterator (`Op::ForIter`): `Some(value)` on the next element, `None` at
    /// exhaustion. The iterator stores its container + position; this reads the position-th
    /// element (a sequence element / a dict key / a 1-char `str`) and advances the position.
    pub fn py_next(&mut self, iterator: Value) -> Result<Option<Value>, Trap> {
        let reference = iterator.as_ref().ok_or(Trap::TypeError)?;
        if self.heap.type_id_of(reference) != self.iter_type_id {
            return Err(Trap::TypeError);
        }
        let container = Value::from_bits(self.heap.read_u32(reference.0));
        let pos = self.heap.read_u32(reference.0 + 4) as usize;
        if self.is_range(container) {
            let (start, stop, step) = self.range_bounds(container);
            if pos as i64 >= range_len(start, stop, step) {
                return Ok(None);
            }
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            let element = start + pos as i64 * step;
            return Ok(Some(Value::fixnum(element as i32).ok_or(Trap::Overflow)?));
        }
        if let Some(elems) = self.seq_value(container) {
            if pos >= elems.len() {
                return Ok(None);
            }
            let element = elems[pos];
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            return Ok(Some(element));
        }
        if let Some(entries) = self.dict_value(container) {
            if pos >= entries.len() {
                return Ok(None);
            }
            let key = entries[pos].0;
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            return Ok(Some(key));
        }
        if let Some(elements) = self.set_value(container) {
            if pos >= elements.len() {
                return Ok(None);
            }
            let element = elements[pos];
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            return Ok(Some(element));
        }
        if let Some(data) = self.bytes_value(container) {
            if pos >= data.len() {
                return Ok(None);
            }
            let byte = data[pos];
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            return Value::fixnum(i32::from(byte)).ok_or(Trap::Overflow).map(Some);
        }
        if self.str_value(container).is_some() {
            let ch = {
                let s = self.str_value(container).ok_or(Trap::TypeError)?;
                match s.chars().nth(pos) {
                    Some(c) => c,
                    None => return Ok(None),
                }
            };
            self.heap.write_u32(reference.0 + 4, (pos + 1) as u32);
            let mut buf = [0u8; 4];
            return Ok(Some(self.new_str(ch.encode_utf8(&mut buf))?));
        }
        Err(Trap::TypeError)
    }

    /// Reads tagged slot `i` (4-byte) of a heap object, as a [`Value`]. The caller has
    /// established `value` is the expected heap kind.
    fn read_slot(&self, value: Value, i: u32) -> Value {
        let reference = value.as_ref().expect("a heap object");
        Value::from_bits(self.heap.read_u32(reference.0 + i * 4))
    }

    /// Allocates a class object `[name, base, namespace]` (`Op::BuildClass`). `base` is a
    /// class or `None`; `namespace` is the class body's dict (methods + class attributes).
    pub fn new_class(&mut self, name: Value, base: Value, namespace: Value) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.class_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, name.bits());
        self.heap.write_u32(reference.0 + 4, base.bits());
        self.heap.write_u32(reference.0 + 8, namespace.bits());
        Ok(Value::from_ref(reference))
    }

    /// Allocates an instance of `class` with a fresh empty `__dict__` (the first half of
    /// calling a type; `__init__` runs in the interpreter's Call arm).
    pub fn new_object(&mut self, class: Value) -> Result<Value, Trap> {
        let dict = self.new_dict(Vec::new())?;
        let reference = self.alloc_object(self.instance_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, class.bits());
        self.heap.write_u32(reference.0 + 4, dict.bits());
        Ok(Value::from_ref(reference))
    }

    /// The default construction of a class that has NO user `__init__` but is called with arguments:
    /// a `BaseException` subclass stores its positional args (so `str(exc)` renders the message and
    /// `exc.args` works, like `BaseException.__init__`); any other class ignores them (a lenient
    /// divergence from CPython's "takes no arguments" TypeError).
    pub fn init_default_args(&mut self, instance: Value, args: &[Value]) -> Result<(), Trap> {
        self.ensure_exception_types();
        let base_exception = self.exc_class_lookup("BaseException").ok_or(Trap::Malformed)?;
        if self.is_instance_of(instance, base_exception) {
            let args_tuple = self.new_tuple(args.to_vec())?;
            self.py_setattr_instance(instance, "args", args_tuple)?;
        }
        Ok(())
    }

    /// Allocates a bound Python method `[receiver, func]` -- the value `LoadAttr` yields for
    /// a function found on an instance's class; `Call` prepends the receiver as `self`.
    pub fn new_py_bound(&mut self, receiver: Value, func: Value) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.py_bound_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, receiver.bits());
        self.heap.write_u32(reference.0 + 4, func.bits());
        Ok(Value::from_ref(reference))
    }

    /// Allocates a `PyFunction` -- a DEFAULTED function `[func_index, defaults, kwdefaults]`. Only a
    /// `def` (or lambda) carrying default args needs this; a plain function stays a `function_ref`
    /// immediate. `defaults` is a tuple (or `None`); `kwdefaults` a dict (or `None`).
    pub fn new_py_function(
        &mut self,
        func_index: u32,
        defaults: Value,
        kwdefaults: Value,
    ) -> Result<Value, Trap> {
        let reference = self
            .heap
            .alloc(self.py_function_type_id)
            .ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, func_index);
        self.heap.write_u32(reference.0 + 4, defaults.bits());
        self.heap.write_u32(reference.0 + 8, kwdefaults.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `PyFunction` (a defaulted function object).
    #[must_use]
    pub fn is_py_function(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.py_function_type_id)
    }

    /// The module-function index a `PyFunction` refers to.
    #[must_use]
    pub fn py_function_index(&self, func: Value) -> u32 {
        let reference = func.as_ref().expect("a PyFunction");
        self.heap.read_u32(reference.0)
    }

    /// The positional DEFAULTS of a `PyFunction` as a vector (the defaults tuple's elements, or
    /// empty if it has none). They align to the trailing positional parameters at bind time.
    #[must_use]
    pub fn py_function_defaults(&self, func: Value) -> Vec<Value> {
        let reference = func.as_ref().expect("a PyFunction");
        let defaults = Value::from_bits(self.heap.read_u32(reference.0 + 4));
        self.seq_value(defaults).cloned().unwrap_or_default()
    }

    /// Allocates a generator object owning `frame` (its fresh, not-yet-run activation, with args
    /// already bound to locals), returning the heap value. The body does not run until the first
    /// resume; the frame lives in the `generators` arena and the heap object holds its index.
    pub fn new_generator(&mut self, frame: Frame) -> Result<Value, Trap> {
        let index = self.generators.len() as u32;
        self.generators.push(Some(frame));
        let reference = self.alloc_object(self.generator_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, index);
        Ok(Value::from_ref(reference))
    }

    /// Takes a recycled frame from the pool (its Vec buffers ready to reuse), or `None` if empty.
    pub(crate) fn take_pooled_frame(&mut self) -> Option<Frame> {
        self.frame_pool.pop()
    }

    /// Returns a finished call frame to the pool (cleared of Values, buffers kept), bounded so a
    /// deep-but-transient call chain does not permanently retain buffers.
    pub(crate) fn recycle_frame(&mut self, mut frame: Frame) {
        const FRAME_POOL_CAP: usize = 256;
        if self.frame_pool.len() < FRAME_POOL_CAP {
            frame.clear_for_reuse();
            self.frame_pool.push(frame);
        }
    }

    /// Whether `value` is a generator object.
    #[must_use]
    pub fn is_generator(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.generator_type_id)
    }

    /// Takes the suspended frame OUT of generator `gen` (leaving the slot `None`), or `None` if it
    /// has already been taken -- the generator is exhausted, or is currently running (a re-entrant
    /// resume). Pair with [`ObjectModel::put_generator_frame`] to suspend it again after a yield.
    pub fn take_generator_frame(&mut self, generator: Value) -> Option<Frame> {
        let reference = generator.as_ref()?;
        if self.heap.type_id_of(reference) != self.generator_type_id {
            return None;
        }
        let index = self.heap.read_u32(reference.0) as usize;
        self.generators.get_mut(index)?.take()
    }

    /// Suspends `frame` back into generator `gen` after it yielded, so the next resume continues
    /// it. The slot was emptied by [`ObjectModel::take_generator_frame`]; leaving it `None`
    /// instead (never calling this) marks the generator exhausted.
    pub fn put_generator_frame(&mut self, generator: Value, frame: Frame) {
        if let Some(reference) = generator.as_ref() {
            let index = self.heap.read_u32(reference.0) as usize;
            if let Some(slot) = self.generators.get_mut(index) {
                *slot = Some(frame);
            }
        }
    }

    /// Whether `value` is a user class object.
    #[must_use]
    pub fn is_class(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.class_type_id)
    }

    /// The class object of a class instance (its `type`); the precondition is that `value` is an
    /// instance ([`ObjectModel::is_instance`]).
    #[must_use]
    pub fn instance_class(&self, value: Value) -> Value {
        self.read_slot(value, 0)
    }

    /// Whether `value` is a user class instance.
    #[must_use]
    pub fn is_instance(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.instance_type_id)
    }

    /// Whether `value` is a bound Python method.
    #[must_use]
    pub fn is_py_bound(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.py_bound_type_id)
    }

    /// The receiver (`self`) of a bound Python method.
    #[must_use]
    pub fn bound_self(&self, bound: Value) -> Value {
        self.read_slot(bound, 0)
    }

    /// The function of a bound Python method.
    #[must_use]
    pub fn bound_func(&self, bound: Value) -> Value {
        self.read_slot(bound, 1)
    }

    /// Allocates a `super` object `[class, self]` -- the `super()` of a method of `class`.
    pub fn new_super(&mut self, class: Value, receiver: Value) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.super_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, class.bits());
        self.heap.write_u32(reference.0 + 4, receiver.bits());
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is a `super` object.
    #[must_use]
    pub fn is_super(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.super_type_id)
    }

    /// `super().name`: resolve `name` from the base of the super's class (the MRO after it),
    /// bound to the super's `self` -- a function there binds (single inheritance), a non-function
    /// is returned as-is; otherwise `AttributeError`.
    pub fn py_getattr_super(&mut self, super_obj: Value, name: &str) -> Result<Value, Trap> {
        let class = self.read_slot(super_obj, 0);
        let receiver = self.read_slot(super_obj, 1);
        let base = self.read_slot(class, 1);
        let found = self.find_in_class(base, name).ok_or(Trap::AttributeError)?;
        if found.as_function_index().is_some() {
            self.new_py_bound(receiver, found)
        } else {
            Ok(found)
        }
    }

    /// Looks up the str-keyed `name` in `dict`, or `None`.
    fn dict_lookup_str(&self, dict: Value, name: &str) -> Option<Value> {
        let entries = self.dict_value(dict)?;
        entries
            .iter()
            .find(|(k, _)| self.str_value(*k) == Some(name))
            .map(|(_, v)| *v)
    }

    /// Resolves `name` in `class`'s namespace, then up the base chain; `None` if unbound.
    fn find_in_class(&self, class: Value, name: &str) -> Option<Value> {
        let mut current = class;
        while self.is_class(current) {
            let namespace = self.read_slot(current, 2);
            if let Some(found) = self.dict_lookup_str(namespace, name) {
                return Some(found);
            }
            current = self.read_slot(current, 1);
        }
        None
    }

    /// `instance.name` (`Op::LoadAttr` on a class instance): the instance `__dict__` first
    /// (returned as-is), then the class + base chain -- a function there binds to the
    /// instance (a [`Self::new_py_bound`]), a non-function is a class attribute; otherwise
    /// `AttributeError`.
    pub fn py_getattr_instance(&mut self, instance: Value, name: &str) -> Result<Value, Trap> {
        let dict = self.read_slot(instance, 1);
        if let Some(found) = self.dict_lookup_str(dict, name) {
            return Ok(found);
        }
        let class = self.read_slot(instance, 0);
        if let Some(found) = self.find_in_class(class, name) {
            if found.as_function_index().is_some() {
                return self.new_py_bound(instance, found);
            }
            return Ok(found);
        }
        Err(Trap::AttributeError)
    }

    /// `instance.name = value` (`Op::SetAttr`): stores into the instance `__dict__`.
    pub fn py_setattr_instance(&mut self, instance: Value, name: &str, value: Value) -> Result<(), Trap> {
        let key = self.new_str(name)?;
        let dict = self.read_slot(instance, 1);
        self.py_setitem(dict, key, value)
    }

    /// Deletes the named attribute from `instance`'s `__dict__` (`delattr(obj, name)` / `del
    /// obj.name`). An `AttributeError` if the value is not an instance or has no such attribute.
    pub fn py_delattr_instance(&mut self, instance: Value, name: &str) -> Result<(), Trap> {
        if !self.is_instance(instance) {
            return Err(Trap::AttributeError);
        }
        let dict = self.read_slot(instance, 1);
        let index = self
            .container_slot(dict, self.dict_type_id)
            .ok_or(Trap::AttributeError)?;
        let entries = core::mem::take(&mut self.dicts[index]);
        let mut found = false;
        let kept: Vec<(Value, Value)> = entries
            .into_iter()
            .filter(|(key, _)| {
                let matches = self.str_value(*key) == Some(name);
                found |= matches;
                !matches
            })
            .collect();
        self.dicts[index] = kept;
        if found {
            Ok(())
        } else {
            Err(Trap::AttributeError)
        }
    }

    /// Resolves `__init__` on `class` (or its bases), if the class defines a constructor.
    #[must_use]
    pub fn find_init(&self, class: Value) -> Option<Value> {
        self.find_in_class(class, "__init__")
    }

    /// The dunder method `name` (e.g. `"__len__"`) bound to `instance`, if its class defines it
    /// as a function; `None` otherwise (or if `instance` is not a class instance). The caller
    /// invokes the returned bound method to run the dunder.
    pub fn find_dunder(&mut self, instance: Value, name: &str) -> Option<Value> {
        if !self.is_instance(instance) {
            return None;
        }
        let class = self.read_slot(instance, 0);
        let found = self.find_in_class(class, name)?;
        if found.as_function_index().is_some() {
            self.new_py_bound(instance, found).ok()
        } else {
            None
        }
    }

    /// Builds the built-in exception class hierarchy on first use (idempotent). Each entry's
    /// base is built before it; `""` is the root's (BaseException's) base.
    fn ensure_exception_types(&mut self) {
        if !self.exception_classes.is_empty() {
            return;
        }
        const HIERARCHY: &[(&str, &str)] = &[
            ("BaseException", ""),
            ("Exception", "BaseException"),
            ("ArithmeticError", "Exception"),
            ("ZeroDivisionError", "ArithmeticError"),
            ("OverflowError", "ArithmeticError"),
            ("LookupError", "Exception"),
            ("IndexError", "LookupError"),
            ("KeyError", "LookupError"),
            ("AttributeError", "Exception"),
            ("NameError", "Exception"),
            ("UnboundLocalError", "NameError"),
            ("TypeError", "Exception"),
            ("ValueError", "Exception"),
            ("AssertionError", "Exception"),
            ("RuntimeError", "Exception"),
            ("RecursionError", "RuntimeError"),
            ("NotImplementedError", "RuntimeError"),
            ("StopIteration", "Exception"),
            ("GeneratorExit", "BaseException"),
        ];
        for &(name, base_name) in HIERARCHY {
            let name_value = match self.new_str(name) {
                Ok(v) => v,
                Err(_) => return,
            };
            let base = if base_name.is_empty() {
                Value::NONE
            } else {
                self.exc_class_lookup(base_name).unwrap_or(Value::NONE)
            };
            let namespace = match self.new_dict(Vec::new()) {
                Ok(v) => v,
                Err(_) => return,
            };
            if let Ok(class) = self.new_class(name_value, base, namespace) {
                self.exception_classes.push((name, class));
            } else {
                return;
            }
        }
    }

    /// The built-in exception class named `name`, or `None` (assumes the hierarchy is built).
    fn exc_class_lookup(&self, name: &str) -> Option<Value> {
        self.exception_classes
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, c)| *c)
    }

    /// The built-in exception class named `name`, building the hierarchy on first use.
    pub fn exception_class(&mut self, name: &str) -> Option<Value> {
        self.ensure_exception_types();
        self.exc_class_lookup(name)
    }

    /// Whether `exc` (a class instance) is an instance of `target` -- i.e. `target` is on its
    /// class's base chain. The basis for `MatchExc` / `except E`.
    #[must_use]
    pub fn exception_isinstance(&self, exc: Value, target: Value) -> bool {
        self.is_instance_of(exc, target)
    }

    /// Whether `value` is a class instance whose class is `class` or one of its bases (the class's
    /// base chain). Backs `MatchExc`/`except E` and the `isinstance` built-in for user classes.
    #[must_use]
    pub fn is_instance_of(&self, value: Value, class: Value) -> bool {
        if !self.is_instance(value) {
            return false;
        }
        let mut current = self.read_slot(value, 0);
        while self.is_class(current) {
            if current == class {
                return true;
            }
            current = self.read_slot(current, 1);
        }
        false
    }

    /// Whether user class `cls` derives from user class `target` (walking `cls`'s base chain, which
    /// includes `cls` itself). Backs `issubclass`.
    #[must_use]
    pub fn is_subclass_of(&self, cls: Value, target: Value) -> bool {
        let mut current = cls;
        while self.is_class(current) {
            if current == target {
                return true;
            }
            current = self.read_slot(current, 1);
        }
        false
    }

    /// Maps a raised interpreter [`Trap`] to a fresh instance of the matching built-in
    /// exception (so `except IndexError:` catches a real index error); `None` for the
    /// internal/fatal traps, which are not catchable Python exceptions.
    pub fn trap_to_exception(&mut self, trap: Trap) -> Option<Value> {
        let name = match trap {
            Trap::TypeError => "TypeError",
            Trap::AttributeError => "AttributeError",
            Trap::IndexError => "IndexError",
            Trap::KeyError => "KeyError",
            Trap::ValueError => "ValueError",
            Trap::ZeroDivisionError => "ZeroDivisionError",
            Trap::NameError => "NameError",
            Trap::UnboundLocal => "UnboundLocalError",
            Trap::RecursionError => "RecursionError",
            Trap::Overflow => "OverflowError",
            Trap::Raised
            | Trap::StackUnderflow
            | Trap::Unsupported
            | Trap::OutOfMemory
            | Trap::Malformed => {
                return None;
            }
        };
        let mut arg = self.pending_trap_arg.take();
        if arg.is_none() {
            arg = self.default_trap_message(trap);
        }
        let class = self.exception_class(name)?;
        let instance = self.new_object(class).ok()?;
        if let Some(arg) = arg {
            let _ = self.init_default_args(instance, &[arg]);
        }
        Some(instance)
    }

    /// The constant CPython message for the traps whose text never varies; `None` for traps whose
    /// message is data-dependent (attached at the site via `pending_trap_arg`) or absent.
    fn default_trap_message(&mut self, trap: Trap) -> Option<Value> {
        let message = match trap {
            Trap::ZeroDivisionError => "division by zero",
            Trap::RecursionError => "maximum recursion depth exceeded",
            _ => return None,
        };
        self.new_str(message).ok()
    }

    /// Attaches a one-shot context argument to the next trap-raised exception (see
    /// [`ObjectModel::pending_trap_arg`]). Call immediately before returning the bare trap.
    pub(crate) fn set_trap_arg(&mut self, arg: Value) {
        self.pending_trap_arg = Some(arg);
    }

    /// Attaches `message` (as the exception's text) to the next raised exception and returns
    /// `trap`, for `return Err(self.with_message(Trap::IndexError, "list index out of range"))`.
    pub(crate) fn with_message(&mut self, trap: Trap, message: &str) -> Trap {
        if let Ok(text) = self.new_str(message) {
            self.set_trap_arg(text);
        }
        trap
    }

    /// A fresh, no-argument instance of the named built-in exception (e.g. `GeneratorExit`).
    pub(crate) fn new_exception(&mut self, name: &str) -> Result<Value, Trap> {
        let class = self.exception_class(name).ok_or(Trap::Malformed)?;
        self.new_object(class)
    }

    /// Raises the named built-in exception with `message`: stashes the instance in the pending slot
    /// and returns `Trap::Raised` (for `return Err(self.raise_named_exception("RuntimeError", ...))`).
    pub(crate) fn raise_named_exception(&mut self, name: &str, message: &str) -> Trap {
        match self.new_exception(name) {
            Ok(exc) => {
                if !message.is_empty() {
                    if let Ok(text) = self.new_str(message) {
                        let _ = self.init_default_args(exc, &[text]);
                    }
                }
                self.set_pending_exception(exc);
                Trap::Raised
            }
            Err(trap) => trap,
        }
    }

    /// Whether the currently pending exception is an instance of the named class -- consumed by the
    /// generator close() protocol (a GeneratorExit escaping cleanly ends the close).
    pub(crate) fn pending_exception_is(&self, name: &str) -> bool {
        match (self.pending_exception, self.exc_class_lookup(name)) {
            (Some(exc), Some(class)) => self.is_instance_of(exc, class),
            _ => false,
        }
    }

    /// Resolves the operand of `raise` (`Op::Raise` argc 1): a class is instantiated no-arg,
    /// an instance is used as-is; the result must derive from `BaseException` (else `TypeError`
    /// -- "exceptions must derive from BaseException").
    pub fn raise_value(&mut self, value: Value) -> Result<Value, Trap> {
        self.ensure_exception_types();
        let base_exception = self.exc_class_lookup("BaseException").ok_or(Trap::Malformed)?;
        let instance = if self.is_class(value) {
            self.new_object(value)?
        } else {
            value
        };
        if self.exception_isinstance(instance, base_exception) {
            Ok(instance)
        } else {
            Err(Trap::TypeError)
        }
    }

    /// Sets the in-flight exception (a `raise`'s object) for the interpreter's exception-table
    /// search to pick up.
    pub fn set_pending_exception(&mut self, exception: Value) {
        self.pending_exception = Some(exception);
    }

    /// Takes the in-flight exception, clearing the slot.
    pub fn take_pending_exception(&mut self) -> Option<Value> {
        self.pending_exception.take()
    }

    /// Binds (or rebinds) the module-global `name`.
    pub fn set_global(&mut self, name: &str, value: Value) {
        if let Some(slot) = self.globals.iter_mut().find(|(n, _)| n == name) {
            slot.1 = value;
        } else {
            self.globals.push((String::from(name), value));
        }
    }

    /// The value bound to module-global `name`, or `None`.
    #[must_use]
    pub fn get_global(&self, name: &str) -> Option<Value> {
        self.globals.iter().find(|(n, _)| n == name).map(|(_, v)| *v)
    }

    /// Renders `value` the way `print()` shows it: an int as decimal, a top-level `str` raw, the
    /// singletons by name, a container via its `repr`.
    #[must_use]
    pub fn display(&self, value: Value) -> String {
        #[cfg(feature = "complex")]
        if let Some((re, im)) = self.complex_value(value) {
            return format_complex(re, im);
        }
        if let Some(n) = value.as_fixnum() {
            alloc::format!("{n}")
        } else if let Some(n) = self.long_value(value) {
            alloc::format!("{n}")
        } else if let Some(big) = self.bigint_value(value) {
            big.to_decimal_string()
        } else if let Some(f) = self.float_value(value) {
            format_float(f)
        } else if value == Value::TRUE {
            String::from("True")
        } else if value == Value::FALSE {
            String::from("False")
        } else if value.is_none() {
            String::from("None")
        } else if let Some(s) = self.str_value(value) {
            String::from(s)
        } else if self.is_instance(value) {
            self.instance_display(value)
        } else {
            self.repr(value)
        }
    }

    /// `str()` of a class instance: an exception renders as its MESSAGE (the str of its single arg,
    /// `""` for none, or the args tuple's repr for several -- Python's `str(exc)`); any other
    /// instance falls back to its `repr` (`<ClassName object>`).
    fn instance_display(&self, instance: Value) -> String {
        if self.is_exception_value(instance) {
            return self.exception_message(instance);
        }
        self.repr(instance)
    }

    /// Whether `value` is an exception instance (derives `BaseException`). Uses the immutable
    /// class lookup: if the exception types are not built yet, no exception instance exists, so
    /// this is `false`.
    #[must_use]
    fn is_exception_value(&self, value: Value) -> bool {
        self.exc_class_lookup("BaseException")
            .is_some_and(|base| self.is_instance_of(value, base))
    }

    /// The str message of an exception instance: `str(arg)` for a single stored arg, `""` for none,
    /// else the args tuple's repr (Python's `BaseException.__str__`).
    fn exception_message(&self, instance: Value) -> String {
        let Some(args) = self.instance_attr(instance, "args") else {
            return String::new();
        };
        let key_error = self
            .exc_class_lookup("KeyError")
            .is_some_and(|class| self.is_instance_of(instance, class));
        match self.seq_value(args) {
            Some(elements) if elements.len() == 1 => {
                if key_error {
                    self.repr(elements[0])
                } else {
                    self.display(elements[0])
                }
            }
            Some(elements) if elements.is_empty() => String::new(),
            _ => self.repr(args),
        }
    }

    /// An instance attribute read straight from its `__dict__` (no method/MRO resolution), or
    /// `None`. For rendering an exception's stored `args` / another instance's class.
    #[must_use]
    fn instance_attr(&self, instance: Value, name: &str) -> Option<Value> {
        let dict = self.read_slot(instance, 1);
        for (key, value) in self.dict_value(dict)? {
            if self.str_value(*key) == Some(name) {
                return Some(*value);
            }
        }
        None
    }

    /// The name of a class instance's class (its class object's name slot), or `None`.
    #[must_use]
    fn instance_class_name(&self, instance: Value) -> Option<&str> {
        let class = self.read_slot(instance, 0);
        self.str_value(self.read_slot(class, 0))
    }

    /// Appends a `print()` line (already formatted) plus a newline to the captured output.
    pub fn write_line(&mut self, line: &str) {
        self.stdout.push_str(line);
        self.stdout.push('\n');
    }

    /// Appends `text` to the captured output WITHOUT a trailing newline -- for `print(..., end=s)`,
    /// which supplies its own terminator.
    pub fn write(&mut self, text: &str) {
        self.stdout.push_str(text);
    }

    /// Drains the captured `print` output.
    pub fn take_stdout(&mut self) -> String {
        core::mem::take(&mut self.stdout)
    }

    /// Unpacks an iterable into exactly `count` elements (`a, b = x`); a length mismatch is a
    /// `ValueError` ("not enough" / "too many values to unpack"). Works over any iterable.
    pub fn unpack_sequence(&mut self, value: Value, count: usize) -> Result<Vec<Value>, Trap> {
        let iterator = self.new_iter(value)?;
        let mut elements = Vec::new();
        while let Some(element) = self.py_next(iterator)? {
            elements.push(element);
        }
        if elements.len() != count {
            return Err(Trap::ValueError);
        }
        Ok(elements)
    }

    /// Unpacks an iterable for a starred target `a, *b, c = x`: the `before` head elements, then a
    /// LIST of the middle (`len - before - after` elements), then the `after` tail elements, in
    /// target order. Fewer than `before + after` elements is a `ValueError`. Works over any
    /// iterable.
    pub fn unpack_ex(
        &mut self,
        value: Value,
        before: usize,
        after: usize,
    ) -> Result<Vec<Value>, Trap> {
        let iterator = self.new_iter(value)?;
        let mut elements = Vec::new();
        while let Some(element) = self.py_next(iterator)? {
            elements.push(element);
        }
        if elements.len() < before + after {
            return Err(Trap::ValueError);
        }
        let middle_end = elements.len() - after;
        let middle = self.new_list(elements[before..middle_end].to_vec())?;
        let mut targets = Vec::with_capacity(before + 1 + after);
        targets.extend_from_slice(&elements[..before]);
        targets.push(middle);
        targets.extend_from_slice(&elements[middle_end..]);
        Ok(targets)
    }

    /// The class name of an exception instance (`"IndexError"`, ...), for reporting an
    /// uncaught exception; `None` if `exc` is not a class instance with a `str` class name.
    #[must_use]
    pub fn exception_type_name(&self, exc: Value) -> Option<&str> {
        if !self.is_instance(exc) {
            return None;
        }
        let class = self.read_slot(exc, 0);
        if !self.is_class(class) {
            return None;
        }
        self.str_value(self.read_slot(class, 0))
    }

    /// The shared heap (for the collector, and for tests that drive a collection).
    #[must_use]
    pub fn heap(&self) -> &Heap {
        &self.heap
    }

    /// The shared heap, mutably (to drive a collection over external roots).
    pub fn heap_mut(&mut self) -> &mut Heap {
        &mut self.heap
    }

    /// The type with id `type_id`, if any.
    #[must_use]
    pub fn type_of(&self, type_id: u32) -> Option<&PyType> {
        self.types.get(type_id as usize)
    }

    /// Allocates an instance of `type_id`, initializing its attribute slots from
    /// `attrs` (one tagged value per slot, in slot order). `attrs` must have exactly
    /// the type's slot count.
    pub fn new_instance(&mut self, type_id: u32, attrs: &[Value]) -> Result<Value, Trap> {
        let ty = self.types.get(type_id as usize).ok_or(Trap::Malformed)?;
        if attrs.len() != usize::from(ty.num_slots) {
            return Err(Trap::Malformed);
        }
        let reference = self.alloc_object(type_id).ok_or(Trap::OutOfMemory)?;
        for (i, value) in attrs.iter().enumerate() {
            self.heap.write_u32(reference.0 + (i as u32) * 4, value.bits());
        }
        Ok(Value::from_ref(reference))
    }

    /// `py_getattr`: the value of attribute `name` on `obj` -- equivalent to `obj.name`,
    /// the built-in `getattr(object, name)` (Python 3.14.6 Library Reference, "Built-in
    /// Functions"). Uses and updates the call-site inline `cache`: on a hit the resolved
    /// slot is reused, on a miss the type's attribute table is consulted and recorded.
    ///
    /// A failed attribute reference raises `AttributeError` ("Built-in Exceptions").
    /// That includes a receiver that is not a heap object here: in Python `(1).x` and
    /// `None.x` raise `AttributeError`, not `TypeError`, because those values DO support
    /// attribute references and merely lack the name -- `TypeError` is reserved for an
    /// object that does not support attribute references at all, which the supported
    /// types have none of. The full default lookup (data descriptors on the type, then
    /// the instance `__dict__`, then non-data descriptors / class attributes, then
    /// `__getattr__`; data model, "Customizing attribute access") is narrowed here to a
    /// fixed per-type slot table -- a simplification of the full lookup, not a deviation in the
    /// observable result for the subset.
    pub fn getattr(
        &mut self,
        obj: Value,
        name: &str,
        cache: &mut InlineCache,
    ) -> Result<Value, Trap> {
        if let Some(id) = obj.as_builtin_id() {
            if name == "__name__" {
                if let Some(builtin) = Builtin::from_id(id) {
                    return self.new_str(builtin.python_name());
                }
            }
            if id == Builtin::Dict.id() && name == "fromkeys" {
                return Ok(Value::builtin_ref(Builtin::DictFromkeys.id()));
            }
            return Err(Trap::AttributeError);
        }
        let reference = obj.as_ref().ok_or(Trap::AttributeError)?;
        let type_id = self.heap.type_id_of(reference);
        if type_id == self.str_type_id {
            let method_id = str_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.bytes_type_id || type_id == self.bytearray_type_id {
            let method_id = bytes_method_id(name, type_id == self.bytearray_type_id)
                .ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.list_type_id {
            let method_id = list_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.dict_type_id {
            let method_id = dict_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.set_type_id {
            let method_id = set_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.frozenset_type_id {
            let method_id = frozenset_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.tuple_type_id {
            let method_id = tuple_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        #[cfg(feature = "complex")]
        if type_id == self.complex_type_id {
            let (re, im) = self.complex_value(obj).ok_or(Trap::AttributeError)?;
            return match name {
                "real" => self.new_float(re),
                "imag" => self.new_float(im),
                _ => {
                    let method_id = complex_method_id(name).ok_or(Trap::AttributeError)?;
                    self.new_bound_method(obj, method_id)
                }
            };
        }
        if type_id == self.generator_type_id {
            let method_id = crate::interp::generator_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.module_type_id {
            let namespace = self.read_slot(obj, 0);
            let index = self
                .container_slot(namespace, self.dict_type_id)
                .ok_or(Trap::AttributeError)?;
            return self.dicts[index]
                .iter()
                .find(|(key, _)| self.str_value(*key) == Some(name))
                .map(|(_, value)| *value)
                .ok_or(Trap::AttributeError);
        }
        if type_id == self.gpio_type_id {
            let method_id = crate::gpio::gpio_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.board_type_id {
            let pin = crate::gpio::board_pin_id(name).ok_or(Trap::AttributeError)?;
            return Value::fixnum(pin as i32).ok_or(Trap::Overflow);
        }
        if type_id == self.pin_type_id {
            let method_id = crate::gpio::pin_method_id(name).ok_or(Trap::AttributeError)?;
            return self.new_bound_method(obj, method_id);
        }
        if type_id == self.machine_type_id {
            if name == "Pin" {
                return self.pin_factory_singleton();
            }
            return Err(Trap::AttributeError);
        }
        if type_id == self.pin_factory_type_id {
            let mode = crate::gpio::machine_pin_const(name).ok_or(Trap::AttributeError)?;
            return Value::fixnum(mode as i32).ok_or(Trap::Overflow);
        }
        if type_id == self.digitalio_type_id {
            return match name {
                "DigitalInOut" => self.dio_factory_singleton(),
                "Direction" => self.direction_singleton(),
                _ => Err(Trap::AttributeError),
            };
        }
        if type_id == self.direction_type_id {
            let d = crate::gpio::direction_const(name).ok_or(Trap::AttributeError)?;
            return Value::fixnum(d as i32).ok_or(Trap::Overflow);
        }
        if type_id == self.dio_type_id {
            return match name {
                "value" => self.dio_read_value(obj),
                "direction" => {
                    let pin = self.dio_pin(obj);
                    let reference = pin.as_ref().ok_or(Trap::TypeError)?;
                    let mode = self.heap.read_u32(reference.0 + crate::gpio::PIN_W_MODE * 4);
                    Value::fixnum(mode as i32).ok_or(Trap::Overflow)
                }
                _ => Err(Trap::AttributeError),
            };
        }
        if type_id == self.class_type_id {
            if name == "__name__" {
                return Ok(self.read_slot(obj, 0));
            }
            return self.find_in_class(obj, name).ok_or(Trap::AttributeError);
        }
        if type_id == self.instance_type_id {
            return self.py_getattr_instance(obj, name);
        }
        if type_id == self.super_type_id {
            return self.py_getattr_super(obj, name);
        }
        let slot = match cache.lookup(type_id) {
            Some(slot) => slot,
            None => {
                let ty = self.types.get(type_id as usize).ok_or(Trap::Malformed)?;
                let slot = ty.slot_of(name).ok_or(Trap::AttributeError)?;
                cache.fill(type_id, slot);
                slot
            }
        };
        let word = self.heap.read_u32(reference.0 + u32::from(slot) * 4);
        Ok(Value::from_bits(word))
    }

    /// Binds str method `method_id` to `receiver`, returning a callable bound-method
    /// object (`receiver.method`). A heap object holding `[receiver, method_id]`, read back
    /// at the call. (`alloc` never relocates, so the receiver stays valid across it.)
    fn new_bound_method(&mut self, receiver: Value, method_id: u32) -> Result<Value, Trap> {
        let reference = self
            .heap
            .alloc(self.bound_method_type_id)
            .ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, receiver.bits());
        self.heap.write_u32(reference.0 + 4, method_id);
        Ok(Value::from_ref(reference))
    }


    /// Installs the volatile MMIO seam (on device the runner passes `lamella_mmio::write32` /
    /// `read32`). The host leaves it unset and uses the simulated register file, so a driver runs
    /// and is verifiable off-device.
    pub fn set_mmio(&mut self, write: fn(u32, u32), read: fn(u32) -> u32) {
        self.mmio_write_fn = Some(write);
        self.mmio_read_fn = Some(read);
    }

    /// A volatile 32-bit register write: through the installed seam on device, else into the host
    /// simulated register file (and the ordered write trace).
    pub fn mmio_write(&mut self, address: u32, value: u32) {
        if let Some(write) = self.mmio_write_fn {
            write(address, value);
            return;
        }
        #[cfg(not(target_os = "none"))]
        {
            self.mmio_sim.insert(address, value);
            self.mmio_trace.push((address, value));
        }
    }

    /// A volatile 32-bit register read: through the installed seam on device, else from the host
    /// simulated register file (0 for a register never written).
    #[must_use]
    pub fn mmio_read(&self, address: u32) -> u32 {
        if let Some(read) = self.mmio_read_fn {
            return read(address);
        }
        #[cfg(not(target_os = "none"))]
        {
            self.mmio_sim.get(&address).copied().unwrap_or(0)
        }
        #[cfg(target_os = "none")]
        0
    }

    /// The host simulated register file (last value written per address) -- a test oracle.
    #[cfg(not(target_os = "none"))]
    #[must_use]
    pub fn mmio_sim(&self) -> &alloc::collections::BTreeMap<u32, u32> {
        &self.mmio_sim
    }

    /// The host ordered log of every MMIO write -- the drive-sequence oracle for a test.
    #[cfg(not(target_os = "none"))]
    #[must_use]
    pub fn mmio_trace(&self) -> &[(u32, u32)] {
        &self.mmio_trace
    }

    /// Installs the `sleep_ms` delay seam (device: a timer/spin). The host leaves it a no-op, so
    /// the differential is not slowed by real sleeps.
    pub fn set_delay(&mut self, delay: fn(u32)) {
        self.delay_fn = Some(delay);
    }

    /// `sleep_ms(ms)`: the installed delay on device, a no-op on the host.
    pub fn delay_ms(&mut self, ms: u32) {
        if let Some(delay) = self.delay_fn {
            delay(ms);
        }
    }

    /// Seeds the firmware-reserved pins (from the target profile) so an app claim of one fails
    /// loud -- never a silent app-vs-firmware register race.
    pub fn reserve_firmware_pins(&mut self, pins: &[u32]) {
        for &pin in pins {
            if !self.gpio_reserved.contains(&pin) {
                self.gpio_reserved.push(pin);
            }
        }
    }

    /// Claims `pin` for the app. Fails LOUD with `ValueError` if the
    /// pin is already app-claimed or firmware-reserved -- one owner per pin.
    fn claim_pin(&mut self, pin: u32) -> Result<(), Trap> {
        if self.gpio_reserved.contains(&pin) || self.gpio_claimed.contains(&pin) {
            return Err(Trap::ValueError);
        }
        self.gpio_claimed.push(pin);
        Ok(())
    }

    /// Releases an app-claimed pin (`Pin.deinit`), so it can be opened again.
    fn release_pin(&mut self, pin: u32) {
        self.gpio_claimed.retain(|&p| p != pin);
    }

    /// The `gpio` module singleton (the clean hardware API). Bind it as the global `gpio` so a
    /// program reaches `gpio.output(...)`.
    pub fn gpio_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.gpio_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// The `board` pin-name singleton. Bind it as the global `board` so a program reaches
    /// `board.LED` etc.
    pub fn board_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.board_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is the `gpio` singleton.
    #[must_use]
    pub fn is_gpio(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.gpio_type_id)
    }

    /// Whether `value` is a `Pin`.
    #[must_use]
    pub fn is_pin(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.pin_type_id)
    }

    /// Allocates a `Pin` handle over the precomputed drive registers `regs` in `mode`, current
    /// output state low.
    fn new_pin(&mut self, mode: u32, regs: &crate::gpio::PinRegs) -> Result<Value, Trap> {
        use crate::gpio::*;
        let reference = self.alloc_object(self.pin_type_id).ok_or(Trap::OutOfMemory)?;
        let base = reference.0;
        self.heap.write_u32(base + PIN_W_ID * 4, regs.pin_id);
        self.heap.write_u32(base + PIN_W_SET_REG * 4, regs.set_reg);
        self.heap.write_u32(base + PIN_W_SET_VAL * 4, regs.set_val);
        self.heap.write_u32(base + PIN_W_CLR_REG * 4, regs.clr_reg);
        self.heap.write_u32(base + PIN_W_CLR_VAL * 4, regs.clr_val);
        self.heap.write_u32(base + PIN_W_READ_REG * 4, regs.read_reg);
        self.heap.write_u32(base + PIN_W_READ_MASK * 4, regs.read_mask);
        self.heap.write_u32(base + PIN_W_CUR * 4, 0);
        self.heap.write_u32(base + PIN_W_MODE * 4, mode);
        Ok(Value::from_ref(reference))
    }

    /// Opens `pin` in the given direction: claims it (fail-loud), configures the port through the
    /// board driver (clock ungate + the pin's MODER direction), and returns a `Pin`. Shared by the
    /// clean `gpio` API.
    fn open_pin(&mut self, pin: u32, output: bool) -> Result<Value, Trap> {
        use crate::gpio::*;
        if pin > MAX_PIN {
            return Err(Trap::ValueError);
        }
        self.claim_pin(pin)?;
        let enabled = self.mmio_read(CLOCK_ENABLE_REG) | CLOCK_ENABLE_BIT;
        self.mmio_write(CLOCK_ENABLE_REG, enabled);
        let (clear_mask, set_value) = moder_bits(pin, output);
        let moder = (self.mmio_read(MODER_REG) & !clear_mask) | set_value;
        self.mmio_write(MODER_REG, moder);
        let regs = pin_regs(pin);
        let mode = if output { PIN_MODE_OUTPUT } else { PIN_MODE_INPUT };
        self.new_pin(mode, &regs)
    }

    /// Dispatches a `gpio` method (`gpio.output(pin)` / `gpio.input(pin)`): opens the pin.
    fn call_gpio_method(
        &mut self,
        _gpio: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        use crate::gpio::{GPIO_INPUT, GPIO_OUTPUT};
        let pin = match args {
            [p] => {
                u32::try_from(p.as_int().ok_or(Trap::TypeError)?).map_err(|_| Trap::ValueError)?
            }
            _ => return Err(Trap::TypeError),
        };
        let output = match method_id {
            GPIO_OUTPUT => true,
            GPIO_INPUT => false,
            _ => return Err(Trap::AttributeError),
        };
        self.open_pin(pin, output)
    }

    /// The `machine` module singleton. Bind it as the global `machine`.
    pub fn machine_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.machine_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// A fresh `machine.Pin` factory (the callable class carrying OUT/IN + constructing pins).
    fn pin_factory_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.pin_factory_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is the `machine` singleton.
    #[must_use]
    pub fn is_machine(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.machine_type_id)
    }

    /// Whether `value` is a `machine.Pin` factory (a callable).
    #[must_use]
    pub fn is_pin_factory(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.pin_factory_type_id)
    }

    /// Constructs `machine.Pin(id[, mode])`: opens the pin and returns it. The
    /// result IS a clean gpio `Pin` -- `value`/`on`/`off`/`toggle` are the same methods.
    pub(crate) fn call_pin_factory(&mut self, args: &[Value]) -> Result<Value, Trap> {
        use crate::gpio::{MACHINE_PIN_IN, MACHINE_PIN_OUT};
        let (id, mode) = match args {
            [id] => (*id, MACHINE_PIN_OUT),
            [id, mode] => (
                *id,
                u32::try_from(mode.as_int().ok_or(Trap::TypeError)?).unwrap_or(MACHINE_PIN_IN),
            ),
            _ => return Err(Trap::TypeError),
        };
        let pin =
            u32::try_from(id.as_int().ok_or(Trap::TypeError)?).map_err(|_| Trap::ValueError)?;
        self.open_pin(pin, mode == MACHINE_PIN_OUT)
    }

    /// The `digitalio` module singleton. Bind it as the global `digitalio`.
    pub fn digitalio_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.digitalio_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// A fresh `digitalio.DigitalInOut` factory (callable).
    fn dio_factory_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.dio_factory_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// A fresh `digitalio.Direction` enum singleton (OUTPUT/INPUT).
    fn direction_singleton(&mut self) -> Result<Value, Trap> {
        let reference = self.alloc_object(self.direction_type_id).ok_or(Trap::OutOfMemory)?;
        Ok(Value::from_ref(reference))
    }

    /// Whether `value` is the `digitalio` singleton.
    #[must_use]
    pub fn is_digitalio(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.digitalio_type_id)
    }

    /// Whether `value` is a `digitalio.DigitalInOut` factory (a callable).
    #[must_use]
    pub fn is_dio_factory(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.dio_factory_type_id)
    }

    /// Whether `value` is a `DigitalInOut` instance.
    #[must_use]
    fn is_dio(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|r| self.heap.type_id_of(r) == self.dio_type_id)
    }

    /// Constructs `digitalio.DigitalInOut(pin)`: opens the pin (input by default -- 
    /// and wraps it as a `DigitalInOut` whose `value`/`direction` are properties.
    pub(crate) fn call_dio_factory(&mut self, args: &[Value]) -> Result<Value, Trap> {
        let pin_id = match args {
            [p] => {
                u32::try_from(p.as_int().ok_or(Trap::TypeError)?).map_err(|_| Trap::ValueError)?
            }
            _ => return Err(Trap::TypeError),
        };
        let pin = self.open_pin(pin_id, false)?;
        let reference = self.alloc_object(self.dio_type_id).ok_or(Trap::OutOfMemory)?;
        self.heap.write_u32(reference.0, pin.bits());
        Ok(Value::from_ref(reference))
    }

    /// The clean gpio `Pin` a `DigitalInOut` wraps.
    fn dio_pin(&self, dio: Value) -> Value {
        self.read_slot(dio, 0)
    }

    /// Reconfigures a `Pin`'s direction in place (rewrites the MODER field + the mode word); the
    /// port clock is already ungated at open.
    fn set_pin_direction(&mut self, pin: Value, output: bool) -> Result<(), Trap> {
        use crate::gpio::*;
        let reference = pin.as_ref().ok_or(Trap::TypeError)?;
        let pin_id = self.heap.read_u32(reference.0 + PIN_W_ID * 4);
        let (clear_mask, set_value) = moder_bits(pin_id, output);
        let moder = (self.mmio_read(MODER_REG) & !clear_mask) | set_value;
        self.mmio_write(MODER_REG, moder);
        let mode = if output { PIN_MODE_OUTPUT } else { PIN_MODE_INPUT };
        self.heap.write_u32(reference.0 + PIN_W_MODE * 4, mode);
        Ok(())
    }

    /// `dio.value` (getattr): the pin's level as a `bool` -- the last driven value for an output,
    /// or the sampled input for an input.
    fn dio_read_value(&self, dio: Value) -> Result<Value, Trap> {
        use crate::gpio::*;
        let pin = self.dio_pin(dio);
        let reference = pin.as_ref().ok_or(Trap::TypeError)?;
        let mode = self.heap.read_u32(reference.0 + PIN_W_MODE * 4);
        let bit = if mode == PIN_MODE_OUTPUT {
            self.heap.read_u32(reference.0 + PIN_W_CUR * 4)
        } else {
            let read_reg = self.heap.read_u32(reference.0 + PIN_W_READ_REG * 4);
            let read_mask = self.heap.read_u32(reference.0 + PIN_W_READ_MASK * 4);
            u32::from(self.mmio_read(read_reg) & read_mask != 0)
        };
        Ok(Value::from_bool(bit != 0))
    }

    /// `dio.value = x` / `dio.direction = d` (attribute SET on a `DigitalInOut`); any other native
    /// object has no settable attribute (`AttributeError`, as before).
    pub(crate) fn py_setattr_native(
        &mut self,
        object: Value,
        name: &str,
        value: Value,
    ) -> Result<(), Trap> {
        use crate::gpio::{PIN_HIGH, PIN_LOW, PIN_MODE_OUTPUT};
        if self.is_dio(object) {
            let pin = self.dio_pin(object);
            match name {
                "value" => {
                    let high = value.as_int().map_or_else(|| value.is_truthy(), |n| n != 0);
                    self.call_pin_method(pin, if high { PIN_HIGH } else { PIN_LOW }, &[])?;
                    return Ok(());
                }
                "direction" => {
                    let output = value.as_int().unwrap_or(0) == i64::from(PIN_MODE_OUTPUT);
                    self.set_pin_direction(pin, output)?;
                    return Ok(());
                }
                _ => return Err(Trap::AttributeError),
            }
        }
        Err(Trap::AttributeError)
    }

    /// Dispatches a `Pin` method: `high`/`on` and `low`/`off` drive the output, `toggle` flips
    /// it, `value` reads (no argument) or writes (one), `read` samples the input, `deinit`
    /// releases the reservation. Each drive is a single register write through the MMIO seam.
    fn call_pin_method(
        &mut self,
        pin: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        use crate::gpio::*;
        let reference = pin.as_ref().ok_or(Trap::TypeError)?;
        let base = reference.0;
        let pin_id = self.heap.read_u32(base + PIN_W_ID * 4);
        let set_reg = self.heap.read_u32(base + PIN_W_SET_REG * 4);
        let set_val = self.heap.read_u32(base + PIN_W_SET_VAL * 4);
        let clr_reg = self.heap.read_u32(base + PIN_W_CLR_REG * 4);
        let clr_val = self.heap.read_u32(base + PIN_W_CLR_VAL * 4);
        let read_reg = self.heap.read_u32(base + PIN_W_READ_REG * 4);
        let read_mask = self.heap.read_u32(base + PIN_W_READ_MASK * 4);
        let cur = self.heap.read_u32(base + PIN_W_CUR * 4);
        match method_id {
            PIN_HIGH => {
                self.mmio_write(set_reg, set_val);
                self.heap.write_u32(base + PIN_W_CUR * 4, 1);
                Ok(Value::NONE)
            }
            PIN_LOW => {
                self.mmio_write(clr_reg, clr_val);
                self.heap.write_u32(base + PIN_W_CUR * 4, 0);
                Ok(Value::NONE)
            }
            PIN_TOGGLE => {
                if cur == 0 {
                    self.mmio_write(set_reg, set_val);
                    self.heap.write_u32(base + PIN_W_CUR * 4, 1);
                } else {
                    self.mmio_write(clr_reg, clr_val);
                    self.heap.write_u32(base + PIN_W_CUR * 4, 0);
                }
                Ok(Value::NONE)
            }
            PIN_VALUE => match args {
                [] => {
                    let bit = i32::from(self.mmio_read(read_reg) & read_mask != 0);
                    Value::fixnum(bit).ok_or(Trap::Overflow)
                }
                [x] => {
                    let high = x.as_int().ok_or(Trap::TypeError)? != 0;
                    if high {
                        self.mmio_write(set_reg, set_val);
                        self.heap.write_u32(base + PIN_W_CUR * 4, 1);
                    } else {
                        self.mmio_write(clr_reg, clr_val);
                        self.heap.write_u32(base + PIN_W_CUR * 4, 0);
                    }
                    Ok(Value::NONE)
                }
                _ => Err(Trap::TypeError),
            },
            PIN_READ => {
                let bit = i32::from(self.mmio_read(read_reg) & read_mask != 0);
                Value::fixnum(bit).ok_or(Trap::Overflow)
            }
            PIN_DEINIT => {
                self.release_pin(pin_id);
                Ok(Value::NONE)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Whether `value` is a bound method (the callable a `str.method` reference produces).
    #[must_use]
    pub fn is_bound_method(&self, value: Value) -> bool {
        value
            .as_ref()
            .is_some_and(|reference| self.heap.type_id_of(reference) == self.bound_method_type_id)
    }

    /// Renders a `str.format(*args)` template: `{}` takes the next positional argument, `{N}` the
    /// N-th, and `{{` / `}}` are literal braces; each field is rendered with `str()` ([`display`]).
    /// An out-of-range index is an `IndexError`. Named fields (`{name}`, kwargs-only) and format
    /// specs (`{:spec}`) are follow-ons -- `Unsupported` for now, never wrong output.
    /// Renders `value` under a format spec `[[fill]align][sign][#][0][width][.prec][type]`. Supports
    /// the int presentation types (d/x/X/o/b/c) and str (s), plus alignment/width/fill/sign/zero-pad
    /// and str precision (truncation). Float types (f/e/g) and grouping (`,`) are unsupported (no
    /// float; a niche) -> `Unsupported`.
    fn format_value_spec(&self, value: Value, spec: &str) -> Result<String, Trap> {
        let chars: Vec<char> = spec.chars().collect();
        let mut i = 0;
        let (mut fill, mut align) = (' ', '\0');
        if chars.len() >= 2 && matches!(chars[1], '<' | '>' | '^' | '=') {
            fill = chars[0];
            align = chars[1];
            i = 2;
        } else if chars.first().is_some_and(|c| matches!(c, '<' | '>' | '^' | '=')) {
            align = chars[0];
            i = 1;
        }
        let mut sign = '-';
        if chars.get(i).is_some_and(|c| matches!(c, '+' | '-' | ' ')) {
            sign = chars[i];
            i += 1;
        }
        let mut alternate = false;
        if chars.get(i) == Some(&'#') {
            alternate = true;
            i += 1;
        }
        if chars.get(i) == Some(&'0') {
            if align == '\0' {
                align = '=';
                fill = '0';
            }
            i += 1;
        }
        let mut width = 0usize;
        while chars.get(i).is_some_and(char::is_ascii_digit) {
            width = width * 10 + (chars[i] as usize - '0' as usize);
            i += 1;
        }
        let mut grouping = None;
        if chars.get(i).is_some_and(|c| matches!(c, ',' | '_')) {
            grouping = Some(chars[i]);
            i += 1;
        }
        let mut precision = None;
        if chars.get(i) == Some(&'.') {
            i += 1;
            let mut p = 0usize;
            while chars.get(i).is_some_and(char::is_ascii_digit) {
                p = p * 10 + (chars[i] as usize - '0' as usize);
                i += 1;
            }
            precision = Some(p);
        }
        let type_char = chars.get(i).copied();
        if type_char.is_some() {
            i += 1;
        }
        if i != chars.len() {
            return Err(Trap::Unsupported);
        }

        if let Some(n) = value.as_int() {
            let magnitude = n.unsigned_abs() as u32;
            let (digits, prefix) = match type_char.unwrap_or('d') {
                'd' | 'n' => (format_radix(magnitude, 10, false), ""),
                'x' => (format_radix(magnitude, 16, false), if alternate { "0x" } else { "" }),
                'X' => (format_radix(magnitude, 16, true), if alternate { "0X" } else { "" }),
                'o' => (format_radix(magnitude, 8, false), if alternate { "0o" } else { "" }),
                'b' => (format_radix(magnitude, 2, false), if alternate { "0b" } else { "" }),
                'c' => {
                    let cp = u32::try_from(n).map_err(|_| Trap::ValueError)?;
                    let ch = char::from_u32(cp).ok_or(Trap::ValueError)?;
                    let mut buf = [0u8; 4];
                    let align = if align == '\0' { '<' } else { align };
                    return Ok(pad_field(ch.encode_utf8(&mut buf), width, fill, align));
                }
                _ => return Err(Trap::Unsupported),
            };
            let digits = match grouping {
                Some(separator) => group_integer_digits(&digits, separator),
                None => digits,
            };
            let sign_str = if n < 0 {
                "-"
            } else {
                match sign {
                    '+' => "+",
                    ' ' => " ",
                    _ => "",
                }
            };
            let align = if align == '\0' { '>' } else { align };
            if align == '=' {
                let head = sign_str.chars().count() + prefix.chars().count() + digits.chars().count();
                let mut out = String::new();
                out.push_str(sign_str);
                out.push_str(prefix);
                (0..width.saturating_sub(head)).for_each(|_| out.push(fill));
                out.push_str(&digits);
                return Ok(out);
            }
            Ok(pad_field(&alloc::format!("{sign_str}{prefix}{digits}"), width, fill, align))
        } else if let Some(s) = self.str_value(value) {
            if !matches!(type_char, None | Some('s')) {
                return Err(Trap::Unsupported);
            }
            let body: String = match precision {
                Some(p) => s.chars().take(p).collect(),
                None => String::from(s),
            };
            let align = if align == '\0' { '<' } else { align };
            Ok(pad_field(&body, width, fill, align))
        } else if let Some(f) = self.float_value(value) {
            let negative = f < 0.0 || (f == 0.0 && f.is_sign_negative());
            let magnitude = if negative { -f } else { f };
            let upper = matches!(type_char, Some('E' | 'F' | 'G'));
            let mut body = if magnitude.is_nan() {
                String::from(if upper { "NAN" } else { "nan" })
            } else if magnitude.is_infinite() {
                String::from(if upper { "INF" } else { "inf" })
            } else {
                match type_char {
                    Some('f' | 'F') => alloc::format!("{magnitude:.*}", precision.unwrap_or(6)),
                    Some('e' | 'E') => float_format_scientific(magnitude, precision.unwrap_or(6), upper),
                    Some('g' | 'G') => {
                        float_format_general(magnitude, precision.unwrap_or(6), upper, alternate, false)
                    }
                    Some('%') => alloc::format!("{:.*}%", precision.unwrap_or(6), magnitude * 100.0),
                    None => match precision {
                        None => format_float(magnitude),
                        Some(p) => float_format_general(magnitude, p, false, alternate, true),
                    },
                    _ => return Err(Trap::Unsupported),
                }
            };
            if let Some(separator) = grouping {
                if magnitude.is_finite() {
                    body = group_integer_digits(&body, separator);
                }
            }
            let sign_str = if negative {
                "-"
            } else {
                match sign {
                    '+' => "+",
                    ' ' => " ",
                    _ => "",
                }
            };
            let align = if align == '\0' { '>' } else { align };
            if align == '=' {
                let head = sign_str.chars().count() + body.chars().count();
                let mut out = String::from(sign_str);
                (0..width.saturating_sub(head)).for_each(|_| out.push(fill));
                out.push_str(&body);
                return Ok(out);
            }
            Ok(pad_field(&alloc::format!("{sign_str}{body}"), width, fill, align))
        } else {
            Err(Trap::Unsupported)
        }
    }

    fn format_template(&self, template: &str, args: &[Value]) -> Result<String, Trap> {
        let mut out = String::new();
        let mut chars = template.chars().peekable();
        let mut auto_index = 0usize;
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    out.push('{');
                }
                '{' => {
                    let mut field = String::new();
                    let mut closed = false;
                    for fc in chars.by_ref() {
                        if fc == '}' {
                            closed = true;
                            break;
                        }
                        field.push(fc);
                    }
                    if !closed {
                        return Err(Trap::Unsupported);
                    }
                    let (name, spec) = match field.split_once(':') {
                        Some((n, s)) => (n, Some(s)),
                        None => (field.as_str(), None),
                    };
                    let index = if name.is_empty() {
                        let i = auto_index;
                        auto_index += 1;
                        i
                    } else {
                        name.parse::<usize>().map_err(|_| Trap::Unsupported)?
                    };
                    let arg = *args.get(index).ok_or(Trap::IndexError)?;
                    match spec {
                        None => out.push_str(&self.display(arg)),
                        Some(spec) => out.push_str(&self.format_value_spec(arg, spec)?),
                    }
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    out.push('}');
                }
                '}' => return Err(Trap::Unsupported),
                _ => out.push(c),
            }
        }
        Ok(out)
    }

    /// `str.format_map(mapping)`: renders `template`, resolving each `{name}` field by looking the
    /// name up (as a str key) in `mapping` (a dict). A missing key is a `KeyError`; a `:spec` is not
    /// yet supported. `{{`/`}}` escape to a literal brace.
    fn format_with_map(&self, template: &str, mapping: Value) -> Result<String, Trap> {
        let entries = self.dict_value(mapping).ok_or(Trap::TypeError)?.clone();
        let mut out = String::new();
        let mut chars = template.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' if chars.peek() == Some(&'{') => {
                    chars.next();
                    out.push('{');
                }
                '{' => {
                    let mut field = String::new();
                    let mut closed = false;
                    for fc in chars.by_ref() {
                        if fc == '}' {
                            closed = true;
                            break;
                        }
                        field.push(fc);
                    }
                    if !closed {
                        return Err(Trap::Unsupported);
                    }
                    let (name, spec) = match field.split_once(':') {
                        Some((n, s)) => (n, Some(s)),
                        None => (field.as_str(), None),
                    };
                    let value = entries
                        .iter()
                        .find_map(|(k, v)| (self.str_value(*k) == Some(name)).then_some(*v))
                        .ok_or(Trap::KeyError)?;
                    match spec {
                        None => out.push_str(&self.display(value)),
                        Some(spec) => out.push_str(&self.format_value_spec(value, spec)?),
                    }
                }
                '}' if chars.peek() == Some(&'}') => {
                    chars.next();
                    out.push('}');
                }
                '}' => return Err(Trap::Unsupported),
                _ => out.push(c),
            }
        }
        Ok(out)
    }

    /// Calls a bound method -- the `Call` dispatch when [`ObjectModel::is_bound_method`]. Reads the
    /// stored `[receiver, method_id]` and runs the receiver's method: list/dict/set/tuple/gpio/pin
    /// methods, else a `str` method (Python 3.14.6 "String Methods"). A wrong argument count, or a
    /// wrong-typed argument, is a `TypeError`.
    pub fn call_bound_method(&mut self, callee: Value, args: &[Value]) -> Result<Value, Trap> {
        let reference = callee.as_ref().ok_or(Trap::TypeError)?;
        let receiver = Value::from_bits(self.heap.read_u32(reference.0));
        let method_id = self.heap.read_u32(reference.0 + 4);
        if self.is_list(receiver) {
            return self.call_list_method(receiver, method_id, args);
        }
        if self.is_dict(receiver) {
            return self.call_dict_method(receiver, method_id, args);
        }
        if self.is_set(receiver) || self.is_frozenset(receiver) {
            return self.call_set_method(receiver, method_id, args);
        }
        if self.is_tuple(receiver) {
            return self.call_tuple_method(receiver, method_id, args);
        }
        #[cfg(feature = "complex")]
        if self.is_complex(receiver) {
            return self.call_complex_method(receiver, method_id, args);
        }
        if self.is_bytes(receiver) || self.is_bytearray(receiver) {
            return self.call_bytes_method(receiver, method_id, args);
        }
        if self.is_gpio(receiver) {
            return self.call_gpio_method(receiver, method_id, args);
        }
        if self.is_pin(receiver) {
            return self.call_pin_method(receiver, method_id, args);
        }
        match method_id {
            STR_UPPER | STR_LOWER => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let cased = if method_id == STR_UPPER {
                    s.to_uppercase()
                } else {
                    s.to_lowercase()
                };
                self.new_str(&cased)
            }
            STR_FORMAT => {
                let template = self.str_value(receiver).map(String::from).ok_or(Trap::TypeError)?;
                let rendered = self.format_template(&template, args)?;
                self.new_str(&rendered)
            }
            STR_FORMAT_MAP => {
                let [mapping] = args else {
                    return Err(Trap::TypeError);
                };
                let template = self.str_value(receiver).map(String::from).ok_or(Trap::TypeError)?;
                let rendered = self.format_with_map(&template, *mapping)?;
                self.new_str(&rendered)
            }
            STR_RSPLIT => {
                let (sep, maxsplit) = match args {
                    [] => (None, -1i64),
                    [sep] if sep.is_none() => (None, -1),
                    [sep] => (Some(String::from(self.str_value(*sep).ok_or(Trap::TypeError)?)), -1),
                    [sep, ms] => {
                        let limit = ms.as_int().ok_or(Trap::TypeError)?;
                        let sep = if sep.is_none() {
                            None
                        } else {
                            Some(String::from(self.str_value(*sep).ok_or(Trap::TypeError)?))
                        };
                        (sep, limit)
                    }
                    _ => return Err(Trap::TypeError),
                };
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let parts: Vec<String> = match &sep {
                    None => s.split_whitespace().map(String::from).collect(),
                    Some(sep) => {
                        if sep.is_empty() {
                            return Err(Trap::ValueError);
                        }
                        let all: Vec<&str> = s.split(sep.as_str()).collect();
                        if maxsplit < 0 || all.len() <= maxsplit as usize + 1 {
                            all.into_iter().map(String::from).collect()
                        } else {
                            let keep_from = all.len() - maxsplit as usize;
                            let head = all[..keep_from].join(sep.as_str());
                            let mut parts = alloc::vec![head];
                            parts.extend(all[keep_from..].iter().map(|p| String::from(*p)));
                            parts
                        }
                    }
                };
                let mut elems = Vec::with_capacity(parts.len());
                for p in &parts {
                    elems.push(self.new_str(p)?);
                }
                self.new_list(elems)
            }
            STR_CASEFOLD => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let folded = self.str_value(receiver).ok_or(Trap::TypeError)?.to_lowercase();
                self.new_str(&folded)
            }
            STR_ENCODE => {
                let name = match args {
                    [] => String::from("utf8"),
                    [encoding] => self.str_value(*encoding).map(String::from).ok_or(Trap::TypeError)?,
                    _ => return Err(Trap::TypeError),
                };
                let text = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                match name.to_ascii_lowercase().replace('-', "").as_str() {
                    "utf8" | "ascii" => self.new_bytes(text.into_bytes()),
                    _ => Err(Trap::Unsupported),
                }
            }
            STR_TRANSLATE => {
                let [table] = args else {
                    return Err(Trap::TypeError);
                };
                let entries = self.dict_value(*table).ok_or(Trap::TypeError)?.clone();
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut out = String::new();
                for ch in s.chars() {
                    let ordinal = ch as i64;
                    let replacement = entries
                        .iter()
                        .find_map(|(k, v)| (k.as_int() == Some(ordinal)).then_some(*v));
                    match replacement {
                        None => out.push(ch),
                        Some(r) if r.is_none() => {}
                        Some(r) => {
                            if let Some(ord) = r.as_int() {
                                let cp = u32::try_from(ord).map_err(|_| Trap::ValueError)?;
                                out.push(char::from_u32(cp).ok_or(Trap::ValueError)?);
                            } else if let Some(repl) = self.str_value(r) {
                                out.push_str(repl);
                            } else {
                                return Err(Trap::TypeError);
                            }
                        }
                    }
                }
                self.new_str(&out)
            }
            STR_STARTSWITH | STR_ENDSWITH => {
                let (affix, start, end) = affix_and_bounds(args)?;
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let (a, b) = normalize_bounds(start, end, s.chars().count() as i64);
                let window = cp_slice(s, a, b);
                let test = |affix: &str| {
                    if method_id == STR_STARTSWITH {
                        window.starts_with(affix)
                    } else {
                        window.ends_with(affix)
                    }
                };
                let holds = if let Some(affix) = self.str_value(affix) {
                    test(affix)
                } else if self.is_tuple(affix) {
                    let elems = self.seq_value(affix).ok_or(Trap::TypeError)?;
                    let mut any = false;
                    for &e in elems {
                        if test(self.str_value(e).ok_or(Trap::TypeError)?) {
                            any = true;
                            break;
                        }
                    }
                    any
                } else {
                    return Err(Trap::TypeError);
                };
                Ok(Value::from_bool(holds))
            }
            STR_FIND | STR_RFIND | STR_INDEX | STR_RINDEX => {
                let (sub, start, end) = affix_and_bounds(args)?;
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let sub = self.str_value(sub).ok_or(Trap::TypeError)?;
                let (a, b) = normalize_bounds(start, end, s.chars().count() as i64);
                let window = cp_slice(s, a, b);
                let from_right = method_id == STR_RFIND || method_id == STR_RINDEX;
                let found = if from_right { window.rfind(sub) } else { window.find(sub) };
                let index = match found {
                    Some(byte_offset) => a as i32 + window[..byte_offset].chars().count() as i32,
                    None => -1,
                };
                if index < 0 && (method_id == STR_INDEX || method_id == STR_RINDEX) {
                    return Err(Trap::ValueError);
                }
                Value::fixnum(index).ok_or(Trap::Overflow)
            }
            STR_STRIP | STR_LSTRIP | STR_RSTRIP => {
                let chars = match args {
                    [] => None,
                    [c] if c.is_none() => None,
                    [c] => Some(*c),
                    _ => return Err(Trap::TypeError),
                };
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let trimmed = match chars {
                    None => match method_id {
                        STR_STRIP => s.trim(),
                        STR_LSTRIP => s.trim_start(),
                        _ => s.trim_end(),
                    },
                    Some(chars) => {
                        let set = self.str_value(chars).ok_or(Trap::TypeError)?;
                        match method_id {
                            STR_STRIP => s.trim_matches(|c| set.contains(c)),
                            STR_LSTRIP => s.trim_start_matches(|c| set.contains(c)),
                            _ => s.trim_end_matches(|c| set.contains(c)),
                        }
                    }
                };
                let trimmed = String::from(trimmed);
                self.new_str(&trimmed)
            }
            STR_REPLACE => {
                let (old, new, count) = match args {
                    [old, new] => (*old, *new, -1i64),
                    [old, new, count] => (*old, *new, count.as_int().ok_or(Trap::TypeError)?),
                    _ => return Err(Trap::TypeError),
                };
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let old = self.str_value(old).ok_or(Trap::TypeError)?;
                let new = self.str_value(new).ok_or(Trap::TypeError)?;
                let replaced = if count < 0 {
                    s.replace(old, new)
                } else {
                    s.replacen(old, new, count as usize)
                };
                self.new_str(&replaced)
            }
            STR_COUNT => {
                let (sub, start, end) = affix_and_bounds(args)?;
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                let sub = self.str_value(sub).ok_or(Trap::TypeError)?;
                let (a, b) = normalize_bounds(start, end, s.chars().count() as i64);
                let window = cp_slice(s, a, b);
                Value::fixnum(window.matches(sub).count() as i32).ok_or(Trap::Overflow)
            }
            STR_ISDIGIT | STR_ISALPHA | STR_ISALNUM | STR_ISSPACE | STR_ISUPPER | STR_ISLOWER
            | STR_ISDECIMAL | STR_ISNUMERIC | STR_ISASCII | STR_ISIDENTIFIER => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let s = self.str_value(receiver).ok_or(Trap::TypeError)?;
                Ok(Value::from_bool(str_predicate(method_id, s)))
            }
            STR_SPLIT => {
                let (sep, maxsplit) = match args {
                    [] => (None, -1i64),
                    [sep] if sep.is_none() => (None, -1),
                    [sep] => (Some(String::from(self.str_value(*sep).ok_or(Trap::TypeError)?)), -1),
                    [sep, ms] => {
                        let limit = ms.as_int().ok_or(Trap::TypeError)?;
                        let sep = if sep.is_none() {
                            None
                        } else {
                            Some(String::from(self.str_value(*sep).ok_or(Trap::TypeError)?))
                        };
                        (sep, limit)
                    }
                    _ => return Err(Trap::TypeError),
                };
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let parts: Vec<String> = match &sep {
                    None => s.split_whitespace().map(String::from).collect(),
                    Some(sep) => {
                        if sep.is_empty() {
                            return Err(Trap::ValueError);
                        }
                        if maxsplit < 0 {
                            s.split(sep.as_str()).map(String::from).collect()
                        } else {
                            s.splitn(maxsplit as usize + 1, sep.as_str())
                                .map(String::from)
                                .collect()
                        }
                    }
                };
                let mut elems = Vec::with_capacity(parts.len());
                for p in &parts {
                    elems.push(self.new_str(p)?);
                }
                self.new_list(elems)
            }
            STR_JOIN => {
                let sep = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let iterable = match args {
                    [it] => *it,
                    _ => return Err(Trap::TypeError),
                };
                let parts: Vec<String> = {
                    let elems = self.seq_value(iterable).ok_or(Trap::TypeError)?;
                    let mut parts = Vec::with_capacity(elems.len());
                    for &e in elems {
                        parts.push(String::from(self.str_value(e).ok_or(Trap::TypeError)?));
                    }
                    parts
                };
                self.new_str(&parts.join(&sep))
            }
            STR_CAPITALIZE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut result = String::new();
                let mut chars = s.chars();
                if let Some(first) = chars.next() {
                    result.extend(first.to_uppercase());
                    result.extend(chars.flat_map(char::to_lowercase));
                }
                self.new_str(&result)
            }
            STR_TITLE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut result = String::new();
                let mut prev_cased = false;
                for c in s.chars() {
                    let cased = c.is_alphabetic();
                    if cased && !prev_cased {
                        result.extend(c.to_uppercase());
                    } else if cased {
                        result.extend(c.to_lowercase());
                    } else {
                        result.push(c);
                    }
                    prev_cased = cased;
                }
                self.new_str(&result)
            }
            STR_SWAPCASE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut result = String::new();
                for c in s.chars() {
                    if c.is_uppercase() {
                        result.extend(c.to_lowercase());
                    } else if c.is_lowercase() {
                        result.extend(c.to_uppercase());
                    } else {
                        result.push(c);
                    }
                }
                self.new_str(&result)
            }
            STR_SPLITLINES => {
                let keepends = match args {
                    [] => false,
                    [k] => self.py_truthy(*k)?.unwrap_or(false),
                    _ => return Err(Trap::TypeError),
                };
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut lines: Vec<String> = Vec::new();
                let mut current = String::new();
                let mut chars = s.chars().peekable();
                while let Some(c) = chars.next() {
                    match c {
                        '\n' => {
                            if keepends {
                                current.push('\n');
                            }
                            lines.push(core::mem::take(&mut current));
                        }
                        '\r' => {
                            let crlf = chars.peek() == Some(&'\n');
                            if crlf {
                                chars.next();
                            }
                            if keepends {
                                current.push('\r');
                                if crlf {
                                    current.push('\n');
                                }
                            }
                            lines.push(core::mem::take(&mut current));
                        }
                        _ => current.push(c),
                    }
                }
                if !current.is_empty() {
                    lines.push(current);
                }
                let mut elems = Vec::with_capacity(lines.len());
                for line in &lines {
                    elems.push(self.new_str(line)?);
                }
                self.new_list(elems)
            }
            STR_REMOVEPREFIX | STR_REMOVESUFFIX => {
                let [affix] = args else {
                    return Err(Trap::TypeError);
                };
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let affix = String::from(self.str_value(*affix).ok_or(Trap::TypeError)?);
                let result = if method_id == STR_REMOVEPREFIX {
                    s.strip_prefix(&affix).unwrap_or(&s)
                } else {
                    s.strip_suffix(&affix).unwrap_or(&s)
                };
                self.new_str(result)
            }
            STR_ZFILL => {
                let [width] = args else {
                    return Err(Trap::TypeError);
                };
                let width = width.as_int().ok_or(Trap::TypeError)?.max(0) as usize;
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let len = s.chars().count();
                let result = if len >= width {
                    s
                } else {
                    let pad = "0".repeat(width - len);
                    let mut chars = s.chars();
                    if matches!(s.chars().next(), Some('+' | '-')) {
                        let sign = chars.next().unwrap_or('+');
                        let mut r = String::new();
                        r.push(sign);
                        r.push_str(&pad);
                        r.push_str(chars.as_str());
                        r
                    } else {
                        let mut r = pad;
                        r.push_str(&s);
                        r
                    }
                };
                self.new_str(&result)
            }
            STR_LJUST | STR_RJUST | STR_CENTER => {
                let (width, fill) = match args {
                    [w] => (w.as_int().ok_or(Trap::TypeError)?, ' '),
                    [w, f] => {
                        let fs = self.str_value(*f).ok_or(Trap::TypeError)?;
                        let mut fc = fs.chars();
                        match (fc.next(), fc.next()) {
                            (Some(c), None) => (w.as_int().ok_or(Trap::TypeError)?, c),
                            _ => return Err(Trap::TypeError),
                        }
                    }
                    _ => return Err(Trap::TypeError),
                };
                let width = width.max(0) as usize;
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let len = s.chars().count();
                if len >= width {
                    return self.new_str(&s);
                }
                let pad = width - len;
                let (left, right) = match method_id {
                    STR_LJUST => (0, pad),
                    STR_RJUST => (pad, 0),
                    _ => {
                        let left = pad / 2 + (pad & width & 1);
                        (left, pad - left)
                    }
                };
                let mut result = String::new();
                for _ in 0..left {
                    result.push(fill);
                }
                result.push_str(&s);
                for _ in 0..right {
                    result.push(fill);
                }
                self.new_str(&result)
            }
            STR_PARTITION | STR_RPARTITION => {
                let [sep_val] = args else {
                    return Err(Trap::TypeError);
                };
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let sep = String::from(self.str_value(*sep_val).ok_or(Trap::TypeError)?);
                if sep.is_empty() {
                    return Err(Trap::ValueError);
                }
                let split = if method_id == STR_PARTITION {
                    s.find(&sep)
                } else {
                    s.rfind(&sep)
                };
                let (before, mid, after) = match split {
                    Some(byte) => (&s[..byte], sep.as_str(), &s[byte + sep.len()..]),
                    None if method_id == STR_PARTITION => (s.as_str(), "", ""),
                    None => ("", "", s.as_str()),
                };
                let parts = alloc::vec![
                    self.new_str(before)?,
                    self.new_str(mid)?,
                    self.new_str(after)?,
                ];
                self.new_tuple(parts)
            }
            STR_EXPANDTABS => {
                let tabsize = match args {
                    [] => 8,
                    [t] => t.as_int().ok_or(Trap::TypeError)?,
                    _ => return Err(Trap::TypeError),
                };
                let s = String::from(self.str_value(receiver).ok_or(Trap::TypeError)?);
                let mut result = String::new();
                let mut column: i64 = 0;
                for c in s.chars() {
                    match c {
                        '\t' => {
                            if tabsize > 0 {
                                let spaces = tabsize - (column % tabsize);
                                for _ in 0..spaces {
                                    result.push(' ');
                                }
                                column += spaces;
                            }
                        }
                        '\n' | '\r' => {
                            result.push(c);
                            column = 0;
                        }
                        _ => {
                            result.push(c);
                            column += 1;
                        }
                    }
                }
                self.new_str(&result)
            }
            _ => Err(Trap::Malformed),
        }
    }

    /// Dispatches a `list` method: `append(x)` (-> None), `pop([i])` (-> the removed element,
    /// default last, `IndexError` on empty / out of range).
    fn call_list_method(&mut self, list: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        let index = self.container_slot(list, self.list_type_id).ok_or(Trap::TypeError)?;
        match method_id {
            LIST_APPEND => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                self.seqs[index].push(*value);
                Ok(Value::NONE)
            }
            LIST_POP => {
                let len = self.seqs[index].len();
                if len == 0 {
                    return Err(Trap::IndexError);
                }
                let at = match args {
                    [] => len - 1,
                    [idx] => {
                        let i = idx.as_int().ok_or(Trap::TypeError)?;
                        let i = if i < 0 { i + len as i64 } else { i };
                        if i < 0 || i >= len as i64 {
                            return Err(Trap::IndexError);
                        }
                        i as usize
                    }
                    _ => return Err(Trap::TypeError),
                };
                Ok(self.seqs[index].remove(at))
            }
            LIST_SORT => {
                let mut elements = core::mem::take(&mut self.seqs[index]);
                let outcome = self.sort_values(&mut elements);
                self.seqs[index] = elements;
                outcome.map(|()| Value::NONE)
            }
            LIST_REVERSE => {
                self.seqs[index].reverse();
                Ok(Value::NONE)
            }
            LIST_INSERT => {
                let [at, value] = args else {
                    return Err(Trap::TypeError);
                };
                let len = self.seqs[index].len() as i64;
                let mut i = at.as_int().ok_or(Trap::TypeError)?;
                if i < 0 {
                    i = (i + len).max(0);
                }
                let pos = i.min(len) as usize;
                self.seqs[index].insert(pos, *value);
                Ok(Value::NONE)
            }
            LIST_REMOVE => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                match self.seqs[index].iter().position(|e| self.key_eq(*e, *value)) {
                    Some(p) => {
                        self.seqs[index].remove(p);
                        Ok(Value::NONE)
                    }
                    None => Err(Trap::ValueError),
                }
            }
            LIST_INDEX => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                match self.seqs[index].iter().position(|e| self.key_eq(*e, *value)) {
                    Some(p) => Value::fixnum(p as i32).ok_or(Trap::Overflow),
                    None => Err(Trap::ValueError),
                }
            }
            LIST_COUNT => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                let n = self.seqs[index]
                    .iter()
                    .filter(|e| self.key_eq(**e, *value))
                    .count();
                Value::fixnum(n as i32).ok_or(Trap::Overflow)
            }
            LIST_EXTEND => {
                let [iterable] = args else {
                    return Err(Trap::TypeError);
                };
                let iterator = self.new_iter(*iterable)?;
                let mut items = Vec::new();
                while let Some(item) = self.py_next(iterator)? {
                    items.push(item);
                }
                self.seqs[index].extend(items);
                Ok(Value::NONE)
            }
            LIST_CLEAR => {
                self.seqs[index].clear();
                Ok(Value::NONE)
            }
            LIST_COPY => {
                let copy = self.seqs[index].clone();
                self.new_list(copy)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a `dict` method: `get(k[, default])` (no KeyError), `keys`/`values`/`items`.
    /// keys/values/items return a new `list` (a cut: CPython returns live views; iteration and
    /// `list(...)` over them match).
    fn call_dict_method(&mut self, dict: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        let index = self.container_slot(dict, self.dict_type_id).ok_or(Trap::TypeError)?;
        match method_id {
            DICT_GET => {
                let (key, default) = match args {
                    [k] => (*k, Value::NONE),
                    [k, d] => (*k, *d),
                    _ => return Err(Trap::TypeError),
                };
                let found = self.dicts[index]
                    .iter()
                    .find(|(k, _)| self.key_eq(*k, key))
                    .map(|(_, v)| *v);
                Ok(found.unwrap_or(default))
            }
            DICT_KEYS => {
                let keys: Vec<Value> = self.dicts[index].iter().map(|(k, _)| *k).collect();
                self.new_list(keys)
            }
            DICT_VALUES => {
                let values: Vec<Value> = self.dicts[index].iter().map(|(_, v)| *v).collect();
                self.new_list(values)
            }
            DICT_ITEMS => {
                let pairs = self.dicts[index].clone();
                let mut items = Vec::with_capacity(pairs.len());
                for (key, value) in pairs {
                    items.push(self.new_tuple(alloc::vec![key, value])?);
                }
                self.new_list(items)
            }
            DICT_UPDATE => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let pairs = if let Some(entries) = self.dict_entries(*other) {
                    entries
                } else {
                    let iterator = self.new_iter(*other)?;
                    let mut kv = Vec::new();
                    while let Some(pair) = self.py_next(iterator)? {
                        let parts = self.unpack_sequence(pair, 2)?;
                        kv.push((parts[0], parts[1]));
                    }
                    kv
                };
                for (key, value) in pairs {
                    match self.dicts[index].iter().position(|(k, _)| self.key_eq(*k, key)) {
                        Some(slot) => self.dicts[index][slot].1 = value,
                        None => self.dicts[index].push((key, value)),
                    }
                }
                Ok(Value::NONE)
            }
            DICT_POP => {
                let (key, default) = match args {
                    [k] => (*k, None),
                    [k, d] => (*k, Some(*d)),
                    _ => return Err(Trap::TypeError),
                };
                match self.dicts[index].iter().position(|(k, _)| self.key_eq(*k, key)) {
                    Some(slot) => Ok(self.dicts[index].remove(slot).1),
                    None => default.ok_or(Trap::KeyError),
                }
            }
            DICT_SETDEFAULT => {
                let (key, default) = match args {
                    [k] => (*k, Value::NONE),
                    [k, d] => (*k, *d),
                    _ => return Err(Trap::TypeError),
                };
                match self.dicts[index].iter().position(|(k, _)| self.key_eq(*k, key)) {
                    Some(slot) => Ok(self.dicts[index][slot].1),
                    None => {
                        self.dicts[index].push((key, default));
                        Ok(default)
                    }
                }
            }
            DICT_CLEAR => {
                self.dicts[index].clear();
                Ok(Value::NONE)
            }
            DICT_COPY => {
                let copy = self.dicts[index].clone();
                self.new_dict(copy)
            }
            DICT_POPITEM => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                match self.dicts[index].pop() {
                    Some((key, value)) => self.new_tuple(alloc::vec![key, value]),
                    None => Err(Trap::KeyError),
                }
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// `dict.fromkeys(iterable, value)`: a new dict with each distinct element of `iterable` as a
    /// key, all mapped to `value`.
    pub fn new_dict_fromkeys(&mut self, iterable: Value, value: Value) -> Result<Value, Trap> {
        let keys = self.collect_elements(iterable)?;
        let mut entries: Vec<(Value, Value)> = Vec::new();
        for key in keys {
            if !entries.iter().any(|(existing, _)| self.key_eq(*existing, key)) {
                entries.push((key, value));
            }
        }
        self.new_dict(entries)
    }

    /// Collects any iterable into an owned `Vec` (a set/frozenset or list/tuple is cloned, else
    /// the iterator protocol drives it) -- the argument side of the set operations.
    fn collect_elements(&mut self, value: Value) -> Result<Vec<Value>, Trap> {
        if let Some(elems) = self.set_value(value) {
            return Ok(elems.clone());
        }
        if let Some(elems) = self.seq_value(value) {
            return Ok(elems.clone());
        }
        let iterator = self.new_iter(value)?;
        let mut elems = Vec::new();
        while let Some(item) = self.py_next(iterator)? {
            elems.push(item);
        }
        Ok(elems)
    }

    /// The union of `a` and `b`: `a`'s elements, then `b`'s new ones.
    fn set_union_elems(&self, a: &[Value], b: &[Value]) -> Vec<Value> {
        let mut result = a.to_vec();
        for &e in b {
            if !result.iter().any(|x| self.key_eq(*x, e)) {
                result.push(e);
            }
        }
        result
    }

    /// The elements of `a` that are (intersection) / are not (difference) also in `b`.
    fn set_filter_elems(&self, a: &[Value], b: &[Value], keep_common: bool) -> Vec<Value> {
        a.iter()
            .copied()
            .filter(|&x| b.iter().any(|&y| self.key_eq(x, y)) == keep_common)
            .collect()
    }

    /// Whether every element of `a` is in `b` (`a` is a subset of `b`).
    fn set_subset(&self, a: &[Value], b: &[Value]) -> bool {
        a.iter().all(|&x| b.iter().any(|&y| self.key_eq(x, y)))
    }

    /// Whether `a` and `b` share no element.
    fn set_disjoint(&self, a: &[Value], b: &[Value]) -> bool {
        !a.iter().any(|&x| b.iter().any(|&y| self.key_eq(x, y)))
    }

    /// Dispatches a `tuple` method: `index(x)` (the first position, `ValueError` if absent) and
    /// `count(x)` -- the immutable sequence reads over the shared arena.
    fn call_tuple_method(&mut self, tuple: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        let index = self.container_slot(tuple, self.tuple_type_id).ok_or(Trap::TypeError)?;
        let [value] = args else {
            return Err(Trap::TypeError);
        };
        match method_id {
            TUPLE_INDEX => match self.seqs[index].iter().position(|e| self.key_eq(*e, *value)) {
                Some(p) => Value::fixnum(p as i32).ok_or(Trap::Overflow),
                None => Err(Trap::ValueError),
            },
            TUPLE_COUNT => {
                let n = self.seqs[index].iter().filter(|e| self.key_eq(**e, *value)).count();
                Value::fixnum(n as i32).ok_or(Trap::Overflow)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a `bytes`/`bytearray` method: `hex()` -> the lowercase hex string; `decode()` ->
    /// the utf-8 `str` (an invalid sequence is a `ValueError`); `append(int)` / `extend(bytes-like)`
    /// mutate a bytearray in place (returning None).
    fn call_bytes_method(&mut self, receiver: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        match method_id {
            BYTES_HEX => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                let mut hex = String::new();
                for &byte in self.bytes_value(receiver).ok_or(Trap::TypeError)? {
                    hex.push_str(&alloc::format!("{byte:02x}"));
                }
                self.new_str(&hex)
            }
            BYTES_DECODE => {
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let text = core::str::from_utf8(&data).map_err(|_| Trap::ValueError)?;
                let owned = String::from(text);
                self.new_str(&owned)
            }
            BYTEARRAY_APPEND => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                let byte = value.as_int().ok_or(Trap::TypeError)?;
                if !(0..=255).contains(&byte) {
                    return Err(Trap::ValueError);
                }
                let slot = self.byte_buffer_slot(receiver).ok_or(Trap::TypeError)?;
                self.byte_buffers[slot].push(byte as u8);
                Ok(Value::NONE)
            }
            BYTEARRAY_EXTEND => {
                let [source] = args else {
                    return Err(Trap::TypeError);
                };
                let extra = self.bytes_value(*source).ok_or(Trap::TypeError)?.to_vec();
                let slot = self.byte_buffer_slot(receiver).ok_or(Trap::TypeError)?;
                self.byte_buffers[slot].extend(extra);
                Ok(Value::NONE)
            }
            BYTES_STARTSWITH | BYTES_ENDSWITH => {
                let [prefix] = args else {
                    return Err(Trap::TypeError);
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?;
                let needle = self.bytes_value(*prefix).ok_or(Trap::TypeError)?;
                let matches = if method_id == BYTES_STARTSWITH {
                    data.starts_with(needle)
                } else {
                    data.ends_with(needle)
                };
                Ok(Value::from_bool(matches))
            }
            BYTES_FIND => {
                let [sub] = args else {
                    return Err(Trap::TypeError);
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?;
                let needle = self.bytes_value(*sub).ok_or(Trap::TypeError)?;
                let index = if needle.is_empty() {
                    0
                } else {
                    data.windows(needle.len()).position(|w| w == needle).map_or(-1, |p| p as i64)
                };
                Value::fixnum(i32::try_from(index).map_err(|_| Trap::Overflow)?).ok_or(Trap::Overflow)
            }
            BYTES_COUNT => {
                let [sub] = args else {
                    return Err(Trap::TypeError);
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?;
                let needle = self.bytes_value(*sub).ok_or(Trap::TypeError)?;
                let count = if needle.is_empty() {
                    data.len() + 1
                } else {
                    let mut n = 0;
                    let mut i = 0;
                    while i + needle.len() <= data.len() {
                        if &data[i..i + needle.len()] == needle {
                            n += 1;
                            i += needle.len();
                        } else {
                            i += 1;
                        }
                    }
                    n
                };
                Value::fixnum(count as i32).ok_or(Trap::Overflow)
            }
            BYTES_REPLACE => {
                let [old, new] = args else {
                    return Err(Trap::TypeError);
                };
                let data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                let old = self.bytes_value(*old).ok_or(Trap::TypeError)?.to_vec();
                let new = self.bytes_value(*new).ok_or(Trap::TypeError)?.to_vec();
                let replaced = replace_bytes(&data, &old, &new);
                if self.is_bytearray(receiver) {
                    self.new_bytearray(replaced)
                } else {
                    self.new_bytes(replaced)
                }
            }
            BYTES_UPPER | BYTES_LOWER => {
                let mut data = self.bytes_value(receiver).ok_or(Trap::TypeError)?.to_vec();
                for byte in &mut data {
                    *byte = if method_id == BYTES_UPPER {
                        byte.to_ascii_uppercase()
                    } else {
                        byte.to_ascii_lowercase()
                    };
                }
                if self.is_bytearray(receiver) {
                    self.new_bytearray(data)
                } else {
                    self.new_bytes(data)
                }
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a `complex` method: `conjugate()` flips the sign of the imaginary part.
    #[cfg(feature = "complex")]
    fn call_complex_method(&mut self, complex: Value, method_id: u32, args: &[Value]) -> Result<Value, Trap> {
        let (re, im) = self.complex_value(complex).ok_or(Trap::TypeError)?;
        match method_id {
            COMPLEX_CONJUGATE => {
                if !args.is_empty() {
                    return Err(Trap::TypeError);
                }
                self.new_complex(re, -im)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// Dispatches a `set`/`frozenset` method. The algebra (union/intersection/difference/
    /// symmetric_difference) returns a NEW set of the receiver's kind; the predicates
    /// (issubset/issuperset/isdisjoint) a bool; the mutators (add/discard/remove/clear/pop/
    /// update) act in place (only a mutable set reaches them). An argument is any iterable.
    fn call_set_method(
        &mut self,
        receiver: Value,
        method_id: u32,
        args: &[Value],
    ) -> Result<Value, Trap> {
        let frozen = self.is_frozenset(receiver);
        match method_id {
            SET_COPY => {
                let elems = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                if frozen {
                    self.new_frozenset(elems)
                } else {
                    self.new_set(elems)
                }
            }
            SET_UNION | SET_INTERSECTION | SET_DIFFERENCE | SET_SYMMETRIC_DIFFERENCE => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let a = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                let b = self.collect_elements(*other)?;
                let result = match method_id {
                    SET_UNION => self.set_union_elems(&a, &b),
                    SET_INTERSECTION => self.set_filter_elems(&a, &b, true),
                    SET_DIFFERENCE => self.set_filter_elems(&a, &b, false),
                    _ => {
                        let mut r = self.set_filter_elems(&a, &b, false);
                        r.extend(self.set_filter_elems(&b, &a, false));
                        r
                    }
                };
                if frozen {
                    self.new_frozenset(result)
                } else {
                    self.new_set(result)
                }
            }
            SET_ISSUBSET | SET_ISSUPERSET | SET_ISDISJOINT => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let a = self.set_value(receiver).ok_or(Trap::TypeError)?.clone();
                let b = self.collect_elements(*other)?;
                let result = match method_id {
                    SET_ISSUBSET => self.set_subset(&a, &b),
                    SET_ISSUPERSET => self.set_subset(&b, &a),
                    _ => self.set_disjoint(&a, &b),
                };
                Ok(Value::from_bool(result))
            }
            SET_ADD => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                self.set_add(receiver, *value)?;
                Ok(Value::NONE)
            }
            SET_DISCARD | SET_REMOVE => {
                let [value] = args else {
                    return Err(Trap::TypeError);
                };
                let slot = self.container_slot(receiver, self.set_type_id).ok_or(Trap::TypeError)?;
                match self.sets[slot].iter().position(|e| self.key_eq(*e, *value)) {
                    Some(p) => {
                        self.sets[slot].remove(p);
                        Ok(Value::NONE)
                    }
                    None if method_id == SET_REMOVE => Err(Trap::KeyError),
                    None => Ok(Value::NONE),
                }
            }
            SET_CLEAR => {
                let slot = self.container_slot(receiver, self.set_type_id).ok_or(Trap::TypeError)?;
                self.sets[slot].clear();
                Ok(Value::NONE)
            }
            SET_POP => {
                let slot = self.container_slot(receiver, self.set_type_id).ok_or(Trap::TypeError)?;
                if self.sets[slot].is_empty() {
                    return Err(Trap::KeyError);
                }
                Ok(self.sets[slot].remove(0))
            }
            SET_UPDATE => {
                let [other] = args else {
                    return Err(Trap::TypeError);
                };
                let b = self.collect_elements(*other)?;
                let slot = self.container_slot(receiver, self.set_type_id).ok_or(Trap::TypeError)?;
                for e in b {
                    if !self.sets[slot].iter().any(|x| self.key_eq(*x, e)) {
                        self.sets[slot].push(e);
                    }
                }
                Ok(Value::NONE)
            }
            _ => Err(Trap::AttributeError),
        }
    }

    /// `set <op> set` for the `| & - ^` operators (both operands must be sets/frozensets); the
    /// result takes the LEFT operand's kind.
    pub(crate) fn set_binary_op(&mut self, op: BinOp, a: Value, b: Value) -> Result<Value, Trap> {
        let a_elems = self.set_value(a).ok_or(Trap::TypeError)?.clone();
        let b_elems = self.set_value(b).ok_or(Trap::TypeError)?.clone();
        let result = match op {
            BinOp::BitOr => self.set_union_elems(&a_elems, &b_elems),
            BinOp::BitAnd => self.set_filter_elems(&a_elems, &b_elems, true),
            BinOp::Sub => self.set_filter_elems(&a_elems, &b_elems, false),
            BinOp::BitXor => {
                let mut r = self.set_filter_elems(&a_elems, &b_elems, false);
                r.extend(self.set_filter_elems(&b_elems, &a_elems, false));
                r
            }
            _ => return Err(Trap::TypeError),
        };
        if self.is_frozenset(a) {
            self.new_frozenset(result)
        } else {
            self.new_set(result)
        }
    }

    /// `set <cmp> other`: == / != by element equality (a non-set `other` simply compares unequal,
    /// not an error); < <= > >= are (proper) subset/superset and require `other` to be a set.
    pub(crate) fn set_compare(&self, op: CmpOp, a: Value, b: Value) -> Result<Value, Trap> {
        let a_elems = self.set_value(a).ok_or(Trap::TypeError)?;
        let b_set = self.set_value(b);
        let value = match op {
            CmpOp::Eq | CmpOp::Ne => {
                let equal = match b_set {
                    Some(b_elems) => {
                        self.set_subset(a_elems, b_elems) && self.set_subset(b_elems, a_elems)
                    }
                    None => false,
                };
                if matches!(op, CmpOp::Ne) {
                    !equal
                } else {
                    equal
                }
            }
            CmpOp::Le => self.set_subset(a_elems, b_set.ok_or(Trap::TypeError)?),
            CmpOp::Ge => self.set_subset(b_set.ok_or(Trap::TypeError)?, a_elems),
            CmpOp::Lt => {
                let b_elems = b_set.ok_or(Trap::TypeError)?;
                self.set_subset(a_elems, b_elems) && !self.set_subset(b_elems, a_elems)
            }
            CmpOp::Gt => {
                let b_elems = b_set.ok_or(Trap::TypeError)?;
                self.set_subset(b_elems, a_elems) && !self.set_subset(a_elems, b_elems)
            }
            CmpOp::Is | CmpOp::IsNot => unreachable!("is/is not handled in the Op::Compare path"),
        };
        Ok(Value::from_bool(value))
    }

    /// Python's ordering comparison for `sorted`/`min`/`max`/`list.sort`/the ordering operators:
    /// int/bool numerically, str lexicographically (by code point), and tuple/list element-wise
    /// (lexicographic, recursive, shorter-is-less when one is a prefix). Comparing two different
    /// orderable kinds -- or any unorderable type -- is a `TypeError`, matching CPython (`1 < "a"`
    /// raises). Only same-kind sequences compare (`[1] < (1,)` is a TypeError).
    pub(crate) fn compare_ordered(&self, a: Value, b: Value) -> Result<Ordering, Trap> {
        if let (Some(x), Some(y)) = (self.as_i128(a), self.as_i128(b)) {
            return Ok(x.cmp(&y));
        }
        if self.is_int(a) && self.is_int(b) {
            if let (Some(x), Some(y)) = (self.as_bigint(a), self.as_bigint(b)) {
                return Ok(x.cmp(&y));
            }
        }
        if self.is_float(a) || self.is_float(b) {
            if let (Some(x), Some(y)) = (self.as_f64(a), self.as_f64(b)) {
                return Ok(x.partial_cmp(&y).unwrap_or(Ordering::Equal));
            }
        }
        if let (Some(x), Some(y)) = (self.str_value(a), self.str_value(b)) {
            return Ok(x.cmp(y));
        }
        let same_sequence =
            (self.is_tuple(a) && self.is_tuple(b)) || (self.is_list(a) && self.is_list(b));
        if same_sequence {
            if let (Some(xs), Some(ys)) = (self.seq_value(a), self.seq_value(b)) {
                let mut i = 0;
                while i < xs.len() && i < ys.len() {
                    match self.compare_ordered(xs[i], ys[i])? {
                        Ordering::Equal => i += 1,
                        non_equal => return Ok(non_equal),
                    }
                }
                return Ok(xs.len().cmp(&ys.len()));
            }
        }
        Err(Trap::TypeError)
    }

    /// Sorts `elements` in place by Python ordering ([`ObjectModel::compare_ordered`]), stably; an
    /// unorderable pair is a `TypeError`. Shared by `list.sort` and the `sorted` built-in.
    pub(crate) fn sort_values(&self, elements: &mut [Value]) -> Result<(), Trap> {
        let mut error = None;
        elements.sort_by(|a, b| match self.compare_ordered(*a, *b) {
            Ok(ordering) => ordering,
            Err(trap) => {
                error = error.or(Some(trap));
                Ordering::Equal
            }
        });
        match error {
            Some(trap) => Err(trap),
            None => Ok(()),
        }
    }

    /// Reorders `pairs` (sort-key, element) IN PLACE so that reading `pairs[i].1` gives the elements
    /// sorted by their key -- all-int numerically or all-str lexicographically (a mixed/unorderable
    /// set of keys is a `TypeError`), stably, DESCENDING when `reverse`. Backs `sorted(key=...,
    /// reverse=...)`: ties keep their original order (Python's stable sort), so `reverse` is a
    /// directional stable sort, not an ascending sort reversed (which would flip equal-key ties).
    pub(crate) fn sort_pairs_by_key(
        &self,
        pairs: &mut [(Value, Value)],
        reverse: bool,
    ) -> Result<(), Trap> {
        let mut error = None;
        pairs.sort_by(|a, b| match self.compare_ordered(a.0, b.0) {
            Ok(ordering) => {
                if reverse {
                    ordering.reverse()
                } else {
                    ordering
                }
            }
            Err(trap) => {
                error = error.or(Some(trap));
                Ordering::Equal
            }
        });
        match error {
            Some(trap) => Err(trap),
            None => Ok(()),
        }
    }

    /// Whether `callee` is a bound `list.sort` method -- the one built-in method with a keyword
    /// surface (`sort(key=None, reverse=False)`), which the interpreter's `CallKw` routes specially.
    #[must_use]
    pub fn is_list_sort_bound(&self, callee: Value) -> bool {
        callee.as_ref().is_some_and(|reference| {
            self.heap.type_id_of(reference) == self.bound_method_type_id && {
                let receiver = Value::from_bits(self.heap.read_u32(reference.0));
                let method_id = self.heap.read_u32(reference.0 + 4);
                self.is_list(receiver) && method_id == LIST_SORT
            }
        })
    }

    /// The receiver a bound method is bound to (its `self`); the precondition is that `callee` is a
    /// bound method (e.g. [`ObjectModel::is_list_sort_bound`]).
    #[must_use]
    pub fn bound_receiver(&self, callee: Value) -> Value {
        let reference = callee.as_ref().expect("a bound method");
        Value::from_bits(self.heap.read_u32(reference.0))
    }

    /// The method id a bound method carries (payload word at offset 4, a raw int, not a tagged
    /// slot); the precondition is that `callee` is a bound method.
    #[must_use]
    pub fn bound_method_id(&self, callee: Value) -> u32 {
        let reference = callee.as_ref().expect("a bound method");
        self.heap.read_u32(reference.0 + 4)
    }

    /// A clone of a list/tuple's elements (so a caller can compute over them without holding a
    /// borrow on the model), or `None` if `value` is not a sequence.
    #[must_use]
    pub fn seq_elements(&self, value: Value) -> Option<Vec<Value>> {
        self.seq_value(value).cloned()
    }

    /// Sorts list `receiver` IN PLACE for `list.sort(key=, reverse=)`: by `keys[i]` (element i's
    /// precomputed sort key, or the element itself when `keys` is `None`), descending when
    /// `reverse`. `keys`, when given, must have one entry per element. Returns `None`.
    pub fn list_sort_in_place(
        &mut self,
        receiver: Value,
        keys: Option<Vec<Value>>,
        reverse: bool,
    ) -> Result<Value, Trap> {
        let index = self.seq_slot(receiver).ok_or(Trap::TypeError)?;
        let mut elements = core::mem::take(&mut self.seqs[index]);
        let outcome = self.sort_elements(&mut elements, keys, reverse);
        self.seqs[index] = elements;
        outcome.map(|()| Value::NONE)
    }

    /// The in-place sort behind [`ObjectModel::list_sort_in_place`]: no key sorts the elements
    /// directly (reverse flips), a key sorts (element, key) pairs by key (directional stable, so
    /// ties keep original order under reverse).
    fn sort_elements(
        &self,
        elements: &mut [Value],
        keys: Option<Vec<Value>>,
        reverse: bool,
    ) -> Result<(), Trap> {
        match keys {
            None => {
                self.sort_values(elements)?;
                if reverse {
                    elements.reverse();
                }
                Ok(())
            }
            Some(keys) => {
                if keys.len() != elements.len() {
                    return Err(Trap::TypeError);
                }
                let mut pairs: Vec<(Value, Value)> =
                    keys.into_iter().zip(elements.iter().copied()).collect();
                self.sort_pairs_by_key(&mut pairs, reverse)?;
                for (slot, (_, element)) in pairs.into_iter().enumerate() {
                    elements[slot] = element;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point_model() -> (ObjectModel, Value) {
        let mut model = ObjectModel::new(alloc::vec![PyType::with_slots("Point", &["x", "y"])], 4096);
        let obj = model
            .new_instance(0, &[Value::fixnum(7).unwrap(), Value::fixnum(9).unwrap()])
            .unwrap();
        (model, obj)
    }

    #[test]
    fn str_round_trips_and_reports_codepoint_len() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("héllo").unwrap();
        assert!(s.is_pointer());
        assert_eq!(model.str_value(s), Some("héllo"));
        assert_eq!(model.py_len(s).unwrap().as_fixnum(), Some(5));
        assert_eq!(model.str_value(Value::fixnum(1).unwrap()), None);
        assert_eq!(model.py_len(Value::NONE), Err(Trap::TypeError));
        let upper = model.getattr(s, "upper", &mut InlineCache::empty()).unwrap();
        assert!(model.is_bound_method(upper));
        assert_eq!(
            model.getattr(s, "nope", &mut InlineCache::empty()),
            Err(Trap::AttributeError)
        );
    }

    #[test]
    fn str_methods_upper_lower_via_bound_method() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("Héllo").unwrap();
        assert!(!model.is_bound_method(s));
        let upper = model.getattr(s, "upper", &mut InlineCache::empty()).unwrap();
        assert!(model.is_bound_method(upper));
        let up = model.call_bound_method(upper, &[]).unwrap();
        assert_eq!(model.str_value(up), Some("HÉLLO"));
        let lower = model.getattr(s, "lower", &mut InlineCache::empty()).unwrap();
        let lo = model.call_bound_method(lower, &[]).unwrap();
        assert_eq!(model.str_value(lo), Some("héllo"));
        let again = model.getattr(s, "upper", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(again, &[s]), Err(Trap::TypeError));
    }

    #[test]
    fn str_format_substitutes_positional_fields() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let t1 = model.new_str("{} + {} = {}").unwrap();
        let b1 = model.getattr(t1, "format", &mut InlineCache::empty()).unwrap();
        let r1 = model.call_bound_method(b1, &[n(1), n(2), n(3)]).unwrap();
        assert_eq!(model.str_value(r1), Some("1 + 2 = 3"));
        let t2 = model.new_str("{0}{1}{0} {{x}}").unwrap();
        let b2 = model.getattr(t2, "format", &mut InlineCache::empty()).unwrap();
        let r2 = model.call_bound_method(b2, &[n(7), n(8)]).unwrap();
        assert_eq!(model.str_value(r2), Some("787 {x}"));
        let t3 = model.new_str("{} {}").unwrap();
        let b3 = model.getattr(t3, "format", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(b3, &[n(1)]), Err(Trap::IndexError));
    }

    #[test]
    fn list_sort_in_place_no_key_and_with_keys() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let l = model.new_list(alloc::vec![n(3), n(1), n(2)]).unwrap();
        model.list_sort_in_place(l, None, true).unwrap();
        assert_eq!(model.repr(l), "[3, 2, 1]");
        let l2 = model.new_list(alloc::vec![n(10), n(20), n(30)]).unwrap();
        model.list_sort_in_place(l2, Some(alloc::vec![n(3), n(1), n(2)]), false).unwrap();
        assert_eq!(model.repr(l2), "[20, 30, 10]");
        let l3 = model.new_list(alloc::vec![n(100), n(200)]).unwrap();
        model.list_sort_in_place(l3, Some(alloc::vec![n(1), n(1)]), true).unwrap();
        assert_eq!(model.repr(l3), "[100, 200]");
    }

    #[test]
    fn exception_instances_render_their_message() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let value_error = model.exception_class("ValueError").unwrap();
        let exc = model.new_object(value_error).unwrap();
        let msg = model.new_str("bad input").unwrap();
        model.init_default_args(exc, &[msg]).unwrap();
        assert_eq!(model.display(exc), "bad input");
        let key_error = model.exception_class("KeyError").unwrap();
        let ke = model.new_object(key_error).unwrap();
        let key = model.new_str("missing").unwrap();
        model.init_default_args(ke, &[key]).unwrap();
        assert_eq!(model.display(ke), "'missing'");
        let empty = model.new_object(value_error).unwrap();
        assert_eq!(model.display(empty), "");
    }

    #[test]
    fn repr_of_builtin_types_and_functions() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        assert_eq!(model.repr(Value::builtin_ref(Builtin::Int.id())), "<class 'int'>");
        assert_eq!(model.repr(Value::builtin_ref(Builtin::List.id())), "<class 'list'>");
        assert_eq!(model.repr(Value::builtin_ref(Builtin::Abs.id())), "<built-in function abs>");
        let mut cache = InlineCache::empty();
        let int_name = model
            .getattr(Value::builtin_ref(Builtin::Int.id()), "__name__", &mut cache)
            .unwrap();
        assert_eq!(model.str_value(int_name), Some("int"));
    }

    #[test]
    fn py_delattr_removes_an_instance_attribute() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let class = model.exception_class("ValueError").unwrap();
        let obj = model.new_object(class).unwrap();
        model.py_setattr_instance(obj, "x", Value::fixnum(1).unwrap()).unwrap();
        let mut cache = InlineCache::empty();
        assert_eq!(model.getattr(obj, "x", &mut cache).unwrap().as_fixnum(), Some(1));
        model.py_delattr_instance(obj, "x").unwrap();
        assert_eq!(model.getattr(obj, "x", &mut cache), Err(Trap::AttributeError));
        assert_eq!(model.py_delattr_instance(obj, "x"), Err(Trap::AttributeError));
    }

    #[test]
    fn provided_module_resolves_members_as_attributes() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let answer_key = model.new_str("answer").unwrap();
        let name_key = model.new_str("name").unwrap();
        let name_val = model.new_str("lib").unwrap();
        let namespace = model
            .new_dict(alloc::vec![
                (answer_key, Value::fixnum(42).unwrap()),
                (name_key, name_val),
            ])
            .unwrap();
        model.provide_module("mylib", namespace).unwrap();
        let module = model.get_global("mylib").unwrap();
        assert!(model.is_module_object(module));
        let mut cache = InlineCache::empty();
        assert_eq!(model.getattr(module, "answer", &mut cache).unwrap().as_fixnum(), Some(42));
        let name = model.getattr(module, "name", &mut cache).unwrap();
        assert_eq!(model.str_value(name), Some("lib"));
        assert_eq!(model.getattr(module, "nope", &mut cache), Err(Trap::AttributeError));
    }

    #[test]
    fn new_long_normalizes_and_round_trips() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let small = model.new_long(42).unwrap();
        assert_eq!(small.as_fixnum(), Some(42));
        assert!(!model.is_long(small));
        let big = model.new_long(1_000_000_000_000_000_000).unwrap();
        assert!(model.is_long(big));
        assert_eq!(model.long_value(big), Some(1_000_000_000_000_000_000));
        assert_eq!(model.as_i128(big), Some(1_000_000_000_000_000_000));
        assert_eq!(model.display(big), "1000000000000000000");
        assert_eq!(model.as_i128(Value::fixnum(7).unwrap()), Some(7));
    }

    #[test]
    fn new_float_round_trips_and_coerces() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        for &n in &[0.0, -0.0, 1.0, 0.1, -3.5, 1e300, f64::INFINITY] {
            let v = model.new_float(n).unwrap();
            assert!(model.is_float(v));
            assert!(!model.is_long(v));
            assert_eq!(model.float_value(v).unwrap().to_bits(), n.to_bits());
            assert_eq!(model.as_f64(v), Some(n));
        }
        assert_eq!(model.as_f64(Value::fixnum(7).unwrap()), Some(7.0));
        assert_eq!(model.as_f64(Value::TRUE), Some(1.0));
        let big = model.new_long(1_000_000_000_000_000_000).unwrap();
        assert_eq!(model.as_f64(big), Some(1e18));
        assert!(!model.is_float(Value::fixnum(7).unwrap()));
        let nan = model.new_float(f64::NAN).unwrap();
        assert!(model.is_float(nan));
        assert!(model.float_value(nan).unwrap().is_nan());
    }

    #[cfg(feature = "complex")]
    #[test]
    fn new_complex_round_trips_and_reprs() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let c = model.new_complex(1.5, -2.5).unwrap();
        assert!(model.is_complex(c));
        assert_eq!(model.complex_value(c), Some((1.5, -2.5)));
        assert_eq!(model.as_complex(Value::fixnum(3).unwrap()), Some((3.0, 0.0)));
        assert_eq!(model.as_complex(c), Some((1.5, -2.5)));
        let f = model.new_float(1.5).unwrap();
        assert!(!model.is_complex(f));
        assert!(model.float_value(c).is_none());
    }

    #[cfg(feature = "complex")]
    #[test]
    fn format_complex_matches_cpython_repr() {
        let cases: &[(f64, f64, &str)] = &[
            (0.0, 0.0, "0j"),
            (0.0, 1.0, "1j"),
            (0.0, -1.0, "-1j"),
            (1.0, 2.0, "(1+2j)"),
            (3.0, -4.0, "(3-4j)"),
            (1.0, 0.0, "(1+0j)"),
            (2.5, -0.5, "(2.5-0.5j)"),
            (-0.0, -1.0, "(-0-1j)"),
            (-0.0, -0.0, "(-0-0j)"),
            (1e100, 2e-5, "(1e+100+2e-05j)"),
        ];
        for &(re, im, expected) in cases {
            assert_eq!(format_complex(re, im), expected, "complex({re}, {im})");
        }
    }

    #[test]
    fn format_float_matches_cpython_repr() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (-1.0, "-1.0"),
            (3.0, "3.0"),
            (0.5, "0.5"),
            (0.1, "0.1"),
            (0.1 + 0.2, "0.30000000000000004"),
            (1.0 / 3.0, "0.3333333333333333"),
            (10.0 / 3.0, "3.3333333333333335"),
            (100.0, "100.0"),
            (123.456, "123.456"),
            (1.2345678901234567, "1.2345678901234567"),
            (0.1234567890123456, "0.1234567890123456"),
            (1e15, "1000000000000000.0"),
            (1234567890123456.0, "1234567890123456.0"),
            (1e16, "1e+16"),
            (1e17, "1e+17"),
            (12345678901234567.0, "1.2345678901234568e+16"),
            (1e20, "1e+20"),
            (1e100, "1e+100"),
            (1e308, "1e+308"),
            (1e-4, "0.0001"),
            (0.000123, "0.000123"),
            (1e-5, "1e-05"),
            (1e-100, "1e-100"),
            (1e-308, "1e-308"),
            (-0.1, "-0.1"),
            (-1e16, "-1e+16"),
            (-1e-5, "-1e-05"),
            (f64::INFINITY, "inf"),
            (f64::NEG_INFINITY, "-inf"),
            (f64::NAN, "nan"),
        ];
        for &(value, expected) in cases {
            assert_eq!(format_float(value), expected, "repr({value:e})");
        }
    }

    #[test]
    fn format_value_spec_float_types_match_python() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let cases: &[(&str, f64, &str)] = &[
            (".2f", 3.14159, "3.14"),
            (".0f", 3.14159, "3"),
            ("08.2f", 3.14159, "00003.14"),
            ("+.2f", 3.14159, "+3.14"),
            ("^10.2f", 3.14159, "   3.14   "),
            (".2e", 12345.678, "1.23e+04"),
            (".0e", 12345.678, "1e+04"),
            ("E", 3.14159, "3.141590E+00"),
            ("g", 1234567.0, "1.23457e+06"),
            (".3g", 0.0001234, "0.000123"),
            (".1%", 0.5, "50.0%"),
            (".3", 3.0, "3.0"),
            (",.2f", 1234567.89, "1,234,567.89"),
            ("_.0f", 1234567.0, "1_234_567"),
            (".2f", -0.0, "-0.00"),
            (".2f", f64::INFINITY, "inf"),
            ("+f", f64::NAN, "+nan"),
            ("F", f64::INFINITY, "INF"),
        ];
        for &(spec, value, expected) in cases {
            let v = model.new_float(value).unwrap();
            assert_eq!(model.format_value_spec(v, spec).unwrap(), expected, "format({value:e}, {spec:?})");
        }
    }

    #[test]
    fn py_hash_matches_python_for_ints_and_rejects_unhashables() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        assert_eq!(model.py_hash(Value::fixnum(5).unwrap()).unwrap().as_fixnum(), Some(5));
        assert_eq!(model.py_hash(Value::fixnum(-1).unwrap()).unwrap().as_fixnum(), Some(-2));
        assert_eq!(model.py_hash(Value::TRUE).unwrap().as_fixnum(), Some(1));
        let s = model.new_str("abc").unwrap();
        assert_eq!(model.py_hash(s).unwrap(), model.py_hash(s).unwrap());
        let t = model.new_tuple(alloc::vec![Value::fixnum(1).unwrap(), s]).unwrap();
        assert_eq!(model.py_hash(t).unwrap(), model.py_hash(t).unwrap());
        let l = model.new_list(alloc::vec![Value::fixnum(1).unwrap()]).unwrap();
        assert_eq!(model.py_hash(l), Err(Trap::TypeError));
    }

    #[test]
    fn new_dict_fromkeys_dedups_keys() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let keys = model
            .new_list(alloc::vec![
                Value::fixnum(1).unwrap(),
                Value::fixnum(1).unwrap(),
                Value::fixnum(2).unwrap(),
            ])
            .unwrap();
        let zero = Value::fixnum(0).unwrap();
        let d = model.new_dict_fromkeys(keys, zero).unwrap();
        assert_eq!(model.py_len(d).unwrap().as_fixnum(), Some(2));
    }

    #[test]
    fn trap_to_exception_carries_context_and_constant_messages() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let key = model.new_str("missing").unwrap();
        model.set_trap_arg(key);
        let exc = model.trap_to_exception(Trap::KeyError).unwrap();
        assert_eq!(model.display(exc), "'missing'");
        let zde = model.trap_to_exception(Trap::ZeroDivisionError).unwrap();
        assert_eq!(model.display(zde), "division by zero");
        let trap = model.with_message(Trap::IndexError, "list index out of range");
        let ie = model.trap_to_exception(trap).unwrap();
        assert_eq!(model.display(ie), "list index out of range");
        let te = model.trap_to_exception(Trap::TypeError).unwrap();
        assert_eq!(model.display(te), "");
    }

    #[test]
    fn format_value_spec_covers_int_and_str() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let n = Value::fixnum(42).unwrap();
        assert_eq!(model.format_value_spec(n, "05d").unwrap(), "00042");
        assert_eq!(model.format_value_spec(n, "x").unwrap(), "2a");
        assert_eq!(model.format_value_spec(Value::fixnum(255).unwrap(), "#X").unwrap(), "0XFF");
        assert_eq!(model.format_value_spec(n, ">5").unwrap(), "   42");
        assert_eq!(model.format_value_spec(Value::fixnum(-42).unwrap(), "+d").unwrap(), "-42");
        let s = model.new_str("abcdef").unwrap();
        assert_eq!(model.format_value_spec(s, ".3").unwrap(), "abc");
        assert_eq!(model.format_value_spec(s, "^8").unwrap(), " abcdef ");
        assert_eq!(model.format_value_spec(n, ".2f"), Err(Trap::Unsupported));
    }

    #[test]
    fn str_dispatch_binary_compare_truthy() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let a = model.new_str("ab").unwrap();
        let b = model.new_str("cd").unwrap();
        let one = Value::fixnum(1).unwrap();

        let cat = model.py_binary(BinOp::Add, a, b).unwrap().unwrap();
        assert_eq!(model.str_value(cat), Some("abcd"));
        assert_eq!(model.py_binary(BinOp::Add, a, one), Err(Trap::TypeError));
        assert_eq!(model.py_binary(BinOp::Sub, a, b), Err(Trap::TypeError));
        assert_eq!(model.py_binary(BinOp::Add, one, one).unwrap(), None);

        let a2 = model.new_str("ab").unwrap();
        assert_eq!(model.py_compare(CmpOp::Eq, a, a2).unwrap(), Some(Value::TRUE));
        assert_eq!(model.py_compare(CmpOp::Lt, a, b).unwrap(), Some(Value::TRUE));
        assert_eq!(model.py_compare(CmpOp::Eq, a, one).unwrap(), Some(Value::FALSE));
        assert_eq!(model.py_compare(CmpOp::Lt, a, one), Err(Trap::TypeError));
        assert_eq!(model.py_compare(CmpOp::Eq, one, one).unwrap(), None);

        let empty = model.new_str("").unwrap();
        assert_eq!(model.py_truthy(a).unwrap(), Some(true));
        assert_eq!(model.py_truthy(empty).unwrap(), Some(false));
        assert_eq!(model.py_truthy(one).unwrap(), Some(true));
    }

    #[test]
    fn str_getitem_indexes_by_code_point() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("abc").unwrap();
        for (i, expect) in [(0, "a"), (2, "c"), (-1, "c"), (-3, "a")] {
            let r = model.py_getitem(s, Value::fixnum(i).unwrap()).unwrap();
            assert_eq!(model.str_value(r), Some(expect));
        }
        let r = model.py_getitem(s, Value::TRUE).unwrap();
        assert_eq!(model.str_value(r), Some("b"));
        assert_eq!(model.py_getitem(s, Value::fixnum(3).unwrap()), Err(Trap::IndexError));
        assert_eq!(model.py_getitem(s, Value::fixnum(-4).unwrap()), Err(Trap::IndexError));
        assert_eq!(model.py_getitem(s, s), Err(Trap::TypeError));
        let five = Value::fixnum(5).unwrap();
        assert_eq!(model.py_getitem(five, Value::fixnum(0).unwrap()), Err(Trap::TypeError));
        let cafe = model.new_str("café").unwrap();
        let at3 = model.py_getitem(cafe, Value::fixnum(3).unwrap()).unwrap();
        assert_eq!(model.str_value(at3), Some("é"));
        let neg1 = model.py_getitem(cafe, Value::fixnum(-1).unwrap()).unwrap();
        assert_eq!(model.str_value(neg1), Some("é"));
    }

    #[test]
    fn str_methods_startswith_endswith_find() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("héllo wörld").unwrap();
        let he = model.new_str("hé").unwrap();
        let ld = model.new_str("ld").unwrap();
        let wo = model.new_str("wö").unwrap();
        let zz = model.new_str("zz").unwrap();

        let sw = model.getattr(s, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(sw, &[he]).unwrap(), Value::TRUE);
        let sw2 = model.getattr(s, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(sw2, &[ld]).unwrap(), Value::FALSE);
        let ew = model.getattr(s, "endswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(ew, &[ld]).unwrap(), Value::TRUE);

        let f1 = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f1, &[wo]).unwrap().as_fixnum(), Some(6));
        let f2 = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f2, &[zz]).unwrap().as_fixnum(), Some(-1));

        let f3 = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(
            model.call_bound_method(f3, &[Value::fixnum(1).unwrap()]),
            Err(Trap::TypeError)
        );
        let sw3 = model.getattr(s, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(sw3, &[]), Err(Trap::TypeError));
    }

    #[test]
    fn str_methods_with_start_end_bounds() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("hello world").unwrap();
        let o = model.new_str("o").unwrap();
        let lo = model.new_str("lo").unwrap();
        let wor = model.new_str("wor").unwrap();
        let n = |v: i32| Value::fixnum(v).unwrap();

        let f = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f, &[o, n(5)]).unwrap().as_fixnum(), Some(7));
        let f = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f, &[wor, n(0), n(5)]).unwrap().as_fixnum(), Some(-1));
        let f = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f, &[o, n(-3)]).unwrap().as_fixnum(), Some(-1));

        let sw = model.getattr(s, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(sw, &[wor, n(6)]).unwrap(), Value::TRUE);
        let ew = model.getattr(s, "endswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(ew, &[lo, n(0), n(5)]).unwrap(), Value::TRUE);

        let f = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f, &[o, lo]), Err(Trap::TypeError));
        let f = model.getattr(s, "find", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(f, &[o, n(0), n(1), n(2)]), Err(Trap::TypeError));
    }

    #[test]
    fn str_methods_strip_replace_count() {
        let mut model = ObjectModel::new(Vec::new(), 4096);

        let s = model.new_str("  hi  ").unwrap();
        let bm = model.getattr(s, "strip", &mut InlineCache::empty()).unwrap();
        let r = model.call_bound_method(bm, &[]).unwrap();
        assert_eq!(model.str_value(r), Some("hi"));

        let url = model.new_str("www.example.com").unwrap();
        let set = model.new_str("cmowz.").unwrap();
        let bm = model.getattr(url, "strip", &mut InlineCache::empty()).unwrap();
        let r = model.call_bound_method(bm, &[set]).unwrap();
        assert_eq!(model.str_value(r), Some("example"));

        let spam = model.new_str("spam, spam, spam").unwrap();
        let old = model.new_str("spam").unwrap();
        let new = model.new_str("eggs").unwrap();
        let bm = model.getattr(spam, "replace", &mut InlineCache::empty()).unwrap();
        let r = model.call_bound_method(bm, &[old, new]).unwrap();
        assert_eq!(model.str_value(r), Some("eggs, eggs, eggs"));
        let bm = model.getattr(spam, "replace", &mut InlineCache::empty()).unwrap();
        let r = model.call_bound_method(bm, &[old, new, Value::fixnum(1).unwrap()]).unwrap();
        assert_eq!(model.str_value(r), Some("eggs, spam, spam"));

        let bm = model.getattr(spam, "count", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(bm, &[old]).unwrap().as_fixnum(), Some(3));
        let bm = model.getattr(spam, "count", &mut InlineCache::empty()).unwrap();
        let five = Value::fixnum(5).unwrap();
        assert_eq!(model.call_bound_method(bm, &[old, five]).unwrap().as_fixnum(), Some(2));
    }

    #[test]
    fn str_methods_predicates() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let cases: &[(&str, &str, bool)] = &[
            ("0123", "isdigit", true),
            ("12a", "isdigit", false),
            ("", "isdigit", false),
            ("abcDEF", "isalpha", true),
            ("abc1", "isalpha", false),
            ("abc123", "isalnum", true),
            ("a b", "isalnum", false),
            ("  \t\n", "isspace", true),
            (" a ", "isspace", false),
            ("BANANA", "isupper", true),
            ("BANANA1", "isupper", true),
            ("Banana", "isupper", false),
            ("123", "isupper", false),
            ("banana", "islower", true),
            ("baNana", "islower", false),
        ];
        for &(text, method, expected) in cases {
            let s = model.new_str(text).unwrap();
            let bm = model.getattr(s, method, &mut InlineCache::empty()).unwrap();
            let got = model.call_bound_method(bm, &[]).unwrap();
            assert_eq!(got, Value::from_bool(expected), "{text:?}.{method}()");
        }
    }

    #[test]
    fn str_slicing() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let s = model.new_str("hello").unwrap();
        let n = |v: i32| Value::fixnum(v).unwrap();
        let slice = |m: &mut ObjectModel, a: Value, b: Value, st: Value| {
            let sl = m.new_slice(a, b, st).unwrap();
            m.py_getitem(s, sl)
        };
        let r = slice(&mut model, n(1), n(4), Value::NONE).unwrap();
        assert_eq!(model.str_value(r), Some("ell"));
        let r = slice(&mut model, Value::NONE, Value::NONE, Value::NONE).unwrap();
        assert_eq!(model.str_value(r), Some("hello"));
        let r = slice(&mut model, Value::NONE, Value::NONE, n(-1)).unwrap();
        assert_eq!(model.str_value(r), Some("olleh"));
        let r = slice(&mut model, n(-3), n(-1), Value::NONE).unwrap();
        assert_eq!(model.str_value(r), Some("ll"));
        let r = slice(&mut model, Value::NONE, Value::NONE, n(2)).unwrap();
        assert_eq!(model.str_value(r), Some("hlo"));
        let r = slice(&mut model, n(2), n(99), Value::NONE).unwrap();
        assert_eq!(model.str_value(r), Some("llo"));
        let r = slice(&mut model, n(4), n(1), Value::NONE).unwrap();
        assert_eq!(model.str_value(r), Some(""));
        assert_eq!(slice(&mut model, Value::NONE, Value::NONE, n(0)), Err(Trap::ValueError));
        assert_eq!(slice(&mut model, s, Value::NONE, Value::NONE), Err(Trap::TypeError));
        assert_eq!(model.py_getitem(s, n(99)), Err(Trap::IndexError));
    }

    #[test]
    fn list_tuple_dict_basics() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();

        let list = model.new_list(alloc::vec![n(10), n(20), n(30)]).unwrap();
        assert!(model.is_list(list));
        assert_eq!(model.py_len(list).unwrap().as_fixnum(), Some(3));
        assert_eq!(model.py_getitem(list, n(0)).unwrap().as_fixnum(), Some(10));
        assert_eq!(model.py_getitem(list, n(-1)).unwrap().as_fixnum(), Some(30));
        assert_eq!(model.py_getitem(list, n(5)), Err(Trap::IndexError));
        model.py_setitem(list, n(1), n(99)).unwrap();
        assert_eq!(model.py_getitem(list, n(1)).unwrap().as_fixnum(), Some(99));
        assert!(model.py_contains(list, n(99)).unwrap());
        assert!(!model.py_contains(list, n(7)).unwrap());

        let tup = model.new_tuple(alloc::vec![n(1), n(2)]).unwrap();
        assert!(model.is_tuple(tup));
        assert_eq!(model.py_getitem(tup, n(1)).unwrap().as_fixnum(), Some(2));
        assert_eq!(model.py_setitem(tup, n(0), n(5)), Err(Trap::TypeError));

        let dict = model.new_dict(alloc::vec![(n(1), n(10)), (n(2), n(20))]).unwrap();
        assert!(model.is_dict(dict));
        assert_eq!(model.py_len(dict).unwrap().as_fixnum(), Some(2));
        assert_eq!(model.py_getitem(dict, n(1)).unwrap().as_fixnum(), Some(10));
        assert_eq!(model.py_getitem(dict, n(9)), Err(Trap::KeyError));
        assert!(model.py_contains(dict, n(2)).unwrap());
        model.py_setitem(dict, n(3), n(30)).unwrap();
        assert_eq!(model.py_getitem(dict, n(3)).unwrap().as_fixnum(), Some(30));
        model.py_setitem(dict, n(1), n(11)).unwrap();
        assert_eq!(model.py_getitem(dict, n(1)).unwrap().as_fixnum(), Some(11));
        let dup = model.new_dict(alloc::vec![(n(1), n(1)), (n(1), n(2))]).unwrap();
        assert_eq!(model.py_len(dup).unwrap().as_fixnum(), Some(1));
        assert_eq!(model.py_getitem(dup, n(1)).unwrap().as_fixnum(), Some(2));
    }

    #[test]
    fn slice_assignment_matches_python() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let n = |i: i32| Value::fixnum(i).unwrap();
        fn run(model: &mut ObjectModel, base: &[i32], s: Value, e: Value, k: Value, rhs: Vec<Value>) -> Result<String, Trap> {
            let list = model.new_list(base.iter().map(|&i| Value::fixnum(i).unwrap()).collect())?;
            let slice = model.new_slice(s, e, k)?;
            model.seq_setitem_slice(list, slice, rhs)?;
            Ok(model.repr(list))
        }
        let five = [1, 2, 3, 4, 5];
        let none = Value::NONE;
        assert_eq!(run(&mut model, &five, n(1), n(3), none, alloc::vec![n(10), n(20), n(30)]).unwrap(), "[1, 10, 20, 30, 4, 5]");
        assert_eq!(run(&mut model, &five, n(1), n(4), none, alloc::vec![n(99)]).unwrap(), "[1, 99, 5]");
        assert_eq!(run(&mut model, &five, n(1), n(1), none, alloc::vec![n(10), n(20)]).unwrap(), "[1, 10, 20, 2, 3, 4, 5]");
        assert_eq!(run(&mut model, &five, n(1), n(3), none, alloc::vec![]).unwrap(), "[1, 4, 5]");
        assert_eq!(run(&mut model, &five, n(-2), none, none, alloc::vec![n(99)]).unwrap(), "[1, 2, 3, 99]");
        assert_eq!(run(&mut model, &five, n(3), n(1), none, alloc::vec![n(88)]).unwrap(), "[1, 2, 3, 88, 4, 5]");
        assert_eq!(run(&mut model, &five, none, none, none, alloc::vec![n(7), n(8)]).unwrap(), "[7, 8]");
        assert_eq!(run(&mut model, &five, none, none, n(2), alloc::vec![n(10), n(20), n(30)]).unwrap(), "[10, 2, 20, 4, 30]");
        assert_eq!(run(&mut model, &[1, 2, 3], none, none, n(-1), alloc::vec![n(10), n(20), n(30)]).unwrap(), "[30, 20, 10]");
        assert!(matches!(run(&mut model, &five, none, none, n(2), alloc::vec![n(1), n(2)]), Err(Trap::ValueError)));
    }

    #[test]
    fn cell_boxes_and_shares_a_value() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let cell = model.new_cell(Value::fixnum(5).unwrap()).unwrap();
        assert!(model.is_cell(cell));
        assert!(!model.is_cell(Value::fixnum(5).unwrap()));
        assert_eq!(model.cell_get(cell).unwrap().as_fixnum(), Some(5));
        model.cell_set(cell, Value::fixnum(10).unwrap()).unwrap();
        assert_eq!(model.cell_get(cell).unwrap().as_fixnum(), Some(10));
        let s = model.new_str("captured").unwrap();
        let boxed = model.new_cell(s).unwrap();
        assert_eq!(model.str_value(model.cell_get(boxed).unwrap()), Some("captured"));
        assert!(matches!(model.cell_get(Value::NONE), Err(Trap::TypeError)));
    }

    #[test]
    fn del_item_matches_python() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let n = |i: i32| Value::fixnum(i).unwrap();
        let none = Value::NONE;
        fn del_index(model: &mut ObjectModel, base: &[i32], index: Value) -> Result<String, Trap> {
            let list = model.new_list(base.iter().map(|&i| Value::fixnum(i).unwrap()).collect())?;
            model.py_delitem(list, index)?;
            Ok(model.repr(list))
        }
        fn del_slice(model: &mut ObjectModel, base: &[i32], s: Value, e: Value, k: Value) -> Result<String, Trap> {
            let list = model.new_list(base.iter().map(|&i| Value::fixnum(i).unwrap()).collect())?;
            let slice = model.new_slice(s, e, k)?;
            model.py_delitem(list, slice)?;
            Ok(model.repr(list))
        }
        let five = [1, 2, 3, 4, 5];
        assert_eq!(del_index(&mut model, &five, n(1)).unwrap(), "[1, 3, 4, 5]");
        assert_eq!(del_index(&mut model, &five, n(-1)).unwrap(), "[1, 2, 3, 4]");
        assert!(matches!(del_index(&mut model, &five, n(10)), Err(Trap::IndexError)));
        assert_eq!(del_slice(&mut model, &five, n(1), n(3), none).unwrap(), "[1, 4, 5]");
        assert_eq!(del_slice(&mut model, &five, none, none, n(2)).unwrap(), "[2, 4]");
        assert_eq!(del_slice(&mut model, &five, n(1), none, n(2)).unwrap(), "[1, 3, 5]");
        let d = model.new_dict(alloc::vec![(n(1), n(10)), (n(2), n(20))]).unwrap();
        model.py_delitem(d, n(1)).unwrap();
        assert_eq!(model.repr(d), "{2: 20}");
        assert!(matches!(model.py_delitem(d, n(9)), Err(Trap::KeyError)));
    }

    #[test]
    fn iteration_over_containers() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let list = model.new_list(alloc::vec![n(10), n(20)]).unwrap();
        let it = model.new_iter(list).unwrap();
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(10));
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(20));
        assert_eq!(model.py_next(it).unwrap(), None);
        assert_eq!(model.py_next(it).unwrap(), None);
        let s = model.new_str("hi").unwrap();
        let it = model.new_iter(s).unwrap();
        let c0 = model.py_next(it).unwrap().unwrap();
        assert_eq!(model.str_value(c0), Some("h"));
        let d = model.new_dict(alloc::vec![(n(5), n(50)), (n(6), n(60))]).unwrap();
        let it = model.new_iter(d).unwrap();
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(5));
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(6));
        assert_eq!(model.py_next(it).unwrap(), None);
        assert_eq!(model.new_iter(n(3)), Err(Trap::TypeError));
    }

    #[test]
    fn str_split_and_tuple_affixes() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let s = model.new_str("a,b,c").unwrap();
        let comma = model.new_str(",").unwrap();
        let m = model.getattr(s, "split", &mut InlineCache::empty()).unwrap();
        let parts = model.call_bound_method(m, &[comma]).unwrap();
        assert!(model.is_list(parts));
        assert_eq!(model.py_len(parts).unwrap().as_fixnum(), Some(3));
        let abc = model.new_str("abc").unwrap();
        let empty = model.new_str("").unwrap();
        let m = model.getattr(abc, "split", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(m, &[empty]), Err(Trap::ValueError));
        let hello = model.new_str("hello").unwrap();
        let he = model.new_str("he").unwrap();
        let xy = model.new_str("xy").unwrap();
        let affixes = model.new_tuple(alloc::vec![xy, he]).unwrap();
        let m = model.getattr(hello, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(m, &[affixes]).unwrap(), Value::TRUE);
        let list_affix = model.new_list(alloc::vec![he]).unwrap();
        let m = model.getattr(hello, "startswith", &mut InlineCache::empty()).unwrap();
        assert_eq!(model.call_bound_method(m, &[list_affix]), Err(Trap::TypeError));
    }

    #[test]
    fn py_str_renders_like_cpython() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n123 = model.py_str(Value::fixnum(123).unwrap()).unwrap();
        assert_eq!(model.str_value(n123), Some("123"));
        let t = model.py_str(Value::TRUE).unwrap();
        assert_eq!(model.str_value(t), Some("True"));
        let none = model.py_str(Value::NONE).unwrap();
        assert_eq!(model.str_value(none), Some("None"));
        let hello = model.new_str("hello").unwrap();
        assert_eq!(model.py_str(hello).unwrap(), hello);
        let list = model
            .new_list(alloc::vec![Value::fixnum(1).unwrap(), Value::fixnum(2).unwrap()])
            .unwrap();
        let rendered = model.py_str(list).unwrap();
        assert_eq!(model.str_value(rendered), Some("[1, 2]"));
    }

    #[test]
    fn classes_substrate() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let name_a = model.new_str("A").unwrap();
        let key_m = model.new_str("m").unwrap();
        let key_k = model.new_str("k").unwrap();
        let ns_a = model
            .new_dict(alloc::vec![(key_m, Value::function_ref(0)), (key_k, n(10))])
            .unwrap();
        let class_a = model.new_class(name_a, Value::NONE, ns_a).unwrap();
        assert!(model.is_class(class_a));
        let obj = model.new_object(class_a).unwrap();
        assert!(model.is_instance(obj));

        assert_eq!(model.py_getattr_instance(obj, "k").unwrap().as_fixnum(), Some(10));
        let bound = model.py_getattr_instance(obj, "m").unwrap();
        assert!(model.is_py_bound(bound));
        assert_eq!(model.bound_self(bound), obj);
        assert_eq!(model.bound_func(bound).as_function_index(), Some(0));
        assert_eq!(model.py_getattr_instance(obj, "nope"), Err(Trap::AttributeError));

        model.py_setattr_instance(obj, "x", n(42)).unwrap();
        assert_eq!(model.py_getattr_instance(obj, "x").unwrap().as_fixnum(), Some(42));
        model.py_setattr_instance(obj, "k", n(99)).unwrap();
        assert_eq!(model.py_getattr_instance(obj, "k").unwrap().as_fixnum(), Some(99));
        assert!(model.find_init(class_a).is_none());

        let name_b = model.new_str("B").unwrap();
        let key_init = model.new_str("__init__").unwrap();
        let ns_b = model
            .new_dict(alloc::vec![(key_init, Value::function_ref(1))])
            .unwrap();
        let class_b = model.new_class(name_b, class_a, ns_b).unwrap();
        let obj_b = model.new_object(class_b).unwrap();
        let bound_b = model.py_getattr_instance(obj_b, "m").unwrap();
        assert!(model.is_py_bound(bound_b));
        assert_eq!(model.bound_func(bound_b).as_function_index(), Some(0));
        assert_eq!(model.py_getattr_instance(obj_b, "k").unwrap().as_fixnum(), Some(10));
        assert_eq!(model.find_init(class_b).unwrap().as_function_index(), Some(1));
    }

    #[test]
    fn str_predicates_match_cpython_at_the_unicode_edges() {
        assert!(str_predicate(STR_ISALPHA, "café"));
        assert!(!str_predicate(STR_ISALPHA, "a1"));
        assert!(!str_predicate(STR_ISALPHA, "\u{0345}"));
        assert!(str_predicate(STR_ISDIGIT, "\u{00b2}"));
        assert!(!str_predicate(STR_ISDECIMAL, "\u{00b2}"));
        assert!(str_predicate(STR_ISDECIMAL, "123"));
        assert!(str_predicate(STR_ISNUMERIC, "\u{00bd}"));
        assert!(!str_predicate(STR_ISDIGIT, "\u{00bd}"));
        assert!(str_predicate(STR_ISNUMERIC, "\u{4e00}"));
        assert!(str_predicate(STR_ISSPACE, "\u{001c}"));
        assert!(!str_predicate(STR_ISSPACE, ""));
        assert!(str_predicate(STR_ISUPPER, "ABC"));
        assert!(!str_predicate(STR_ISUPPER, "A\u{01c5}"));
        assert!(str_predicate(STR_ISLOWER, "abc"));
        assert!(!str_predicate(STR_ISLOWER, "a\u{01c5}"));
    }

    #[test]
    fn sequence_slicing_and_join() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let list = model.new_list(alloc::vec![n(1), n(2), n(3), n(4), n(5)]).unwrap();
        let sl = model.new_slice(n(1), n(4), Value::NONE).unwrap();
        let r = model.py_getitem(list, sl).unwrap();
        assert!(model.is_list(r));
        assert_eq!(model.py_len(r).unwrap().as_fixnum(), Some(3));
        assert_eq!(model.py_getitem(r, n(0)).unwrap().as_fixnum(), Some(2));
        let tup = model.new_tuple(alloc::vec![n(7), n(8), n(9)]).unwrap();
        let sl2 = model.new_slice(n(1), Value::NONE, Value::NONE).unwrap();
        let rt = model.py_getitem(tup, sl2).unwrap();
        assert!(model.is_tuple(rt));
        assert_eq!(model.py_len(rt).unwrap().as_fixnum(), Some(2));
        let sep = model.new_str(", ").unwrap();
        let a = model.new_str("a").unwrap();
        let b = model.new_str("b").unwrap();
        let items = model.new_list(alloc::vec![a, b]).unwrap();
        let join = model.getattr(sep, "join", &mut InlineCache::empty()).unwrap();
        let joined = model.call_bound_method(join, &[items]).unwrap();
        assert_eq!(model.str_value(joined), Some("a, b"));
    }

    #[test]
    fn exception_hierarchy_isinstance_and_trap_mapping() {
        let mut model = ObjectModel::new(Vec::new(), 64 * 1024);
        let index_error = model.exception_class("IndexError").unwrap();
        let lookup_error = model.exception_class("LookupError").unwrap();
        let exception = model.exception_class("Exception").unwrap();
        let base = model.exception_class("BaseException").unwrap();
        let value_error = model.exception_class("ValueError").unwrap();
        assert!(model.is_class(index_error));

        let exc = model.new_object(index_error).unwrap();
        assert!(model.exception_isinstance(exc, index_error));
        assert!(model.exception_isinstance(exc, lookup_error));
        assert!(model.exception_isinstance(exc, exception));
        assert!(model.exception_isinstance(exc, base));
        assert!(!model.exception_isinstance(exc, value_error));

        let from_trap = model.trap_to_exception(Trap::KeyError).unwrap();
        let key_error = model.exception_class("KeyError").unwrap();
        assert!(model.exception_isinstance(from_trap, key_error));
        assert!(model.exception_isinstance(from_trap, lookup_error));
        assert!(model.trap_to_exception(Trap::Malformed).is_none());

        let raised = model.raise_value(value_error).unwrap();
        assert!(model.exception_isinstance(raised, value_error));
        assert_eq!(model.raise_value(Value::fixnum(5).unwrap()), Err(Trap::TypeError));
    }

    #[test]
    fn list_and_dict_methods() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let empty_cache = || InlineCache::empty();

        let list = model.new_list(alloc::vec![n(1), n(2)]).unwrap();
        let append = model.getattr(list, "append", &mut empty_cache()).unwrap();
        model.call_bound_method(append, &[n(3)]).unwrap();
        assert_eq!(model.py_len(list).unwrap().as_fixnum(), Some(3));
        let pop = model.getattr(list, "pop", &mut empty_cache()).unwrap();
        assert_eq!(model.call_bound_method(pop, &[]).unwrap().as_fixnum(), Some(3));
        assert_eq!(model.py_len(list).unwrap().as_fixnum(), Some(2));
        let empty = model.new_list(Vec::new()).unwrap();
        let pop_e = model.getattr(empty, "pop", &mut empty_cache()).unwrap();
        assert_eq!(model.call_bound_method(pop_e, &[]), Err(Trap::IndexError));

        let dict = model.new_dict(alloc::vec![(n(1), n(10))]).unwrap();
        let get = model.getattr(dict, "get", &mut empty_cache()).unwrap();
        assert_eq!(model.call_bound_method(get, &[n(1)]).unwrap().as_fixnum(), Some(10));
        let get2 = model.getattr(dict, "get", &mut empty_cache()).unwrap();
        assert_eq!(model.call_bound_method(get2, &[n(9), n(99)]).unwrap().as_fixnum(), Some(99));
        let keys = model.getattr(dict, "keys", &mut empty_cache()).unwrap();
        let key_list = model.call_bound_method(keys, &[]).unwrap();
        assert!(model.is_list(key_list));
        assert_eq!(model.py_len(key_list).unwrap().as_fixnum(), Some(1));

        assert_eq!(
            model.getattr(list, "nope", &mut empty_cache()),
            Err(Trap::AttributeError)
        );
    }

    #[test]
    fn range_object() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let r = model.new_range(2, 10, 2).unwrap();
        assert!(model.is_range(r));
        assert_eq!(model.py_len(r).unwrap().as_fixnum(), Some(4));
        assert_eq!(model.py_getitem(r, n(0)).unwrap().as_fixnum(), Some(2));
        assert_eq!(model.py_getitem(r, n(3)).unwrap().as_fixnum(), Some(8));
        assert_eq!(model.py_getitem(r, n(-1)).unwrap().as_fixnum(), Some(8));
        assert_eq!(model.py_getitem(r, n(4)), Err(Trap::IndexError));
        let it = model.new_iter(r).unwrap();
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(2));
        assert_eq!(model.py_next(it).unwrap().and_then(|v| v.as_fixnum()), Some(4));
        let empty = model.new_range(5, 5, 1).unwrap();
        assert_eq!(model.py_len(empty).unwrap().as_fixnum(), Some(0));
        assert_eq!(model.py_truthy(empty).unwrap(), Some(false));
        assert_eq!(model.py_truthy(r).unwrap(), Some(true));
    }

    #[test]
    fn set_object() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let s = model.new_set(alloc::vec![n(1), n(2), n(2), n(3), n(1)]).unwrap();
        assert!(model.is_set(s));
        assert_eq!(model.py_len(s).unwrap().as_fixnum(), Some(3));
        assert_eq!(model.repr(s), "{1, 2, 3}");
        assert!(model.py_contains(s, n(2)).unwrap());
        assert!(!model.py_contains(s, n(5)).unwrap());
        assert_eq!(model.py_truthy(s).unwrap(), Some(true));
        model.set_add(s, n(2)).unwrap();
        model.set_add(s, n(4)).unwrap();
        assert_eq!(model.py_len(s).unwrap().as_fixnum(), Some(4));
        let empty = model.new_set(Vec::new()).unwrap();
        assert_eq!(model.repr(empty), "set()");
        assert_eq!(model.py_truthy(empty).unwrap(), Some(false));
    }

    #[test]
    fn frozenset_object() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let fs = model.new_frozenset(alloc::vec![n(1), n(2), n(2), n(3)]).unwrap();
        assert!(model.is_frozenset(fs));
        assert!(!model.is_set(fs));
        assert_eq!(model.py_len(fs).unwrap().as_fixnum(), Some(3));
        assert_eq!(model.repr(fs), "frozenset({1, 2, 3})");
        assert!(model.py_contains(fs, n(2)).unwrap());
        assert!(!model.py_contains(fs, n(7)).unwrap());
        assert_eq!(model.py_truthy(fs).unwrap(), Some(true));
        assert_eq!(model.set_add(fs, n(9)), Err(Trap::TypeError));
        let empty = model.new_frozenset(Vec::new()).unwrap();
        assert_eq!(model.repr(empty), "frozenset()");
        assert_eq!(model.py_truthy(empty).unwrap(), Some(false));
    }

    #[test]
    fn set_algebra() {
        let mut model = ObjectModel::new(Vec::new(), 32 * 1024);
        let n = |v: i32| Value::fixnum(v).unwrap();
        let a = model.new_set(alloc::vec![n(1), n(2), n(3)]).unwrap();
        let b = model.new_set(alloc::vec![n(3), n(4), n(5)]).unwrap();
        let union = model.set_binary_op(BinOp::BitOr, a, b).unwrap();
        assert_eq!(model.py_len(union).unwrap().as_fixnum(), Some(5));
        let inter = model.set_binary_op(BinOp::BitAnd, a, b).unwrap();
        assert_eq!(model.py_len(inter).unwrap().as_fixnum(), Some(1));
        assert!(model.py_contains(inter, n(3)).unwrap());
        let diff = model.set_binary_op(BinOp::Sub, a, b).unwrap();
        assert_eq!(model.py_len(diff).unwrap().as_fixnum(), Some(2));
        let symdiff = model.set_binary_op(BinOp::BitXor, a, b).unwrap();
        assert_eq!(model.py_len(symdiff).unwrap().as_fixnum(), Some(4));
        let a2 = model.new_set(alloc::vec![n(3), n(2), n(1)]).unwrap();
        assert_eq!(model.set_compare(CmpOp::Eq, a, a2).unwrap(), Value::TRUE);
        let one = model.new_set(alloc::vec![n(1)]).unwrap();
        assert_eq!(model.set_compare(CmpOp::Lt, one, a).unwrap(), Value::TRUE);
        assert_eq!(model.set_compare(CmpOp::Lt, a, a2).unwrap(), Value::FALSE);
        let list = model.new_list(alloc::vec![n(1), n(2), n(3)]).unwrap();
        assert_eq!(model.set_compare(CmpOp::Eq, a, list).unwrap(), Value::FALSE);
        let fa = model.new_frozenset(alloc::vec![n(1)]).unwrap();
        let fu = model.set_binary_op(BinOp::BitOr, fa, b).unwrap();
        assert!(model.is_frozenset(fu));
        assert_eq!(model.set_binary_op(BinOp::Add, a, b), Err(Trap::TypeError));
        assert_eq!(model.set_compare(CmpOp::Lt, a, list), Err(Trap::TypeError));
    }

    #[test]
    fn super_resolves_base_method() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let m_key = model.new_str("m").unwrap();
        let base_name = model.new_str("Base").unwrap();
        let base_ns = model.new_dict(alloc::vec![(m_key, Value::function_ref(0))]).unwrap();
        let base = model.new_class(base_name, Value::NONE, base_ns).unwrap();
        let der_name = model.new_str("Derived").unwrap();
        let der_ns = model.new_dict(alloc::vec![(m_key, Value::function_ref(1))]).unwrap();
        let derived = model.new_class(der_name, base, der_ns).unwrap();
        let instance = model.new_object(derived).unwrap();
        let sup = model.new_super(derived, instance).unwrap();
        assert!(model.is_super(sup));
        let bound = model.py_getattr_super(sup, "m").unwrap();
        assert!(model.is_py_bound(bound));
        assert_eq!(model.bound_func(bound).as_function_index(), Some(0));
        assert_eq!(model.bound_self(bound), instance);
    }

    #[test]
    fn find_dunder_resolves_class_methods() {
        let mut model = ObjectModel::new(Vec::new(), 16 * 1024);
        let name = model.new_str("C").unwrap();
        let key = model.new_str("__len__").unwrap();
        let ns = model
            .new_dict(alloc::vec![(key, Value::function_ref(0))])
            .unwrap();
        let class = model.new_class(name, Value::NONE, ns).unwrap();
        let obj = model.new_object(class).unwrap();
        let bound = model.find_dunder(obj, "__len__").unwrap();
        assert!(model.is_py_bound(bound));
        assert_eq!(model.bound_self(bound), obj);
        assert_eq!(model.bound_func(bound).as_function_index(), Some(0));
        assert!(model.find_dunder(obj, "__str__").is_none());
        assert!(model.find_dunder(Value::fixnum(5).unwrap(), "__len__").is_none());
    }

    #[test]
    fn getattr_reads_the_right_slot() {
        let (mut model, obj) = point_model();
        let mut cx = InlineCache::empty();
        let mut cy = InlineCache::empty();
        assert_eq!(model.getattr(obj, "x", &mut cx).unwrap().as_fixnum(), Some(7));
        assert_eq!(model.getattr(obj, "y", &mut cy).unwrap().as_fixnum(), Some(9));
    }

    #[test]
    fn inline_cache_misses_then_hits() {
        let (mut model, obj) = point_model();
        let mut cache = InlineCache::empty();
        assert_eq!(cache.lookup(0), None);
        assert_eq!(model.getattr(obj, "x", &mut cache).unwrap().as_fixnum(), Some(7));
        assert_eq!(cache.lookup(0), Some(0));
        assert_eq!(model.getattr(obj, "x", &mut cache).unwrap().as_fixnum(), Some(7));
    }

    #[test]
    fn unknown_attribute_is_attribute_error() {
        let (mut model, obj) = point_model();
        assert_eq!(
            model.getattr(obj, "z", &mut InlineCache::empty()),
            Err(Trap::AttributeError)
        );
    }

    #[test]
    fn attribute_access_on_a_non_object_is_attribute_error() {
        let (mut model, _obj) = point_model();
        assert_eq!(
            model.getattr(Value::fixnum(1).unwrap(), "x", &mut InlineCache::empty()),
            Err(Trap::AttributeError)
        );
        assert_eq!(
            model.getattr(Value::NONE, "x", &mut InlineCache::empty()),
            Err(Trap::AttributeError)
        );
    }

    #[test]
    fn two_instances_share_one_filled_cache() {
        let mut model = ObjectModel::new(alloc::vec![PyType::with_slots("Point", &["x", "y"])], 4096);
        let a = model.new_instance(0, &[Value::fixnum(1).unwrap(), Value::fixnum(2).unwrap()]).unwrap();
        let b = model.new_instance(0, &[Value::fixnum(3).unwrap(), Value::fixnum(4).unwrap()]).unwrap();
        let mut cache = InlineCache::empty();
        assert_eq!(model.getattr(a, "x", &mut cache).unwrap().as_fixnum(), Some(1));
        assert_eq!(cache.lookup(0), Some(0));
        assert_eq!(model.getattr(b, "x", &mut cache).unwrap().as_fixnum(), Some(3));
    }

    #[test]
    fn compare_ordered_covers_ints_strs_and_tuples() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let i = |n: i32| Value::fixnum(n).unwrap();
        assert_eq!(model.compare_ordered(i(1), i(2)), Ok(Ordering::Less));
        assert_eq!(model.compare_ordered(i(5), i(5)), Ok(Ordering::Equal));
        assert_eq!(model.compare_ordered(Value::TRUE, i(0)), Ok(Ordering::Greater));
        let a = model.new_str("apple").unwrap();
        let b = model.new_str("banana").unwrap();
        assert_eq!(model.compare_ordered(a, b), Ok(Ordering::Less));
        let t1 = model.new_tuple(alloc::vec![i(1), i(2)]).unwrap();
        let t2 = model.new_tuple(alloc::vec![i(1), i(3)]).unwrap();
        let t3 = model.new_tuple(alloc::vec![i(1)]).unwrap();
        assert_eq!(model.compare_ordered(t1, t2), Ok(Ordering::Less));
        assert_eq!(model.compare_ordered(t3, t1), Ok(Ordering::Less));
        assert_eq!(model.compare_ordered(i(1), a), Err(Trap::TypeError));
        assert_eq!(model.compare_ordered(t1, a), Err(Trap::TypeError));
    }

    #[test]
    fn py_binary_concatenates_and_repeats_strs_and_sequences() {
        let mut model = ObjectModel::new(Vec::new(), 4096);
        let i = |n: i32| Value::fixnum(n).unwrap();
        let ab = model.new_str("ab").unwrap();
        let cat = model.py_binary(BinOp::Add, ab, ab).unwrap().unwrap();
        assert_eq!(model.str_value(cat), Some("abab"));
        let rep = model.py_binary(BinOp::Mul, ab, i(3)).unwrap().unwrap();
        assert_eq!(model.str_value(rep), Some("ababab"));
        let rep_rev = model.py_binary(BinOp::Mul, i(2), ab).unwrap().unwrap();
        assert_eq!(model.str_value(rep_rev), Some("abab"));
        let l1 = model.new_list(alloc::vec![i(1), i(2)]).unwrap();
        let l2 = model.new_list(alloc::vec![i(3)]).unwrap();
        let lcat = model.py_binary(BinOp::Add, l1, l2).unwrap().unwrap();
        assert_eq!(model.repr(lcat), "[1, 2, 3]");
        let lrep = model.py_binary(BinOp::Mul, l1, i(2)).unwrap().unwrap();
        assert_eq!(model.repr(lrep), "[1, 2, 1, 2]");
        let t1 = model.new_tuple(alloc::vec![i(1)]).unwrap();
        let t2 = model.new_tuple(alloc::vec![i(2)]).unwrap();
        let tcat = model.py_binary(BinOp::Add, t1, t2).unwrap().unwrap();
        assert_eq!(model.repr(tcat), "(1, 2)");
        assert_eq!(model.py_binary(BinOp::Add, l1, t1), Err(Trap::TypeError));
        let empty = model.py_binary(BinOp::Mul, l1, i(0)).unwrap().unwrap();
        assert_eq!(model.repr(empty), "[]");
        assert_eq!(model.py_binary(BinOp::Add, i(1), i(2)).unwrap(), None);
    }
}
