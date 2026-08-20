//! The compiled program: a flat instruction list the matcher walks.

use crate::ast::{Assertion, ClassEntry};
use crate::Vec;

/// Which way a consuming instruction moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Forward,
    /// Inside a lookbehind. The instruction reads the character ENDING at the current position and
    /// moves the position down.
    Backward,
}

/// How a comparison canonicalizes case, RESOLVED by the front end.
///
/// # WHY THIS IS THREE STATES AND NOT A `bool`
///
/// A `bool` here was a silent wrong answer, and the reason is that ECMAScript canonicalizes case
/// two different ways depending on a flag the matcher must never see. Under `u` the standard folds
/// -- `Canonicalize` is Unicode simple case folding. Without `u` it MAPS, through
/// `toUppercase`, and the two do not agree even on which characters are equivalent: `ss` and its
/// capital U+1E9E fold together and uppercase apart, and U+017F folds to `s` while the mapping rule
/// explicitly refuses to carry a non-ASCII character into ASCII.
///
/// So a single "ignore case" flag cannot select the right behaviour, and the one that was here
/// selected an ASCII fold for BOTH -- correct for ASCII in both modes, and wrong for everything
/// else in one of them. Resolving it in the front end keeps the matcher free of flags, which is the
/// same rule [`Instruction::Any`] follows for `dotAll`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Fold {
    /// Compare code points exactly.
    #[default]
    None,
    /// ECMAScript `i` WITHOUT `u`: fold the ASCII letters and nothing else.
    ///
    /// Canonicalization in this mode is defined by case MAPPING, which needs a `toUppercase`
    /// table. The shared Unicode home ships case PROPERTIES and fold tables and no mapping data,
    /// so the ASCII range is folded exactly and nothing outside it is folded at all.
    ///
    /// **WARNING: THIS ARM CHANGES WHAT A PATTERN MATCHES.** It is not interchangeable with
    /// [`Fold::Simple`], and must not be selected to save space.
    /// [`Fold::Simple`] in this mode would match U+03A3 with U+03C3, and would ALSO match U+017F
    /// with `s` and U+212A with `k` -- pairs the mapping rule specifically separates, because it
    /// returns the character unchanged when a non-ASCII input would uppercase into ASCII. **Those
    /// two are right under the ASCII rule.** So the ASCII range is done exactly and the rest is
    /// named rather than approximated, which is the same choice the front end makes when it
    /// refuses a literal it cannot fold.
    Ascii,
    /// ECMAScript `i` WITH `u`: Unicode simple case folding, which is what the standard's
    /// `Canonicalize` is under that flag. Complete, over the whole code space.
    Simple,
}

impl Fold {
    /// Whether two code points are the same character under this canonicalization.
    #[must_use]
    pub fn same(self, left: u32, right: u32) -> bool {
        if left == right {
            return true;
        }
        match self {
            Fold::None => false,
            Fold::Ascii => fold_ascii(left) == fold_ascii(right),
            Fold::Simple => {
                let fold = |ch: u32| lamella_unicode::simple_case_fold(ch).unwrap_or(ch);
                fold(left) == fold(right)
            }
        }
    }
}

fn fold_ascii(ch: u32) -> u32 {
    if (0x41..=0x5A).contains(&ch) {
        ch + 0x20
    } else {
        ch
    }
}

/// One instruction.
///
/// Program counters are indices into [`Program::instructions`]. They are `u32` rather than `usize`
/// because a program that needs more than four billion instructions is not one this engine intends
/// to run, and the narrower field halves the size of the hottest structure in the crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    /// Consumes one character and requires it to equal `ch`.
    Char { ch: u32, direction: Direction },

    /// Consumes one character and requires membership in a class-table range.
    ///
    /// The entries live in [`Program::classes`] as a shared table rather than inline, so a class
    /// repeated across a pattern is stored once and the instruction stays small.
    Class { start: u32, len: u32, negated: bool, direction: Direction },

    /// The dot. `dot_all` decides whether the line terminators are members, resolved by the front
    /// end so the matcher never sees a flag.
    Any { dot_all: bool, direction: Direction },

    /// Tries `first`; on failure resumes at `second` with the position it had here.
    ///
    /// The ORDER of the two is how greed is expressed: a greedy repetition puts the body first, a
    /// lazy one puts the exit first, and nothing else about them differs.
    Split { first: u32, second: u32 },

    Jump(u32),

    /// Records the current position into a capture slot. Slot `2n` is group `n`'s start and slot
    /// `2n + 1` its end; slots 0 and 1 are the whole match.
    Save { slot: u32 },

    /// Sets a contiguous run of capture slots back to "did not participate".
    ClearCaptures { from: u32, to: u32 },

    Assert { assertion: Assertion, multiline: bool },

    /// Matches the text a capturing group matched.
    ///
    /// A group that has not participated matches the EMPTY STRING and does not fail, which is the
    /// rule that makes a backreference nullable.
    Backreference { group: u32, direction: Direction, fold: Fold },

    /// Records the current position into a progress register, for the empty-iteration guard.
    Mark { register: u32 },

    /// Fails the current path when an iteration consumed nothing.
    ///
    /// The check is skipped while the counter is still below `min`, and that is the standard's rule
    /// rather than an optimization: a repetition with a required minimum is bounded by that
    /// minimum, so it cannot spin, and `/(?:){3}/` is required to run its three empty iterations
    /// rather than fail on the first. The guard exists only for the unbounded tail.
    Progress { register: u32, counter: u32, min: u32 },

    /// Sets a counter to zero.
    CounterInit { counter: u32 },

    /// The head of a counted repetition: decides between entering the body and leaving.
    ///
    /// It is one instruction rather than a comparison and two jumps because the decision reads
    /// three things at once -- the count, the bounds, and the greed -- and splitting it across
    /// instructions would put a partially-updated counter on the backtrack stack.
    CounterSplit { counter: u32, min: u32, max: Option<u32>, body: u32, exit: u32, greedy: bool },

    /// The tail of a counted repetition: increments and returns to the head.
    CounterNext { counter: u32, head: u32 },

    /// Runs a sub-program as an atomic assertion.
    ///
    /// On success the matcher continues at the instruction after this one, keeping any captures the
    /// body made when `negate` is false and discarding them when it is true. There is no
    /// backtracking INTO a lookaround: it is tried once and its own choices are not revisited,
    /// which is what makes it an assertion rather than a group.
    Look { negate: bool, body: u32 },

    /// Ends a lookaround body successfully.
    LookEnd,

    /// The whole pattern matched.
    Match,
}

/// A compiled pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub instructions: Vec<Instruction>,
    /// The class ranges every [`Instruction::Class`] indexes into.
    pub classes: Vec<ClassEntry>,
    /// Capture slots, which is twice the group count plus the two for the whole match.
    pub slots: usize,
    /// Counter registers used by counted repetitions.
    pub counters: usize,
    /// Progress registers used by empty-iteration guards.
    pub registers: usize,
}

impl Program {
    /// The number of capturing groups, not counting the whole match.
    #[must_use]
    pub fn group_count(&self) -> usize {
        self.slots.saturating_sub(2) / 2
    }
}
