//! What this profile does not implement, as a closed set rather than a habit.

/// A feature this profile does not implement at RUN time.
///
/// A program that reaches one is refused with an `InternalError` whose message is
/// [`Absence::message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absence {
    /// `await`, which suspends an async function until a promise settles.
    Await,

    /// `eval(...)`, which compiles source in the caller's scope.
    ///
    /// Present only in a build with the `eval` feature, which links the parser.
    Eval,
    /// `new Function(...)`, which compiles source in the global scope.
    FunctionConstructor,

    /// `"a".normalize()`.
    ///
    /// Its Unicode data costs more flash than one string method justifies, and returning the string
    /// unchanged would be wrong for the very inputs a program normalizes.
    StringNormalize,
}

impl Absence {
    /// The stable identifier for this absence. Never a sentence, so it can be searched for.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Absence::Await => "await",
            Absence::Eval => "eval",
            Absence::FunctionConstructor => "function-constructor",
            Absence::StringNormalize => "string-normalize",
        }
    }

    /// What a program is told when it meets this absence.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Absence::Await => "await is not in this profile",
            Absence::Eval => "eval compiles source at run time and is not in this profile",
            Absence::FunctionConstructor => {
                "the Function constructor compiles source at run time and is not in this profile"
            }
            Absence::StringNormalize => "String.prototype.normalize is not in this profile",
        }
    }

    /// The cargo feature that FILLS this gap, for the gaps a build can choose to fill.
    ///
    /// The answer is the same in every build; [`Absence::refusable`] answers for the build you are
    /// running.
    #[must_use]
    pub fn knob(self) -> Option<&'static str> {
        match self {
            Absence::Eval => Some("eval"),
            _ => None,
        }
    }

    /// Whether this gap exists in **the binary you are running**.
    ///
    /// It is `false` only where a feature enabled in this build fills the gap, so a caller that
    /// expects a refusal asks this first.
    #[must_use]
    pub fn refusable(self) -> bool {
        match self {
            Absence::Eval => cfg!(not(feature = "eval")),
            _ => true,
        }
    }

    /// Every absence, whatever features this build has.
    ///
    /// Filter with [`Absence::refusable`] for the gaps in the binary you are running.
    #[must_use]
    pub fn all() -> &'static [Absence] {
        &[
            Absence::Await,
            Absence::Eval,
            Absence::FunctionConstructor,
            Absence::StringNormalize,
        ]
    }
}

/// A feature this profile does not implement at PARSE time, refused with a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxAbsence {
    Class(&'static str),
    /// A `yield` in a position the state-machine transform cannot cut the body at.
    ///
    /// [`SyntaxAbsence::reason`] lists those positions, and a refusal names the one it found.
    Yield,
    AsyncFunctions,
    BigIntLiterals,
    AnnexBStringEscapes,
    AnnexBBlockScopedFunctions,
}

impl SyntaxAbsence {
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            SyntaxAbsence::Class(which) => which,
            SyntaxAbsence::Yield => "yield",
            SyntaxAbsence::AsyncFunctions => "async-functions",
            SyntaxAbsence::BigIntLiterals => "bigint-literals",
            SyntaxAbsence::AnnexBStringEscapes => "annexb-string-escapes",
            SyntaxAbsence::AnnexBBlockScopedFunctions => "annexb-block-scoped-functions",
        }
    }

    /// Why it is absent, which is the part a reader cannot reconstruct from the name.
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            SyntaxAbsence::Class("class-fields") => "class fields",
            SyntaxAbsence::Class("private-class-members") => "private class members",
            SyntaxAbsence::Class("class-static-blocks") => "class static blocks",
            SyntaxAbsence::Class(_) => "a class feature",
            SyntaxAbsence::Yield => {
                "a `yield` in a loop or `if` condition, in a `for` header, in a `for`-`in`, in a \
                 `for`-`of` head, in a `catch` parameter, inside a `catch` that destructures \
                 its parameter or assigns it, inside a labelled statement, a `switch` or a \
                 `with`, in the operand of a `throw`, or in a class's computed member name; and, \
                 among the positions inside a larger expression, \
                 in an array spread that a later suspension follows, in a template substitution \
                 that a later suspension follows, in an argument of a method call, in the \
                 arguments of a `new`, in a tagged template's substitution, in the operand of an \
                 increment, in the value of a compound assignment to a member, or in an arrow's \
                 parameter \
                 default; a `return` written in a `catch` that a `finally` encloses, and a `break` \
                 or `continue` whose loop is outside the `finally` it would have to leave; and a \
                 `let` or `const` in a body that suspends -- a `yield`, or a delegating `yield*`, \
                 standing as a whole statement, as the whole value of a declarator, a `return`, \
                 or a plain assignment to a name, to a member or to a destructuring pattern, \
                 or nested inside a larger expression whose other operands can be \
                 parked in the frame, is rewritten into a control-flow graph and runs, including \
                 inside `while`, `do`, `for`, `if`, the BODY of a `for`-`of`, a `try` whose \
                 handler is a `catch`, and a `try` that has a `finally`, and across `break` and \
                 `continue`"
            }
            SyntaxAbsence::AsyncFunctions => "async functions and methods",
            SyntaxAbsence::BigIntLiterals => "BigInt literals, whose arithmetic is a second numeric tower",
            SyntaxAbsence::AnnexBStringEscapes => "Annex B legacy octal and \\8 \\9 string escapes",
            SyntaxAbsence::AnnexBBlockScopedFunctions => {
                "Annex B block-scoped function declarations in sloppy code"
            }
        }
    }

    #[must_use]
    pub fn all() -> &'static [SyntaxAbsence] {
        &[
            SyntaxAbsence::Class("class-fields"),
            SyntaxAbsence::Class("private-class-members"),
            SyntaxAbsence::Class("class-static-blocks"),
            SyntaxAbsence::Yield,
            SyntaxAbsence::AsyncFunctions,
            SyntaxAbsence::BigIntLiterals,
            SyntaxAbsence::AnnexBStringEscapes,
            SyntaxAbsence::AnnexBBlockScopedFunctions,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE IDS ARE SEARCH KEYS AND MUST BE UNIQUE, or a search for one finds another.
    #[test]
    fn every_absence_has_a_distinct_searchable_id() {
        let mut ids: Vec<&str> = Absence::all().iter().map(|a| a.id()).collect();
        ids.extend(SyntaxAbsence::all().iter().map(|a| a.id()));
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "an absence id is duplicated");
        for id in ids {
            assert!(!id.is_empty());
            assert!(
                id.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "{id} is not a searchable key"
            );
        }
    }

    /// THE TWO HALVES OF A KNOB ARE SEPARATE `match`ES AND NOTHING MAKES THEM AGREE.
    /// `cfg!` needs a literal feature name, so [`Absence::refusable`] cannot be derived from
    /// [`Absence::knob`]; they can only be checked against each other. An absence with no knob must
    /// be refusable in every build.
    ///
    /// This has teeth only where a feature is ON: in the default build every `refusable()` is
    /// true and the assertion is vacuous.
    #[test]
    fn a_knob_is_named_wherever_it_is_honored() {
        for absence in Absence::all() {
            if absence.knob().is_none() {
                assert!(
                    absence.refusable(),
                    "{:?} is not refusable in this build but names no knob -- a `cfg` arm without \
                     a `knob()` entry makes the published list wrong for this configuration",
                    absence
                );
            }
        }
    }

    /// The same fact from the other side, and the one that is NOT vacuous in the default build:
    /// `eval` is knob-controlled, and `refusable()` must follow the feature in both directions.
    #[test]
    fn the_eval_knob_moves_the_entry() {
        assert_eq!(Absence::Eval.knob(), Some("eval"));
        assert_eq!(
            Absence::Eval.refusable(),
            cfg!(not(feature = "eval")),
            "the entry must be absent exactly when the feature is off"
        );
    }

    /// A MESSAGE THAT DOES NOT SAY IT IS AN ABSENCE reads as a defect to whoever meets it, which
    /// is the whole failure this type exists to prevent.
    #[test]
    fn every_message_says_it_is_an_absence() {
        for absence in Absence::all() {
            let message = absence.message();
            assert!(
                message.contains("not in this profile") || message.contains("not evaluated in this profile"),
                "{:?} must say it is an absence: {message}",
                absence
            );
        }
    }
}



