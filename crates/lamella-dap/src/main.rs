//! The Lamella debug-adapter server.

use lamella_dap::{Debugger, InterpreterBackend, host_machine, serve};
use lamella_wire_host::engine::LcscCompiler;
use std::{env, fs, io, path::Path, process};

fn main() {
    let Some(path) = env::args().nth(1) else {
        eprintln!("usage: lamella-dap <program.dll>");
        process::exit(2);
    };
    let bytes = fs::read(&path).unwrap_or_else(|error| {
        eprintln!("cannot read {path}: {error}");
        process::exit(1);
    });
    let bytes = lamella_load::resident_bytes(&bytes);
    let assembly = lamella_metadata::Assembly::read(bytes).unwrap_or_else(|error| {
        eprintln!("cannot parse {path}: {error:?}");
        process::exit(1);
    });
    let corlib_bytes = class_library();
    let corlib_bytes = corlib_bytes
        .as_deref()
        .map(|bytes| lamella_load::resident_bytes(bytes));
    let corlib = corlib_bytes.map(|bytes| {
        lamella_metadata::Assembly::read(bytes).unwrap_or_else(|error| {
            eprintln!("cannot parse the class library: {error:?}");
            process::exit(1);
        })
    });
    let loaded = match corlib.as_ref() {
        Some(corlib) => lamella_load::load_with_corlib(corlib, &assembly),
        None => lamella_load::load(&assembly),
    };
    let program = loaded.unwrap_or_else(|error| {
        eprintln!("cannot load program: {error}");
        process::exit(1);
    });

    let pdb_path = Path::new(&path).with_extension("pdb");
    let backend = match fs::read(&pdb_path) {
        Ok(pdb) => InterpreterBackend::with_pdb(program.module, program.entry, pdb),
        Err(_) => InterpreterBackend::new(program.module, program.entry),
    };
    let mut debugger = Debugger::with_backend(Box::new(backend.configure_machine(host_machine)));
    let stdin = io::stdin();
    let stdout = io::stdout();
    if let Err(error) = serve(&mut debugger, &mut stdin.lock(), &mut stdout.lock()) {
        eprintln!("dap server error: {error}");
        process::exit(1);
    }
}

/// The corlib the program runs against: the one the class-library search finds
/// ([`LcscCompiler::discover`]), as `lamella run` finds it.
///
/// `None` when there is none, said on the error stream: the program is then loaded alone, and a
/// call into a class-library member with a managed body, such as `Thread.Sleep`, traps.
fn class_library() -> Option<Vec<u8>> {
    match LcscCompiler::discover() {
        Ok(compiler) => compiler.references().first().cloned(),
        Err(reason) => {
            eprintln!(
                "lamella-dap: debugging without a class library, so a call into a member of it \
                 with a managed body, such as Thread.Sleep, traps: {reason}"
            );
            None
        }
    }
}
