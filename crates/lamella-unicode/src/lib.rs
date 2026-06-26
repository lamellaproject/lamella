//! Language-neutral Unicode Character Database properties, as compact range tables.
#![no_std]

#[cfg(feature = "normalization")]
extern crate alloc;

use core::cmp::Ordering;

#[allow(dead_code)]
mod tables;

/// The Unicode version these tables are generated from (matches CPython 3.14.6's
/// `unicodedata.unidata_version`).
pub const UNICODE_VERSION: &str = "16.0.0";

/// A Unicode general category, in `System.Globalization.UnicodeCategory` order so the C#
/// BCL can map by the discriminant. [`GeneralCategory::NotAssigned`] (`Cn`) is the value
/// for unassigned and out-of-range code points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum GeneralCategory {
    /// `Lu` -- an uppercase letter.
    UppercaseLetter = 0,
    /// `Ll` -- a lowercase letter.
    LowercaseLetter = 1,
    /// `Lt` -- a digraph encoded as a single titlecase letter.
    TitlecaseLetter = 2,
    /// `Lm` -- a modifier letter.
    ModifierLetter = 3,
    /// `Lo` -- other letters, including ideographs.
    OtherLetter = 4,
    /// `Mn` -- a non-spacing combining mark (zero advance width).
    NonSpacingMark = 5,
    /// `Mc` -- a spacing combining mark (positive advance width).
    SpacingCombiningMark = 6,
    /// `Me` -- an enclosing combining mark.
    EnclosingMark = 7,
    /// `Nd` -- a decimal digit.
    DecimalDigitNumber = 8,
    /// `Nl` -- a letterlike numeric character.
    LetterNumber = 9,
    /// `No` -- a numeric character of other type.
    OtherNumber = 10,
    /// `Zs` -- a space separator.
    SpaceSeparator = 11,
    /// `Zl` -- a line separator (only U+2028).
    LineSeparator = 12,
    /// `Zp` -- a paragraph separator (only U+2029).
    ParagraphSeparator = 13,
    /// `Cc` -- a C0 or C1 control code.
    Control = 14,
    /// `Cf` -- a format control character.
    Format = 15,
    /// `Cs` -- a surrogate code point.
    Surrogate = 16,
    /// `Co` -- a private-use character.
    PrivateUse = 17,
    /// `Pc` -- a connector punctuation.
    ConnectorPunctuation = 18,
    /// `Pd` -- a dash or hyphen punctuation.
    DashPunctuation = 19,
    /// `Ps` -- an opening punctuation.
    OpenPunctuation = 20,
    /// `Pe` -- a closing punctuation.
    ClosePunctuation = 21,
    /// `Pi` -- an initial quote punctuation.
    InitialQuotePunctuation = 22,
    /// `Pf` -- a final quote punctuation.
    FinalQuotePunctuation = 23,
    /// `Po` -- other punctuation.
    OtherPunctuation = 24,
    /// `Sm` -- a math symbol.
    MathSymbol = 25,
    /// `Sc` -- a currency symbol.
    CurrencySymbol = 26,
    /// `Sk` -- a non-letterlike modifier symbol.
    ModifierSymbol = 27,
    /// `So` -- a symbol of other type.
    OtherSymbol = 28,
    /// `Cn` -- an unassigned, reserved, or noncharacter code point (the default).
    NotAssigned = 29,
}

impl GeneralCategory {
    /// The category for a table discriminant `0..=29`; [`GeneralCategory::NotAssigned`] for
    /// any other value (so a malformed table can never produce an invalid enum).
    #[must_use]
    pub const fn from_u8(value: u8) -> GeneralCategory {
        use GeneralCategory::*;
        match value {
            0 => UppercaseLetter,
            1 => LowercaseLetter,
            2 => TitlecaseLetter,
            3 => ModifierLetter,
            4 => OtherLetter,
            5 => NonSpacingMark,
            6 => SpacingCombiningMark,
            7 => EnclosingMark,
            8 => DecimalDigitNumber,
            9 => LetterNumber,
            10 => OtherNumber,
            11 => SpaceSeparator,
            12 => LineSeparator,
            13 => ParagraphSeparator,
            14 => Control,
            15 => Format,
            16 => Surrogate,
            17 => PrivateUse,
            18 => ConnectorPunctuation,
            19 => DashPunctuation,
            20 => OpenPunctuation,
            21 => ClosePunctuation,
            22 => InitialQuotePunctuation,
            23 => FinalQuotePunctuation,
            24 => OtherPunctuation,
            25 => MathSymbol,
            26 => CurrencySymbol,
            27 => ModifierSymbol,
            28 => OtherSymbol,
            _ => NotAssigned,
        }
    }

    /// Whether this is one of the five letter categories (`Lu Ll Lt Lm Lo`).
    #[must_use]
    pub const fn is_letter(self) -> bool {
        matches!(
            self,
            GeneralCategory::UppercaseLetter
                | GeneralCategory::LowercaseLetter
                | GeneralCategory::TitlecaseLetter
                | GeneralCategory::ModifierLetter
                | GeneralCategory::OtherLetter
        )
    }
}

/// The general category of `cp` ([`GeneralCategory::NotAssigned`] when unassigned or out of
/// range).
#[must_use]
pub fn general_category(cp: u32) -> GeneralCategory {
    GeneralCategory::from_u8(lookup_triple(tables::GENERAL_CATEGORY, cp).unwrap_or(29))
}

/// Whether `cp` has the `White_Space` property (the C# lexer's whitespace, and the bulk of
/// Python's `str.isspace`).
#[must_use]
pub fn is_white_space(cp: u32) -> bool {
    in_ranges(tables::WHITE_SPACE, cp)
}

/// Whether `cp` has the `Alphabetic` derived property (Python `str.isalpha` -- a superset of
/// the letter categories, including e.g. `Other_Alphabetic` marks).
#[must_use]
pub fn is_alphabetic(cp: u32) -> bool {
    in_ranges(tables::ALPHABETIC, cp)
}

/// Whether `cp` has the `Uppercase` derived property.
#[must_use]
pub fn is_uppercase(cp: u32) -> bool {
    in_ranges(tables::UPPERCASE, cp)
}

/// Whether `cp` has the `Lowercase` derived property.
#[must_use]
pub fn is_lowercase(cp: u32) -> bool {
    in_ranges(tables::LOWERCASE, cp)
}

/// Whether `cp` has the `Cased` derived property (a character that participates in case --
/// uppercase, lowercase, or titlecase).
#[must_use]
pub fn is_cased(cp: u32) -> bool {
    in_ranges(tables::CASED, cp)
}

/// Whether `cp` has the `XID_Start` derived property (an identifier-start, NFC-closed).
#[must_use]
pub fn is_xid_start(cp: u32) -> bool {
    in_ranges(tables::XID_START, cp)
}

/// Whether `cp` has the `XID_Continue` derived property (an identifier-continuation,
/// NFC-closed).
#[must_use]
pub fn is_xid_continue(cp: u32) -> bool {
    in_ranges(tables::XID_CONTINUE, cp)
}

/// The numeric "strength" of `cp`: `3` = has a `Decimal` value (`Numeric_Type=Decimal`),
/// `2` = has a `Digit` value, `1` = has a `Numeric` value, `0` = non-numeric. The levels
/// nest (Decimal is a Digit is a Numeric), so Python derives `isdecimal` = `>= 3`,
/// `isdigit` = `>= 2`, `isnumeric` = `>= 1`.
#[must_use]
pub fn numeric_level(cp: u32) -> u8 {
    lookup_triple(tables::NUMERIC_LEVEL, cp).unwrap_or(0)
}

/// Whether `cp` falls in one of the sorted, non-overlapping `(start, end)` ranges.
fn in_ranges(table: &[(u32, u32)], cp: u32) -> bool {
    table
        .binary_search_by(|&(lo, hi)| {
            if cp < lo {
                Ordering::Greater
            } else if cp > hi {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .is_ok()
}

/// The payload of the `(start, end, value)` range containing `cp`, or `None`.
fn lookup_triple(table: &[(u32, u32, u8)], cp: u32) -> Option<u8> {
    table
        .binary_search_by(|&(lo, hi, _)| {
            if cp < lo {
                Ordering::Greater
            } else if cp > hi {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        })
        .ok()
        .map(|i| table[i].2)
}

#[cfg(feature = "normalization")]
pub use normalization::*;

/// Unicode normalization (UAX #15): NFC/NFD/NFKC/NFKD over the canonical/compatibility
/// decomposition, combining-class, and composition tables. The `normalization` feature tier --
/// a categories-only consumer drops it (and `alloc`) entirely.
#[cfg(feature = "normalization")]
mod normalization {
    use alloc::string::String;
    use alloc::vec::Vec;

    use super::{lookup_triple, tables};

    /// A Unicode normalization form (UAX #15).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum NormalizationForm {
        /// NFC -- canonical decomposition, then canonical composition.
    Nfc,
    /// NFD -- canonical decomposition.
    Nfd,
    /// NFKC -- compatibility decomposition, then canonical composition.
    Nfkc,
    /// NFKD -- compatibility decomposition.
    Nfkd,
}

const S_BASE: u32 = 0xAC00;
const L_BASE: u32 = 0x1100;
const V_BASE: u32 = 0x1161;
const T_BASE: u32 = 0x11A7;
const L_COUNT: u32 = 19;
const V_COUNT: u32 = 21;
const T_COUNT: u32 = 28;
const N_COUNT: u32 = V_COUNT * T_COUNT;
const S_COUNT: u32 = L_COUNT * N_COUNT;

/// The canonical combining class of `cp` (0 for a starter / non-combining character).
#[must_use]
pub fn combining_class(cp: u32) -> u8 {
    lookup_triple(tables::COMBINING_CLASS, cp).unwrap_or(0)
}

/// The `(is_compatibility, mapping)` one-level decomposition of `cp`, or `None` when `cp` has
/// no decomposition mapping. (Hangul is decomposed algorithmically and also returns `None`.)
#[must_use]
pub fn decomposition(cp: u32) -> Option<(bool, &'static [u32])> {
    let i = tables::DECOMP_INDEX
        .binary_search_by(|&(c, _, _, _)| c.cmp(&cp))
        .ok()?;
    let (_, offset, len, is_compat) = tables::DECOMP_INDEX[i];
    let start = offset as usize;
    Some((is_compat != 0, &tables::DECOMP_DATA[start..start + len as usize]))
}

/// Normalizes `s` to `form` (UAX #15). Allocates the result.
#[must_use]
pub fn normalize(s: &str, form: NormalizationForm) -> String {
    let compat = matches!(form, NormalizationForm::Nfkc | NormalizationForm::Nfkd);
    let mut cps: Vec<u32> = Vec::new();
    for ch in s.chars() {
        decompose_into(ch as u32, compat, &mut cps);
    }
    canonical_order(&mut cps);
    if matches!(form, NormalizationForm::Nfc | NormalizationForm::Nfkc) {
        cps = compose(&cps);
    }
    cps.into_iter().filter_map(char::from_u32).collect()
}

/// Recursively decomposes `cp` (canonical always; compatibility too when `compat`) into `out`.
fn decompose_into(cp: u32, compat: bool, out: &mut Vec<u32>) {
    if (S_BASE..S_BASE + S_COUNT).contains(&cp) {
        let si = cp - S_BASE;
        out.push(L_BASE + si / N_COUNT);
        out.push(V_BASE + (si % N_COUNT) / T_COUNT);
        let ti = si % T_COUNT;
        if ti != 0 {
            out.push(T_BASE + ti);
        }
        return;
    }
    if let Some((is_compat, mapping)) = decomposition(cp) {
        if !is_compat || compat {
            for &c in mapping {
                decompose_into(c, compat, out);
            }
            return;
        }
    }
    out.push(cp);
}

/// Reorders each run of combining marks by canonical combining class (a stable sort).
fn canonical_order(cps: &mut [u32]) {
    let n = cps.len();
    let mut i = 0;
    while i < n {
        if combining_class(cps[i]) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && combining_class(cps[i]) != 0 {
            i += 1;
        }
        for j in (start + 1)..i {
            let mut k = j;
            while k > start && combining_class(cps[k - 1]) > combining_class(cps[k]) {
                cps.swap(k - 1, k);
                k -= 1;
            }
        }
    }
}

/// Canonical composition (UAX #15): folds combining marks back into the preceding starter.
fn compose(cps: &[u32]) -> Vec<u32> {
    let mut out: Vec<u32> = Vec::with_capacity(cps.len());
    let mut starter: Option<usize> = None;
    let mut last_ccc: u8 = 0;
    for &cp in cps {
        let cc = combining_class(cp);
        if let Some(si) = starter {
            if last_ccc == 0 || last_ccc < cc {
                if let Some(composed) = primary_composite(out[si], cp) {
                    out[si] = composed;
                    continue;
                }
            }
        }
        out.push(cp);
        if cc == 0 {
            starter = Some(out.len() - 1);
            last_ccc = 0;
        } else {
            last_ccc = cc;
        }
    }
    out
}

/// The primary composite of starter `a` and following `b` (Hangul algorithmic, then the
/// canonical-composition table), or `None`.
fn primary_composite(a: u32, b: u32) -> Option<u32> {
    if (L_BASE..L_BASE + L_COUNT).contains(&a) && (V_BASE..V_BASE + V_COUNT).contains(&b) {
        return Some(S_BASE + (a - L_BASE) * N_COUNT + (b - V_BASE) * T_COUNT);
    }
    if (S_BASE..S_BASE + S_COUNT).contains(&a)
        && (a - S_BASE) % T_COUNT == 0
        && (T_BASE + 1..T_BASE + T_COUNT).contains(&b)
    {
        return Some(a + (b - T_BASE));
    }
    let i = tables::COMPOSITION
        .binary_search_by(|&(x, y, _)| (x, y).cmp(&(a, b)))
        .ok()?;
    Some(tables::COMPOSITION[i].2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spot_checks_match_the_ucd() {
        assert_eq!(UNICODE_VERSION, "16.0.0");
        assert_eq!(general_category(0x41), GeneralCategory::UppercaseLetter);
        assert_eq!(general_category(0x61), GeneralCategory::LowercaseLetter);
        assert_eq!(general_category(0x30), GeneralCategory::DecimalDigitNumber);
        assert_eq!(general_category(0x20), GeneralCategory::SpaceSeparator);
        assert_eq!(general_category(0x4E00), GeneralCategory::OtherLetter);
        assert_eq!(general_category(0x10_FFFF), GeneralCategory::NotAssigned);
        assert!(general_category(0x41).is_letter() && !general_category(0x30).is_letter());
        assert!(is_white_space(0x20) && is_white_space(0xA0) && !is_white_space(0x41));
        assert!(!is_white_space(0x1C));
        assert_eq!(numeric_level(0x30), 3);
        assert_eq!(numeric_level(0xB2), 2);
        assert_eq!(numeric_level(0xBD), 1);
        assert_eq!(numeric_level(0x4E00), 1);
        assert_eq!(numeric_level(0x41), 0);
        assert!(is_uppercase(0x41) && is_cased(0x41) && !is_lowercase(0x41));
        assert!(is_lowercase(0x61) && is_cased(0x61));
        assert!(!is_cased(0x35));
        assert!(is_xid_start(0x41) && is_xid_continue(0x41));
        assert!(!is_xid_start(0x30) && is_xid_continue(0x30));
    }

    #[cfg(feature = "normalization")]
    #[test]
    fn normalization_spot_checks() {
        use NormalizationForm::{Nfc, Nfd, Nfkc};
        assert_eq!(normalize("\u{00c0}", Nfd), "A\u{0300}");
        assert_eq!(normalize("A\u{0300}", Nfc), "\u{00c0}");
        assert_eq!(normalize("\u{00b2}", Nfkc), "2");
        assert_eq!(normalize("\u{00b2}", Nfc), "\u{00b2}");
        assert_eq!(normalize("\u{fb01}", Nfkc), "fi");
        assert_eq!(normalize("\u{212b}", Nfc), "\u{00c5}");
        assert_eq!(normalize("\u{ac01}", Nfd), "\u{1100}\u{1161}\u{11a8}");
        assert_eq!(normalize("\u{1100}\u{1161}\u{11a8}", Nfc), "\u{ac01}");
        assert_eq!(normalize("D\u{0307}\u{0323}", Nfc), "\u{1e0c}\u{0307}");
        assert_eq!(combining_class(0x0300), 230);
        assert_eq!(combining_class(0x0041), 0);
    }
}
