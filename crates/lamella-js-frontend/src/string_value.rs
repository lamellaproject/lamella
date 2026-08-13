//! An ECMAScript String value: **a sequence of UTF-16 code units, lone surrogates included.**

use crate::{String, Vec};

/// A String value: UTF-16 code units, no well-formedness requirement.
#[derive(Debug, Clone, PartialEq, Eq, Default, Hash)]
pub struct JsString(Vec<u16>);

impl JsString {
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    /// Appends one code unit **verbatim**, surrogate or not.
    ///
    /// This is the operation `String` cannot express, and it is the one `\uD834` needs.
    pub fn push_code_unit(&mut self, unit: u16) {
        self.0.push(unit);
    }

    /// Appends a code point, encoding it as one or two units.
    pub fn push_char(&mut self, ch: char) {
        let mut buffer = [0u16; 2];
        self.0.extend_from_slice(ch.encode_utf16(&mut buffer));
    }

    pub fn push_str(&mut self, text: &str) {
        self.0.extend(text.encode_utf16());
    }

    /// Appends another value's units unchanged. Used where a scratch buffer is folded into a
    /// template's cooked value; going via a Rust `String` would lose any lone surrogate on the way.
    pub fn extend_from(&mut self, other: &JsString) {
        self.0.extend_from_slice(&other.0);
    }

    /// The code unit at an index, which is what `charAt`, `charCodeAt` and `s[i]` all read.
    ///
    /// INDEXING IS BY CODE UNIT, NOT BY CHARACTER. `'\u{1F600}'[0]` is a lone high surrogate, and
    /// an implementation that indexes by `char` returns the whole astral character instead --
    /// a different language, silently, in exactly the inputs that make it matter.
    #[must_use]
    pub fn unit_at(&self, index: usize) -> Option<u16> {
        self.0.get(index).copied()
    }

    /// Builds a string from code units, which may include an unpaired surrogate.
    #[must_use]
    pub fn from_units(units: &[u16]) -> Self {
        JsString(units.to_vec())
    }

    #[must_use]
    pub fn units(&self) -> &[u16] {
        &self.0
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
        String::from_utf16_lossy(&self.0)
    }
}

impl From<&str> for JsString {
    fn from(text: &str) -> Self {
        Self(text.encode_utf16().collect())
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

    #[test]
    fn the_lossy_rendering_is_for_diagnostics_only() {
        let mut value = JsString::new();
        value.push_code_unit(0xD834);
        assert_eq!(value.to_lossy_string().chars().next(), Some('\u{FFFD}'));
        assert_eq!(value.len(), 1, "and the value itself is untouched by having been rendered");
    }
}
