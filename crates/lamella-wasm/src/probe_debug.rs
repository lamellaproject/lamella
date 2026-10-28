//! The probe's debugger (feature `probe-debug`): a Debug Adapter Protocol session over the program on
//! the part a probe session reaches, driven through that same probe session.

#![allow(unsafe_code)]

use core::cell::RefCell;

use lamella_dap::{Debugger, Message};
use lamella_debug_device::DeviceBackend;
use lamella_debug_device::program::{DebugProgram, from_elf};
use lamella_probe_core::{CallFrame, ProbeError, TargetAccess};
use serde_json::Value;

use crate::abi::result_buffer;
use crate::probe::{Session, borrow_session};

/// One debug session: the adapter, and the probe session it holds.
struct DebugSession {
    debugger: Debugger,
    /// The handle of the probe session the debugger drives the part through. `None` for a session
    /// that cannot start, which holds nothing.
    probe: Option<u32>,
}

thread_local! {
    /// Live debug sessions, indexed by `handle - 1`. wasm is single-threaded.
    static DEBUG_SESSIONS: RefCell<Vec<Option<DebugSession>>> = const { RefCell::new(Vec::new()) };
}


/// The probe requests a probe session still answers while a debug session holds it. Each reads the
/// part, writes its memory, or looks at its console, so none of them moves the core or changes what
/// stops it. Every request not named here is refused while the session is held.
const OPEN_WHILE_HELD: &[&str] = &[
    "info",
    "isHalted",
    "waitHalted",
    "readWords",
    "writeWords",
    "readCoreReg",
    "readCoreRegs",
    "consolePoll",
    "consoleStop",
];

/// The debug session holding the probe session `probe`, if one does.
fn holder(probe: u32) -> Option<u32> {
    DEBUG_SESSIONS.with(|sessions| {
        let sessions = sessions.borrow();
        let index = sessions.iter().position(|slot| {
            slot.as_ref()
                .is_some_and(|session| session.probe == Some(probe))
        })?;
        u32::try_from(index + 1).ok()
    })
}

/// Refuses `what` on the probe session `probe` while a debug session holds it.
fn refuse(probe: u32, what: &str) -> Result<(), String> {
    match holder(probe) {
        Some(debug) => Err(format!(
            "debug session {debug} holds this probe session, so {what} is refused: until \
             lamella_probe_debug_dispose({debug}) ends that session, only its debugger moves the \
             core or changes what stops it"
        )),
        None => Ok(()),
    }
}

/// Refuses `request` on the probe session `probe` while a debug session holds it, unless it is one
/// of [`OPEN_WHILE_HELD`]. A request that names no operation is left for the dispatch to refuse.
pub(crate) fn refuse_request(probe: u32, request: &Value) -> Result<(), String> {
    match request.get("op").and_then(Value::as_str) {
        Some(op) if !OPEN_WHILE_HELD.contains(&op) => refuse(probe, &format!("`{op}`")),
        _ => Ok(()),
    }
}

/// Refuses a write of the part on the probe session `probe` while a debug session holds it.
pub(crate) fn refuse_write(probe: u32) -> Result<(), String> {
    refuse(probe, "a write of the part")
}


/// The part a probe session reaches, as the device backend drives it. Every call runs on the probe
/// session the handle names, looked up for that call.
struct ProbeTarget {
    probe: u32,
}

impl ProbeTarget {
    /// Runs `f` on the probe session, or fails as a lost link fails when the session is gone.
    fn on<R>(
        &mut self,
        f: impl FnOnce(&mut Session) -> Result<R, ProbeError>,
    ) -> Result<R, ProbeError> {
        borrow_session(self.probe, f).unwrap_or_else(|| {
            Err(ProbeError::Transport(format!(
                "the probe session {} is gone",
                self.probe
            )))
        })
    }
}

impl TargetAccess for ProbeTarget {
    /// Does nothing. A debug session starts on a part the page has connected already, the way the
    /// part's family needs, and the generic sequence would undo that on an RP2350: it selects
    /// memory access port 0, where that part's debug port addresses the core's port by its ADIv6
    /// base address.
    fn connect(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }
    fn read_idcode(&mut self) -> Result<u32, ProbeError> {
        self.on(|session| session.read_idcode())
    }
    /// Does nothing, for the reason [`connect`](Self::connect) does: the page's connect selected and
    /// powered the core's memory access port.
    fn init_mem(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }
    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        self.on(|session| session.read_word(address))
    }
    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        self.on(|session| session.write_word(address, value))
    }
    fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
        self.on(|session| session.read_words_into(address, out))
    }
    fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        self.on(|session| session.write_words(address, words))
    }
    fn read_byte(&mut self, address: u32) -> Result<u8, ProbeError> {
        self.on(|session| session.read_byte(address))
    }
    fn write_byte(&mut self, address: u32, value: u8) -> Result<(), ProbeError> {
        self.on(|session| session.write_byte(address, value))
    }
    fn read_halfword(&mut self, address: u32) -> Result<u16, ProbeError> {
        self.on(|session| session.read_halfword(address))
    }
    fn write_halfword(&mut self, address: u32, value: u16) -> Result<(), ProbeError> {
        self.on(|session| session.write_halfword(address, value))
    }
    fn halt(&mut self) -> Result<(), ProbeError> {
        self.on(|session| session.halt())
    }
    fn resume(&mut self) -> Result<(), ProbeError> {
        self.on(|session| session.resume())
    }
    fn step(&mut self) -> Result<(), ProbeError> {
        self.on(|session| session.step())
    }
    fn is_halted(&mut self) -> Result<bool, ProbeError> {
        self.on(|session| session.is_halted())
    }
    fn wait_halted(&mut self) -> Result<(), ProbeError> {
        self.on(|session| session.wait_halted())
    }
    fn reset_and_run(&mut self) -> Result<(), ProbeError> {
        self.on(|session| session.reset_and_run())
    }
    fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
        self.on(|session| session.reset_and_halt())
    }
    fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError> {
        self.on(|session| session.set_reset(assert))
    }
    fn reset_extension(&mut self) -> Result<bool, ProbeError> {
        self.on(|session| session.reset_extension())
    }
    fn read_core_reg(&mut self, selector: u8) -> Result<u32, ProbeError> {
        self.on(|session| session.read_core_reg(selector))
    }
    fn write_core_reg(&mut self, selector: u8, value: u32) -> Result<(), ProbeError> {
        self.on(|session| session.write_core_reg(selector, value))
    }
    fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
        self.on(|session| session.arm_reset_catch())
    }
    fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
        self.on(|session| session.disarm_reset_catch())
    }
    fn set_breakpoint(&mut self, address: u32) -> Result<(), ProbeError> {
        self.on(|session| session.set_breakpoint(address))
    }
    fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
        self.on(|session| session.clear_breakpoint())
    }
    fn set_breakpoints(&mut self, addresses: &[u32]) -> Result<(), ProbeError> {
        self.on(|session| session.set_breakpoints(addresses))
    }
    fn call_target(
        &mut self,
        address: u32,
        args: &[u32],
        frame: &CallFrame,
    ) -> Result<u32, ProbeError> {
        self.on(|session| session.call_target(address, args, frame))
    }
}


/// A session that cannot start and holds nothing: its `launch` fails with `reason`.
fn refusing(reason: String) -> DebugSession {
    DebugSession {
        debugger: Debugger::refusing(reason),
        probe: None,
    }
}

/// The device backend over the part the probe session `probe` reaches, with what `program` says
/// about the code there.
fn backend(probe: u32, program: DebugProgram) -> DeviceBackend<ProbeTarget> {
    let DebugProgram {
        image: _,
        image_base,
        lines,
        names,
        files,
        entry,
        frames,
        locals,
        unwind,
    } = program;
    DeviceBackend::new(
        ProbeTarget { probe },
        lines,
        image_base,
        names,
        files,
        entry,
        frames,
    )
    .with_locals(locals)
    .with_unwind_tables(unwind)
}

/// Starts a debug session over the program `elf` describes on the probe session `probe`; see
/// [`lamella_probe_debug_create`].
fn create(probe: u32, elf: &[u8]) -> u32 {
    if borrow_session(probe, |_| ()).is_none() {
        return 0;
    }
    let session = match holder(probe) {
        Some(holding) => refusing(format!(
            "debug session {holding} is debugging the program on this probe session's part \
             already; end that session before starting another"
        )),
        None => match from_elf(elf) {
            Ok(program) => DebugSession {
                debugger: Debugger::with_backend(Box::new(backend(probe, program))),
                probe: Some(probe),
            },
            Err(why) => refusing(format!("this program cannot be debugged: {why}")),
        },
    };
    DEBUG_SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        sessions.push(Some(session));
        u32::try_from(sessions.len()).unwrap_or(0)
    })
}

/// Runs `f` over the debugger of the session `debug` names, or answers `None` when it names none.
fn with_debugger<R>(debug: u32, f: impl FnOnce(&mut Debugger) -> R) -> Option<R> {
    DEBUG_SESSIONS.with(|sessions| {
        let mut sessions = sessions.borrow_mut();
        match sessions.get_mut((debug as usize).wrapping_sub(1)) {
            Some(Some(session)) => Some(f(&mut session.debugger)),
            _ => None,
        }
    })
}

/// The replies a host reads, as a `[u32 length][JSON array]` buffer.
fn replies(messages: &[Message]) -> *mut u8 {
    result_buffer(serde_json::to_vec(messages).unwrap_or_default())
}


/// Starts a debug session over the program the ELF at `elf_ptr..elf_ptr + elf_len` describes, on
/// the part the probe session `probe` reaches, and returns a 1-based handle; 0 only when `probe`
/// names no probe session.
///
/// The part must be connected already, the way its family needs: `{"op": "stopAtEntry"}` connects
/// it and leaves the core halted at the program's entry, where a `launch` with `stopOnEntry` reports
/// it stopped. The session's own `launch` does not connect. It reads the debug port's id to check
/// the link, halts the core, and reads how many comparators the breakpoint unit has.
///
/// A session that cannot debug the program still gets a handle, and its `launch` fails with the
/// reason, for the client to show: an ELF with no line table, a file that is not an ELF, and a probe
/// session another debug session holds already. Such a session holds nothing.
///
/// From this call until [`lamella_probe_debug_dispose`], the session holds the probe session: every
/// probe request but the reads, the memory writes and the console's own is refused on it, and so is
/// a write of the part.
///
/// # Safety
/// `elf_ptr`/`elf_len` must be a buffer the host filled via a prior `lamella_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_probe_debug_create(
    probe: u32,
    elf_ptr: *const u8,
    elf_len: usize,
) -> u32 {
    let elf = unsafe { core::slice::from_raw_parts(elf_ptr, elf_len) };
    create(probe, elf)
}

/// Handles one DAP request -- UTF-8 JSON at `ptr..ptr + len` -- in the debug session `debug`, and
/// returns `[u32 little-endian length][UTF-8 JSON]`: an array of the response and the events that
/// follow it, as `lamella_dap_request` answers. Free it with `lamella_dealloc(result, 4 + length)`.
/// The array is empty for a handle that names no session and for a message that is not a request.
///
/// # Safety
/// `ptr`/`len` must be a buffer the host filled via a prior `lamella_alloc`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lamella_probe_debug_request(
    debug: u32,
    ptr: *const u8,
    len: usize,
) -> *mut u8 {
    let bytes = unsafe { core::slice::from_raw_parts(ptr, len) };
    let messages = with_debugger(debug, |debugger| {
        match serde_json::from_slice::<Message>(bytes) {
            Ok(Message::Request(request)) => debugger.handle(&request),
            _ => Vec::new(),
        }
    });
    replies(&messages.unwrap_or_default())
}

/// Looks once at a program the debug session `debug` left running, and returns the same array as
/// [`lamella_probe_debug_request`]: the events of its stop, the output it wrote, or nothing while it
/// runs. A look at a running core is two reads of its halting status through the probe. When the
/// core is stopped at semihosting calls, the same look performs them, a bounded number of them, so
/// their output arrives with it. Nothing waits for the core to stop.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_probe_debug_poll(debug: u32) -> *mut u8 {
    replies(&with_debugger(debug, Debugger::poll).unwrap_or_default())
}

/// 1 while the debug session `debug` has left its program running, so a host looks at it with
/// [`lamella_probe_debug_poll`]; 0 when it is stopped, and for a handle that names no session.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_probe_debug_running(debug: u32) -> u32 {
    u32::from(with_debugger(debug, |debugger| debugger.is_running()).unwrap_or(false))
}

/// Ends the debug session `debug`, and with it the session's hold on its probe session. Unless the
/// program was ended, the part is handed back first, as a `disconnect` hands it back: every
/// breakpoint removed, the core running, and halting debug off. The probe session stays open, for
/// the host to dispose of separately.
#[unsafe(no_mangle)]
pub extern "C" fn lamella_probe_debug_dispose(debug: u32) {
    let session = DEBUG_SESSIONS.with(|sessions| {
        sessions
            .borrow_mut()
            .get_mut((debug as usize).wrapping_sub(1))
            .and_then(Option::take)
    });
    if let Some(mut session) = session {
        let _ = session.debugger.release_target();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{
        answer_with, lamella_probe_create, lamella_probe_dispose, lamella_probe_flash,
        lamella_probe_follow_console, lamella_probe_request, reset_host,
    };
    use lamella_cmsis_dap::proto;
    use serde_json::json;
    use std::collections::HashMap;
    use std::rc::Rc;

    /// Where the test program's code begins, as an RP2350's image does, and its entry.
    const ENTRY: u32 = 0x1000_0100;
    /// The source file the test program's line table names.
    const FILE: &str = "Program.cs";
    /// The lines of the test program's four rows, one every four bytes from [`ENTRY`].
    const LINES: [u32; 4] = [10, 11, 12, 13];
    /// Where the test program's console ring lives in RAM, and its transcript's size.
    const RING: u32 = 0x2000_0100;
    const RING_SIZE: u32 = 64;

    /// A part behind a probe, as far as a debug session and a console follow reach it.
    ///
    /// Its debug port addresses the core's memory access port as an RP2350's does, by ADIv6 base
    /// address: the port answers only while `SELECT` holds 0x2d00, and an access made through any
    /// other selection reaches nothing -- a write goes nowhere, and a read answers 0. The core runs
    /// a loop of straight-line Thumb code, one instruction for each look at DHCSR while it runs, and
    /// it halts where an enabled comparator holds the next instruction's address. The breakpoint unit
    /// is the Armv8-M revision, with four comparators.
    struct Part {
        /// The debug port's `SELECT`.
        select: u32,
        /// Every value written to `SELECT` since [`Part::forget`].
        selects: Vec<u32>,
        /// The command byte of every packet since [`Part::forget`].
        commands: Vec<u8>,
        tar: u32,
        words: HashMap<u32, u32>,
        debugen: bool,
        halted: bool,
        /// r0 to r12, the stack pointer, the link register, the program counter and xPSR, by their
        /// DCRSR selectors.
        registers: [u32; 17],
        dcrdr: u32,
        breakpoint_unit: bool,
        comparators: [u32; 4],
        /// How many times a comparator has halted the core.
        stops: usize,
        /// The loop's last instruction, after which the core runs from [`ENTRY`] again.
        last: u32,
    }

    impl Part {
        const DPIDR: u32 = 0x4c01_3477;
        /// The selection that reaches the core's memory access port.
        const CORE_PORT: u32 = 0x2d00;
        const DHCSR: u32 = 0xe000_edf0;
        const DCRSR: u32 = 0xe000_edf4;
        const DCRDR: u32 = 0xe000_edf8;
        const FP_CTRL: u32 = 0xe000_2000;
        const FP_COMP0: u32 = 0xe000_2008;
        const PC: usize = 15;

        /// As a stop at the entry leaves a part: the core halted at [`ENTRY`] with halting debug on,
        /// nothing armed, and the program's console ring set up in its RAM.
        fn halted_at_the_entry() -> Self {
            let mut words = HashMap::new();
            for word in 0..8 {
                words.insert(ENTRY + word * 4, 0xbf00_bf00);
            }
            words.insert(RING + 4, RING_SIZE);
            words.insert(RING, lamella_probe_console::RUNNING);
            let mut registers = [0; 17];
            registers[13] = 0x2000_8000;
            registers[Self::PC] = ENTRY;
            registers[16] = 0x0100_0000;
            Part {
                select: 0,
                selects: Vec::new(),
                commands: Vec::new(),
                tar: 0,
                words,
                debugen: true,
                halted: true,
                registers,
                dcrdr: 0,
                breakpoint_unit: false,
                comparators: [0; 4],
                stops: 0,
                last: ENTRY + 0x1e,
            }
        }

        /// Empties the logs, so a test reads what came after.
        fn forget(&mut self) {
            self.selects.clear();
            self.commands.clear();
        }

        fn comparator(address: u32) -> Option<usize> {
            let index = address.checked_sub(Self::FP_COMP0)? / 4;
            (address.is_multiple_of(4) && index < 4).then_some(index as usize)
        }

        /// Whether the core halts before the instruction at `address`: halting debug on, the unit on,
        /// and an enabled comparator holding the address (Armv8-M ARM, D1.2.106).
        fn breaks_at(&self, address: u32) -> bool {
            self.debugen
                && self.breakpoint_unit
                && self
                    .comparators
                    .iter()
                    .any(|&comparator| comparator & 1 != 0 && comparator & !1 == address)
        }

        /// One instruction: the program counter moves to the next one, and from the loop's last
        /// back to its first.
        fn advance(&mut self) {
            self.registers[Self::PC] = if self.registers[Self::PC] < self.last {
                self.registers[Self::PC] + 2
            } else {
                ENTRY
            };
        }

        /// Halts the core where a comparator holds the instruction it runs next, and counts the stop.
        fn halt_at_a_comparator(&mut self) {
            self.halted = self.breaks_at(self.registers[Self::PC]);
            self.stops += usize::from(self.halted);
        }

        /// A DHCSR write: halting debug on or off, a halt, a resume, and a resume for one step.
        fn control(&mut self, value: u32) {
            self.debugen = value & 1 != 0;
            if !self.debugen {
                self.halted = false;
            } else if value & 2 != 0 {
                self.halted = true;
            } else if self.halted && value & 4 != 0 {
                self.advance();
            } else if self.halted {
                self.halt_at_a_comparator();
            }
        }

        /// A read as the probe makes one: a look at DHCSR while the core runs is one instruction.
        fn look(&mut self, address: u32) -> u32 {
            if address == Self::DHCSR && !self.halted {
                self.advance();
                self.halt_at_a_comparator();
            }
            self.read(address)
        }

        fn read(&self, address: u32) -> u32 {
            match address {
                Self::DHCSR => u32::from(self.debugen) | 1 << 16 | u32::from(self.halted) << 17,
                Self::DCRDR => self.dcrdr,
                Self::FP_CTRL => 1 << 28 | 4 << 4 | u32::from(self.breakpoint_unit),
                _ => match Self::comparator(address) {
                    Some(index) => self.comparators[index],
                    None => self.words.get(&(address & !3)).copied().unwrap_or(0),
                },
            }
        }

        fn write(&mut self, address: u32, value: u32) {
            match address {
                Self::DHCSR if value >> 16 == 0xa05f => self.control(value),
                Self::DCRDR => self.dcrdr = value,
                Self::DCRSR => {
                    let selector = (value & 0x7f) as usize;
                    if value & (1 << 16) != 0 {
                        if let Some(register) = self.registers.get_mut(selector) {
                            *register = self.dcrdr;
                        }
                    } else {
                        self.dcrdr = self.registers.get(selector).copied().unwrap_or(0);
                    }
                }
                Self::FP_CTRL if value & 2 != 0 => self.breakpoint_unit = value & 1 != 0,
                _ => match Self::comparator(address) {
                    Some(index) => self.comparators[index] = value,
                    None => {
                        self.words.insert(address & !3, value);
                    }
                },
            }
        }

        /// One DP or AP register access; the word a read returns.
        fn transfer(&mut self, request: u8, data: u32) -> Option<u32> {
            let read = request & 0x02 != 0;
            let register = request & 0x0c;
            if request & 0x01 == 0 {
                return match (register, read) {
                    (0x0, true) => Some(Self::DPIDR),
                    (0x4, true) => Some(0xf000_0000),
                    (0x8, false) => {
                        self.select = data;
                        self.selects.push(data);
                        None
                    }
                    _ => read.then_some(0),
                };
            }
            if self.select & !0xf != Self::CORE_PORT {
                return read.then_some(0);
            }
            match register {
                0x4 if !read => {
                    self.tar = data;
                    None
                }
                0xc => {
                    let address = self.tar;
                    self.tar = self.tar.wrapping_add(4);
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
            self.commands.push(op);
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

    fn uleb128(out: &mut Vec<u8>, mut value: u64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn sleb128(out: &mut Vec<u8>, mut value: i64) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            let done = (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0);
            out.push(if done { byte } else { byte | 0x80 });
            if done {
                return;
            }
        }
    }

    fn inline_string(out: &mut Vec<u8>, text: &str) {
        out.extend_from_slice(text.as_bytes());
        out.push(0);
    }

    /// A DWARF 5 `.debug_line` holding one sequence: a row for each of `lines`, four bytes apart
    /// from `start`, in `file`.
    fn line_section(file: &str, start: u32, lines: &[u32]) -> Vec<u8> {
        const DW_LNS_COPY: u8 = 0x01;
        const DW_LNS_ADVANCE_PC: u8 = 0x02;
        const DW_LNS_ADVANCE_LINE: u8 = 0x03;
        const DW_LNE_END_SEQUENCE: u8 = 0x01;
        const DW_LNE_SET_ADDRESS: u8 = 0x02;
        const DW_LNCT_PATH: u64 = 1;
        const DW_LNCT_DIRECTORY_INDEX: u64 = 2;
        const DW_FORM_STRING: u64 = 0x08;
        const DW_FORM_UDATA: u64 = 0x0f;

        let mut program = vec![0x00, 0x05, DW_LNE_SET_ADDRESS];
        program.extend_from_slice(&start.to_le_bytes());
        let mut line = 1;
        for (index, &row) in lines.iter().enumerate() {
            if index > 0 {
                program.push(DW_LNS_ADVANCE_PC);
                uleb128(&mut program, 4);
            }
            program.push(DW_LNS_ADVANCE_LINE);
            sleb128(&mut program, i64::from(row) - line);
            line = i64::from(row);
            program.push(DW_LNS_COPY);
        }
        program.push(DW_LNS_ADVANCE_PC);
        uleb128(&mut program, 4);
        program.extend_from_slice(&[0x00, 0x01, DW_LNE_END_SEQUENCE]);

        let mut header = vec![1, 1, 1, (-5i8) as u8, 14, 13];
        header.extend_from_slice(&[0, 1, 1, 1, 1, 0, 0, 0, 1, 0, 0, 1]);
        header.push(1);
        uleb128(&mut header, DW_LNCT_PATH);
        uleb128(&mut header, DW_FORM_STRING);
        uleb128(&mut header, 1);
        inline_string(&mut header, "");
        header.push(2);
        uleb128(&mut header, DW_LNCT_PATH);
        uleb128(&mut header, DW_FORM_STRING);
        uleb128(&mut header, DW_LNCT_DIRECTORY_INDEX);
        uleb128(&mut header, DW_FORM_UDATA);
        uleb128(&mut header, 2);
        for _ in 0..2 {
            inline_string(&mut header, file);
            uleb128(&mut header, 0);
        }

        let mut body = 5u16.to_le_bytes().to_vec();
        body.extend_from_slice(&[4, 0]);
        body.extend_from_slice(&(header.len() as u32).to_le_bytes());
        body.extend_from_slice(&header);
        body.extend_from_slice(&program);
        let mut section = (body.len() as u32).to_le_bytes().to_vec();
        section.extend_from_slice(&body);
        section
    }

    /// A DWARF 4 `.debug_abbrev` and `.debug_info` describing one unit with one subprogram, `name`,
    /// covering `length` bytes from `low`.
    fn info_sections(name: &str, low: u32, length: u32) -> (Vec<u8>, Vec<u8>) {
        const DW_TAG_COMPILE_UNIT: u64 = 0x11;
        const DW_TAG_SUBPROGRAM: u64 = 0x2e;
        const DW_AT_NAME: u64 = 0x03;
        const DW_AT_LOW_PC: u64 = 0x11;
        const DW_AT_HIGH_PC: u64 = 0x12;
        const DW_FORM_ADDR: u64 = 0x01;
        const DW_FORM_DATA4: u64 = 0x06;
        const DW_FORM_STRING: u64 = 0x08;

        let mut abbrev = Vec::new();
        uleb128(&mut abbrev, 1);
        uleb128(&mut abbrev, DW_TAG_COMPILE_UNIT);
        abbrev.push(1);
        uleb128(&mut abbrev, DW_AT_NAME);
        uleb128(&mut abbrev, DW_FORM_STRING);
        abbrev.extend_from_slice(&[0, 0]);
        uleb128(&mut abbrev, 2);
        uleb128(&mut abbrev, DW_TAG_SUBPROGRAM);
        abbrev.push(0);
        uleb128(&mut abbrev, DW_AT_NAME);
        uleb128(&mut abbrev, DW_FORM_STRING);
        uleb128(&mut abbrev, DW_AT_LOW_PC);
        uleb128(&mut abbrev, DW_FORM_ADDR);
        uleb128(&mut abbrev, DW_AT_HIGH_PC);
        uleb128(&mut abbrev, DW_FORM_DATA4);
        abbrev.extend_from_slice(&[0, 0, 0]);

        let mut body = 4u16.to_le_bytes().to_vec();
        body.extend_from_slice(&0u32.to_le_bytes());
        body.push(4);
        uleb128(&mut body, 1);
        inline_string(&mut body, FILE);
        uleb128(&mut body, 2);
        inline_string(&mut body, name);
        body.extend_from_slice(&low.to_le_bytes());
        body.extend_from_slice(&length.to_le_bytes());
        body.push(0);
        let mut info = (body.len() as u32).to_le_bytes().to_vec();
        info.extend_from_slice(&body);
        (abbrev, info)
    }

    /// The test program's debug ELF, as a class-library build writes one: sixteen Thumb NOPs at
    /// [`ENTRY`], which is also its entry; a line table putting [`LINES`] four bytes apart from the
    /// entry; one function, `Program.Main`, covering all the code; and the console ring at [`RING`].
    fn debug_elf() -> Vec<u8> {
        let line = line_section(FILE, ENTRY, &LINES);
        let (abbrev, info) = info_sections("Program.Main", ENTRY, 0x20);
        lamella_elf::write_debuggable_executable_with_objects(
            lamella_elf::Machine::Arm,
            &[0x00, 0xbf].repeat(16),
            0,
            ENTRY,
            true,
            &[
                (".debug_line", &line),
                (".debug_abbrev", &abbrev),
                (".debug_info", &info),
            ],
            &[lamella_elf::NamedObject {
                name: lamella_probe_console::LAMELLA_RING,
                section: lamella_probe_console::LAMELLA_RING,
                address: RING,
                size: 16 + RING_SIZE,
            }],
        )
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

    /// Runs a probe request through the exported call and returns its envelope.
    fn probe_request(probe: u32, body: &Value) -> Value {
        let bytes = serde_json::to_vec(body).unwrap();
        take_result(unsafe { lamella_probe_request(probe, bytes.as_ptr(), bytes.len()) })
    }

    /// Writes `image` to the part through the exported call and returns its envelope.
    fn write_the_part(probe: u32, image: &[u8]) -> Value {
        let family = "rp2350";
        take_result(unsafe {
            lamella_probe_flash(
                probe,
                family.as_ptr(),
                family.len(),
                image.as_ptr(),
                image.len(),
            )
        })
    }

    /// Starts a debug session through the exported call.
    fn debug_session(probe: u32, elf: &[u8]) -> u32 {
        unsafe { lamella_probe_debug_create(probe, elf.as_ptr(), elf.len()) }
    }

    /// Sends one DAP request to the debug session through the exported call; the replies.
    fn dap(debug: u32, seq: i64, command: &str, arguments: Value) -> Vec<Value> {
        let message = json!({
            "type": "request",
            "seq": seq,
            "command": command,
            "arguments": arguments,
        });
        let bytes = serde_json::to_vec(&message).unwrap();
        let raw = unsafe { lamella_probe_debug_request(debug, bytes.as_ptr(), bytes.len()) };
        take_result(raw).as_array().cloned().unwrap_or_default()
    }

    /// One look at the debug session's running program, through the exported call; the replies.
    fn look(debug: u32) -> Vec<Value> {
        take_result(lamella_probe_debug_poll(debug))
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// The response to `command` among `replies`.
    fn response<'a>(replies: &'a [Value], command: &str) -> &'a Value {
        replies
            .iter()
            .find(|reply| reply["type"] == "response" && reply["command"] == command)
            .unwrap_or_else(|| panic!("no `{command}` response in {replies:?}"))
    }

    /// The `name` event among `replies`, if one is there.
    fn event<'a>(replies: &'a [Value], name: &str) -> Option<&'a Value> {
        replies
            .iter()
            .find(|reply| reply["type"] == "event" && reply["event"] == name)
    }

    /// Initializes and launches the debug session, stopping at the entry, and returns the launch's
    /// response.
    fn launch(debug: u32) -> Value {
        dap(debug, 1, "initialize", json!({}));
        response(
            &dap(debug, 2, "launch", json!({ "stopOnEntry": true })),
            "launch",
        )
        .clone()
    }

    /// Why the debug session's launch failed: the message a client shows.
    fn launch_refusal(debug: u32) -> String {
        let launched = launch(debug);
        assert_eq!(launched["success"], json!(false), "{launched}");
        launched["message"].as_str().unwrap_or_default().to_string()
    }

    /// A probe session over `part`, connected the RP2350's way as the page connects one, with the
    /// part's logs emptied after the connect.
    fn connected(part: Part) -> (u32, Rc<RefCell<Part>>) {
        reset_host();
        let part = Rc::new(RefCell::new(part));
        let model = Rc::clone(&part);
        answer_with(move |command| model.borrow_mut().answer(command));
        let probe = lamella_probe_create();
        let reply = probe_request(probe, &json!({ "op": "connect", "family": "rp2350" }));
        assert_eq!(reply["ok"], json!(true), "{reply}");
        part.borrow_mut().forget();
        (probe, part)
    }

    /// **A DEBUG SESSION TAKES THE PART AS THE PAGE CONNECTED IT.** The page connects an RP2350 its
    /// family's way, which selects the core's memory access port by its ADIv6 address. The session's
    /// launch sends no connect and writes no `SELECT`, so the port stays selected, and the stop at
    /// the entry is read through it: the entry's function, at the entry's line.
    #[test]
    fn a_debug_session_launches_on_the_part_as_the_page_connected_it() {
        let (probe, part) = connected(Part::halted_at_the_entry());
        assert_eq!(part.borrow().select, Part::CORE_PORT, "the page's connect");
        let debug = debug_session(probe, &debug_elf());
        assert_ne!(debug, 0);

        let launched = launch(debug);
        assert_eq!(launched["success"], json!(true), "{launched}");
        let configured = dap(debug, 3, "configurationDone", json!({}));
        let stopped = event(&configured, "stopped").expect("a stop is reported");
        assert_eq!(stopped["body"]["reason"], json!("entry"), "{stopped}");
        let frames = dap(debug, 4, "stackTrace", json!({ "threadId": 1 }));
        let top = &response(&frames, "stackTrace")["body"]["stackFrames"][0];
        assert_eq!(top["name"], json!("Program.Main"), "{top}");
        assert_eq!(top["line"], json!(LINES[0]), "{top}");

        let part = part.borrow();
        assert!(
            part.selects.is_empty(),
            "the session wrote no SELECT: {:x?}",
            part.selects
        );
        assert!(
            !part.commands.contains(&proto::cmd::CONNECT)
                && !part.commands.contains(&proto::cmd::SWJ_SEQUENCE),
            "the session sent no connect: {:02x?}",
            part.commands
        );
        assert!(part.halted && part.debugen, "the core is held at the entry");
        drop(part);
        lamella_probe_debug_dispose(debug);
        lamella_probe_dispose(probe);
    }

    /// **A SESSION RUNS ITS PROGRAM TO A BREAKPOINT, AND ITS DISPOSAL HANDS THE PART BACK.** A
    /// source breakpoint is armed on a comparator; a continue leaves the core running and the
    /// session saying so; each look at it is a few packets and waits for nothing; and the look
    /// after the core reaches the breakpoint reports the stop, at the breakpoint's line. Disposing of
    /// the session with the core stopped there removes the breakpoint, lets the core run and turns
    /// halting debug off. After that, the handle names no session.
    #[test]
    fn a_session_runs_to_a_breakpoint_and_its_disposal_hands_the_part_back() {
        let (probe, part) = connected(Part::halted_at_the_entry());
        let debug = debug_session(probe, &debug_elf());
        assert_eq!(launch(debug)["success"], json!(true));
        dap(debug, 3, "configurationDone", json!({}));
        let set = dap(
            debug,
            4,
            "setBreakpoints",
            json!({ "source": { "path": FILE }, "breakpoints": [{ "line": LINES[2] }] }),
        );
        let armed = &response(&set, "setBreakpoints")["body"]["breakpoints"][0];
        assert_eq!(armed["verified"], json!(true), "{armed}");
        assert_eq!(
            lamella_probe_debug_running(debug),
            0,
            "stopped at the entry"
        );

        let resumed = dap(debug, 5, "continue", json!({ "threadId": 1 }));
        assert_eq!(response(&resumed, "continue")["success"], json!(true));
        assert_eq!(lamella_probe_debug_running(debug), 1, "the program runs");
        let before = part.borrow().commands.len();
        let first = look(debug);
        let packets = part.borrow().commands.len() - before;
        assert!(event(&first, "stopped").is_none(), "{first:?}");
        assert!(
            (1..=8).contains(&packets),
            "a look is a few packets: {packets}"
        );

        let mut stopped = None;
        for _ in 0..32 {
            if let Some(stop) = event(&look(debug), "stopped") {
                stopped = Some(stop.clone());
                break;
            }
        }
        let stopped = stopped.expect("the core reaches the breakpoint");
        assert_eq!(stopped["body"]["reason"], json!("breakpoint"), "{stopped}");
        assert_eq!(lamella_probe_debug_running(debug), 0);
        let frames = dap(debug, 6, "stackTrace", json!({ "threadId": 1 }));
        let top = &response(&frames, "stackTrace")["body"]["stackFrames"][0];
        assert_eq!(top["line"], json!(LINES[2]), "{top}");
        assert_eq!(part.borrow().registers[Part::PC], ENTRY + 8);

        lamella_probe_debug_dispose(debug);
        {
            let part = part.borrow();
            assert!(!part.halted, "the core runs");
            assert!(!part.debugen, "halting debug is off");
            assert!(!part.breakpoint_unit, "the breakpoint unit is off");
            assert_eq!(part.comparators, [0; 4], "no comparator is armed");
        }
        assert_eq!(lamella_probe_debug_running(debug), 0);
        assert!(look(debug).is_empty());
        assert!(dap(debug, 7, "threads", json!({})).is_empty());
        lamella_probe_debug_dispose(debug);
        lamella_probe_dispose(probe);
    }

    /// **WHILE A DEBUG SESSION HOLDS A PROBE SESSION, WHAT WOULD MOVE THE CORE OR CHANGE WHAT STOPS
    /// IT IS REFUSED, AND NOTHING OF IT REACHES THE PROBE.** Each refusal names the debug session and
    /// the call that ends its hold. Reads, memory writes and the console's requests go on, and once
    /// the debug session is disposed of, the refused requests are taken again.
    #[test]
    fn a_held_probe_session_refuses_run_control_and_writes_and_answers_reads() {
        let (probe, part) = connected(Part::halted_at_the_entry());
        let debug = debug_session(probe, &debug_elf());
        let sent = part.borrow().commands.len();
        for op in [
            "connect",
            "disconnect",
            "halt",
            "resume",
            "step",
            "resetRun",
            "resetHalt",
            "armResetCatch",
            "disarmResetCatch",
            "writeCoreReg",
            "setBreakpoints",
            "clearBreakpoints",
            "stopAtEntry",
        ] {
            let reply = probe_request(
                probe,
                &json!({
                    "op": op,
                    "family": "rp2350",
                    "entry": ENTRY | 1,
                    "selector": 15,
                    "value": ENTRY,
                    "addresses": [ENTRY],
                }),
            );
            assert_eq!(reply["ok"], json!(false), "{op}: {reply}");
            let error = reply["error"].as_str().unwrap_or_default();
            assert!(
                error.contains(&format!("`{op}`"))
                    && error.contains(&format!("debug session {debug}"))
                    && error.contains("lamella_probe_debug_dispose"),
                "{op}: {reply}"
            );
        }
        let written = write_the_part(probe, &[0x5a; 64]);
        assert_eq!(written["ok"], json!(false), "{written}");
        assert!(
            written["error"]
                .as_str()
                .unwrap_or_default()
                .contains(&format!("debug session {debug}")),
            "{written}"
        );
        assert_eq!(
            part.borrow().commands.len(),
            sent,
            "no refused request reached the probe"
        );

        for body in [
            json!({ "op": "info" }),
            json!({ "op": "isHalted" }),
            json!({ "op": "readWords", "address": ENTRY, "count": 2 }),
            json!({ "op": "writeWords", "address": 0x2000_0400u32, "values": [7] }),
            json!({ "op": "readCoreReg", "selector": 15 }),
            json!({ "op": "readCoreRegs" }),
            json!({ "op": "consoleStop" }),
        ] {
            let reply = probe_request(probe, &body);
            assert_eq!(reply["ok"], json!(true), "{body}: {reply}");
        }
        assert_eq!(part.borrow().words.get(&0x2000_0400), Some(&7));

        lamella_probe_debug_dispose(debug);
        let halted = probe_request(probe, &json!({ "op": "halt" }));
        assert_eq!(
            halted["ok"],
            json!(true),
            "the hold ends with the session: {halted}"
        );
        lamella_probe_dispose(probe);
    }

    /// **A WRITE OR A STOP REFUSED WHILE A SESSION DEBUGS LEAVES THE CONSOLE FOLLOWED.** Both end a
    /// follow when they are carried out, because the program being followed ends with them. Refused,
    /// they change nothing, and the program the debugger holds goes on being followed.
    #[test]
    fn a_refused_write_or_stop_leaves_the_console_followed() {
        let (probe, _part) = connected(Part::halted_at_the_entry());
        let elf = debug_elf();
        let followed =
            take_result(unsafe { lamella_probe_follow_console(probe, elf.as_ptr(), elf.len(), 0) });
        assert_eq!(followed["ok"], json!(true), "{followed}");
        let debug = debug_session(probe, &elf);

        assert_eq!(write_the_part(probe, &[0x5a; 64])["ok"], json!(false));
        let stop = probe_request(
            probe,
            &json!({ "op": "stopAtEntry", "family": "rp2350", "entry": ENTRY | 1 }),
        );
        assert_eq!(stop["ok"], json!(false), "{stop}");
        let poll = probe_request(probe, &json!({ "op": "consolePoll" }));
        assert_eq!(poll["ok"], json!(true), "the follow goes on: {poll}");

        lamella_probe_debug_dispose(debug);
        lamella_probe_dispose(probe);
    }

    /// **A SESSION THAT CANNOT DEBUG STILL ANSWERS, AND SAYS WHY WHEN IT IS LAUNCHED.** A handle that
    /// names no probe session gets no session at all. An ELF with no line table gets one whose launch
    /// says so, and so does a probe session another debug session holds, naming that session. A
    /// session that cannot start holds nothing, and disposing of it ends no other session's hold.
    #[test]
    fn a_session_that_cannot_debug_answers_and_its_launch_says_why() {
        let (probe, _part) = connected(Part::halted_at_the_entry());
        assert_eq!(debug_session(probe + 1, &debug_elf()), 0);

        let bare = lamella_elf::write_executable_arm_thumb(&[0x00, 0xbf, 0x00, 0xbf], 0, ENTRY);
        let undebuggable = debug_session(probe, &bare);
        assert_ne!(undebuggable, 0);
        let why = launch_refusal(undebuggable);
        assert!(why.contains(".debug_line"), "{why}");
        let halted = probe_request(probe, &json!({ "op": "halt" }));
        assert_eq!(halted["ok"], json!(true), "it holds nothing: {halted}");

        let first = debug_session(probe, &debug_elf());
        let second = debug_session(probe, &debug_elf());
        assert_ne!(second, 0);
        let why = launch_refusal(second);
        assert!(why.contains(&format!("debug session {first}")), "{why}");
        lamella_probe_debug_dispose(second);
        let halted = probe_request(probe, &json!({ "op": "halt" }));
        assert_eq!(
            halted["ok"],
            json!(false),
            "the first still holds it: {halted}"
        );

        lamella_probe_debug_dispose(first);
        lamella_probe_debug_dispose(undebuggable);
        lamella_probe_dispose(probe);
    }

    /// Sets one source breakpoint, `breakpoint`, on the debug session's program, and lets the
    /// program run from the entry.
    fn run_with(debug: u32, breakpoint: Value) {
        assert_eq!(launch(debug)["success"], json!(true));
        dap(debug, 3, "configurationDone", json!({}));
        let set = dap(
            debug,
            4,
            "setBreakpoints",
            json!({ "source": { "path": FILE }, "breakpoints": [breakpoint] }),
        );
        let armed = &response(&set, "setBreakpoints")["body"]["breakpoints"][0];
        assert_eq!(armed["verified"], json!(true), "{armed}");
        dap(debug, 5, "continue", json!({ "threadId": 1 }));
    }

    /// **A LOGPOINT WRITES ITS MESSAGE AT EACH HIT, AND THE PROGRAM RUNS ON.** The part's program
    /// loops, so the logpoint's line comes round on every pass, and every stop there is taken as a
    /// logpoint is: its message written, and the core let go again. Nothing reports a stop.
    #[test]
    fn a_logpoint_writes_its_message_and_the_program_runs_on() {
        let (probe, part) = connected(Part::halted_at_the_entry());
        let debug = debug_session(probe, &debug_elf());
        run_with(
            debug,
            json!({ "line": LINES[2], "logMessage": "at twelve" }),
        );
        let mut seen = Vec::new();
        for _ in 0..64 {
            seen.extend(look(debug));
        }
        assert!(
            event(&seen, "stopped").is_none(),
            "a logpoint stopped the program: {seen:?}"
        );
        let printed = seen
            .iter()
            .filter(|reply| reply["event"] == "output" && reply["body"]["output"] == "at twelve\n")
            .count();
        let stops = part.borrow().stops;
        assert!(
            printed >= 2 && stops - printed <= 1,
            "each pass writes the message: {printed} written for {stops} stops"
        );
        assert_eq!(lamella_probe_debug_running(debug), 1, "the program runs on");
        lamella_probe_debug_dispose(debug);
        lamella_probe_dispose(probe);
    }

    /// **A HIT COUNT STOPS THE PROGRAM AT THE HIT IT NAMES.** With `hitCondition: "2"`, the first
    /// pass over the line goes on unreported, and the program stops at the second.
    #[test]
    fn a_hit_count_stops_the_program_at_the_hit_it_names() {
        let (probe, part) = connected(Part::halted_at_the_entry());
        let debug = debug_session(probe, &debug_elf());
        run_with(debug, json!({ "line": LINES[2], "hitCondition": "2" }));
        let mut stopped = None;
        for _ in 0..64 {
            if let Some(stop) = event(&look(debug), "stopped") {
                stopped = Some(stop.clone());
                break;
            }
        }
        let stopped = stopped.expect("the program stops");
        assert_eq!(stopped["body"]["reason"], json!("breakpoint"), "{stopped}");
        assert_eq!(part.borrow().stops, 2, "the stop is the second hit's");
        assert_eq!(part.borrow().registers[Part::PC], ENTRY + 8);
        lamella_probe_debug_dispose(debug);
        lamella_probe_dispose(probe);
    }

    /// **A SESSION WHOSE PROBE SESSION IS DISPOSED OF SAYS THE PROBE SESSION IS GONE.** Every call
    /// its backend makes looks the probe session up, and a handle is never reused, so the session
    /// cannot reach a probe session opened after it.
    #[test]
    fn a_session_whose_probe_session_is_gone_says_so() {
        let (probe, _part) = connected(Part::halted_at_the_entry());
        let debug = debug_session(probe, &debug_elf());
        lamella_probe_dispose(probe);
        let later = lamella_probe_create();
        assert_ne!(later, probe);
        let why = launch_refusal(debug);
        assert!(
            why.contains(&format!("the probe session {probe} is gone")),
            "{why}"
        );
        lamella_probe_debug_dispose(debug);
        lamella_probe_dispose(later);
    }
}
