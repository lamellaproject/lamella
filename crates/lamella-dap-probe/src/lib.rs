//! A [`DebugBackend`] over a debug probe: halt / step / registers / memory / hardware breakpoints
//! on a target the host reaches through SWD, driven from the same DAP adapter that drives the
//! interpreter.

use core::cell::RefCell;

use lamella_debug_backend::{DebugBackend, Disassembled, Frame, Register, Scope, Stop, Variable};
use lamella_probe_core::{ProbeError, TargetAccess};

/// The DCRSR selector of the program counter. Selectors 0-15 are `r0`-`r15`, 16 is `xPSR`.
const PC: u8 = 15;

/// The Flash Patch and Breakpoint unit's control register: bit 0 ENABLE, bit 1 KEY, bits [7:4]
/// NUM_CODE (how many code comparators the unit implements).
const FP_CTRL: u32 = 0xe000_2000;

/// The core registers reported to a debugger, in DCRSR selector order.
const REGISTERS: [(&str, u8); 17] = [
    ("r0", 0),
    ("r1", 1),
    ("r2", 2),
    ("r3", 3),
    ("r4", 4),
    ("r5", 5),
    ("r6", 6),
    ("r7", 7),
    ("r8", 8),
    ("r9", 9),
    ("r10", 10),
    ("r11", 11),
    ("r12", 12),
    ("sp", 13),
    ("lr", 14),
    ("pc", 15),
    ("xpsr", 16),
];

/// How [`ProbeBackend::launch`] brings the target under control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// Reset the target and catch it halted at the reset vector, so the session sees the program
    /// from its first instruction. DAP's `launch`.
    Reset,
    /// Halt whatever the target is already running, leaving its state intact. DAP's `attach` --
    /// and the only option when the interesting state is the state a reset would destroy.
    Attach,
}

/// A [`DebugBackend`] over a probe-reachable target.
///
/// The target sits behind a [`RefCell`] because the seam's inspection methods take `&self` while
/// every probe operation needs `&mut` -- a read of target memory is a bus transaction, not a field
/// access. Nothing here re-enters, so the borrow is uncontended.
pub struct ProbeBackend<T: TargetAccess> {
    target: RefCell<T>,
    start: Start,
    /// The addresses the adapter last asked for, in the order it asked. Kept whether or not they
    /// have been armed, so a `setBreakpoints` before `launch` is not lost.
    breakpoints: Vec<u32>,
    /// The breakpoint unit's comparator count, read from the target at launch.
    comparators: Option<usize>,
    /// The program counter as of the last stop. Cached so the `&self` methods can answer without a
    /// bus transaction, and because a running target has no meaningful PC to report.
    pc: u32,
    /// Whether the target was left running by a `resume`, so `poll` knows to look.
    running: bool,
}

impl<T: TargetAccess> ProbeBackend<T> {
    /// Wraps a connected-or-connectable probe target. Nothing touches the wire until
    /// [`DebugBackend::launch`].
    pub fn new(target: T, start: Start) -> Self {
        ProbeBackend {
            target: RefCell::new(target),
            start,
            breakpoints: Vec::new(),
            comparators: None,
            pc: 0,
            running: false,
        }
    }

    /// The wrapped target, for a caller that needs the probe back (to flash, or to close it).
    pub fn into_target(self) -> T {
        self.target.into_inner()
    }

    /// The program counter as of the last stop.
    pub fn pc(&self) -> u32 {
        self.pc
    }

    /// Re-reads the program counter after a stop. A failure leaves the previous value rather than
    /// reporting zero, because a zero PC is a real address and would read as a stop at the reset
    /// vector.
    fn sync_pc(&mut self) {
        if let Ok(value) = self.target.borrow_mut().read_core_reg(PC) {
            self.pc = value;
        }
    }

    /// Whether the stop lands on an address the adapter armed.
    ///
    /// The classification is by ADDRESS rather than by asking the target why it halted, and the
    /// difference is observable: a stop the debug unit raised for another reason at an address that
    /// happens to carry a breakpoint reports as a breakpoint hit. Naming it here rather than
    /// leaving it implicit -- distinguishing them needs the debug fault status register, which is a
    /// separate read this does not yet make.
    fn classify(&self) -> Stop {
        if self.breakpoints.contains(&self.pc) {
            Stop::Breakpoint
        } else {
            Stop::Step
        }
    }

    /// Pushes the current breakpoint set to the unit.
    fn arm(&mut self) -> Result<(), ProbeError> {
        let armed: Vec<u32> = match self.comparators {
            Some(limit) => self.breakpoints.iter().copied().take(limit).collect(),
            None => self.breakpoints.clone(),
        };
        self.target.borrow_mut().set_breakpoints(&armed)
    }

    /// Reads the breakpoint unit's comparator count from `FP_CTRL`, or `None` if the register does
    /// not read back a plausible count -- in which case no limit is claimed, since claiming a wrong
    /// one is worse than claiming none.
    fn read_comparators(&mut self) -> Option<usize> {
        let ctrl = self.target.borrow_mut().read_word(FP_CTRL).ok()?;
        let count = ((ctrl >> 4) & 0xf) as usize;
        if count == 0 {
            None
        } else {
            Some(count)
        }
    }
}

impl<T: TargetAccess> DebugBackend for ProbeBackend<T> {
    fn launch(&mut self) -> bool {
        {
            let mut target = self.target.borrow_mut();
            if target.connect().is_err() || target.init_mem().is_err() {
                return false;
            }
            let started = match self.start {
                Start::Reset => target.reset_and_halt(),
                Start::Attach => target.halt().and_then(|()| target.wait_halted()),
            };
            if started.is_err() {
                return false;
            }
        }
        self.comparators = self.read_comparators();
        self.sync_pc();
        self.running = false;
        self.arm().is_ok()
    }

    fn resume(&mut self) -> Stop {
        if let Err(error) = self.arm() {
            return Stop::Fault(format!("could not arm breakpoints: {error}"));
        }
        match self.target.borrow_mut().resume() {
            Ok(()) => {
                self.running = true;
                Stop::Running
            }
            Err(error) => Stop::Fault(format!("could not resume the target: {error}")),
        }
    }

    fn step(&mut self) -> Stop {
        let stepped = self.target.borrow_mut().step();
        if let Err(error) = stepped {
            return Stop::Fault(format!("could not step the target: {error}"));
        }
        self.running = false;
        self.sync_pc();
        self.classify()
    }

    fn poll(&mut self) -> Stop {
        if !self.running {
            return self.classify();
        }
        let halted = self.target.borrow_mut().is_halted();
        match halted {
            Ok(false) => Stop::Running,
            Ok(true) => {
                self.running = false;
                self.sync_pc();
                self.classify()
            }
            Err(error) => {
                self.running = false;
                Stop::Fault(format!("lost contact with the target: {error}"))
            }
        }
    }

    fn pause(&mut self) -> bool {
        let halted = {
            let mut target = self.target.borrow_mut();
            target.halt().and_then(|()| target.wait_halted())
        };
        if halted.is_err() {
            return false;
        }
        self.running = false;
        self.sync_pc();
        true
    }

    /// One, always: recovering a caller needs an unwinder, and this backend has no debug info to
    /// unwind with. The adapter degrades `next` / `stepOut` to a single step, which is the correct
    /// behaviour for an instruction-level session and is why the seam allows the answer.
    fn depth(&self) -> usize {
        1
    }

    fn set_breakpoints(&mut self, addresses: &[u64]) {
        self.breakpoints = addresses
            .iter()
            .filter_map(|&address| u32::try_from(address).ok())
            .collect();
        let _ = self.arm();
    }

    fn max_breakpoints(&self) -> Option<usize> {
        self.comparators
    }

    fn stack(&self) -> Vec<Frame> {
        vec![Frame {
            address: u64::from(self.pc),
            name: format!("0x{:08x}", self.pc),
            line: 1,
        }]
    }

    /// Empty: a local's home is in debug info the AOT tier does not emit. The register file and
    /// target memory are what this backend can show, and both are served in full.
    fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
        Vec::new()
    }

    fn read_memory(&self, address: u64, len: usize) -> Vec<u8> {
        let Ok(base) = u32::try_from(address) else {
            return Vec::new();
        };
        if len == 0 {
            return Vec::new();
        }
        let start = base & !3;
        let skip = (base - start) as usize;
        let words = (skip + len).div_ceil(4);
        let mut buffer = vec![0u32; words];
        if self
            .target
            .borrow_mut()
            .read_words_into(start, &mut buffer)
            .is_err()
        {
            return Vec::new();
        }
        let mut bytes = Vec::with_capacity(words * 4);
        for word in buffer {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        bytes.drain(..skip);
        bytes.truncate(len);
        bytes
    }

    fn read_registers(&self) -> Vec<Register> {
        let mut target = self.target.borrow_mut();
        REGISTERS
            .iter()
            .filter_map(|&(name, selector)| {
                target.read_core_reg(selector).ok().map(|value| Register {
                    name: name.to_string(),
                    value: u64::from(value),
                })
            })
            .collect()
    }

    /// Empty: rendering an instruction needs a Thumb decoder, and there is not one to call. An
    /// empty answer leaves the client showing no disassembly, which is what it should show rather
    /// than something invented.
    fn disassemble(&self, _address: u64, _offset: i64, _count: usize) -> Vec<Disassembled> {
        Vec::new()
    }

    /// None: a probe carries no program output. A target's `Console.Write` reaches the host over a
    /// serial carrier or a Lamella Link, which is a different connection from this one -- so a
    /// backend that invented an output stream here would be reporting silence as fact.
    fn take_output(&mut self) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_probe_core::CallFrame;
    use std::collections::BTreeMap;

    /// A target that answers from a memory map and a register file, so the backend's logic is
    /// exercised without a probe or a board.
    #[derive(Default)]
    struct FakeTarget {
        memory: BTreeMap<u32, u32>,
        registers: BTreeMap<u8, u32>,
        halted: bool,
        armed: Vec<u32>,
        connected: bool,
        /// Steps remaining before a `resume`d target reports halted, so a test can observe the
        /// free-running state the seam's `Running` exists for.
        run_for: u32,
        /// Where the target lands when it stops running.
        lands_at: u32,
        fail_connect: bool,
    }

    impl FakeTarget {
        fn with_pc(pc: u32) -> FakeTarget {
            let mut target = FakeTarget::default();
            target.registers.insert(PC, pc);
            target.memory.insert(FP_CTRL, 0x0000_0043);
            target
        }
    }

    impl TargetAccess for FakeTarget {
        fn connect(&mut self) -> Result<(), ProbeError> {
            if self.fail_connect {
                return Err(ProbeError::Device("no target"));
            }
            self.connected = true;
            Ok(())
        }
        fn read_idcode(&mut self) -> Result<u32, ProbeError> {
            Ok(0x2ba0_1477)
        }
        fn init_mem(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
            Ok(self.memory.get(&address).copied().unwrap_or(0))
        }
        fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
            self.memory.insert(address, value);
            Ok(())
        }
        fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
            for (index, slot) in out.iter_mut().enumerate() {
                *slot = self.read_word(address + (index as u32) * 4)?;
            }
            Ok(())
        }
        fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
            for (index, &word) in words.iter().enumerate() {
                self.write_word(address + (index as u32) * 4, word)?;
            }
            Ok(())
        }
        fn read_byte(&mut self, address: u32) -> Result<u8, ProbeError> {
            let word = self.read_word(address & !3)?;
            Ok(word.to_le_bytes()[(address & 3) as usize])
        }
        fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_halfword(&mut self, address: u32) -> Result<u16, ProbeError> {
            let word = self.read_word(address & !3)?;
            Ok((word >> ((address & 2) * 8)) as u16)
        }
        fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
            Ok(())
        }
        fn halt(&mut self) -> Result<(), ProbeError> {
            self.halted = true;
            self.run_for = 0;
            Ok(())
        }
        fn resume(&mut self) -> Result<(), ProbeError> {
            self.halted = false;
            Ok(())
        }
        fn step(&mut self) -> Result<(), ProbeError> {
            let pc = self.registers.get(&PC).copied().unwrap_or(0);
            self.registers.insert(PC, pc + 2);
            self.halted = true;
            Ok(())
        }
        fn is_halted(&mut self) -> Result<bool, ProbeError> {
            if self.halted {
                return Ok(true);
            }
            if self.run_for == 0 {
                self.halted = true;
                self.registers.insert(PC, self.lands_at);
                return Ok(true);
            }
            self.run_for -= 1;
            Ok(false)
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            self.halted = true;
            Ok(())
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            self.halted = true;
            Ok(())
        }
        fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
            Ok(0)
        }
        fn read_core_reg(&mut self, selector: u8) -> Result<u32, ProbeError> {
            Ok(self.registers.get(&selector).copied().unwrap_or(0))
        }
        fn write_core_reg(&mut self, selector: u8, value: u32) -> Result<(), ProbeError> {
            self.registers.insert(selector, value);
            Ok(())
        }
        fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn set_breakpoint(&mut self, address: u32) -> Result<(), ProbeError> {
            self.armed = vec![address];
            Ok(())
        }
        fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
            self.armed.clear();
            Ok(())
        }
        fn set_breakpoints(&mut self, addresses: &[u32]) -> Result<(), ProbeError> {
            self.armed = addresses.to_vec();
            Ok(())
        }
        fn call_target(
            &mut self,
            _address: u32,
            _args: &[u32],
            _frame: &CallFrame,
        ) -> Result<u32, ProbeError> {
            Ok(0)
        }
    }

    #[test]
    fn launch_halts_the_target_and_reports_where_it_stopped() {
        let mut backend = ProbeBackend::new(FakeTarget::with_pc(0x0800_0100), Start::Reset);
        assert!(backend.launch());
        assert_eq!(backend.pc(), 0x0800_0100);
        let stack = backend.stack();
        assert_eq!(stack.len(), 1, "no unwinder: exactly one frame");
        assert_eq!(stack[0].address, 0x0800_0100);
    }

    #[test]
    fn a_target_that_cannot_be_reached_fails_launch_rather_than_reporting_a_stop() {
        let mut target = FakeTarget::with_pc(0);
        target.fail_connect = true;
        let mut backend = ProbeBackend::new(target, Start::Reset);
        assert!(!backend.launch(), "a probe that cannot connect must not report a launched session");
    }

    #[test]
    fn the_breakpoint_limit_comes_from_the_target_not_a_constant() {
        let mut backend = ProbeBackend::new(FakeTarget::with_pc(0x1000), Start::Reset);
        assert_eq!(backend.max_breakpoints(), None, "nothing is claimed before the target is read");
        assert!(backend.launch());
        assert_eq!(
            backend.max_breakpoints(),
            Some(4),
            "FP_CTRL NUM_CODE = 4 must be reported, so the adapter can grey a fifth breakpoint"
        );
    }

    #[test]
    fn a_unit_that_reports_no_comparators_claims_no_limit() {
        let mut target = FakeTarget::with_pc(0x1000);
        target.memory.insert(FP_CTRL, 0x0000_0003);
        let mut backend = ProbeBackend::new(target, Start::Reset);
        assert!(backend.launch());
        assert_eq!(
            backend.max_breakpoints(),
            None,
            "an implausible count must not be reported as a real limit"
        );
    }

    #[test]
    fn breakpoints_set_before_launch_are_armed_by_it() {
        let mut backend = ProbeBackend::new(FakeTarget::with_pc(0x2000), Start::Reset);
        backend.set_breakpoints(&[0x2100, 0x2200]);
        assert!(backend.launch());
        let armed = backend.into_target().armed;
        assert_eq!(
            armed,
            vec![0x2100, 0x2200],
            "DAP sends setBreakpoints before configurationDone, so the pre-launch set is the normal case"
        );
    }

    #[test]
    fn breakpoints_past_the_comparator_count_are_not_sent_to_the_unit() {
        let mut backend = ProbeBackend::new(FakeTarget::with_pc(0x2000), Start::Reset);
        assert!(backend.launch());
        backend.set_breakpoints(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(backend.max_breakpoints(), Some(4));
        let armed = backend.into_target().armed;
        assert_eq!(armed.len(), 4, "only as many as the unit implements");
        assert_eq!(armed, vec![1, 2, 3, 4], "and they are the first four, in the order asked");
    }

    #[test]
    fn without_a_known_limit_every_breakpoint_is_armed() {
        let mut target = FakeTarget::with_pc(0x2000);
        target.memory.insert(FP_CTRL, 0x0000_0003);
        let mut backend = ProbeBackend::new(target, Start::Reset);
        assert!(backend.launch());
        assert_eq!(backend.max_breakpoints(), None);
        backend.set_breakpoints(&[1, 2, 3, 4, 5, 6]);
        assert_eq!(backend.into_target().armed.len(), 6);
    }

    #[test]
    fn an_address_too_wide_for_the_target_is_dropped_not_truncated() {
        let mut backend = ProbeBackend::new(FakeTarget::with_pc(0x2000), Start::Reset);
        assert!(backend.launch());
        backend.set_breakpoints(&[0x1_0000_2100, 0x2200]);
        let armed = backend.into_target().armed;
        assert_eq!(
            armed,
            vec![0x2200],
            "truncating would arm 0x2100 -- a real address nobody asked for"
        );
    }

    #[test]
    fn resume_reports_running_and_poll_reports_the_eventual_stop() {
        let mut target = FakeTarget::with_pc(0x3000);
        target.run_for = 2;
        target.lands_at = 0x3400;
        let mut backend = ProbeBackend::new(target, Start::Reset);
        assert!(backend.launch());
        backend.set_breakpoints(&[0x3400]);

        assert!(matches!(backend.resume(), Stop::Running), "a probe target is free-running");
        assert!(matches!(backend.poll(), Stop::Running));
        assert!(matches!(backend.poll(), Stop::Running));
        assert!(
            matches!(backend.poll(), Stop::Breakpoint),
            "a stop on an armed address reports as a breakpoint hit"
        );
        assert_eq!(backend.pc(), 0x3400);
    }

    #[test]
    fn a_stop_away_from_every_breakpoint_is_not_reported_as_one() {
        let mut target = FakeTarget::with_pc(0x3000);
        target.lands_at = 0x3fff;
        let mut backend = ProbeBackend::new(target, Start::Reset);
        assert!(backend.launch());
        backend.set_breakpoints(&[0x3400]);
        assert!(matches!(backend.resume(), Stop::Running));
        assert!(
            matches!(backend.poll(), Stop::Step),
            "the control: an unrelated halt must not be attributed to a breakpoint"
        );
    }

    #[test]
    fn step_advances_the_program_counter() {
        let mut backend = ProbeBackend::new(FakeTarget::with_pc(0x4000), Start::Reset);
        assert!(backend.launch());
        assert!(matches!(backend.step(), Stop::Step));
        assert_eq!(backend.pc(), 0x4002);
    }

    #[test]
    fn pause_halts_a_running_target_and_resyncs_the_program_counter() {
        let mut target = FakeTarget::with_pc(0x5000);
        target.run_for = 100;
        let mut backend = ProbeBackend::new(target, Start::Reset);
        assert!(backend.launch());
        assert!(matches!(backend.resume(), Stop::Running));
        assert!(backend.pause());
        assert!(matches!(backend.poll(), Stop::Step), "after a pause the target is not running");
    }

    #[test]
    fn memory_is_served_at_byte_granularity_across_word_boundaries() {
        let mut target = FakeTarget::with_pc(0);
        target.memory.insert(0x2000_0000, 0x0403_0201);
        target.memory.insert(0x2000_0004, 0x0807_0605);
        let backend = ProbeBackend::new(target, Start::Attach);

        assert_eq!(backend.read_memory(0x2000_0000, 8), vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            backend.read_memory(0x2000_0001, 3),
            vec![2, 3, 4],
            "a request inside one word is trimmed to the bytes asked for"
        );
        assert_eq!(
            backend.read_memory(0x2000_0003, 2),
            vec![4, 5],
            "and one that straddles two words reads both"
        );
        assert!(backend.read_memory(0x2000_0000, 0).is_empty());
    }

    #[test]
    fn the_register_file_is_reported_with_the_program_counter_in_it() {
        let mut target = FakeTarget::with_pc(0x0800_1234);
        target.registers.insert(13, 0x2000_8000);
        target.registers.insert(16, 0x0100_0000);
        let backend = ProbeBackend::new(target, Start::Attach);

        let registers = backend.read_registers();
        assert_eq!(registers.len(), 17, "r0-r12, sp, lr, pc, xpsr");
        let named = |name: &str| {
            registers
                .iter()
                .find(|r| r.name == name)
                .unwrap_or_else(|| panic!("{name} must be reported"))
                .value
        };
        assert_eq!(named("pc"), 0x0800_1234);
        assert_eq!(named("sp"), 0x2000_8000);
        assert_eq!(named("xpsr"), 0x0100_0000);
    }

    #[test]
    fn the_source_level_surface_reports_absence_rather_than_a_guess() {
        let mut backend = ProbeBackend::new(FakeTarget::with_pc(0x6000), Start::Attach);
        assert!(!backend.has_source(), "no native debug info is loaded, and saying so is the contract");
        assert!(backend.variables(0, Scope::Locals).is_empty());
        assert!(backend.resolve_source_breakpoint("a.cs", 1).is_none());
        assert!(backend.source_location(0x6000).is_none());
        assert!(backend.disassemble(0x6000, 0, 4).is_empty());
        assert!(backend.take_output().is_none(), "a probe carries no program output");
    }
}
