//! The shared pattern tree a front end produces and the compiler consumes.

use crate::{Box, Vec};

/// One member of a character class: a single character, or an inclusive range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassEntry {
    Single(u32),
    /// Inclusive at both ends. A front end emits `lo <= hi`; the compiler does not reorder.
    Range(u32, u32),
}

impl ClassEntry {
    #[must_use]
    pub fn contains(self, ch: u32) -> bool {
        match self {
            ClassEntry::Single(value) => value == ch,
            ClassEntry::Range(lo, hi) => lo <= ch && ch <= hi,
        }
    }
}

/// A zero-width test.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assertion {
    /// Start of input, or of a line when the front end compiled with multiline semantics.
    Start,
    End,
    WordBoundary,
    NotWordBoundary,
}

/// What a repetition does when the body could match nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Greed {
    /// Prefers to match as many times as possible, giving back on failure.
    Greedy,
    /// Prefers to match as few times as possible, taking more on failure.
    Lazy,
}

/// A node of the shared pattern tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Node {
    /// Matches at any position without consuming, which is what an empty alternative is.
    Empty,

    /// A single character, already canonicalized by the front end if the pattern is
    /// case-insensitive.
    Char(u32),

    /// A set of characters. `negated` inverts membership AFTER the entries are consulted, so an
    /// entry list stays a plain description of what is named.
    Class { entries: Vec<ClassEntry>, negated: bool },

    /// The dot. `dot_all` is resolved by the front end into whether the line terminators are
    /// members, so the compiler does not need to know a flag letter.
    Any { dot_all: bool },

    /// Matched in order, left to right.
    Concat(Vec<Node>),

    /// Tried in order. ORDER IS OBSERVABLE: the first alternative that leads to an overall match
    /// wins, which is why this is a list and not a set.
    Alternate(Vec<Node>),

    /// `min` repetitions are required and `max` bounds the total, unbounded when `None`.
    Repeat { node: Box<Node>, min: u32, max: Option<u32>, greed: Greed },

    /// A group. `index` is `Some` for a capturing group, numbered from one.
    Group { index: Option<u32>, node: Box<Node> },

    Assertion(Assertion),

    /// Matches what a capturing group matched. Answers the empty string when the group has not
    /// participated, which is not the same as failing.
    Backreference(u32),

    /// A lookaround. `behind` runs the body right to left; `negate` succeeds when the body fails.
    Look { behind: bool, negate: bool, node: Box<Node> },
}

impl Node {
    /// Wraps a list into the cheapest node that means the same thing.
    ///
    /// A one-element concatenation and a zero-element one are both common enough out of a parser
    /// loop that flattening them here keeps the compiler's instruction count honest.
    #[must_use]
    pub fn concat(mut parts: Vec<Node>) -> Node {
        match parts.len() {
            0 => Node::Empty,
            1 => parts.pop().unwrap_or(Node::Empty),
            _ => Node::Concat(parts),
        }
    }

    #[must_use]
    pub fn alternate(mut parts: Vec<Node>) -> Node {
        match parts.len() {
            0 => Node::Empty,
            1 => parts.pop().unwrap_or(Node::Empty),
            _ => Node::Alternate(parts),
        }
    }

    /// Whether this node can succeed without consuming anything.
    ///
    /// The compiler needs it to decide which repetitions require an empty-progress guard, and
    /// answering conservatively -- `true` when unsure -- costs a runtime check and never
    /// correctness. Answering `false` wrongly would let a pattern loop forever.
    #[must_use]
    pub fn matches_empty(&self) -> bool {
        match self {
            Node::Empty | Node::Assertion(_) | Node::Look { .. } => true,
            Node::Backreference(_) => true,
            Node::Char(_) | Node::Class { .. } | Node::Any { .. } => false,
            Node::Concat(parts) => parts.iter().all(Node::matches_empty),
            Node::Alternate(parts) => parts.iter().any(Node::matches_empty),
            Node::Repeat { node, min, .. } => *min == 0 || node.matches_empty(),
            Node::Group { node, .. } => node.matches_empty(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_range_is_inclusive_at_both_ends() {
        let entry = ClassEntry::Range(b'a' as u32, b'c' as u32);
        assert!(entry.contains(b'a' as u32));
        assert!(entry.contains(b'c' as u32));
        assert!(!entry.contains(b'd' as u32));
    }

    /// The guard the compiler needs: a body that can match nothing is the one that can loop.
    #[test]
    fn nullability_is_answered_through_the_structure() {
        assert!(Node::Empty.matches_empty());
        assert!(!Node::Char(b'a' as u32).matches_empty());

        let optional = Node::Repeat {
            node: Box::new(Node::Char(b'a' as u32)),
            min: 0,
            max: None,
            greed: Greed::Greedy,
        };
        assert!(optional.matches_empty(), "zero repetitions is a match of nothing");

        let required = Node::Repeat {
            node: Box::new(Node::Char(b'a' as u32)),
            min: 1,
            max: None,
            greed: Greed::Greedy,
        };
        assert!(!required.matches_empty());
    }

    /// A concatenation is nullable only when EVERY part is; an alternation when ANY part is. Using
    /// one rule for both is the mistake this test exists to catch.
    #[test]
    fn concatenation_and_alternation_answer_nullability_differently() {
        let parts = crate::vec![Node::Empty, Node::Char(b'a' as u32)];
        assert!(!Node::Concat(parts.clone()).matches_empty());
        assert!(Node::Alternate(parts).matches_empty());
    }

    /// A backreference to a group that never participated matches the empty string, so a repetition
    /// over one can spin unless it is treated as nullable.
    #[test]
    fn a_backreference_is_nullable_because_a_missing_group_matches_nothing() {
        assert!(Node::Backreference(1).matches_empty());
    }
}
