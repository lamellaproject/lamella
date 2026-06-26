//! The wireline debug + REPL **runner core**: the piece that runs a host-compiled program on the
//! interpreter and answers over the wire. ONE implementation serves three hosts:
//! - the **host reference runner** (in-process, for the `lamella-repl` CLI loopback + tests),
//! - the **browser runner** (compiled into `lamella-wasm` for the Studio REPL),
//! - the **on-device firmware** (flashed onto a microcontroller behind the wire).

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use lamella_cil_runtime::memory::SafeMemory;
use lamella_cil_runtime::{Value, Vm, run};
use lamella_load::load_with_corlib;
use lamella_metadata::Assembly;
use lamella_wire::{Transport, TransportError};

/// Wireline message types for the REPL (debug types live elsewhere).
pub mod repl {
    /// Host -> target: run a program. Payload = the program assembly (PE) bytes.
    pub const RUN_PROGRAM: u8 = 0x20;
    /// Target -> host: the program's result. Payload = `exit(i32 LE) | stdout(UTF-8)`.
    pub const RUN_RESULT: u8 = 0x21;
}

/// The result of running a program on the target: its process exit code and captured console output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunResult {
    /// The program's exit code (its `Main` return, 0 if none, 70 on an unhandled trap).
    pub exit: i32,
    /// The program's captured console (`Console.Out`) output.
    pub stdout: String,
}

impl RunResult {
    /// `exit(i32 LE) | stdout(UTF-8)`.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut payload = Vec::with_capacity(4 + self.stdout.len());
        payload.extend_from_slice(&self.exit.to_le_bytes());
        payload.extend_from_slice(self.stdout.as_bytes());
        payload
    }

    /// Decode a [`repl::RUN_RESULT`] payload.
    #[must_use]
    pub fn decode(payload: &[u8]) -> Option<Self> {
        if payload.len() < 4 {
            return None;
        }
        Some(Self {
            exit: i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]),
            stdout: String::from_utf8_lossy(&payload[4..]).into_owned(),
        })
    }
}

/// Run a program assembly against `corlib_bytes`, capturing its console output + exit code. This is the
/// runner's actual work -- the host reference runner, the browser runner, and the device firmware all call
/// it. A bad assembly / load failure is reported as exit -1 with the reason in `stdout`; an unhandled
/// trap is exit 70 (matching the interpreter's abort convention).
#[must_use]
pub fn run_program(corlib_bytes: &[u8], program_bytes: &[u8]) -> RunResult {
    let Ok(corlib) = Assembly::read(corlib_bytes) else {
        return failure("could not read the corlib assembly");
    };
    let Ok(program) = Assembly::read(program_bytes) else {
        return failure("could not read the program assembly");
    };
    let loaded = match load_with_corlib(&corlib, &program) {
        Ok(loaded) => loaded,
        Err(error) => return failure(&format!("load failed: {error}")),
    };
    let mut vm = Vm::default();
    vm.set_memory_backend(Box::new(SafeMemory::new()));
    let outcome = run(&loaded.module, &mut vm, loaded.entry, Vec::new());
    let exit = match outcome {
        Ok(Some(Value::Int32(code))) => code,
        Ok(_) => 0,
        Err(_) => 70,
    };
    RunResult { exit, stdout: String::from_utf16_lossy(vm.output()) }
}

fn failure(reason: &str) -> RunResult {
    RunResult { exit: -1, stdout: reason.to_string() }
}

/// The runner's request handler: if a [`repl::RUN_PROGRAM`] is pending, run it (against `corlib_bytes`)
/// and reply with a [`repl::RUN_RESULT`] on the same seq. Returns whether a request was handled. A
/// device firmware's main loop is this call in a loop.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
pub fn serve_one(transport: &mut impl Transport, corlib_bytes: &[u8]) -> Result<bool, TransportError> {
    if let Some(frame) = transport.poll()? {
        if frame.msg_type == repl::RUN_PROGRAM {
            let result = run_program(corlib_bytes, &frame.payload);
            transport.send(repl::RUN_RESULT, frame.seq, &result.encode())?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Host driver: send a compiled `program` to the target for execution.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
pub fn send_program(transport: &mut impl Transport, seq: u16, program: &[u8]) -> Result<(), TransportError> {
    transport.send(repl::RUN_PROGRAM, seq, program)
}

/// Host driver: poll for the [`repl::RUN_RESULT`] matching `seq` (non-blocking; `None` if not in yet).
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
pub fn try_recv_result(transport: &mut impl Transport, seq: u16) -> Result<Option<RunResult>, TransportError> {
    while let Some(frame) = transport.poll()? {
        if frame.msg_type == repl::RUN_RESULT && frame.seq == seq {
            return Ok(RunResult::decode(&frame.payload));
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_result_round_trips() {
        let result = RunResult { exit: 7, stdout: "hi\n".to_string() };
        assert_eq!(RunResult::decode(&result.encode()), Some(result));
    }

    #[test]
    fn run_result_decode_rejects_a_short_payload() {
        assert_eq!(RunResult::decode(&[1, 2, 3]), None);
    }

    #[test]
    fn run_result_decode_tolerates_lossy_utf8() {
        let decoded = RunResult::decode(&[0, 0, 0, 0, 0xFF, 0xFE]).expect("decodes");
        assert_eq!(decoded.exit, 0);
    }
}
