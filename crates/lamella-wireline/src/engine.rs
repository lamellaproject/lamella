//! The host-stateless REPL engine: a read-compile-eval-print core that keeps the session transcript
//! on the host and re-emits the whole session as one fresh program each submission, so the target
//! only ever runs CIL and holds no REPL state. A microcontroller running just the
//! [`crate::run_program`] runner is a complete target.

use crate::{RunResult, send_program, serve_one, try_recv_result};
use lamella_wire::{MemTransport, TransportError};

#[cfg(feature = "serial")]
use crate::{SerialTransport, eval_blocking};
#[cfg(feature = "serial")]
use std::time::Duration;

/// A source compilation failure from a [`ReplCompiler`].
#[derive(Clone, Debug)]
pub enum CompileFailure {
    /// The compiler rejected the source; the text is the diagnostics to show the user.
    Diagnostics(String),
    /// The compiler / toolchain itself failed (not a source error) -- e.g. the compiler could not be located.
    Toolchain(String),
}

/// Compiles a C# submission to a program assembly's bytes. This is the engine's COMPILE seam, the
/// symmetric counterpart to [`ReplLink`]: a host supplies the host-side [`crate::compile::Compiler`];
/// a browser host (Lamella Studio) supplies an in-process compiler over `lamella-wasm`. Because the
/// engine is written against this trait (not a concrete compiler), the SAME [`Repl`] -- transcript
/// model, classification, suffix logic -- drives both, so the browser REPL is a faithful preview of the
/// CLI one.
pub trait ReplCompiler {
    /// Compile `source` (a complete compilation unit) to program assembly (PE) bytes.
    ///
    /// # Errors
    /// [`CompileFailure::Diagnostics`] if the source is rejected (the usual case); or
    /// [`CompileFailure::Toolchain`] on a toolchain fault.
    fn compile(&self, source: &str) -> Result<Vec<u8>, CompileFailure>;
}

/// How the REPL reaches the runner that runs a compiled program: the in-process reference runner over a
/// loopback, or a real device over the serial wire. The engine is written against this seam alone, so a
/// device REPL and the host loopback REPL share every line above it.
pub trait ReplLink {
    /// Run a compiled program assembly (tagging the round-trip with `seq`) and return its result.
    ///
    /// # Errors
    /// A [`TransportError`] from the carrier (or a timeout / closed link).
    fn run(&mut self, seq: u16, program: &[u8]) -> Result<RunResult, TransportError>;
}

/// The in-process [`ReplLink`]: drives the [`crate::run_program`] reference runner over a real
/// [`MemTransport`] loopback (encode a `RUN_PROGRAM` frame, the runner serves it, decode the
/// `RUN_RESULT`). Hardware-free, and exercises the genuine framed protocol path -- not a shortcut
/// around it -- so it behaves like the serial link minus the wire.
pub struct LoopbackLink {
    corlib: Vec<u8>,
    driver: MemTransport,
    runner: MemTransport,
}

impl LoopbackLink {
    /// A loopback link whose reference runner runs programs against `corlib` (the managed corlib bytes).
    #[must_use]
    pub fn new(corlib: Vec<u8>) -> Self {
        Self { corlib, driver: MemTransport::new(), runner: MemTransport::new() }
    }
}

impl ReplLink for LoopbackLink {
    fn run(&mut self, seq: u16, program: &[u8]) -> Result<RunResult, TransportError> {
        send_program(&mut self.driver, seq, program)?;
        self.runner.feed(&self.driver.take_sent());
        serve_one(&mut self.runner, &self.corlib)?;
        self.driver.feed(&self.runner.take_sent());
        try_recv_result(&mut self.driver, seq)?.ok_or(TransportError::Closed)
    }
}

/// The serial [`ReplLink`]: a real device runs the runner loop and answers over USB-CDC / UART. The
/// device side is the same runner core as [`LoopbackLink`]'s, just behind the wire.
#[cfg(feature = "serial")]
pub struct SerialLink {
    transport: SerialTransport,
    timeout: Duration,
}

#[cfg(feature = "serial")]
impl SerialLink {
    /// Open the serial port at `path` (`"COM5"` / `"/dev/ttyACM0"`) at `baud`, giving each evaluation
    /// up to `timeout` for the device to reply.
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the port cannot be opened.
    pub fn open(path: &str, baud: u32, timeout: Duration) -> Result<Self, TransportError> {
        Ok(Self { transport: SerialTransport::open(path, baud)?, timeout })
    }
}

#[cfg(feature = "serial")]
impl ReplLink for SerialLink {
    fn run(&mut self, seq: u16, program: &[u8]) -> Result<RunResult, TransportError> {
        eval_blocking(&mut self.transport, seq, program, self.timeout)
    }
}

/// How a submission folds into the session transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A statement (ends with `;` or `}`): appended to the transcript and re-emitted henceforth.
    Statement,
    /// A `using` directive: hoisted to the file header and persisted.
    Using,
    /// Anything else: an expression, wrapped in a print and run once, NOT persisted.
    Expression,
}

/// The host-side REPL transcript: the accumulated `using` directives + statements, and the console
/// output of the last successful run (so the engine can show only the new suffix). Pure -- it owns no
/// compiler or link, so its classification + program-rendering logic is unit-tested in isolation.
#[derive(Default)]
pub struct Session {
    usings: Vec<String>,
    statements: Vec<String>,
    last_stdout: String,
}

impl Session {
    /// Classify a trimmed submission.
    fn classify(submission: &str) -> Kind {
        if is_using_directive(submission) {
            Kind::Using
        } else if submission.ends_with(';') || submission.ends_with('}') {
            Kind::Statement
        } else {
            Kind::Expression
        }
    }

    /// Render a complete compilation unit = the persistent transcript with `candidate` folded in per
    /// its `kind`. Does not mutate the session. Indentation is omitted on purpose: the compiler ignores it, and
    /// leaving multi-line submissions verbatim keeps their layout intact.
    fn render(&self, candidate: &str, kind: Kind) -> String {
        let mut source = String::from("using System;\n");
        for directive in &self.usings {
            source.push_str(directive);
            source.push('\n');
        }
        if kind == Kind::Using {
            source.push_str(candidate);
            source.push('\n');
        }
        source.push_str("class __Repl {\n    static void Main() {\n");
        for statement in &self.statements {
            source.push_str(statement);
            source.push('\n');
        }
        match kind {
            Kind::Statement => {
                source.push_str(candidate);
                source.push('\n');
            }
            Kind::Expression => {
                source.push_str("System.Console.WriteLine(");
                source.push_str(candidate);
                source.push_str(");\n");
            }
            Kind::Using => {}
        }
        source.push_str("    }\n}\n");
        source
    }

    /// The new output beyond the previous run: the suffix of `full` after the recorded `last_stdout`
    /// prefix. Falls back to the whole output if the prefix doesn't match (a non-deterministic session).
    fn suffix<'a>(&self, full: &'a str) -> &'a str {
        full.strip_prefix(self.last_stdout.as_str()).unwrap_or(full)
    }

    /// Persist a statement and record the run's full output as the new baseline.
    fn commit_statement(&mut self, statement: String, full_stdout: String) {
        self.statements.push(statement);
        self.last_stdout = full_stdout;
    }

    /// Persist a `using` directive (deduplicated; the implicit `using System;` is never stored) and
    /// record the run's output as the new baseline.
    fn commit_using(&mut self, directive: String, full_stdout: String) {
        if directive != "using System;" && !self.usings.iter().any(|existing| existing == &directive) {
            self.usings.push(directive);
        }
        self.last_stdout = full_stdout;
    }

    /// Clear the transcript (the `#reset` command).
    pub fn reset(&mut self) {
        self.usings.clear();
        self.statements.clear();
        self.last_stdout.clear();
    }

    /// The persisted lines (usings then statements), for listing the session.
    pub fn transcript(&self) -> impl Iterator<Item = &str> {
        self.usings.iter().chain(self.statements.iter()).map(String::as_str)
    }

    /// How many statements the transcript holds.
    #[must_use]
    pub fn statement_count(&self) -> usize {
        self.statements.len()
    }
}

/// Whether a trimmed line is a `using` *directive* (`using System.Text;`, `using static System.Math;`,
/// `using Json = System.Text.Json;`) -- as opposed to a `using (resource) { ... }` statement, which
/// carries a `(`/`{`.
fn is_using_directive(line: &str) -> bool {
    line.starts_with("using ") && line.ends_with(';') && !line.contains('(') && !line.contains('{')
}

/// The result of evaluating one submission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The submission was empty / whitespace -- nothing to do.
    Empty,
    /// The compiler rejected the submission; the payload is the diagnostics text.
    CompileError(String),
    /// The submission ran. `output` is the NEW console output (the suffix beyond prior runs); `exit`
    /// is the program's exit code (0 for a normal statement/expression, the program's `Main` return in
    /// raw mode, or 70 if the interpreter aborted on an unhandled exception); `persisted` is whether it
    /// joined the transcript.
    Ran {
        /// The new console output beyond the previous run.
        output: String,
        /// The program's exit code (70 = aborted on an unhandled exception).
        exit: i32,
        /// Whether the submission was added to the session transcript.
        persisted: bool,
    },
}

/// A non-recoverable engine error (as opposed to a compile diagnostic, which is an [`Outcome`]).
#[derive(Debug)]
pub enum ReplError {
    /// The link to the runner failed (carrier error, timeout, or closed).
    Transport(TransportError),
    /// The toolchain itself failed (couldn't launch the compiler, scratch I/O) -- not a source diagnostic.
    Compile(String),
}

impl core::fmt::Display for ReplError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "link error: {error:?}"),
            Self::Compile(detail) => write!(f, "toolchain error: {detail}"),
        }
    }
}

impl std::error::Error for ReplError {}

/// The host-stateless REPL: a [`ReplCompiler`], a [`ReplLink`], and the host-side [`Session`] transcript. Both
/// seams are trait objects, so the SAME engine drives a host (compiler + serial/loopback) and a browser
/// (lamella-wasm compiler + in-process runner).
pub struct Repl {
    compiler: Box<dyn ReplCompiler>,
    link: Box<dyn ReplLink>,
    session: Session,
    seq: u16,
}

impl Repl {
    /// A REPL that compiles with `compiler` and runs over `link`, starting from an empty session.
    #[must_use]
    pub fn new(compiler: Box<dyn ReplCompiler>, link: Box<dyn ReplLink>) -> Self {
        Self { compiler, link, session: Session::default(), seq: 0 }
    }

    /// Evaluate a submission under the transcript model (the default REPL mode): classify it, re-emit
    /// the whole session, run it, and -- on a clean (exit 0) run -- persist a statement / using.
    ///
    /// # Errors
    /// [`ReplError`] if the toolchain or the link fails. A source-level compile failure is the
    /// non-error [`Outcome::CompileError`].
    pub fn eval(&mut self, submission: &str) -> Result<Outcome, ReplError> {
        let trimmed = submission.trim();
        if trimmed.is_empty() {
            return Ok(Outcome::Empty);
        }
        let kind = Session::classify(trimmed);
        let source = self.session.render(trimmed, kind);
        let program = match self.compiler.compile(&source) {
            Ok(bytes) => bytes,
            Err(CompileFailure::Diagnostics(text)) => return Ok(Outcome::CompileError(text)),
            Err(CompileFailure::Toolchain(detail)) => return Err(ReplError::Compile(detail)),
        };
        let result = self.run(&program)?;
        let output = self.session.suffix(&result.stdout).to_string();
        let persisted = if result.exit == 0 {
            match kind {
                Kind::Statement => {
                    self.session.commit_statement(trimmed.to_string(), result.stdout);
                    true
                }
                Kind::Using => {
                    self.session.commit_using(trimmed.to_string(), result.stdout);
                    true
                }
                Kind::Expression => false,
            }
        } else {
            false
        };
        Ok(Outcome::Ran { output, exit: result.exit, persisted })
    }

    /// Compile `source` as a complete compilation unit verbatim and run it once, bypassing the
    /// transcript (the `#raw` mode -- for pasting a whole program or declaring types). Shows the full
    /// console output; the session is untouched.
    ///
    /// # Errors
    /// [`ReplError`] if the toolchain or the link fails.
    pub fn eval_program(&mut self, source: &str) -> Result<Outcome, ReplError> {
        let program = match self.compiler.compile(source) {
            Ok(bytes) => bytes,
            Err(CompileFailure::Diagnostics(text)) => return Ok(Outcome::CompileError(text)),
            Err(CompileFailure::Toolchain(detail)) => return Err(ReplError::Compile(detail)),
        };
        let result = self.run(&program)?;
        Ok(Outcome::Ran { output: result.stdout, exit: result.exit, persisted: false })
    }

    /// Clear the session transcript.
    pub fn reset(&mut self) {
        self.session.reset();
    }

    /// The session transcript (usings then statements), for `#list`.
    pub fn transcript(&self) -> impl Iterator<Item = &str> {
        self.session.transcript()
    }

    /// Send a compiled program over the link under a fresh sequence number.
    fn run(&mut self, program: &[u8]) -> Result<RunResult, ReplError> {
        self.seq = self.seq.wrapping_add(1);
        self.link.run(self.seq, program).map_err(ReplError::Transport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn corlib() -> Option<Vec<u8>> {
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../lamella-load/tests/fixtures/corlib.dll")).ok()
    }

    fn hello() -> Option<Vec<u8>> {
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hello.exe")).ok()
    }

    #[test]
    fn classifies_statements_expressions_and_usings() {
        assert_eq!(Session::classify("int x = 5;"), Kind::Statement);
        assert_eq!(Session::classify("for (int i = 0; i < 3; i++) { }"), Kind::Statement);
        assert_eq!(Session::classify("40 + 2"), Kind::Expression);
        assert_eq!(Session::classify("Math.Sqrt(2)"), Kind::Expression);
        assert_eq!(Session::classify("using System.Text;"), Kind::Using);
        assert_eq!(Session::classify("using Json = System.Text.Json;"), Kind::Using);
        assert_eq!(Session::classify("using (var d = Make()) { d.Go(); }"), Kind::Statement);
    }

    #[test]
    fn renders_the_transcript_plus_the_candidate() {
        let mut session = Session::default();
        session.commit_statement("int x = 40;".to_string(), String::new());
        session.commit_using("using System.Text;".to_string(), String::new());

        let expression = session.render("x + 2", Kind::Expression);
        assert!(expression.contains("using System.Text;"));
        assert!(expression.contains("int x = 40;"));
        assert!(expression.contains("System.Console.WriteLine(x + 2);"));

        let statement = session.render("x++;", Kind::Statement);
        assert!(statement.contains("int x = 40;\nx++;\n"));
    }

    #[test]
    fn suffix_is_the_new_output_beyond_the_prior_run() {
        let mut session = Session::default();
        session.commit_statement("Console.WriteLine(1);".to_string(), "1\n".to_string());
        assert_eq!(session.suffix("1\n2\n"), "2\n");
        assert_eq!(session.suffix("9\n"), "9\n");
    }

    #[test]
    fn using_commit_dedups_and_drops_the_implicit_system() {
        let mut session = Session::default();
        session.commit_using("using System;".to_string(), String::new());
        session.commit_using("using System.Text;".to_string(), String::new());
        session.commit_using("using System.Text;".to_string(), String::new());
        assert_eq!(session.transcript().collect::<Vec<_>>(), vec!["using System.Text;"]);
    }

    #[test]
    fn loopback_link_runs_a_program_over_the_framed_protocol() {
        let (Some(program), Some(corlib)) = (hello(), corlib()) else { return };
        let mut link = LoopbackLink::new(corlib);
        let result = link.run(1, &program).expect("the loopback round-trips");
        assert_eq!(result.exit, 7);
        assert_eq!(result.stdout, "hi\n");
    }

    /// End-to-end through the real host toolchain -- ignored by default (needs the .NET SDK + ref assemblies, like the
    /// differential). Run on demand: `cargo test -p lamella-wireline -- --ignored`. Gated on the host
    /// compiler (`repl-host`).
    #[cfg(feature = "repl-host")]
    #[test]
    #[ignore = "needs the .NET SDK (the host compiler + ref assemblies); run with --ignored"]
    fn end_to_end_repl_over_loopback() {
        let Ok(compiler) = crate::compile::Compiler::discover() else {
            eprintln!("skipping: no .NET toolchain");
            return;
        };
        let Some(corlib) = corlib() else { return };
        let mut repl = Repl::new(Box::new(compiler), Box::new(LoopbackLink::new(corlib)));

        match repl.eval("40 + 2").expect("eval") {
            Outcome::Ran { output, exit, persisted } => {
                assert_eq!(output, "42\n");
                assert_eq!(exit, 0);
                assert!(!persisted);
            }
            other => panic!("expected a run, got {other:?}"),
        }

        assert!(matches!(repl.eval("int n = 40;").expect("eval"), Outcome::Ran { persisted: true, .. }));
        match repl.eval("Console.WriteLine(n + 2);").expect("eval") {
            Outcome::Ran { output, persisted, .. } => {
                assert_eq!(output, "42\n");
                assert!(persisted);
            }
            other => panic!("expected a run, got {other:?}"),
        }

        assert!(matches!(repl.eval("nonsense +").expect("eval"), Outcome::CompileError(_)));
        assert_eq!(repl.transcript().count(), 2);
    }
}
