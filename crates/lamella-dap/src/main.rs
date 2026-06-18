//! The Lamella debug-adapter server.

use lamella_dap::{Debugger, serve};
use lamella_metadata::Assembly;
use std::{env, fs, io, process};

fn main() {
    let Some(path) = env::args().nth(1) else {
        eprintln!("usage: lamella-dap <program.dll>");
        process::exit(2);
    };
    let bytes = fs::read(&path).unwrap_or_else(|error| {
        eprintln!("cannot read {path}: {error}");
        process::exit(1);
    });
    let assembly = Assembly::read(&bytes).unwrap_or_else(|error| {
        eprintln!("cannot read metadata: {error:?}");
        process::exit(1);
    });
    let program = lamella_load::load(&assembly).unwrap_or_else(|error| {
        eprintln!("cannot load program: {error}");
        process::exit(1);
    });

    let mut debugger = Debugger::new(program.module, program.entry);
    let stdin = io::stdin();
    let stdout = io::stdout();
    if let Err(error) = serve(&mut debugger, &mut stdin.lock(), &mut stdout.lock()) {
        eprintln!("dap server error: {error}");
        process::exit(1);
    }
}
