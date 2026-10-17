//! This tool's own lines during a run, told apart from the program's.
//!
//! A run gives the program's standard output to standard output, and nothing else. This tool's own
//! lines -- what it read, what it wrote and where, how the program ended -- go to standard error,
//! where the program's standard error goes as well, so each of them is prefixed `lamella:`: a reader
//! of standard error tells the two apart by it, and a caller keeping standard output in a file keeps
//! the program's output alone.

use std::io::{self, Write};

/// The prefix every line of this tool's own carries.
const PREFIX: &str = "lamella:";

/// Says `text` on `err` as this tool's own words: every line prefixed `lamella:`, a blank one too.
pub(crate) fn say(err: &mut dyn Write, text: &str) {
    for line in text.lines() {
        let _ = said_line(err, line);
    }
    let _ = err.flush();
}

/// One line of this tool's own, prefixed.
fn said_line(err: &mut dyn Write, line: &str) -> io::Result<()> {
    if line.is_empty() { writeln!(err, "{PREFIX}") } else { writeln!(err, "{PREFIX} {line}") }
}

/// A writer that says every line written to it as this tool's own, on `inner`.
///
/// For a routine that writes its progress to a writer it is given: each line is said once it is
/// whole, and a line left without its end is said as it stands when the writer is dropped.
#[cfg_attr(not(feature = "bake"), allow(dead_code))]
pub(crate) struct OwnLines<W: Write> {
    inner: W,
    /// The line written so far, without its end.
    partial: Vec<u8>,
}

impl<W: Write> OwnLines<W> {
    /// A writer saying its lines on `inner`.
    #[cfg_attr(not(feature = "bake"), allow(dead_code))]
    pub(crate) fn new(inner: W) -> Self {
        OwnLines { inner, partial: Vec::new() }
    }

    /// Says the line written so far, and starts the next.
    fn end_line(&mut self) -> io::Result<()> {
        let line = String::from_utf8_lossy(&self.partial).into_owned();
        self.partial.clear();
        said_line(&mut self.inner, line.strip_suffix('\r').unwrap_or(&line))
    }
}

impl<W: Write> Write for OwnLines<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for &byte in bytes {
            if byte == b'\n' {
                self.end_line()?;
            } else {
                self.partial.push(byte);
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Write> Drop for OwnLines<W> {
    fn drop(&mut self) {
        if !self.partial.is_empty() {
            let _ = self.end_line();
        }
        let _ = self.inner.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **EVERY LINE OF THIS TOOL'S IS MARKED**, a blank one too.
    #[test]
    fn every_line_this_tool_says_is_marked() {
        let mut err = Vec::new();
        say(&mut err, "first\n\nthird");
        assert_eq!(String::from_utf8(err).expect("text"), "lamella: first\nlamella:\nlamella: third\n");
    }

    /// **A LINE WRITTEN IN PIECES IS SAID ONCE, WHOLE**, a carriage return before its end is not
    /// kept, and a line left open is said when the writer goes.
    #[test]
    fn a_writer_says_each_whole_line_once_and_the_last_when_it_goes() {
        let mut err = Vec::new();
        {
            let mut lines = OwnLines::new(&mut err);
            write!(lines, "deploying 40000 B").expect("written");
            writeln!(lines, " to usb").expect("written");
            lines.write_all(b"running on usb\r\n\nleft open").expect("written");
        }
        assert_eq!(
            String::from_utf8(err).expect("text"),
            "lamella: deploying 40000 B to usb\nlamella: running on usb\nlamella:\nlamella: left open\n"
        );
    }
}
