//! Whether a message renders the way a person will read it.
//!
//! **A MESSAGE IS CHECKED AS OUTPUT, NOT AS SOURCE.** A multi-line string keeps the indentation of
//! the source it was written in unless it is spelled to avoid that, so a refusal that lines up
//! neatly in an editor can print columns of stray spaces mid-sentence. The difference is invisible
//! to a `contains` assertion, which answers "is this word present" and never "is this the sentence
//! we meant to emit", so it survives every test anybody writes about the message's content.
//!
//! **ONE IMPLEMENTATION, BECAUSE THE LAST THREE WERE THREE.** This check was written out longhand
//! in each module that renders a refusal, which meant a module that gained a refusal gained no
//! check -- and the newest set of them shipped a run of eighteen spaces mid-sentence with every
//! assertion on them green. What varies between callers is one predicate, so that is the argument.

/// Assert that `message` renders with no whitespace a reader did not ask for.
///
/// `indented` names the lines whose leading space is deliberate -- a command line offered for
/// copying, an indented sample -- because a rule with no exemption would be answered by deleting
/// the indentation that makes those readable.
pub(crate) fn assert_renders_cleanly(message: &str, indented: impl Fn(&str) -> bool) {
    for line in message.lines() {
        let deliberate = indented(line);
        assert!(
            line == line.trim_end(),
            "a line ends in whitespace:\n{message}"
        );
        assert!(
            !line.starts_with(' ') || deliberate,
            "a continuation kept its source indentation:\n{message}"
        );
        if !deliberate {
            assert!(
                !line.contains("  "),
                "a doubled space mid-line:\n{message}"
            );
        }
    }
}

/// A line indented by exactly four spaces: the sample-block shape these messages use.
pub(crate) fn four_space_sample(line: &str) -> bool {
    line.starts_with("    ") && !line.starts_with("     ")
}
