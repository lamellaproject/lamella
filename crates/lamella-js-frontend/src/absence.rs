//! What this profile does not implement, as a closed set rather than a habit.

use crate::{format, String};

/// A feature this profile does not implement at RUN time.
///
/// Every variant is a DELIBERATE gap with a reason, not a to-do. The order here is the order the
/// published list uses, grouped by what a reader would look for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Absence {
    Await,
    PromiseFinally,
    Eval,
    FunctionConstructor,
    StringNormalize,
    MathRandom,
}

impl Absence {
    /// The stable identifier the published list uses. Never a sentence, so it can be searched for.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Absence::Await => "await",
            Absence::PromiseFinally => "promise-finally",
            Absence::Eval => "eval",
            Absence::FunctionConstructor => "function-constructor",
            Absence::StringNormalize => "string-normalize",
            Absence::MathRandom => "math-random",
        }
    }

    /// What a program is told when it meets this absence.
    #[must_use]
    pub fn message(self) -> &'static str {
        match self {
            Absence::Await => "await is not in this profile",
            Absence::PromiseFinally => "Promise.prototype.finally is not in this profile",
            Absence::Eval => "eval compiles source at run time and is not in this profile",
            Absence::FunctionConstructor => {
                "the Function constructor compiles source at run time and is not in this profile"
            }
            Absence::StringNormalize => "String.prototype.normalize is not in this profile",
            Absence::MathRandom => {
                "Math.random needs a per-realm entropy source and is not in this profile"
            }
        }
    }

    /// The cargo feature that FILLS this gap, for the gaps a build can choose to fill.
    ///
    /// THIS IS DELIBERATELY NOT `cfg`-AWARE. It answers "is this entry knob-controlled at all",
    /// which is a property of the engine and the same in every build -- so the published absence
    /// list beside this crate is ONE document rather than one per feature combination, and a reader
    /// building the engine themselves can see which entries they are able to make go away.
    /// [`Absence::refusable`] is the half that knows about the build you are running.
    #[must_use]
    pub fn knob(self) -> Option<&'static str> {
        match self {
            Absence::Eval => Some("eval"),
            _ => None,
        }
    }

    /// Whether this gap exists in **the binary you are running**.
    ///
    /// A TEST THAT DEMANDS A REFUSAL MUST ASK THIS FIRST. "Every listed absence actually
    /// refuses" stopped being true the day a feature could fill one, and the failure looked like
    /// three unrelated broken tests rather than like a list that had become configuration-dependent.
    ///
    /// The default for a new variant is `true`, which is the safe direction: a gap nobody has
    /// wired a knob to is a gap in every build.
    #[must_use]
    pub fn refusable(self) -> bool {
        match self {
            Absence::Eval => cfg!(not(feature = "eval")),
            _ => true,
        }
    }

    /// Every absence, in published order. The generator walks this; nothing greps.
    ///
    /// THIS STAYS THE COMPLETE SET OF VARIANTS AND IS NEVER FILTERED BY BUILD. It is what the
    /// count assertion in `the_published_absences` checks against the enum, and a `cfg`-shortened
    /// array would weaken the one guard standing between a new variant and a list that omits it.
    /// Filter with [`Absence::refusable`] at the point of use instead.
    #[must_use]
    pub fn all() -> &'static [Absence] {
        &[
            Absence::Await,
            Absence::PromiseFinally,
            Absence::Eval,
            Absence::FunctionConstructor,
            Absence::StringNormalize,
            Absence::MathRandom,
        ]
    }
}

/// A feature this profile does not implement at PARSE time, refused with a diagnostic.
///
/// These are a different mechanism from [`Absence`] and belong on the same published list: a
/// reader asking "what does this engine not do?" does not care which phase said no. Keeping them in
/// two places is how one of them goes stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyntaxAbsence {
    Class(&'static str),
    /// A `yield` that is not a statement of its own: one in an OPERAND, inside a loop or a `try`,
    /// or a delegating `yield*`.
    ///
    /// **SUSPENSION IS PRESENT.** `function* g() { a; yield x; b; }` is rewritten into a state
    /// machine and runs one step per `next()`, and the generator object, its protocol and all four
    /// states behave as the standard says. What is absent is every shape that needs machinery the
    /// dispatch does not have:
    ///
    /// - **an operand** -- `var a = yield x`, `f(yield x)` -- needs the partly-evaluated operand
    ///   stack spilled into the frame, because `a + (yield b)` suspends with `a` already computed;
    /// - **a loop** -- the loop's own state has to become states of the machine;
    /// - **a `try`** -- the handler stack has to be re-entered on resumption, and a `finally` around
    ///   a `yield` must run when the consumer calls `return()` rather than only on the normal path;
    /// - **`yield*`** -- a forwarding loop over the inner iterator, with the `IteratorClose` rules.
    ///
    /// Each is refused where it is written rather than mis-executed, and by the transform itself:
    /// a shape is supported exactly when the transform rewrites it, so this list cannot drift from
    /// what the engine does.
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
                "a `yield` in an operand, inside a loop or a `try`, or a delegating `yield*` -- a \
                 `yield` between statements is rewritten into a state machine and runs"
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

/// The published absence list, generated from the same types the engine refuses with.
///
/// **A HAND-WRITTEN LIST IS A CLAIM, AND A CLAIM ABOUT WHAT AN ENGINE DOES NOT DO GOES STALE THE
/// FIRST TIME SOMEBODY IMPLEMENTS AN ENTRY AND FORGETS.** This walks [`Absence`] and
/// [`SyntaxAbsence`] -- the very types a refusal must go through -- so the list cannot disagree with
/// the code. The profile's own exclusions are named here too but generated by the harness, because
/// they are selection rules rather than engine behaviour, and a reader asking "what does this engine
/// not do?" should not have to know which mechanism said no.
#[must_use]
pub fn published_list() -> String {
    let mut out = String::new();
    out.push_str("# What this ECMAScript profile does not implement\n\n");
    out.push_str(
        "GENERATED by `cargo run --example publish-absences`. Do not edit: a test regenerates this\n\
         and compares, so an edit here fails the gate rather than misinforming a reader.\n\n\
         The rule this list exists to keep honest: **a feature is either absent AND LISTED, or it is\n\
         correct.** There is no third category. Anything not named here and not working is a defect,\n\
         not a gap -- which is the whole point of writing it down.\n\n",
    );

    out.push_str("## Refused when the program RUNS\n\n");
    out.push_str(
        "Each throws an error whose kind no `negative.type` in Test262 names, so a run stopped by\n\
         one is attributable and can never be scored as a pass.\n\n\
         An entry marked **build knob** is absent in the DEFAULT build and present in one built\n\
         with the named cargo feature. This document describes the default; it is generated the\n\
         same way whichever features are on, so it never quietly changes underneath a reader.\n\n",
    );
    for absence in Absence::all() {
        match absence.knob() {
            None => out.push_str(&format!("- `{}` -- {}\n", absence.id(), absence.message())),
            Some(knob) => out.push_str(&format!(
                "- `{}` -- {} **(build knob: `--features {}` fills this gap.)**\n",
                absence.id(),
                absence.message(),
                knob
            )),
        }
    }

    out.push_str("\n## Refused when the program is PARSED\n\n");
    out.push_str(
        "Each is reported as a `NotInProfile` diagnostic that CONSUMES its construct, so one\n\
         absence produces one message rather than a trail of unrelated complaints.\n\n",
    );
    for absence in SyntaxAbsence::all() {
        out.push_str(&format!("- `{}` -- {}\n", absence.id(), absence.reason()));
    }

    out.push_str("\n## Refused when a REGULAR EXPRESSION is compiled\n\n");
    for absent in lamella_regexp::js::ErrorKind::absences() {
        out.push_str(&format!("- `{}` -- {}\n", absent.id, absent.reason));
    }

    out.push_str("\n## Excluded from the corpus before a test is RUN\n\n");

    out.push_str("\n## DEVIATIONS: answered, but not the way the standard says\n\n");

    out.push_str("## What is NOT on this list, on purpose\n\n");
    out.push_str(
        "An engine INVARIANT -- a condition meaning this engine is broken -- is not an absence and\n\
         is thrown under a different kind. Five of them were being reported as absences before this\n\
         list existed, which would have published our own defect as a gap we had chosen.\n",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// THE IDS ARE SEARCH KEYS AND MUST BE UNIQUE, or the published list has two rows a reader
    /// cannot tell apart and a grep for one finds the other.
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
    /// be refusable in every build -- if one is not, somebody added a `cfg` arm and did not name
    /// the feature, and the published list is silently describing a build nobody asked for.
    ///
    /// This has teeth only where a feature is ON: in the default build every `refusable()` is
    /// true and the assertion is vacuous, which is exactly the shape that fails both ways. The
    /// `--features eval` suite is therefore part of the gate, not an extra.
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



