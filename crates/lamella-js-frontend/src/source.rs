//! Source text, and the two things every later stage depends on getting right first: **where a
//! character is** and **whether a line ended before it**.

use crate::{vec, Vec};

/// A half-open range of the source, in UTF-8 byte offsets.
///
/// Bytes rather than code-point indices because every consumer that turns a span back into text --
/// a diagnostic, a `Function.prototype.toString`, a template's raw value -- slices the original
/// string, and a code-point index would have to be converted back at every one of those sites.
/// Line and column are computed on demand from a [`LineIndex`], which is where code points matter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    #[must_use]
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    #[must_use]
    pub fn text<'a>(&self, source: &'a str) -> &'a str {
        &source[self.start..self.end]
    }
}

/// A 1-based line and column, for humans and for line tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Position {
    pub line: u32,
    /// **Counted in code POINTS**, so an astral character advances the column by one rather than by
    /// the two UTF-16 units or four UTF-8 bytes it occupies. A column that counts bytes puts a
    /// diagnostic's caret in the wrong place the first time a program contains a non-ASCII string.
    pub column: u32,
}

/// Line starts, for turning a byte offset into a [`Position`].
///
/// Built once per source rather than tracked incrementally during scanning: the scanner backtracks
/// (arrow heads, `async`, `let` all re-scan), and a counter that has to be unwound on every
/// backtrack is a counter that gets unwound wrongly.
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    #[must_use]
    pub fn new(source: &str) -> Self {
        let mut starts = vec![0];
        let bytes = source.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            match bytes[i] {
                b'\n' => {
                    starts.push(i + 1);
                    i += 1;
                }
                b'\r' => {
                    let width = if bytes.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
                    starts.push(i + width);
                    i += width;
                }
                0xE2 if bytes.get(i + 1) == Some(&0x80)
                    && matches!(bytes.get(i + 2), Some(0xA8 | 0xA9)) =>
                {
                    starts.push(i + 3);
                    i += 3;
                }
                _ => i += 1,
            }
        }
        Self { starts }
    }

    /// The 1-based line and column of a byte offset.
    #[must_use]
    pub fn position(&self, source: &str, offset: usize) -> Position {
        let line = match self.starts.binary_search(&offset) {
            Ok(exact) => exact,
            Err(next) => next - 1,
        };
        let line_start = self.starts[line];
        let column = source
            .get(line_start..offset)
            .map_or(0, |text| text.chars().count());
        Position { line: line as u32 + 1, column: column as u32 + 1 }
    }
}

/// Whether a code point ends a line.
///
/// `LineTerminator :: <LF> | <CR> | <LS> | <PS>`. Used for comment termination and for the
/// line-terminator-before-this-token flag that automatic semicolon insertion reads. **Not** the
/// predicate for what a string literal may contain -- see the module note.
#[must_use]
pub fn is_line_terminator(ch: char) -> bool {
    matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// Whether a code point is `WhiteSpace`.
///
/// `<TAB> <VT> <FF> <ZWNBSP>` plus everything in Unicode category **Zs**. The Zs members are
/// enumerated rather than looked up in a table because there are eleven of them and they are stable
/// -- unlike ID_Start/ID_Continue, which are tens of thousands of code points and are the reason
/// `unicode.rs` exists. **U+FEFF is whitespace anywhere, not just at the start**: it is
/// `<ZWNBSP>`, and treating it as a byte-order mark only at offset zero rejects legal programs.
#[must_use]
pub fn is_whitespace(ch: char) -> bool {
    matches!(
        ch,
        '\u{0009}'
        | '\u{000B}'
        | '\u{000C}'
        | '\u{FEFF}'
        | '\u{0020}' | '\u{00A0}' | '\u{1680}'
        | '\u{2000}'..='\u{200A}'
        | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

/// A code-point cursor over the source.
///
/// Cheap to copy, so a backtracking point is a `let saved = *cursor;` and a restore is
/// `*cursor = saved;`. That is the landmine map's *"re-lex-at-goal API rather than buffering
/// tokens"*: nothing is buffered, so nothing has to be invalidated when the parser changes its mind
/// about which goal symbol it wanted.
#[derive(Debug, Clone, Copy)]
pub struct Cursor<'a> {
    source: &'a str,
    offset: usize,
}

impl<'a> Cursor<'a> {
    #[must_use]
    pub fn new(source: &'a str) -> Self {
        Self { source, offset: 0 }
    }

    #[must_use]
    pub fn source(&self) -> &'a str {
        self.source
    }

    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    #[must_use]
    pub fn at_end(&self) -> bool {
        self.offset >= self.source.len()
    }

    /// The next code point without consuming it.
    #[must_use]
    pub fn peek(&self) -> Option<char> {
        self.source[self.offset..].chars().next()
    }

    /// The code point after the next one. Enough for every mandated two-character lookahead in the
    /// lexical grammar (`?.` before a digit, `0x`, `\u`, `<!--`); deeper lookahead is the parser's
    /// job and is done by saving and restoring a cursor.
    #[must_use]
    pub fn peek_second(&self) -> Option<char> {
        let mut chars = self.source[self.offset..].chars();
        chars.next();
        chars.next()
    }

    #[must_use]
    pub fn peek_nth(&self, n: usize) -> Option<char> {
        self.source[self.offset..].chars().nth(n)
    }

    /// Consumes and returns the next code point.
    pub fn next(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.offset += ch.len_utf8();
        Some(ch)
    }

    /// Consumes `expected` if it is next.
    pub fn eat(&mut self, expected: char) -> bool {
        if self.peek() == Some(expected) {
            self.offset += expected.len_utf8();
            true
        } else {
            false
        }
    }

    /// Consumes one `LineTerminatorSequence`, treating **CR LF as one**.
    ///
    /// The distinction is not cosmetic: a template literal normalizes `\r\n` to a single `\n` in
    /// both its cooked and its raw value, and a string literal's line continuation swallows the
    /// whole sequence. Two terminators where the standard says one changes both values.
    pub fn eat_line_terminator_sequence(&mut self) -> bool {
        match self.peek() {
            Some('\r') => {
                self.offset += 1;
                if self.peek() == Some('\n') {
                    self.offset += 1;
                }
                true
            }
            Some(ch) if is_line_terminator(ch) => {
                self.offset += ch.len_utf8();
                true
            }
            _ => false,
        }
    }

    /// Rewinds to a previously observed offset. Used only to restore a saved cursor.
    pub fn reset_to(&mut self, offset: usize) {
        self.offset = offset;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_four_line_terminators_end_a_line() {
        for ch in ['\n', '\r', '\u{2028}', '\u{2029}'] {
            assert!(is_line_terminator(ch), "{ch:?} is a LineTerminator");
        }
        assert!(!is_line_terminator('\u{0085}'), "NEL is not one, though it looks like it should be");
    }

    /// THE DEFECT THIS GUARDS: counting CR and LF separately makes every line number after the first
    /// one too high, on every file written on Windows -- an off-by-a-constant defect that survives
    /// because nothing looks obviously wrong.
    #[test]
    fn crlf_is_one_line_not_two() {
        let source = "a\r\nb\r\nc";
        let index = LineIndex::new(source);
        assert_eq!(index.position(source, 0).line, 1);
        assert_eq!(index.position(source, source.find('b').unwrap()).line, 2);
        assert_eq!(index.position(source, source.find('c').unwrap()).line, 3);
    }

    #[test]
    fn the_exotic_line_terminators_advance_the_line_too() {
        let source = "a\u{2028}b\u{2029}c";
        let index = LineIndex::new(source);
        assert_eq!(index.position(source, source.find('b').unwrap()).line, 2);
        assert_eq!(index.position(source, source.find('c').unwrap()).line, 3);
    }

    /// A column counted in bytes puts the caret in the wrong place the first time a line contains a
    /// non-ASCII character, which is exactly when a reader most needs it to be right.
    #[test]
    fn columns_count_code_points_not_bytes() {
        let source = "let \u{1F600} = 1;";
        let index = LineIndex::new(source);
        let equals = source.find('=').unwrap();
        assert_eq!(index.position(source, equals).column, 7, "l,e,t,space,emoji,space -> col 7");
    }

    #[test]
    fn zwnbsp_is_whitespace_anywhere_not_a_byte_order_mark_at_the_start() {
        assert!(is_whitespace('\u{FEFF}'));
        for ch in ['\u{00A0}', '\u{3000}', '\u{2009}', '\t', '\u{000B}', '\u{000C}'] {
            assert!(is_whitespace(ch), "{ch:?} is WhiteSpace");
        }
        assert!(!is_whitespace('\n'), "a line terminator is not whitespace; it is its own category");
    }

    #[test]
    fn a_cursor_walks_code_points() {
        let mut cursor = Cursor::new("a\u{1F600}b");
        assert_eq!(cursor.next(), Some('a'));
        assert_eq!(cursor.next(), Some('\u{1F600}'));
        assert_eq!(cursor.offset(), 5, "one ASCII byte plus four");
        assert_eq!(cursor.next(), Some('b'));
        assert!(cursor.at_end());
    }

    #[test]
    fn crlf_is_eaten_as_one_sequence() {
        let mut cursor = Cursor::new("\r\nx");
        assert!(cursor.eat_line_terminator_sequence());
        assert_eq!(cursor.peek(), Some('x'), "both halves consumed, not just the CR");
    }

    #[test]
    fn a_saved_cursor_restores_exactly() {
        let mut cursor = Cursor::new("abc");
        let saved = cursor;
        cursor.next();
        cursor.next();
        cursor = saved;
        assert_eq!(cursor.peek(), Some('a'), "backtracking is a copy, so nothing can be missed");
    }
}
