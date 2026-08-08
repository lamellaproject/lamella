//! The probe-neutral debug seam.

#![forbid(unsafe_code)]

use std::fmt;

/// The acknowledge a DP/AP transfer returned. `Ok` is success; the others are the ADIv5 wire-level
/// responses a probe reports back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// The transfer completed.
    Ok,
    /// The target asked for a retry.
    Wait,
    /// The target reported a fault (sticky error flags are set in the DP).
    Fault,
    /// No acknowledge at all -- typically nothing is driving the wire.
    NoAck,
    /// An acknowledge value the specification does not define.
    Unknown(u8),
}

/// An error from a debug operation, at any layer.
///
/// Probe-specific decode detail is flattened to a string so this crate can stay dependency-free;
/// each probe family converts its own error into [`ProbeError::Protocol`] or
/// [`ProbeError::Transport`] with a `From` impl on its own side of the boundary.
#[derive(Debug)]
pub enum ProbeError {
    /// The packet transport to the probe failed.
    Transport(String),
    /// The probe's reply could not be decoded, or echoed the wrong command.
    Protocol(String),
    /// A transfer returned a non-OK acknowledge.
    Ack(Ack),
    /// An operation polled past its limit without completing (names what was awaited).
    Timeout(&'static str),
    /// The target device reported an operation failure (names the device-side condition) -- e.g. a
    /// flash controller refusing a command or failing its post-write verify.
    Device(&'static str),
    /// The probe is present but cannot be driven -- e.g. on Windows its debug interface is not bound
    /// to a usable driver. Carries the remedy, so callers can print something actionable rather than
    /// a cryptic transport failure.
    Unusable(String),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProbeError::Transport(e) => write!(f, "{e}"),
            ProbeError::Protocol(e) => write!(f, "malformed probe reply: {e}"),
            ProbeError::Ack(ack) => write!(f, "transfer not acknowledged: {ack:?}"),
            ProbeError::Timeout(what) => write!(f, "timed out waiting for {what}"),
            ProbeError::Device(what) => write!(f, "target device error: {what}"),
            ProbeError::Unusable(remedy) => write!(f, "probe present but not usable: {remedy}"),
        }
    }
}

impl std::error::Error for ProbeError {}

/// The scratch frame a [`TargetAccess::call_target`] invocation runs in: a stack top and a
/// return-trap address, both in the TARGET's RAM (chip-specific, so the caller supplies them --
/// e.g. `0x2004_0000` / `0x2000_0000` on an RP2350), plus the halt-poll budget (raise it for a
/// long-running callee such as a flash erase, which can take many polls to return).
#[derive(Debug, Clone, Copy)]
pub struct CallFrame {
    /// Scratch stack top (full-descending SP), in target RAM.
    pub sp: u32,
    /// Return-trap address, in target RAM: the call plants a `BKPT` here and points LR at it. The
    /// word at this address is clobbered.
    pub trap: u32,
    /// How many times to poll for the return halt before giving up.
    pub poll_tries: u32,
}

impl CallFrame {
    /// A frame with a default halt-poll budget; set `poll_tries` higher for a long-running callee.
    pub fn new(sp: u32, trap: u32) -> CallFrame {
        CallFrame { sp, trap, poll_tries: 8000 }
    }
}

/// Raw ADIv5 Debug-Port / Access-Port register access -- what a LOW-LEVEL probe provides.
///
/// Implemented by probes that hand us the ARM debug port directly (CMSIS-DAP over HID or bulk,
/// FTDI-MPSSE bit-banging JTAG). High-level probes that do this layer internally (ST-Link, J-Link)
/// implement [`TargetAccess`] instead and never appear here.
///
/// Register addresses are the ADIv5 4-bit register selectors within the currently selected bank.
pub trait DapAccess {
    /// Brings the wire up and connects to the target's debug port (SWD line reset / JTAG TAP reset
    /// and scan, per the implementation's wire).
    fn connect(&mut self) -> Result<(), ProbeError>;

    /// Reads a Debug Port register.
    fn read_dp(&mut self, address: u8) -> Result<u32, ProbeError>;
    /// Writes a Debug Port register.
    fn write_dp(&mut self, address: u8, value: u32) -> Result<(), ProbeError>;
    /// Reads an Access Port register.
    fn read_ap(&mut self, address: u8) -> Result<u32, ProbeError>;
    /// Writes an Access Port register.
    fn write_ap(&mut self, address: u8, value: u32) -> Result<(), ProbeError>;

    /// Drives the probe's nRESET line, returning the probe's reported pin state.
    fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError>;

    /// Writes `values` to one Access Port register back to back, in as few probe round-trips as the
    /// probe allows (CMSIS-DAP streams these with `DAP_TransferBlock`).
    ///
    /// This is not a convenience: with the MEM-AP auto-incrementing `TAR`, block transfers are how
    /// staging and flash programming avoid one USB round-trip PER WORD. The default implementation
    /// loops so a probe without a block command still works -- but a probe that HAS one must
    /// override this, or bulk operations fall off a performance cliff.
    fn write_ap_block(&mut self, address: u8, values: &[u32]) -> Result<(), ProbeError> {
        for &value in values {
            self.write_ap(address, value)?;
        }
        Ok(())
    }

    /// Reads `out.len()` values from one Access Port register back to back, into a CALLER-PROVIDED
    /// buffer. See [`write_ap_block`](Self::write_ap_block) for why the block form matters.
    ///
    /// This is the read primitive, and it takes a buffer rather than returning one so that no layer
    /// of this stack has to allocate: a master that is itself a microcontroller programming a second
    /// microcontroller has no allocator to spare, and retrofitting that later would mean changing
    /// every signature above here. [`DapAccessExt::read_ap_block`] is the allocating convenience for
    /// callers that would rather have a `Vec`; it is deliberately NOT a member of this trait, so an
    /// implementation cannot accelerate the convenience and leave the primitive on the slow path.
    fn read_ap_block_into(&mut self, address: u8, out: &mut [u32]) -> Result<(), ProbeError> {
        for slot in out.iter_mut() {
            *slot = self.read_ap(address)?;
        }
        Ok(())
    }
}

/// The allocating conveniences over [`DapAccess`], provided for every implementation and overridable
/// by none.
///
/// Splitting these out is what keeps the primitive honest. A `Vec`-returning method sitting in
/// `DapAccess` beside the buffer one would be overridable, and an implementation that accelerated
/// only the convenience would leave [`DapAccess::read_ap_block_into`] -- the form every allocation-
/// free caller uses -- silently looping one word at a time. That is the batching cliff described on
/// [`DapAccess::write_ap_block`], and putting the convenience on an extension trait makes it
/// unreachable rather than merely documented.
pub trait DapAccessExt: DapAccess {
    /// Reads `count` values from one Access Port register into a freshly allocated buffer.
    fn read_ap_block(&mut self, address: u8, count: usize) -> Result<Vec<u32>, ProbeError> {
        let mut out = vec![0; count];
        self.read_ap_block_into(address, &mut out)?;
        Ok(out)
    }
}

impl<D: DapAccess + ?Sized> DapAccessExt for D {}

/// Lets a BORROWED accessor stand in for an owned one, so a caller holding only `&mut D` -- a probe
/// discovery session that hands out its debug port, say -- can still wrap it in an [`ArmDap`]
/// without surrendering ownership.
///
/// Every method forwards, INCLUDING the two block operations. Leaving those to the trait's default
/// bodies would compile and work while quietly replacing the probe's native block command with a
/// word-at-a-time loop -- the batching cliff described on [`DapAccess::write_ap_block`], reachable
/// only through a borrow and therefore easy to miss.
impl<D: DapAccess + ?Sized> DapAccess for &mut D {
    fn connect(&mut self) -> Result<(), ProbeError> {
        (**self).connect()
    }
    fn read_dp(&mut self, address: u8) -> Result<u32, ProbeError> {
        (**self).read_dp(address)
    }
    fn write_dp(&mut self, address: u8, value: u32) -> Result<(), ProbeError> {
        (**self).write_dp(address, value)
    }
    fn read_ap(&mut self, address: u8) -> Result<u32, ProbeError> {
        (**self).read_ap(address)
    }
    fn write_ap(&mut self, address: u8, value: u32) -> Result<(), ProbeError> {
        (**self).write_ap(address, value)
    }
    fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError> {
        (**self).set_reset(assert)
    }
    fn write_ap_block(&mut self, address: u8, values: &[u32]) -> Result<(), ProbeError> {
        (**self).write_ap_block(address, values)
    }
    fn read_ap_block_into(&mut self, address: u8, out: &mut [u32]) -> Result<(), ProbeError> {
        (**self).read_ap_block_into(address, out)
    }
}

/// Target memory access and run control -- the seam the flash algorithms, deploy tools, and
/// diagnostics consume, and the ONLY thing they should depend on.
///
/// Implemented two ways: directly by a high-level probe that already speaks memory and run control
/// (ST-Link, J-Link -- keeping its native block operations), or via a per-architecture bridge over
/// [`DapAccess`] for a low-level probe ([`ArmDap`] is the ARM/Cortex-M one).
///
/// Architecture note: memory access and coarse run control are meaningful on ARM, RISC-V, Xtensa and
/// x86 alike, which is what makes this the neutral seam. The register and breakpoint members are
/// currently shaped by ARM/Cortex-M (selector numbering, a single hardware breakpoint); when a
/// second architecture arrives they move to an architecture extension trait rather than growing
/// variants here.
pub trait TargetAccess {
    /// Brings the wire up and connects to the target.
    fn connect(&mut self) -> Result<(), ProbeError>;
    /// Reads the debug port's identification code.
    fn read_idcode(&mut self) -> Result<u32, ProbeError>;
    /// Powers up the debug domains and prepares the memory interface for access.
    fn init_mem(&mut self) -> Result<(), ProbeError>;

    /// Reads a 32-bit word from target memory.
    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError>;
    /// Writes a 32-bit word to target memory.
    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError>;
    /// Reads `out.len()` consecutive words into a CALLER-PROVIDED buffer, batched where the probe
    /// allows.
    ///
    /// The buffer form is the primitive for the same reason it is one layer down -- see
    /// [`DapAccess::read_ap_block_into`]. [`TargetAccessExt::read_words`] is the allocating
    /// convenience, and lives on an extension trait so it cannot be overridden in place of this.
    fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError>;
    /// Writes consecutive words, batched where the probe allows.
    fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError>;

    /// Reads one byte.
    fn read_byte(&mut self, address: u32) -> Result<u8, ProbeError>;
    /// Writes one byte, without disturbing its neighbours.
    fn write_byte(&mut self, address: u32, value: u8) -> Result<(), ProbeError>;
    /// Reads a halfword.
    fn read_halfword(&mut self, address: u32) -> Result<u16, ProbeError>;
    /// Writes a halfword. NOT guaranteed to be a single 16-bit bus cycle -- see the note above.
    fn write_halfword(&mut self, address: u32, value: u16) -> Result<(), ProbeError>;

    /// Halts the processor core.
    fn halt(&mut self) -> Result<(), ProbeError>;
    /// Resumes the processor core.
    fn resume(&mut self) -> Result<(), ProbeError>;
    /// Executes a single instruction.
    fn step(&mut self) -> Result<(), ProbeError>;
    /// Whether the core is currently halted.
    fn is_halted(&mut self) -> Result<bool, ProbeError>;
    /// Polls (bounded) until the core reports halted.
    fn wait_halted(&mut self) -> Result<(), ProbeError>;
    /// Resets the target and lets it run.
    fn reset_and_run(&mut self) -> Result<(), ProbeError>;
    /// Resets the target and catches it halted at the reset vector.
    fn reset_and_halt(&mut self) -> Result<(), ProbeError>;
    /// Drives the probe's nRESET line, returning the probe's reported pin state.
    fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError>;

    /// Reads a core register by its architecture-specific selector.
    fn read_core_reg(&mut self, selector: u8) -> Result<u32, ProbeError>;
    /// Writes a core register by its architecture-specific selector.
    fn write_core_reg(&mut self, selector: u8, value: u32) -> Result<(), ProbeError>;

    /// Arms halting debug and the reset vector catch, so the next reset -- from any source -- halts
    /// at the reset vector before the first instruction runs.
    fn arm_reset_catch(&mut self) -> Result<(), ProbeError>;
    /// Disarms the reset vector catch, so later resets boot freely.
    fn disarm_reset_catch(&mut self) -> Result<(), ProbeError>;

    /// Sets a hardware breakpoint at `address`.
    fn set_breakpoint(&mut self, address: u32) -> Result<(), ProbeError>;
    /// Clears the hardware breakpoint(s).
    fn clear_breakpoint(&mut self) -> Result<(), ProbeError>;
    /// Replaces every hardware breakpoint with `addresses`, one per comparator; comparators past
    /// `addresses` are cleared and addresses beyond the unit's capacity are dropped.
    fn set_breakpoints(&mut self, addresses: &[u32]) -> Result<(), ProbeError>;

    /// Calls a function already resident in target memory and returns its result, running it on the
    /// supplied scratch [`CallFrame`]. Used by flash algorithms that stage a loader into RAM.
    fn call_target(&mut self, address: u32, args: &[u32], frame: &CallFrame) -> Result<u32, ProbeError>;
}

/// The allocating conveniences over [`TargetAccess`], provided for every implementation and
/// overridable by none -- the counterpart of [`DapAccessExt`], and split out for the same reason.
pub trait TargetAccessExt: TargetAccess {
    /// Reads `count` consecutive words into a freshly allocated buffer.
    fn read_words(&mut self, address: u32, count: usize) -> Result<Vec<u32>, ProbeError> {
        let mut out = vec![0; count];
        self.read_words_into(address, &mut out)?;
        Ok(out)
    }
}

impl<T: TargetAccess + ?Sized> TargetAccessExt for T {}

/// The primitives Cortex-M run control is built out of.
///
/// Halting a core, stepping it, reading its registers and planting breakpoints are all just writes
/// and reads to debug registers in the target's own address space. NONE of it depends on how that
/// memory is reached -- a CMSIS-DAP host driving an ADIv5 MEM-AP by hand and an ST-Link whose
/// firmware does it internally arrive at the same registers. So the logic is written once against
/// this trait, in [`cortex_m`], and every probe family gets run control for the cost of its
/// transport.
pub trait CoreMemory {
    /// Reads a 32-bit word of target memory.
    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError>;
    /// Writes a 32-bit word of target memory.
    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError>;
    /// Drives the probe's nRESET line, returning the probe's reported pin state.
    fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError>;
}

/// Cortex-M run control, written once against [`CoreMemory`] and shared by every probe family.
///
/// The register facts are the Armv6-M / Armv7-M architecture (DDI0419, DDI0403): the Debug Control
/// and Status register and its key, the core-register transfer pair, the vector-catch bit, and the
/// Flash Patch and Breakpoint comparators.
pub mod cortex_m {
    use super::{
        AIRCR, AIRCR_SYSRESETREQ, C_DEBUGEN, C_HALT, C_MASKINTS, C_STEP, CallFrame, CoreMemory,
        DBGKEY, DCRDR, DCRSR, DCRSR_WRITE, DEMCR, DHCSR, FP_COMP0, FP_CTRL, ProbeError, S_HALT,
        S_REGRDY, VC_CORERESET,
    };

    /// Polls DHCSR until `flag` is set (S_HALT after a step, S_REGRDY after a register transfer).
    pub fn poll_dhcsr<M: CoreMemory>(core: &mut M, flag: u32, what: &'static str) -> Result<(), ProbeError> {
        for _ in 0..128 {
            if core.read_word(DHCSR)? & flag != 0 {
                return Ok(());
            }
        }
        Err(ProbeError::Timeout(what))
    }

    /// Halts the processor core.
    pub fn halt<M: CoreMemory>(core: &mut M) -> Result<(), ProbeError> {
        core.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_HALT)
    }

    /// Resumes the processor core from a halt.
    pub fn resume<M: CoreMemory>(core: &mut M) -> Result<(), ProbeError> {
        core.write_word(DHCSR, DBGKEY | C_DEBUGEN)
    }

    /// Whether the core is currently halted.
    pub fn is_halted<M: CoreMemory>(core: &mut M) -> Result<bool, ProbeError> {
        Ok(core.read_word(DHCSR)? & S_HALT != 0)
    }

    /// Polls (bounded) until the core reports halted.
    pub fn wait_halted<M: CoreMemory>(core: &mut M) -> Result<(), ProbeError> {
        poll_dhcsr(core, S_HALT, "core halt")
    }

    /// Executes a single instruction.
    ///
    /// Per the Armv6-M ARM (DDI0419E, C1.5), `C_MASKINTS` must change in a write SEPARATE from the
    /// one clearing `C_HALT`, so this masks while halted, steps, then unmasks while halted again.
    /// The breakpoint unit is disabled across the step: a comparator armed at the current PC would
    /// re-trap the step before it advances, so the core could never leave a breakpointed line.
    pub fn step<M: CoreMemory>(core: &mut M) -> Result<(), ProbeError> {
        core.write_word(FP_CTRL, 0b10)?;
        core.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_HALT | C_MASKINTS)?;
        core.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_STEP | C_MASKINTS)?;
        poll_dhcsr(core, S_HALT, "core halt")?;
        core.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_HALT)?;
        core.write_word(FP_CTRL, 0b11)
    }

    /// Resets the target and lets it run.
    pub fn reset_and_run<M: CoreMemory>(core: &mut M) -> Result<(), ProbeError> {
        let _ = core.write_word(AIRCR, AIRCR_SYSRESETREQ);
        resume(core)
    }

    /// Resets and CATCHES the core halted at the reset vector -- the attach for a target whose
    /// running firmware defeats a plain halt (an armed watchdog resetting straight through one).
    /// Arms under `nRESET` where the line works (the core is held, so nothing races the arm); a
    /// probe with no reset line falls through to racing arm + `SYSRESETREQ` rounds.
    pub fn reset_and_halt<M: CoreMemory>(core: &mut M) -> Result<(), ProbeError> {
        let _ = core.set_reset(true);
        let armed_held = arm_reset_catch(core).is_ok();
        let _ = core.set_reset(false);
        if armed_held && poll_dhcsr(core, S_HALT, "reset catch").is_ok() {
            return disarm_reset_catch(core);
        }
        for _ in 0..8 {
            if arm_reset_catch(core).is_ok() {
                let _ = core.write_word(AIRCR, AIRCR_SYSRESETREQ);
                if poll_dhcsr(core, S_HALT, "reset catch").is_ok() {
                    return disarm_reset_catch(core);
                }
            }
        }
        Err(ProbeError::Timeout("reset catch"))
    }

    /// Reads a core register by its architecture-specific selector.
    pub fn read_core_reg<M: CoreMemory>(core: &mut M, selector: u8) -> Result<u32, ProbeError> {
        core.write_word(DCRSR, u32::from(selector))?;
        poll_dhcsr(core, S_REGRDY, "register transfer")?;
        core.read_word(DCRDR)
    }

    /// Writes a core register by its architecture-specific selector.
    pub fn write_core_reg<M: CoreMemory>(core: &mut M, selector: u8, value: u32) -> Result<(), ProbeError> {
        core.write_word(DCRDR, value)?;
        core.write_word(DCRSR, u32::from(selector) | DCRSR_WRITE)?;
        poll_dhcsr(core, S_REGRDY, "register transfer")
    }

    /// Arms halting debug and the reset vector catch.
    pub fn arm_reset_catch<M: CoreMemory>(core: &mut M) -> Result<(), ProbeError> {
        core.write_word(DHCSR, DBGKEY | C_DEBUGEN)?;
        core.write_word(DEMCR, VC_CORERESET)
    }

    /// Disarms the reset vector catch, so later resets boot freely.
    pub fn disarm_reset_catch<M: CoreMemory>(core: &mut M) -> Result<(), ProbeError> {
        core.write_word(DEMCR, 0)
    }

    /// The FPB comparator word selecting `address`: BP_MATCH (bits 31:30) picks the halfword --
    /// 01 lower, 10 upper -- COMP carries address[28:2], and bit 0 enables.
    fn comparator(address: u32) -> u32 {
        let bp_match: u32 = if address & 0x2 != 0 { 0b10 } else { 0b01 };
        (bp_match << 30) | (address & 0x1fff_fffc) | 1
    }

    /// Sets a hardware breakpoint at `address`.
    pub fn set_breakpoint<M: CoreMemory>(core: &mut M, address: u32) -> Result<(), ProbeError> {
        core.write_word(FP_CTRL, 0b11)?;
        core.write_word(FP_COMP0, comparator(address))
    }

    /// Clears the hardware breakpoint.
    pub fn clear_breakpoint<M: CoreMemory>(core: &mut M) -> Result<(), ProbeError> {
        core.write_word(FP_COMP0, 0)
    }

    /// Replaces every hardware breakpoint with `addresses`, one per comparator.
    pub fn set_breakpoints<M: CoreMemory>(core: &mut M, addresses: &[u32]) -> Result<(), ProbeError> {
        core.write_word(FP_CTRL, 0b11)?;
        for i in 0..4u32 {
            let comp = match addresses.get(i as usize) {
                Some(&address) => comparator(address),
                None => 0,
            };
            core.write_word(FP_COMP0 + i * 4, comp)?;
        }
        Ok(())
    }

    /// Calls a function resident in target memory on the supplied scratch frame.
    ///
    /// Plants `BKPT #0 ; BKPT #0` at the return trap so the callee's `bx lr` halts the core, sets up
    /// an ARM call frame (args in r0-r3, scratch SP, LR at the trap), and runs it. Core state is
    /// disrupted -- reset afterward to run normally.
    pub fn call_target<M: CoreMemory>(
        core: &mut M,
        address: u32,
        args: &[u32],
        frame: &CallFrame,
    ) -> Result<u32, ProbeError> {
        halt(core)?;
        core.write_word(frame.trap, 0xbe00_be00)?;
        for i in 0..4u8 {
            write_core_reg(core, i, args.get(i as usize).copied().unwrap_or(0))?;
        }
        write_core_reg(core, 13, frame.sp)?;
        write_core_reg(core, 14, frame.trap | 1)?;
        write_core_reg(core, 15, address)?;
        write_core_reg(core, 16, 0x0100_0000)?;

        core.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_HALT | C_MASKINTS)?;
        core.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_MASKINTS)?;

        for _ in 0..frame.poll_tries {
            if is_halted(core)? {
                core.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_HALT)?;
                return read_core_reg(core, 0);
            }
        }
        let _ = core.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_HALT);
        Err(ProbeError::Timeout("call_target: the callee did not return"))
    }
}

const AP_CSW: u8 = 0x00;
const AP_TAR: u8 = 0x04;
const AP_DRW: u8 = 0x0c;
const CSW_WORD: u32 = 0x2300_0052;
const CSW_BYTE: u32 = 0x2300_0040;
const CSW_HALF: u32 = 0x2300_0041;
/// The MEM-AP auto-increments `TAR` only within a 1 KB window (ADIv5), so block transfers restart
/// `TAR` at every boundary.
const TAR_WINDOW: u32 = 0x400;

const DP_IDCODE: u8 = 0x0;
const DP_ABORT: u8 = 0x0;
const DP_CTRL_STAT: u8 = 0x4;
const DP_SELECT: u8 = 0x8;
const ABORT_CLEAR_STICKY: u32 = 0x0000_001e;
const CTRL_POWERUP_REQ: u32 = 0x5000_0000;
const CTRL_POWERUP_ACK: u32 = 0xa000_0000;

const DHCSR: u32 = 0xe000_edf0;
const DCRSR: u32 = 0xe000_edf4;
const DCRDR: u32 = 0xe000_edf8;
const DBGKEY: u32 = 0xa05f_0000;
const C_DEBUGEN: u32 = 1 << 0;
const C_HALT: u32 = 1 << 1;
const C_STEP: u32 = 1 << 2;
const C_MASKINTS: u32 = 1 << 3;
const S_REGRDY: u32 = 1 << 16;
const S_HALT: u32 = 1 << 17;
const DCRSR_WRITE: u32 = 1 << 16;
const AIRCR: u32 = 0xe000_ed0c;
const AIRCR_SYSRESETREQ: u32 = 0x05fa_0004;
const DEMCR: u32 = 0xe000_edfc;
const VC_CORERESET: u32 = 1 << 0;
const FP_CTRL: u32 = 0xe000_2000;
const FP_COMP0: u32 = 0xe000_2008;

/// The ARM bridge: turns raw [`DapAccess`] (DP/AP registers) into [`TargetAccess`] (memory and run
/// control) by implementing the ADIv5 MEM-AP and the Cortex-M debug unit on top of it.
///
/// This is where the ARM-specific knowledge lives, written ONCE and shared by every low-level probe
/// family -- CMSIS-DAP today, an FTDI-MPSSE JTAG probe tomorrow. A high-level probe that already
/// speaks memory and run control (ST-Link, J-Link) bypasses this entirely and implements
/// [`TargetAccess`] directly, keeping its native block operations.
///
/// It is a newtype rather than a blanket `impl<T: DapAccess> TargetAccess for T` deliberately: a
/// blanket impl would collide the moment a probe wants to offer both layers.
pub struct ArmDap<D: DapAccess> {
    dap: D,
}

impl<D: DapAccess> ArmDap<D> {
    /// Wraps a raw DP/AP accessor.
    pub fn new(dap: D) -> Self {
        ArmDap { dap }
    }

    /// The underlying accessor, for probe-specific operations outside this trait.
    pub fn inner(&self) -> &D {
        &self.dap
    }

    /// The underlying accessor, mutably.
    pub fn inner_mut(&mut self) -> &mut D {
        &mut self.dap
    }

    /// Unwraps back to the underlying accessor.
    pub fn into_inner(self) -> D {
        self.dap
    }

    /// [`TargetAccess::init_mem`] with a caller-supplied DP `SELECT` value -- an ADIv6 DP addresses
    /// its MEM-AP by base ADDRESS plus the AP register-file offset instead of the ADIv5 `APSEL`
    /// field, e.g. `0x2d00` for an RP2350 core-0 AP at `0x2000` plus the MEM-AP file at `0xd00`.
    pub fn init_mem_select(&mut self, select: u32) -> Result<(), ProbeError> {
        self.dap.write_dp(DP_ABORT, ABORT_CLEAR_STICKY)?;
        self.dap.write_dp(DP_SELECT, select)?;
        self.dap.write_dp(DP_CTRL_STAT, CTRL_POWERUP_REQ)?;
        for _ in 0..128 {
            if self.dap.read_dp(DP_CTRL_STAT)? & CTRL_POWERUP_ACK == CTRL_POWERUP_ACK {
                return self.dap.write_ap(AP_CSW, CSW_WORD);
            }
        }
        Err(ProbeError::Timeout("debug power-up"))
    }

    /// Points `TAR` at `address` and reads `DRW`.
    fn read_drw_at(&mut self, address: u32) -> Result<u32, ProbeError> {
        self.dap.write_ap(AP_TAR, address)?;
        self.dap.read_ap(AP_DRW)
    }

    /// Points `TAR` at `address` and writes `DRW`.
    fn write_drw_at(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        self.dap.write_ap(AP_TAR, address)?;
        self.dap.write_ap(AP_DRW, value)
    }

    /// Runs one sub-word access with the MEM-AP switched to `csw`, restoring the 32-bit CSW
    /// afterward even when the access fails.
    fn with_csw<R>(
        &mut self,
        csw: u32,
        body: impl FnOnce(&mut Self) -> Result<R, ProbeError>,
    ) -> Result<R, ProbeError> {
        self.dap.write_ap(AP_CSW, csw)?;
        let result = body(self);
        self.dap.write_ap(AP_CSW, CSW_WORD)?;
        result
    }

}

impl<D: DapAccess> TargetAccess for ArmDap<D> {
    fn connect(&mut self) -> Result<(), ProbeError> {
        self.dap.connect()
    }

    fn read_idcode(&mut self) -> Result<u32, ProbeError> {
        self.dap.read_dp(DP_IDCODE)
    }

    fn init_mem(&mut self) -> Result<(), ProbeError> {
        self.init_mem_select(0x0000_0000)
    }

    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        self.read_drw_at(address)
    }

    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        self.write_drw_at(address, value)
    }

    fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
        let mut address = address;
        let mut remaining = out;
        while !remaining.is_empty() {
            let to_boundary = ((TAR_WINDOW - (address & (TAR_WINDOW - 1))) / 4) as usize;
            let batch = remaining.len().min(to_boundary);
            self.dap.write_ap(AP_TAR, address)?;
            self.dap.read_ap_block_into(AP_DRW, &mut remaining[..batch])?;
            address += (batch * 4) as u32;
            remaining = &mut remaining[batch..];
        }
        Ok(())
    }

    fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        let mut address = address;
        let mut remaining = words;
        while !remaining.is_empty() {
            let to_boundary = ((TAR_WINDOW - (address & (TAR_WINDOW - 1))) / 4) as usize;
            let count = remaining.len().min(to_boundary);
            self.dap.write_ap(AP_TAR, address)?;
            self.dap.write_ap_block(AP_DRW, &remaining[..count])?;
            address += (count * 4) as u32;
            remaining = &remaining[count..];
        }
        Ok(())
    }

    fn read_byte(&mut self, address: u32) -> Result<u8, ProbeError> {
        let lanes = self.with_csw(CSW_BYTE, |me| me.read_drw_at(address))?;
        Ok((lanes >> (8 * (address & 3))) as u8)
    }

    fn write_byte(&mut self, address: u32, value: u8) -> Result<(), ProbeError> {
        let lanes = u32::from(value) << (8 * (address & 3));
        self.with_csw(CSW_BYTE, |me| me.write_drw_at(address, lanes))
    }

    fn read_halfword(&mut self, address: u32) -> Result<u16, ProbeError> {
        let lanes = self.with_csw(CSW_HALF, |me| me.read_drw_at(address))?;
        Ok((lanes >> (8 * (address & 2))) as u16)
    }

    fn write_halfword(&mut self, address: u32, value: u16) -> Result<(), ProbeError> {
        let lanes = u32::from(value) << (8 * (address & 2));
        self.with_csw(CSW_HALF, |me| me.write_drw_at(address, lanes))
    }

    fn halt(&mut self) -> Result<(), ProbeError> {
        cortex_m::halt(self)
    }

    fn resume(&mut self) -> Result<(), ProbeError> {
        cortex_m::resume(self)
    }

    fn step(&mut self) -> Result<(), ProbeError> {
        cortex_m::step(self)
    }

    fn is_halted(&mut self) -> Result<bool, ProbeError> {
        cortex_m::is_halted(self)
    }

    fn wait_halted(&mut self) -> Result<(), ProbeError> {
        cortex_m::wait_halted(self)
    }

    fn reset_and_run(&mut self) -> Result<(), ProbeError> {
        cortex_m::reset_and_run(self)
    }

    fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
        cortex_m::reset_and_halt(self)
    }

    fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError> {
        self.dap.set_reset(assert)
    }

    fn read_core_reg(&mut self, selector: u8) -> Result<u32, ProbeError> {
        cortex_m::read_core_reg(self, selector)
    }

    fn write_core_reg(&mut self, selector: u8, value: u32) -> Result<(), ProbeError> {
        cortex_m::write_core_reg(self, selector, value)
    }

    fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
        cortex_m::arm_reset_catch(self)
    }

    fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
        cortex_m::disarm_reset_catch(self)
    }

    fn set_breakpoint(&mut self, address: u32) -> Result<(), ProbeError> {
        cortex_m::set_breakpoint(self, address)
    }

    fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
        cortex_m::clear_breakpoint(self)
    }

    fn set_breakpoints(&mut self, addresses: &[u32]) -> Result<(), ProbeError> {
        cortex_m::set_breakpoints(self, addresses)
    }

    fn call_target(&mut self, address: u32, args: &[u32], frame: &CallFrame) -> Result<u32, ProbeError> {
        cortex_m::call_target(self, address, args, frame)
    }
}

/// `ArmDap` reaches memory through the ADIv5 MEM-AP; that is the only thing the shared Cortex-M
/// run control needs from it.
impl<D: DapAccess> CoreMemory for ArmDap<D> {
    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        self.read_drw_at(address)
    }

    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        self.write_drw_at(address, value)
    }

    fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError> {
        self.dap.set_reset(assert)
    }
}
