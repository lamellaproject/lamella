//! Look a Unicode character up by its NAME -- what a Python `\N{NAME}` escape resolves.

#[cfg(not(feature = "char-names-full"))]
use crate::char_names_table::NAMES_DEFAULT as NAMES;
#[cfg(feature = "char-names-full")]
use crate::char_names_table_full::NAMES_FULL as NAMES;

/// The character a Unicode NAME denotes, or `None` when this build's table does not carry it.
///
/// The name is matched EXACTLY: Unicode names are upper-case ASCII with spaces and hyphens, and
/// no case folding or whitespace normalization happens here. A caller that wants to accept other
/// spellings owns that decision, because loosening it here would silently change what every
/// consumer accepts.
///
/// `None` means "not in this table", which is not the same as "not a Unicode name" -- see the
/// module docs for the two kinds this table never carries.
#[must_use]
pub fn lookup(name: &str) -> Option<char> {
    NAMES
        .binary_search_by(|(candidate, _)| (*candidate).cmp(name))
        .ok()
        .map(|i| NAMES[i].1)
}

/// How many names this build can resolve.
///
/// Exposed so a consumer can report the size of the set it is refusing from, rather than a
/// caller having to know which feature was on.
#[must_use]
pub fn len() -> usize {
    NAMES.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_resolves_to_its_code_point() {
        assert_eq!(lookup("LATIN SMALL LETTER A"), Some('a'));
        assert_eq!(lookup("GREEK SMALL LETTER ALPHA"), Some('\u{3B1}'));
        assert_eq!(lookup("EM DASH"), Some('\u{2014}'));
        assert_eq!(lookup("OX"), Some('\u{1F402}'));
        assert_eq!(lookup("ABACUS"), Some('\u{1F9EE}'));
    }

    #[test]
    fn the_table_is_sorted_so_the_binary_search_is_valid() {
        assert!(NAMES.windows(2).all(|w| w[0].0 < w[1].0), "names must be strictly ascending");
    }

    #[test]
    fn a_name_this_build_does_not_carry_is_none_rather_than_a_guess() {
        assert_eq!(lookup("NOT A REAL UNICODE NAME"), None);
        assert_eq!(lookup(""), None);
        assert_eq!(lookup("latin small letter a"), None);
        assert_eq!(lookup("LATIN  SMALL  LETTER  A"), None);
        assert_eq!(lookup("CJK UNIFIED IDEOGRAPH-4E00"), None);
        assert_eq!(lookup("NULL"), None);
    }

    #[cfg(not(feature = "char-names-full"))]
    #[test]
    fn the_default_cut_is_the_ruled_one() {
        assert_eq!(len(), 13_052);
        assert_eq!(lookup("CIRCLED IDEOGRAPH QUESTION"), None);
    }
}
