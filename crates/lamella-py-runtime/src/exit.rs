//! Interpret an unhandled `SystemExit` for every host and board that ends a program (Python 3.14 Library
//! Reference, "Built-in Exceptions", SystemExit, and sys.exit).

use alloc::format;
use alloc::string::String;

use lamella_py_bytecode::Functions;

use crate::interp::{read_attr_as_the_program_does, str_as_the_program_does};
use crate::object::ObjectModel;
use crate::trap::Trap;
use crate::value::Value;

/// The exit status and diagnostic text of a run that a trap stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrappedEnd {
    /// Whether the run ended as an exit, by a `SystemExit`, rather than as a failure.
    pub exited: bool,
    /// The run's exit code: what a process exits with, of which a system keeps the low bits.
    pub code: i32,
    /// What goes on standard error before the run ends: whole lines, or nothing.
    pub said: String,
    /// The trap that ended the run, or an empty string when the run ended as an exit.
    pub trap_name: String,
}

impl TrappedEnd {
    /// What a board's own console prints when the run has ended so: the end's text on standard error,
    /// then `[py] exit=<code>` for an exit, or `[py] trap=<trap_name>` for a failure. One wording, so a
    /// board's boot run and every other board end a program alike.
    #[must_use]
    pub fn console_report(&self) -> String {
        if self.exited {
            format!("{}\r\n[py] exit={}\r\n", self.said, self.code)
        } else {
            format!("{}\r\n[py] trap={}\r\n", self.said, self.trap_name)
        }
    }
}

/// How the run whose program `model` was running ends, now that a trap stopped it, read with the
/// program's functions as [`crate::run_bundle`] installed them on `model`. A host that ran the program
/// another way, with [`crate::run_module`] or [`crate::run`], passes its own to [`trapped_end_in`]; with
/// none, a read that runs none of the program's code still answers.
pub fn trapped_end(model: &mut ObjectModel, trap_name: &str) -> TrappedEnd {
    let functions = model.managed_functions_rc(0).unwrap_or_default();
    trapped_end_in(model, &functions, trap_name)
}

/// How the run ends, now that a trap stopped it: a `SystemExit` ends it as an exit with its own code
/// ([`system_exit`]); any other exception nothing caught ends it with 1, its type and message said on
/// standard error as `python`'s last line says them; and a trap that left no exception, or a fault of
/// the runtime's own while the code was read, ends it with 1, the trap named.
///
/// Every host and board that runs a program ends it here, so a program ends the same way wherever it
/// runs. Deciding the end can run the program's own code (a `code` getter, a `__str__`), and what that
/// code writes is the program's output: a host writes it first, then [`TrappedEnd::said`].
pub fn trapped_end_in(model: &mut ObjectModel, functions: &Functions, trap_name: &str) -> TrappedEnd {
    let failed = |name: &str, said: String| TrappedEnd { exited: false, code: 1, said, trap_name: String::from(name) };
    let Some(exception) = model.take_pending_exception() else {
        return failed(trap_name, format!("{trap_name}\n"));
    };
    let mut roots = model.root_scope();
    let model = &mut *roots;
    let exception_root = model.root_value(exception);
    match system_exit(model, functions, exception) {
        Ok(Some(SystemExitCode::Code(code))) => {
            return TrappedEnd { exited: true, code, said: String::new(), trap_name: String::new() };
        }
        Ok(Some(SystemExitCode::Said(text))) => {
            return TrappedEnd { exited: true, code: 1, said: format!("{text}\n"), trap_name: String::new() };
        }
        Ok(None) => {}
        Err(fault) => return failed(&format!("{fault:?}"), format!("{fault:?}\n")),
    }
    let exception = model.rooted_value(exception_root);
    let Some(name) = model.exception_type_name(exception).map(String::from) else {
        return failed(trap_name, format!("{trap_name}\n"));
    };
    match model.exception_message(exception).filter(|message| !message.is_empty()) {
        Some(message) => failed(trap_name, format!("{name}: {message}\n")),
        None => failed(trap_name, format!("{name}\n")),
    }
}

/// The end a `SystemExit` asks for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemExitCode {
    /// The program exits with this code.
    Code(i32),
    /// The program exits with 1, after this text on standard error.
    Said(String),
}

/// What `exception`, which ended a program uncaught, asks for when it is a `SystemExit`: an instance
/// of the built-in class or of one of its subclasses, asked by the class's ancestry and not by its
/// name. `Ok(None)` for any other exception, a class of the program's own called `SystemExit` included.
///
/// `None` selects status 0, an integer supplies the status, and any other value is printed on
/// standard error with status 1. Python 3.14 Library Reference, "Built-in Exceptions", SystemExit,
/// and sys.exit describe these cases.
///
/// The code attribute and string conversions run through the interpreter. If reading code raises
/// a Python exception, the original exception's string supplies the text and the status is 1.
/// If that conversion raises, the text is empty. Runtime faults propagate to the caller.
/// Integer codes use the low 32 bits of a signed 64-bit conversion, or -1 if that conversion fails.
pub fn system_exit(
    model: &mut ObjectModel,
    functions: &Functions,
    exception: Value,
) -> Result<Option<SystemExitCode>, Trap> {
    let Some(class) = model.exception_class("SystemExit") else {
        return Ok(None);
    };
    if !model.exception_isinstance(exception, class) {
        return Ok(None);
    }
    let mut roots = model.root_scope();
    let model = &mut *roots;
    let exception_root = model.root_value(exception);
    let code = match read_attr_as_the_program_does(exception, "code", functions, model) {
        Ok(code) => code,
        Err(trap) => {
            drop_what_the_program_raised(model, trap)?;
            let exception = model.rooted_value(exception_root);
            return Ok(Some(SystemExitCode::Said(str_or_nothing(model, functions, exception)?)));
        }
    };
    if code.is_none() {
        return Ok(Some(SystemExitCode::Code(0)));
    }
    if model.is_int(code) {
        let wide = model.as_bigint(code).and_then(|number| number.to_i128()).and_then(|number| i64::try_from(number).ok());
        return Ok(Some(SystemExitCode::Code(wide.unwrap_or(-1) as i32)));
    }
    Ok(Some(SystemExitCode::Said(str_or_nothing(model, functions, code)?)))
}

/// Drops the Python exception a read or a string conversion raised. A runtime fault propagates.
fn drop_what_the_program_raised(model: &mut ObjectModel, trap: Trap) -> Result<(), Trap> {
    if trap == Trap::Raised {
        let _ = model.take_pending_exception();
        return Ok(());
    }
    match model.trap_to_exception(trap) {
        Some(_) => Ok(()),
        None => Err(trap),
    }
}

/// `str(value)` as the program's own `str()` makes it, or nothing when that raises.
fn str_or_nothing(model: &mut ObjectModel, functions: &Functions, value: Value) -> Result<String, Trap> {
    match str_as_the_program_does(value, functions, model) {
        Ok(text) => Ok(text),
        Err(trap) => {
            drop_what_the_program_raised(model, trap)?;
            Ok(String::new())
        }
    }
}
