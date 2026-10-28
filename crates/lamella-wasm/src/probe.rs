//! The in-page debug-probe agent (feature `probe`): drive a CMSIS-DAP probe, and the chip behind
//! it, from a WebAssembly host, and follow the chip's program's console in its RAM.

#![allow(unsafe_code)]

use core::cell::RefCell;

use lamella_cmsis_dap::{Dap, Transport, TransportError};
use lamella_probe_console::{
    END_MAGICS, LAMELLA_RING, NoRing, ProgramEnd, RHU_RING, RingReader, Seen, TargetMemory, ring_in,
};
use lamella_probe_core::{ArmDap, ProbeError, TargetAccess, TargetAccessExt, cortex_m};
use serde_json::{Value, json};

use crate::abi::result_buffer;


#[cfg(target_arch = "wasm32")]
mod imported {
    #[link(wasm_import_module = "lamella_host")]
    unsafe extern "C" {
        pub fn probe_write_packet(ptr: *const u8, len: usize) -> i32;
        pub fn probe_read_packet(ptr: *mut u8, cap: usize) -> i32;
    }
}

/// Hands one command packet to the host.
#[cfg(target_arch = "wasm32")]
fn host_write_packet(data: &[u8]) -> Result<(), TransportError> {
    let status = unsafe { imported::probe_write_packet(data.as_ptr(), data.len()) };
    if status < 0 {
        return Err(TransportError(format!("the host refused to send a packet ({status})")));
    }
    Ok(())
}

/// Takes one reply packet from the host.
#[cfg(target_arch = "wasm32")]
fn host_read_packet(buf: &mut [u8]) -> Result<usize, TransportError> {
    let got = unsafe { imported::probe_read_packet(buf.as_mut_ptr(), buf.len()) };
    if got < 0 {
        return Err(TransportError(format!("the host delivered no reply ({got})")));
    }
    Ok(got as usize)
}

#[cfg(not(target_arch = "wasm32"))]
mod native_host {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Computes the reply to one sent packet.
    pub type Responder = Box<dyn FnMut(&[u8]) -> Vec<u8>>;

    thread_local! {
        /// Packets the module has sent, oldest first.
        pub static SENT: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
        /// Replies a caller has queued for the module to read, oldest first.
        pub static REPLIES: RefCell<VecDeque<Vec<u8>>> = const { RefCell::new(VecDeque::new()) };
        /// What answers each packet as it is sent, when a caller has installed one.
        pub static RESPONDER: RefCell<Option<Responder>> = const { RefCell::new(None) };
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn host_write_packet(data: &[u8]) -> Result<(), TransportError> {
    native_host::SENT.with(|sent| sent.borrow_mut().push(data.to_vec()));
    let reply = native_host::RESPONDER
        .with(|responder| responder.borrow_mut().as_mut().map(|answer| answer(data)));
    if let Some(reply) = reply {
        queue_reply(reply);
    }
    Ok(())
}

#[cfg(not(target_arch = "wasm32"))]
fn host_read_packet(buf: &mut [u8]) -> Result<usize, TransportError> {
    let reply = native_host::REPLIES
        .with(|replies| replies.borrow_mut().pop_front())
        .ok_or_else(|| TransportError("no reply queued".into()))?;
    let copied = reply.len().min(buf.len());
    buf[..copied].copy_from_slice(&reply[..copied]);
    Ok(reply.len())
}

/// Queues a reply for the next `probe_read_packet`. Native builds only; the tests use it.
#[cfg(not(target_arch = "wasm32"))]
pub fn queue_reply(reply: Vec<u8>) {
    native_host::REPLIES.with(|replies| replies.borrow_mut().push_back(reply));
}

/// Answers every packet sent from here on with `responder`'s reply to it, so a test can stand a
/// model of a probe and a part behind the seam rather than queue each reply by hand. Native builds
/// only; the tests use it.
#[cfg(not(target_arch = "wasm32"))]
pub fn answer_with(responder: impl FnMut(&[u8]) -> Vec<u8> + 'static) {
    native_host::RESPONDER.with(|slot| *slot.borrow_mut() = Some(Box::new(responder)));
}

/// Every packet sent since [`reset_host`]. Native builds only; the tests use it.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn sent_packets() -> Vec<Vec<u8>> {
    native_host::SENT.with(|sent| sent.borrow().clone())
}

/// Empties both queues and removes any responder. Native builds only; the tests use it.
#[cfg(not(target_arch = "wasm32"))]
pub fn reset_host() {
    native_host::SENT.with(|sent| sent.borrow_mut().clear());
    native_host::REPLIES.with(|replies| replies.borrow_mut().clear());
    native_host::RESPONDER.with(|slot| *slot.borrow_mut() = None);
}

/// The packet transport a wasm host supplies, in the shape the CMSIS-DAP command layer consumes.
pub struct HostTransport;

impl Transport for HostTransport {
    fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError> {
        host_write_packet(data)
    }
    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        let got = host_read_packet(buf)?;
        if got > buf.len() {
            return Err(TransportError(format!(
                "the host reported {got} bytes of reply into a {}-byte buffer",
                buf.len()
            )));
        }
        Ok(got)
    }
}

/// One open probe session: the ARM debug bridge over the CMSIS-DAP command layer over the host's
/// packet transport.
pub type Session = ArmDap<Dap<HostTransport>>;

/// A probe session as the table holds it: the session, and the console it follows, if any.
struct Probe {
    session: Session,
    /// The console ring being followed, from [`lamella_probe_follow_console`] until the program's
    /// end arrives, a look at the ring fails, the follow is stopped, or the part is written again.
    console: Option<RingReader>,
}

thread_local! {
    /// Live probe sessions, indexed by `handle - 1`. wasm is single-threaded.
    static SESSIONS: RefCell<Vec<Option<Probe>>> = const { RefCell::new(Vec::new()) };
}

/// A board's memory as a console ring is read from it: plain reads through the memory access port,
/// which the part answers while its core runs, so following a console never halts the board.
struct RingMemory<'a>(&'a mut Session);

impl TargetMemory for RingMemory<'_> {
    fn read(&mut self, address: u32, into: &mut [u8]) -> Result<(), String> {
        if into.is_empty() {
            return Ok(());
        }
        let skip = (address & 3) as usize;
        let count = (skip + into.len()).div_ceil(4);
        let words = self
            .0
            .read_words(address & !3, count)
            .map_err(|e| e.to_string())?;
        for (index, byte) in into.iter_mut().enumerate() {
            let at = skip + index;
            *byte = words[at / 4].to_le_bytes()[at % 4];
        }
        Ok(())
    }
}

/// How many looks a follow waits through for a ring the board has not set up, when its caller does
/// not say: ten seconds of them at the 20 ms a page looks every.
const CONSOLE_PATIENCE: u32 = 500;

/// Why an ELF gives no console ring, as a sentence.
fn no_ring(why: NoRing) -> String {
    match why {
        NoRing::Neither => format!(
            "the ELF names no console ring, neither `{LAMELLA_RING}` nor `{RHU_RING}`, so the \
             program's console cannot be followed"
        ),
        NoRing::Both => format!(
            "the ELF names both `{LAMELLA_RING}` and `{RHU_RING}`, so which ring its program \
             writes cannot be told"
        ),
        NoRing::TooSmall { symbol, size } => {
            format!("the ELF's `{symbol}` is {size} bytes, too few for a console ring's header")
        }
    }
}

/// What one look at a followed ring found, as the host reads it: each entry an `out` of the bytes
/// the console carried, a `lost` count, a `restarted`, or the program's end with its code.
fn seen_json(seen: &[Seen]) -> Vec<Value> {
    seen.iter()
        .map(|item| match item {
            Seen::Out(bytes) => json!({ "out": bytes }),
            Seen::Lost(count) => json!({ "lost": count }),
            Seen::Restarted => json!({ "restarted": true }),
            Seen::Ended(ProgramEnd::Returned(code)) => json!({ "ended": "returned", "code": code }),
            Seen::Ended(ProgramEnd::Aborted(code)) => json!({ "ended": "aborted", "code": code }),
        })
        .collect()
}


/// The `DAP_Info` identifiers this module reads, from the Arm CMSIS-DAP command specification.
mod info_id {
    /// The probe vendor's name.
    pub const VENDOR: u8 = 0x01;
    /// The probe product's name.
    pub const PRODUCT: u8 = 0x02;
    /// The probe firmware's version string.
    pub const FIRMWARE: u8 = 0x04;
    /// The largest command packet the probe accepts, as a 16-bit value.
    pub const PACKET_SIZE: u8 = 0xff;
}

/// Reads a `u32` field, or reports which one was missing or malformed.
fn u32_field(request: &Value, name: &str) -> Result<u32, String> {
    request
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| format!("`{name}` is required and must be a 32-bit unsigned integer"))
}

/// Reads a `u8` field, or reports which one was missing or malformed.
fn u8_field(request: &Value, name: &str) -> Result<u8, String> {
    request
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(|| format!("`{name}` is required and must be an 8-bit unsigned integer"))
}

/// Reads an array-of-`u32` field, or reports which one was missing or malformed.
fn u32_array_field(request: &Value, name: &str) -> Result<Vec<u32>, String> {
    let items = request
        .get(name)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("`{name}` is required and must be an array"))?;
    items
        .iter()
        .map(|item| {
            item.as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| format!("every `{name}` entry must be a 32-bit unsigned integer"))
        })
        .collect()
}

/// Brings the wire up. `family` selects the connect sequence a part needs: an RP2350's debug port
/// powers up dormant and is addressed as ADIv6, so it takes the sequence in its own crate; anything
/// else takes the ADIv5 one.
fn connect(session: &mut Session, family: Option<&str>) -> Result<Value, String> {
    connect_part(session, family).map(|idcode| json!({ "idcode": idcode }))
}

/// [`connect`]'s sequence, returning the `DPIDR` the part's debug port answered.
fn connect_part(session: &mut Session, family: Option<&str>) -> Result<u32, String> {
    match family {
        Some("rp2350") => lamella_cmsis_dap_rp2350::connect(session).map_err(|e| e.to_string()),
        Some("rp2040") => lamella_cmsis_dap_rp2040::connect(session).map_err(|e| e.to_string()),
        _ => {
            session.connect().map_err(|e| e.to_string())?;
            let idcode = session.read_idcode().map_err(|e| e.to_string())?;
            session.init_mem().map_err(|e| e.to_string())?;
            Ok(idcode)
        }
    }
}

/// [`connect_part`] before a write, its refusal saying that nothing was written.
fn connect_for_write(session: &mut Session, family: &str) -> Result<u32, String> {
    connect_part(session, Some(family)).map_err(|why| {
        format!("the probe did not connect to the part, so nothing was written: {why}")
    })
}

/// The Cortex-M core-register selectors a stopped-state read reports: r0 through r15, then the
/// program status register. The architecture numbers them in that order, so the index into the
/// returned array is the selector.
const CORE_REG_COUNT: u8 = 17;

/// Runs one request against one session.
fn dispatch(probe: &mut Probe, request: &Value) -> Result<Value, String> {
    let op = request
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| "`op` is required and must be a string".to_string())?;
    let family = request.get("family").and_then(Value::as_str);
    let Probe { session, console } = probe;

    match op {
        "consolePoll" => {
            let reader = console.as_mut().ok_or_else(|| {
                "no console is being followed on this session: lamella_probe_follow_console starts \
                 a follow, and the program's end or a failed look ends one"
                    .to_string()
            })?;
            match reader.poll(&mut RingMemory(session)) {
                Ok(seen) => {
                    if seen.iter().any(|item| matches!(item, Seen::Ended(_))) {
                        *console = None;
                    }
                    Ok(json!({ "seen": seen_json(&seen) }))
                }
                Err(why) => {
                    *console = None;
                    Err(why)
                }
            }
        }
        "consoleStop" => {
            *console = None;
            Ok(json!({}))
        }
        "info" => {
            let dap = session.inner_mut();
            let vendor = dap.info_string(info_id::VENDOR).map_err(|e| e.to_string())?;
            let product = dap.info_string(info_id::PRODUCT).map_err(|e| e.to_string())?;
            let firmware = dap.info_string(info_id::FIRMWARE).map_err(|e| e.to_string())?;
            let raw = dap.info_bytes(info_id::PACKET_SIZE).map_err(|e| e.to_string())?;
            let packet_size = match raw.as_slice() {
                [low, high, ..] => u32::from(u16::from_le_bytes([*low, *high])),
                [only] => u32::from(*only),
                [] => 0,
            };
            Ok(json!({
                "vendor": vendor,
                "product": product,
                "firmware": firmware,
                "packetSize": packet_size,
            }))
        }
        "connect" => connect(session, family),
        "disconnect" => {
            session.inner_mut().release().map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        "halt" => session.halt().map(|()| json!({})).map_err(|e| e.to_string()),
        "resume" => session.resume().map(|()| json!({})).map_err(|e| e.to_string()),
        "step" => session.step().map(|()| json!({})).map_err(|e| e.to_string()),
        "waitHalted" => session.wait_halted().map(|()| json!({})).map_err(|e| e.to_string()),
        "isHalted" => session
            .is_halted()
            .map(|halted| json!({ "halted": halted }))
            .map_err(|e| e.to_string()),
        "resetRun" => session.reset_and_run().map(|()| json!({})).map_err(|e| e.to_string()),
        "resetHalt" => session.reset_and_halt().map(|()| json!({})).map_err(|e| e.to_string()),
        "armResetCatch" => session.arm_reset_catch().map(|()| json!({})).map_err(|e| e.to_string()),
        "disarmResetCatch" => {
            session.disarm_reset_catch().map(|()| json!({})).map_err(|e| e.to_string())
        }
        "handBack" => lamella_probe_core::hand_back(session)
            .map(|()| json!({}))
            .map_err(|why| why.to_string()),
        "stopAtEntry" => {
            *console = None;
            let family =
                family.ok_or_else(|| "`family` is required and must be a string".to_string())?;
            let entry = u32_field(request, "entry")?;
            stop_at_entry(session, family, entry)
        }
        "readWords" => {
            let address = u32_field(request, "address")?;
            let count = usize::try_from(u32_field(request, "count")?).unwrap_or(0);
            let values = session.read_words(address, count).map_err(|e| e.to_string())?;
            Ok(json!({ "values": values }))
        }
        "writeWords" => {
            let address = u32_field(request, "address")?;
            let values = u32_array_field(request, "values")?;
            session.write_words(address, &values).map_err(|e| e.to_string())?;
            Ok(json!({ "written": values.len() }))
        }
        "readCoreReg" => {
            let selector = u8_field(request, "selector")?;
            session
                .read_core_reg(selector)
                .map(|value| json!({ "value": value }))
                .map_err(|e| e.to_string())
        }
        "writeCoreReg" => {
            let selector = u8_field(request, "selector")?;
            let value = u32_field(request, "value")?;
            session.write_core_reg(selector, value).map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        "readCoreRegs" => {
            let mut values = Vec::with_capacity(CORE_REG_COUNT as usize);
            for selector in 0..CORE_REG_COUNT {
                values.push(session.read_core_reg(selector).map_err(|e| e.to_string())?);
            }
            Ok(json!({ "values": values }))
        }
        "setBreakpoints" => {
            let addresses = u32_array_field(request, "addresses")?;
            session.set_breakpoints(&addresses).map_err(|e| e.to_string())?;
            Ok(json!({ "requested": addresses.len() }))
        }
        "clearBreakpoints" => {
            session.clear_breakpoint().map_err(|e| e.to_string())?;
            Ok(json!({}))
        }
        other => Err(format!("no such operation: `{other}`")),
    }
}

/// Programs `image` into the part named by `family`, and hands the board back running it.
///
/// The routine that writes each part's flash is that part's own, and lives in the same crate a
/// command-line deploy calls: an nRF51 and an nRF52 are erased and programmed through the Nordic
/// Non-Volatile Memory Controller, and an RP2350 and an RP2040 by calling the bootrom's flash entry
/// points on the halted core. A family with no whole-image entry point of its own is refused by
/// name, because a deploy assembled out of primitives here would be a second implementation of a
/// sequence that belongs beside the controller it drives.
///
/// **A PART WHOSE ROUTINE DOES NOT CONNECT BY ITSELF IS CONNECTED FIRST.** The RP2350's and the
/// RP2040's routines begin with a transfer on a session they take as connected. On a probe nothing
/// has connected -- one freshly plugged, or one left in SWD mode with the part's debug port dormant
/// -- that first transfer goes unacknowledged and nothing is written. The Nordic routines connect
/// as their first step.
///
/// **AND THE BOARD IS HANDED BACK AS A DEBUG SESSION ENDS**, through
/// [`lamella_probe_core::hand_back`]: every hardware breakpoint removed, the core running its new
/// program, and halting debug off. Every routine here turns halting debug on and none turns it off,
/// and with it on, a `BKPT` the program meets later halts the core and waits for a debugger that
/// has gone. A hand-back that fails is reported, though the image is already written and verified,
/// because the board is not in the state the call promises.
fn flash(session: &mut Session, family: &str, image: &[u8]) -> Result<Value, String> {
    let written = match family {
        "nrf51" => lamella_cmsis_dap_nrf::flash_and_run(session, 0x0, image)
            .map(|report| json!({ "idcode": report.idcode, "bytes": report.bytes }))
            .map_err(|e| format!("{e:?}"))?,
        "nrf52" => lamella_cmsis_dap_nrf::erase_all_and_run(
            session,
            0x0,
            image,
            lamella_cmsis_dap_nrf::NRF52_IDCODE,
        )
        .map(|report| json!({ "idcode": report.idcode, "bytes": report.bytes }))
        .map_err(|e| format!("{e:?}"))?,
        "rp2040" => {
            let idcode = connect_for_write(session, family)?;
            let mut progress: Vec<String> = Vec::new();
            lamella_cmsis_dap_rp2040::flash_image(session, image, |line| {
                progress.push(line.to_string());
            })
            .map_err(|e| e.to_string())?;
            session.set_breakpoints(&[]).map_err(|e| e.to_string())?;
            session.reset_and_run().map_err(|e| e.to_string())?;
            json!({ "idcode": idcode, "bytes": image.len(), "progress": progress })
        }
        "rp2350" => {
            let idcode = connect_for_write(session, family)?;
            let mut progress: Vec<String> = Vec::new();
            lamella_cmsis_dap_rp2350::flash_image(session, image, |line| {
                progress.push(line.to_string());
            })
            .map_err(|e| e.to_string())?;
            json!({ "idcode": idcode, "bytes": image.len(), "progress": progress })
        }
        other => {
            return Err(format!(
                "no whole-image flash routine for `{other}`; the families with one are \
                 `nrf51`, `nrf52`, `rp2040` and `rp2350`"
            ));
        }
    };
    lamella_probe_core::hand_back(session).map_err(|why| {
        format!("the image was written and verified, but the board was not handed back: {why}")
    })?;
    Ok(written)
}

/// The core-register selector of the program counter.
const PC: u8 = 15;

/// The breakpoint unit's control register, `FP_CTRL`, at one address on every Cortex-M. It says
/// which revision of the unit a part implements, and so which addresses a comparator can match.
const FP_CTRL: u32 = 0xe000_2000;

/// Where the instruction addresses an Armv6-M or Armv7-M breakpoint unit can match end: a
/// comparator holds address bits `[28:2]` within the code region, which ends here (Armv6-M ARM,
/// C1.8.3; Armv7-M ARM, C1.11.5). An Armv8-M unit matches the whole address space.
const V1_BREAKPOINT_REACH: u32 = 0x2000_0000;

/// How many times a stop polls the core for its arrival at the image's entry, once it has
/// released the core from its reset, before it gives the start up as one that does not get there.
///
/// A poll is one read through the probe: about 0.4 ms through a Raspberry Pi Debug Probe, so this
/// is about four seconds there. The start it waits through is a boot ROM finding and entering the
/// image, then the image's own startup preparing its memory, which takes milliseconds -- but far
/// longer than the 128 polls that the wait for a halt allows.
const ENTRY_POLLS: u32 = 10_000;

/// Resets the part `family` names, catches its core at the reset, and runs it to `entry`, the
/// image's entry point as its ELF names it. The core is left halted there with halting debug on, for
/// a debugger to take over, and with nothing armed: no comparator, the breakpoint unit off, and no
/// vector catch.
///
/// The reset is each family's own. An RP2350's and an RP2040's is a system reset caught by the
/// vector catch, which keeps the debug port's selection; an nRF51's and an nRF52's is the shared
/// reset-and-halt, through the probe's reset line where it has one. A core caught at the entry
/// itself, on a part whose reset vector is the image's entry, stays there. Otherwise one comparator
/// is armed at the entry, the core runs from its reset through its boot ROM and the image's startup
/// to the entry, and the comparator is cleared once the core has stopped.
///
/// Returns the debug port's `idcode`, the `pc` the core stopped at, and how many `polls` its run to
/// the entry took, 0 where it was caught there.
///
/// # Errors
/// A family with no reset here; a probe that does not connect; an entry the part's breakpoint unit
/// cannot match, refused before the reset; a part that is not caught at its reset; and a core that
/// stops anywhere but the entry, or does not stop within [`ENTRY_POLLS`] polls. The core is then
/// left halted where it is, with nothing armed, and the refusal says where.
fn stop_at_entry(session: &mut Session, family: &str, entry: u32) -> Result<Value, String> {
    let reset_halt: fn(&mut Session) -> Result<(), ProbeError> = match family {
        "rp2350" => lamella_cmsis_dap_rp2350::reset_halt,
        "rp2040" => lamella_cmsis_dap_rp2040::reset_halt,
        "nrf51" | "nrf52" => <Session as TargetAccess>::reset_and_halt,
        other => {
            return Err(format!(
                "no stop at the entry for `{other}`; the families with one are `nrf51`, `nrf52`, \
                 `rp2040` and `rp2350`"
            ));
        }
    };
    let target = entry & !1;
    let idcode = connect_part(session, Some(family)).map_err(|why| {
        format!("the probe did not connect to the part, so it was not reset: {why}")
    })?;
    let unit = session.read_word(FP_CTRL).map_err(|why| {
        format!("could not read the part's breakpoint unit, so it was not reset: {why}")
    })?;
    let revision = cortex_m::fpb_revision_of(unit)
        .map_err(|why| format!("{why}, so the part was not reset"))?;
    if revision == cortex_m::FpbRevision::V1 && target >= V1_BREAKPOINT_REACH {
        return Err(format!(
            "the entry {target:#010x} is past what this part's breakpoint unit can match \
             (addresses below {V1_BREAKPOINT_REACH:#010x}), so the part was not reset"
        ));
    }
    reset_halt(session)
        .map_err(|why| format!("the part was not caught halted at its reset: {why}"))?;
    let caught = session
        .read_core_reg(PC)
        .map_err(|why| format!("could not read where the core was caught: {why}"))?;
    if caught == target {
        return Ok(json!({ "idcode": idcode, "pc": caught, "polls": 0 }));
    }
    if let Err(why) = session.set_breakpoints(&[target]) {
        let _ = session.set_breakpoints(&[]);
        return Err(format!(
            "could not arm a comparator at the entry {target:#010x}, so the core is halted at its \
             reset, {caught:#010x}: {why}"
        ));
    }
    if let Err(why) = session.resume() {
        let _ = session.set_breakpoints(&[]);
        return Err(format!("could not release the core from its reset: {why}"));
    }
    let mut polls = 0;
    let mut halted = false;
    let mut failed = None;
    while !halted && polls < ENTRY_POLLS {
        polls += 1;
        match session.is_halted() {
            Ok(now) => halted = now,
            Err(why) => failed = Some(why),
        }
    }
    if !halted && let Err(why) = session.halt().and_then(|()| session.wait_halted()) {
        let _ = session.set_breakpoints(&[]);
        return Err(format!(
            "the core did not reach the entry {target:#010x} in {ENTRY_POLLS} polls, and could not \
             be halted: {why}"
        ));
    }
    session
        .set_breakpoints(&[])
        .map_err(|why| format!("could not clear the comparator at the entry: {why}"))?;
    let pc = session
        .read_core_reg(PC)
        .map_err(|why| format!("could not read where the core stopped: {why}"))?;
    if !halted {
        let last = failed
            .map(|why| format!(" (the last poll failed: {why})"))
            .unwrap_or_default();
        return Err(format!(
            "the core did not reach the entry {target:#010x} in {ENTRY_POLLS} polls{last}; it is \
             halted at {pc:#010x}, with nothing armed"
        ));
    }
    if pc != target {
        return Err(format!(
            "the core stopped at {pc:#010x} before it reached the entry {target:#010x}; it is \
             halted there, with nothing armed"
        ));
    }
    Ok(json!({ "idcode": idcode, "pc": pc, "polls": polls }))
}

/// Wraps an operation's outcome in the envelope a host reads, so a failure arrives as data rather
/// than as an empty buffer a caller has to guess about.
fn envelope(outcome: Result<Value, String>) -> Vec<u8> {
    let body = match outcome {
        Ok(Value::Object(mut fields)) => {
            fields.insert("ok".into(), Value::Bool(true));
            Value::Object(fields)
        }
        Ok(other) => json!({ "ok": true, "value": other }),
        Err(message) => json!({ "ok": false, "error": message }),
    };
    serde_json::to_vec(&body).unwrap_or_default()
}

/// Runs `f` over the session `handle` names, or reports that the handle names none.
fn with_session(handle: u32, f: impl FnOnce(&mut Probe) -> Result<Value, String>) -> Vec<u8> {
    let outcome = SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        match sessions.get_mut((handle as usize).wrapping_sub(1)) {
            Some(Some(probe)) => f(probe),
            _ => Err(format!("no probe session with handle {handle}")),
        }
    });
    envelope(outcome)
}

/// Runs `f` over the session `handle` names, for the probe's debugger, which borrows the session
/// for each call it makes; `None` when the handle names none.
#[cfg(feature = "probe-debug")]
pub(crate) fn borrow_session<R>(handle: u32, f: impl FnOnce(&mut Session) -> R) -> Option<R> {
    SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        match sessions.get_mut((handle as usize).wrapping_sub(1)) {
            Some(Some(probe)) => Some(f(&mut probe.session)),
            _ => None,
        }
    })
}

/// Starts following the console ring `elf` names, replacing any follow the session had.
fn follow_console(probe: &mut Probe, elf: &[u8], patience: u32) -> Result<Value, String> {
    let (symbol, ring) = ring_in(elf).map_err(no_ring)?;
    probe.console = Some(RingReader::new(ring, END_MAGICS, patience));
    Ok(json!({ "symbol": symbol, "address": ring.address, "size": ring.size }))
}


/// Opens a probe session over the host's packet transport and returns a 1-based handle.
///
/// The host has already chosen and opened the physical probe; this call takes no device identity,
/// because the transport imports are the whole of what this module knows about it.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_probe_create() -> u32 {
    SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        sessions.push(Some(Probe {
            session: ArmDap::new(Dap::new(HostTransport)),
            console: None,
        }));
        u32::try_from(sessions.len()).unwrap_or(0)
    })
}

/// Runs one request (UTF-8 JSON at `ptr..ptr + len`) against the session `handle` names, and
/// returns `[u32 little-endian length][UTF-8 JSON]`; free it with
/// `lamella_dealloc(result, 4 + length)`.
///
/// The reply always carries an `ok` field. A refusal is `{"ok": false, "error": ...}` rather than an
/// empty result, so a host distinguishes "the probe said no" from "the call did not happen".
///
/// While a debug session holds the session (`lamella_probe_debug_create`, feature `probe-debug`),
/// every request but the reads, the memory writes and the console's own is refused, naming that
/// debug session, and the refusal changes nothing on the part or in the console follow.
///
/// # Safety
/// `ptr`/`len` must be the UTF-8 buffer the host filled via a prior `lamella_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_probe_request(handle: u32, ptr: *const u8, len: usize) -> *mut u8 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    let parsed = serde_json::from_slice::<Value>(bytes);
    let body = match parsed {
        Ok(request) => with_session(handle, |probe| {
            #[cfg(feature = "probe-debug")]
            crate::probe_debug::refuse_request(handle, &request)?;
            dispatch(probe, &request)
        }),
        Err(e) => envelope(Err(format!("the request is not JSON: {e}"))),
    };
    result_buffer(body)
}

/// Programs the image at `image_ptr..image_ptr + image_len` into the part whose family name is the
/// UTF-8 at `family_ptr..family_ptr + family_len`, connecting first where the part's write routine
/// does not, and hands the board back running the image with halting debug off. Returns the same
/// `[u32 length][UTF-8 JSON]` envelope as [`lamella_probe_request`].
///
/// The image stays binary rather than riding the JSON request, because a flash image is the one
/// payload here big enough for a text encoding of it to cost more than the transfer.
///
/// While a debug session holds the session (feature `probe-debug`), the write is refused, nothing
/// is written, and the console follow goes on.
///
/// # Safety
/// Both pointer/length pairs must be buffers the host filled via prior `lamella_alloc` calls.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_probe_flash(
    handle: u32,
    family_ptr: *const u8,
    family_len: usize,
    image_ptr: *const u8,
    image_len: usize,
) -> *mut u8 {
    let family_bytes = unsafe { core::slice::from_raw_parts(family_ptr, family_len) };
    let image = unsafe { core::slice::from_raw_parts(image_ptr, image_len) };
    let body = match core::str::from_utf8(family_bytes) {
        Ok(family) => with_session(handle, |probe| {
            #[cfg(feature = "probe-debug")]
            crate::probe_debug::refuse_write(handle)?;
            probe.console = None;
            flash(&mut probe.session, family, image)
        }),
        Err(_) => envelope(Err("the family name is not UTF-8".to_string())),
    };
    result_buffer(body)
}

/// Starts following the console ring that the ELF at `elf_ptr..elf_ptr + elf_len` names
/// (`__lamella_console_log`, or `__rhu_console_log` in an image Lamella RHU built) on the session
/// `handle` names. Returns the envelope [`lamella_probe_request`] does, with the ring's `symbol`,
/// `address` and `size`.
///
/// Each `{"op": "consolePoll"}` request after this one looks at the ring once and answers what
/// arrived since the look before, as `seen`: the bytes the program's console carried (`out`), bytes
/// the board overwrote before they were read (`lost`), a board that started again (`restarted`),
/// and at last the program's end (`ended`, `returned` or `aborted`, with its `code`). A look reads
/// the board's memory and nothing else, so the core is never halted. `patience` is how many looks
/// may find no ring set up before that is an error, and 0 asks for the default of 500. The follow
/// ends with the program's end, with a look that fails, with `{"op": "consoleStop"}`, and when the
/// part is written again.
///
/// The ELF stays binary rather than riding a JSON request, for the reason the image does.
///
/// # Safety
/// `elf_ptr`/`elf_len` must be a buffer the host filled via a prior `lamella_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_probe_follow_console(
    handle: u32,
    elf_ptr: *const u8,
    elf_len: usize,
    patience: u32,
) -> *mut u8 {
    let elf = unsafe { core::slice::from_raw_parts(elf_ptr, elf_len) };
    let patience = if patience == 0 {
        CONSOLE_PATIENCE
    } else {
        patience
    };
    result_buffer(with_session(handle, |probe| {
        follow_console(probe, elf, patience)
    }))
}

/// Ends a probe session. The host still owns the device and closes it separately.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_probe_dispose(handle: u32) {
    SESSIONS.with(|sessions| {
        if let Some(slot) = sessions.borrow_mut().get_mut((handle as usize).wrapping_sub(1)) {
            *slot = None;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cmsis_dap::proto;

    /// A `DAP_Transfer` reply: the command byte, the completed count, an OK acknowledge, then one
    /// little-endian word per completed read.
    fn transfer_reply(words: &[u32]) -> Vec<u8> {
        let mut reply = vec![proto::cmd::TRANSFER, u8::try_from(words.len()).unwrap_or(0), 0x01];
        for word in words {
            reply.extend_from_slice(&word.to_le_bytes());
        }
        reply
    }

    /// A `DAP_TransferBlock` read reply: the command byte, the completed count as a 16-bit value,
    /// an OK acknowledge, then one little-endian word per completed transfer.
    fn block_reply(words: &[u32]) -> Vec<u8> {
        let mut reply = vec![proto::cmd::TRANSFER_BLOCK];
        reply.extend_from_slice(&u16::try_from(words.len()).unwrap_or(0).to_le_bytes());
        reply.push(0x01);
        for word in words {
            reply.extend_from_slice(&word.to_le_bytes());
        }
        reply
    }

    /// Queues the PAIR of replies one `readWords` needs: the MEM-AP layer writes the transfer
    /// address register and then reads the data register as a block, and those are two commands.
    fn queue_word_read(words: &[u32]) {
        queue_reply(transfer_reply(&[]));
        queue_reply(block_reply(words));
    }

    /// A reply to a command that answers with a single status byte.
    fn status_reply(command: u8) -> Vec<u8> {
        vec![command, 0x00]
    }

    /// Runs a JSON request against a fresh session and returns the parsed envelope.
    fn request(handle: u32, body: &Value) -> Value {
        let bytes = serde_json::to_vec(body).unwrap();
        take_result(unsafe { lamella_probe_request(handle, bytes.as_ptr(), bytes.len()) })
    }

    /// Reads, frees and parses a `[u32 len][json]` result this module allocated.
    fn take_result(raw: *mut u8) -> Value {
        let json = unsafe {
            let length = u32::from_le_bytes(core::slice::from_raw_parts(raw, 4).try_into().unwrap())
                as usize;
            let json = core::slice::from_raw_parts(raw.add(4), length).to_vec();
            crate::abi::lamella_dealloc(raw, 4 + length);
            json
        };
        serde_json::from_slice(&json).unwrap()
    }

    /// Programs `image` into the part `family` names, through the exported call, and returns the
    /// parsed envelope.
    fn flash_request(handle: u32, family: &str, image: &[u8]) -> Value {
        take_result(unsafe {
            lamella_probe_flash(
                handle,
                family.as_ptr(),
                family.len(),
                image.as_ptr(),
                image.len(),
            )
        })
    }

    /// Starts following the console ring `elf` names, through the exported call, and returns the
    /// parsed envelope.
    fn follow_request(handle: u32, elf: &[u8], patience: u32) -> Value {
        take_result(unsafe {
            lamella_probe_follow_console(handle, elf.as_ptr(), elf.len(), patience)
        })
    }

    fn fresh_session() -> u32 {
        reset_host();
        lamella_probe_create()
    }

    /// A memory read must reach the transport as CMSIS-DAP commands and come back as the word the
    /// probe reported. This is the whole stack -- dispatcher, MEM-AP layer, command layer, host
    /// transport -- exercised end to end with only the wire replaced.
    #[test]
    fn a_word_read_crosses_the_host_seam_and_returns_the_probes_value() {
        let handle = fresh_session();
        queue_word_read(&[0x1234_5678]);
        let reply = request(handle, &json!({ "op": "readWords", "address": 0x2000_0000u32, "count": 1 }));
        assert_eq!(reply["ok"], json!(true), "{reply}");
        assert_eq!(reply["values"], json!([0x1234_5678u32]));
        assert!(!sent_packets().is_empty(), "the read reached the transport");
        lamella_probe_dispose(handle);
    }

    /// A host that reports more bytes than the buffer it was handed is refused rather than
    /// believed. Nothing downstream can tell an over-long count from a genuinely long reply -- the
    /// command layer would go on to decode bytes the host never wrote -- so the check has to be at
    /// the seam, and the refusal has to name both numbers.
    #[test]
    fn a_reply_longer_than_the_buffer_is_refused_at_the_seam() {
        let capacity = lamella_cmsis_dap::MAX_PACKET;
        let over_long = capacity + 32;
        let handle = fresh_session();
        queue_reply(vec![proto::cmd::TRANSFER; over_long]);
        let reply = request(handle, &json!({ "op": "readWords", "address": 0u32, "count": 1 }));
        assert_eq!(reply["ok"], json!(false), "{reply}");
        let error = reply["error"].as_str().unwrap_or_default();
        assert!(
            error.contains(&over_long.to_string()),
            "the refusal names the reported length: {reply}"
        );
        assert!(
            error.contains(&capacity.to_string()),
            "the refusal names the buffer: {reply}"
        );
        lamella_probe_dispose(handle);
    }

    /// A reply that FITS is passed straight through. Without this row the refusal above would be
    /// satisfied by a seam that rejects every reply, and every other test here would still pass on
    /// its error path.
    #[test]
    fn a_reply_that_fits_is_not_refused() {
        let handle = fresh_session();
        queue_word_read(&[0xdead_beef]);
        let reply = request(handle, &json!({ "op": "readWords", "address": 0u32, "count": 1 }));
        assert_eq!(reply["ok"], json!(true), "{reply}");
        assert_eq!(reply["values"], json!([0xdead_beefu32]));
        lamella_probe_dispose(handle);
    }

    /// An unknown operation names itself in the refusal. A dispatcher that returned an empty result
    /// would be indistinguishable from a call that never reached the module.
    #[test]
    fn an_unknown_operation_is_named_in_the_refusal() {
        let handle = fresh_session();
        let reply = request(handle, &json!({ "op": "teleport" }));
        assert_eq!(reply["ok"], json!(false));
        assert!(
            reply["error"].as_str().unwrap_or_default().contains("teleport"),
            "the refusal names the operation: {reply}"
        );
        lamella_probe_dispose(handle);
    }

    /// A handle that names no session is refused, including one that has been disposed. A disposed
    /// slot stays in the table, so this is the case a bare index check would walk straight past.
    #[test]
    fn a_disposed_handle_is_refused() {
        let handle = fresh_session();
        lamella_probe_dispose(handle);
        let reply = request(handle, &json!({ "op": "isHalted" }));
        assert_eq!(reply["ok"], json!(false), "{reply}");
        let reply = request(handle + 500, &json!({ "op": "isHalted" }));
        assert_eq!(reply["ok"], json!(false), "{reply}");
    }

    /// A malformed request reports why. The parse failure and an operation failure must not look
    /// alike to a host deciding whether to retry.
    #[test]
    fn a_request_that_is_not_json_reports_that() {
        let handle = fresh_session();
        let bytes = b"{not json";
        let reply =
            take_result(unsafe { lamella_probe_request(handle, bytes.as_ptr(), bytes.len()) });
        assert_eq!(reply["ok"], json!(false));
        assert!(
            reply["error"]
                .as_str()
                .unwrap_or_default()
                .contains("not JSON"),
            "{reply}"
        );
        lamella_probe_dispose(handle);
    }

    /// A field the caller left out is named, rather than defaulted. A missing address silently
    /// becoming 0 would write to the vector table.
    #[test]
    fn a_missing_field_is_named_rather_than_defaulted() {
        let handle = fresh_session();
        let reply = request(handle, &json!({ "op": "writeWords", "values": [1u32] }));
        assert_eq!(reply["ok"], json!(false), "{reply}");
        assert!(reply["error"].as_str().unwrap_or_default().contains("address"), "{reply}");
        lamella_probe_dispose(handle);
    }

    /// An unknown flash family is refused by name and lists the ones that exist. A deploy that
    /// silently did nothing is the failure this refusal replaces.
    #[test]
    fn an_unknown_flash_family_is_refused_by_name() {
        let handle = fresh_session();
        let reply = flash_request(handle, "esp32", &[0u8; 4]);
        assert_eq!(reply["ok"], json!(false));
        let error = reply["error"].as_str().unwrap_or_default();
        assert!(error.contains("esp32"), "{reply}");
        for family in ["nrf51", "nrf52", "rp2040", "rp2350"] {
            assert!(error.contains(family), "the refusal lists `{family}`: {reply}");
        }
        lamella_probe_dispose(handle);
    }

    /// The probe's own identity comes back as strings and a packet size. `DAP_Info` involves no
    /// target at all, so this is the one exchange a host can run before anything is attached.
    #[test]
    fn probe_identity_reads_back_from_dap_info() {
        let handle = fresh_session();
        let mut vendor = vec![proto::cmd::INFO, 6];
        vendor.extend_from_slice(b"Vendor");
        let mut product = vec![proto::cmd::INFO, 5];
        product.extend_from_slice(b"Probe");
        let mut firmware = vec![proto::cmd::INFO, 3];
        firmware.extend_from_slice(b"2.1");
        queue_reply(vendor);
        queue_reply(product);
        queue_reply(firmware);
        queue_reply(vec![proto::cmd::INFO, 2, 0x00, 0x02]);
        let reply = request(handle, &json!({ "op": "info" }));
        assert_eq!(reply["ok"], json!(true), "{reply}");
        assert_eq!(reply["vendor"], json!("Vendor"));
        assert_eq!(reply["product"], json!("Probe"));
        assert_eq!(reply["firmware"], json!("2.1"));
        assert_eq!(
            reply["packetSize"],
            json!(512),
            "the packet size is the probe's, not a constant: {reply}"
        );
        lamella_probe_dispose(handle);
    }

    /// Two sessions get two handles and disposing one leaves the other usable, which is what makes
    /// the table an index rather than a single slot with a counter.
    #[test]
    fn sessions_are_independent() {
        reset_host();
        let first = lamella_probe_create();
        let second = lamella_probe_create();
        assert_ne!(first, second);
        lamella_probe_dispose(first);
        queue_word_read(&[0x0000_0000]);
        let reply = request(second, &json!({ "op": "readWords", "address": 0u32, "count": 1 }));
        assert_eq!(reply["ok"], json!(true), "{reply}");
        lamella_probe_dispose(second);
    }

    /// A status-only command reaches the probe and its acknowledgement comes back as `ok`.
    #[test]
    fn a_disconnect_reaches_the_probe() {
        let handle = fresh_session();
        queue_reply(status_reply(proto::cmd::DISCONNECT));
        let reply = request(handle, &json!({ "op": "disconnect" }));
        assert_eq!(reply["ok"], json!(true), "{reply}");
        assert_eq!(
            sent_packets().first().map(|packet| packet[0]),
            Some(proto::cmd::DISCONNECT),
            "the packet on the wire is DAP_Disconnect"
        );
        lamella_probe_dispose(handle);
    }

    /// A probe that takes a connect as a real one does: every port, clock, sequence and
    /// configuration command acknowledged, a read of DP address 0 answered with `dpidr` and one of
    /// CTRL/STAT with both power-up acknowledges. Every transfer after the first `budget` is
    /// answered FAULT, so a write that follows the connect stops instead of polling a core that no
    /// canned reply can halt.
    fn connecting_probe(dpidr: u32, budget: usize) -> impl FnMut(&[u8]) -> Vec<u8> {
        let mut transfers = 0;
        move |command| {
            let op = command[0];
            match op {
                proto::cmd::INFO => vec![op, 0],
                proto::cmd::CONNECT => vec![op, 0x01],
                proto::cmd::TRANSFER => {
                    transfers += 1;
                    if transfers > budget {
                        return vec![op, 0, 0x04];
                    }
                    let request = command[3];
                    let mut reply = vec![op, 1, 0x01];
                    let word = match request & 0x0f {
                        0x02 => Some(dpidr),
                        0x06 => Some(0xf000_0000),
                        read if read & 0x02 != 0 => Some(0),
                        _ => None,
                    };
                    if let Some(word) = word {
                        reply.extend_from_slice(&word.to_le_bytes());
                    }
                    reply
                }
                proto::cmd::TRANSFER_BLOCK => vec![op, 0, 0, 0x04],
                _ => vec![op, 0x00],
            }
        }
    }

    /// **A WRITE CONNECTS FOR ITS FAMILY BEFORE ITS FIRST TRANSFER**, where the part's routine does
    /// not connect by itself. The RP2350's and RP2040's routines begin with a transfer, and a write
    /// whose first packet is that transfer writes nothing on a probe nothing has connected: a
    /// fresh probe answers it with no acknowledge, and a part whose debug port is dormant answers
    /// nothing. The connect sets the SWD clock to 1 MHz before anything else is clocked, and on
    /// the RP2040 it addresses core 0's port on the multi-drop bus.
    #[test]
    fn a_write_connects_for_its_family_before_its_first_transfer() {
        for (family, dpidr) in [
            ("rp2350", 0x4c01_3477),
            ("rp2040", lamella_cmsis_dap_rp2040::RP2040_DPIDR),
        ] {
            let handle = fresh_session();
            answer_with(connecting_probe(dpidr, 30));
            let reply = flash_request(handle, family, &[0x5a; 512]);
            let sent = sent_packets();
            let ops: Vec<u8> = sent.iter().map(|packet| packet[0]).collect();
            let first_transfer = ops.iter().position(|&op| op == proto::cmd::TRANSFER);
            let before = |op: u8| {
                let at = ops.iter().position(|&o| o == op);
                at.is_some_and(|at| Some(at) < first_transfer)
            };
            assert!(
                before(proto::cmd::CONNECT),
                "{family}: DAP_Connect goes out before the first transfer ({:02x?}...; {reply})",
                &ops[..ops.len().min(6)]
            );
            let clock = sent
                .iter()
                .find(|packet| packet[0] == proto::cmd::SWJ_CLOCK);
            let rate = clock.map(|p| u32::from_le_bytes([p[1], p[2], p[3], p[4]]));
            assert_eq!(
                rate,
                Some(1_000_000),
                "{family}: the clock is the part's connect rate"
            );
            assert!(
                before(proto::cmd::SWJ_CLOCK),
                "{family}: and set before the first transfer"
            );
            if family == "rp2040" {
                assert!(
                    before(proto::cmd::SWD_SEQUENCE),
                    "rp2040: core 0's port is selected on the bus before the first transfer"
                );
            }
            lamella_probe_dispose(handle);
        }
    }

    /// An nRF51 behind a probe, as far as a whole write, its hand-back and a stop at an image's
    /// entry reach: the debug port and one MEM-AP over word-addressed memory, the NVMC erasing pages
    /// and programming the flash at 0, halting debug as DHCSR reports it, the system reset and the
    /// reset vector catch, the program counter through the core-register transfer, and the
    /// breakpoint unit. Flash is NOR, so a word programmed over an unerased one keeps only the bits
    /// both share.
    struct Nrf51 {
        words: std::collections::HashMap<u32, u32>,
        tar: u32,
        config: u32,
        debugen: bool,
        halted: bool,
        breakpoint_unit: bool,
        comparators: [u32; 4],
        /// How many memory writes the probe has made, so a reader that writes nothing can say so.
        writes: usize,
        /// The program counter, as the core-register transfer reads and writes it.
        pc: u32,
        /// The core-register transfer's data word.
        dcrdr: u32,
        /// `DEMCR`, whose bit 0 is the reset vector catch.
        demcr: u32,
        /// How many system resets the part has taken.
        resets: usize,
        /// Where the image's startup calls once the core leaves its reset, and how many DHCSR reads
        /// the core takes to get there while it runs; none where the model runs no startup.
        startup: Option<(u32, usize)>,
        /// While the core runs its startup, how many DHCSR reads are left before the call.
        starting: Option<usize>,
        /// Whether the core sits at its reset vector with its startup not yet begun.
        at_reset: bool,
    }

    impl Nrf51 {
        const FLASH_END: u32 = 0x0004_0000;
        const NVMC_READY: u32 = 0x4001_e400;
        const NVMC_CONFIG: u32 = 0x4001_e504;
        const NVMC_ERASEPAGE: u32 = 0x4001_e508;
        const AIRCR: u32 = 0xe000_ed0c;
        const DHCSR: u32 = 0xe000_edf0;
        const DCRSR: u32 = 0xe000_edf4;
        const DCRDR: u32 = 0xe000_edf8;
        const DEMCR: u32 = 0xe000_edfc;
        const FP_CTRL: u32 = 0xe000_2000;
        const FP_COMP0: u32 = 0xe000_2008;

        /// As a debug session leaves one: halting debug on, the breakpoint unit on and two
        /// comparators armed.
        fn after_a_debug_session() -> Self {
            Nrf51 {
                words: std::collections::HashMap::new(),
                tar: 0,
                config: 0,
                debugen: true,
                halted: false,
                breakpoint_unit: true,
                comparators: [0x0000_0101, 0x0000_0201, 0, 0],
                writes: 0,
                pc: 0,
                dcrdr: 0,
                demcr: 0,
                resets: 0,
                startup: None,
                starting: None,
                at_reset: false,
            }
        }

        /// Running an image as a write hands a board back -- halting debug off, nothing armed --
        /// whose vector table names `reset` and whose startup calls `calls` `polls` DHCSR reads after
        /// the core leaves its reset.
        fn running_an_image(reset: u32, calls: u32, polls: usize) -> Self {
            let mut part = Nrf51::after_a_debug_session();
            part.debugen = false;
            part.breakpoint_unit = false;
            part.comparators = [0; 4];
            part.words.insert(0, 0x2000_4000);
            part.words.insert(4, reset);
            part.startup = Some((calls, polls));
            part
        }

        fn comparator(address: u32) -> Option<usize> {
            let index = address.checked_sub(Self::FP_COMP0)? / 4;
            (address.is_multiple_of(4) && index < 4).then_some(index as usize)
        }

        /// Whether the breakpoint unit breaks at `address`: the unit on, and an enabled comparator
        /// on the word holding it whose match field selects its halfword (Armv6-M ARM, C1.8.3).
        fn breaks_at(&self, address: u32) -> bool {
            let halfword = if address & 2 != 0 { 0b10 } else { 0b01 };
            self.breakpoint_unit
                && self.comparators.iter().any(|&comparator| {
                    comparator & 1 != 0
                        && comparator & 0x1fff_fffc == address & 0x1fff_fffc
                        && (comparator >> 30) & halfword != 0
                })
        }

        /// A system reset: the core takes the reset handler the vector table at 0 names, and halts
        /// there when the vector catch is armed or a halt was requested; otherwise it runs its
        /// startup.
        fn reset(&mut self) {
            self.resets += 1;
            self.pc = self.read(4) & !1;
            self.starting = None;
            self.at_reset = true;
            self.halted = self.debugen && (self.demcr & 1 != 0 || self.halted);
            if !self.halted {
                self.start();
            }
        }

        /// The core leaves its reset vector and runs its startup.
        fn start(&mut self) {
            self.at_reset = false;
            self.starting = self.startup.map(|(_, polls)| polls.max(1));
        }

        /// One DHCSR read of a running core: its startup moves on by one, and at the call the
        /// core stops where a comparator breaks.
        fn tick(&mut self) {
            let Some(left) = self.starting else { return };
            if left > 1 {
                self.starting = Some(left - 1);
                return;
            }
            self.starting = None;
            if let Some((calls, _)) = self.startup {
                self.pc = calls & !1;
                self.halted = self.debugen && self.breaks_at(self.pc);
            }
        }

        /// A read as the probe makes one: a DHCSR read of a running core is a look that lets its
        /// startup move on.
        fn look(&mut self, address: u32) -> u32 {
            if address == Self::DHCSR && !self.halted {
                self.tick();
            }
            self.read(address)
        }

        fn read(&self, address: u32) -> u32 {
            match address {
                Self::NVMC_READY => 1,
                Self::DHCSR => u32::from(self.debugen) | 1 << 16 | u32::from(self.halted) << 17,
                Self::DCRDR => self.dcrdr,
                Self::DEMCR => self.demcr,
                Self::FP_CTRL => 0x40 | u32::from(self.breakpoint_unit),
                _ if Self::comparator(address).is_some() => {
                    Self::comparator(address).map_or(0, |index| self.comparators[index])
                }
                _ if address < Self::FLASH_END => {
                    self.words.get(&address).copied().unwrap_or(u32::MAX)
                }
                _ => self.words.get(&address).copied().unwrap_or(0),
            }
        }

        fn write(&mut self, address: u32, value: u32) {
            self.writes += 1;
            match address {
                Self::NVMC_CONFIG => self.config = value,
                Self::NVMC_ERASEPAGE if self.config == 2 => {
                    for word in 0..256 {
                        self.words.remove(&((value & !0x3ff) + word * 4));
                    }
                }
                Self::DHCSR if value >> 16 == 0xa05f => {
                    self.debugen = value & 1 != 0;
                    if self.debugen {
                        let resumed = self.halted && value & 2 == 0;
                        self.halted = value & 2 != 0;
                        if resumed && self.at_reset {
                            self.start();
                        }
                    }
                }
                Self::AIRCR if value >> 16 == 0x05fa && value & 4 != 0 => self.reset(),
                Self::DEMCR => self.demcr = value,
                Self::DCRDR => self.dcrdr = value,
                Self::DCRSR if value & (1 << 16) != 0 => {
                    if value & 0x7f == 15 {
                        self.pc = self.dcrdr;
                    }
                }
                Self::DCRSR => self.dcrdr = if value & 0x7f == 15 { self.pc } else { 0 },
                Self::FP_CTRL if value & 2 != 0 => self.breakpoint_unit = value & 1 != 0,
                _ if Self::comparator(address).is_some() => {
                    if let Some(index) = Self::comparator(address) {
                        self.comparators[index] = value;
                    }
                }
                _ if address < Self::FLASH_END => {
                    if self.config == 1 {
                        let programmed = self.read(address) & value;
                        self.words.insert(address, programmed);
                    }
                }
                _ => {
                    self.words.insert(address, value);
                }
            }
        }

        /// One DP or AP register access; the word a read returns.
        fn transfer(&mut self, request: u8, data: u32) -> Option<u32> {
            let read = request & 0x02 != 0;
            match (request & 0x01 != 0, request & 0x0c) {
                (false, 0x0) if read => Some(lamella_cmsis_dap_nrf::NRF51_IDCODE),
                (false, 0x4) if read => Some(0xf000_0000),
                (true, 0x4) if !read => {
                    self.tar = data;
                    None
                }
                (true, 0xc) => {
                    let address = self.tar;
                    self.tar += 4;
                    if read {
                        Some(self.look(address))
                    } else {
                        self.write(address, data);
                        None
                    }
                }
                _ => read.then_some(0),
            }
        }

        /// The reply to one command packet.
        fn answer(&mut self, command: &[u8]) -> Vec<u8> {
            let op = command[0];
            let word = |at: usize| {
                command
                    .get(at..at + 4)
                    .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            };
            match op {
                proto::cmd::INFO if command[1] == 0xff => vec![op, 2, 64, 0],
                proto::cmd::INFO => vec![op, 0],
                proto::cmd::CONNECT => vec![op, 0x01],
                proto::cmd::TRANSFER => {
                    let mut reply = vec![op, 1, 0x01];
                    if let Some(value) = self.transfer(command[3], word(4)) {
                        reply.extend_from_slice(&value.to_le_bytes());
                    }
                    reply
                }
                proto::cmd::TRANSFER_BLOCK => {
                    let count = usize::from(u16::from_le_bytes([command[2], command[3]]));
                    let mut reply = vec![op, command[2], command[3], 0x01];
                    for index in 0..count {
                        if let Some(value) = self.transfer(command[4], word(5 + index * 4)) {
                            reply.extend_from_slice(&value.to_le_bytes());
                        }
                    }
                    reply
                }
                _ => vec![op, 0x00],
            }
        }
    }

    /// **A WRITE HANDS THE BOARD BACK**: the image written, the core running it, halting debug off
    /// and the breakpoint unit off with every comparator clear. Every write routine turns halting
    /// debug on and none turns it off, and with it on a `BKPT` the program meets later halts the
    /// core and waits for a debugger that has gone. The part starts as a debug session leaves it,
    /// with two comparators armed. An nRF51's write needs only memory and the NVMC, so a model this
    /// small runs the whole of it.
    #[test]
    fn a_write_hands_the_board_back_running_with_halting_debug_off() {
        let handle = fresh_session();
        let part = std::rc::Rc::new(std::cell::RefCell::new(Nrf51::after_a_debug_session()));
        let model = std::rc::Rc::clone(&part);
        answer_with(move |command| model.borrow_mut().answer(command));
        let image: Vec<u8> = (0..2048u32).map(|index| (index % 251) as u8).collect();
        let reply = flash_request(handle, "nrf51", &image);
        assert_eq!(reply["ok"], json!(true), "{reply}");
        let part = part.borrow();
        let flashed: Vec<u8> = (0..512u32)
            .flat_map(|word| part.read(word * 4).to_le_bytes())
            .collect();
        assert_eq!(flashed, image, "the flash holds the image");
        assert!(!part.halted, "the core runs");
        assert!(!part.debugen, "DHCSR.C_DEBUGEN is clear after the write");
        assert!(!part.breakpoint_unit, "the breakpoint unit is off");
        assert_eq!(part.comparators, [0; 4], "no comparator is armed");
        lamella_probe_dispose(handle);
    }

    /// An ELF naming `objects`, each a data object at an address with a size, as the class-library
    /// build names the console ring.
    fn elf_naming(objects: &[(&'static str, u32, u32)]) -> Vec<u8> {
        let named: Vec<lamella_elf::NamedObject<'_>> = objects
            .iter()
            .map(|&(name, address, size)| lamella_elf::NamedObject {
                name,
                section: name,
                address,
                size,
            })
            .collect();
        lamella_elf::write_debuggable_executable_with_objects(
            lamella_elf::Machine::Arm,
            &[0x00, 0xbf],
            0,
            0x1000_0000,
            true,
            &[],
            &named,
        )
    }

    /// The board's side of a console ring in the model's RAM, as an image's console keeps it:
    /// armed, written to and ended straight into memory, so none of it counts as a probe's write.
    struct BoardRing {
        at: u32,
        size: u32,
        written: u32,
    }

    impl BoardRing {
        /// The ring at `at` with a `size`-byte buffer, set up as a start sets it: the magic last.
        fn arm(part: &mut Nrf51, at: u32, size: u32) -> BoardRing {
            part.words.insert(at + 4, size);
            part.words.insert(at + 8, 0);
            part.words.insert(at + 12, 0);
            part.words.insert(at, lamella_probe_console::RUNNING);
            BoardRing {
                at,
                size,
                written: 0,
            }
        }

        /// The console carries `text`: each byte at the count modulo the size, then the count.
        fn emit(&mut self, part: &mut Nrf51, text: &[u8]) {
            for &byte in text {
                let address = self.at + 16 + self.written % self.size;
                let word = part.words.get(&(address & !3)).copied().unwrap_or(0);
                let shift = (address & 3) * 8;
                let merged = (word & !(0xff << shift)) | (u32::from(byte) << shift);
                part.words.insert(address & !3, merged);
                self.written += 1;
            }
            part.words.insert(self.at + 8, self.written);
        }

        /// The program ends: the code first, then the end magic in the running magic's place.
        fn end(&self, part: &mut Nrf51, magic: u32, code: i32) {
            part.words.insert(self.at + 12, code as u32);
            part.words.insert(self.at, magic);
        }
    }

    /// **A FOLLOWED CONSOLE ARRIVES IN ORDER, THEN THE PROGRAM's END, AND THE BOARD IS ONLY READ.**
    /// The second look's bytes wrap the ten-byte buffer at an offset no word boundary falls on, so
    /// the reads that cross the wrap are cut from whole words. After the end, the follow is over.
    #[test]
    fn a_followed_console_arrives_in_order_then_its_end_and_the_board_is_only_read() {
        let handle = fresh_session();
        let part = std::rc::Rc::new(std::cell::RefCell::new(Nrf51::after_a_debug_session()));
        let model = std::rc::Rc::clone(&part);
        answer_with(move |command| model.borrow_mut().answer(command));
        assert_eq!(
            request(handle, &json!({ "op": "connect" }))["ok"],
            json!(true)
        );
        let at = 0x2000_0100;
        let mut ring = BoardRing::arm(&mut part.borrow_mut(), at, 10);
        let followed = follow_request(handle, &elf_naming(&[(LAMELLA_RING, at, 16 + 10)]), 0);
        assert_eq!(followed["ok"], json!(true), "{followed}");
        assert_eq!(followed["symbol"], json!(LAMELLA_RING));
        assert_eq!(followed["address"], json!(at));
        assert_eq!(followed["size"], json!(26));
        let writes = part.borrow().writes;

        ring.emit(&mut part.borrow_mut(), b"Hello, ");
        let first = request(handle, &json!({ "op": "consolePoll" }));
        assert_eq!(
            first["seen"],
            json!([{ "out": b"Hello, ".to_vec() }]),
            "{first}"
        );

        ring.emit(&mut part.borrow_mut(), b"board\n");
        ring.end(&mut part.borrow_mut(), END_MAGICS.returned, 70_000);
        let second = request(handle, &json!({ "op": "consolePoll" }));
        assert_eq!(
            second["seen"],
            json!([{ "out": b"board\n".to_vec() }, { "ended": "returned", "code": 70_000 }]),
            "{second}"
        );
        assert_eq!(
            part.borrow().writes,
            writes,
            "a follow writes nothing to the board"
        );

        let after = request(handle, &json!({ "op": "consolePoll" }));
        assert_eq!(after["ok"], json!(false), "{after}");
        let error = after["error"].as_str().unwrap_or_default();
        assert!(error.contains("no console is being followed"), "{after}");
        lamella_probe_dispose(handle);
    }

    /// **AN ELF THAT NAMES NO RING IS REFUSED BY THE NAMES LOOKED FOR**, and nothing is followed.
    #[test]
    fn an_elf_naming_no_ring_is_refused_by_the_names_looked_for() {
        let handle = fresh_session();
        let reply = follow_request(
            handle,
            &elf_naming(&[("something_else", 0x2000_0100, 64)]),
            0,
        );
        assert_eq!(reply["ok"], json!(false), "{reply}");
        let error = reply["error"].as_str().unwrap_or_default();
        assert!(
            error.contains(LAMELLA_RING) && error.contains(RHU_RING),
            "{reply}"
        );
        let poll = request(handle, &json!({ "op": "consolePoll" }));
        assert_eq!(poll["ok"], json!(false), "{poll}");
        lamella_probe_dispose(handle);
    }

    /// **A FOLLOW ENDS WHEN THE PART IS WRITTEN AGAIN, AND WHEN IT IS STOPPED**: it belongs to the
    /// image it was started for, and the ring of the image that replaces it is another ring.
    #[test]
    fn writing_the_part_or_stopping_the_follow_ends_it() {
        let handle = fresh_session();
        let part = std::rc::Rc::new(std::cell::RefCell::new(Nrf51::after_a_debug_session()));
        let model = std::rc::Rc::clone(&part);
        answer_with(move |command| model.borrow_mut().answer(command));
        let at = 0x2000_0100;
        BoardRing::arm(&mut part.borrow_mut(), at, 64);
        let elf = elf_naming(&[(LAMELLA_RING, at, 16 + 64)]);

        assert_eq!(follow_request(handle, &elf, 0)["ok"], json!(true));
        assert_eq!(
            flash_request(handle, "nrf51", &[0x5a; 64])["ok"],
            json!(true)
        );
        let poll = request(handle, &json!({ "op": "consolePoll" }));
        assert_eq!(
            poll["ok"],
            json!(false),
            "the write ended the follow: {poll}"
        );

        assert_eq!(follow_request(handle, &elf, 0)["ok"], json!(true));
        assert_eq!(
            request(handle, &json!({ "op": "consoleStop" }))["ok"],
            json!(true)
        );
        let poll = request(handle, &json!({ "op": "consolePoll" }));
        assert_eq!(
            poll["ok"],
            json!(false),
            "the stop ended the follow: {poll}"
        );
        lamella_probe_dispose(handle);
    }

    /// A session answered by `part`, and the part, for a test to read after the request.
    fn session_with(part: Nrf51) -> (u32, std::rc::Rc<std::cell::RefCell<Nrf51>>) {
        let handle = fresh_session();
        let part = std::rc::Rc::new(std::cell::RefCell::new(part));
        let model = std::rc::Rc::clone(&part);
        answer_with(move |command| model.borrow_mut().answer(command));
        (handle, part)
    }

    /// **A STOP AT THE ENTRY REFUSES WHAT IT CANNOT CARRY OUT, BEFORE ANY PACKET.** A stop resets
    /// the board, so a request it cannot finish is refused before the first packet: with no family
    /// there is no reset to choose, and with no entry nothing to stop at. An unknown family is
    /// refused by name with the families there are, as a write's is.
    #[test]
    fn a_stop_at_the_entry_refuses_what_it_cannot_carry_out_before_any_packet() {
        let families: &[&str] = &["nrf51", "nrf52", "rp2040", "rp2350"];
        for (body, named) in [
            (
                json!({ "op": "stopAtEntry", "entry": 0x101u32 }),
                &["family"][..],
            ),
            (
                json!({ "op": "stopAtEntry", "family": "rp2350" }),
                &["entry"][..],
            ),
            (
                json!({ "op": "stopAtEntry", "family": "esp32", "entry": 0x101u32 }),
                &["esp32"][..],
            ),
        ] {
            let handle = fresh_session();
            let reply = request(handle, &body);
            assert_eq!(reply["ok"], json!(false), "{reply}");
            let error = reply["error"].as_str().unwrap_or_default();
            for name in named {
                assert!(error.contains(name), "the refusal names `{name}`: {reply}");
            }
            if body["family"] == json!("esp32") {
                for family in families {
                    assert!(
                        error.contains(family),
                        "the refusal lists `{family}`: {reply}"
                    );
                }
            }
            assert!(
                sent_packets().is_empty(),
                "no packet reached the probe for {body}"
            );
            lamella_probe_dispose(handle);
        }
    }

    /// **A STOP RUNS A RESET PART TO ITS ENTRY ON ONE COMPARATOR, AND LEAVES IT THERE FOR A
    /// DEBUGGER**: halted at the entry with halting debug on, and nothing armed -- no comparator,
    /// the breakpoint unit off and no vector catch. This part's reset vector is the image's boot
    /// code, not its entry, so the core is caught at its reset and run from there to the entry.
    #[test]
    fn a_stop_runs_a_reset_part_to_its_entry_and_leaves_it_halted_there_with_nothing_armed() {
        let (handle, part) = session_with(Nrf51::running_an_image(0xc1, 0x101, 5));
        let reply = request(
            handle,
            &json!({ "op": "stopAtEntry", "family": "nrf51", "entry": 0x101u32 }),
        );
        assert_eq!(reply["ok"], json!(true), "{reply}");
        assert_eq!(
            reply["pc"],
            json!(0x100),
            "the core is at the entry: {reply}"
        );
        assert_eq!(
            reply["polls"],
            json!(5),
            "as many looks as the start took: {reply}"
        );
        assert_eq!(
            reply["idcode"],
            json!(lamella_cmsis_dap_nrf::NRF51_IDCODE),
            "{reply}"
        );
        let part = part.borrow();
        assert_eq!(part.resets, 1, "the part was reset once");
        assert!(part.halted && part.pc == 0x100, "halted at the entry");
        assert!(
            part.debugen,
            "halting debug is on, for the debugger that follows"
        );
        assert!(!part.breakpoint_unit, "the breakpoint unit is off");
        assert_eq!(part.comparators, [0; 4], "no comparator is armed");
        assert_eq!(part.demcr & 1, 0, "the reset vector catch is clear");
        lamella_probe_dispose(handle);
    }

    /// **A PART CAUGHT AT ITS ENTRY STAYS THERE**: where the reset vector is the image's entry,
    /// the core is not run at all, and no comparator is armed.
    #[test]
    fn a_part_whose_reset_vector_is_its_entry_is_not_run() {
        let (handle, part) = session_with(Nrf51::running_an_image(0x101, 0x201, 5));
        let reply = request(
            handle,
            &json!({ "op": "stopAtEntry", "family": "nrf51", "entry": 0x101u32 }),
        );
        assert_eq!(reply["ok"], json!(true), "{reply}");
        assert_eq!(reply["pc"], json!(0x100), "{reply}");
        assert_eq!(reply["polls"], json!(0), "the core was not run: {reply}");
        let part = part.borrow();
        assert!(part.halted && part.pc == 0x100, "halted at the entry");
        assert!(
            !part.breakpoint_unit && part.comparators == [0; 4],
            "nothing was armed"
        );
        lamella_probe_dispose(handle);
    }

    /// **AN ENTRY THE PART's BREAKPOINT UNIT CANNOT MATCH IS REFUSED BEFORE THE RESET.** An Armv6-M
    /// or Armv7-M unit compares instruction addresses below `0x2000_0000` only, so no comparator
    /// can be armed at an entry above that, and a stop would reset the board for nothing.
    #[test]
    fn an_entry_the_breakpoint_unit_cannot_match_is_refused_before_the_reset() {
        let (handle, part) = session_with(Nrf51::running_an_image(0xc1, 0x101, 5));
        let reply = request(
            handle,
            &json!({ "op": "stopAtEntry", "family": "nrf51", "entry": 0x2000_0101u32 }),
        );
        assert_eq!(reply["ok"], json!(false), "{reply}");
        let error = reply["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("0x20000100") && error.contains("0x20000000"),
            "{reply}"
        );
        let part = part.borrow();
        assert_eq!(part.resets, 0, "the part was not reset");
        assert!(!part.halted, "the program still runs");
        lamella_probe_dispose(handle);
    }

    /// **A START THAT NEVER REACHES THE ENTRY ENDS THE STOP, AND SAYS WHERE THE CORE IS**: halted
    /// there, with nothing armed, once the stop has looked as long as any boot takes. This image's
    /// startup calls somewhere other than the entry, which is how a wrong ELF meets a board.
    #[test]
    fn a_start_that_never_reaches_the_entry_ends_the_stop_saying_where_the_core_is() {
        let (handle, part) = session_with(Nrf51::running_an_image(0xc1, 0x201, 5));
        let reply = request(
            handle,
            &json!({ "op": "stopAtEntry", "family": "nrf51", "entry": 0x101u32 }),
        );
        assert_eq!(reply["ok"], json!(false), "{reply}");
        let error = reply["error"].as_str().unwrap_or_default();
        assert!(
            error.contains("0x00000100") && error.contains("0x00000200"),
            "the refusal names the entry and where the core is: {reply}"
        );
        let part = part.borrow();
        assert!(part.halted && part.pc == 0x200, "halted where it was");
        assert!(part.debugen, "halting debug is on");
        assert!(
            !part.breakpoint_unit && part.comparators == [0; 4],
            "nothing is armed"
        );
        lamella_probe_dispose(handle);
    }

    /// **A STOP AT THE ENTRY ENDS THE FOLLOW OF THE CONSOLE**, as a write does: the reset ends the
    /// run being followed, and the stop then holds the next one for a debugger.
    #[test]
    fn a_stop_at_the_entry_ends_the_follow() {
        let (handle, part) = session_with(Nrf51::running_an_image(0xc1, 0x101, 5));
        let at = 0x2000_0100;
        BoardRing::arm(&mut part.borrow_mut(), at, 64);
        let elf = elf_naming(&[(LAMELLA_RING, at, 16 + 64)]);
        assert_eq!(follow_request(handle, &elf, 0)["ok"], json!(true));
        let stop = request(
            handle,
            &json!({ "op": "stopAtEntry", "family": "nrf51", "entry": 0x101u32 }),
        );
        assert_eq!(stop["ok"], json!(true), "{stop}");
        let poll = request(handle, &json!({ "op": "consolePoll" }));
        assert_eq!(
            poll["ok"],
            json!(false),
            "the stop ended the follow: {poll}"
        );
        lamella_probe_dispose(handle);
    }

    /// **A HAND-BACK LETS A PART A STOP LEFT AT ITS ENTRY RUN, AS A DEBUG SESSION's END WOULD**: the
    /// core running, halting debug off and nothing armed, for a host that stopped a part and then
    /// does not debug it.
    #[test]
    fn a_hand_back_lets_a_part_a_stop_left_at_its_entry_run() {
        let (handle, part) = session_with(Nrf51::running_an_image(0xc1, 0x101, 5));
        let stop = request(
            handle,
            &json!({ "op": "stopAtEntry", "family": "nrf51", "entry": 0x101u32 }),
        );
        assert_eq!(stop["ok"], json!(true), "{stop}");
        let reply = request(handle, &json!({ "op": "handBack" }));
        assert_eq!(reply["ok"], json!(true), "{reply}");
        let part = part.borrow();
        assert!(!part.halted, "the core runs");
        assert!(!part.debugen, "halting debug is off");
        assert!(
            !part.breakpoint_unit && part.comparators == [0; 4],
            "nothing is armed"
        );
        lamella_probe_dispose(handle);
    }
}
