//! An in-memory card answering the [`SdmmcHost`](crate::SdmmcHost) seam at the command level.

use crate::{BusWidth, Response, ResponseKind, SdmmcError, SdmmcHost};
use lamella_sd_core::{SECTOR_LEN, cmd, switch};

/// How many commands the log remembers. The ladder is ten; the rest is headroom for a test that
/// does real transfers afterwards.
pub const LOG_CAP: usize = 64;

/// One logged command: index, argument, and the response kind the driver asked for.
pub type LoggedCommand = (u8, u32, ResponseKind);

/// A fixed-capacity record of what the driver sent.
#[derive(Debug, Clone, Copy)]
pub struct CommandLog {
    entries: [LoggedCommand; LOG_CAP],
    len: usize,
}

impl CommandLog {
    const fn new() -> Self {
        CommandLog { entries: [(0, 0, ResponseKind::None); LOG_CAP], len: 0 }
    }

    fn push(&mut self, entry: LoggedCommand) {
        if self.len < LOG_CAP {
            self.entries[self.len] = entry;
            self.len += 1;
        }
    }

    /// Every command sent, oldest first.
    pub fn iter(&self) -> core::slice::Iter<'_, LoggedCommand> {
        self.entries[..self.len].iter()
    }

    /// How many commands were sent.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing was sent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// A simulated card.
#[derive(Debug)]
pub struct SimCard {
    /// Every command the driver sent, in order.
    pub command_log: CommandLog,
    /// The clock the host was last told to run at.
    pub clock_hz: u32,
    /// The width the host was last told to drive.
    pub width: BusWidth,
    high_capacity: bool,
    answers_cmd8: bool,
    good_check_pattern: bool,
    grants_wide_bus: bool,
    grants_high_speed: bool,
    alive: bool,
    rca: u16,
    csd: [u8; 16],
    /// How many ACMD41 polls remain before the card reports itself ready. Exercises the loop
    /// rather than letting it succeed on the first pass.
    busy_polls: u32,
}

impl SimCard {
    /// The sector count [`SimCard::sdhc`] advertises.
    pub const SDHC_SECTORS: u64 = 62_333_952;

    /// The sector count [`SimCard::sdsc`] advertises through its v1 CSD.
    ///
    pub const SDSC_SECTORS: u64 = 1_921_024;

    /// A modern high-capacity card: answers CMD8, sector-addressed, grants four bits and High
    /// Speed.
    #[must_use]
    pub fn sdhc() -> Self {
        let mut csd = [0u8; 16];
        csd[0] = 0x40;
        csd[7] = 0x00;
        csd[8] = 0xED;
        csd[9] = 0xC8;
        SimCard {
            command_log: CommandLog::new(),
            clock_hz: 0,
            width: BusWidth::One,
            high_capacity: true,
            answers_cmd8: true,
            good_check_pattern: true,
            grants_wide_bus: true,
            grants_high_speed: true,
            alive: true,
            rca: 0xAAAA,
            csd,
            busy_polls: 3,
        }
    }

    /// An older standard-capacity card: does not answer CMD8, and is BYTE-addressed.
    #[must_use]
    pub fn sdsc() -> Self {
        let mut csd = [0u8; 16];
        csd[5] = 0x09;
        csd[6] = 0x03;
        csd[7] = 0xA9;
        csd[8] = 0xC0;
        csd[9] = 0x03;
        csd[10] = 0x80;
        SimCard {
            answers_cmd8: false,
            high_capacity: false,
            csd,
            ..Self::sdhc()
        }
    }

    /// A card that will not widen its bus.
    #[must_use]
    pub fn refusing_wide_bus(mut self) -> Self {
        self.grants_wide_bus = false;
        self
    }

    /// A card that answers CMD6 and declines the function -- the behavior both real cards on this
    /// bench showed over SPI.
    #[must_use]
    pub fn refusing_high_speed(mut self) -> Self {
        self.grants_high_speed = false;
        self
    }

    /// A slot with nothing in it.
    #[must_use]
    pub fn dead(mut self) -> Self {
        self.alive = false;
        self
    }

    /// A card that answers CMD8 without echoing the pattern, i.e. did not understand it.
    #[must_use]
    pub fn with_bad_check_pattern(mut self) -> Self {
        self.good_check_pattern = false;
        self
    }

    /// The switch status this card would return.
    fn switch_status(&self) -> [u8; switch::STATUS_LEN] {
        let mut status = [0u8; switch::STATUS_LEN];
        status[12] = 0x80;
        status[13] = if self.grants_high_speed { 0x03 } else { 0x01 };
        status[16] = if self.grants_high_speed {
            switch::FUNCTION_HIGH_SPEED
        } else {
            switch::NO_INFLUENCE
        };
        status
    }
}

impl SdmmcHost for SimCard {
    type Error = ();

    fn command(
        &mut self,
        index: u8,
        arg: u32,
        kind: ResponseKind,
    ) -> Result<Response, SdmmcError<()>> {
        self.command_log.push((index, arg, kind));
        if !self.alive {
            return Err(SdmmcError::NoResponse);
        }
        if index == cmd::APP_SEND_OP_COND && kind != ResponseKind::ShortNoCrc {
            return Err(SdmmcError::BadCrc);
        }
        let last_was_app = self
            .command_log
            .iter()
            .rev()
            .nth(1)
            .is_some_and(|entry| entry.0 == cmd::APP_CMD);

        match index {
            cmd::GO_IDLE_STATE => Ok(Response::None),
            cmd::SEND_IF_COND if !self.answers_cmd8 => Err(SdmmcError::NoResponse),
            cmd::SEND_IF_COND => {
                let echo = if self.good_check_pattern { arg & 0xFF } else { 0x00 };
                Ok(Response::Short((arg & 0xFFFF_FF00) | echo))
            }
            cmd::APP_CMD => Ok(Response::Short(0)),
            cmd::APP_SEND_OP_COND => {
                if arg & lamella_sd_core::ACMD41_VOLTAGE_WINDOW == 0 {
                    return Ok(Response::Short(0));
                }
                if self.busy_polls > 0 {
                    self.busy_polls -= 1;
                    return Ok(Response::Short(0));
                }
                let ccs = if self.high_capacity { lamella_sd_core::OCR_CCS } else { 0 };
                Ok(Response::Short(lamella_sd_core::OCR_BUSY_COMPLETE | ccs))
            }
            cmd::ALL_SEND_CID => Ok(Response::Long([0x0353_4453, 0x4B33_3247, 0x85C4_C72B, 0xAE01_A547])),
            cmd::SEND_RELATIVE_ADDR => Ok(Response::Short(u32::from(self.rca) << 16)),
            cmd::SEND_CSD => {
                let mut words = [0u32; 4];
                for (i, word) in words.iter_mut().enumerate() {
                    *word = u32::from_be_bytes([
                        self.csd[i * 4],
                        self.csd[i * 4 + 1],
                        self.csd[i * 4 + 2],
                        self.csd[i * 4 + 3],
                    ]);
                }
                Ok(Response::Long(words))
            }
            cmd::SELECT_CARD => Ok(Response::Short(1 << 8)),
            cmd::SWITCH_FUNC if last_was_app => {
                if self.grants_wide_bus && arg == lamella_sd_core::BUS_WIDTH_4BIT {
                    Ok(Response::Short(1 << 8))
                } else {
                    Ok(Response::Short(crate::R1_ERROR_MASK))
                }
            }
            cmd::SWITCH_FUNC => Ok(Response::Short(1 << 8)),
            cmd::READ_SINGLE_BLOCK | cmd::READ_MULTIPLE_BLOCK => Ok(Response::Short(1 << 8)),
            cmd::WRITE_BLOCK | cmd::WRITE_MULTIPLE_BLOCK => Ok(Response::Short(1 << 8)),
            cmd::STOP_TRANSMISSION => Ok(Response::Short(1 << 8)),
            cmd::SEND_STATUS => Ok(Response::Short(1 << 8)),
            _ => Ok(Response::Short(0)),
        }
    }

    fn read_blocks(
        &mut self,
        index: u8,
        arg: u32,
        buf: &mut [u8],
    ) -> Result<(), SdmmcError<()>> {
        self.command(index, arg, ResponseKind::Short)?;
        if index == cmd::SWITCH_FUNC {
            let status = self.switch_status();
            let n = buf.len().min(status.len());
            buf[..n].copy_from_slice(&status[..n]);
            return Ok(());
        }
        for (i, byte) in buf.iter_mut().enumerate() {
            *byte = (i % SECTOR_LEN) as u8;
        }
        Ok(())
    }

    fn write_blocks(&mut self, index: u8, arg: u32, _buf: &[u8]) -> Result<(), SdmmcError<()>> {
        self.command(index, arg, ResponseKind::Short)?;
        Ok(())
    }

    fn set_bus_width(&mut self, width: BusWidth) {
        self.width = width;
    }

    fn set_clock_hz(&mut self, hz: u32) {
        self.clock_hz = hz;
    }

    fn delay_ms(&mut self, _ms: u32) {}
}
