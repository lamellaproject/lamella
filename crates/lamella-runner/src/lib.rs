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
    /// `(method_id: u32 LE, offset: u32 LE)`. Replies [`DBG_ACK`]. Accepted BOTH while halted
    /// AND mid-run (during a [`DBG_RESUME`]) -- a breakpoint set while the program runs takes
    /// effect on its next hit, so the IDE can add one without first pausing.
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
    /// Host -> target: debug the PERSISTENTLY DEPLOYED image (the flash region
    /// `DEPLOY_IMAGE`/`DEPLOY_CHUNK` programmed) IN PLACE -- no payload, so attaching to a
    /// ~190 KB deployed app costs nothing on the wire. The target boots that image HALTED
    /// at the entry and replies [`EVT_STOPPED`] (reason `Entry`); from there the session is
    /// identical to a [`DBG_IMAGE`] one. A missing/corrupt image answers [`EVT_STOPPED`]
    /// (reason `Trap`) carrying the boot error. Served by deploy-capable targets
    /// ([`crate::serve_one_deploy`]), advertised as `Capabilities::DEBUG_ATTACH`.
    pub const DBG_ATTACH: u8 = 0x1A;

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

/// Wireline message types for PROFILE INTROSPECTION (the 0x30 range): the board tells the IDE
/// what it is. The `HELLO_ACK` already carries the compact [`lamella_wire::ProfileIdentity`]
/// (abi level + surface hash + name) at zero extra round-trips; this pair pulls the FULL
/// resident manifest only when the host's cache misses that hash
/// (docs/deployment-tiers.md's identity/manifest split).
pub mod profile {
    /// Host -> target: request the resident-profile manifest. Empty payload. Answered by
    /// [`PROFILE_MANIFEST`].
    pub const GET_PROFILE: u8 = 0x30;
    /// Target -> host: the manifest ([`lamella_wire::ProfileManifest`] bytes -- the identity +
    /// the complete intrinsic-id listing of the resident surface).
    pub const PROFILE_MANIFEST: u8 = 0x31;
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
    /// Host -> target: query the deployed image (no payload). Answered by [`DEPLOY_STATUS_RESULT`].
    /// Lets a host skip re-deploying an image the target already holds -- content-addressed deploy,
    /// the phone-style "already installed, just run it" flow.
    pub const DEPLOY_STATUS: u8 = 0x27;
    /// Target -> host: `present(u8) | checksum(u64 LE)` -- present=1 with the stored image's content
    /// checksum (`lamella_cil_runtime::baked_image_checksum`) if a valid image is deployed, else
    /// present=0 (checksum 0). The host compares it to a freshly-baked image's checksum.
    pub const DEPLOY_STATUS_RESULT: u8 = 0x28;
    /// Host -> target: boot the deployed image NOW -- a clean self-reset into the boot-run path, so
    /// deploy->run needs no debug probe. No reply and no payload (the target resets).
    pub const DEPLOY_RUN: u8 = 0x29;
    /// Host -> target: begin a WINC1500 module-firmware write. Payload =
    /// `offset(u32 LE) | total(u32 LE)`: the firmware initializes the module into its flash
    /// download mode and erases the module-flash span `[offset, offset + total)`. Answered by
    /// [`WINC_FW_RESULT`] (the erase of a full ~332 KB image takes seconds -- hosts wait
    /// generously). A target without a WINC module answers `ok = 0`.
    pub const WINC_FW_START: u8 = 0x2a;
    /// Host -> target: one chunk of the module-firmware image. Payload =
    /// `offset(u32 LE, absolute module-flash address) | chunk bytes`; the firmware programs the
    /// chunk into the erased span and verifies it by read-back. Answered by [`WINC_FW_RESULT`]
    /// per chunk, so the STREAM never needs the whole image in the target's RAM -- a 32 KB part
    /// updates a 332 KB module image.
    pub const WINC_FW_CHUNK: u8 = 0x2b;
    /// Host -> target: the module-firmware write is complete (no payload). The firmware runs its
    /// final sanity read and parks the module for a clean reboot into the new firmware. Answered
    /// by [`WINC_FW_RESULT`].
    pub const WINC_FW_END: u8 = 0x2c;
    /// Target -> host: `ok(u8)` -- 1 if the begin/program/finish step succeeded, 0 on any module
    /// wire fault, verify mismatch, or a target without a module flasher.
    pub const WINC_FW_RESULT: u8 = 0x2d;
}

/// Re-exported for a deploy host: read a baked image's content checksum from its header, to compare
/// against what a target already holds (content-addressed deploy-skip) without a direct dependency on
/// the interpreter crate.
pub use lamella_cil_runtime::baked_image_checksum;

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

/// A WINC1500 WiFi module's OWN firmware store, reachable from the firmware over the module's
/// SPI: the target side of the `WINC_FW_*` streaming update (the module image is far larger
/// than a small part's RAM, so the host chunks it and the firmware programs each chunk as it
/// arrives). The generic runner drives the protocol against this seam; a WiFi board implements
/// it over its hardware SPI, and a board without a module passes none (the runner then answers
/// every `WINC_FW_*` request `ok = 0`).
#[cfg(feature = "baked-image")]
pub trait WincFlasher {
    /// Brings the module into its flash download mode from scratch (a fresh power-up sequence,
    /// so it works whether or not the module's firmware was booted) and erases the module-flash
    /// span `[offset, offset + total)`. Returns whether the module answered sanely and the
    /// erase completed.
    fn begin(&mut self, offset: usize, total: usize) -> bool;
    /// Programs `data` at the absolute module-flash `offset` (within the span `begin` erased)
    /// and verifies it by read-back. Returns whether the readback matched.
    fn program(&mut self, offset: usize, data: &[u8]) -> bool;
    /// Finishes the update: a final module-flash read proves the wire stayed sane, then the
    /// module is parked (powered down) so the next board reset boots it fresh into the new
    /// firmware.
    fn finish(&mut self) -> bool;
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

/// What [`serve_one_deploy`] did with a pending frame -- so a firmware serve loop knows when the host
/// asked to BOOT the deployed image, which is a reset the firmware must perform itself (the runner is
/// target-agnostic and cannot reset the MCU).
#[cfg(feature = "baked-image")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Served {
    /// No frame was pending.
    Nothing,
    /// A frame was handled; keep serving.
    Handled,
    /// The host sent [`deploy::DEPLOY_RUN`]: the firmware should reset into its boot-run path.
    RunRequested,
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
/// The capability set every baked-image serve advertises. `PROFILE_CHIPID` is unconditional
/// -- this build's identity always carries the structured board/chip fields, even when a
/// firmware left them `0` = unknown.
fn serve_caps() -> lamella_wire::Capabilities {
    use lamella_wire::Capabilities;
    Capabilities(
        Capabilities::BAKED_IMAGE
            | Capabilities::DEBUG_BASIC
            | Capabilities::BREAKPOINTS
            | Capabilities::STEPPING
            | Capabilities::PROFILE_CHIPID,
    )
}

/// The board/chip identity words the firmware installed at boot ([`set_board_identity`]):
/// `board_model` in the low half of the first word; the IDCODE and device-id whole. Statics
/// (not parameters) because the identity is a property of the running FIRMWARE, set once at
/// boot and read wherever a `HELLO_ACK`/manifest is built -- the same shape as the other
/// boot-installed seams.
#[cfg(feature = "baked-image")]
static BOARD_MODEL: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "baked-image")]
static CHIP_IDCODE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
#[cfg(feature = "baked-image")]
static CHIP_DEVID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Installs the board/chip identity this firmware advertises in its `HELLO_ACK` profile
/// (docs in [`lamella_wire::ProfileIdentity`]): the board-model code from
/// [`lamella_wire::board_model`] (0 for a custom board), the chip's SW-DP IDCODE, and the
/// vendor device-id register value the firmware read from its own silicon (each 0 =
/// unknown). Call once at boot, before serving.
#[cfg(feature = "baked-image")]
pub fn set_board_identity(board_model: u16, chip_idcode: u32, chip_devid: u32) {
    use core::sync::atomic::Ordering;
    BOARD_MODEL.store(u32::from(board_model), Ordering::Relaxed);
    CHIP_IDCODE.store(chip_idcode, Ordering::Relaxed);
    CHIP_DEVID.store(chip_devid, Ordering::Relaxed);
}

/// [`serve_caps`] plus the deploy tier's extras: a target with a persistent image region
/// also debugs it in place ([`debug::DBG_ATTACH`]).
#[cfg(feature = "baked-image")]
fn deploy_caps() -> lamella_wire::Capabilities {
    lamella_wire::Capabilities(serve_caps().0 | lamella_wire::Capabilities::DEBUG_ATTACH)
}

/// This build's resident-profile identity (docs/deployment-tiers.md): the intrinsic-ABI level +
/// the registry fingerprint as the surface hash + the surface's name, all derived in
/// `intrinsic_registry` from the one feature set that shapes the registry. A Tier-2 target
/// folds its resident corlib's content hash in once one is resident.
#[cfg(feature = "baked-image")]
fn profile_identity() -> lamella_wire::ProfileIdentity {
    use core::sync::atomic::Ordering;
    use lamella_cil_runtime::intrinsic_registry;
    lamella_wire::ProfileIdentity::new(
        intrinsic_registry::INTRINSIC_ABI,
        intrinsic_registry::registry_fingerprint(),
        intrinsic_registry::profile_name(),
    )
    .with_chip(
        BOARD_MODEL.load(Ordering::Relaxed) as u16,
        CHIP_IDCODE.load(Ordering::Relaxed),
        CHIP_DEVID.load(Ordering::Relaxed),
    )
}

#[cfg(feature = "baked-image")]
fn hello_reply(
    transport: &mut impl Transport,
    frame: &lamella_wire::Frame,
) -> Result<(), TransportError> {
    hello_reply_caps(transport, frame, serve_caps())
}

#[cfg(feature = "baked-image")]
fn hello_reply_caps(
    transport: &mut impl Transport,
    frame: &lamella_wire::Frame,
    caps: lamella_wire::Capabilities,
) -> Result<(), TransportError> {
    use lamella_wire::{Hello, PROTOCOL_VERSION, ProtocolRange, msg, target_respond};
    let range = ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION };
    match Hello::decode(&frame.payload) {
        Some(hello) => match target_respond(&hello, range, caps) {
            Ok(mut ack) => {
                ack.profile = Some(profile_identity());
                transport.send(msg::HELLO_ACK, frame.seq, &ack.encode())
            }
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
    /// A mid-run `HELLO` reclaimed the target (already answered): the session is over.
    Reclaimed,
    /// A mid-run [`debug::DBG_DETACH`] (already acked): the session is over.
    Detached,
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

/// Applies a [`debug::DBG_BREAK`] payload to `session`: replace the whole breakpoint set with
/// the `(method_id, offset)` pairs it carries (`count(u16 LE)` then `count` x two u32 LE). On
/// the `in_place` tier the offset is the CIL byte offset, the domain `is_at_breakpoint` checks
/// the running ip against -- so a breakpoint applied here (even mid-run) fires on the next hit.
/// Shared by the halted command loop and the mid-run poll, so setting a breakpoint WHILE the
/// program runs behaves identically to setting one while it is stopped.
#[cfg(feature = "baked-image")]
fn apply_breakpoints(session: &mut lamella_cil_runtime::Session, payload: &[u8]) {
    session.clear_breakpoints();
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
}

/// Run until a breakpoint, completion, a trap, or a [`debug::DBG_PAUSE`]: bounded bursts
/// of steps with a wire poll between bursts, so a running target stays pause-able.
/// A mid-run `HELLO` is answered with `caps` and ends the run ([`RunStop::Reclaimed`]);
/// a mid-run detach is acked and ends it ([`RunStop::Detached`]) -- without these, a
/// resume over a non-terminating program left the wireline PERMANENTLY deaf on every
/// carrier (the F427 post-debug wedge: the blink loops forever, no breakpoint fires,
/// and the old loop dropped every reclaim HELLO here).
#[cfg(feature = "baked-image")]
fn run_until_stop(
    transport: &mut impl Transport,
    module: &lamella_cil_runtime::Module,
    vm: &mut Vm,
    session: &mut lamella_cil_runtime::Session,
    caps: lamella_wire::Capabilities,
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
            match frame.msg_type {
                debug::DBG_PAUSE => return Ok(RunStop::Paused),
                lamella_wire::msg::HELLO => {
                    hello_reply_caps(transport, &frame, caps)?;
                    return Ok(RunStop::Reclaimed);
                }
                debug::DBG_DETACH => {
                    transport.send(debug::DBG_ACK, frame.seq, &[])?;
                    return Ok(RunStop::Detached);
                }
                debug::DBG_BREAK => {
                    apply_breakpoints(session, &frame.payload);
                    transport.send(debug::DBG_ACK, frame.seq, &[])?;
                }
                _ => {}
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
    let image: &'static [u8] = Box::leak(image.into_boxed_slice());
    run_debug_session_static(transport, image, image_seq, serve_caps(), &mut |_| {})
}

/// [`run_debug_session`] over an ALREADY-RESIDENT image -- the [`debug::DBG_ATTACH`] arm:
/// a deploy-capable target debugs the flash region it already holds, so nothing but
/// commands crosses the wire. `caps` is what a mid-session `HELLO` (a NEW host adopting a
/// stale session) is answered with -- the serve tier's own set. `configure` is the
/// firmware's [`Vm`] hook, the SAME one every other run path gets -- an attach-run must see
/// the board's backends (network, output policy), or "attach and resume to completion"
/// silently runs a lesser machine than a deployed boot does.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn run_debug_session_static(
    transport: &mut impl Transport,
    image: &'static [u8],
    image_seq: u16,
    caps: lamella_wire::Capabilities,
    configure: &mut dyn FnMut(&mut Vm),
) -> Result<(), TransportError> {
    use debug::reason;
    use lamella_cil_runtime::{Module, Session, Status};

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
    configure(&mut vm);
    if let Err(trap) = boot_static_ctors(&module, &mut vm) {
        let result = failure(&format!("static constructor: {trap:?}"));
        return send_stopped(transport, image_seq, reason::TRAP, (0, 0), Some(&result));
    }
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
                match run_until_stop(transport, &module, &mut vm, &mut session, caps)? {
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
                    RunStop::Reclaimed | RunStop::Detached => return Ok(()),
                }
            }
            debug::DBG_BREAK => {
                apply_breakpoints(&mut session, &frame.payload);
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
                hello_reply_caps(transport, &frame, caps)?;
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
    run_image_with(image, &mut |_vm| {})
}

/// [`run_image`] with a firmware hook that configures the fresh [`Vm`] before anything
/// runs (static constructors included): a device installs its board seams here -- the
/// clock, the networking backend -- exactly as a host embedder does after `Vm::default()`.
/// The runner cannot know a board's backends (they live in the firmware crate), so the
/// firmware hands them in per evaluation.
#[cfg(feature = "baked-image")]
#[must_use]
pub fn run_image_with(image: Vec<u8>, configure: &mut dyn FnMut(&mut Vm)) -> RunResult {
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
    configure(&mut vm);
    if boot_static_ctors(&module, &mut vm).is_err() {
        return RunResult { exit: 70, stdout: String::from_utf16_lossy(vm.output()) };
    }
    let outcome = run(&module, &mut vm, entry, Vec::new());
    let exit = match outcome {
        Ok(Some(Value::Int32(code))) => code,
        Ok(_) => 0,
        Err(_) => 70,
    };
    RunResult { exit, stdout: String::from_utf16_lossy(vm.output()) }
}

/// Boots a baked image's static constructors EAGERLY (the lazy-trigger tables
/// of II.10.5.3 are loader-built and not part of the baked format), then marks every type
/// initialized. EVERY baked-image entry path must run this before the entry method --
/// transient RUN_IMAGE, the flash boot-run, and a debug session alike; a path that skips it
/// runs the program with null corlib statics (`IPAddress.Any`, `Encoding.ASCII`, ...).
///
/// # Errors
/// The first trapping constructor's [`lamella_cil_runtime::Trap`] (its console output is in
/// the [`Vm`] for the caller to flush).
#[cfg(feature = "baked-image")]
fn boot_static_ctors(
    module: &lamella_cil_runtime::Module,
    vm: &mut Vm,
) -> Result<(), lamella_cil_runtime::Trap> {
    for &cctor in module.static_ctors() {
        lamella_cil_runtime::Session::new(module, cctor, Vec::new())
            .and_then(|mut session| session.run(module, vm))?;
    }
    vm.mark_all_cctors_run(module);
    Ok(())
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
    serve_frame_baked(transport, frame, &mut |_vm| {})?;
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
    configure: &mut dyn FnMut(&mut Vm),
) -> Result<(), TransportError> {
    use lamella_wire::msg;
    match frame.msg_type {
        msg::HELLO => hello_reply(transport, &frame)?,
        repl::RUN_IMAGE => {
            let result = run_image_with(frame.payload, configure);
            transport.send(repl::RUN_RESULT, frame.seq, &result.encode())?;
        }
        debug::DBG_IMAGE => {
            let image: &'static [u8] = alloc::boxed::Box::leak(frame.payload.into_boxed_slice());
            run_debug_session_static(transport, image, frame.seq, serve_caps(), configure)?;
        }
        profile::GET_PROFILE => {
            let manifest = lamella_wire::ProfileManifest {
                identity: profile_identity(),
                intrinsic_ids: lamella_cil_runtime::intrinsic_registry::registry_ids().collect(),
            };
            transport.send(profile::PROFILE_MANIFEST, frame.seq, &manifest.encode())?;
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
) -> Result<Served, TransportError> {
    serve_one_deploy_with(transport, flash, &mut |_vm| {}, None)
}

/// [`serve_one_deploy`] with the firmware's [`Vm`]-configure hook (see
/// [`run_image_with`]) -- every evaluation the serve runs gets the board seams installed --
/// and, on a WiFi board, its [`WincFlasher`] so a host can stream a module-firmware update
/// over the wire (`None` answers every `WINC_FW_*` request `ok = 0`).
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn serve_one_deploy_with(
    transport: &mut impl Transport,
    flash: &mut impl FlashSink,
    configure: &mut dyn FnMut(&mut Vm),
    mut winc: Option<&mut dyn WincFlasher>,
) -> Result<Served, TransportError> {
    let Some(frame) = transport.poll()? else {
        return Ok(Served::Nothing);
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
        deploy::WINC_FW_START => {
            let payload = &frame.payload;
            let ok = match (payload.len() >= 8, winc.as_deref_mut()) {
                (true, Some(flasher)) => {
                    let offset =
                        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                    let total =
                        u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]) as usize;
                    flasher.begin(offset, total)
                }
                _ => false,
            };
            transport.send(deploy::WINC_FW_RESULT, frame.seq, &[u8::from(ok)])?;
        }
        deploy::WINC_FW_CHUNK => {
            let payload = &frame.payload;
            let ok = match (payload.len() >= 4, winc.as_deref_mut()) {
                (true, Some(flasher)) => {
                    let offset =
                        u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
                    flasher.program(offset, &payload[4..])
                }
                _ => false,
            };
            transport.send(deploy::WINC_FW_RESULT, frame.seq, &[u8::from(ok)])?;
        }
        deploy::WINC_FW_END => {
            let ok = match winc.as_deref_mut() {
                Some(flasher) => flasher.finish(),
                None => false,
            };
            transport.send(deploy::WINC_FW_RESULT, frame.seq, &[u8::from(ok)])?;
        }
        deploy::DEPLOY_STATUS => {
            let (present, checksum) = match baked_image_checksum(flash.image_slice()) {
                Some(sum) => (1u8, sum),
                None => (0u8, 0u64),
            };
            let mut payload = [0u8; 9];
            payload[0] = present;
            payload[1..].copy_from_slice(&checksum.to_le_bytes());
            transport.send(deploy::DEPLOY_STATUS_RESULT, frame.seq, &payload)?;
        }
        deploy::DEPLOY_RUN => {
            return Ok(Served::RunRequested);
        }
        lamella_wire::msg::HELLO => hello_reply_caps(transport, &frame, deploy_caps())?,
        debug::DBG_ATTACH => {
            run_debug_session_static(transport, flash.image_slice(), frame.seq, deploy_caps(), configure)?;
        }
        _ => serve_frame_baked(transport, frame, configure)?,
    }
    Ok(Served::Handled)
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
    run_deployed_with(transport, module, entry, &mut |_vm| {})
}

/// [`run_deployed`] with the firmware's [`Vm`]-configure hook (see [`run_image_with`]):
/// the deployed app's [`Vm`] gets the board seams before its static constructors run.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(feature = "baked-image")]
pub fn run_deployed_with(
    transport: &mut impl Transport,
    module: &lamella_cil_runtime::Module,
    entry: lamella_cil_runtime::MethodId,
    configure: &mut dyn FnMut(&mut Vm),
) -> Result<Deployed, TransportError> {
    use lamella_cil_runtime::{Session, Status};
    use lamella_wire::msg;
    let mut vm = Vm::default();
    #[cfg(target_os = "none")]
    vm.set_mmio(lamella_mmio::write32, lamella_mmio::read32);
    vm.set_memory_backend(Box::new(SafeMemory::new()));
    configure(&mut vm);
    if let Err(trap) = boot_static_ctors(module, &mut vm) {
        return Ok(Deployed::Completed(RunResult {
            exit: 70,
            stdout: format!(
                "{}BOOT TRAP (static constructor): {trap:?}",
                String::from_utf16_lossy(vm.output())
            ),
        }));
    }
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
                hello_reply_caps(transport, &frame, deploy_caps())?;
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
    fn hello_ack_and_get_profile_report_the_resident_surface() {
        use lamella_cil_runtime::intrinsic_registry;
        use lamella_wire::{
            Capabilities, Hello, HelloAck, MemTransport, PROTOCOL_VERSION, ProfileManifest,
            ProtocolRange, msg,
        };

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE),
        };
        driver.send(msg::HELLO, 1, &hello.encode()).unwrap();
        driver.send(profile::GET_PROFILE, 2, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the HELLO was served");
        assert!(serve_one_baked(&mut runner).unwrap(), "the GET_PROFILE was served");
        driver.feed(&runner.take_sent());

        let ack = driver.poll().unwrap().expect("a HELLO_ACK");
        let identity = HelloAck::decode(&ack.payload)
            .expect("the ack decodes")
            .profile
            .expect("the ack advertises an identity");
        assert_eq!(identity.abi, intrinsic_registry::INTRINSIC_ABI);
        assert_eq!(identity.hash, intrinsic_registry::registry_fingerprint());
        assert_eq!(identity.name(), intrinsic_registry::profile_name());

        let frame = driver.poll().unwrap().expect("a manifest reply");
        assert_eq!(frame.msg_type, profile::PROFILE_MANIFEST);
        let manifest = ProfileManifest::decode(&frame.payload).expect("the manifest decodes");
        assert_eq!(manifest.identity, identity);
        assert_eq!(manifest.intrinsic_ids.len(), intrinsic_registry::registry_ids().count());
        assert!(
            manifest
                .intrinsic_ids
                .contains(&intrinsic_registry::intrinsic_id("console_write_line_empty")),
            "a known intrinsic id is listed"
        );
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn a_hello_reclaims_a_debug_session_resumed_into_an_infinite_program() {
        use lamella_wire::{
            Capabilities, Hello, HelloAck, MemTransport, PROTOCOL_VERSION, ProtocolRange, msg,
        };

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wireline/tests/fixtures/spin.exe"
        )) else {
            return;
        };
        let program: &'static [u8] = Box::leak(program.into_boxed_slice());
        let assembly = Assembly::read(program).expect("fixture parses");
        let loaded = lamella_load::load(&assembly).expect("fixture loads");
        let mut module = loaded.module;
        let image = module.write_baked(Some(loaded.entry)).expect("fixture bakes");
        let image: &'static [u8] = Box::leak(image.into_boxed_slice());

        let mut driver = MemTransport::new();
        let mut target = MemTransport::new();
        driver.send(debug::DBG_RESUME, 7, &[]).unwrap();
        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE),
        };
        driver.send(msg::HELLO, 8, &hello.encode()).unwrap();
        target.feed(&driver.take_sent());

        run_debug_session_static(&mut target, image, 3, serve_caps(), &mut |_| {})
            .expect("the session ends instead of spinning forever");
        driver.feed(&target.take_sent());

        let stopped = driver.poll().unwrap().expect("the entry stop report");
        assert_eq!(stopped.msg_type, debug::EVT_STOPPED);
        let ack_frame = driver.poll().unwrap().expect("the reclaim HELLO_ACK");
        assert_eq!(ack_frame.msg_type, msg::HELLO_ACK);
        let ack = HelloAck::decode(&ack_frame.payload).expect("the ack decodes");
        assert!(ack.caps.has(Capabilities::BAKED_IMAGE));
        assert!(driver.poll().unwrap().is_none(), "nothing else in flight");
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn a_breakpoint_set_while_running_fires_on_the_next_hit() {
        use lamella_wire::MemTransport;

        let Ok(program) = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../lamella-wireline/tests/fixtures/spin.exe"
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

        const STEPS: u16 = 12;
        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        driver.send(debug::DBG_IMAGE, 1, &image).unwrap();
        for seq in 0..STEPS {
            driver.send(debug::DBG_STEP, 2 + seq, &[]).unwrap();
        }
        driver.send(debug::DBG_DETACH, 2 + STEPS, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(serve_one_baked(&mut runner).unwrap(), "the target served the steps");
        driver.feed(&runner.take_sent());
        let (why, _, _) = stopped(&driver.poll().unwrap().expect("entry stop"));
        assert_eq!(why, debug::reason::ENTRY);
        let mut loop_method = 0;
        let mut loop_offset = 0;
        for _ in 0..STEPS {
            let (why, method, offset) = stopped(&driver.poll().unwrap().expect("a step stop"));
            assert_eq!(why, debug::reason::STEP);
            loop_method = method;
            loop_offset = offset;
        }
        assert_eq!(driver.poll().unwrap().expect("phase-1 detach ack").msg_type, debug::DBG_ACK);

        let mut break_payload = Vec::new();
        break_payload.extend_from_slice(&1u16.to_le_bytes());
        break_payload.extend_from_slice(&loop_method.to_le_bytes());
        break_payload.extend_from_slice(&loop_offset.to_le_bytes());
        driver.send(debug::DBG_IMAGE, 4, &image).unwrap();
        driver.send(debug::DBG_RESUME, 5, &[]).unwrap();
        driver.send(debug::DBG_BREAK, 6, &break_payload).unwrap();
        driver.send(debug::DBG_DETACH, 7, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(
            serve_one_baked(&mut runner).unwrap(),
            "the session stops at the mid-run breakpoint instead of spinning forever"
        );
        driver.feed(&runner.take_sent());
        assert_eq!(
            stopped(&driver.poll().unwrap().expect("phase-2 entry stop")).0,
            debug::reason::ENTRY
        );

        let ack = driver.poll().unwrap().expect("the mid-run break ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);
        let (why, hit_method, hit_offset) = stopped(&driver.poll().unwrap().expect("breakpoint stop"));
        assert_eq!(why, debug::reason::BREAKPOINT, "the mid-run breakpoint fired");
        assert_eq!((hit_method, hit_offset), (loop_method, loop_offset));
        let detach = driver.poll().unwrap().expect("the detach ack");
        assert_eq!(detach.msg_type, debug::DBG_ACK);
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn dbg_attach_debugs_the_deployed_image_without_resending_it() {
        use lamella_wire::{
            Capabilities, Hello, HelloAck, MemTransport, PROTOCOL_VERSION, ProtocolRange, msg,
        };

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

        struct Deployed(&'static [u8]);
        impl FlashSink for Deployed {
            fn image_slice(&self) -> &'static [u8] {
                self.0
            }
            fn erase(&mut self) {}
            fn program(&mut self, _image: &[u8]) -> bool {
                false
            }
            fn program_chunk(&mut self, _offset: usize, _chunk: &[u8], _total: usize) -> bool {
                false
            }
        }
        let mut flash = Deployed(Box::leak(image.into_boxed_slice()));

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();

        let hello = Hello {
            range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
            caps: Capabilities(Capabilities::BAKED_IMAGE | Capabilities::DEBUG_ATTACH),
        };
        driver.send(msg::HELLO, 1, &hello.encode()).unwrap();
        runner.feed(&driver.take_sent());
        assert!(matches!(serve_one_deploy(&mut runner, &mut flash).unwrap(), Served::Handled));
        driver.feed(&runner.take_sent());
        let ack = driver.poll().unwrap().expect("a HELLO_ACK");
        assert_eq!(ack.msg_type, msg::HELLO_ACK);
        let ack = HelloAck::decode(&ack.payload).expect("the ack decodes");
        assert!(ack.caps.has(Capabilities::DEBUG_ATTACH), "the deploy serve advertises attach");

        driver.send(debug::DBG_ATTACH, 2, &[]).unwrap();
        driver.send(debug::DBG_STEP, 3, &[]).unwrap();
        driver.send(debug::DBG_STACK, 4, &[]).unwrap();
        driver.send(debug::DBG_DETACH, 5, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(matches!(serve_one_deploy(&mut runner, &mut flash).unwrap(), Served::Handled));
        driver.feed(&runner.take_sent());

        let entry = driver.poll().unwrap().expect("an entry stop");
        assert_eq!(entry.msg_type, debug::EVT_STOPPED);
        assert_eq!(entry.payload[0], debug::reason::ENTRY);
        let step = driver.poll().unwrap().expect("a step stop");
        assert_eq!(step.payload[0], debug::reason::STEP);
        let frames = driver.poll().unwrap().expect("a stack reply");
        assert_eq!(frames.msg_type, debug::DBG_FRAMES);
        let ack = driver.poll().unwrap().expect("a detach ack");
        assert_eq!(ack.msg_type, debug::DBG_ACK);

        let mut empty = Deployed(&[0xFF; 64]);
        driver.send(debug::DBG_ATTACH, 6, &[]).unwrap();
        runner.feed(&driver.take_sent());
        assert!(matches!(serve_one_deploy(&mut runner, &mut empty).unwrap(), Served::Handled));
        driver.feed(&runner.take_sent());
        let stop = driver.poll().unwrap().expect("a trap stop");
        assert_eq!(stop.msg_type, debug::EVT_STOPPED);
        assert_eq!(stop.payload[0], debug::reason::TRAP);
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
            assert_eq!(
                serve_one_deploy(&mut runner, &mut sink).unwrap(),
                Served::Handled,
                "the target handled a chunk"
            );
            driver.feed(&runner.take_sent());
            let ack = driver.poll().unwrap().expect("a chunk ack");
            assert_eq!(ack.msg_type, deploy::DEPLOY_RESULT);
            assert_eq!(ack.payload, vec![1], "the chunk verified");

            offset = end;
            seq += 1;
        }
        assert_eq!(sink.data, image, "the reassembled image matches the original");
    }

    #[cfg(feature = "baked-image")]
    #[derive(Default)]
    struct MockWincFlasher {
        begun: Option<(usize, usize)>,
        data: Vec<u8>,
        finished: bool,
    }

    #[cfg(feature = "baked-image")]
    impl WincFlasher for MockWincFlasher {
        fn begin(&mut self, offset: usize, total: usize) -> bool {
            self.begun = Some((offset, total));
            self.data = vec![0xff; offset + total];
            true
        }
        fn program(&mut self, offset: usize, data: &[u8]) -> bool {
            if offset + data.len() > self.data.len() {
                return false;
            }
            self.data[offset..offset + data.len()].copy_from_slice(data);
            true
        }
        fn finish(&mut self) -> bool {
            self.finished = true;
            true
        }
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn winc_firmware_streams_through_the_flasher_hook() {
        use lamella_wire::MemTransport;

        let firmware: Vec<u8> = (0..5000u32).map(|i| (i % 241) as u8).collect();
        let base = 4096usize;
        let chunk_len = 1024usize;
        let mut sink = MockFlash::default();
        let mut flasher = MockWincFlasher::default();

        let mut exchange = |msg_type: u8, seq: u16, payload: &[u8], flasher: &mut MockWincFlasher| {
            let mut driver = MemTransport::new();
            let mut runner = MemTransport::new();
            driver.send(msg_type, seq, payload).unwrap();
            runner.feed(&driver.take_sent());
            assert_eq!(
                serve_one_deploy_with(
                    &mut runner,
                    &mut sink,
                    &mut |_vm| {},
                    Some(flasher as &mut dyn WincFlasher),
                )
                .unwrap(),
                Served::Handled,
            );
            driver.feed(&runner.take_sent());
            let ack = driver.poll().unwrap().expect("a WINC_FW_RESULT ack");
            assert_eq!(ack.msg_type, deploy::WINC_FW_RESULT);
            assert_eq!(ack.payload, vec![1], "the step succeeded");
        };

        let mut start = Vec::new();
        start.extend_from_slice(&(base as u32).to_le_bytes());
        start.extend_from_slice(&(firmware.len() as u32).to_le_bytes());
        exchange(deploy::WINC_FW_START, 1, &start, &mut flasher);
        assert_eq!(flasher.begun, Some((base, firmware.len())));

        let mut offset = 0;
        let mut seq = 2u16;
        while offset < firmware.len() {
            let end = (offset + chunk_len).min(firmware.len());
            let mut payload = Vec::new();
            payload.extend_from_slice(&((base + offset) as u32).to_le_bytes());
            payload.extend_from_slice(&firmware[offset..end]);
            exchange(deploy::WINC_FW_CHUNK, seq, &payload, &mut flasher);
            offset = end;
            seq += 1;
        }
        exchange(deploy::WINC_FW_END, seq, &[], &mut flasher);

        assert!(flasher.finished, "END reached finish");
        assert_eq!(&flasher.data[base..], &firmware[..], "the programmed image matches");
    }

    #[cfg(feature = "baked-image")]
    #[test]
    fn a_target_without_a_winc_flasher_answers_not_ok() {
        use lamella_wire::MemTransport;

        let mut sink = MockFlash::default();
        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();
        driver.send(deploy::WINC_FW_START, 1, &[0, 0, 0, 0, 16, 0, 0, 0]).unwrap();
        runner.feed(&driver.take_sent());
        assert_eq!(
            serve_one_deploy_with(&mut runner, &mut sink, &mut |_vm| {}, None).unwrap(),
            Served::Handled,
        );
        driver.feed(&runner.take_sent());
        let ack = driver.poll().unwrap().expect("a WINC_FW_RESULT ack");
        assert_eq!(ack.msg_type, deploy::WINC_FW_RESULT);
        assert_eq!(ack.payload, vec![0], "no flasher answers not-ok");
    }
}
