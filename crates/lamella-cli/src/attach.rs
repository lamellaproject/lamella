//! `lamella run --target`: run a program on a board with its output on your terminal.

use lamella_debug_backend::{DebugBackend, Stop};
use lamella_wire_host::debug_backend::WireHostBackend;
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

/// The default serial baud for a Lamella Link carrier (USB-CDC ignores it; a real UART wants it).
const BAUD: u32 = 115_200;

/// How long to wait on each wire exchange while setting the session up.
const TIMEOUT: Duration = Duration::from_secs(20);


/// Compile `path` and run it on the firmware at `target`, streaming its output.
pub fn run_on_target(path: &Path, target: &str) -> ExitCode {
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("lamella run: read {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    if path.extension().and_then(|extension| extension.to_str()) != Some("cs") {
        eprintln!(
            "lamella run: running on a --target is a C# path today; a Python program reaches a \
             board as a\nbundle, whose host-side send is not a library call yet."
        );
        return ExitCode::FAILURE;
    }

    let compiler = match lamella_wire_host::engine::LcscCompiler::discover() {
        Ok(compiler) => compiler,
        Err(error) => {
            eprintln!("lamella run: {error}");
            return ExitCode::FAILURE;
        }
    };
    let image = match crate::bake::compile_and_bake(&compiler, &source) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    println!("built {} B from {}", image.len(), path.display());

    let mut backend = match WireHostBackend::open_target(target, BAUD, image, TIMEOUT) {
        Ok(backend) => backend,
        Err(error) => {
            eprintln!("lamella run: cannot open {target}: {error:?}");
            eprintln!(
                "\nthis build can open: {}.\n\
                 `lamella devices` lists what is attached and what to write here.",
                lamella_wire_host::available_carriers().join(", ")
            );
            return ExitCode::FAILURE;
        }
    };
    if !backend.launch() {
        eprintln!(
            "lamella run: {target} would not take the program.\n\n\
             An attached run needs firmware that offers the debug capabilities -- stepping and \
             breakpoints --\nbecause the output stream rides that channel. A board running a \
             serve build without them can\nstill take `lamella deploy`, which needs none of it."
        );
        return ExitCode::FAILURE;
    }

    println!("running on {target}; output follows.\n");
    stream(&mut backend)
}

/// Drive the program to its end, printing output as it arrives.
///
/// **EVERY STOP DRAINS THE OUTPUT BEFORE IT IS ACTED ON.** A program that faults has usually
/// printed something explaining itself, and printing the fault first would put the explanation
/// after the complaint.
fn stream(backend: &mut WireHostBackend) -> ExitCode {
    show(backend);
    let mut stop = backend.resume();
    loop {
        show(backend);
        match stop {
            Stop::Running => stop = backend.poll(),
            Stop::Done => {
                let code = backend.exit_code();
                println!("\nthe program ended, exit code {code}.");
                return if code == 0 {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                };
            }
            Stop::Fault(why) => {
                eprintln!("\nthe program stopped: {why}");
                return ExitCode::FAILURE;
            }
            Stop::Breakpoint | Stop::Step => {
                eprintln!(
                    "\nthe target halted, which `run` has no way to continue from -- it sets no \
                     breakpoints.\nA debugger session (the editor, over `lamella-dap`) is the tool \
                     that can."
                );
                return ExitCode::FAILURE;
            }
        }
    }
}

/// Print whatever the program has produced since the last look.
fn show(backend: &mut WireHostBackend) {
    if let Some(text) = backend.take_output() {
        print!("{text}");
        let _ = std::io::stdout().flush();
    }
}

/// What to tell somebody before they wait on a program that will not end.
///
/// **INTERRUPTING AN ATTACHED RUN LEAVES A SESSION ON THE TARGET**, and saying so up front is the
/// difference between a known cost and a mysterious board. There is no polite way to put a session
/// down from outside this loop yet, so the recovery is stated rather than implied: the next launch
/// clears a stale session, which makes `lamella run` its own repair.
#[must_use]
pub fn forever_warning() -> String {
    "(a program that loops forever runs until you stop this tool. Interrupting leaves a debug \
     session on\n the board -- the next `lamella run` clears it. To start a program and get your \
     shell back instead,\n use `lamella deploy`.)\n"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **THE COST OF INTERRUPTING IS STATED BEFORE IT IS PAID.** A reader who stops the tool and
    /// then finds the board unresponsive has no way to connect the two, so the note is asserted
    /// rather than left to drift out of the output.
    #[test]
    fn the_warning_names_both_the_cost_and_the_recovery() {
        let text = forever_warning();
        assert!(text.contains("leaves a debug session"), "the cost: {text}");
        assert!(text.contains("next `lamella run` clears it"), "the recovery: {text}");
        assert!(text.contains("lamella deploy"), "and the verb that does not have the cost");
    }
}
