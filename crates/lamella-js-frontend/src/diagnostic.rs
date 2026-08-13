//! Diagnostics, accumulated rather than thrown.

use crate::{String, Vec};
use crate::source::Span;

/// How serious a diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Error,
}

/// The phase at which a problem was detected.
///
/// # THIS IS NOT COSMETIC -- IT IS WHAT `negative: phase: parse` ASSERTS
///
/// Test262 distinguishes an error raised while parsing (including its **early errors**) from one
/// raised during evaluation, and a test that expects the first and gets the second **fails**. An
/// engine that accepts syntax the standard forbids and only complains when control reaches it is a
/// different language, so the phase has to be a fact the frontend records rather than something a
/// runner infers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Produced by the scanner.
    Lexical,
    /// Produced by the grammar: a token that cannot appear here.
    Syntactic,
    /// Produced by an **Early Error** rule: the source parses, and a per-production check forbids
    /// it anyway. `parse` for Test262's purposes, and the category the standard attaches to
    /// individual productions.
    Early,
}

/// One problem, with where it is and what kind it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub severity: Severity,
    pub phase: Phase,
    pub kind: DiagnosticKind,
    pub span: Span,
    pub message: String,
}

impl Diagnostic {
    #[must_use]
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// What kind of problem this is.
///
/// Stable identities for tests to match on, kept coarse enough that adding a message does not change
/// one. Prose lives in [`Diagnostic::message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticKind {
    /// A lexical error, forwarded from the scanner into the same channel.
    Lexical,
    /// A token the grammar cannot accept here.
    UnexpectedToken,
    /// A token the grammar requires and did not find.
    MissingToken,
    /// Automatic semicolon insertion did not apply and no `;` was present.
    MissingSemicolon,
    /// A construct this profile deliberately excludes, refused loudly.
    NotInProfile,
    /// An Early Error rule was violated.
    EarlyError,
}

/// The accumulating channel, shared by the scanner and the parser.
#[derive(Debug, Default)]
pub struct Diagnostics {
    items: Vec<Diagnostic>,
    /// Where the last error was reported, for cascade suppression.
    last_error_at: Option<usize>,
    suppressed: usize,
}

impl Diagnostics {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a diagnostic, **dropping it if it is a cascade of the previous one**.
    ///
    /// # WHY SUPPRESSION IS PART OF THE CHANNEL RATHER THAN A CLEANUP PASS
    ///
    /// After a syntax error the parser is, by construction, in a state the program did not describe,
    /// and the errors it reports next are consequences rather than causes. `lamella-syntax` learned
    /// this and filters cascades before returning (`without_gated_operator_cascades`). Reporting
    /// them all is not "more information": it is one real finding buried under its own wreckage,
    /// and it trains a reader to skim past the line that mattered.
    ///
    /// The rule here is deliberately the simplest one that works: **at most one error per source
    /// position, and never a second error at a position at or before the last one.** A parser that
    /// has not consumed a token since the last error has not learned anything new, so whatever it
    /// wants to say now is the same problem wearing a different message.
    pub fn report(&mut self, diagnostic: Diagnostic) {
        if diagnostic.severity == Severity::Error {
            if let Some(last) = self.last_error_at {
                if diagnostic.span.start <= last {
                    self.suppressed += 1;
                    return;
                }
            }
            self.last_error_at = Some(diagnostic.span.start);
        }
        self.items.push(diagnostic);
    }

    pub fn error(&mut self, phase: Phase, kind: DiagnosticKind, span: Span, message: impl Into<String>) {
        self.report(Diagnostic {
            severity: Severity::Error,
            phase,
            kind,
            span,
            message: message.into(),
        });
    }

    #[must_use]
    pub fn items(&self) -> &[Diagnostic] {
        &self.items
    }

    #[must_use]
    pub fn has_errors(&self) -> bool {
        self.items.iter().any(Diagnostic::is_error)
    }

    /// The first error, which is the one a single-error consumer should report.
    #[must_use]
    pub fn first_error(&self) -> Option<&Diagnostic> {
        self.items.iter().find(|item| item.is_error())
    }

    /// How many cascades were dropped. Counted rather than discarded silently: a suppression rule
    /// that is too aggressive hides real findings, and the only way to notice is for the count to be
    /// visible.
    #[must_use]
    pub fn suppressed(&self) -> usize {
        self.suppressed
    }

    /// Drops diagnostics recorded after `len`, and rewinds the cascade cursor with them.
    ///
    /// # THE ONLY LEGITIMATE USE IS ABANDONING A SPECULATIVE PARSE
    ///
    /// An arrow head that turns out to be a parenthesized expression must not leave its attempt's
    /// complaints behind: the text it complained about is about to be parsed correctly a different
    /// way, so those diagnostics describe a reading that did not happen. **Any other use is hiding a
    /// finding**, which is why this is crate-private and says so.
    ///
    /// The cascade cursor is rewound with them. Leaving it pointing past the dropped entries would
    /// suppress the NEXT real error in that region -- a silent loss, and the harder kind to notice.
    pub(crate) fn truncate(&mut self, len: usize) {
        if len >= self.items.len() {
            return;
        }
        self.items.truncate(len);
        self.last_error_at = self
            .items
            .iter()
            .filter(|item| item.is_error())
            .map(|item| item.span.start)
            .next_back();
    }

    #[must_use]
    pub fn into_items(self) -> Vec<Diagnostic> {
        self.items
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error_at(start: usize) -> Diagnostic {
        Diagnostic {
            severity: Severity::Error,
            phase: Phase::Syntactic,
            kind: DiagnosticKind::UnexpectedToken,
            span: Span::new(start, start + 1),
            message: "boom".to_string(),
        }
    }

    /// The reason this follows C# rather than Python: several problems in one parse must all be
    /// reportable, because a REPL and an editor need them and an early-error catalog can violate
    /// more than one rule per production.
    #[test]
    fn several_errors_at_different_positions_are_all_kept() {
        let mut diagnostics = Diagnostics::new();
        diagnostics.report(error_at(0));
        diagnostics.report(error_at(10));
        diagnostics.report(error_at(20));
        assert_eq!(diagnostics.items().len(), 3);
    }

    /// THE DEFECT THIS GUARDS: after an error the parser is in a state the program did not
    /// describe, so what it says next is wreckage. Reporting it all buries the one real finding.
    #[test]
    fn a_cascade_at_or_before_the_last_error_is_dropped_and_counted() {
        let mut diagnostics = Diagnostics::new();
        diagnostics.report(error_at(10));
        diagnostics.report(error_at(10));
        diagnostics.report(error_at(4));
        assert_eq!(diagnostics.items().len(), 1, "one finding, not three");
        assert_eq!(diagnostics.suppressed(), 2);
    }

    /// Counted rather than dropped silently, because a suppression rule that is too aggressive hides
    /// real findings and the count is the only way anyone would notice.
    #[test]
    fn suppression_is_visible() {
        let mut diagnostics = Diagnostics::new();
        assert_eq!(diagnostics.suppressed(), 0);
        diagnostics.report(error_at(5));
        diagnostics.report(error_at(5));
        assert_eq!(diagnostics.suppressed(), 1);
    }

    /// A warning is not a cascade source: it does not mean the parser lost its place, so it must not
    /// suppress a genuine error that follows it.
    #[test]
    fn a_warning_does_not_suppress_a_later_error() {
        let mut diagnostics = Diagnostics::new();
        diagnostics.report(Diagnostic { severity: Severity::Warning, ..error_at(10) });
        diagnostics.report(error_at(2));
        assert_eq!(diagnostics.items().len(), 2);
        assert!(diagnostics.has_errors());
    }

    #[test]
    fn the_phase_is_recorded_because_a_negative_test_asserts_it() {
        let mut diagnostics = Diagnostics::new();
        diagnostics.error(Phase::Early, DiagnosticKind::EarlyError, Span::new(0, 1), "dup");
        assert_eq!(diagnostics.first_error().unwrap().phase, Phase::Early);
    }
}
