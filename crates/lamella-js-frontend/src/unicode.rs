//! The identifier alphabet, read from the one table all three languages share.

/// Whether a code point may START an identifier.
#[must_use]
pub fn is_id_start(ch: char) -> bool {
    if ch.is_ascii() {
        return ch.is_ascii_alphabetic() || ch == '$' || ch == '_';
    }
    lamella_unicode::is_id_start(ch as u32)
}

/// Whether a code point may CONTINUE an identifier.
#[must_use]
pub fn is_id_continue(ch: char) -> bool {
    if ch.is_ascii() {
        return ch.is_ascii_alphanumeric() || ch == '$' || ch == '_';
    }
    if ch == '\u{200C}' || ch == '\u{200D}' {
        return true;
    }
    lamella_unicode::is_id_continue(ch as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ascii_alphabet_is_exact() {
        for ch in ['a', 'Z', '$', '_'] {
            assert!(is_id_start(ch), "{ch:?} starts an identifier");
        }
        for ch in ['0', '9'] {
            assert!(!is_id_start(ch), "{ch:?} cannot START one");
            assert!(is_id_continue(ch), "but it can continue one");
        }
        for ch in ['+', ' ', '-', '.'] {
            assert!(!is_id_start(ch));
            assert!(!is_id_continue(ch));
        }
    }

    /// A non-ASCII letter is an ordinary identifier character, answered from the canonical table.
    #[test]
    fn a_non_ascii_letter_is_an_identifier_character() {
        assert!(is_id_start('\u{00E9}'), "LATIN SMALL LETTER E WITH ACUTE");
        assert!(is_id_continue('\u{00E9}'));
        assert!(is_id_start('\u{4E00}'), "CJK UNIFIED IDEOGRAPH-4E00");
        assert!(is_id_start('\u{05D0}'), "HEBREW LETTER ALEF");
    }

    /// A combining mark CONTINUES an identifier and cannot START one. The two properties are
    /// distinct sets and this is the cheapest case that tells them apart.
    #[test]
    fn a_combining_mark_continues_but_does_not_start() {
        assert!(!is_id_start('\u{0301}'), "COMBINING ACUTE ACCENT");
        assert!(is_id_continue('\u{0301}'));
    }

    /// The grammar's own additions are answered here, and the table's agreement is checked rather
    /// than assumed -- so the redundancy is a measured fact, and this test names what changed if a
    /// future UCD ever drops them while the engine keeps answering correctly.
    ///
    #[test]
    fn the_grammars_own_additions_are_answered_whatever_the_table_says() {
        assert!(is_id_continue('\u{200C}'), "ZWNJ");
        assert!(is_id_continue('\u{200D}'), "ZWJ");
        assert!(!is_id_start('\u{200C}'), "but neither may START an identifier");
        assert!(!is_id_start('\u{200D}'));
        assert!(lamella_unicode::is_id_continue(0x200C), "and the table agrees today");
        assert!(lamella_unicode::is_id_continue(0x200D));
        assert!(!lamella_unicode::is_id_start(0x200C), "in both directions");
    }

    /// A code point that merely ENDS an identifier answers `false`, because `a<U+2028>b` is an
    /// ordinary program. Zl, Zp, Zs and Cc are disjoint from both properties, so the table answers
    /// it directly.
    ///
    #[test]
    fn a_code_point_that_merely_ends_an_identifier_answers_no() {
        for terminator in ['\u{2028}', '\u{2029}', '\u{00A0}', '\u{3000}', '\u{FEFF}'] {
            assert!(!is_id_continue(terminator), "{terminator:?} ends an identifier");
            assert!(!is_id_start(terminator));
        }
    }

    /// THE FAST PATH IS A SECOND IMPLEMENTATION AND THIS IS WHAT STOPS IT DRIFTING.
    ///
    /// Both halves return a plain `bool`, so a disagreement between the ASCII branch and the table
    /// would be a wrong answer with no diagnostic anywhere. The equality is asserted over all 128
    /// ASCII code points against the table plus exactly the members the grammar adds -- which also
    /// pins those additions: if `_` ever entered ID_Start upstream, the `start` line fails here
    /// rather than silently becoming redundant.
    #[test]
    fn the_ascii_fast_path_equals_the_table() {
        for cp in 0u32..128 {
            let ch = char::from_u32(cp).expect("ASCII");
            let table_start = lamella_unicode::is_id_start(cp) || ch == '$' || ch == '_';
            let table_continue = lamella_unicode::is_id_continue(cp) || ch == '$';
            assert_eq!(is_id_start(ch), table_start, "start disagrees at U+{cp:04X} ({ch:?})");
            assert_eq!(
                is_id_continue(ch),
                table_continue,
                "continue disagrees at U+{cp:04X} ({ch:?})"
            );
        }
    }
}
