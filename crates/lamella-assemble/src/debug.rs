//! Mapping source byte offsets to line and column, for debug info.

use alloc::vec;
use alloc::vec::Vec;
use lamella_syntax::lexer::is_new_line;
use lamella_syntax::span::Span;

/// The line at a position, plus the start and end line/column of a span.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpanLines {
    /// 1-based line of the span's start.
    pub start_line: u32,
    /// 1-based column of the span's start.
    pub start_column: u32,
    /// 1-based line of the span's end.
    pub end_line: u32,
    /// 1-based column of the span's end.
    pub end_column: u32,
}

/// An index of where each line begins, for resolving byte offsets to line/column.
pub struct LineMap {
    /// `line_starts[n]` is the byte offset where line `n` (0-based) begins.
    line_starts: Vec<u32>,
}

impl LineMap {
    /// Builds the line index for `source`. Every ECMA-334 9.3.2 line terminator opens a
    /// new line -- a line feed, a lone carriage return, a CR+LF pair (counted once), and
    /// the Unicode next-line (U+0085), line-separator (U+2028), and paragraph-separator
    /// (U+2029) -- the same set the lexer recognizes, so the PDB's line numbers agree with
    /// csc even on a source that ends a comment with a NEL or a Unicode separator.
    #[must_use]
    pub fn new(source: &str) -> LineMap {
        let mut line_starts = vec![0];
        let mut chars = source.char_indices().peekable();
        while let Some((index, ch)) = chars.next() {
            if !is_new_line(ch) {
                continue;
            }
            let next_line_start = if ch == '\r' && chars.peek().map(|&(_, c)| c) == Some('\n') {
                let (lf_index, _) = chars.next().expect("peeked a line feed");
                lf_index + 1
            } else {
                index + ch.len_utf8()
            };
            line_starts.push(next_line_start as u32);
        }
        LineMap { line_starts }
    }

    /// The 1-based line and column of `offset` in `source`.
    #[must_use]
    pub fn position(&self, source: &str, offset: u32) -> (u32, u32) {
        let offset = offset.min(source.len() as u32);
        let line_index = match self.line_starts.binary_search(&offset) {
            Ok(index) => index,
            Err(index) => index - 1,
        };
        let line_start = self.line_starts[line_index];
        let column = source[line_start as usize..offset as usize]
            .encode_utf16()
            .count() as u32
            + 1;
        (line_index as u32 + 1, column)
    }

    /// Resolves a span to the line/column of both its ends.
    #[must_use]
    pub fn span_lines(&self, source: &str, span: Span) -> SpanLines {
        let (start_line, start_column) = self.position(source, span.start);
        let (end_line, end_column) = self.position(source, span.end);
        SpanLines {
            start_line,
            start_column,
            end_line,
            end_column,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_are_one_based_line_and_column() {
        let source = "class C\n{\n    int x;\n}\n";
        let map = LineMap::new(source);
        assert_eq!(map.position(source, 0), (1, 1));
        let brace = source.find('{').unwrap() as u32;
        assert_eq!(map.position(source, brace), (2, 1));
        let int = source.find("int").unwrap() as u32;
        assert_eq!(map.position(source, int), (3, 5));
    }

    #[test]
    fn counts_every_ecma334_line_terminator() {
        for source in ["a\nb", "a\rb", "a\u{0085}b", "a\u{2028}b", "a\u{2029}b"] {
            let map = LineMap::new(source);
            let b = source.find('b').unwrap() as u32;
            assert_eq!(map.position(source, b).0, 2, "a terminator opens line 2");
        }
        let crlf = "a\r\nb\r\nc";
        let map = LineMap::new(crlf);
        assert_eq!(map.position(crlf, crlf.find('c').unwrap() as u32).0, 3);
        let nel = "xy\u{0085}zw";
        let map = LineMap::new(nel);
        assert_eq!(map.position(nel, nel.find('w').unwrap() as u32), (2, 2));
    }

    #[test]
    fn columns_count_utf16_code_units() {
        let source = "a = \"\u{1F600}\"; b";
        let map = LineMap::new(source);
        let b = source.find('b').unwrap() as u32;
        assert_eq!(map.position(source, b), (1, 11));
    }

    #[test]
    fn span_lines_resolve_both_ends() {
        let source = "a\nbc;\n";
        let map = LineMap::new(source);
        let start = source.find("bc").unwrap() as u32;
        let lines = map.span_lines(source, Span::new(start, start + 2));
        assert_eq!(
            lines,
            SpanLines {
                start_line: 2,
                start_column: 1,
                end_line: 2,
                end_column: 3,
            }
        );
    }
}
