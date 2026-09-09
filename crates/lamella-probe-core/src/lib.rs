//! The probe-neutral debug seam.

#![forbid(unsafe_code)]

use std::fmt;

pub mod selection;

pub mod coresight;

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
///
/// **`non_exhaustive` because every probe family converts into this enum, so it gains a variant
/// whenever a family needs to report something the existing ones cannot carry.** Eleven crates
/// build against it. The attribute is free at the moment it is added only while nothing matches
/// exhaustively -- after that it breaks every such match -- so it is taken here ahead of the first
/// variant that would need it, rather than discovered to be needed once it is already too late.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
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
    /// More than one PHYSICAL probe matched, so which board was meant is not decidable.
    ///
    /// **Refusing is the whole point: the alternative is a successful operation on somebody else's
    /// target, and that failure is silent.** Carries a name per distinct probe, because the fix is
    /// to name one and the message should not send the user off to look them up.
    ///
    /// Produced by [`selection::choose`]; see that module for the ladder that leads here.
    Ambiguous(Vec<String>),
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
            ProbeError::Ambiguous(names) => write!(
                f,
                "{} probes match and none was named; pass a serial or export {}=<one of>: {}",
                names.len(),
                selection::PROBE_SERIAL_ENV,
                names.join(", ")
            ),
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

    /// Releases the target from reset with `SWCLK` held LOW across the release -- the SAM "reset
    /// extension" -- so the application never runs and never takes the debug pins.
    ///
    /// Returns whether the probe could hold `SWCLK` low across the release. **`Ok(false)` is an
    /// ANSWER AND NOT A FAILURE**: the target was reset, but by the plain pulse this trait's
    /// default performs, and the guarantee the method exists for was not provided.
    ///
    /// # Why this is not [`set_reset`](Self::set_reset) twice
    ///
    /// Releasing nRESET starts the application, and a debugger then has to reach the debug port
    /// before that application reconfigures the pins it is attaching through. On a part whose
    /// firmware repurposes `SWCLK` or `SWDIO` the window is a few instructions wide, so the attach
    /// succeeds intermittently or not at all -- and the failure is indistinguishable from bad
    /// wiring. A SAM part samples `SWCLK` as reset is released: held low, it enters debug instead
    /// of starting the application, and there is no window to lose.
    ///
    /// # The default is the race, deliberately, and it says so in the return
    ///
    /// A probe that cannot drive the two lines together gets nRESET pulsed and `Ok(false)`, which
    /// is what it was doing before this method existed. What changes is that the caller can now
    /// ASK, so a path that needs the guarantee can name the reason it did not get it rather than
    /// working by luck on the probes that happen to idle `SWCLK` low.
    fn reset_extension(&mut self) -> Result<bool, ProbeError> {
        self.set_reset(true)?;
        self.set_reset(false)?;
        Ok(false)
    }

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
    ///
    /// AN IMPLEMENTATION MUST WRITE EVERY SLOT OR RETURN `Err`. A PARTIAL FILL REPORTED AS `Ok`
    /// IS THE ONE FORBIDDEN OUTCOME, and stating it here is load-bearing rather than pedantry: while
    /// this returned a `Vec`, a short read came back as a SHORT VEC and callers caught it by
    /// comparing lengths -- `stlink-flash`'s verify did exactly that, on the grounds that "a short
    /// read is a failed verify, not a shorter one". A caller-provided buffer is always full length,
    /// so that check can no longer fire and the guarantee has nowhere to live except here. An
    /// implementation that filled a prefix and answered `Ok` would hand back a buffer that LOOKS
    /// complete, with the untouched tail carrying whatever the caller left there.
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
    fn reset_extension(&mut self) -> Result<bool, ProbeError> {
        (**self).reset_extension()
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
    ///
    /// EVERY SLOT IS WRITTEN OR AN `Err` IS RETURNED -- a partial fill reported as `Ok` is the
    /// one forbidden outcome, for the reason spelled out on [`DapAccess::read_ap_block_into`]: a
    /// caller who sized the buffer itself cannot detect a short read by checking its LENGTH, so the
    /// guarantee has to be made here.
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

    /// Releases the target from reset with `SWCLK` held LOW across the release -- the SAM "reset
    /// extension" -- so the application never runs and never takes the debug pins.
    ///
    /// Returns whether the probe could hold `SWCLK` low across the release. **`Ok(false)` is an
    /// ANSWER AND NOT A FAILURE**: the target was reset, but by the plain pulse this trait's
    /// default performs, and the guarantee the method exists for was not provided.
    ///
    /// # Why this is not [`set_reset`](Self::set_reset) twice
    ///
    /// Releasing nRESET starts the application, and a debugger then has to reach the debug port
    /// before that application reconfigures the pins it is attaching through. On a part whose
    /// firmware repurposes `SWCLK` or `SWDIO` the window is a few instructions wide, so the attach
    /// succeeds intermittently or not at all -- and the failure is indistinguishable from bad
    /// wiring. A SAM part samples `SWCLK` as reset is released: held low, it enters debug instead
    /// of starting the application, and there is no window to lose.
    ///
    /// # The default is the race, deliberately, and it says so in the return
    ///
    /// A probe that cannot drive the two lines together gets nRESET pulsed and `Ok(false)`, which
    /// is what it was doing before this method existed. What changes is that the caller can now
    /// ASK, so a path that needs the guarantee can name the reason it did not get it rather than
    /// working by luck on the probes that happen to idle `SWCLK` low.
    fn reset_extension(&mut self) -> Result<bool, ProbeError> {
        self.set_reset(true)?;
        self.set_reset(false)?;
        Ok(false)
    }

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
        DBGKEY, DCRDR, DCRSR, DCRSR_WRITE, DEMCR, DEMCR_TRCENA, DHCSR, DWT_CTRL, DWT_CYCCNT,
        DWT_CYCCNTENA, DWT_NOCYCCNT, FP_COMP0, FP_CTRL, FP_CTRL_ENABLE, FP_CTRL_KEY, ProbeError,
        S_HALT, S_REGRDY, VC_CORERESET,
    };

    /// What [`cycle_counter_begin`] turned on, so [`cycle_counter_end`] can put it back.
    pub struct CycleCounter {
        demcr: u32,
        dwt_ctrl: u32,
    }

    /// Turn on the core's cycle counter, returning what to restore -- or `None` when the part
    /// declares it has no counter (`DWT_CTRL.NOCYCCNT`).
    ///
    /// This is how you learn what rate a core is ACTUALLY running at when its clock tree cannot be
    /// read as an equation -- a part running from a ring oscillator has a rate that is a range
    /// rather than a number, and a firmware that never programs its tree inherits whichever state
    /// it booted into. Sample this against a HOST clock and the answer is measured rather than
    /// assumed.
    ///
    /// The half that gets copied wrong is the RESTORE: a counter left enabled is a change to the
    /// firmware under test, made by a tool whose whole claim is that it changes nothing. Hence a
    /// saved value the caller has to hand back rather than a bare enable.
    pub fn cycle_counter_begin<M: CoreMemory>(
        core: &mut M,
    ) -> Result<Option<CycleCounter>, ProbeError> {
        let demcr = core.read_word(DEMCR)?;
        let dwt_ctrl = core.read_word(DWT_CTRL)?;
        if dwt_ctrl & DWT_NOCYCCNT != 0 {
            return Ok(None);
        }
        core.write_word(DEMCR, demcr | DEMCR_TRCENA)?;
        core.write_word(DWT_CTRL, dwt_ctrl | DWT_CYCCNTENA)?;
        Ok(Some(CycleCounter { demcr, dwt_ctrl }))
    }

    /// The cycle counter's current value. It is 32 bits and WRAPS with no flag, so a caller
    /// measuring a rate must subtract with `wrapping_sub` and bound its own interval -- at 150 MHz
    /// the counter turns over about every 28 seconds. It also does not advance while the core is
    /// halted, or while its clock is gated in a sleep state, both of which read as a slow core
    /// rather than as no measurement.
    pub fn cycle_counter_read<M: CoreMemory>(core: &mut M) -> Result<u32, ProbeError> {
        core.read_word(DWT_CYCCNT)
    }

    /// Put `DWT_CTRL` and `DEMCR` back as they were found.
    pub fn cycle_counter_end<M: CoreMemory>(
        core: &mut M,
        saved: CycleCounter,
    ) -> Result<(), ProbeError> {
        core.write_word(DWT_CTRL, saved.dwt_ctrl)?;
        core.write_word(DEMCR, saved.demcr)?;
        Ok(())
    }

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
        let _ = disarm_reset_catch(core);
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

    /// Which revision of the breakpoint unit a part implements, from `FP_CTRL.REV` (bits [31:28]).
    ///
    /// **THE TWO REVISIONS SHARE NO COMPARATOR LAYOUT, so a comparator word cannot be built without
    /// knowing which one is present**, and the register that says so is the same one the enable is
    /// written to. Nothing here can be derived from the part's name: it is read from the part.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum FpbRevision {
        /// `FP_CTRL.REV == 0` -- Armv6-M and Armv7-M. A comparator carries `BP_MATCH` in bits
        /// [31:30] selecting which halfword to break on, and `COMP` in bits [28:2], so the unit
        /// reaches only the low 512 MB and needs the halfword picked for it.
        V1,
        /// `FP_CTRL.REV == 1` -- Armv8-M (Cortex-M23, Cortex-M33 and later). A comparator carries
        /// `BPADDR` in bits [31:1] and there is NO match field: the whole address space, and the
        /// halfword falls out of the address itself. Literal remapping does not exist on Armv8-M.
        ///
        /// Armv8-M ARM (DDI 0553B.y), D1.2 -- `FP_COMP{0..125}` and `FP_CTRL`.
        V2,
    }

    /// Reads `FP_CTRL` and reports which breakpoint-unit revision the part implements.
    ///
    /// **A reserved value is refused rather than guessed.** Planting a comparator in a layout the
    /// part does not implement does not fail loudly -- it arms a breakpoint at some other address,
    /// or at none, and the next person debugs the debugger. A refusal names the problem.
    pub fn fpb_revision<M: CoreMemory>(core: &mut M) -> Result<FpbRevision, ProbeError> {
        fpb_revision_of(core.read_word(FP_CTRL)?)
    }

    /// [`fpb_revision`] from an `FP_CTRL` word already in hand, so a caller that also wants the
    /// comparator count pays for one read rather than two. Both facts live in this register.
    pub fn fpb_revision_of(fp_ctrl: u32) -> Result<FpbRevision, ProbeError> {
        match fp_ctrl >> 28 {
            0 => Ok(FpbRevision::V1),
            1 => Ok(FpbRevision::V2),
            _ => Err(ProbeError::Device("unknown FP_CTRL.REV -- breakpoint unit not recognized")),
        }
    }

    /// How many code comparators the unit implements, from the `FP_CTRL` word already read.
    ///
    /// `NUM_CODE` is split across two fields: bits [14:12] carry its high three bits and bits [7:4]
    /// its low four. Both are needed -- the low field alone saturates at 15, and an Armv8-M unit
    /// may implement up to 126.
    pub fn fpb_num_code(fp_ctrl: u32) -> u32 {
        (((fp_ctrl >> 12) & 0x7) << 4) | ((fp_ctrl >> 4) & 0xf)
    }

    /// The FPB comparator word breaking at `address`, in the layout `revision` implements.
    ///
    /// V1: `BP_MATCH` (bits [31:30]) picks the halfword -- 01 lower, 10 upper -- `COMP` carries
    /// address[28:2], and bit 0 enables.
    ///
    /// V2: `BPADDR` (bits [31:1]) carries address[31:1] and `BE` (bit 0) enables. There is no match
    /// field and no truncation of the address.
    pub fn comparator(revision: FpbRevision, address: u32) -> u32 {
        match revision {
            FpbRevision::V1 => {
                let bp_match: u32 = if address & 0x2 != 0 { 0b10 } else { 0b01 };
                (bp_match << 30) | (address & 0x1fff_fffc) | 1
            }
            FpbRevision::V2 => (address & 0xffff_fffe) | 1,
        }
    }

    /// Sets a hardware breakpoint at `address`, on comparator 0.
    ///
    /// A unit reporting no comparators is refused rather than written to: the writes would be
    /// accepted and the breakpoint would never fire.
    pub fn set_breakpoint<M: CoreMemory>(core: &mut M, address: u32) -> Result<(), ProbeError> {
        let fp_ctrl = core.read_word(FP_CTRL)?;
        let revision = fpb_revision_of(fp_ctrl)?;
        if fpb_num_code(fp_ctrl) == 0 {
            return Err(ProbeError::Device("breakpoint unit reports no comparators"));
        }
        core.write_word(FP_CTRL, FP_CTRL_KEY | FP_CTRL_ENABLE)?;
        core.write_word(FP_COMP0, comparator(revision, address))
    }

    /// Clears the hardware breakpoint.
    ///
    /// Zero to the WHOLE register, which the architecture requires when disabling a comparator
    /// rather than merely clearing the enable bit.
    pub fn clear_breakpoint<M: CoreMemory>(core: &mut M) -> Result<(), ProbeError> {
        core.write_word(FP_COMP0, 0)
    }

    /// Replaces every hardware breakpoint with `addresses`, one per comparator.
    ///
    /// Drives every comparator the unit reports, programming `addresses` and clearing the rest.
    ///
    /// **More addresses than the unit has comparators is an ERROR, not a truncation.** This drove a
    /// fixed four and took `addresses.get(i)`, so a caller asking for more got the first four armed,
    /// no error, and no indication anywhere that the rest had been dropped -- a debug session where
    /// breakpoints five and up simply never fire, which reads as a target fault rather than as a
    /// refusal. The capacity is read from the part, so it is the hardware's cap and not a constant.
    ///
    /// Every comparator up to the capacity is written on every call, which is what makes the clear
    /// half correct: a comparator armed by a previous call and not named by this one has to be
    /// cleared, and nothing here tracks what a previous call armed.
    pub fn set_breakpoints<M: CoreMemory>(core: &mut M, addresses: &[u32]) -> Result<(), ProbeError> {
        let fp_ctrl = core.read_word(FP_CTRL)?;
        let revision = fpb_revision_of(fp_ctrl)?;
        let capacity = fpb_num_code(fp_ctrl) as usize;
        if addresses.len() > capacity {
            return Err(ProbeError::Device(
                "more breakpoints requested than the breakpoint unit has comparators",
            ));
        }
        let enable = if addresses.is_empty() { FP_CTRL_KEY } else { FP_CTRL_KEY | FP_CTRL_ENABLE };
        core.write_word(FP_CTRL, enable)?;
        for i in 0..capacity {
            let comp = addresses.get(i).map_or(0, |&address| comparator(revision, address));
            core.write_word(FP_COMP0 + i as u32 * 4, comp)?;
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
/// `BASE` sits at offset `0xF8` in the MEM-AP register file (ADIv5.2 IHI 0031G, C2.6.1), which is
/// the third word of bank `0xF` -- `CSW`, `TAR` and `DRW` are all in bank 0, so it is the first
/// register in this crate that needs the bank switched at all.
const AP_BASE_IN_BANK: u8 = 0x08;
/// `SELECT.APBANKSEL`, bits[7:4] (IHI 0031G, B2.2.9), set to bank `0xF` and leaving `APSEL` and
/// `DPBANKSEL` alone. On an ADIv6 DP the same bits are part of the AP ADDRESS rather than a bank
/// field, and replacing them with `0xF` lands on the same `+0xF8` -- so one expression serves both.
const AP_BANK_F: u32 = 0x0000_00f0;
/// The MEM-AP auto-increments `TAR` only within a 1 KB window (ADIv5), so block transfers restart
/// `TAR` at every boundary.
const TAR_WINDOW: u32 = 0x400;

const DP_IDCODE: u8 = 0x0;
const DP_ABORT: u8 = 0x0;
const DP_CTRL_STAT: u8 = 0x4;
const DP_SELECT: u8 = 0x8;
/// `SELECT.DPBANKSEL`, bits[3:0] (IHI 0074E, B2.2.11). The three DP banks below hold registers at
/// DP offset `0x0`, which is `DPIDR`'s offset in bank `0x0`.
const DP_BANK_MASK: u32 = 0x0000_000f;
/// `DPIDR1` lives at DP offset `0x0` in this bank (IHI 0074E, B2.2.7).
const DP_BANK_DPIDR1: u32 = 0x1;
/// `BASEPTR0` lives at DP offset `0x0` in this bank (IHI 0074E, B2.2.2).
const DP_BANK_BASEPTR0: u32 = 0x2;
/// `BASEPTR1` lives at DP offset `0x0` in this bank (IHI 0074E, B2.2.2).
const DP_BANK_BASEPTR1: u32 = 0x3;
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
/// `DEMCR.TRCENA` gates the trace/debug block; with it clear the DWT registers below may read as
/// zero and never advance.
const DEMCR_TRCENA: u32 = 1 << 24;
/// The Data Watchpoint and Trace cycle counter. `CTRL` enables it in bit 0 and declares its
/// ABSENCE in bit 25; `CYCCNT` is the counter, the word immediately after `CTRL`. Architectural on
/// every Cortex-M that has a DWT, which is why it lives here and not in a part's own crate.
const DWT_CTRL: u32 = 0xe000_1000;
const DWT_CYCCNT: u32 = 0xe000_1004;
const DWT_CYCCNTENA: u32 = 1 << 0;
const DWT_NOCYCCNT: u32 = 1 << 25;
const FP_CTRL: u32 = 0xe000_2000;
/// `FP_CTRL.KEY` -- a write to `FP_CTRL` is ignored unless this is set in the same write.
const FP_CTRL_KEY: u32 = 1 << 1;
/// `FP_CTRL.ENABLE` -- the breakpoint unit's global enable.
const FP_CTRL_ENABLE: u32 = 1 << 0;
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
    /// The DP `SELECT` value [`init_mem_select`](Self::init_mem_select) last wrote.
    ///
    /// Held so a register outside the MEM-AP's bank 0 can be reached and the selection put back --
    /// [`rom_table_base`](Self::rom_table_base) is the only such register today. **Remembering it
    /// is what lets that work on an ADIv6 DP too**, where the selection is an AP ADDRESS rather
    /// than an index and no caller of this type would have it to hand.
    select: u32,
}

impl<D: DapAccess> ArmDap<D> {
    /// Wraps a raw DP/AP accessor.
    pub fn new(dap: D) -> Self {
        ArmDap { dap, select: 0 }
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
        self.select = select;
        self.dap.write_dp(DP_CTRL_STAT, CTRL_POWERUP_REQ)?;
        for _ in 0..128 {
            if self.dap.read_dp(DP_CTRL_STAT)? & CTRL_POWERUP_ACK == CTRL_POWERUP_ACK {
                return self.dap.write_ap(AP_CSW, CSW_WORD);
            }
        }
        Err(ProbeError::Timeout("debug power-up"))
    }

    /// Reads this MEM-AP's `BASE` register -- where the AP says its debug components are described.
    ///
    /// **This is the one step of CoreSight discovery that is not a memory read**, and the reason the
    /// walk lives in [`coresight`] while this lives here: `BASE` is an AP register, so only a probe
    /// that hands us the DP/AP layer can read it. A high-level probe reaches the same ROM table
    /// through [`coresight::walk`] once it has the address by some other route.
    ///
    /// ADIv5.2 (IHI 0031G), C2.6.1 puts `BASE` at offset `0xF8` in the AP register file, which is
    /// bank `0xF`; `CSW`, `TAR` and `DRW` are all in bank 0, so this switches `SELECT.APBANKSEL`
    /// and puts it back. **Restoring it is not tidiness** -- every memory access this type makes
    /// afterwards addresses `TAR` and `DRW` by their offsets within the selected bank, so leaving
    /// bank `0xF` selected would silently redirect them.
    ///
    /// WARNING: **only the 32-bit `BASE` is read.** IHI 0031G, C2.6.1 places the upper word at
    /// offset `0xF0` when the Large Physical Address extension is implemented, and states in the
    /// same section that "Armv7-R, Armv6-M, Armv7-M, and Armv8-M processors can access only a
    /// 32-bit physical address space" -- so on every part this crate drives there is no upper word
    /// to read. A 64-bit target reaches this and gets the low half, which is the wrong answer; the
    /// call to add is `0xF0`, and it wants a 64-bit address to return it in.
    ///
    /// Call it after [`init_mem`](TargetAccess::init_mem) or
    /// [`init_mem_select`](Self::init_mem_select): the AP must be powered and selected first, and
    /// this restores the selection those made rather than assuming AP 0.
    pub fn rom_table_base(&mut self) -> Result<coresight::DebugBase, ProbeError> {
        let banked = (self.select & !AP_BANK_F) | AP_BANK_F;
        self.dap.write_dp(DP_SELECT, banked)?;
        let word = self.dap.read_ap(AP_BASE_IN_BANK);
        let restored = self.dap.write_dp(DP_SELECT, self.select);
        let word = word?;
        restored?;
        Ok(coresight::decode_base(word))
    }

    /// Asks the DEBUG PORT where its debug components are described, which an ADIv6 target can
    /// answer before any AP has been selected.
    ///
    /// **This is the read that breaks the ADIv6 chicken-and-egg.** An ADIv5 MEM-AP's `BASE` is
    /// reachable only once an AP is selected, and an ADIv6 DP selects an AP by ADDRESS rather than
    /// by index -- so a caller with no prior knowledge of the part cannot select one, and asking
    /// for ADIv5 AP 0 on such a target selects an AP that does not exist. Every register then reads
    /// as zero, which is [`coresight::DebugBase::Zero`] and is what the ADIv5 path reports.
    /// `BASEPTR` is a DP register, so it needs nothing selected first.
    ///
    /// Four reads, in three banks (IHI 0074E, B2.2.2, B2.2.6 and B2.2.7): `DPIDR` at `DPBANKSEL`
    /// `0x0`, `DPIDR1` at `0x1`, `BASEPTR0` at `0x2` and `BASEPTR1` at `0x3`, all at DP register
    /// offset `0x0`. **The version gate is not a courtesy**: offset `0x0` reads `DPIDR` on a
    /// pre-DPv3 port whatever the bank field holds, so a port that is not a DPv3 would answer its
    /// own id and it would decode as an address.
    ///
    /// The DP `SELECT` value is put back, for the reason
    /// [`rom_table_base`](Self::rom_table_base) puts it back: everything else this type does
    /// addresses AP registers through the selection, so leaving another bank selected would
    /// silently redirect them.
    ///
    /// # Errors
    /// When a DP register access fails.
    pub fn debug_base_pointer(&mut self) -> Result<coresight::DebugBasePointer, ProbeError> {
        let dpidr = self.dap.read_dp(DP_IDCODE)?;
        if (dpidr >> 12) as u8 & 0xf != coresight::DPV3 {
            return Ok(coresight::decode_base_pointer(dpidr, 0, 0, 0));
        }
        let selected = self.select & !DP_BANK_MASK;
        let banked = |dap: &mut D| -> Result<(u32, u32, u32), ProbeError> {
            dap.write_dp(DP_SELECT, selected | DP_BANK_DPIDR1)?;
            let dpidr1 = dap.read_dp(DP_IDCODE)?;
            dap.write_dp(DP_SELECT, selected | DP_BANK_BASEPTR0)?;
            let low = dap.read_dp(DP_IDCODE)?;
            dap.write_dp(DP_SELECT, selected | DP_BANK_BASEPTR1)?;
            let high = dap.read_dp(DP_IDCODE)?;
            Ok((dpidr1, low, high))
        };
        let read = banked(&mut self.dap);
        let restored = self.dap.write_dp(DP_SELECT, self.select);
        let (dpidr1, low, high) = read?;
        restored?;
        Ok(coresight::decode_base_pointer(dpidr, dpidr1, low, high))
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
    /// Brings the wire up AND reads `DPIDR`, because ADIv5 does not let anything else go first.
    ///
    /// After the JTAG-to-SWD switch and line reset the debug port answers exactly one transaction:
    /// a read of `DPIDR`. Anything else is not acknowledged. So a `connect` that stops at the
    /// switch leaves a link that LOOKS up and refuses the next access -- and the refusal is
    /// `NoAck`, which is also what a board with no target wired to it returns. **A host-side
    /// protocol slip, reported as an absent board.**
    ///
    /// It belongs here rather than at the call sites because the requirement is ADIv5's, and this
    /// is the ADIv5 bridge; the transport below has no DP registers to know about. A requirement
    /// every caller happens to satisfy is not enforced anywhere, and the caller that omits it is
    /// invisible -- the others working is the same coincidence repeated, not evidence.
    ///
    /// The id itself is discarded here. [`TargetAccess::read_idcode`] is how a caller that wants
    /// the value asks for it, and asking twice costs one transaction and is not an error.
    fn connect(&mut self) -> Result<(), ProbeError> {
        self.dap.connect()?;
        self.dap.read_dp(DP_IDCODE)?;
        Ok(())
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

    fn reset_extension(&mut self) -> Result<bool, ProbeError> {
        self.dap.reset_extension()
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

#[cfg(test)]
mod tests {
    use super::{ArmDap, DP_IDCODE, DapAccess, ProbeError, TargetAccess};

    /// What a probe was asked to do to its reset line, in order.
    #[derive(Debug, PartialEq, Eq, Clone, Copy)]
    enum Pin {
        Assert,
        Release,
        Extension,
    }

    /// A [`DapAccess`] that records reset activity and nothing else. `can_hold_swclk` selects which
    /// of the two probe shapes it models: one that overrides `reset_extension` (a CMSIS-DAP probe)
    /// and one that does not (whatever the trait's default covers).
    struct FakeDap {
        can_hold_swclk: bool,
        log: Vec<Pin>,
    }

    impl FakeDap {
        fn new(can_hold_swclk: bool) -> FakeDap {
            FakeDap { can_hold_swclk, log: Vec::new() }
        }
    }

    impl DapAccess for FakeDap {
        fn connect(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_dp(&mut self, _address: u8) -> Result<u32, ProbeError> {
            Ok(0)
        }
        fn write_dp(&mut self, _address: u8, _value: u32) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_ap(&mut self, _address: u8) -> Result<u32, ProbeError> {
            Ok(0)
        }
        fn write_ap(&mut self, _address: u8, _value: u32) -> Result<(), ProbeError> {
            Ok(())
        }
        fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError> {
            self.log.push(if assert { Pin::Assert } else { Pin::Release });
            Ok(0)
        }
        fn reset_extension(&mut self) -> Result<bool, ProbeError> {
            if !self.can_hold_swclk {
                self.set_reset(true)?;
                self.set_reset(false)?;
                return Ok(false);
            }
            self.log.push(Pin::Extension);
            Ok(true)
        }
    }

    /// ADIv5 answers exactly one transaction after the switch, and it must be the `DPIDR` read.
    ///
    /// The defect this pins was silent for as long as it existed: every bench tool called
    /// `read_idcode` between `connect` and `init_mem` out of habit, so only the two probe ROUTES --
    /// which did not -- were wrong, and they failed with `NoAck`, the same answer a board with
    /// nothing wired to it gives. Asserting the ORDER is the only way to catch it, because both
    /// spellings of the sequence return `Ok` against a fake that does not care.
    #[test]
    fn connect_reads_dpidr_first_because_the_port_answers_nothing_else() {
        #[derive(PartialEq, Debug)]
        enum Op {
            Connect,
            ReadDp(u8),
            WriteDp(u8),
        }
        struct Recorder(Vec<Op>);
        impl DapAccess for Recorder {
            fn connect(&mut self) -> Result<(), ProbeError> {
                self.0.push(Op::Connect);
                Ok(())
            }
            fn read_dp(&mut self, address: u8) -> Result<u32, ProbeError> {
                self.0.push(Op::ReadDp(address));
                Ok(u32::MAX)
            }
            fn write_dp(&mut self, address: u8, _value: u32) -> Result<(), ProbeError> {
                self.0.push(Op::WriteDp(address));
                Ok(())
            }
            fn read_ap(&mut self, _address: u8) -> Result<u32, ProbeError> {
                Ok(0)
            }
            fn write_ap(&mut self, _address: u8, _value: u32) -> Result<(), ProbeError> {
                Ok(())
            }
            fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
                Ok(0)
            }
        }

        let mut dap = ArmDap::new(Recorder(Vec::new()));
        dap.connect().expect("connect");
        assert_eq!(
            dap.inner().0,
            vec![Op::Connect, Op::ReadDp(DP_IDCODE)],
            "the switch must be followed by the DPIDR read and nothing else"
        );

        dap.init_mem().expect("init_mem");
        let ops = &dap.inner().0;
        let first_write = ops.iter().position(|op| matches!(op, Op::WriteDp(_)));
        let first_read = ops.iter().position(|op| op == &Op::ReadDp(DP_IDCODE));
        assert!(
            first_read < first_write,
            "DPIDR must precede the first DP write; got {ops:?}"
        );
    }

    /// A probe that cannot drive the pair still resets the target, and SAYS it could not hold
    /// SWCLK. Both halves matter: the pulse is what keeps a probe with no pin mask working, and the
    /// `false` is the only way a caller can tell that the guarantee was not provided.
    #[test]
    fn the_default_reset_extension_pulses_nreset_and_reports_that_it_held_nothing() {
        struct Bare(Vec<Pin>);
        impl DapAccess for Bare {
            fn connect(&mut self) -> Result<(), ProbeError> {
                Ok(())
            }
            fn read_dp(&mut self, _address: u8) -> Result<u32, ProbeError> {
                Ok(0)
            }
            fn write_dp(&mut self, _address: u8, _value: u32) -> Result<(), ProbeError> {
                Ok(())
            }
            fn read_ap(&mut self, _address: u8) -> Result<u32, ProbeError> {
                Ok(0)
            }
            fn write_ap(&mut self, _address: u8, _value: u32) -> Result<(), ProbeError> {
                Ok(())
            }
            fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError> {
                self.0.push(if assert { Pin::Assert } else { Pin::Release });
                Ok(0)
            }
        }

        let mut bare = Bare(Vec::new());
        assert!(
            !bare.reset_extension().unwrap(),
            "a probe with no pin mask must answer false rather than claim the guarantee"
        );
        assert_eq!(
            bare.0,
            vec![Pin::Assert, Pin::Release],
            "and it must still reset the target, which is what it did before this method existed"
        );
    }

    /// THE ONE THAT CATCHES A MISSING FORWARD, and it is the whole reason the bridge writes the
    /// method out. `ArmDap` compiles perfectly well without overriding `reset_extension` -- it
    /// would take the trait default, pulse nRESET, and report `false` for a probe that had just
    /// told it `true`. That is a silent downgrade of a real capability, reached through a bridge
    /// that looks complete because it compiles. Delete the forward in `impl TargetAccess for
    /// ArmDap` and this test fails; nothing else in the tree does.
    #[test]
    fn armdap_forwards_reset_extension_instead_of_taking_the_default() {
        let mut arm = ArmDap::new(FakeDap::new(true));
        assert!(
            TargetAccess::reset_extension(&mut arm).unwrap(),
            "the bridge must report the PROBE's answer, not the default's"
        );

        let mut arm = ArmDap::new(FakeDap::new(false));
        assert!(
            !TargetAccess::reset_extension(&mut arm).unwrap(),
            "and it must report a probe that cannot hold SWCLK as such"
        );
    }

    /// The same downgrade, reached through a BORROW rather than through the bridge. A caller
    /// holding `&mut D` -- a discovery session handing out its debug port -- must get the probe's
    /// own sequence, not the default's.
    #[test]
    fn a_borrowed_probe_forwards_reset_extension() {
        fn through_the_seam<D: DapAccess>(mut probe: D) -> bool {
            probe.reset_extension().unwrap()
        }

        let mut probe = FakeDap::new(true);
        assert!(
            through_the_seam(&mut probe),
            "a borrowed probe must not silently fall back to the pulse"
        );
        assert_eq!(probe.log, vec![Pin::Extension]);
    }
}

#[cfg(test)]
mod fpb_tests {
    use super::cortex_m::{FpbRevision, comparator, fpb_num_code};

    /// The two revisions must not agree on a comparator word, because that is the whole reason the
    /// revision has to be read. Asserted on an address that exercises every difference at once:
    /// bit 1 set (V1 needs the upper-halfword selector), and bits above 28 set (V1 cannot carry
    /// them at all).
    #[test]
    fn the_two_revisions_encode_the_same_address_differently() {
        let address = 0x8000_0002;
        let v1 = comparator(FpbRevision::V1, address);
        let v2 = comparator(FpbRevision::V2, address);
        assert_ne!(v1, v2);
        assert_eq!(v1, (0b10u32 << 30) | (address & 0x1fff_fffc) | 1);
        assert_eq!(v2, (address & 0xffff_fffe) | 1);
    }

    /// V1's `COMP` field is bits [28:2], so it cannot carry an address at or above 512 MB; V2's
    /// `BPADDR` is bits [31:1] and can. An execute-in-place alias high in the map is the realistic
    /// case that separates them.
    ///
    #[test]
    fn only_revision_2_keeps_an_address_above_the_v1_field() {
        let address = 0x9000_2468;
        assert_eq!(comparator(FpbRevision::V2, address) & 0xffff_fffe, address);
        assert_ne!(comparator(FpbRevision::V1, address) & 0x1fff_fffc, address);
    }

    /// Every comparator word enables its comparator, on both revisions -- V1's ENABLE and V2's BE
    /// are the same bit, which is the one thing the two layouts do share.
    #[test]
    fn both_revisions_set_the_enable_bit() {
        for revision in [FpbRevision::V1, FpbRevision::V2] {
            assert_eq!(comparator(revision, 0x0000_1000) & 1, 1);
        }
    }

    /// NUM_CODE is split across bits [14:12] and [7:4]. Reading the low nibble alone truncates at
    /// 15, which is why this is a function rather than a shift at each call site.
    #[test]
    fn num_code_is_read_from_both_of_its_fields() {
        assert_eq!(fpb_num_code(0x0000_0060), 6);
        assert_eq!(fpb_num_code(0x0000_7080), 0x78);
        assert_eq!(fpb_num_code(0x1000_0080), 8);
    }
}
