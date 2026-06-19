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
    /// The method's display name.
    name: String,
    /// The source file the method came from (for source locations), empty if unknown.
    file: String,
    /// Semihosting output captured from the target, drained by `take_output`.
    output: String,
}

impl<T: Transport> DeviceBackend<T> {
    /// Wraps a probe with the loaded method's line table (native offset -> source line),
    /// its flash `base`, a display `name`, and the source `file` it came from.
    pub fn new(dap: Dap<T>, lines: Vec<(u32, u32)>, base: u32, name: String, file: String) -> Self {
        DeviceBackend {
            dap: RefCell::new(dap),
            lines,
            base,
            name,
            file,
            output: String::new(),
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

impl<T: Transport> DebugBackend for DeviceBackend<T> {
    fn launch(&mut self) -> bool {
        let dap = self.dap.get_mut();
        dap.connect_swd().is_ok()
            && dap.read_idcode().is_ok()
            && dap.init_mem().is_ok()
            && dap.halt().is_ok()
    }

    fn resume(&mut self) -> Stop {
        match self.dap.get_mut().resume() {
            Ok(()) => Stop::Running,
            Err(_) => Stop::Fault("resume failed".into()),
        }
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
        1
    }

    fn set_breakpoints(&mut self, addresses: &[u64]) {
        let words: Vec<u32> = addresses.iter().map(|&a| a as u32).collect();
        let _ = self.dap.get_mut().set_breakpoints(&words);
    }

    fn stack(&self) -> Vec<Frame> {
        let pc = self.dap.borrow_mut().read_core_reg(15).unwrap_or(0);
        let line = self.source_line_at(pc.saturating_sub(self.base));
        vec![Frame {
            address: u64::from(pc),
            name: self.name.clone(),
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
