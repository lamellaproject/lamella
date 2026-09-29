//! The eye model: a wire that models the chip's bus registers and 64 bytes
//! of its RAM -- enough for the gSPI transport's own attach to reach
//! `Ready` and for the tuning's traffic -- and answers a read-back of the
//! RAM corrupted by a table's count for the selected setting and phase.

use crate::error::Refusal;
use crate::gspi::{
    GspiWire, Setting, TEST_PATTERN, TUNE_PHASES_MAX, decode_configured, decode_default,
    encode_configured, encode_default, f0,
};

/// The stage name of a frame the model does not answer.
pub const STAGE_EYE_MODEL: &str = "fixture: eye model";
/// The stage name of a read the table marks refused.
pub const STAGE_EYE_REFUSED: &str = "fixture: eye refused";

/// A read the table marks refused.
pub const REFUSED: u16 = 0xFFFF;

const REGS: usize = 32;
const RAM: usize = 64;
const SELECTS: usize = 64;

/// A wire that models the register file and the scratch RAM.
#[derive(Debug)]
pub struct EyeWire<'t> {
    settings: &'t [Setting],
    table: &'t [[u16; TUNE_PHASES_MAX]],
    regs: [u8; REGS],
    ram: [u8; RAM],
    scratch: u32,
    configured: bool,
    selected: (u8, i8),
    selects: [(u8, i8); SELECTS],
    n_selects: usize,
    frames: u32,
    fault: Option<Refusal>,
}

impl<'t> EyeWire<'t> {
    /// A model offering `settings`, answering read-backs per `table` (one
    /// row per setting, one count per phase from the setting's first), with
    /// its RAM at function-1 address `scratch`.
    pub fn new(settings: &'t [Setting], table: &'t [[u16; TUNE_PHASES_MAX]], scratch: u32) -> Self {
        let mut regs = [0u8; REGS];
        regs[f0::TEST_READ as usize..f0::TEST_READ as usize + 4]
            .copy_from_slice(&TEST_PATTERN.to_le_bytes());
        EyeWire {
            settings,
            table,
            regs,
            ram: [0; RAM],
            scratch,
            configured: false,
            selected: (0, 0),
            selects: [(0, 0); SELECTS],
            n_selects: 0,
            frames: 0,
            fault: None,
        }
    }

    /// The selections made, in order (the first 64).
    pub fn selects(&self) -> &[(u8, i8)] {
        &self.selects[..self.n_selects]
    }

    /// The frames answered.
    pub fn frames(&self) -> u32 {
        self.frames
    }

    /// The first frame the model refused, if any.
    pub fn fault(&self) -> Option<Refusal> {
        self.fault
    }

    /// The RAM as the last write left it.
    pub fn ram(&self) -> &[u8; RAM] {
        &self.ram
    }

    /// Make the test register read 0xDEADBEEF from now on: a bus whose
    /// chosen setting corrupts a register read.
    pub fn corrupt_test_register(&mut self) {
        self.regs[f0::TEST_READ as usize..f0::TEST_READ as usize + 4]
            .copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
    }

    fn refuse(&mut self, stage: &'static str, status: u32) -> Refusal {
        let refusal = Refusal::new(stage, status);
        self.fault.get_or_insert(refusal);
        refusal
    }

    fn mismatches(&self) -> u16 {
        let (s, phase) = self.selected;
        let Some(setting) = self.settings.get(s as usize) else {
            return REFUSED;
        };
        let index = i16::from(phase) - i16::from(setting.first);
        if index < 0 || index >= i16::from(setting.phases) {
            return REFUSED;
        }
        self.table
            .get(s as usize)
            .map_or(REFUSED, |row| row[index as usize])
    }
}

impl GspiWire for EyeWire<'_> {
    fn set_power(&mut self, _on: bool) {}

    fn strap(&mut self, _hold: bool) {}

    fn frame(&mut self, cmd: [u8; 4], tx: &[u8], rx: &mut [u8]) -> Result<(), Refusal> {
        self.frames += 1;
        let word = if self.configured {
            decode_configured(cmd)
        } else {
            decode_default(cmd)
        };
        let write = word & (1 << 31) != 0;
        let func = (word >> 28) & 3;
        let addr = (word >> 11) & 0x1_FFFF;
        let len = (word & 0x7FF) as usize;
        match func {
            0 => {
                let a = addr as usize;
                if a + len > REGS || len == 0 {
                    return Err(self.refuse(STAGE_EYE_MODEL, word));
                }
                if write {
                    if tx.len() != len {
                        return Err(self.refuse(STAGE_EYE_MODEL, word));
                    }
                    if a != f0::TEST_READ as usize {
                        if !self.configured && len == 4 {
                            let value = decode_default([tx[0], tx[1], tx[2], tx[3]]);
                            self.regs[a..a + 4].copy_from_slice(&value.to_le_bytes());
                        } else {
                            self.regs[a..a + len].copy_from_slice(tx);
                        }
                    }
                    if a == f0::CONFIG as usize && self.regs[0] & 0x03 == 0x03 {
                        self.configured = true;
                    }
                    Ok(())
                } else {
                    if rx.len() != len {
                        return Err(self.refuse(STAGE_EYE_MODEL, word));
                    }
                    let mut bytes = [0u8; REGS];
                    bytes.copy_from_slice(&self.regs);
                    bytes[f0::INTERRUPT as usize..f0::INTERRUPT as usize + 2].fill(0);
                    bytes[f0::STATUS as usize..f0::STATUS as usize + 4].fill(0);
                    if len == 4 {
                        let value = u32::from_le_bytes([
                            bytes[a],
                            bytes[a + 1],
                            bytes[a + 2],
                            bytes[a + 3],
                        ]);
                        let wire = if self.configured {
                            encode_configured(value)
                        } else {
                            encode_default(value)
                        };
                        rx.copy_from_slice(&wire);
                    } else {
                        rx.copy_from_slice(&bytes[a..a + len]);
                    }
                    Ok(())
                }
            }
            1 if self.configured && addr == self.scratch && len <= RAM && len > 0 => {
                if write {
                    if tx.len() != len {
                        return Err(self.refuse(STAGE_EYE_MODEL, word));
                    }
                    self.ram[..len].copy_from_slice(tx);
                    Ok(())
                } else {
                    if rx.len() != 4 + len {
                        return Err(self.refuse(STAGE_EYE_MODEL, word));
                    }
                    let bad = self.mismatches();
                    if bad == REFUSED {
                        return Err(self.refuse(STAGE_EYE_REFUSED, word));
                    }
                    rx[..4].fill(0);
                    rx[4..].copy_from_slice(&self.ram[..len]);
                    for b in rx[4..].iter_mut().take(bad as usize) {
                        *b ^= 0xFF;
                    }
                    Ok(())
                }
            }
            _ => Err(self.refuse(STAGE_EYE_MODEL, word)),
        }
    }

    fn irq_asserted(&mut self) -> bool {
        false
    }

    fn settings(&self) -> u8 {
        self.settings.len() as u8
    }

    fn setting(&self, index: u8) -> Setting {
        self.settings
            .get(index as usize)
            .copied()
            .unwrap_or(Setting::NONE)
    }

    fn select(&mut self, index: u8, phase: i8) {
        self.selected = (index, phase);
        if self.n_selects < SELECTS {
            self.selects[self.n_selects] = (index, phase);
            self.n_selects += 1;
        }
    }

    fn selected(&self) -> (u8, i8) {
        self.selected
    }
}
