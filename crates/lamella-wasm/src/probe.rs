//! The in-page debug-probe agent (feature `probe`): drive a CMSIS-DAP probe, and the chip behind
//! it, from a WebAssembly host.

#![allow(unsafe_code)]

use core::cell::RefCell;

use lamella_cmsis_dap::{Dap, Transport, TransportError};
use lamella_probe_core::{ArmDap, TargetAccess, TargetAccessExt};
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

    thread_local! {
        /// Packets the module has sent, oldest first.
        pub static SENT: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
        /// Replies a caller has queued for the module to read, oldest first.
        pub static REPLIES: RefCell<VecDeque<Vec<u8>>> = const { RefCell::new(VecDeque::new()) };
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn host_write_packet(data: &[u8]) -> Result<(), TransportError> {
    native_host::SENT.with(|sent| sent.borrow_mut().push(data.to_vec()));
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

/// Every packet sent since [`reset_host`]. Native builds only; the tests use it.
#[cfg(not(target_arch = "wasm32"))]
#[must_use]
pub fn sent_packets() -> Vec<Vec<u8>> {
    native_host::SENT.with(|sent| sent.borrow().clone())
}

/// Empties both queues. Native builds only; the tests use it.
#[cfg(not(target_arch = "wasm32"))]
pub fn reset_host() {
    native_host::SENT.with(|sent| sent.borrow_mut().clear());
    native_host::REPLIES.with(|replies| replies.borrow_mut().clear());
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

thread_local! {
    /// Live probe sessions, indexed by `handle - 1`. wasm is single-threaded.
    static SESSIONS: RefCell<Vec<Option<Session>>> = const { RefCell::new(Vec::new()) };
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

/// The debug-port identification code an RP2350 answers with, so a connect can say which silicon
/// replied without the caller decoding it.
const RP2350_IDCODE: u32 = 0x4c01_3477;

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
    let idcode = match family {
        Some("rp2350") => lamella_cmsis_dap_rp2350::connect(session).map_err(|e| e.to_string())?,
        Some("rp2040") => lamella_cmsis_dap_rp2040::connect(session).map_err(|e| e.to_string())?,
        _ => {
            session.connect().map_err(|e| e.to_string())?;
            let idcode = session.read_idcode().map_err(|e| e.to_string())?;
            session.init_mem().map_err(|e| e.to_string())?;
            idcode
        }
    };
    Ok(json!({ "idcode": idcode }))
}

/// The Cortex-M core-register selectors a stopped-state read reports: r0 through r15, then the
/// program status register. The architecture numbers them in that order, so the index into the
/// returned array is the selector.
const CORE_REG_COUNT: u8 = 17;

/// Runs one request against one session.
fn dispatch(session: &mut Session, request: &Value) -> Result<Value, String> {
    let op = request
        .get("op")
        .and_then(Value::as_str)
        .ok_or_else(|| "`op` is required and must be a string".to_string())?;
    let family = request.get("family").and_then(Value::as_str);

    match op {
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

/// Programs `image` into the part named by `family` and leaves it running.
///
/// The routine that writes each part's flash is that part's own, and lives in the same crate a
/// command-line deploy calls: an nRF51 and an nRF52 are erased and programmed through the Nordic
/// Non-Volatile Memory Controller, and an RP2350 by calling its bootrom's flash entry points on the
/// halted core. A family with no whole-image entry point of its own is refused by name, because a
/// deploy assembled out of primitives here would be a second implementation of a sequence that
/// belongs beside the controller it drives.
fn flash(session: &mut Session, family: &str, image: &[u8]) -> Result<Value, String> {
    match family {
        "nrf51" => lamella_cmsis_dap_nrf::flash_and_run(session, 0x0, image)
            .map(|report| json!({ "idcode": report.idcode, "bytes": report.bytes }))
            .map_err(|e| format!("{e:?}")),
        "nrf52" => lamella_cmsis_dap_nrf::erase_all_and_run(
            session,
            0x0,
            image,
            lamella_cmsis_dap_nrf::NRF52_IDCODE,
        )
        .map(|report| json!({ "idcode": report.idcode, "bytes": report.bytes }))
        .map_err(|e| format!("{e:?}")),
        "rp2040" => {
            let mut progress: Vec<String> = Vec::new();
            lamella_cmsis_dap_rp2040::flash_image(session, image, |line| {
                progress.push(line.to_string());
            })
            .map_err(|e| e.to_string())?;
            Ok(json!({
                "idcode": lamella_cmsis_dap_rp2040::RP2040_DPIDR,
                "bytes": image.len(),
                "progress": progress,
            }))
        }
        "rp2350" => {
            let mut progress: Vec<String> = Vec::new();
            lamella_cmsis_dap_rp2350::flash_image(session, image, |line| {
                progress.push(line.to_string());
            })
            .map_err(|e| e.to_string())?;
            Ok(json!({ "idcode": RP2350_IDCODE, "bytes": image.len(), "progress": progress }))
        }
        other => Err(format!(
            "no whole-image flash routine for `{other}`; the families with one are \
             `nrf51`, `nrf52`, `rp2040` and `rp2350`"
        )),
    }
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
fn with_session(handle: u32, f: impl FnOnce(&mut Session) -> Result<Value, String>) -> Vec<u8> {
    let outcome = SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        match sessions.get_mut((handle as usize).wrapping_sub(1)) {
            Some(Some(session)) => f(session),
            _ => Err(format!("no probe session with handle {handle}")),
        }
    });
    envelope(outcome)
}


/// Opens a probe session over the host's packet transport and returns a 1-based handle.
///
/// The host has already chosen and opened the physical probe; this call takes no device identity,
/// because the transport imports are the whole of what this module knows about it.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_probe_create() -> u32 {
    SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        sessions.push(Some(ArmDap::new(Dap::new(HostTransport))));
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
/// # Safety
/// `ptr`/`len` must be the UTF-8 buffer the host filled via a prior `lamella_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_probe_request(handle: u32, ptr: *const u8, len: usize) -> *mut u8 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    let parsed = serde_json::from_slice::<Value>(bytes);
    let body = match parsed {
        Ok(request) => with_session(handle, |session| dispatch(session, &request)),
        Err(e) => envelope(Err(format!("the request is not JSON: {e}"))),
    };
    result_buffer(body)
}

/// Programs the image at `image_ptr..image_ptr + image_len` into the part whose family name is the
/// UTF-8 at `family_ptr..family_ptr + family_len`, and leaves it running. Returns the same
/// `[u32 length][UTF-8 JSON]` envelope as [`lamella_probe_request`].
///
/// The image stays binary rather than riding the JSON request, because a flash image is the one
/// payload here big enough for a text encoding of it to cost more than the transfer.
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
        Ok(family) => with_session(handle, |session| flash(session, family, image)),
        Err(_) => envelope(Err("the family name is not UTF-8".to_string())),
    };
    result_buffer(body)
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
        let raw = unsafe { lamella_probe_request(handle, bytes.as_ptr(), bytes.len()) };
        let (length, json) = unsafe {
            let length =
                u32::from_le_bytes(core::slice::from_raw_parts(raw, 4).try_into().unwrap()) as usize;
            let json = core::slice::from_raw_parts(raw.add(4), length).to_vec();
            crate::abi::lamella_dealloc(raw, 4 + length);
            (length, json)
        };
        assert_eq!(length, json.len());
        serde_json::from_slice(&json).unwrap()
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
        let handle = fresh_session();
        queue_reply(vec![proto::cmd::TRANSFER; 96]);
        let reply = request(handle, &json!({ "op": "readWords", "address": 0u32, "count": 1 }));
        assert_eq!(reply["ok"], json!(false), "{reply}");
        let error = reply["error"].as_str().unwrap_or_default();
        assert!(error.contains("96"), "the refusal names the reported length: {reply}");
        assert!(error.contains("64"), "the refusal names the buffer: {reply}");
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
        let raw = unsafe { lamella_probe_request(handle, bytes.as_ptr(), bytes.len()) };
        let json = unsafe {
            let length =
                u32::from_le_bytes(core::slice::from_raw_parts(raw, 4).try_into().unwrap()) as usize;
            let json = core::slice::from_raw_parts(raw.add(4), length).to_vec();
            crate::abi::lamella_dealloc(raw, 4 + length);
            json
        };
        let reply: Value = serde_json::from_slice(&json).unwrap();
        assert_eq!(reply["ok"], json!(false));
        assert!(reply["error"].as_str().unwrap_or_default().contains("not JSON"), "{reply}");
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
        let family = b"esp32";
        let image = [0u8; 4];
        let raw = unsafe {
            lamella_probe_flash(handle, family.as_ptr(), family.len(), image.as_ptr(), image.len())
        };
        let json = unsafe {
            let length =
                u32::from_le_bytes(core::slice::from_raw_parts(raw, 4).try_into().unwrap()) as usize;
            let json = core::slice::from_raw_parts(raw.add(4), length).to_vec();
            crate::abi::lamella_dealloc(raw, 4 + length);
            json
        };
        let reply: Value = serde_json::from_slice(&json).unwrap();
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
}
