//! The in-page Debug Adapter Protocol agent (feature `dap`).

#![allow(unsafe_code)]

use core::cell::RefCell;

use lamella_dap::{Debugger, Message};
use lamella_load::load;
use lamella_metadata::Assembly;

use crate::abi::result_buffer;

thread_local! {
    /// Live debug sessions, indexed by `handle - 1`. wasm is single-threaded.
    static SESSIONS: RefCell<Vec<Option<Debugger>>> = const { RefCell::new(Vec::new()) };
}

/// Loads a program and starts a debug session, returning a 1-based handle, or 0 on
/// a load failure.
fn create(bytes: &[u8], pdb: Option<Vec<u8>>) -> u32 {
    let Ok(assembly) = Assembly::read(bytes) else {
        return 0;
    };
    let Ok(program) = load(&assembly) else {
        return 0;
    };
    let debugger = match pdb {
        Some(pdb_bytes) => Debugger::with_source(program.module, program.entry, pdb_bytes),
        None => Debugger::new(program.module, program.entry),
    };
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
    create(bytes, None)
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
    create(bytes, Some(pdb_bytes.to_vec()))
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
        let handle = create(&bytes, None);
        assert_ne!(handle, 0);

        let init = reply_text(
            handle,
            br#"{"type":"request","seq":1,"command":"initialize"}"#,
        );
        assert!(init.contains(r#""command":"initialize""#));
        assert!(init.contains(r#""event":"initialized""#));

        reply_text(handle, br#"{"type":"request","seq":2,"command":"launch"}"#);
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

    #[test]
    fn create_with_pdb_starts_a_runnable_session() {
        let Some(bytes) = fixture("hello.dll") else {
            eprintln!("hello.dll absent; skipping");
            return;
        };
        let handle = create(&bytes, Some(Vec::new()));
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
}
