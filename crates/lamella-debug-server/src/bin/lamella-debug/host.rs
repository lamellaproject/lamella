//! This computer's tier: the program run on the interpreter in this process, against the class
//! library `lamella run` gives it, and on this computer's clock.

use crate::launch::Plan;
use lamella_dap::{DebugBackend, InterpreterBackend, host_machine};
use lamella_wire_host::engine::LcscCompiler;

/// The backend for a program run on this computer: the program loaded with its class library, its
/// Portable PDB read where the build wrote one, and `Main` given the launch's arguments.
///
/// # Errors
/// Why the program cannot be debugged, in words for the user: a file that cannot be read or parsed,
/// or a program the loader refuses.
pub fn backend(plan: &Plan) -> Result<Box<dyn DebugBackend>, String> {
    let path = plan.image.display().to_string();
    let bytes = std::fs::read(&plan.image).map_err(|error| format!("could not read {path}: {error}"))?;
    let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
    let bytes = lamella_load::resident_bytes(bytes);
    let assembly = lamella_metadata::Assembly::read(bytes)
        .map_err(|_| format!("{path} is not a .NET assembly this debugger can read"))?;
    let corlib_bytes = class_library(plan)?.map(|bytes| {
        let bytes: &'static [u8] = Box::leak(bytes.into_boxed_slice());
        lamella_load::resident_bytes(bytes)
    });
    let corlib = corlib_bytes
        .map(lamella_metadata::Assembly::read)
        .transpose()
        .map_err(|_| String::from("the class library is not a .NET assembly this debugger can read"))?;
    let loaded = match corlib.as_ref() {
        Some(corlib) => lamella_load::load_with_corlib(corlib, &assembly),
        None => lamella_load::load(&assembly),
    };
    let program = loaded.map_err(|error| format!("could not load {path}: {error}"))?;
    let backend = match &plan.symbols {
        Some(symbols) => {
            let pdb = std::fs::read(symbols).map_err(|error| {
                format!("could not read the symbols {}: {error}", symbols.display())
            })?;
            lamella_metadata::PortablePdb::read(&pdb)
                .map_err(|_| crate::launch::not_a_portable_pdb(symbols))?;
            InterpreterBackend::with_pdb(program.module, program.entry, pdb).with_local_types(&assembly)
        }
        None => InterpreterBackend::new(program.module, program.entry),
    };
    let args = plan.args.clone();
    Ok(Box::new(
        backend
            .configure_machine(move |module, vm| {
                host_machine(module, vm);
                vm.set_program_args(args);
            })
            .polled(),
    ))
}

/// The corlib the program runs against: the one in the build's class-library folder, or else the
/// one the class-library search finds ([`LcscCompiler::discover`]), as `lamella run` finds it.
///
/// `None` when the search finds none, said on the session's Debug Console: the program is then
/// loaded alone, and a call into a class-library member with a managed body, such as `Thread.Sleep`,
/// traps.
fn class_library(plan: &Plan) -> Result<Option<Vec<u8>>, String> {
    match &plan.references {
        Some(folder) => LcscCompiler::discover_from(Some(folder), "the debug manifest's \"references\"")
            .map(|compiler| compiler.references().first().cloned()),
        None => Ok(searched(LcscCompiler::discover())),
    }
}

/// The corlib the class-library search `found`, or `None` for a search that found none, which is
/// said on the session's Debug Console with the reason.
fn searched(found: Result<LcscCompiler, String>) -> Option<Vec<u8>> {
    match found {
        Ok(compiler) => compiler.references().first().cloned(),
        Err(reason) => {
            crate::launch::say(format!(
                "Debugging without a class library, so a call into a member of it \
                 with a managed body, such as Thread.Sleep, traps: {reason}"
            ));
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::searched;

    /// **A SESSION WITH NO CLASS LIBRARY SAYS SO ON ITS DEBUG CONSOLE**, with the search's reason:
    /// what it costs is a trap at the first call into the library.
    #[test]
    fn a_search_that_finds_no_class_library_says_so_for_the_sessions_debug_console() {
        assert_eq!(searched(Err(String::from("no Lamella SDK was found"))), None);
        assert_eq!(
            crate::launch::take_said(),
            [String::from(
                "Debugging without a class library, so a call into a member of it with a \
                 managed body, such as Thread.Sleep, traps: no Lamella SDK was found"
            )]
        );
    }
}
