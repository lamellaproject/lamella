//! A CMSIS-DAP debug-probe host: connect to a target over SWD and run debug-port
//! transactions, built on the [`proto`] command layer and a byte-packet [`Transport`].

pub mod proto;

use proto::{Ack, Port};

/// A failure exchanging a packet with the probe.
#[derive(Debug)]
pub struct TransportError(pub String);

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "probe transport error: {}", self.0)
    }
}
impl std::error::Error for TransportError {}

/// A byte-packet link to a CMSIS-DAP probe: write a command packet, read its reply.
pub trait Transport {
    /// Sends one command packet to the probe.
    fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError>;
    /// Reads one reply packet into `buf`, returning its length.
    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
}

/// The standard CMSIS-DAP v1 HID report size.
const PACKET: usize = 64;

/// MEM-AP CSW for 32-bit memory accesses with single auto-increment: reserved bit +
/// master-type debug + HPROT data + DbgStatus + size-word + single-increment.
const CSW_WORD: u32 = 0x2300_0052;

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

const NVMC_READY: u32 = 0x4001_e400;
const NVMC_CONFIG: u32 = 0x4001_e504;
const NVMC_ERASEPAGE: u32 = 0x4001_e508;
const NVMC_REN: u32 = 0;
const NVMC_WEN: u32 = 1;
const NVMC_EEN: u32 = 2;

const SAMD21_CTRLA: u32 = 0x4100_4000;
const SAMD21_CTRLB: u32 = 0x4100_4004;
const SAMD21_INTFLAG: u32 = 0x4100_4014;
const SAMD21_ADDR: u32 = 0x4100_401c;
const SAMD21_CMDEX: u32 = 0xa500;
const SAMD21_CMD_ER: u32 = 0x02;
const SAMD21_CMD_WP: u32 = 0x04;
const SAMD21_CMD_PBC: u32 = 0x44;
const SAMD21_PAGE: usize = 64;
const SAMD21_ROW: u32 = 256;
const SAMD21_MANW: u32 = 1 << 7;

const AIRCR: u32 = 0xe000_ed0c;
const AIRCR_SYSRESETREQ: u32 = 0x05fa_0004;

const FP_CTRL: u32 = 0xe000_2000;
const FP_COMP0: u32 = 0xe000_2008;

#[cfg(feature = "usbhid")]
impl Transport for lamella_usbhid::Device {
    fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError> {
        self.write_report(data)
            .map_err(|e| TransportError(e.to_string()))
    }
    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        self.read_report(buf, std::time::Duration::from_millis(1000))
            .map_err(|e| TransportError(e.to_string()))
    }
}

/// An error from a debug operation.
#[derive(Debug)]
pub enum DapError {
    /// The packet transport failed.
    Transport(TransportError),
    /// A reply could not be decoded.
    Proto(proto::ProtoError),
    /// The probe's reply echoed the wrong command id.
    Unexpected {
        /// The command id sent.
        expected: u8,
        /// The command id received.
        got: u8,
    },
    /// A transfer returned a non-OK acknowledge.
    Ack(Ack),
    /// An operation polled past its limit without completing (names what was awaited).
    Timeout(&'static str),
}

impl std::fmt::Display for DapError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DapError::Transport(e) => write!(f, "{e}"),
            DapError::Proto(e) => write!(f, "malformed probe reply: {e:?}"),
            DapError::Unexpected { expected, got } => {
                write!(
                    f,
                    "probe echoed command {got:#04x}, expected {expected:#04x}"
                )
            }
            DapError::Ack(ack) => write!(f, "transfer not acknowledged: {ack:?}"),
            DapError::Timeout(what) => write!(f, "timed out waiting for {what}"),
        }
    }
}
impl std::error::Error for DapError {}

impl From<TransportError> for DapError {
    fn from(e: TransportError) -> Self {
        DapError::Transport(e)
    }
}
impl From<proto::ProtoError> for DapError {
    fn from(e: proto::ProtoError) -> Self {
        DapError::Proto(e)
    }
}

/// A connected CMSIS-DAP probe driving a target over SWD.
pub struct Dap<T: Transport> {
    transport: T,
    reply: [u8; PACKET],
}

impl<T: Transport> Dap<T> {
    /// Wraps a packet transport.
    pub fn new(transport: T) -> Self {
        Dap {
            transport,
            reply: [0; PACKET],
        }
    }

    /// Sends a command and returns the reply slice, checking the command-id echo.
    fn command(&mut self, request: &[u8]) -> Result<&[u8], DapError> {
        self.transport.write_packet(request)?;
        let n = self.transport.read_packet(&mut self.reply)?;
        let reply = &self.reply[..n];
        if reply.first() != request.first() {
            return Err(DapError::Unexpected {
                expected: request.first().copied().unwrap_or(0),
                got: reply.first().copied().unwrap_or(0),
            });
        }
        Ok(reply)
    }

    /// Connects to the target over SWD: select the port, set the clock, then send the
    /// line-reset and JTAG-to-SWD switch sequence (ADIv5).
    pub fn connect_swd(&mut self) -> Result<(), DapError> {
        self.command(&proto::connect(Port::Swd))?;
        self.command(&proto::swj_clock(1_000_000))?;
        self.command(&proto::swj_sequence(51, &[0xff; 7]))?;
        self.command(&proto::swj_sequence(16, &[0x9e, 0xe7]))?;
        self.command(&proto::swj_sequence(51, &[0xff; 7]))?;
        self.command(&proto::swj_sequence(8, &[0x00]))?;
        Ok(())
    }

    /// Reads the Debug Port `IDCODE` (`DPIDR`) -- the first transaction after connecting,
    /// and the proof the link is alive.
    pub fn read_idcode(&mut self) -> Result<u32, DapError> {
        self.read_dp(0x0)
    }

    /// Powers up the debug and system domains and configures the MEM-AP for 32-bit
    /// access. Call once after connecting, before any memory access.
    pub fn init_mem(&mut self) -> Result<(), DapError> {
        self.write_dp(0x0, 0x0000_001e)?;
        self.write_dp(0x8, 0x0000_0000)?;
        self.write_dp(0x4, 0x5000_0000)?;
        for _ in 0..128 {
            if self.read_dp(0x4)? & 0xa000_0000 == 0xa000_0000 {
                return self.write_ap(0x0, CSW_WORD);
            }
        }
        Err(DapError::Timeout("debug power-up"))
    }

    /// Reads a 32-bit word from target memory through the MEM-AP. A CMSIS-DAP
    /// `DAP_Transfer` resolves the posted AP read itself, so the DRW read returns the
    /// data directly.
    pub fn read_word(&mut self, address: u32) -> Result<u32, DapError> {
        self.write_ap(0x4, address)?;
        self.read_ap(0xc)
    }

    /// Writes a 32-bit word to target memory through the MEM-AP.
    pub fn write_word(&mut self, address: u32, value: u32) -> Result<(), DapError> {
        self.write_ap(0x4, address)?;
        self.write_ap(0xc, value)
    }

    /// Halts the processor core.
    pub fn halt(&mut self) -> Result<(), DapError> {
        self.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_HALT)
    }

    /// Resumes the processor core from a halt.
    pub fn resume(&mut self) -> Result<(), DapError> {
        self.write_word(DHCSR, DBGKEY | C_DEBUGEN)
    }

    /// Single-steps one instruction; the core must already be halted. Interrupts (PendSV,
    /// SysTick, external) are masked across the step so it advances the program rather than
    /// entering a pending handler.
    ///
    /// Per the Armv6-M ARM (DDI0419E, C1.5 Debug event behavior), `C_MASKINTS` must be set in a
    /// write SEPARATE from the one that clears `C_HALT` -- changing `C_MASKINTS` while clearing
    /// `C_HALT` in a single write is UNPREDICTABLE. So this masks while still halted, then steps,
    /// then unmasks while halted again -- the last write keeps a subsequent `resume` (which
    /// clears `C_HALT`) from having to change `C_MASKINTS` in the same write, which would itself
    /// be UNPREDICTABLE.
    pub fn step(&mut self) -> Result<(), DapError> {
        self.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_HALT | C_MASKINTS)?;
        self.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_STEP | C_MASKINTS)?;
        self.poll_dhcsr(S_HALT, "core halt")?;
        self.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_HALT)
    }

    /// Returns whether the core is currently halted.
    pub fn is_halted(&mut self) -> Result<bool, DapError> {
        Ok(self.read_word(DHCSR)? & S_HALT != 0)
    }

    /// Reads a core register by its DCRSR selector: 0-15 are `r0`-`r15`, 16 is `xPSR`.
    /// The core must be halted.
    pub fn read_core_reg(&mut self, selector: u8) -> Result<u32, DapError> {
        self.write_word(DCRSR, u32::from(selector))?;
        self.poll_dhcsr(S_REGRDY, "register transfer")?;
        self.read_word(DCRDR)
    }

    /// Writes a core register by its DCRSR selector. The core must be halted.
    pub fn write_core_reg(&mut self, selector: u8, value: u32) -> Result<(), DapError> {
        self.write_word(DCRDR, value)?;
        self.write_word(DCRSR, u32::from(selector) | DCRSR_WRITE)?;
        self.poll_dhcsr(S_REGRDY, "register transfer")
    }

    /// Polls DHCSR until `flag` is set (used for S_HALT after a step and S_REGRDY after
    /// a core-register transfer).
    fn poll_dhcsr(&mut self, flag: u32, what: &'static str) -> Result<(), DapError> {
        for _ in 0..128 {
            if self.read_word(DHCSR)? & flag != 0 {
                return Ok(());
            }
        }
        Err(DapError::Timeout(what))
    }

    /// Erases the flash page containing `address` (nRF51 pages are 1 KB) via the NVMC.
    /// Halt the core first so it is not executing from flash during the erase.
    pub fn erase_flash_page(&mut self, address: u32) -> Result<(), DapError> {
        self.write_word(NVMC_CONFIG, NVMC_EEN)?;
        self.nvmc_wait()?;
        self.write_word(NVMC_ERASEPAGE, address & !0x3ff)?;
        self.nvmc_wait()?;
        self.write_word(NVMC_CONFIG, NVMC_REN)
    }

    /// Programs consecutive 32-bit `words` to flash starting at `address`, via the NVMC.
    /// The target pages must already be erased.
    pub fn write_flash(&mut self, address: u32, words: &[u32]) -> Result<(), DapError> {
        self.write_word(NVMC_CONFIG, NVMC_WEN)?;
        self.nvmc_wait()?;
        for (i, &word) in words.iter().enumerate() {
            self.write_word(address + (i as u32) * 4, word)?;
            self.nvmc_wait()?;
        }
        self.write_word(NVMC_CONFIG, NVMC_REN)
    }

    /// Polls the NVMC READY register until the controller is idle.
    fn nvmc_wait(&mut self) -> Result<(), DapError> {
        for _ in 0..1000 {
            if self.read_word(NVMC_READY)? & 1 != 0 {
                return Ok(());
            }
        }
        Err(DapError::Timeout("flash controller"))
    }

    /// Erases the SAMD21 flash row (256 bytes) containing `address`, via the NVMCTRL. Halt the
    /// core first so it is not fetching from flash during the erase.
    pub fn erase_flash_row_samd21(&mut self, address: u32) -> Result<(), DapError> {
        self.write_word(SAMD21_ADDR, (address & !(SAMD21_ROW - 1)) / 2)?;
        self.samd21_command(SAMD21_CMD_ER)
    }

    /// Programs consecutive 32-bit `words` to SAMD21 flash from `address`, via the NVMCTRL, one
    /// 64-byte page at a time (the rows must already be erased). Manual write, per datasheet
    /// 22.6.4.3.1: clear the page buffer, fill it through the flash address space, issue a
    /// read-memory barrier, set the page address, then Write-Page.
    pub fn write_flash_samd21(&mut self, address: u32, words: &[u32]) -> Result<(), DapError> {
        let ctrlb = self.read_word(SAMD21_CTRLB)?;
        self.write_word(SAMD21_CTRLB, ctrlb | SAMD21_MANW)?;
        for (page, chunk) in words.chunks(SAMD21_PAGE / 4).enumerate() {
            let page_addr = address + (page as u32) * SAMD21_PAGE as u32;
            self.samd21_command(SAMD21_CMD_PBC)?;
            for (i, &word) in chunk.iter().enumerate() {
                self.write_word(page_addr + (i as u32) * 4, word)?;
            }
            self.read_word(page_addr)?;
            self.write_word(SAMD21_ADDR, page_addr / 2)?;
            self.samd21_command(SAMD21_CMD_WP)?;
        }
        Ok(())
    }

    /// Issues an NVMCTRL command (CMDEX key + `cmd`) and waits for the controller to be ready.
    fn samd21_command(&mut self, cmd: u32) -> Result<(), DapError> {
        self.write_word(SAMD21_CTRLA, SAMD21_CMDEX | cmd)?;
        for _ in 0..1000 {
            if self.read_word(SAMD21_INTFLAG)? & 1 != 0 {
                return Ok(());
            }
        }
        Err(DapError::Timeout("SAMD21 flash controller"))
    }

    /// Resets the core (SYSRESETREQ) and resumes it, so it restarts from the reset
    /// vector -- the run step after flashing a fresh image.
    pub fn reset_and_run(&mut self) -> Result<(), DapError> {
        let _ = self.write_word(AIRCR, AIRCR_SYSRESETREQ);
        self.resume()
    }

    /// Sets hardware breakpoint comparator 0 at a code `address`: the core halts when its
    /// PC reaches that instruction. Uses the Cortex-M0 Breakpoint Unit.
    pub fn set_breakpoint(&mut self, address: u32) -> Result<(), DapError> {
        self.write_word(FP_CTRL, 0b11)?;
        let bp_match = if address & 0x2 != 0 { 0b10 } else { 0b01 };
        let comp = (bp_match << 30) | (address & 0x1fff_fffc) | 1;
        self.write_word(FP_COMP0, comp)
    }

    /// Disables hardware breakpoint comparator 0.
    pub fn clear_breakpoint(&mut self) -> Result<(), DapError> {
        self.write_word(FP_COMP0, 0)
    }

    /// Replaces every hardware breakpoint with `addresses`, one per comparator (the
    /// Cortex-M0 BPU has four). Enables the FPB; comparators past `addresses` are cleared,
    /// and any address beyond the fourth is dropped.
    pub fn set_breakpoints(&mut self, addresses: &[u32]) -> Result<(), DapError> {
        self.write_word(FP_CTRL, 0b11)?;
        for i in 0..4u32 {
            let comp = match addresses.get(i as usize) {
                Some(&address) => {
                    let bp_match = if address & 0x2 != 0 { 0b10 } else { 0b01 };
                    (bp_match << 30) | (address & 0x1fff_fffc) | 1
                }
                None => 0,
            };
            self.write_word(FP_COMP0 + i * 4, comp)?;
        }
        Ok(())
    }

    fn read_dp(&mut self, reg: u8) -> Result<u32, DapError> {
        self.transfer_read(proto::dp_read(reg))
    }
    fn write_dp(&mut self, reg: u8, value: u32) -> Result<(), DapError> {
        self.transfer_write(proto::dp_write(reg), value)
    }
    fn read_ap(&mut self, reg: u8) -> Result<u32, DapError> {
        self.transfer_read(proto::ap_read(reg))
    }
    fn write_ap(&mut self, reg: u8, value: u32) -> Result<(), DapError> {
        self.transfer_write(proto::ap_write(reg), value)
    }

    /// Issues one read transfer and returns its data.
    fn transfer_read(&mut self, request: u8) -> Result<u32, DapError> {
        let reply = self.command(&proto::transfer_one(request, None))?;
        let parsed = proto::parse_read(reply)?;
        match parsed.ack {
            Ack::Ok => Ok(parsed.data.unwrap_or(0)),
            other => Err(DapError::Ack(other)),
        }
    }

    /// Issues one write transfer.
    fn transfer_write(&mut self, request: u8, value: u32) -> Result<(), DapError> {
        let reply = self.command(&proto::transfer_one(request, Some(value)))?;
        match proto::parse_read(reply)?.ack {
            Ack::Ok => Ok(()),
            other => Err(DapError::Ack(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A transport that returns canned reply packets and records what was sent.
    struct Mock {
        replies: VecDeque<Vec<u8>>,
        sent: Vec<Vec<u8>>,
    }

    impl Mock {
        fn new(replies: Vec<Vec<u8>>) -> Self {
            Mock {
                replies: replies.into(),
                sent: Vec::new(),
            }
        }
    }

    impl Transport for Mock {
        fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError> {
            self.sent.push(data.to_vec());
            Ok(())
        }
        fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            let r = self
                .replies
                .pop_front()
                .ok_or_else(|| TransportError("no canned reply".into()))?;
            buf[..r.len()].copy_from_slice(&r);
            Ok(r.len())
        }
    }

    fn echo(id: u8, rest: &[u8]) -> Vec<u8> {
        let mut v = vec![id];
        v.extend_from_slice(rest);
        v
    }

    #[test]
    fn connect_then_read_idcode() {
        let replies = vec![
            echo(proto::cmd::CONNECT, &[Port::Swd as u8]),
            echo(proto::cmd::SWJ_CLOCK, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x77, 0x14, 0xb1, 0x0b],
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.connect_swd().unwrap();
        assert_eq!(dap.read_idcode().unwrap(), 0x0bb1_1477);
    }

    #[test]
    fn wrong_echo_is_error() {
        let mut dap = Dap::new(Mock::new(vec![vec![0xff, 0, 0]]));
        assert!(matches!(
            dap.read_idcode(),
            Err(DapError::Unexpected { .. })
        ));
    }

    #[test]
    fn fault_ack_surfaces() {
        let mut dap = Dap::new(Mock::new(vec![vec![proto::cmd::TRANSFER, 0x00, 0x04]]));
        assert!(matches!(dap.read_idcode(), Err(DapError::Ack(Ack::Fault))));
    }

    #[test]
    fn read_word_returns_drw() {
        let replies = vec![
            echo(proto::cmd::TRANSFER, &[0x01, 0x01]),
            vec![proto::cmd::TRANSFER, 0x01, 0x01, 0xef, 0xbe, 0xad, 0xde],
        ];
        let mut dap = Dap::new(Mock::new(replies));
        assert_eq!(dap.read_word(0x2000_0000).unwrap(), 0xdead_beef);
    }

    #[test]
    fn write_word_sends_tar_then_drw() {
        let replies = vec![
            echo(proto::cmd::TRANSFER, &[0x01, 0x01]),
            echo(proto::cmd::TRANSFER, &[0x01, 0x01]),
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.write_word(0x2000_0000, 0xdead_beef).unwrap();
        assert_eq!(dap.transport.sent.len(), 2);
        assert_eq!(&dap.transport.sent[1][4..8], &[0xef, 0xbe, 0xad, 0xde]);
    }

    #[test]
    fn init_mem_powers_up_then_sets_csw() {
        let replies = vec![
            echo(proto::cmd::TRANSFER, &[0x01, 0x01]),
            echo(proto::cmd::TRANSFER, &[0x01, 0x01]),
            echo(proto::cmd::TRANSFER, &[0x01, 0x01]),
            vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x00, 0x00, 0x00, 0xf0],
            echo(proto::cmd::TRANSFER, &[0x01, 0x01]),
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.init_mem().unwrap();
    }

    #[test]
    fn halt_writes_dhcsr_with_key() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let mut dap = Dap::new(Mock::new(vec![ack.clone(), ack]));
        dap.halt().unwrap();
        assert_eq!(&dap.transport.sent[1][4..8], &0xa05f_0003u32.to_le_bytes());
    }

    #[test]
    fn step_masks_interrupts_in_a_separate_write_then_unmasks() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let halted = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x00, 0x00, 0x02, 0x00];
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            halted,
            ack.clone(),
            ack.clone(),
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.step().unwrap();
        assert_eq!(&dap.transport.sent[1][4..8], &0xa05f_000bu32.to_le_bytes());
        assert_eq!(&dap.transport.sent[3][4..8], &0xa05f_000du32.to_le_bytes());
        assert_eq!(&dap.transport.sent[7][4..8], &0xa05f_0003u32.to_le_bytes());
    }

    #[test]
    fn read_core_reg_selects_then_reads_dcrdr() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let regrdy = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00];
        let value = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0xef, 0xbe, 0xad, 0xde];
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            regrdy,
            ack.clone(),
            value,
        ];
        let mut dap = Dap::new(Mock::new(replies));
        assert_eq!(dap.read_core_reg(15).unwrap(), 0xdead_beef);
    }

    #[test]
    fn write_core_reg_writes_dcrdr_then_dcrsr() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let regrdy = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00];
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            regrdy,
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.write_core_reg(0, 0xcafe_f00d).unwrap();
        assert_eq!(&dap.transport.sent[1][4..8], &0xcafe_f00du32.to_le_bytes());
        assert_eq!(&dap.transport.sent[3][4..8], &0x0001_0000u32.to_le_bytes());
    }

    #[test]
    fn erase_flash_page_drives_nvmc() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let ready = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00];
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready,
            ack.clone(),
            ack,
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.erase_flash_page(0x0003_f000).unwrap();
        assert_eq!(&dap.transport.sent[1][4..8], &2u32.to_le_bytes());
        assert_eq!(&dap.transport.sent[5][4..8], &0x0003_f000u32.to_le_bytes());
    }

    #[test]
    fn write_flash_enables_then_writes_words() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let ready = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00];
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready,
            ack.clone(),
            ack,
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.write_flash(0x0003_f000, &[0xcafe_babe]).unwrap();
        assert_eq!(&dap.transport.sent[1][4..8], &1u32.to_le_bytes());
        assert_eq!(&dap.transport.sent[5][4..8], &0xcafe_babeu32.to_le_bytes());
    }

    #[test]
    fn samd21_erase_row_drives_nvmctrl() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let ready = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00];
        let replies = vec![
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready,
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.erase_flash_row_samd21(0x0000_0100).unwrap();
        assert_eq!(&dap.transport.sent[1][4..8], &0x80u32.to_le_bytes());
        assert_eq!(&dap.transport.sent[3][4..8], &0x0000_a502u32.to_le_bytes());
    }

    #[test]
    fn samd21_write_flash_fills_buffer_then_writes_page() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let ready = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00];
        let ctrlb = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00];
        let flash = vec![proto::cmd::TRANSFER, 0x01, 0x01, 0xff, 0xff, 0xff, 0xff];
        let replies = vec![
            ack.clone(),
            ctrlb,
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            flash,
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ready,
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.write_flash_samd21(0x0, &[0xcafe_babe]).unwrap();
        assert_eq!(&dap.transport.sent[3][4..8], &0x80u32.to_le_bytes());
        assert_eq!(&dap.transport.sent[9][4..8], &0xcafe_babeu32.to_le_bytes());
        assert_eq!(&dap.transport.sent[15][4..8], &0x0000_a504u32.to_le_bytes());
    }

    #[test]
    fn reset_and_run_resets_then_resumes() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let mut dap = Dap::new(Mock::new(vec![ack.clone(), ack.clone(), ack.clone(), ack]));
        dap.reset_and_run().unwrap();
        assert_eq!(&dap.transport.sent[1][4..8], &0x05fa_0004u32.to_le_bytes());
        assert_eq!(
            &dap.transport.sent[3][4..8],
            &(DBGKEY | C_DEBUGEN).to_le_bytes()
        );
    }

    #[test]
    fn set_breakpoint_enables_fpb_and_sets_comp() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let mut dap = Dap::new(Mock::new(vec![ack.clone(), ack.clone(), ack.clone(), ack]));
        dap.set_breakpoint(0x0000_0030).unwrap();
        assert_eq!(&dap.transport.sent[1][4..8], &0b11u32.to_le_bytes());
        let expected = (0b01u32 << 30) | (0x30 & 0x1fff_fffc) | 1;
        assert_eq!(&dap.transport.sent[3][4..8], &expected.to_le_bytes());
    }

    #[test]
    fn set_breakpoints_programs_four_comparators() {
        let ack = echo(proto::cmd::TRANSFER, &[0x01, 0x01]);
        let mut dap = Dap::new(Mock::new(vec![ack; 10]));
        dap.set_breakpoints(&[0x0000_0030, 0x0000_0050]).unwrap();
        let comp0 = (0b01u32 << 30) | (0x30 & 0x1fff_fffc) | 1;
        let comp1 = (0b01u32 << 30) | (0x50 & 0x1fff_fffc) | 1;
        assert_eq!(&dap.transport.sent[3][4..8], &comp0.to_le_bytes());
        assert_eq!(&dap.transport.sent[5][4..8], &comp1.to_le_bytes());
        assert_eq!(&dap.transport.sent[9][4..8], &0u32.to_le_bytes());
    }
}
