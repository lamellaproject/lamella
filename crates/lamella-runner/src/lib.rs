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
    /// Host -> target: run a BAKED image. Payload = one self-contained `.lmli` image's bytes
    /// (the host compiles + bakes per submission). The PE-less constrained-target path,
    /// advertised by `Capabilities::BAKED_IMAGE`; answered by the same [`RUN_RESULT`].
    pub const RUN_IMAGE: u8 = 0x22;
}

/// Wireline message types for the DEBUG channel (the reserved 0x10+ range). A code
/// location crosses the wire as `(method_id: u32, offset: u32)` in the TARGET's code-unit
/// domain -- on the `in_place` (baked) tier that is the CIL BYTE offset, the same domain
/// Portable-PDB sequence points use, so the host's source mapping needs no conversion.
/// All source/PDB knowledge stays host-side; the target only ever sees ids and offsets.
pub mod debug {
    /// Host -> target: a baked image to debug. The target boots it HALTED at the entry
    /// point and replies [`EVT_STOPPED`] (reason `Entry`).
    pub const DBG_IMAGE: u8 = 0x10;
    /// Host -> target: run until a breakpoint, completion, a trap, or a [`DBG_PAUSE`].
    /// Replies [`EVT_STOPPED`] when execution stops.
    pub const DBG_RESUME: u8 = 0x11;
    /// Host -> target: execute one step. Replies [`EVT_STOPPED`] (reason `Step`, or
    /// `Breakpoint`/`Done`/`Trap` if the step landed there).
    pub const DBG_STEP: u8 = 0x12;
    /// Host -> target: replace ALL breakpoints. Payload = `count(u16 LE)` then `count` x
    /// `(method_id: u32 LE, offset: u32 LE)`. Replies [`DBG_ACK`].
    pub const DBG_BREAK: u8 = 0x13;
    /// Host -> target: request the call stack. Replies [`DBG_FRAMES`].
    pub const DBG_STACK: u8 = 0x14;
    /// Target -> host: the call stack, innermost first. Payload = `count(u16 LE)` then
    /// `count` x `(method_id: u32 LE, offset: u32 LE)`.
    pub const DBG_FRAMES: u8 = 0x15;
    /// Host -> target: pause a running [`DBG_RESUME`] at the next poll boundary. Replies
    /// [`EVT_STOPPED`] (reason `Paused`); a no-op acknowledged the same way while halted.
    pub const DBG_PAUSE: u8 = 0x16;
    /// Target -> host: a command that changes no execution state completed ([`DBG_BREAK`]).
    pub const DBG_ACK: u8 = 0x17;
    /// Target -> host: execution stopped. Payload = `reason(u8)` + `method_id(u32 LE)` +
    /// `offset(u32 LE)`, and for `Done`/`Trap` additionally the run result tail:
    /// `exit(i32 LE)` + `stdout(UTF-8)`. Reasons: 0 Entry, 1 Step, 2 Breakpoint,
    /// 3 Paused, 4 Done, 5 Trap. `Done`/`Trap` END the debug session.
    pub const EVT_STOPPED: u8 = 0x18;
    /// Host -> target: end the debug session (the target discards it and returns to the
    /// serve loop). Replies [`DBG_ACK`].
    pub const DBG_DETACH: u8 = 0x19;

    /// [`EVT_STOPPED`] reasons.
    pub mod reason {
        /// Booted and halted at the entry point (the reply to [`super::DBG_IMAGE`]).
        pub const ENTRY: u8 = 0;
        /// A [`super::DBG_STEP`] completed.
        pub const STEP: u8 = 1;
        /// Execution arrived at a [`super::DBG_BREAK`] location.
        pub const BREAKPOINT: u8 = 2;
        /// A [`super::DBG_PAUSE`] took effect (or acknowledged an already-halted target).
        pub const PAUSED: u8 = 3;
        /// The program completed; the payload carries the run-result tail. Ends the session.
        pub const DONE: u8 = 4;
        /// The program trapped (or the image failed to boot); run-result tail attached.
        /// Ends the session.
        pub const TRAP: u8 = 5;
    }
}

/// Wireline message types for PERSISTENT deploy (write a baked image to the target's flash
/// so it boots on reset), in the 0x20 REPL range beside [`repl::RUN_IMAGE`].
pub mod deploy {
    /// Host -> target: write a baked image to the persistent flash region and keep it, so
    /// the target boots it on the next reset / power-cycle. Payload = the image bytes.
    /// Answered by [`DEPLOY_RESULT`]. (Run-from-RAM stays [`super::repl::RUN_IMAGE`]; this
    /// is the durable "deploy the app" mode.)
    pub const DEPLOY_IMAGE: u8 = 0x23;
    /// Host -> target: erase the persistent image (un-deploy), so the next boot serves
    /// instead of running an app. Answered by [`DEPLOY_RESULT`].
    pub const DEPLOY_CLEAR: u8 = 0x24;
    /// Target -> host: `ok(u8)` -- 1 if the flash write verified (or the clear succeeded),
    /// 0 on a flash fault.
    pub const DEPLOY_RESULT: u8 = 0x25;
    /// Host -> target: write one CHUNK of a baked image to flash. Payload =
    /// `offset(u32 LE) | total(u32 LE) | chunk bytes`. Chunks arrive in ascending offset order;
    /// the target erases the region on the first chunk (`offset == 0`, sized from `total`) and
    /// writes each chunk straight to flash -- so an image larger than one 64 KB wire frame deploys
    /// without ever holding it whole in RAM. Each chunk is answered by [`DEPLOY_RESULT`]; the final
    /// chunk's reply is the deploy's overall result.
    pub const DEPLOY_CHUNK: u8 = 0x26;
}

/// The persistent-image flash region a deploy-capable target owns. The generic runner
/// drives the deploy protocol against this seam; the erase/write primitives are
/// device-specific (each MCU's flash controller differs), so the firmware implements them.
#[cfg(feature = "baked-image")]
pub trait FlashSink {
    /// The image region as a `'static` slice -- where a deployed image is written, and
    /// where the boot path reads `Module::from_baked` from. May be longer than any stored
    /// image; `from_baked` reads the true length from the image header.
    fn image_slice(&self) -> &'static [u8];
    /// Erase enough of the region to invalidate any stored image (its fingerprint), so a
    /// subsequent boot finds none.
    fn erase(&mut self);
    /// Erase, then program `image` into the region. Returns whether a readback verified.
    fn program(&mut self, image: &[u8]) -> bool;
    /// Program one CHUNK of a larger image: write `chunk` at `offset` within the region, having
    /// erased enough of the region for `total` bytes on the first chunk (`offset == 0`). Returns
    /// whether the chunk read back. The default is unsupported (a target that never receives an
    /// image larger than one wire frame need not implement it); a roomy target overrides it to
    /// stream a corlib-baked image to flash a frame at a time.
    fn program_chunk(&mut self, offset: usize, chunk: &[u8], total: usize) -> bool {
        let _ = (offset, chunk, total);
        false
    }
}

/// How a [`run_deployed`] app run ended.
#[cfg(feature = "baked-image")]
pub enum Deployed {
    /// The app ran to completion or trapped -- nothing is listening now; serve.
    Completed(RunResult),
    /// A host sent `HELLO` mid-run: the app was aborted and the host acknowledged; the
    /// firmware should enter its serve loop where the host's next command lands.
    Interrupted,
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
    #[cfg(feature = "baked-image")]
    let (corlib_bytes, program_bytes): (&'static [u8], &'static [u8]) = (
        Box::leak(corlib_bytes.to_vec().into_boxed_slice()),
        Box::leak(program_bytes.to_vec().into_boxed_slice()),
    );
    let corlib = match Assembly::read(corlib_bytes) {
        Ok(assembly) => assembly,
        Err(error) => return failure(&format!("corlib does not parse: {error:?}")),
    };
    let program = match Assembly::read(program_bytes) {
        Ok(assembly) => assembly,
        Err(error) => return failure(&format!("program does not parse: {error:?}")),
    };
    let loaded = match load_with_corlib(&corlib, &program) {
        Ok(loaded) => loaded,
        Err(error) => return failure(&format!("load failed: {error}")),
    };
    let mut vm = Vm::default();
    #[cfg(target_os = "none")]
    vm.set_mmio(lamella_mmio::write32, lamella_mmio::read32);
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

/// Answers a `HELLO` with this baked target's honest advertisement (or a `NAK` on a
/// version mismatch). A malformed `HELLO` is dropped; the host times out and retries.
#[cfg(feature = "baked-image")]
fn hello_reply(
    transport: &mut impl Transport,
    frame: &lamella_wire::Frame,
) -> Result<(), TransportError> {
    use lamella_wire::{Capabilities, Hello, PROTOCOL_VERSION, ProtocolRange, msg, target_respond};
    let range = ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION };
    let caps = Capabilities(
        Capabilities::BAKED_IMAGE
            | Capabilities::DEBUG_BASIC
            | Capabilities::BREAKPOINTS
            | Capabilities::STEPPING,
    );
    match Hello::decode(&frame.payload) {
        Some(hello) => match target_respond(&hello, range, caps) {
            Ok(ack) => transport.send(msg::HELLO_ACK, frame.seq, &ack.encode()),
            Err(nak) => transport.send(msg::NAK, frame.seq, &nak.encode()),
        },
        None => Ok(()),
    }
}

/// How a [`run_debug_session`] resume leg ended.
#[cfg(feature = "baked-image")]
enum RunStop {
    Breakpoint,
    Paused,
    Done(RunResult),
    Trap(RunResult),
}

/// The debugged program's current `(method_id, offset)` -- the innermost frame. On the
/// baked (`in_place`) tier the offset is the CIL BYTE offset.
#[cfg(feature = "baked-image")]
fn debug_location(session: &lamella_cil_runtime::Session) -> (u32, u32) {
    let depth = session.depth();
    if depth == 0 {
        return (0, 0);
    }
    session
        .frame(depth - 1)
        .map_or((0, 0), |frame| (frame.method, frame.ip as u32))
}

#[cfg(feature = "baked-image")]
fn send_stopped(
    transport: &mut impl Transport,
    seq: u16,
    why: u8,
    location: (u32, u32),
    tail: Option<&RunResult>,
) -> Result<(), TransportError> {
    let mut payload = Vec::with_capacity(9);
    payload.push(why);
    payload.extend_from_slice(&location.0.to_le_bytes());
    payload.extend_from_slice(&location.1.to_le_bytes());
    if let Some(result) = tail {
        payload.extend_from_slice(&result.encode());
    }
    transport.send(debug::EVT_STOPPED, seq, &payload)
}

#[cfg(feature = "baked-image")]
fn run_result_of(vm: &Vm, value: &Option<lamella_cil_runtime::Value>) -> RunResult {
    let exit = match value {
        Some(Value::Int32(code)) => *code,
        _ => 0,
    };
    RunResult { exit, stdout: String::from_utf16_lossy(vm.output()) }
}

/// Run until a breakpoint, completion, a trap, or a [`debug::DBG_PAUSE`]: bounded bursts
/// of steps with a wire poll between bursts, so a running target stays pause-able.
#[cfg(feature = "baked-image")]
fn run_until_stop(
    transport: &mut impl Transport,
    module: &lamella_cil_runtime::Module,
    vm: &mut Vm,
    session: &mut lamella_cil_runtime::Session,
) -> Result<RunStop, TransportError> {
    use lamella_cil_runtime::Status;
    loop {
        for _ in 0..2048 {
            match session.step(module, vm) {
                Ok(Status::Done(value)) => return Ok(RunStop::Done(run_result_of(vm, &value))),
                Err(trap) => {
                    return Ok(RunStop::Trap(RunResult {
                        exit: 70,
                        stdout: {
                            let mut text = String::from_utf16_lossy(vm.output());
                            text.push_str(&format!("TRAP: {trap:?}"));
                            text
                        },
                    }));
                }
                Ok(Status::Running | Status::Paused) => {
                    if session.is_at_breakpoint() {
                        return Ok(RunStop::Breakpoint);
                    }
                }
            }
        }
        if let Some(frame) = transport.poll()? {
            if frame.msg_type == debug::DBG_PAUSE {
                return Ok(RunStop::Paused);
            }
        }
    }
}

/// Debug a baked image over the wire: boot it HALTED at the entry, report
/// [`debug::EVT_STOPPED`] (reason `Entry`), then serve debug commands until the program
/// completes, traps, or the host detaches. See the [`debug`] message set; a device
/// firmware reaches this through [`serve_one_baked`]'s `DBG_IMAGE` arm.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn run_debug_session(
    transport: &mut impl Transport,
    image: Vec<u8>,
    image_seq: u16,
) -> Result<(), TransportError> {
    use debug::reason;
    use lamella_cil_runtime::{Module, Session, Status};

    let image: &'static [u8] = Box::leak(image.into_boxed_slice());
    let (module, entry) = match Module::from_baked(image) {
        Ok((module, Some(entry))) => (module, entry),
        Ok((_, None)) => {
            let result = failure("image records no entry point");
            return send_stopped(transport, image_seq, reason::TRAP, (0, 0), Some(&result));
        }
        Err(error) => {
            let result = failure(&format!("image does not boot: {error:?}"));
            return send_stopped(transport, image_seq, reason::TRAP, (0, 0), Some(&result));
        }
    };
    let mut vm = Vm::default();
    #[cfg(target_os = "none")]
    vm.set_mmio(lamella_mmio::write32, lamella_mmio::read32);
    vm.set_memory_backend(Box::new(SafeMemory::new()));
    let mut session = match Session::new(&module, entry, Vec::new()) {
        Ok(session) => session,
        Err(trap) => {
            let result = failure(&format!("session: {trap:?}"));
            return send_stopped(transport, image_seq, reason::TRAP, (0, 0), Some(&result));
        }
    };
    let mut at_reported_breakpoint = false;
    send_stopped(transport, image_seq, reason::ENTRY, debug_location(&session), None)?;

    loop {
        let Some(frame) = transport.poll()? else {
            continue;
        };
        match frame.msg_type {
            debug::DBG_STEP => match session.step(&module, &mut vm) {
                Ok(Status::Done(value)) => {
                    let result = run_result_of(&vm, &value);
                    return send_stopped(transport, frame.seq, reason::DONE, (0, 0), Some(&result));
                }
                Err(trap) => {
                    let result = RunResult {
                        exit: 70,
                        stdout: format!("{}TRAP: {trap:?}", String::from_utf16_lossy(vm.output())),
                    };
                    return send_stopped(transport, frame.seq, reason::TRAP, (0, 0), Some(&result));
                }
                Ok(Status::Running | Status::Paused) => {
                    at_reported_breakpoint = session.is_at_breakpoint();
                    let why = if at_reported_breakpoint { reason::BREAKPOINT } else { reason::STEP };
                    send_stopped(transport, frame.seq, why, debug_location(&session), None)?;
                }
            },
            debug::DBG_RESUME => {
                if at_reported_breakpoint {
                    at_reported_breakpoint = false;
                    match session.step(&module, &mut vm) {
                        Ok(Status::Done(value)) => {
                            let result = run_result_of(&vm, &value);
                            return send_stopped(transport, frame.seq, reason::DONE, (0, 0), Some(&result));
                        }
                        Err(trap) => {
                            let result = RunResult {
                                exit: 70,
                                stdout: format!(
                                    "{}TRAP: {trap:?}",
                                    String::from_utf16_lossy(vm.output())
                                ),
                            };
                            return send_stopped(transport, frame.seq, reason::TRAP, (0, 0), Some(&result));
                        }
                        Ok(_) => {}
                    }
                }
                match run_until_stop(transport, &module, &mut vm, &mut session)? {
                    RunStop::Breakpoint => {
                        at_reported_breakpoint = true;
                        send_stopped(
                            transport,
                            frame.seq,
                            reason::BREAKPOINT,
                            debug_location(&session),
                            None,
                        )?;
                    }
                    RunStop::Paused => {
                        send_stopped(transport, frame.seq, reason::PAUSED, debug_location(&session), None)?;
                    }
                    RunStop::Done(result) => {
                        return send_stopped(transport, frame.seq, reason::DONE, (0, 0), Some(&result));
                    }
                    RunStop::Trap(result) => {
                        return send_stopped(transport, frame.seq, reason::TRAP, (0, 0), Some(&result));
                    }
                }
            }
            debug::DBG_BREAK => {
                session.clear_breakpoints();
                let payload = &frame.payload;
                let count = payload
                    .get(0..2)
                    .map_or(0, |bytes| u16::from_le_bytes([bytes[0], bytes[1]]) as usize);
                for pair in 0..count {
                    let base = 2 + pair * 8;
                    let Some(bytes) = payload.get(base..base + 8) else {
                        break;
                    };
                    let method = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    let offset = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
                    session.add_breakpoint(method, offset);
                }
                transport.send(debug::DBG_ACK, frame.seq, &[])?;
            }
            debug::DBG_STACK => {
                let depth = session.depth();
                let mut payload = Vec::with_capacity(2 + depth * 8);
                payload.extend_from_slice(&(depth as u16).to_le_bytes());
                for index in (0..depth).rev() {
                    let (method, offset) =
                        session.frame(index).map_or((0, 0), |frame| (frame.method, frame.ip as u32));
                    payload.extend_from_slice(&method.to_le_bytes());
                    payload.extend_from_slice(&offset.to_le_bytes());
                }
                transport.send(debug::DBG_FRAMES, frame.seq, &payload)?;
            }
            debug::DBG_PAUSE => {
                send_stopped(transport, frame.seq, reason::PAUSED, debug_location(&session), None)?;
            }
            debug::DBG_DETACH => {
                transport.send(debug::DBG_ACK, frame.seq, &[])?;
                return Ok(());
            }
            lamella_wire::msg::HELLO => {
                hello_reply(transport, &frame)?;
                return Ok(());
            }
            _ => {}
        }
    }
}

/// Boot a baked image ([`lamella_cil_runtime::Module::from_baked`]) and run its entry point,
/// capturing console output + exit code -- [`run_program`]'s twin for the PE-less path. The
/// image bytes are leaked to `'static` (the image is borrowed in place, never copied): the
/// host reference runner accepts one leak per evaluation; a device firmware's bump-arena
/// reset between evaluations reclaims it.
#[cfg(feature = "baked-image")]
#[must_use]
pub fn run_image(image: Vec<u8>) -> RunResult {
    let image: &'static [u8] = Box::leak(image.into_boxed_slice());
    let (module, entry) = match lamella_cil_runtime::Module::from_baked(image) {
        Ok(booted) => booted,
        Err(error) => return failure(&format!("image does not boot: {error:?}")),
    };
    let Some(entry) = entry else {
        return failure("image records no entry point");
    };
    let mut vm = Vm::default();
    #[cfg(target_os = "none")]
    vm.set_mmio(lamella_mmio::write32, lamella_mmio::read32);
    vm.set_memory_backend(Box::new(SafeMemory::new()));
    let outcome = run(&module, &mut vm, entry, Vec::new());
    let exit = match outcome {
        Ok(Some(Value::Int32(code))) => code,
        Ok(_) => 0,
        Err(_) => 70,
    };
    RunResult { exit, stdout: String::from_utf16_lossy(vm.output()) }
}

/// Serve one pending request on a PE-less BAKED-IMAGE target: a `HELLO` gets a `HELLO_ACK`
/// advertising `Capabilities::BAKED_IMAGE` (or a `NAK` on version mismatch), and a
/// [`repl::RUN_IMAGE`] boots + runs the received image and replies with [`repl::RUN_RESULT`].
/// Returns whether a frame was handled. A device firmware's main loop is this call in a
/// loop, plus its own storage reset between evaluations (nothing survives a request by
/// design -- the host-stateless REPL re-sends whole sessions).
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn serve_one_baked(transport: &mut impl Transport) -> Result<bool, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(false);
    };
    serve_frame_baked(transport, frame)?;
    Ok(true)
}

/// Handle one already-polled frame on a baked-image target: `HELLO` -> `HELLO_ACK`,
/// `RUN_IMAGE` -> run-from-RAM + `RUN_RESULT`, `DBG_IMAGE` -> a debug session. Anything
/// else (e.g. `RUN_PROGRAM`: this target is PE-less) is consumed -- the host learns the
/// supported surface from the `HELLO_ACK` capabilities. Split out so a deploy-capable
/// serve loop can dispatch the deploy messages itself and delegate the rest here.
#[cfg(feature = "baked-image")]
fn serve_frame_baked(
    transport: &mut impl Transport,
    frame: lamella_wire::Frame,
) -> Result<(), TransportError> {
    use lamella_wire::msg;
    match frame.msg_type {
        msg::HELLO => hello_reply(transport, &frame)?,
        repl::RUN_IMAGE => {
            let result = run_image(frame.payload);
            transport.send(repl::RUN_RESULT, frame.seq, &result.encode())?;
        }
        debug::DBG_IMAGE => {
            run_debug_session(transport, frame.payload, frame.seq)?;
        }
        _ => {}
    }
    Ok(())
}

/// Serve one pending request on a DEPLOY-capable baked-image target: `DEPLOY_IMAGE`
/// programs the image into `flash` and keeps it (boots on reset); `DEPLOY_CLEAR` erases
/// it; every other frame is delegated to [`serve_frame_baked`]. A device firmware's serve
/// loop is this call in a loop (plus its own arena reset between requests).
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn serve_one_deploy(
    transport: &mut impl Transport,
    flash: &mut impl FlashSink,
) -> Result<bool, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(false);
    };
    match frame.msg_type {
        deploy::DEPLOY_IMAGE => {
            let ok = flash.program(&frame.payload);
            transport.send(deploy::DEPLOY_RESULT, frame.seq, &[u8::from(ok)])?;
        }
        deploy::DEPLOY_CLEAR => {
            flash.erase();
            transport.send(deploy::DEPLOY_RESULT, frame.seq, &[1])?;
        }
        deploy::DEPLOY_CHUNK => {
            let payload = &frame.payload;
            let ok = if payload.len() >= 8 {
                let offset = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                let total = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
                flash.program_chunk(offset, &payload[8..], total)
            } else {
                false
            };
            transport.send(deploy::DEPLOY_RESULT, frame.seq, &[u8::from(ok)])?;
        }
        _ => serve_frame_baked(transport, frame)?,
    }
    Ok(true)
}

/// Run a DEPLOYED baked image (booted from flash) at full speed, staying interruptible: it
/// drives the interpreter session in bounded bursts and polls the wire between them, so a
/// host that sends `HELLO` always takes the board back within a burst -- guaranteed,
/// because an interpreted app cannot execute without the firmware stepping it (there is no
/// "app masks interrupts and spins" failure mode a native deployment has). A trap or a
/// completed run returns [`Deployed::Completed`]; a `HELLO` returns [`Deployed::Interrupted`]
/// (acknowledged), and the firmware then serves the host's next command.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn run_deployed(
    transport: &mut impl Transport,
    module: &lamella_cil_runtime::Module,
    entry: lamella_cil_runtime::MethodId,
) -> Result<Deployed, TransportError> {
    use lamella_cil_runtime::{Session, Status};
    use lamella_wire::msg;
    let mut vm = Vm::default();
    #[cfg(target_os = "none")]
    vm.set_mmio(lamella_mmio::write32, lamella_mmio::read32);
    vm.set_memory_backend(Box::new(SafeMemory::new()));
    let mut session = match Session::new(module, entry, Vec::new()) {
        Ok(session) => session,
        Err(trap) => {
            return Ok(Deployed::Completed(failure(&format!("session: {trap:?}"))));
        }
    };
    loop {
        for _ in 0..4096 {
            match session.step(module, &mut vm) {
                Ok(Status::Done(value)) => {
                    return Ok(Deployed::Completed(run_result_of(&vm, &value)));
                }
                Err(trap) => {
                    return Ok(Deployed::Completed(RunResult {
                        exit: 70,
                        stdout: format!("{}TRAP: {trap:?}", String::from_utf16_lossy(vm.output())),
                    }));
                }
                Ok(_) => {}
            }
        }
        if let Some(frame) = transport.poll()? {
            if frame.msg_type == msg::HELLO {
                hello_reply(transport, &frame)?;
                return Ok(Deployed::Interrupted);
            }
        }
    }
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
        #[cfg(feature = "baked-image")]
        if frame.msg_type == repl::RUN_IMAGE {
            let result = run_image(frame.payload);
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

/// Host driver: send a baked `.lmli` `image` to the target for execution. Unconditional (no
/// interpreter feature needed to DRIVE a device): the target answers with the same
/// [`repl::RUN_RESULT`] that [`try_recv_result`] reads.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
pub fn send_image(transport: &mut impl Transport, seq: u16, image: &[u8]) -> Result<(), TransportError> {
    transport.send(repl::RUN_IMAGE, seq, image)
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

    #[cfg(feature = "baked-image")]
    #[test]
    fn run_image_round_trips_over_the_wire() {
        use lamella_wire::MemTransport;

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wireline/tests/fixtures/hello.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        send_image(&mut driver, 9, &image).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one(&mut runner, &[]).unwrap(), "the runner handled a RUN_IMAGE");
        driver.feed(&runner.take_sent());

        let result = try_recv_result(&mut driver, 9).unwrap().expect("a result arrived");
        assert_eq!(result.exit, 7);
        assert_eq!(result.stdout, "hi\n");
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn serve_one_baked_negotiates_then_runs_an_image() {
        use lamella_wire::{Capabilities, Hello, HelloAck, MemTransport, PROTOCOL_VERSION, ProtocolRange, msg};

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wireline/tests/fixtures/hello.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();

        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE | Capabilities::REPL_RUN),
        };
        driver.send(msg::HELLO, 1, &hello.encode()).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the target handled the HELLO");
        driver.feed(&runner.take_sent());
        let ack_frame = driver.poll().unwrap().expect("a HELLO_ACK arrived");
        assert_eq!(ack_frame.msg_type, msg::HELLO_ACK);
        let ack = HelloAck::decode(&ack_frame.payload).expect("decodes");
        assert_eq!(ack.chosen, PROTOCOL_VERSION);
        assert!(ack.caps.has(Capabilities::BAKED_IMAGE));

        send_image(&mut driver, 2, &image).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the target ran the image");
        driver.feed(&runner.take_sent());
        let result = try_recv_result(&mut driver, 2).unwrap().expect("a result arrived");
        assert_eq!(result.exit, 7);
        assert_eq!(result.stdout, "hi\n");
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn debug_session_steps_breaks_and_completes_over_the_wire() {
        use lamella_wire::MemTransport;

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wireline/tests/fixtures/hello.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");

        let stopped = |frame: &lamella_wire::Frame| {
            assert_eq!(frame.msg_type, debug::EVT_STOPPED, "expected a stop event");
            let method = u32::from_le_bytes(frame.payload[1..5].try_into().unwrap());
            let offset = u32::from_le_bytes(frame.payload[5..9].try_into().unwrap());
            (frame.payload[0], method, offset)
        };

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        driver.send(debug::DBG_IMAGE, 1, &image).unwrap();
        driver.send(debug::DBG_STEP, 2, &[]).unwrap();
        driver.send(debug::DBG_STEP, 3, &[]).unwrap();
        driver.send(debug::DBG_STACK, 4, &[]).unwrap();
        driver.send(debug::DBG_DETACH, 5, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the target served the debug session");
        driver.feed(&runner.take_sent());

        let entry_stop = driver.poll().unwrap().expect("an entry stop");
        let (why, entry_method, entry_offset) = stopped(&entry_stop);
        assert_eq!(why, debug::reason::ENTRY);
        let step1 = driver.poll().unwrap().expect("a step stop");
        let (why, method_1, offset_1) = stopped(&step1);
        assert_eq!(why, debug::reason::STEP);
        assert_eq!(method_1, entry_method);
        assert_ne!((method_1, offset_1), (entry_method, entry_offset), "the step advanced");
        let step2 = driver.poll().unwrap().expect("a second step stop");
        let (why, _, _) = stopped(&step2);
        assert_eq!(why, debug::reason::STEP);
        let frames = driver.poll().unwrap().expect("a stack reply");
        assert_eq!(frames.msg_type, debug::DBG_FRAMES);
        let count = u16::from_le_bytes(frames.payload[0..2].try_into().unwrap());
        assert!(count >= 1, "at least the entry frame");
        let top_method = u32::from_le_bytes(frames.payload[2..6].try_into().unwrap());
        assert_eq!(top_method, entry_method, "innermost frame first");
        let ack = driver.poll().unwrap().expect("a detach ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);

        let mut break_payload = Vec::new();
        break_payload.extend_from_slice(&1u16.to_le_bytes());
        break_payload.extend_from_slice(&method_1.to_le_bytes());
        break_payload.extend_from_slice(&offset_1.to_le_bytes());
        driver.send(debug::DBG_IMAGE, 6, &image).unwrap();
        driver.send(debug::DBG_BREAK, 7, &break_payload).unwrap();
        driver.send(debug::DBG_RESUME, 8, &[]).unwrap();
        driver.send(debug::DBG_RESUME, 9, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the target served session B");
        driver.feed(&runner.take_sent());

        let (why, _, _) = stopped(&driver.poll().unwrap().expect("entry stop B"));
        assert_eq!(why, debug::reason::ENTRY);
        let ack = driver.poll().unwrap().expect("a breakpoint ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);
        let hit = driver.poll().unwrap().expect("a breakpoint stop");
        let (why, hit_method, hit_offset) = stopped(&hit);
        assert_eq!(why, debug::reason::BREAKPOINT);
        assert_eq!((hit_method, hit_offset), (method_1, offset_1));
        let done = driver.poll().unwrap().expect("a done stop");
        let (why, _, _) = stopped(&done);
        assert_eq!(why, debug::reason::DONE);
        let result = RunResult::decode(&done.payload[9..]).expect("a run result tail");
        assert_eq!(result.exit, 7);
        assert_eq!(result.stdout, "hi\n");
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn a_hello_mid_debug_session_ends_it_for_the_new_host() {
        use lamella_wire::{Capabilities, Hello, MemTransport, PROTOCOL_VERSION, ProtocolRange, msg};

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wireline/tests/fixtures/hello.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        driver.send(debug::DBG_IMAGE, 1, &image).unwrap();
        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE),
        };
        driver.send(msg::HELLO, 2, &hello.encode()).unwrap();
        send_image(&mut driver, 3, &image).unwrap();
        runner.feed(&driver.take_sent());

        assert!(serve_one_baked(&mut runner).unwrap(), "the stale debug session served + ended");
        assert!(serve_one_baked(&mut runner).unwrap(), "the fresh RUN_IMAGE served");
        driver.feed(&runner.take_sent());

        let entry = driver.poll().unwrap().expect("the stale session's entry stop");
        assert_eq!(entry.msg_type, debug::EVT_STOPPED);
        let ack = driver.poll().unwrap().expect("the successor's HELLO_ACK");
        assert_eq!(ack.msg_type, msg::HELLO_ACK);
        let result = try_recv_result(&mut driver, 3).unwrap().expect("a fresh run result");
        assert_eq!(result.exit, 7);
    }

    #[cfg(feature = "baked-image")]
    #[derive(Default)]
    struct MockFlash {
        data: Vec<u8>,
    }

    #[cfg(feature = "baked-image")]
    impl FlashSink for MockFlash {
        fn image_slice(&self) -> &'static [u8] {
            &[]
        }
        fn erase(&mut self) {
            self.data.clear();
        }
        fn program(&mut self, image: &[u8]) -> bool {
            self.data = image.to_vec();
            true
        }
        fn program_chunk(&mut self, offset: usize, chunk: &[u8], total: usize) -> bool {
            if offset == 0 {
                self.data = vec![0xff; total];
            }
            if offset + chunk.len() > self.data.len() {
                return false;
            }
            self.data[offset..offset + chunk.len()].copy_from_slice(chunk);
            true
        }
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn chunked_deploy_reassembles_an_over_64k_image() {
        use lamella_wire::MemTransport;

        let image: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let total = image.len();
        let chunk_len = 32_768usize;
        let mut sink = MockFlash::default();

        let mut offset = 0;
        let mut seq = 0u16;
        while offset < total {
            let end = (offset + chunk_len).min(total);
            let mut payload = Vec::new();
            payload.extend_from_slice(&(offset as u32).to_le_bytes());
            payload.extend_from_slice(&(total as u32).to_le_bytes());
            payload.extend_from_slice(&image[offset..end]);

            let mut driver = MemTransport::new();
            let mut runner = MemTransport::new();
            driver.send(deploy::DEPLOY_CHUNK, seq, &payload).unwrap();
            runner.feed(&driver.take_sent());
            assert!(serve_one_deploy(&mut runner, &mut sink).unwrap(), "the target handled a chunk");
            driver.feed(&runner.take_sent());
            let ack = driver.poll().unwrap().expect("a chunk ack");
            assert_eq!(ack.msg_type, deploy::DEPLOY_RESULT);
            assert_eq!(ack.payload, vec![1], "the chunk verified");

            offset = end;
            seq += 1;
        }
        assert_eq!(sink.data, image, "the reassembled image matches the original");
    }
}
