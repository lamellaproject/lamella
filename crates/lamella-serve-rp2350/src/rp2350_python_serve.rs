//! The Python firmware's serve tier (`python-serve`, hygiene-allow: a cargo feature, not a crate):
//! the Lamella Link over the board's own USB port,
//! as the C# interpreter firmware has it, so the lamella CLI stores a program in the board's flash,
//! starts it, and follows its output.
//!
//! - A HELLO is answered with the Python tier: a bundle stored in the deploy window (`DEPLOY_BUNDLE`,
//!   `DEPLOY_STATUS`, `DEPLOY_CLEAR`), started with or without a reset (`EXEC`), and stopped
//!   (`ABORT`).
//! - A run's output goes to the host as `EVT_OUTPUT` as it is printed, and its end as `EVT_STOPPED`,
//!   with the exit code CPython 3.14's `python` gives the same program: 0, the code `sys.exit` was
//!   given (Python 3.14 Library Reference, `sys.exit` and "Built-in Exceptions", `SystemExit`), or 1
//!   for an exception nothing caught, whose type and message go to standard error first.
//! - At each boot the board runs the bundle it stores, else the one built into it, and then serves.
//!   A run that took the firmware down is not run again at the next boot: that boot serves.
//! - A stored bundle of another format is not run, at boot or when a host starts it: the board
//!   says why on its console and to the host, as a run that stopped with exit 1.
//! - While a program runs, the board reads its USB in the program's own calls: each print, each
//!   sleep and, on the Pico 2 W, each wait on the network. `EXEC_STATUS` is answered that the
//!   program runs. A HELLO takes the board back and an `ABORT` stops the program: either resets the
//!   board into its serve loop, so the board leaves the bus for a moment and the host opens it again.
//!   A program that prints nothing, never sleeps and waits on no socket is not answered until it
//!   does one of them.

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, Ordering};

use lamella_py_runtime::{Bundle, ObjectModel, Source, Trap, run_bundle};
use lamella_wire::msg::{self, deploy_state, exec_ack, exec_flags, exec_source, output, stop_reason, xfer};
use lamella_wire::{Capabilities, Frame, TargetIdentity, Transport, crc32};

use crate::python::{self as py, uart};
use crate::python_window::{HEADER_LEN, Stored};
use crate::rp2350_flash::{IMAGE_LEN, PAGE_BYTES, Rp2350Flash};
use crate::rp2350_flash::settings::SECTOR_BYTES;
use crate::rp2350_usb::UsbBus;
use crate::usb_transport::UsbCarrier;

// ---- the link ------------------------------------------------------------------------------------

/// The USB carrier, made once at boot and kept for the board's life: its device holds the
/// enumeration a host has made, and its frame reader the frame that is arriving.
pub(crate) type Link = UsbCarrier<'static, 'static, UsbBus>;

/// The carrier and at most one unsent terminal event, whose payload includes an exit code.
struct LinkState {
    link: Link,
    pending_end: Option<(u16, [u8; 14])>,
}

impl LinkState {
    /// Offers the terminal event once. Backpressure and carrier failures leave it queued.
    fn send_pending_end(&mut self) {
        if let Some((seq, payload)) = &self.pending_end {
            if self.link.try_send(msg::EVT_STOPPED, *seq, payload) == Ok(true) {
                self.pending_end = None;
            }
        }
    }

    /// Sends any queued terminal event before a reply, retaining it if the carrier fails.
    fn send_reply(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), lamella_wire::TransportError> {
        if let Some((end_seq, end_payload)) = &self.pending_end {
            self.link.send(msg::EVT_STOPPED, *end_seq, end_payload)?;
            self.pending_end = None;
        }
        self.link.send(msg_type, seq, payload)
    }
}

/// Where the board's one [`Link`] is kept, so the runtime's hooks, which are bare functions, can reach
/// it.
struct LinkCell(UnsafeCell<Option<LinkState>>);

// SAFETY: the RP2350 runs this firmware on one core with no interrupt handler that reaches the link,
// and `with_link_state` refuses a second borrow, so the cell is never reached twice at once.
unsafe impl Sync for LinkCell {}

static LINK: LinkCell = LinkCell(UnsafeCell::new(None));

/// Set while [`with_link_state`] holds the link and its pending event.
static LINK_HELD: AtomicBool = AtomicBool::new(false);

/// Runs `work` on the link, and answers what it answered; `None` when the board has no link (its USB
/// clock did not come up) or the link is already in use further up the stack.
fn with_link<R>(work: impl FnOnce(&mut Link) -> R) -> Option<R> {
    with_link_state(|state| work(&mut state.link))
}

/// Borrows the carrier and its pending event together under the link's reentrancy guard.
fn with_link_state<R>(work: impl FnOnce(&mut LinkState) -> R) -> Option<R> {
    if LINK_HELD.swap(true, Ordering::Acquire) {
        return None;
    }
    // SAFETY: the flag above makes this the only reference to the cell's contents.
    let answer = unsafe { (*LINK.0.get()).as_mut() }.map(work);
    LINK_HELD.store(false, Ordering::Release);
    answer
}

/// Keeps the link the firmware made, for the rest of the board's life.
fn keep_link(link: Link) {
    // SAFETY: called once, at boot, before anything else reaches the cell.
    unsafe { *LINK.0.get() = Some(LinkState { link, pending_end: None }) };
}

/// The USB device and its vendor interface over the RP2350's controller, built with `build`, which
/// names the board: connected to the bus, which starts the enumeration the link's polls carry on.
pub(crate) fn link_with(
    build: impl FnOnce(&'static usb_device::bus::UsbBusAllocator<UsbBus>) -> (
        crate::usb_transport::WinUsbClass<'static, UsbBus>,
        usb_device::device::UsbDevice<'static, UsbBus>,
    ),
) -> Link {
    let bus: &'static _ = Box::leak(Box::new(UsbBus::new()));
    let (class, device) = build(bus);
    UsbCarrier::new(Box::leak(Box::new(device)), Box::leak(Box::new(class)))
}

/// The next whole frame, servicing the device and retrying its pending terminal event.
/// A possible replacement request leaves the terminal-event decision to its validating handler.
fn next_frame() -> Option<Frame> {
    with_link_state(|state| {
        let frame = state.link.poll().ok().flatten();
        if !frame.as_ref().is_some_and(|frame| matches!(frame.msg_type, msg::HELLO | msg::EXEC)) {
            state.send_pending_end();
        }
        frame
    }).flatten()
}

/// Sends a frame a host is waiting for.
fn reply(msg_type: u8, seq: u16, payload: &[u8]) {
    let _ = with_link_state(|state| state.send_reply(msg_type, seq, payload));
}

/// Services the device for `ms` milliseconds, so what was just sent leaves before a reset takes the
/// board off the bus. Frames that arrive meanwhile are dropped: the reset would drop them anyway.
fn drain(ms: u64) {
    service_for(ms, || false);
}

/// The most steps [`for_at_most`] takes while the clock reads the same, so a clock that is not
/// running cannot hold the board in a wait.
const MOST_POLLS: u32 = 1_000_000;

/// Services the device until `done` answers true or `ms` milliseconds pass, dropping frames.
fn service_for(ms: u64, done: impl Fn() -> bool) {
    for_at_most(ms, || {
        if done() {
            return true;
        }
        let _ = next_frame();
        false
    });
}

/// Takes `step` until it answers true or `ms` milliseconds pass. A running clock bounds the wait by
/// time alone: steps are counted only while the clock does not move, and [`MOST_POLLS`] of them end
/// it, so the boot's own tolerance of a clock that does not run cannot hold the board here.
fn for_at_most(ms: u64, mut step: impl FnMut() -> bool) {
    let start = crate::systick_clock::now_ms();
    let (mut last, mut still) = (start, 0u32);
    loop {
        if step() {
            return;
        }
        let now = crate::systick_clock::now_ms();
        if now.saturating_sub(start) >= ms {
            return;
        }
        if now == last {
            still += 1;
            if still >= MOST_POLLS {
                return;
            }
        } else {
            (last, still) = (now, 0);
        }
    }
}

// ---- the boot flag -------------------------------------------------------------------------------

/// Kept across a reset (`.noinit`, which startup does not zero): why the board reset, so the next
/// boot knows whether to run the stored program.
#[unsafe(link_section = ".noinit")]
static mut BOOT_FLAG: u32 = 0;

/// The last run took the firmware down: out of memory, or a fault.
const FAULTED: u32 = 0x5079_4654;
/// A host took the board back from a run.
const TAKEN_BACK: u32 = 0x5079_5442;

/// Resets the processors with `flag` for the next boot to read: SYSRESETREQ keeps `.noinit`, and so
/// [`BOOT_FLAG`]. The board first leaves the USB bus for long enough that the host sees it go
/// (`rp2350_usb::detach_and_reset`): the reset restarts only the processors, and the USB controller
/// would keep its pull-up through it.
fn reset_with(flag: u32) -> ! {
    // SAFETY: a plain store to a static nothing else is using; the reset follows.
    unsafe { core::ptr::write_volatile(&raw mut BOOT_FLAG, flag) };
    crate::rp2350_usb::detach_and_reset()
}

/// Resets into the serve loop after a fault, so a program that takes the firmware down is not run
/// again at the next boot: that boot serves, and the one after it runs the program again.
pub(crate) fn reset_after_fault() -> ! {
    reset_with(FAULTED)
}

/// Reads and clears the flag the last reset left.
fn take_boot_flag() -> u32 {
    // SAFETY: read and cleared once, at boot, before anything else runs.
    unsafe {
        let flag = core::ptr::read_volatile(&raw const BOOT_FLAG);
        core::ptr::write_volatile(&raw mut BOOT_FLAG, 0);
        flag
    }
}

// ---- the stored bundle ---------------------------------------------------------------------------

/// Where the bundle's bytes begin: the window's second page, so the header's page is written on its
/// own. The header, and how the window is read, are [`crate::python_window`]'s.
const BUNDLE_AT: usize = PAGE_BYTES;

/// The largest bundle the window holds.
const BUNDLE_ROOM: usize = IMAGE_LEN - BUNDLE_AT;

/// What the deploy window holds, its bytes read back.
fn stored(window: &'static [u8]) -> Stored<'static> {
    crate::python_window::stored(window, BUNDLE_AT, |bytes| crc32::update(0, bytes), lamella_py_runtime::is_bundle_header)
}

/// Every sector of the window that holds anything, erased; answers how many bytes that was, or
/// `None` when an erase failed.
fn clear_window(flash: &mut Rp2350Flash) -> Option<usize> {
    let window = flash.window();
    let sectors = IMAGE_LEN / SECTOR_BYTES;
    let holds = |index: usize| window[index * SECTOR_BYTES..(index + 1) * SECTOR_BYTES].iter().any(|&b| b != 0xFF);
    let (mut erased, mut index) = (0, 0);
    while index < sectors {
        if !holds(index) {
            index += 1;
            continue;
        }
        let first = index;
        while index < sectors && holds(index) {
            index += 1;
        }
        let len = (index - first) * SECTOR_BYTES;
        if !flash.erase_window_range(first * SECTOR_BYTES, len) {
            return None;
        }
        erased += len;
    }
    Some(erased)
}

/// How far a deploy has written, and the CRC-32 of what it wrote, as the flash reads it back.
#[derive(Default)]
struct Deploy {
    frontier: usize,
    crc: u32,
}

/// One `DEPLOY_BUNDLE` chunk, `[offset u32][total u32][bytes]`, written to the window: answers the
/// transfer status and the CRC-32 of the bundle's bytes up to the end of this chunk, as the flash
/// reads them back.
///
/// The chunk at offset 0 must open a bundle this firmware runs, or nothing is erased: that is the
/// last moment a wrong payload can be refused without the program the board holds being lost. It
/// then clears the window. The chunks follow in order, each where the last ended, and after the last
/// one the header goes down, so a board reset part way stores nothing.
fn deploy_chunk(flash: &mut Rp2350Flash, deploy: &mut Deploy, payload: &[u8]) -> (u8, u32) {
    let Some(header) = payload.get(..8) else {
        return (xfer::RANGE_REJECTED, 0);
    };
    let offset = u32::from_le_bytes([header[0], header[1], header[2], header[3]]) as usize;
    let total = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    let chunk = &payload[8..];
    if total > BUNDLE_ROOM || offset.saturating_add(chunk.len()) > total {
        return (xfer::RANGE_REJECTED, 0);
    }
    if offset == 0 {
        if !lamella_py_runtime::is_bundle_header(chunk) {
            return (xfer::WRITE_FAILED, 0);
        }
        if clear_window(flash).is_none() {
            return (xfer::WRITE_FAILED, 0);
        }
        *deploy = Deploy::default();
    }
    if offset != deploy.frontier || !flash.program_window(BUNDLE_AT + offset, chunk) {
        *deploy = Deploy::default();
        return (xfer::WRITE_FAILED, 0);
    }
    let end = offset + chunk.len();
    deploy.crc = crc32::update(deploy.crc, &flash.window()[BUNDLE_AT + offset..BUNDLE_AT + end]);
    deploy.frontier = end;
    if end == total {
        let mut header = [0u8; HEADER_LEN];
        header[..4].copy_from_slice(&(total as u32).to_le_bytes());
        header[4..].copy_from_slice(&deploy.crc.to_le_bytes());
        if !flash.program_window(0, &header) {
            return (xfer::WRITE_FAILED, 0);
        }
    }
    (xfer::MATCHED, deploy.crc)
}

// ---- a run ---------------------------------------------------------------------------------------

/// Who a run's output goes to.
const TO_NOBODY: u8 = 0;
/// The host that started the run, which is reading: each frame waits until the host takes it.
const TO_HOST: u8 = 1;
/// Whoever reads the link: a run the board started at boot, which no host asked for, sends a frame
/// only when the host takes it at once, and drops it otherwise. Its end remains queued until the
/// carrier accepts it or an accepted HELLO or EXEC replaces its session.
const TO_ANYONE: u8 = 2;

static OUTPUT_TO: AtomicU8 = AtomicU8::new(TO_NOBODY);

/// Set when output was dropped, so the next chunk that goes says so.
static OUTPUT_DROPPED: AtomicBool = AtomicBool::new(false);

/// The sequence number a run's end goes out at: the `EXEC` that started it, or 0 for a boot run.
static RUN_SEQ: AtomicU16 = AtomicU16::new(0);

/// The most text one `EVT_OUTPUT` frame carries.
const OUTPUT_CHUNK: usize = 1024;

/// Sends `text` on `stream` to whoever [`OUTPUT_TO`] names.
fn send_output(stream: u8, text: &str) {
    for (chunk, flags) in lamella_runner::debug::output_chunks(text, OUTPUT_CHUNK) {
        let dropped = if OUTPUT_DROPPED.load(Ordering::Relaxed) { output::OUTPUT_DROPPED } else { 0 };
        let mut payload = Vec::with_capacity(2 + chunk.len());
        payload.extend_from_slice(&[stream, flags | dropped]);
        payload.extend_from_slice(chunk.as_bytes());
        let sent = match OUTPUT_TO.load(Ordering::Relaxed) {
            TO_HOST => {
                let sent = with_link(|link| link.send(msg::EVT_OUTPUT, 0, &payload).is_ok()) == Some(true);
                if !sent {
                    OUTPUT_TO.store(TO_NOBODY, Ordering::Relaxed);
                }
                sent
            }
            TO_ANYONE => with_link(|link| link.try_send(msg::EVT_OUTPUT, 0, &payload) == Ok(true)) == Some(true),
            _ => false,
        };
        OUTPUT_DROPPED.store(!sent, Ordering::Relaxed);
    }
}

/// The program's standard output.
fn console(text: &str) {
    send_output(output::STDOUT, text);
    serve_during_run();
}

/// The program's standard error.
fn error_console(text: &str) {
    send_output(output::STDERR, text);
    serve_during_run();
}

/// When the link was last read during a run: the low 32 bits of the clock's milliseconds, which is
/// all a once-a-millisecond limit compares.
static LAST_SERVED_MS: AtomicU32 = AtomicU32::new(0);

/// Reads the link during a run: `EXEC_STATUS` is answered that the program runs, a HELLO takes the
/// board back and an `ABORT` stops the program, each by a reset into the serve loop. Anything else
/// is dropped: a host asks one thing of a running board at a time, and these are what it asks.
fn serve_during_run() {
    LAST_SERVED_MS.store(crate::systick_clock::now_ms() as u32, Ordering::Relaxed);
    let Some(frame) = next_frame() else {
        return;
    };
    match frame.msg_type {
        msg::EXEC_STATUS => reply(msg::EXEC_ACK, frame.seq, &[exec_ack::RUNNING]),
        msg::HELLO => reset_with(TAKEN_BACK),
        msg::ABORT => {
            reply(msg::EVT_STOPPED, frame.seq, &stop(stop_reason::ABORTED, None));
            drain(50);
            reset_with(TAKEN_BACK)
        }
        _ => {}
    }
}

/// The sleep a run's `time.sleep` takes, reading the link at least once in each of the clock's
/// sleeps, each 16 ms or shorter. Rounds a sub-millisecond request up to one millisecond, as the
/// image without the serve does.
fn sleep_ns(nanos: i64) {
    use crate::systick_clock::{now_ms, sleep_ms};
    if nanos <= 0 {
        return;
    }
    let until = now_ms().saturating_add(((nanos + 999_999) / 1_000_000) as u64);
    loop {
        serve_during_run();
        let now = now_ms();
        if now >= until {
            return;
        }
        sleep_ms(until - now);
        if now_ms() == now {
            return;
        }
    }
}

/// The clock the Pico 2 W's network stack reads, reading the link at most once a millisecond: the
/// stack spends a program's sleeps and its waits on a socket polling the radio, and reads its clock
/// in each pass.
#[cfg(feature = "python-net")]
pub(crate) fn net_clock_ms() -> u64 {
    let now = crate::systick_clock::now_ms();
    if now as u32 != LAST_SERVED_MS.load(Ordering::Relaxed) {
        serve_during_run();
    }
    now
}

/// A stop event's payload: the reason, no stop site, and the exit code when the run has one.
fn stop(reason: u8, exit: Option<i32>) -> Vec<u8> {
    let mut payload = Vec::with_capacity(14);
    payload.push(reason);
    payload.extend_from_slice(&[0; 8]);
    if let Some(exit) = exit {
        payload.extend_from_slice(&exit.to_le_bytes());
        payload.push(0);
    }
    payload
}

/// Queues a run's end without waiting for a reader. The serve loop retries it on each poll.
fn send_end(reason: u8, exit: i32) {
    let payload = stop(reason, Some(exit)).try_into().expect("a run end includes an exit code");
    let seq = RUN_SEQ.load(Ordering::Relaxed);
    let _ = with_link_state(|state| {
        if state.pending_end.is_none() {
            state.pending_end = Some((seq, payload));
        }
        state.send_pending_end();
    });
}

/// The refusal a stored bundle of another format gets, at boot and when a host starts it: the
/// board's answer every firmware gives, from the one place the answers are spelled.
const OTHER_FORMAT: &str = lamella_wire::image_format::REDEPLOY_PROGRAM;

/// The one image format this firmware's runtime loads, which its HELLO states: a bundle's.
const FORMATS: [u16; 1] = [lamella_py_runtime::BUNDLE_FORMAT_VERSION];

/// Runs `bundle`, sending its output and its end as [`OUTPUT_TO`] says, at `seq`.
fn run(bundle: &'static [u8], to: u8, seq: u16, net: &NetSetup) {
    OUTPUT_TO.store(to, Ordering::Relaxed);
    OUTPUT_DROPPED.store(false, Ordering::Relaxed);
    RUN_SEQ.store(seq, Ordering::Relaxed);
    let decoded = Bundle::decode_deferred(&Source::Static(bundle));
    let (reason, exit) = match decoded {
        Ok((bundle, _features)) => {
            let mut model = ObjectModel::new(Vec::new(), py::PY_HEAP_BYTES);
            py::configure_native_stack(&mut model);
            model.set_console(console);
            model.set_error_console(error_console);
            if net.ticks_per_ms.is_some() {
                model.set_monotonic(py::board_clock::now_ns, sleep_ns);
            }
            #[cfg(feature = "python-gc-engine")]
            model.set_arena_probe(py::arena_probe);
            #[cfg(feature = "python-gc-engine")]
            model.set_collect_when_full(true);
            #[cfg(feature = "python-net")]
            if let Some(ticks) = net.ticks_per_ms {
                py::install_net(&mut model, ticks, net_clock_ms);
            }
            let ended = match run_bundle(bundle, &mut model) {
                Ok(_) => (stop_reason::DONE, 0),
                Err(trap) => ending(&mut model, trap),
            };
            py::report_arena(&model);
            ended
        }
        Err(error) => {
            let said = if lamella_py_runtime::is_bundle_header(bundle) {
                alloc::format!("{error}")
            } else {
                String::from(OTHER_FORMAT)
            };
            uart::str("[py] ");
            uart::str(&said);
            uart::str("\r\n");
            send_output(output::STDERR, &alloc::format!("{said}\n"));
            (stop_reason::TRAP, 1)
        }
    };
    send_end(reason, exit);
    OUTPUT_TO.store(TO_NOBODY, Ordering::Relaxed);
}

/// How a run a trap stopped ended: its stop reason and exit code, as CPython 3.14's `python` ends the
/// same program. `SystemExit` ends it with its own code, as an exit ([`lamella_py_runtime::system_exit`]);
/// any other exception nothing caught ends it with 1, as a trap, its type and message said on standard
/// error first.
fn ending(model: &mut ObjectModel, trap: Trap) -> (u8, i32) {
    let end = lamella_py_runtime::trapped_end(model, py::trap_name(trap));
    if !end.said.is_empty() {
        send_output(output::STDERR, &end.said);
    }
    (if end.exited { stop_reason::DONE } else { stop_reason::TRAP }, end.code)
}

// ---- the serve loop ------------------------------------------------------------------------------

/// What a run needs from the boot: the core clock in kilohertz when the clock tree said what it is,
/// which the network's radio needs too.
pub(crate) struct NetSetup {
    pub(crate) ticks_per_ms: Option<u32>,
}

/// What this tier does, and nothing it does not.
fn capabilities(clock: bool) -> Capabilities {
    let mut caps = Capabilities::BUNDLE
        | Capabilities::DEPLOY_PREFIX_CRC
        | Capabilities::EXEC_NO_RESET
        | Capabilities::PROFILE_CHIPID;
    if clock {
        caps |= Capabilities::MONOTONIC_CLOCK;
    }
    Capabilities(caps)
}

/// Answers a HELLO with this board's identity and this tier's capabilities.
fn hello(frame: &Frame, caps: Capabilities, identity: &TargetIdentity) {
    use lamella_wire::{Hello, PROTOCOL_VERSION, ProtocolRange, target_respond};
    let range = ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION };
    let _ = with_link_state(|state| {
        let inbound = state.link.max_inbound_payload();
        match Hello::decode(&frame.payload) {
            Some(hello) => match target_respond(&hello, range, caps, identity.clone(), inbound, Some(&FORMATS)) {
                Ok(ack) => {
                    state.pending_end = None;
                    state.send_reply(msg::HELLO_ACK, frame.seq, &ack.encode())
                }
                Err(nak) => state.send_reply(msg::HELLO_NAK, frame.seq, &nak.encode()),
            },
            None => Ok(()),
        }
    });
}

/// The answer to `DEPLOY_STATUS`: `state(u8) | tier(u8) | checksum(u64) | window(u32)`. The checksum
/// is the stored bundle's CRC-32, verified against its bytes, which is what a host compares with
/// the CRC-32 of the bundle it would send.
fn deploy_status(flash: &Rp2350Flash) -> [u8; 14] {
    let (state, tier, checksum) = match stored(flash.window()) {
        Stored::Nothing => (deploy_state::NONE, 0, 0),
        Stored::Bundle(_, crc) => (deploy_state::VERIFIED, msg::tier::PYTHON, u64::from(crc)),
        Stored::OtherFormat(_) | Stored::Damaged => (deploy_state::UNVERIFIABLE, 0, 0),
        Stored::OtherRuntime => (deploy_state::TIER_MISMATCH, msg::tier::CIL, 0),
    };
    let mut payload = [0u8; 14];
    payload[0] = state;
    payload[1] = tier;
    payload[2..10].copy_from_slice(&checksum.to_le_bytes());
    payload[10..].copy_from_slice(&(BUNDLE_ROOM as u32).to_le_bytes());
    payload
}

/// Serves the link until a request resets the board.
fn serve(flash: &mut Rp2350Flash, net: &NetSetup, caps: Capabilities, identity: &TargetIdentity) -> ! {
    let mut deploy = Deploy::default();
    loop {
        let Some(frame) = next_frame() else {
            continue;
        };
        let seq = frame.seq;
        match frame.msg_type {
            msg::HELLO => hello(&frame, caps, identity),
            msg::ABORT | msg::EXEC_STATUS => reply(msg::EXEC_ACK, seq, &[exec_ack::IDLE]),
            msg::DEPLOY_STATUS => reply(msg::DEPLOY_STATUS_RESULT, seq, &deploy_status(flash)),
            msg::DEPLOY_BUNDLE => {
                let (status, crc) = deploy_chunk(flash, &mut deploy, &frame.payload);
                let mut payload = [status, 0, 0, 0, 0];
                payload[1..].copy_from_slice(&crc.to_le_bytes());
                reply(msg::XFER_RESULT, seq, &payload);
            }
            msg::DEPLOY_CLEAR => {
                deploy = Deploy::default();
                let (status, erased) = match clear_window(flash) {
                    Some(bytes) => (xfer::MATCHED, bytes as u32),
                    None => (xfer::WRITE_FAILED, 0),
                };
                let mut payload = [status, 0, 0, 0, 0];
                payload[1..].copy_from_slice(&erased.to_le_bytes());
                reply(msg::XFER_RESULT, seq, &payload);
            }
            msg::EXEC => {
                let source = frame.payload.first().copied().unwrap_or(0);
                let flags = frame.payload.get(1).copied().unwrap_or(0);
                let bundle = match stored(flash.window()) {
                    Stored::Bundle(bytes, _) | Stored::OtherFormat(bytes) => Some(bytes),
                    _ => None,
                };
                if source != exec_source::DEPLOYED {
                    reply(msg::EXEC_ACK, seq, &[exec_ack::NO_SUCH_SOURCE]);
                } else if flags & exec_flags::START_HALTED != 0 {
                    reply(msg::EXEC_ACK, seq, &[exec_ack::HALTED_UNSUPPORTED]);
                } else if let Some(bundle) = bundle {
                    let _ = with_link_state(|state| state.pending_end = None);
                    reply(msg::EXEC_ACK, seq, &[exec_ack::STARTED]);
                    if flags & exec_flags::NO_RESET != 0 {
                        run(bundle, TO_HOST, seq, net);
                    } else {
                        drain(50);
                        reset_with(0);
                    }
                } else {
                    reply(msg::EXEC_ACK, seq, &[exec_ack::NOTHING_TO_RUN]);
                }
            }
            other => reply(msg::ERROR, seq, &lamella_wire::error::unknown_message_type(other)),
        }
    }
}

/// How long a boot that is about to run a program services the USB first, so a host that is
/// attached finishes enumerating the board: the program reads the link only when it prints, sleeps
/// or waits.
const ENUMERATION_MS: u64 = 2000;

/// What the firmware hands its serve tier at boot.
pub(crate) struct Boot {
    /// The board's link, or `None` when its USB clock did not come up.
    pub(crate) link: Option<Link>,
    /// What this board's HELLO says it is.
    pub(crate) identity: TargetIdentity,
    /// What a run needs from the clock tree.
    pub(crate) net: NetSetup,
    /// Whether the installed clock was seen moving.
    pub(crate) clock_runs: bool,
    /// The bundle built into the image: a program, or a placeholder that opens no bundle.
    pub(crate) built_in: &'static [u8],
}

/// The boot: the link, then the stored program, else the one built in, then the serve loop. A board
/// whose USB has no clock has no link: it runs what a boot with one runs, and then stays as its
/// program left it.
pub(crate) fn boot(boot: Boot) -> ! {
    let Boot { link, identity, net, clock_runs, built_in } = boot;
    let flag = take_boot_flag();
    let mut flash = Rp2350Flash::new();
    let linked = link.is_some();
    match link {
        Some(link) => keep_link(link),
        None => uart::str("[py] no USB clock, so no link\r\n"),
    }
    let program = match flag {
        FAULTED if linked => {
            uart::str("[py] the last run took the firmware down; serving instead of running it\r\n");
            None
        }
        FAULTED => {
            uart::str("[py] the last run took the firmware down; not running it again\r\n");
            None
        }
        TAKEN_BACK if linked => None,
        _ => match stored(flash.window()) {
            Stored::Bundle(bundle, _) | Stored::OtherFormat(bundle) => Some(bundle),
            _ if lamella_py_runtime::is_bundle_header(built_in) => Some(built_in),
            _ => None,
        },
    };
    if let Some(bundle) = program {
        let to = if linked {
            service_for(ENUMERATION_MS, || with_link(|link| link.is_configured()).unwrap_or(false));
            TO_ANYONE
        } else {
            TO_NOBODY
        };
        uart::str("[py] running the program\r\n");
        run(bundle, to, 0, &net);
    }
    if !linked {
        loop {}
    }
    uart::str("[py] serving\r\n");
    serve(&mut flash, &net, capabilities(clock_runs), &identity)
}

#[cfg(test)]
#[path = "../tests/support/python_serve_cases.rs"]
mod tests;
