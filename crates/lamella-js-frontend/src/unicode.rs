//! The identifier alphabet, and an honest hole in it.


/// Whether a code point may START an identifier, or why this scanner cannot say.
///
/// `Err` is not "no". It is **"this scanner cannot answer"**, which is a different fact and has to
/// stay different -- collapsing it to `false` would report a conforming program as a syntax error
/// with no way to tell that apart from a genuine one.
pub fn is_id_start(ch: char) -> Result<bool, Unsupported> {
    if ch.is_ascii() {
        return Ok(ch.is_ascii_alphabetic() || ch == '$' || ch == '_');
    }
    if definitely_not_an_identifier_character(ch) {
        return Ok(false);
    }
    Err(refusal(ch))
}

/// Code points that are non-ASCII but still answerable, because they belong to categories DISJOINT
/// from ID_Start and ID_Continue.
///
/// # THE DEFECT THIS EXISTS TO FIX, FOUND BY A TEST RATHER THAN BY READING
///
/// Refusing to classify is right when the answer is genuinely unknown. It is WRONG when the code
/// point merely ENDS an identifier -- and `a<U+2028>b` is a perfectly ordinary program where U+2028
/// terminates `a`. The first version refused there, so a legal program became a lexical error, and
/// the "refuse rather than guess" rule had quietly turned into "reject anything unfamiliar".
///
/// Line terminators and whitespace are in categories (Zl, Zp, Zs, Cc) that ID_Start and ID_Continue
/// do not draw from at all, so `false` here is a fact rather than a guess -- which is the only
/// standard a member of this list has to meet.
fn definitely_not_an_identifier_character(ch: char) -> bool {
    crate::source::is_line_terminator(ch) || crate::source::is_whitespace(ch)
}

/// Whether a code point may CONTINUE an identifier, or why we cannot say.
pub fn is_id_continue(ch: char) -> Result<bool, Unsupported> {
    if ch.is_ascii() {
        return Ok(ch.is_ascii_alphanumeric() || ch == '$' || ch == '_');
    }
    if ch == '\u{200C}' || ch == '\u{200D}' {
        return Ok(true);
    }
    if definitely_not_an_identifier_character(ch) {
        return Ok(false);
    }
    Err(refusal(ch))
}

/// A code point this scanner declines to classify.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsupported {
    pub code_point: char,
    pub reason: &'static str,
}

fn refusal(code_point: char) -> Unsupported {
    Unsupported {
        code_point,
        reason: "non-ASCII identifiers need ID_Start/ID_Continue tables, which this frontend does \
                 not carry. It will not substitute XID_Start/XID_Continue, which differ: that would \
                 silently accept and reject the wrong programs. Absent and listed, never wrong.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_ascii_alphabet_is_exact() {
        for ch in ['a', 'Z', '$', '_'] {
            assert_eq!(is_id_start(ch), Ok(true), "{ch:?} starts an identifier");
        }
        for ch in ['0', '9'] {
            assert_eq!(is_id_start(ch), Ok(false), "{ch:?} cannot START one");
            assert_eq!(is_id_continue(ch), Ok(true), "but it can continue one");
        }
        for ch in ['+', ' ', '-', '.'] {
            assert_eq!(is_id_start(ch), Ok(false));
            assert_eq!(is_id_continue(ch), Ok(false));
        }
    }

    /// THE POINT OF THIS MODULE. A code point we cannot classify must not answer `false`: that
    /// would report a conforming program as a syntax error, indistinguishably from a genuine one.
    #[test]
    fn an_unclassifiable_code_point_refuses_rather_than_answering_no() {
        let result = is_id_start('\u{00E9}');
        assert!(result.is_err(), "the answer is 'cannot say', which is not 'no'");
        assert_eq!(result.unwrap_err().code_point, '\u{00E9}', "and it names the code point");
    }

    /// ZWNJ/ZWJ come from the GRAMMAR rather than from ID_Continue, so they are answerable without
    /// the tables -- and answering them costs nothing while refusing them would be wrong.
    #[test]
    fn the_grammars_own_additions_are_answered_not_refused() {
        assert_eq!(is_id_continue('\u{200C}'), Ok(true), "ZWNJ");
        assert_eq!(is_id_continue('\u{200D}'), Ok(true), "ZWJ");
        assert!(is_id_start('\u{200C}').is_err(), "but neither may START an identifier");
    }

    /// THE DEFECT THIS GUARDS, and it was live: refusing is right when the answer is unknown and
    /// WRONG when the code point merely ends the identifier. `a<U+2028>b` is an ordinary program.
    /// The first version rejected it, turning "refuse rather than guess" into "reject anything
    /// unfamiliar" -- which fails conforming programs, the exact failure mode the rule exists to
    /// prevent.
    #[test]
    fn a_code_point_that_merely_ends_an_identifier_is_answered_not_refused() {
        for terminator in ['\u{2028}', '\u{2029}', '\u{00A0}', '\u{3000}', '\u{FEFF}'] {
            assert_eq!(
                is_id_continue(terminator),
                Ok(false),
                "{terminator:?} ends an identifier; that is a fact, not a guess"
            );
            assert_eq!(is_id_start(terminator), Ok(false));
        }
    }

    #[test]
    fn the_refusal_says_why_and_names_the_substitution_it_refuses_to_make() {
        let reason = is_id_start('\u{4E00}').unwrap_err().reason;
        assert!(reason.contains("ID_Start"), "names what is missing");
        assert!(reason.contains("XID_Start"), "and names the wrong answer it declines to give");
    }
}
