//! The in-page Debug Adapter Protocol agent (feature `dap`).

#![allow(unsafe_code)]

use core::cell::RefCell;

use lamella_dap::{Debugger, InterpreterBackend, Message};
use lamella_load::{load, load_with_corlib};

use crate::abi::result_buffer;

thread_local! {
    /// Live debug sessions, indexed by `handle - 1`. wasm is single-threaded.
    static SESSIONS: RefCell<Vec<Option<Debugger>>> = const { RefCell::new(Vec::new()) };
}

/// Loads a program and starts a debug session, returning a 1-based handle, or 0 on
/// a load failure.
fn create(bytes: &[u8], pdb: Option<Vec<u8>>, corlib: Option<&[u8]>) -> u32 {
    let bytes = lamella_load::resident_bytes(bytes);
    let corlib = corlib.map(lamella_load::resident_bytes);
    let Ok(assembly) = lamella_metadata::Assembly::read(bytes) else {
        return 0;
    };
    let corlib_assembly = match corlib.map(lamella_metadata::Assembly::read) {
        None => None,
        Some(Ok(corlib)) => Some(corlib),
        Some(Err(_)) => return 0,
    };
    let loaded = match corlib_assembly.as_ref() {
        Some(corlib) => load_with_corlib(corlib, &assembly),
        None => load(&assembly),
    };
    let Ok(program) = loaded else {
        return 0;
    };
    let backend = match pdb {
        Some(pdb_bytes) => InterpreterBackend::with_pdb(program.module, program.entry, pdb_bytes),
        None => InterpreterBackend::new(program.module, program.entry),
    };
    let debugger =
        Debugger::with_backend(Box::new(backend.configure_machine(crate::clock::install)));
    SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        sessions.push(Some(debugger));
        u32::try_from(sessions.len()).unwrap_or(0)
    })
}

/// Dispatches one DAP request (JSON) to a session and returns the response plus any
/// events as a JSON array -- an empty array for a bad handle or unparseable request.
fn request(handle: u32, request_bytes: &[u8]) -> Vec<u8> {
    let messages = SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        let Some(Some(debugger)) = sessions.get_mut((handle as usize).wrapping_sub(1)) else {
            return Vec::new();
        };
        match serde_json::from_slice::<Message>(request_bytes) {
            Ok(Message::Request(dap_request)) => debugger.handle(&dap_request),
            _ => Vec::new(),
        }
    });
    serde_json::to_vec(&messages).unwrap_or_default()
}

/// Ends a debug session.
fn dispose(handle: u32) {
    SESSIONS.with(|sessions| {
        if let Some(slot) = sessions
            .borrow_mut()
            .get_mut((handle as usize).wrapping_sub(1))
        {
            *slot = None;
        }
    });
}

/// Starts a debug session for the assembly at `ptr..ptr + len`; returns a 1-based
/// handle, or 0 on failure.
///
/// # Safety
/// `ptr`/`len` must be a buffer the host filled via a prior `lamella_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_dap_create(ptr: *const u8, len: usize) -> u32 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    create(bytes, None, None)
}

/// Starts a source-mapped debug session for the assembly at `prog_ptr..prog_ptr + prog_len`
/// with its Portable PDB at `pdb_ptr..pdb_ptr + pdb_len`; returns a 1-based handle, or 0 on
/// failure. The PDB lights up source breakpoints, source-located frames, and named locals --
/// the browser has no filesystem for a `.pdb` sidecar, so it passes the bytes in memory.
///
/// # Safety
/// Both pointer/length pairs must be buffers the host filled via prior `lamella_alloc` calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_dap_create_with_pdb(
    prog_ptr: *const u8,
    prog_len: usize,
    pdb_ptr: *const u8,
    pdb_len: usize,
) -> u32 {
    let bytes = unsafe { core::slice::from_raw_parts(prog_ptr, prog_len) };
    let pdb_bytes = unsafe { core::slice::from_raw_parts(pdb_ptr, pdb_len) };
    create(bytes, Some(pdb_bytes.to_vec()), None)
}

/// [`lamella_dap_create_with_pdb`] with a managed corlib loaded alongside the program, so a DEBUG
/// session resolves exactly what a RUN does. Returns a 1-based handle, or 0 on failure (including a
/// corlib that does not parse -- the host's mistake, and a session that cannot resolve is worse than none).
///
/// **Prefer this whenever the host has a corlib.** Without one the loader reaches only its own intrinsic
/// bindings, so any corlib method with a MANAGED body traps mid-session while the SAME program runs fine
/// through `lamella_run_with_corlib`. A program that runs and then fails to debug is the worst place to
/// discover that difference.
///
/// # Safety
/// All three pointer/length pairs must be buffers the host filled via prior `lamella_alloc` calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_dap_create_with_corlib(
    prog_ptr: *const u8,
    prog_len: usize,
    pdb_ptr: *const u8,
    pdb_len: usize,
    corlib_ptr: *const u8,
    corlib_len: usize,
) -> u32 {
    let bytes = unsafe { core::slice::from_raw_parts(prog_ptr, prog_len) };
    let pdb = if pdb_len == 0 {
        None
    } else {
        Some(unsafe { core::slice::from_raw_parts(pdb_ptr, pdb_len) }.to_vec())
    };
    let corlib = unsafe { core::slice::from_raw_parts(corlib_ptr, corlib_len) };
    create(bytes, pdb, Some(corlib))
}

/// Dispatches a DAP request (JSON at `ptr..ptr + len`) to session `handle`,
/// returning a `[u32 len][UTF-8 JSON]` buffer holding a JSON array of replies.
///
/// # Safety
/// `ptr`/`len` must be a buffer the host filled via a prior `lamella_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_dap_request(handle: u32, ptr: *const u8, len: usize) -> *mut u8 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    result_buffer(request(handle, bytes))
}

/// Ends session `handle` (freeing its interpreter state).
#[unsafe(no_mangle)]
pub extern "C" fn lamella_dap_dispose(handle: u32) {
    dispose(handle);
}

/// Drives a scripted debug session over an embedded fixture and returns 1 if it
/// produced the expected output and terminated, else 0 -- so a wasm host validates
/// the whole DAP-in-wasm path (incl. JSON) with one `--invoke`. Behind `selftest`.
#[cfg(feature = "selftest")]
#[unsafe(no_mangle)]
pub extern "C" fn lamella_dap_selftest() -> i32 {
    let handle = create(
        include_bytes!("../../lamella-load/tests/fixtures/hello.dll"),
        None,
        None,
    );
    if handle == 0 {
        return 0;
    }
    request(
        handle,
        br#"{"type":"request","seq":1,"command":"initialize"}"#,
    );
    request(handle, br#"{"type":"request","seq":2,"command":"launch"}"#);
    let reply = String::from_utf8(request(
        handle,
        br#"{"type":"request","seq":3,"command":"continue"}"#,
    ))
    .unwrap_or_default();
    dispose(handle);
    i32::from(reply.contains("Hello, World!") && reply.contains(r#""event":"terminated""#))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Option<Vec<u8>> {
        let path = format!(
            "{}/../lamella-load/tests/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::read(path).ok()
    }

    fn reply_text(handle: u32, json: &[u8]) -> String {
        String::from_utf8(request(handle, json)).unwrap()
    }

    #[test]
    fn a_scripted_dap_session_runs_hello_world() {
        let Some(bytes) = fixture("hello.dll") else {
            eprintln!("hello.dll absent; skipping");
            return;
        };
        let handle = create(&bytes, None, None);
        assert_ne!(handle, 0);

        let init = reply_text(
            handle,
            br#"{"type":"request","seq":1,"command":"initialize"}"#,
        );
        assert!(init.contains(r#""command":"initialize""#));

        let launched = reply_text(handle, br#"{"type":"request","seq":2,"command":"launch"}"#);
        assert!(launched.contains(r#""event":"initialized""#));
        let ran = reply_text(
            handle,
            br#"{"type":"request","seq":3,"command":"continue"}"#,
        );
        assert!(ran.contains("Hello, World!"), "reply was {ran}");
        assert!(ran.contains(r#""event":"terminated""#));

        dispose(handle);
        let after = request(handle, br#"{"type":"request","seq":4,"command":"threads"}"#);
        assert_eq!(after, b"[]");
    }

    /// A program that times a 300 ms `Thread.Sleep` and prints the year `DateTime.Now` reads.
    const SLEEPS_AND_READS_THE_DATE: &str = "\
using System;
using System.Threading;

class Program
{
    static int Main()
    {
        int before = Environment.TickCount;
        Thread.Sleep(300);
        int after = Environment.TickCount;
        Console.WriteLine(after - before);
        DateTime now = DateTime.Now;
        Console.WriteLine(now.Year);
        return 0;
    }
}
";

    #[test]
    fn a_debugged_program_sleeps_for_its_time_and_reads_todays_date() {
        let Some(corlib) = fixture("corlib.dll") else {
            eprintln!("corlib.dll absent; skipping");
            return;
        };
        let references = [lamella_metadata::Assembly::read(&corlib).expect("the corlib parses")];
        let compilation = lamella_assemble::compile_source(
            SLEEPS_AND_READS_THE_DATE,
            "app.cs",
            "app.dll",
            "app",
            &references,
            true,
        );
        assert!(
            compilation.diagnostics.is_empty(),
            "{:?}",
            compilation.diagnostics
        );
        let image = compilation.image.expect("an assembly image");
        let handle = create(&image, compilation.pdb, Some(corlib.as_slice()));
        assert_ne!(handle, 0);
        reply_text(
            handle,
            br#"{"type":"request","seq":1,"command":"initialize"}"#,
        );
        reply_text(handle, br#"{"type":"request","seq":2,"command":"launch"}"#);
        let started = std::time::Instant::now();
        let ran = reply_text(
            handle,
            br#"{"type":"request","seq":3,"command":"configurationDone"}"#,
        );
        let took = started.elapsed();
        dispose(handle);

        let replies: Vec<serde_json::Value> = serde_json::from_str(&ran).expect("a JSON array");
        let printed: String = replies
            .iter()
            .filter(|reply| reply["event"] == "output" && reply["body"]["category"] == "stdout")
            .filter_map(|reply| reply["body"]["output"].as_str())
            .collect();
        let lines: Vec<i64> = printed
            .lines()
            .map(|line| line.trim().parse().expect("a number"))
            .collect();
        assert_eq!(lines.len(), 2, "two lines, not {printed:?}");
        assert!(
            lines[0] >= 300,
            "the program measured {} ms across its 300 ms sleep",
            lines[0]
        );
        assert!(
            took.as_millis() >= 300,
            "the session returned after {took:?}, before the sleep's time"
        );
        assert!(lines[1] >= 2026, "DateTime.Now read the year {}", lines[1]);
    }

    #[test]
    fn create_with_pdb_starts_a_runnable_session() {
        let Some(bytes) = fixture("hello.dll") else {
            eprintln!("hello.dll absent; skipping");
            return;
        };
        let handle = create(&bytes, Some(Vec::new()), None);
        assert_ne!(handle, 0);
        reply_text(
            handle,
            br#"{"type":"request","seq":1,"command":"initialize"}"#,
        );
        reply_text(handle, br#"{"type":"request","seq":2,"command":"launch"}"#);
        let ran = reply_text(
            handle,
            br#"{"type":"request","seq":3,"command":"continue"}"#,
        );
        assert!(ran.contains("Hello, World!"), "reply was {ran}");
        dispose(handle);
    }

    /// Runs the session `handle` names from its start to the program's end, then disposes of it.
    /// Returns what the program printed and the exit code the session reported.
    fn run_to_the_end(handle: u32) -> (String, Option<i64>) {
        reply_text(
            handle,
            br#"{"type":"request","seq":1,"command":"initialize"}"#,
        );
        reply_text(handle, br#"{"type":"request","seq":2,"command":"launch"}"#);
        let ran = reply_text(
            handle,
            br#"{"type":"request","seq":3,"command":"configurationDone"}"#,
        );
        dispose(handle);
        let replies: Vec<serde_json::Value> = serde_json::from_str(&ran).expect("a JSON array");
        let printed = replies
            .iter()
            .filter(|reply| reply["event"] == "output" && reply["body"]["category"] == "stdout")
            .filter_map(|reply| reply["body"]["output"].as_str())
            .collect();
        let exit = replies
            .iter()
            .find(|reply| reply["event"] == "exited")
            .and_then(|reply| reply["body"]["exitCode"].as_i64());
        (printed, exit)
    }

    /// **A DEBUG SESSION WITH NO CORLIB CASTS AN ARRAY AS .NET's DOES**, through
    /// `lamella_dap_create`, which loads the program alone: `(IComparable)new int[3]` throws.
    #[test]
    fn a_program_debugged_with_no_corlib_casts_an_array_as_dotnet_does() {
        let Some((image, _)) =
            crate::tests::compiled_against_the_corlib(crate::tests::CASTS_AN_ARRAY)
        else {
            eprintln!("corlib.dll absent; skipping");
            return;
        };
        let handle = unsafe { lamella_dap_create(image.as_ptr(), image.len()) };
        assert_ne!(handle, 0);
        let (printed, exit) = run_to_the_end(handle);
        assert_eq!(printed, "InvalidCastException\n");
        assert_eq!(exit, Some(0), "the first check that failed");
    }

    /// **AND THROUGH `lamella_dap_create_with_pdb`**, the page's path, which loads the program
    /// alone beside its PDB.
    #[test]
    fn a_program_debugged_from_its_pdb_with_no_corlib_casts_an_array_as_dotnet_does() {
        let Some((image, pdb)) =
            crate::tests::compiled_against_the_corlib(crate::tests::CASTS_AN_ARRAY)
        else {
            eprintln!("corlib.dll absent; skipping");
            return;
        };
        let pdb = pdb.expect("a PDB");
        let handle = unsafe {
            lamella_dap_create_with_pdb(image.as_ptr(), image.len(), pdb.as_ptr(), pdb.len())
        };
        assert_ne!(handle, 0);
        let (printed, exit) = run_to_the_end(handle);
        assert_eq!(printed, "InvalidCastException\n");
        assert_eq!(exit, Some(0), "the first check that failed");
    }
}
