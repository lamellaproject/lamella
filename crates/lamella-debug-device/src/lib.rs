//! An on-device implementation of [`DebugBackend`]: it drives a halted Cortex-M target
//! over the Lamella CMSIS-DAP stack, so the `lamella-dap` adapter -- and through it VS
//! Code -- can debug AOT-compiled code running on real hardware, the same protocol layer
//! that drives the interpreter.

use core::cell::RefCell;

use lamella_cmsis_dap::{Dap, Transport};
use lamella_debug_backend::{
    DebugBackend, Disassembled, Frame, Register, Scope, Stop, Variable,
};

/// Drives a Cortex-M target over a CMSIS-DAP probe as a [`DebugBackend`]. The trait's
/// inspection methods take `&self` (suited to the interpreter's in-memory state), so the
/// probe sits behind a `RefCell` for the I/O those methods must perform.
pub struct DeviceBackend<T: Transport> {
    dap: RefCell<Dap<T>>,
    /// `(native offset, CIL index)` for the loaded method, ascending by offset.
    lines: Vec<(u32, u32)>,
    /// The loaded method's flash base, subtracted from a PC to index `lines`.
    base: u32,
    /// The method's display name.
    name: String,
}

impl<T: Transport> DeviceBackend<T> {
    /// Wraps a probe with the loaded method's line table (native offset -> CIL index),
    /// its flash `base`, and a display `name`.
    pub fn new(dap: Dap<T>, lines: Vec<(u32, u32)>, base: u32, name: String) -> Self {
        DeviceBackend {
            dap: RefCell::new(dap),
            lines,
            base,
            name,
        }
    }

    /// The CIL instruction index whose native code contains `offset` (the last entry at
    /// or before it).
    fn cil_index_at(&self, offset: u32) -> u32 {
        self.lines
            .iter()
            .rev()
            .find(|&&(start, _)| start <= offset)
            .map_or(0, |&(_, cil)| cil)
    }

    /// Resumes and polls until the core halts, mapping the outcome to a [`Stop`].
    fn run_until_halt(&mut self) -> Stop {
        let dap = self.dap.get_mut();
        if dap.resume().is_err() {
            return Stop::Fault("resume failed".into());
        }
        for _ in 0..100_000 {
            match dap.is_halted() {
                Ok(true) => return Stop::Breakpoint,
                Ok(false) => {}
                Err(_) => return Stop::Fault("could not read halt status".into()),
            }
        }
        Stop::Fault("target did not halt".into())
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
        self.run_until_halt()
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
        let dap = self.dap.get_mut();
        match addresses.first() {
            Some(&addr) => {
                let _ = dap.set_breakpoint(addr as u32);
            }
            None => {
                let _ = dap.clear_breakpoint();
            }
        }
    }

    fn stack(&self) -> Vec<Frame> {
        let pc = self.dap.borrow_mut().read_core_reg(15).unwrap_or(0);
        let cil = self.cil_index_at(pc.saturating_sub(self.base));
        vec![Frame {
            address: u64::from(pc),
            name: self.name.clone(),
            line: cil + 1,
        }]
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
            "r0", "r1", "r2", "r3", "r4", "r5", "r6", "r7", "r8", "r9", "r10", "r11",
            "r12", "sp", "lr", "pc", "xpsr",
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
                        let half = if addr & 2 != 0 { word >> 16 } else { word & 0xffff };
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
        None
    }
}
