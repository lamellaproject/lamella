//! A from-scratch RISC-V External Debug (Debug Module) core, driven over a [`Dmi`] link. The register
//! addresses and bit fields are the RISC-V Debug specification; the sequences -- bring the DM up, halt,
//! resume, and read/write registers through abstract commands -- are the standard flows. Transport
//! agnostic: the WCH-Link's `dmi_op` is one [`Dmi`]; a JTAG probe could be another.

use core::fmt;

/// A Debug Module Interface: read/write one DM register (a 7-bit address, 32-bit data). The WCH-Link's
/// `dmi_op` command implements this; a mock implements it in tests.
pub trait Dmi {
    /// Reads the DM register at `addr`.
    fn dmi_read(&mut self, addr: u8) -> Result<u32, DmError>;
    /// Writes `data` to the DM register at `addr`.
    fn dmi_write(&mut self, addr: u8, data: u32) -> Result<(), DmError>;
}

/// DM register addresses (RISC-V Debug specification).
pub mod reg {
    /// Abstract data 0 -- the value an Access Register abstract command reads or writes.
    pub const DATA0: u8 = 0x04;
    /// Debug Module control.
    pub const DMCONTROL: u8 = 0x10;
    /// Debug Module status.
    pub const DMSTATUS: u8 = 0x11;
    /// Hart info.
    pub const HARTINFO: u8 = 0x12;
    /// Abstract control and status.
    pub const ABSTRACTCS: u8 = 0x16;
    /// Abstract command.
    pub const COMMAND: u8 = 0x17;
    /// Program buffer 0 (the base of the program-buffer window).
    pub const PROGBUF0: u8 = 0x20;
    /// System-bus control and status.
    pub const SBCS: u8 = 0x38;
}

const DMCONTROL_DMACTIVE: u32 = 1 << 0;
const DMCONTROL_RESUMEREQ: u32 = 1 << 30;
const DMCONTROL_HALTREQ: u32 = 1 << 31;

const DMSTATUS_VERSION: u32 = 0xf;
const DMSTATUS_ALLHALTED: u32 = 1 << 9;
const DMSTATUS_ALLRESUMEACK: u32 = 1 << 17;

const ABSTRACTCS_BUSY: u32 = 1 << 12;
const ABSTRACTCS_CMDERR_SHIFT: u32 = 8;
const ABSTRACTCS_CMDERR_MASK: u32 = 0x7 << ABSTRACTCS_CMDERR_SHIFT;

const CMD_ACCESS_REGISTER: u32 = 0 << 24;
const CMD_AARSIZE_32: u32 = 2 << 20;
const CMD_TRANSFER: u32 = 1 << 17;
const CMD_WRITE: u32 = 1 << 16;

/// The abstract-command register number of GPR `n` (RISC-V Debug: `0x1000 + n`). RV32EC has `n` in
/// `0..16`; RV32I/E-with-more have up to `0..32`.
pub const fn gpr(n: u16) -> u16 {
    0x1000 + n
}

/// The abstract-command register number of CSR `n` (the CSR address itself, e.g. `dcsr` = `0x7b0`,
/// `dpc` = `0x7b1`).
pub const fn csr(n: u16) -> u16 {
    n
}

/// How many times a status poll (halt, resume, or abstract-command-busy) is retried before timing out.
const POLL_LIMIT: u32 = 4096;

/// A RISC-V hart behind a Debug Module, reached over a [`Dmi`] link. Borrows the link for the session.
pub struct Dm<'a, T: Dmi> {
    dmi: &'a mut T,
}

impl<'a, T: Dmi> Dm<'a, T> {
    /// Wraps a DMI link. Call [`Dm::enable`] before the other operations.
    pub fn new(dmi: &'a mut T) -> Self {
        Self { dmi }
    }

    /// Brings the Debug Module up (`dmactive`) and returns the debug-spec version from `dmstatus`
    /// (`2` = 0.13, `3` = 1.0).
    pub fn enable(&mut self) -> Result<u8, DmError> {
        self.dmi.dmi_write(reg::DMCONTROL, DMCONTROL_DMACTIVE)?;
        let status = self.dmi.dmi_read(reg::DMSTATUS)?;
        Ok((status & DMSTATUS_VERSION) as u8)
    }

    /// Requests a halt and waits until the hart reports halted, then drops `haltreq` (so a later
    /// [`Dm::resume`] is not immediately re-halted).
    pub fn halt(&mut self) -> Result<(), DmError> {
        self.dmi.dmi_write(reg::DMCONTROL, DMCONTROL_HALTREQ | DMCONTROL_DMACTIVE)?;
        self.poll(|s| s & DMSTATUS_ALLHALTED != 0, DmError::HaltTimeout)?;
        self.dmi.dmi_write(reg::DMCONTROL, DMCONTROL_DMACTIVE)?;
        Ok(())
    }

    /// Requests a resume and waits for the resume acknowledgement.
    pub fn resume(&mut self) -> Result<(), DmError> {
        self.dmi.dmi_write(reg::DMCONTROL, DMCONTROL_RESUMEREQ | DMCONTROL_DMACTIVE)?;
        self.poll(|s| s & DMSTATUS_ALLRESUMEACK != 0, DmError::ResumeTimeout)?;
        self.dmi.dmi_write(reg::DMCONTROL, DMCONTROL_DMACTIVE)?;
        Ok(())
    }

    /// True if the hart currently reports halted.
    pub fn is_halted(&mut self) -> Result<bool, DmError> {
        Ok(self.dmi.dmi_read(reg::DMSTATUS)? & DMSTATUS_ALLHALTED != 0)
    }

    /// Reads abstract register `regno` (build one with [`gpr`] or [`csr`]) via an Access Register
    /// abstract command.
    pub fn read_register(&mut self, regno: u16) -> Result<u32, DmError> {
        let command = CMD_ACCESS_REGISTER | CMD_AARSIZE_32 | CMD_TRANSFER | u32::from(regno);
        self.run_abstract(command)?;
        self.dmi.dmi_read(reg::DATA0)
    }

    /// Writes `value` to abstract register `regno`.
    pub fn write_register(&mut self, regno: u16, value: u32) -> Result<(), DmError> {
        self.dmi.dmi_write(reg::DATA0, value)?;
        let command =
            CMD_ACCESS_REGISTER | CMD_AARSIZE_32 | CMD_TRANSFER | CMD_WRITE | u32::from(regno);
        self.run_abstract(command)
    }

    /// Reads GPR `n` (RV32EC: `0..16`).
    pub fn read_gpr(&mut self, n: u16) -> Result<u32, DmError> {
        self.read_register(gpr(n))
    }

    /// Writes GPR `n`.
    pub fn write_gpr(&mut self, n: u16, value: u32) -> Result<(), DmError> {
        self.write_register(gpr(n), value)
    }

    /// Reads CSR `n` (e.g. `dpc` = `0x7b1`).
    pub fn read_csr(&mut self, n: u16) -> Result<u32, DmError> {
        self.read_register(csr(n))
    }

    /// Issues an abstract command and waits for it to finish, surfacing any `cmderr`.
    fn run_abstract(&mut self, command: u32) -> Result<(), DmError> {
        self.dmi.dmi_write(reg::COMMAND, command)?;
        let mut tries = 0;
        loop {
            let status = self.dmi.dmi_read(reg::ABSTRACTCS)?;
            if status & ABSTRACTCS_BUSY == 0 {
                let cmderr = (status & ABSTRACTCS_CMDERR_MASK) >> ABSTRACTCS_CMDERR_SHIFT;
                if cmderr != 0 {
                    self.dmi.dmi_write(reg::ABSTRACTCS, ABSTRACTCS_CMDERR_MASK)?;
                    return Err(DmError::AbstractCommand(cmderr as u8));
                }
                return Ok(());
            }
            tries += 1;
            if tries >= POLL_LIMIT {
                return Err(DmError::AbstractTimeout);
            }
        }
    }

    /// Polls `dmstatus` until `done` holds, or fails with `timeout` after [`POLL_LIMIT`] tries.
    fn poll(&mut self, done: impl Fn(u32) -> bool, timeout: DmError) -> Result<(), DmError> {
        let mut tries = 0;
        loop {
            if done(self.dmi.dmi_read(reg::DMSTATUS)?) {
                return Ok(());
            }
            tries += 1;
            if tries >= POLL_LIMIT {
                return Err(timeout);
            }
        }
    }
}

/// A Debug Module operation error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DmError {
    /// The DMI transport failed (its message).
    Dmi(String),
    /// The hart did not report halted within the poll limit.
    HaltTimeout,
    /// The hart did not acknowledge resume within the poll limit.
    ResumeTimeout,
    /// An abstract command stayed busy past the poll limit.
    AbstractTimeout,
    /// An abstract command reported a `cmderr` (the RISC-V Debug `cmderr` code).
    AbstractCommand(u8),
}

impl fmt::Display for DmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DmError::Dmi(message) => write!(f, "DMI transport error: {message}"),
            DmError::HaltTimeout => write!(f, "hart did not halt within the poll limit"),
            DmError::ResumeTimeout => write!(f, "hart did not acknowledge resume within the poll limit"),
            DmError::AbstractTimeout => write!(f, "abstract command stayed busy past the poll limit"),
            DmError::AbstractCommand(code) => write!(f, "abstract command failed (cmderr {code})"),
        }
    }
}

impl std::error::Error for DmError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A minimal Debug Module model: halt/resume flags and an Access Register engine over a GPR file,
    /// with abstract commands completing immediately (never busy, no cmderr).
    #[derive(Default)]
    struct MockDm {
        halted: bool,
        resumeack: bool,
        data0: u32,
        gprs: BTreeMap<u16, u32>,
    }

    impl Dmi for MockDm {
        fn dmi_read(&mut self, addr: u8) -> Result<u32, DmError> {
            Ok(match addr {
                reg::DMSTATUS => {
                    let mut s = 2u32;
                    if self.halted {
                        s |= DMSTATUS_ALLHALTED;
                    }
                    if self.resumeack {
                        s |= DMSTATUS_ALLRESUMEACK;
                    }
                    s
                }
                reg::ABSTRACTCS => 0,
                reg::DATA0 => self.data0,
                _ => 0,
            })
        }

        fn dmi_write(&mut self, addr: u8, data: u32) -> Result<(), DmError> {
            match addr {
                reg::DMCONTROL => {
                    if data & DMCONTROL_HALTREQ != 0 {
                        self.halted = true;
                    }
                    if data & DMCONTROL_RESUMEREQ != 0 {
                        self.halted = false;
                        self.resumeack = true;
                    }
                }
                reg::DATA0 => self.data0 = data,
                reg::COMMAND => {
                    let regno = (data & 0xffff) as u16;
                    if data & CMD_WRITE != 0 {
                        self.gprs.insert(regno, self.data0);
                    } else {
                        self.data0 = *self.gprs.get(&regno).unwrap_or(&0);
                    }
                }
                _ => {}
            }
            Ok(())
        }
    }

    #[test]
    fn enable_reports_the_debug_version() {
        let mut mock = MockDm::default();
        assert_eq!(Dm::new(&mut mock).enable().unwrap(), 2);
    }

    #[test]
    fn halt_and_resume_track_hart_state() {
        let mut mock = MockDm::default();
        let mut dm = Dm::new(&mut mock);
        dm.enable().unwrap();
        dm.halt().unwrap();
        assert!(dm.is_halted().unwrap());
        dm.resume().unwrap();
        assert!(!dm.is_halted().unwrap());
    }

    #[test]
    fn writes_then_reads_a_gpr_via_abstract_commands() {
        let mut mock = MockDm::default();
        let mut dm = Dm::new(&mut mock);
        dm.enable().unwrap();
        dm.write_gpr(15, 0xDEAD_BEEF).unwrap();
        assert_eq!(dm.read_gpr(15).unwrap(), 0xDEAD_BEEF);
        assert_eq!(*mock.gprs.get(&0x100f).unwrap(), 0xDEAD_BEEF);
    }
}
