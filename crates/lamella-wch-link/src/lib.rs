//! WCH-Link (RV mode) debug-probe host.

mod dm;
pub mod proto;

pub use dm::{Dm, DmError, Dmi, csr, gpr, reg};
pub use proto::WchError;

use core::fmt;

/// The WCH-Link USB vendor id (QinHeng / WCH), in any probe mode.
pub const WCH_VID: u16 = 0x1A86;
/// The WCH-Link USB product id in RV (RISC-V) mode -- the mode whose interface 0 is the vendor bulk
/// debug pipe this crate drives. (DAP/ARM mode enumerates under a different product id.)
pub const WCH_PID_RV: u16 = 0x8010;

/// A byte-packet link to a WCH-Link probe: write a request packet, read its reply. A WinUSB bulk device
/// (endpoints OUT `0x01` / IN `0x81`) is the host implementation; a mock serves tests. (The same
/// two-method shape as `lamella_cmsis_dap::Transport`, but carrying WCH vendor packets, not CMSIS-DAP.)
pub trait Transport {
    /// Sends one request packet to the probe.
    fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError>;
    /// Reads one reply packet into `buf`, returning its length in bytes.
    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
}

/// A byte-transport failure (its message).
#[derive(Debug, Clone)]
pub struct TransportError(pub String);

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TransportError {}

/// The host USB transport: a WCH-Link probe's bulk interface via the shared [`lamella_usbbulk`] backend.
/// Open the device with `lamella_usbbulk::Device::open(WCH_VID, WCH_PID_RV, None)` (it selects the
/// class-0xFF interface's bulk OUT `0x01` / IN `0x81` pipes), then hand it to [`WchLink::new`]. Reads use
/// a fixed one-second timeout, matching the probe's prompt command replies.
#[cfg(feature = "usbbulk")]
impl Transport for lamella_usbbulk::Device {
    fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError> {
        lamella_usbbulk::Device::write_packet(self, data).map_err(|e| TransportError(e.to_string()))
    }

    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        lamella_usbbulk::Device::read_packet(self, buf, core::time::Duration::from_millis(1000))
            .map_err(|e| TransportError(e.to_string()))
    }
}

/// A WCH-Link probe over a byte-packet [`Transport`]. Exposes the vendor commands (firmware version,
/// target attach) and, as a [`Dmi`] link, drives a [`Dm`] RISC-V Debug Module (halt/resume, registers).
pub struct WchLink<T: Transport> {
    transport: T,
    reply: Vec<u8>,
}

impl<T: Transport> WchLink<T> {
    /// Wraps a transport. The reply scratch grows if a command ever needs more than the initial 256 B.
    pub fn new(transport: T) -> Self {
        Self { transport, reply: vec![0; 256] }
    }

    /// Reads the probe firmware version as `(major, minor)`.
    pub fn firmware_version(&mut self) -> Result<(u8, u8), WchError> {
        let n = self.exchange(proto::CMD_CONTROL, &[proto::CTRL_VERSION])?;
        let payload = proto::parse_reply(&self.reply[..n], proto::CMD_CONTROL)?;
        proto::parse_version(payload)
    }

    /// Connects/attaches the target chip, returning the probe's raw attach reply (chip id + info bytes,
    /// interpreted per target elsewhere).
    pub fn attach(&mut self) -> Result<Vec<u8>, WchError> {
        let n = self.exchange(proto::CMD_CONTROL, &[proto::CTRL_ATTACH])?;
        let payload = proto::parse_reply(&self.reply[..n], proto::CMD_CONTROL)?;
        Ok(payload.to_vec())
    }

    /// Borrows this probe as a RISC-V Debug Module (it is the [`Dmi`] link).
    pub fn dm(&mut self) -> Dm<'_, Self> {
        Dm::new(self)
    }

    /// Sends a framed request and reads the reply into `self.reply`, returning its byte length. The
    /// caller re-parses `self.reply[..n]` (keeping the borrow local).
    fn exchange(&mut self, cmd: u8, payload: &[u8]) -> Result<usize, WchError> {
        let request = proto::frame(cmd, payload);
        self.transport
            .write_packet(&request)
            .map_err(|e| WchError::Transport(e.0))?;
        self.transport
            .read_packet(&mut self.reply)
            .map_err(|e| WchError::Transport(e.0))
    }

    /// One DMI operation over `CMD_DMI`: send `[addr, data(BE), op]`, parse `[addr, data(BE), status]`,
    /// and surface a failed status. A read passes `data = 0`; the returned data is the read result.
    fn dmi_op(&mut self, addr: u8, data: u32, op: u8) -> Result<u32, DmError> {
        let payload = proto::dmi_payload(addr, data, op);
        let to_dm = |e: WchError| DmError::Dmi(e.to_string());
        let n = self.exchange(proto::CMD_DMI, &payload).map_err(to_dm)?;
        let reply = proto::parse_reply(&self.reply[..n], proto::CMD_DMI).map_err(to_dm)?;
        let (_, read_data, status) = proto::parse_dmi(reply).map_err(to_dm)?;
        if status == proto::DMI_STATUS_FAIL {
            return Err(DmError::Dmi(format!("DMI op reported status 0x{status:02x}")));
        }
        Ok(read_data)
    }
}

impl<T: Transport> Dmi for WchLink<T> {
    fn dmi_read(&mut self, addr: u8) -> Result<u32, DmError> {
        self.dmi_op(addr, 0, proto::DMI_READ)
    }

    fn dmi_write(&mut self, addr: u8, data: u32) -> Result<(), DmError> {
        self.dmi_op(addr, data, proto::DMI_WRITE).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A mock probe transport modelling a tiny Debug Module: it answers the firmware-version command and
    /// serves `dmi_op` against an in-memory DM register file, so `WchLink` + [`Dm`] exercise end to end.
    #[derive(Default)]
    struct MockProbe {
        halted: bool,
        resumeack: bool,
        data0: u32,
        gprs: BTreeMap<u16, u32>,
        pending: Vec<u8>,
    }

    impl MockProbe {
        fn dmi(&mut self, addr: u8, data: u32, op: u8) -> (u32, u8) {
            match (addr, op) {
                (0x10, 2) => {
                    if data & (1 << 31) != 0 {
                        self.halted = true;
                    }
                    if data & (1 << 30) != 0 {
                        self.halted = false;
                        self.resumeack = true;
                    }
                    (0, 0)
                }
                (0x11, 1) => {
                    let mut s = 2u32;
                    s |= if self.halted { 1 << 9 } else { 1 << 11 };
                    if self.resumeack {
                        s |= 1 << 17;
                    }
                    (s, 0)
                }
                (0x16, 1) => (0, 0),
                (0x04, 2) => {
                    self.data0 = data;
                    (0, 0)
                }
                (0x04, 1) => (self.data0, 0),
                (0x17, 2) => {
                    let regno = (data & 0xffff) as u16;
                    if data & (1 << 16) != 0 {
                        self.gprs.insert(regno, self.data0);
                    } else {
                        self.data0 = *self.gprs.get(&regno).unwrap_or(&0);
                    }
                    (0, 0)
                }
                _ => (0, 0),
            }
        }
    }

    impl Transport for MockProbe {
        fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError> {
            let (cmd, payload) = (data[1], &data[3..]);
            self.pending = match cmd {
                proto::CMD_CONTROL if payload[0] == proto::CTRL_VERSION => {
                    vec![proto::REPLY_OK, cmd, 0x02, 0x02, 0x0B]
                }
                proto::CMD_DMI => {
                    let addr = payload[0];
                    let in_data =
                        u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
                    let (out, status) = self.dmi(addr, in_data, payload[5]);
                    let d = out.to_be_bytes();
                    vec![proto::REPLY_OK, cmd, 0x06, addr, d[0], d[1], d[2], d[3], status]
                }
                _ => vec![proto::REPLY_OK, cmd, 0x00],
            };
            Ok(())
        }

        fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            buf[..self.pending.len()].copy_from_slice(&self.pending);
            Ok(self.pending.len())
        }
    }

    #[test]
    fn reads_the_firmware_version() {
        let mut probe = WchLink::new(MockProbe::default());
        assert_eq!(probe.firmware_version().unwrap(), (0x02, 0x0B));
    }

    #[test]
    fn drives_the_debug_module_end_to_end() {
        let mut probe = WchLink::new(MockProbe::default());
        let mut dm = probe.dm();
        assert_eq!(dm.enable().unwrap(), 2);
        dm.halt().unwrap();
        assert!(dm.is_halted().unwrap());
        dm.write_gpr(15, 0xC0FF_EE00).unwrap();
        assert_eq!(dm.read_gpr(15).unwrap(), 0xC0FF_EE00);
        dm.resume().unwrap();
        assert!(!dm.is_halted().unwrap());
    }
}
