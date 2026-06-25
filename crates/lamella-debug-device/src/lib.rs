//! An on-device implementation of [`DebugBackend`]: it drives a halted Cortex-M target
//! over the Lamella CMSIS-DAP stack, so the `lamella-dap` adapter -- and through it VS
//! Code -- can debug AOT-compiled code running on real hardware, the same protocol layer
//! that drives the interpreter.

use core::cell::RefCell;

use lamella_cmsis_dap::{Dap, Transport};
use lamella_debug_backend::{
    DebugBackend, Disassembled, Frame, Register, Scope, SourceLocation, Stop, Variable,
};

/// Drives a Cortex-M target over a CMSIS-DAP probe as a [`DebugBackend`]. The trait's
/// inspection methods take `&self` (suited to the interpreter's in-memory state), so the
/// probe sits behind a `RefCell` for the I/O those methods must perform.
pub struct DeviceBackend<T: Transport> {
    dap: RefCell<Dap<T>>,
    /// `(native offset, source line)` for the loaded method, ascending by offset.
    lines: Vec<(u32, u32)>,
    /// The loaded method's flash base, subtracted from a PC to index `lines`.
    base: u32,
    /// Per-method `(image offset, Type.Method name)`, ascending by offset -- the frame name at a PC.
    names: Vec<(u32, String)>,
    /// The source file the method came from (for source locations), empty if unknown.
    file: String,
    /// Semihosting output captured from the target, drained by `take_output`.
    output: String,
    /// The user's hardware breakpoints (the code addresses last set), kept so a step-over can
    /// re-arm them around its temporary return-address breakpoint.
    breakpoints: Vec<u32>,
    /// The entry method's `Type.Method` name. Stepping out of it means "continue" -- the entry
    /// has no caller within the program (its return is the startup trampoline), so there is no
    /// frame to return to.
    entry: String,
}

impl<T: Transport> DeviceBackend<T> {
    /// Wraps a probe with the loaded method's line table (native offset -> source line),
    /// its flash `base`, per-method `names`, the source `file` it came from, and the `entry`
    /// method's name (stepping out of which continues, having no in-program caller).
    pub fn new(
        dap: Dap<T>,
        lines: Vec<(u32, u32)>,
        base: u32,
        names: Vec<(u32, String)>,
        file: String,
        entry: String,
    ) -> Self {
        DeviceBackend {
            dap: RefCell::new(dap),
            lines,
            base,
            names,
            file,
            output: String::new(),
            breakpoints: Vec::new(),
            entry,
        }
    }

    /// The 1-based source line whose native code contains `offset` (the last entry at or
    /// before it), or 0 if unknown.
    fn source_line_at(&self, offset: u32) -> u32 {
        self.lines
            .iter()
            .rev()
            .find(|&&(start, _)| start <= offset)
            .map_or(0, |&(_, line)| line)
    }

    /// The `Type.Method` name whose code contains `offset` (the last entry at or before it).
    fn method_name_at(&self, offset: u32) -> String {
        self.names
            .iter()
            .rev()
            .find(|&&(start, _)| start <= offset)
            .map_or_else(|| String::from("?"), |(_, name)| name.clone())
    }

    /// Services a halt: if the core stopped at a semihosting `BKPT 0xAB`, captures a
    /// `SYS_WRITE0` string into the output buffer, steps past it, resumes, and reports
    /// `Some(true)` (keep running); a non-semihosting halt is `Some(false)` (a real
    /// stop); a probe error is `None`.
    fn service_semihosting(&mut self) -> Option<bool> {
        let string_bytes = {
            let dap = self.dap.get_mut();
            let pc = dap.read_core_reg(15).ok()?;
            let word = dap.read_word(pc & !3).ok()?;
            let halfword = if pc & 2 != 0 {
                (word >> 16) as u16
            } else {
                word as u16
            };
            if halfword != 0xBEAB {
                return Some(false);
            }
            let bytes = if dap.read_core_reg(0).ok()? == 0x04 {
                let mut addr = dap.read_core_reg(1).ok()?;
                let mut collected = Vec::new();
                while collected.len() < 4096 {
                    let w = dap.read_word(addr & !3).ok()?;
                    let byte = (w >> ((addr & 3) * 8)) as u8;
                    if byte == 0 {
                        break;
                    }
                    collected.push(byte);
                    addr = addr.wrapping_add(1);
                }
                Some(collected)
            } else {
                None
            };
            dap.write_core_reg(15, pc.wrapping_add(2)).ok()?;
            dap.resume().ok()?;
            bytes
        };
        if let Some(bytes) = string_bytes {
            self.output.push_str(&String::from_utf8_lossy(&bytes));
        }
        Some(true)
    }
}

impl<T: Transport> DeviceBackend<T> {
    /// Runs the target at full speed to `target` -- a return address -- arming it as a temporary
    /// breakpoint; reports `Step` on arrival or `Breakpoint` if a user breakpoint intervened. Used
    /// by step-over (run a callee to the live LR) and step-out (run the frame to its saved return).
    /// Comparators: keep all user breakpoints and add `target` if a slot is free or `target` is
    /// already one; else BORROW the comparator of the breakpoint on `target`'s call-site line (the
    /// instruction before it) -- that code cannot run before we reach `target`, so disarming it for
    /// the duration misses nothing; else single-step with a bound (never hanging), stopping on any
    /// user breakpoint (never missing).
    fn run_to_address(&mut self, target: u32) -> Stop {
        let armed: Option<Vec<u32>> =
            if self.breakpoints.len() < 4 || self.breakpoints.contains(&target) {
                let mut a = self.breakpoints.clone();
                if !a.contains(&target) {
                    a.push(target);
                }
                Some(a)
            } else {
                let call_line =
                    self.source_line_at(target.saturating_sub(self.base).saturating_sub(4));
                let borrow = (call_line != 0)
                    .then(|| {
                        self.breakpoints.iter().position(|&bp| {
                            self.source_line_at(bp.saturating_sub(self.base)) == call_line
                        })
                    })
                    .flatten();
                borrow.map(|index| {
                    let mut a = self.breakpoints.clone();
                    a[index] = target;
                    a
                })
            };

        if let Some(armed) = armed {
            let dap = self.dap.get_mut();
            if dap.set_breakpoints(&armed).is_err() {
                return Stop::Fault("arm return breakpoint".into());
            }
            if dap.resume().is_err() {
                return Stop::Fault("resume into call".into());
            }
            let mut halted = false;
            for _ in 0..1_000_000u32 {
                match dap.is_halted() {
                    Ok(true) => {
                        halted = true;
                        break;
                    }
                    Ok(false) => {}
                    Err(_) => return Stop::Fault("poll halt".into()),
                }
            }
            let _ = dap.set_breakpoints(&self.breakpoints);
            if !halted {
                let _ = dap.halt();
                return Stop::Fault("call did not return".into());
            }
            let pc = dap.read_core_reg(15).unwrap_or(0) & !1;
            return if self.breakpoints.contains(&pc) {
                Stop::Breakpoint
            } else {
                Stop::Step
            };
        }

        const FALLBACK_STEP_LIMIT: u32 = 2048;
        for _ in 0..FALLBACK_STEP_LIMIT {
            if self.dap.get_mut().step().is_err() {
                return Stop::Fault("step in call".into());
            }
            let pc = self.dap.get_mut().read_core_reg(15).unwrap_or(0) & !1;
            if pc == target {
                return Stop::Step;
            }
            if self.breakpoints.contains(&pc) {
                return Stop::Breakpoint;
            }
        }
        self.output.push_str(
            "[lamella] Step stopped inside a long-running call: all hardware breakpoints are in \
             use, so it could not be run at full speed. Free a breakpoint, or set one past the \
             call and Continue.\n",
        );
        Stop::Breakpoint
    }

    /// The current frame's return address, recovered by reading the saved LR off the stack -- so
    /// step-out works from a NON-LEAF frame, where the live LR is the frame's own internal return
    /// (set by a call it already made), not its caller's. Our AOT prologue for a non-leaf method is
    /// `push {<callee-saved>, lr}` then an optional `sub sp, #frame`, so the saved LR is the topmost
    /// pushed word, at `sp + frame + 4*saved_count` (SP sits `frame` below the push through the
    /// body). Decodes those two Thumb instructions at the method's entry. Returns `None` if the
    /// prologue is not that shape (e.g. a leaf that never saved LR), so the caller can fall back.
    fn frame_return_address(&mut self, pc: u32) -> Option<u32> {
        let off = pc.saturating_sub(self.base);
        let method_off = self
            .names
            .iter()
            .rev()
            .find(|&&(start, _)| start <= off)
            .map(|&(start, _)| start)?;
        let method_start = self.base + method_off;
        let dap = self.dap.get_mut();
        let w0 = dap.read_word(method_start & !3).ok()?;
        let push = if method_start & 2 != 0 {
            (w0 >> 16) as u16
        } else {
            w0 as u16
        };
        if push & 0xFE00 != 0xB400 || push & 0x0100 == 0 {
            return None;
        }
        let saved_count = u32::from(push & 0x00FF).count_ones();
        let after = method_start + 2;
        let w1 = dap.read_word(after & !3).ok()?;
        let next = if after & 2 != 0 {
            (w1 >> 16) as u16
        } else {
            w1 as u16
        };
        let frame = if next & 0xFF80 == 0xB080 {
            u32::from(next & 0x7F) * 4
        } else {
            0
        };
        let sp = dap.read_core_reg(13).ok()?;
        let saved_lr = dap.read_word((sp + frame + 4 * saved_count) & !3).ok()?;
        Some(saved_lr & !1)
    }
}

impl<T: Transport> DebugBackend for DeviceBackend<T> {
    fn launch(&mut self) -> bool {
        let dap = self.dap.get_mut();
        dap.connect_swd().is_ok()
            && dap.read_idcode().is_ok()
            && dap.init_mem().is_ok()
            && dap.halt().is_ok()
    }

    fn resume(&mut self) -> Stop {
        let bps = self.breakpoints.clone();
        let dap = self.dap.get_mut();
        let _ = dap.set_breakpoints(&bps);
        if let Ok(pc) = dap.read_core_reg(15) {
            if bps.contains(&(pc & !1)) {
                let _ = dap.step();
            }
        }
        match dap.resume() {
            Ok(()) => Stop::Running,
            Err(_) => Stop::Fault("resume failed".into()),
        }
    }

    fn pause(&mut self) -> bool {
        self.dap.get_mut().halt().is_ok()
    }

    fn run_to_return(&mut self) -> Stop {
        let lr = match self.dap.get_mut().read_core_reg(14) {
            Ok(lr) => lr & !1,
            Err(_) => return Stop::Fault("read LR".into()),
        };
        self.run_to_address(lr)
    }

    fn step_out(&mut self) -> Option<Stop> {
        let (pc, lr) = {
            let dap = self.dap.get_mut();
            (
                dap.read_core_reg(15).unwrap_or(0) & !1,
                dap.read_core_reg(14).unwrap_or(0) & !1,
            )
        };
        let here = self.method_name_at(pc.saturating_sub(self.base));
        if here == self.entry {
            return Some(self.resume());
        }
        if self.method_name_at(lr.saturating_sub(self.base)) == here {
            return Some(match self.frame_return_address(pc) {
                Some(ret) => self.run_to_address(ret),
                None => Stop::Step,
            });
        }
        Some(self.run_to_return())
    }

    fn poll(&mut self) -> Stop {
        match self.dap.get_mut().is_halted() {
            Ok(false) => Stop::Running,
            Ok(true) => match self.service_semihosting() {
                Some(true) => Stop::Running,
                Some(false) => Stop::Breakpoint,
                None => Stop::Fault("semihosting service failed".into()),
            },
            Err(_) => Stop::Fault("could not read halt status".into()),
        }
    }

    fn step(&mut self) -> Stop {
        match self.dap.get_mut().step() {
            Ok(()) => Stop::Step,
            Err(_) => Stop::Fault("step failed".into()),
        }
    }

    fn depth(&self) -> usize {
        self.dap
            .borrow_mut()
            .read_core_reg(13)
            .map_or(0, |sp| sp.wrapping_neg() as usize)
    }

    fn set_breakpoints(&mut self, addresses: &[u64]) {
        let words: Vec<u32> = addresses.iter().take(4).map(|&a| a as u32).collect();
        self.breakpoints = words.clone();
        let _ = self.dap.get_mut().set_breakpoints(&words);
    }

    fn max_breakpoints(&self) -> Option<usize> {
        Some(4)
    }

    fn stack(&self) -> Vec<Frame> {
        let pc = self.dap.borrow_mut().read_core_reg(15).unwrap_or(0);
        let line = self.source_line_at(pc.saturating_sub(self.base));
        vec![Frame {
            address: u64::from(pc),
            name: self.method_name_at(pc.saturating_sub(self.base)),
            line,
        }]
    }

    fn resolve_source_breakpoint(&self, _document: &str, line: u32) -> Option<u64> {
        self.lines
            .iter()
            .find(|&&(_, src)| src == line)
            .map(|&(native, _)| u64::from(self.base + native))
    }

    fn source_location(&self, address: u64) -> Option<SourceLocation> {
        if self.file.is_empty() {
            return None;
        }
        let line = self.source_line_at((address as u32).saturating_sub(self.base));
        if line == 0 {
            return None;
        }
        Some(SourceLocation {
            file: self.file.clone(),
            line,
            column: 1,
            end_line: line,
            end_column: 1,
        })
    }

    fn has_source(&self) -> bool {
        !self.lines.is_empty()
    }

    fn at_source_boundary(&self) -> bool {
        let pc = self.dap.borrow_mut().read_core_reg(15).unwrap_or(0);
        let offset = pc.saturating_sub(self.base);
        match self.lines.iter().position(|&(native, _)| native == offset) {
            Some(0) => true,
            Some(i) => self.lines[i].1 != self.lines[i - 1].1,
            None => false,
        }
    }

    fn variables(&self, _frame: usize, _scope: Scope) -> Vec<Variable> {
        Vec::new()
    }

    fn read_memory(&self, address: u64, len: usize) -> Vec<u8> {
        let mut dap = self.dap.borrow_mut();
        let mut out = Vec::with_capacity(len);
        let mut addr = address as u32;
        while out.len() < len {
            match dap.read_word(addr) {
                Ok(word) => out.extend_from_slice(&word.to_le_bytes()),
                Err(_) => break,
            }
            addr = addr.wrapping_add(4);
        }
        out.truncate(len);
        out
    }

    fn read_registers(&self) -> Vec<Register> {
        const NAMES: [&str; 17] = [
            "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11", "r12", "sp",
            "lr", "pc", "xpsr",
        ];
        let mut dap = self.dap.borrow_mut();
        NAMES
            .iter()
            .enumerate()
            .filter_map(|(sel, name)| {
                dap.read_core_reg(sel as u8).ok().map(|value| Register {
                    name: (*name).into(),
                    value: u64::from(value),
                })
            })
            .collect()
    }

    fn disassemble(&self, address: u64, offset: i64, count: usize) -> Vec<Disassembled> {
        let mut dap = self.dap.borrow_mut();
        let start = (address as i64).wrapping_add(offset * 2) as u32;
        (0..count)
            .map(|i| {
                let addr = start.wrapping_add((i as u32) * 2);
                let text = match dap.read_word(addr & !3) {
                    Ok(word) => {
                        let half = if addr & 2 != 0 {
                            word >> 16
                        } else {
                            word & 0xffff
                        };
                        format!("{half:04x}")
                    }
                    Err(_) => "????".into(),
                };
                Disassembled {
                    address: u64::from(addr),
                    text,
                }
            })
            .collect()
    }

    fn take_output(&mut self) -> Option<String> {
        if self.output.is_empty() {
            None
        } else {
            Some(core::mem::take(&mut self.output))
        }
    }
}
