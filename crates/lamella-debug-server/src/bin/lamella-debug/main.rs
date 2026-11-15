//! `lamella-debug`, the Lamella debugger. It speaks the Debug Adapter Protocol over its standard input
//! and output, so VS Code and every other editor that speaks it can debug a Lamella program: on this
//! computer's interpreter, on a board's interpreter over Lamella Link, or as native code on a board
//! through its debug probe.
//!
//! The editor starts it with no arguments. Which target a session debugs is decided at its `launch`,
//! from the launch's device and the debug manifest the project's build wrote: `local` is this
//! computer, a board's own USB or serial port is its interpreter, and a debug probe is the native
//! image. The debugger never builds: it debugs what the build made.

mod host;
mod launch;
mod link;
mod probe;

use lamella_dap::{Chosen, Debugger};
use launch::Device;
use serde_json::Value as Json;
use std::cell::Cell;
use std::process::ExitCode;
use std::rc::Rc;

const USAGE: &str = "\
usage: lamella-debug

Debugs a Lamella program for an editor that speaks the Debug Adapter Protocol, such as VS Code,
over standard input and output. The editor starts it; the session's launch names the device and
the debug manifest the project's build wrote.

  lamella-debug --version   the debugger's version
  lamella-debug --help      this text
";

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.first().map(String::as_str) {
        None => {}
        Some("--version" | "-V") => {
            println!("lamella-debug {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Some("--help" | "-h") => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Some(other) => {
            eprintln!("lamella-debug: unknown argument {other:?}.\n\n{USAGE}");
            return ExitCode::from(2);
        }
    }
    let refused = Rc::new(Cell::new(false));
    let started = Rc::new(Cell::new(false));
    let (on_refusal, on_start) = (refused.clone(), started.clone());
    let mut debugger = Debugger::choosing(Box::new(choose)).reporting_starts(Box::new(
        move |outcome: Result<(), &str>| match outcome {
            Ok(()) => on_start.set(true),
            Err(reason) => {
                eprintln!("lamella-debug: {reason}");
                on_refusal.set(true);
            }
        },
    ));
    let served = lamella_dap::serve_polled(
        &mut debugger,
        std::io::BufReader::new(std::io::stdin()),
        &mut std::io::stdout().lock(),
    );
    if let Err(error) = served {
        eprintln!("lamella-debug: the session's channel failed: {error}");
        return ExitCode::FAILURE;
    }
    if refused.get() && !started.get() {
        return ExitCode::from(2);
    }
    ExitCode::SUCCESS
}

/// The target a launch names, read from its arguments and the build's debug manifest.
///
/// A start that fails has no session to say its lines in, so what it said goes to the error stream,
/// and the next start says only its own.
fn choose(command: &str, arguments: &Json) -> Result<Chosen, String> {
    let chosen = start(command, arguments);
    if chosen.is_err() {
        for line in launch::take_said() {
            eprintln!("{line}");
        }
    }
    chosen
}

/// The session a launch starts, with the lines its start said for the Debug Console.
fn start(command: &str, arguments: &Json) -> Result<Chosen, String> {
    let plan = launch::plan(command, arguments)?;
    let backend = match &plan.device {
        Device::Local => host::backend(&plan)?,
        Device::Link(target) => link::backend(&plan, target)?,
        Device::Probe(serial) => {
            let image = plan.image.display().to_string();
            let probe_plan = probe::ProbePlan::new(serial, plan.board.as_deref(), &image, plan.attach)?
                .reading_symbols(plan.symbols.clone())
                .stopping_at_entry(plan.stop_at_entry && !plan.no_debug);
            probe::backend(&probe_plan)?
        }
    };
    Ok(plan.chosen(backend))
}
