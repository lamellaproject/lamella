//! Showing a run's streamed output in a terminal as it arrives.
//!
//! [`TerminalOutput`] writes each [`OutputChunk`] as a collector folds it
//! ([`RunCollector::poll_streaming`]) and flushes before returning, so a program that prints and
//! then waits is visibly waiting rather than silent. It applies the two rules the output header
//! exists for: a chunk on a different stream starts a new line when the previous chunk left one
//! open, and output the target dropped is marked at the point where it was lost.

use std::io::{self, Write};

use lamella_runner::debug::{output, reason};
use lamella_runner::{OutputChunk, RunCollector};

/// The line [`TerminalOutput`] writes, to the error writer, where the target dropped output.
pub const DROPPED_MARKER: &str = "[output dropped]";

/// Renders a run's streamed output to two writers as it arrives: the program's standard output to
/// one, and its standard error and debug channel to the other.
///
/// A stream a board family defines for itself goes with standard output, as [`RunCollector`]
/// folds it.
pub struct TerminalOutput<O: Write, E: Write> {
    out: O,
    err: E,
    /// The stream whose line the last chunk left open, if any.
    open_line: Option<u8>,
}

impl<O: Write, E: Write> TerminalOutput<O, E> {
    /// A renderer writing standard output to `out`, and standard error and the debug channel to
    /// `err`.
    pub fn new(out: O, err: E) -> Self {
        Self { out, err, open_line: None }
    }

    /// Writes one chunk and flushes the writer it went to.
    ///
    /// `received_ms`, when given, prefixes the chunk with `[+N ms] ` if the chunk starts a line: the
    /// time the caller received it, counted from an instant the caller chose.
    ///
    /// # Errors
    /// An I/O error from either writer.
    pub fn show(&mut self, chunk: OutputChunk<'_>, received_ms: Option<u64>) -> io::Result<()> {
        let other_stream = self.open_line.is_some_and(|open| open != chunk.stream);
        if other_stream || chunk.follows_dropped_output() {
            self.end_open_line()?;
        }
        if chunk.follows_dropped_output() {
            writeln!(self.err, "{DROPPED_MARKER}")?;
            self.err.flush()?;
        }
        if chunk.text.is_empty() {
            return Ok(());
        }
        let starts_line = self.open_line.is_none();
        let writer = self.writer_for(chunk.stream);
        if let (Some(ms), true) = (received_ms, starts_line) {
            write!(writer, "[+{ms} ms] ")?;
        }
        writer.write_all(chunk.text.as_bytes())?;
        writer.flush()?;
        self.open_line = if chunk.ends_a_line() { None } else { Some(chunk.stream) };
        Ok(())
    }

    /// Ends the line the last chunk left open, if any, so whatever is written next starts a line.
    ///
    /// # Errors
    /// An I/O error from the writer holding the open line.
    pub fn end_open_line(&mut self) -> io::Result<()> {
        if let Some(open) = self.open_line.take() {
            let writer = self.writer_for(open);
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        Ok(())
    }

    /// Reports how a run ended, on a line of its own after any line the program left open.
    ///
    /// A start the target refused goes to the error writer, naming the acknowledgement code the
    /// target gave and what it means. Otherwise standard output gets `stopped: done, exit N`,
    /// `stopped: TRAP, exit N`, `stopped: aborted`, or the reason's number for a stop this renderer
    /// has no word for.
    ///
    /// # Errors
    /// An I/O error from either writer.
    pub fn report_stop(&mut self, run: &RunCollector) -> io::Result<()> {
        self.end_open_line()?;
        if let Some(code) = run.start_refusal() {
            writeln!(self.err, "{}", crate::describe_start_refusal(Some(code)))?;
            return self.err.flush();
        }
        match (run.stop_reason(), run.exit_value()) {
            (Some(reason::DONE), Some(exit)) => writeln!(self.out, "stopped: done, exit {exit}")?,
            (Some(reason::TRAP), Some(exit)) => writeln!(self.out, "stopped: TRAP, exit {exit}")?,
            (Some(reason::ABORTED), _) => writeln!(self.out, "stopped: aborted")?,
            (Some(other), _) => writeln!(self.out, "stopped: reason {other}")?,
            (None, _) => writeln!(self.out, "stopped")?,
        }
        self.out.flush()
    }

    /// The two writers, handed back.
    pub fn into_writers(self) -> (O, E) {
        (self.out, self.err)
    }

    /// The writer a stream's text goes to.
    fn writer_for(&mut self, stream: u8) -> &mut dyn Write {
        if stream == output::STDERR || stream == output::DEBUG { &mut self.err } else { &mut self.out }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_wire::{MemTransport, Transport};

    /// A chunk carrying the line-boundary flag exactly when a target would set it.
    fn chunk(stream: u8, text: &str) -> OutputChunk<'_> {
        let flags = if text.ends_with('\n') { output::ENDS_ON_LINE_BOUNDARY } else { 0 };
        OutputChunk { stream, flags, text }
    }

    fn render(chunks: &[OutputChunk<'_>]) -> (String, String) {
        let mut terminal = TerminalOutput::new(Vec::new(), Vec::new());
        for chunk in chunks {
            terminal.show(*chunk, None).unwrap();
        }
        let (out, err) = terminal.into_writers();
        (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap())
    }

    /// A collector that has seen execution 7 stop for `why` with `exit`.
    fn stopped(why: u8, exit: i32) -> RunCollector {
        let mut target = MemTransport::new();
        let mut payload = vec![why];
        payload.extend_from_slice(&[0; 8]);
        payload.extend_from_slice(&exit.to_le_bytes());
        payload.push(0);
        target.send(lamella_runner::debug::EVT_STOPPED, 7, &payload).unwrap();
        let mut driver = MemTransport::new();
        driver.feed(&target.take_sent());
        let mut run = RunCollector::new(7);
        assert!(run.poll(&mut driver).unwrap(), "the stop ends the run");
        run
    }

    #[test]
    fn a_chunk_on_another_stream_first_ends_the_line_the_last_chunk_left_open() {
        let (out, err) = render(&[
            chunk(output::STDOUT, "working"),
            chunk(output::DEBUG, "tick\n"),
            chunk(output::STDOUT, "done\n"),
        ]);
        assert_eq!(out, "working\ndone\n");
        assert_eq!(err, "tick\n");
    }

    #[test]
    fn a_chunk_on_the_same_stream_continues_its_line() {
        let (out, err) = render(&[chunk(output::STDOUT, "work"), chunk(output::STDOUT, "ing\n")]);
        assert_eq!(out, "working\n");
        assert_eq!(err, "");
    }

    #[test]
    fn standard_error_and_the_debug_channel_share_the_error_writer_but_not_a_line() {
        let (out, err) = render(&[chunk(output::STDERR, "TRAP: "), chunk(output::DEBUG, "trace\n")]);
        assert_eq!(out, "");
        assert_eq!(err, "TRAP: \ntrace\n");
    }

    #[test]
    fn a_stream_a_board_family_defines_goes_with_standard_output() {
        let (out, err) = render(&[chunk(output::FIRST_VENDOR_STREAM, "sensor 21.5\n")]);
        assert_eq!(out, "sensor 21.5\n");
        assert_eq!(err, "");
    }

    #[test]
    fn dropped_output_is_marked_where_it_was_lost() {
        let mut after = chunk(output::STDOUT, "after\n");
        after.flags |= output::OUTPUT_DROPPED;
        let (out, err) = render(&[chunk(output::STDOUT, "before"), after]);
        assert_eq!(out, "before\nafter\n");
        assert_eq!(err, format!("{DROPPED_MARKER}\n"));
    }

    #[test]
    fn a_receive_time_prefixes_only_a_chunk_that_starts_a_line() {
        let mut terminal = TerminalOutput::new(Vec::new(), Vec::new());
        terminal.show(chunk(output::STDOUT, "a"), Some(5)).unwrap();
        terminal.show(chunk(output::STDOUT, "b\n"), Some(9)).unwrap();
        terminal.show(chunk(output::STDOUT, "c\n"), Some(12)).unwrap();
        let (out, _) = terminal.into_writers();
        assert_eq!(String::from_utf8(out).unwrap(), "[+5 ms] ab\n[+12 ms] c\n");
    }

    #[test]
    fn how_a_run_ended_is_reported_on_a_line_of_its_own() {
        let mut terminal = TerminalOutput::new(Vec::new(), Vec::new());
        terminal.show(chunk(output::STDOUT, "unfinished"), None).unwrap();
        terminal.report_stop(&stopped(reason::DONE, 3)).unwrap();
        let (out, err) = terminal.into_writers();
        assert_eq!(String::from_utf8(out).unwrap(), "unfinished\nstopped: done, exit 3\n");
        assert!(err.is_empty());

        let mut terminal = TerminalOutput::new(Vec::new(), Vec::new());
        terminal.report_stop(&stopped(reason::TRAP, 70)).unwrap();
        let (out, _) = terminal.into_writers();
        assert_eq!(String::from_utf8(out).unwrap(), "stopped: TRAP, exit 70\n");
    }

    #[test]
    fn a_refused_start_is_reported_to_the_error_writer_with_what_its_code_means() {
        use lamella_runner::exec;
        let mut target = MemTransport::new();
        target.send(exec::EXEC_ACK, 7, &[exec::ack::NOTHING_TO_RUN]).unwrap();
        let mut driver = MemTransport::new();
        driver.feed(&target.take_sent());
        let mut run = RunCollector::new(7);
        assert!(run.poll(&mut driver).unwrap(), "a refused start ends the run");

        let mut terminal = TerminalOutput::new(Vec::new(), Vec::new());
        terminal.report_stop(&run).unwrap();
        let (out, err) = terminal.into_writers();
        assert!(out.is_empty(), "nothing ran, so nothing stopped");
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("nothing at the requested source") && err.contains("NOTHING_TO_RUN"), "{err}");
    }

    /// A writer that remembers whether anything written to it is still waiting for a flush.
    #[derive(Default)]
    struct FlushTracking {
        bytes: Vec<u8>,
        unflushed: bool,
    }

    impl Write for FlushTracking {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(buf);
            self.unflushed = true;
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            self.unflushed = false;
            Ok(())
        }
    }

    #[test]
    fn every_chunk_is_flushed_before_show_returns() {
        let mut terminal = TerminalOutput::new(FlushTracking::default(), FlushTracking::default());
        terminal.show(chunk(output::STDOUT, "no newline yet"), None).unwrap();
        terminal.show(chunk(output::DEBUG, "trace\n"), None).unwrap();
        let (out, err) = terminal.into_writers();
        assert!(!out.unflushed, "standard output was left in a buffer");
        assert!(!err.unflushed, "the error writer was left in a buffer");
        assert_eq!(out.bytes, b"no newline yet\n");
    }
}
