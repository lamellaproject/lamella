//! Source positions.

use core::fmt;

/// A half-open range of byte offsets `[start, end)` into a source file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Span {
    /// Byte offset of the first byte in the span.
    pub start: u32,
    /// Byte offset one past the last byte in the span.
    pub end: u32,
}

impl Span {
    /// Creates a span covering `[start, end)`.
    ///
    /// # Panics
    /// Panics if `end < start`, which would describe a negative-length span and
    /// only happens through a caller bug.
    #[must_use]
    pub fn new(start: u32, end: u32) -> Span {
        assert!(start <= end, "span end must not precede its start");
        Span { start, end }
    }

    /// Creates an empty span at `position`, used to point at a zero-width
    /// location such as an unexpected end of file.
    #[must_use]
    pub fn empty_at(position: u32) -> Span {
        Span {
            start: position,
            end: position,
        }
    }

    /// The length of the span in bytes.
    #[must_use]
    pub fn len(self) -> u32 {
        self.end - self.start
    }

    /// Returns `true` when the span covers no bytes.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.start == self.end
    }

    /// Returns the smallest span that covers both `self` and `other`.
    #[must_use]
    pub fn cover(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }

    /// Slices the matching text out of `source`, which must be the same text the
    /// span was produced from.
    ///
    /// # Panics
    /// Panics if the span lies outside `source` or splits a UTF-8 character,
    /// which can only happen when the span and the source do not correspond.
    #[must_use]
    pub fn slice(self, source: &str) -> &str {
        &source[self.start as usize..self.end as usize]
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}..{}", self.start, self.end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_and_emptiness() {
        let span = Span::new(3, 7);
        assert_eq!(span.len(), 4);
        assert!(!span.is_empty());

        let empty = Span::empty_at(5);
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn cover_is_order_independent() {
        let left = Span::new(2, 4);
        let right = Span::new(6, 9);
        assert_eq!(left.cover(right), Span::new(2, 9));
        assert_eq!(right.cover(left), Span::new(2, 9));
    }

    #[test]
    fn slice_returns_the_covered_text() {
        let source = "class Hello";
        assert_eq!(Span::new(0, 5).slice(source), "class");
        assert_eq!(Span::new(6, 11).slice(source), "Hello");
    }

    #[test]
    #[should_panic = "span end must not precede its start"]
    fn negative_length_is_rejected() {
        let _ = Span::new(7, 3);
    }
}
