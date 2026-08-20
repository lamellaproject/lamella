//! An ECMAScript String value: **a sequence of UTF-16 code units, lone surrogates included.**

//! # THE UNITS MAY LIVE IN FLASH, AND THAT IS WHY THIS IS NOT A `Vec<u16>`

use crate::object::Store;
#[cfg(test)]
use crate::Vec;
use crate::String;

/// A String value: UTF-16 code units, no well-formedness requirement.
#[derive(Clone, PartialEq, Eq, Default, Hash)]
pub struct JsString(Store<u16>);

/// The units, and **never which arm holds them**.
///
/// # A DERIVE HERE PUTS A FALSE DISAGREEMENT INTO EVERY INSTRUMENT THAT COMPARES RENDERINGS
///
/// The derived form prints `JsString(Owned([104, 105]))` or `JsString(Static([104, 105]))` depending
/// on where the units came from -- so two strings that ARE equal render differently, and anything
/// diffing debug output reports a difference that is not one. This engine's differentials compare
/// renderings, and the realm's property values are exactly the strings that would come from flash on
/// one side and be built at run time on the other. The first pass at the flash tables produced that
/// disagreement on `[3,1,2].sort().join(',')`, which has no realm string in its answer at all.
///
/// It prints what the derive printed before the units could be borrowed, so nothing downstream had to
/// change. `a_borrowed_and_an_owned_string_render_identically` is the guard.
impl core::fmt::Debug for JsString {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.debug_tuple("JsString").field(&self.units()).finish()
    }
}

impl JsString {
    #[must_use]
    pub fn new() -> Self {
        Self(Store::new())
    }

    /// Units emitted as constant data, borrowed where they lie.
    ///
    /// `pub(crate)` ON PURPOSE: the only caller is the realm's build-time tables, which are internal.
    /// A published constructor taking a `&'static [u16]` would widen the surface for no outside
    /// reader's benefit.
    #[must_use]
    pub(crate) const fn from_static_units(units: &'static [u16]) -> Self {
        Self(Store::Static(units))
    }

    /// Appends one code unit **verbatim**, surrogate or not.
    ///
    /// This is the operation `String` cannot express, and it is the one `\uD834` needs.
    pub fn push_code_unit(&mut self, unit: u16) {
        self.0.owned().push(unit);
    }

    /// Appends a code point, encoding it as one or two units.
    pub fn push_char(&mut self, ch: char) {
        let mut buffer = [0u16; 2];
        self.0.owned().extend_from_slice(ch.encode_utf16(&mut buffer));
    }

    pub fn push_str(&mut self, text: &str) {
        self.0.owned().extend(text.encode_utf16());
    }

    /// Appends another value's units unchanged. Used where a scratch buffer is folded into a
    /// template's cooked value; going via a Rust `String` would lose any lone surrogate on the way.
    pub fn extend_from(&mut self, other: &JsString) {
        let units = other.0.as_slice().to_vec();
        self.0.owned().extend_from_slice(&units);
    }

    /// The code unit at an index, which is what `charAt`, `charCodeAt` and `s[i]` all read.
    ///
    /// INDEXING IS BY CODE UNIT, NOT BY CHARACTER. `'\u{1F600}'[0]` is a lone high surrogate, and
    /// an implementation that indexes by `char` returns the whole astral character instead --
    /// a different language, silently, in exactly the inputs that make it matter.
    #[must_use]
    pub fn unit_at(&self, index: usize) -> Option<u16> {
        self.0.as_slice().get(index).copied()
    }

    /// Builds a string from code units, which may include an unpaired surrogate.
    #[must_use]
    pub fn from_units(units: &[u16]) -> Self {
        JsString(Store::Owned(units.to_vec()))
    }

    #[must_use]
    pub fn units(&self) -> &[u16] {
        self.0.as_slice()
    }

    /// The `length` a program would observe: **code units, not characters.** An astral character is
    /// 2, which is the number the corpus checks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// A human-readable rendering for diagnostics, with unpaired surrogates replaced.
    ///
    /// **Lossy on purpose and named so.** Nothing that decides program behaviour may go through
    /// here -- the moment a value's meaning depends on this function, the type has stopped doing its
    /// job and the surrogate has been lost again somewhere new.
    #[must_use]
    pub fn to_lossy_string(&self) -> String {
        String::from_utf16_lossy(self.0.as_slice())
    }
}

impl From<&str> for JsString {
    fn from(text: &str) -> Self {
        Self(Store::Owned(text.encode_utf16().collect()))
    }
}

impl From<String> for JsString {
    fn from(text: String) -> Self {
        Self::from(text.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An FNV-1a, because this crate is `no_std` and there is no `DefaultHasher` to borrow. Any
    /// hasher answers the only question asked of it: whether two equal values hash alike.
    #[derive(Default)]
    struct Fnv(u64);

    impl core::hash::Hasher for Fnv {
        fn finish(&self) -> u64 {
            self.0
        }

        fn write(&mut self, bytes: &[u8]) {
            for byte in bytes {
                self.0 = (self.0 ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3);
            }
        }
    }

    fn hash_of(value: &JsString) -> u64 {
        use core::hash::{Hash, Hasher};
        let mut hasher = Fnv::default();
        value.hash(&mut hasher);
        hasher.finish()
    }

    /// THE DEFECT THIS TYPE EXISTS FOR: a Rust `String` cannot hold this, so a scanner built on one
    /// rejects a legal program.
    #[test]
    fn a_lone_surrogate_is_a_value_not_an_error() {
        let mut value = JsString::new();
        value.push_code_unit(0xD834);
        assert_eq!(value.units(), &[0xD834]);
        assert_eq!(value.len(), 1);
    }

    /// The number a program observes for an astral character is TWO. A length counted in `char`s
    /// reports 1, which is the observable difference between the two languages.
    #[test]
    fn length_counts_code_units_so_an_astral_character_is_two() {
        let value = JsString::from("\u{1D306}");
        assert_eq!(value.len(), 2, "one character, two code units");
        assert_eq!(value.units(), &[0xD834, 0xDF06]);
    }

    #[test]
    fn a_surrogate_pair_built_from_units_equals_the_same_text() {
        let mut built = JsString::new();
        built.push_code_unit(0xD834);
        built.push_code_unit(0xDF06);
        assert_eq!(built, JsString::from("\u{1D306}"), "the pair IS the character");
    }

    /// The units may be borrowed from `.rodata` or owned in the arena, and **a String value is its
    /// contents**. Every observable has to agree with that: equality, hashing and the debug rendering
    /// the differentials compare.
    #[test]
    fn a_borrowed_and_an_owned_string_are_the_same_value() {
        let borrowed = JsString::from_static_units(&[0x68, 0x69]);
        let owned = JsString::from("hi");

        assert_eq!(borrowed, owned, "`\"hi\" === \"hi\"` would be FALSE across the arms");
        assert_eq!(owned, borrowed, "and equality is symmetric");
        assert_eq!(borrowed.units(), owned.units());
        assert_eq!(borrowed.len(), owned.len());

        assert_eq!(hash_of(&borrowed), hash_of(&owned), "equal strings hashed differently");
    }

    /// Borrowing the units must cost NOTHING, because a `JsString` sits inside every property key and
    /// every string-valued property in the realm -- 1,429 of them. If the arm ever stops fitting a
    /// niche, every one of those grows by a word and the change pays for nothing.
    #[test]
    fn borrowing_the_units_costs_no_space() {
        use core::mem::size_of;
        assert_eq!(
            size_of::<JsString>(),
            size_of::<Vec<u16>>(),
            "the borrow arm stopped fitting a niche"
        );
    }

    /// Renderings are compared by the differentials, so the arm must not show through one.
    #[test]
    fn a_borrowed_and_an_owned_string_render_identically() {
        let borrowed = JsString::from_static_units(&[0x68, 0x69]);
        let owned = JsString::from("hi");
        assert_eq!(format!("{borrowed:?}"), format!("{owned:?}"));
        assert_eq!(format!("{owned:?}"), "JsString([104, 105])", "the rendering changed shape");
    }

    /// A borrowed string PROMOTES when appended to, exactly as a property store does -- so building
    /// a value out of a flash one cannot write through the flash.
    #[test]
    fn appending_to_a_borrowed_string_promotes_it() {
        let mut value = JsString::from_static_units(&[0x68]);
        value.push_str("i");
        assert_eq!(value, JsString::from("hi"));
        assert_eq!(value.units(), &[0x68, 0x69]);
    }

    #[test]
    fn the_lossy_rendering_is_for_diagnostics_only() {
        let mut value = JsString::new();
        value.push_code_unit(0xD834);
        assert_eq!(value.to_lossy_string().chars().next(), Some('\u{FFFD}'));
        assert_eq!(value.len(), 1, "and the value itself is untouched by having been rendered");
    }
}
