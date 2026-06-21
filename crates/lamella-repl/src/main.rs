//! `lamella-repl` -- a host-PC C# REPL on the lamella interpreter.

use std::io::{self, BufRead, Write};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if let Some(flag) = args.first() {
        if flag == "--eval" || flag == "-e" {
            let Some(expression) = args.get(1) else {
                eprintln!("usage: lamella-repl --eval <expression>");
                return ExitCode::from(2);
            };
            return match lamella_repl::eval(expression) {
                Ok(output) => {
                    print!("{output}");
                    let _ = io::stdout().flush();
                    ExitCode::SUCCESS
                }
                Err(message) => {
                    eprintln!("{message}");
                    ExitCode::FAILURE
                }
            };
        }
        if flag == "--session" || flag == "-s" {
            let lines = &args[1..];
            return if lines.is_empty() {
                session_repl()
            } else {
                session_oneshot(lines)
            };
        }
        if flag == "--help" || flag == "-h" {
            print!(
                "lamella-repl -- a C# REPL on the lamella interpreter.\n\n\
                 usage:\n  \
                 lamella-repl                    start a stateless expression prompt\n  \
                 lamella-repl --eval <expr>      evaluate one expression and exit\n  \
                 lamella-repl --session          start a STATEFUL prompt (declarations persist)\n  \
                 lamella-repl --session <line>.. run each line as a submission, in order\n\n\
                 in the stateful prompt, input spans multiple lines until balanced (a blank line\n  \
                 submits), and `:`-commands (:history, :reset, :help, :quit) are available.\n"
            );
            return ExitCode::SUCCESS;
        }
        eprintln!("lamella-repl: unknown argument {flag:?}; try --help");
        return ExitCode::from(2);
    }

    repl()
}

/// Runs each `line` as a submission against one [`lamella_repl::ReplSession`], in order,
/// printing the output of each (a declaration prints nothing). Exits nonzero on the first
/// error -- so `--session "int x = 5;" "x * 2"` prints `10` then exits 0.
fn session_oneshot(lines: &[String]) -> ExitCode {
    let mut session = match lamella_repl::ReplSession::new() {
        Ok(session) => session,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    for line in lines {
        match session.submit(line) {
            Ok(output) => print!("{output}"),
            Err(message) => {
                eprintln!("{message}");
                return ExitCode::FAILURE;
            }
        }
    }
    let _ = io::stdout().flush();
    ExitCode::SUCCESS
}

/// The interactive STATEFUL prompt. Unlike [`repl`], state declared on one line is visible to
/// later lines (the `__Repl` instance is reused, with fields migrated forward). Two ergonomic
/// layers sit on top of [`lamella_repl::ReplSession`]:
///
/// - MULTI-LINE input: lines are accumulated into one submission until
///   [`lamella_repl::submission_is_complete`] says the brackets balance and the statement is
///   terminated (or it is a bare expression). While a submission is incomplete the prompt
///   changes to a continuation marker; a BLANK line force-submits whatever has accumulated so a
///   user is never stranded mid-block.
/// - META-COMMANDS: a line that begins with `:` at the START of a submission is a meta-command
///   (`:history`, `:reset`, `:help`, `:quit`), handled here and kept OUT of the C# classifier.
///   An in-session history records each C# submission for `:history` to list.
///
/// A bad submission is reported and the loop continues; EOF (or `:quit`) exits.
fn session_repl() -> ExitCode {
    let mut session = match lamella_repl::ReplSession::new() {
        Ok(session) => session,
        Err(message) => {
            eprintln!("lamella-repl: {message}");
            return ExitCode::FAILURE;
        }
    };
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut line = String::new();
    let mut buffer = String::new();
    let mut history: Vec<String> = Vec::new();

    println!(
        "lamella-repl (stateful: declarations persist). \
         Multi-line input continues until balanced; a blank line submits. \
         `:help` for commands; Ctrl-Z then Enter (or EOF) to quit."
    );
    loop {
        print!("{}", if buffer.is_empty() { "> " } else { ". " });
        if stdout.flush().is_err() {
            return ExitCode::FAILURE;
        }
        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                if !buffer.trim().is_empty() {
                    run_submission(&mut session, &mut history, buffer.trim());
                }
                println!();
                return ExitCode::SUCCESS;
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("lamella-repl: input error: {error}");
                return ExitCode::FAILURE;
            }
        }

        let blank = line.trim().is_empty();

        if buffer.is_empty() {
            if blank {
                continue;
            }
            if let Some(command) = line.trim().strip_prefix(':') {
                match handle_meta(command, &history) {
                    Meta::Continue => continue,
                    Meta::Reset => match lamella_repl::ReplSession::new() {
                        Ok(fresh) => {
                            session = fresh;
                            history.clear();
                            println!("session reset.");
                            continue;
                        }
                        Err(message) => {
                            println!("cannot reset: {message}");
                            continue;
                        }
                    },
                    Meta::Quit => return ExitCode::SUCCESS,
                }
            }
        }

        buffer.push_str(&line);

        if blank || lamella_repl::submission_is_complete(&buffer) {
            let submission = buffer.trim().to_owned();
            buffer.clear();
            if !submission.is_empty() {
                run_submission(&mut session, &mut history, &submission);
            }
        }
    }
}

/// Records `submission` in `history` and runs it against `session`, printing its output or the
/// error. Shared by the normal completed-submission path and the EOF flush.
fn run_submission(
    session: &mut lamella_repl::ReplSession,
    history: &mut Vec<String>,
    submission: &str,
) {
    history.push(submission.to_owned());
    match session.submit(submission) {
        Ok(output) => print!("{output}"),
        Err(message) => println!("{message}"),
    }
    let _ = io::stdout().flush();
}

/// What a meta-command asks the [`session_repl`] loop to do next.
enum Meta {
    /// Handled in place (e.g. `:history`, `:help`); keep the current session and buffer.
    Continue,
    /// Discard the session and history and start fresh.
    Reset,
    /// Leave the REPL.
    Quit,
}

/// Interprets a `:`-prefixed meta-command (the text AFTER the colon). `:history` lists prior
/// submissions, `:reset` clears the session, `:help` prints the command list, `:quit`/`:exit`
/// leave. An unknown command prints a hint. Meta-commands never reach the C# classifier.
fn handle_meta(command: &str, history: &[String]) -> Meta {
    match command.trim() {
        "history" => {
            if history.is_empty() {
                println!("(history is empty)");
            } else {
                for (index, entry) in history.iter().enumerate() {
                    let shown = entry.replace('\n', "\n     ");
                    println!("{:>3}  {shown}", index + 1);
                }
            }
            Meta::Continue
        }
        "reset" => Meta::Reset,
        "quit" | "exit" => Meta::Quit,
        "help" => {
            print!(
                "meta-commands (start a line with `:`):\n  \
                 :history   list prior submissions this session\n  \
                 :reset     clear all declared state and history\n  \
                 :help      show this help\n  \
                 :quit      leave the REPL (also: :exit, or EOF)\n"
            );
            Meta::Continue
        }
        other => {
            println!("unknown command :{other} (try :help)");
            Meta::Continue
        }
    }
}

/// The interactive prompt loop: read a line, evaluate it, print the result or the
/// error, repeat until EOF.
fn repl() -> ExitCode {
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut line = String::new();

    println!("lamella-repl (stateless expressions). Ctrl-Z then Enter (or EOF) to quit.");
    loop {
        print!("> ");
        if stdout.flush().is_err() {
            return ExitCode::FAILURE;
        }

        line.clear();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => {
                println!();
                return ExitCode::SUCCESS;
            }
            Ok(_) => {}
            Err(error) => {
                eprintln!("lamella-repl: input error: {error}");
                return ExitCode::FAILURE;
            }
        }

        let expression = line.trim();
        if expression.is_empty() {
            continue;
        }

        match lamella_repl::eval(expression) {
            Ok(output) => print!("{output}"),
            Err(message) => println!("{message}"),
        }
    }
}
