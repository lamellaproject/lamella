//! WCH-Link (RV mode) debug-probe host.

mod dm;
mod flash;
pub mod proto;

pub use dm::{Dm, DmError, Dmi, csr, gpr, reg};
pub use flash::{CH32V003, ChipFlash};
pub use proto::WchError;

use core::fmt;

/// The WCH-Link USB vendor id (QinHeng / WCH), in any probe mode.
pub const WCH_VID: u16 = 0x1A86;
/// The WCH-Link USB product id in RV (RISC-V) mode -- the mode whose interface 0 is the vendor bulk
/// debug pipe this crate drives. (DAP/ARM mode enumerates under a different product id.)
pub const WCH_PID_RV: u16 = 0x8010;

/// The WCH-Link data bulk endpoint (OUT) in RV mode. The fast-program flow streams the RAM flash-loader
/// and the image over this second pipe, separate from the command pair (`0x01`/`0x81`).
pub const DATA_ENDPOINT_OUT: u8 = 0x02;
/// The WCH-Link data bulk endpoint (IN) in RV mode -- fast-program pack acknowledgements arrive here.
pub const DATA_ENDPOINT_IN: u8 = 0x82;

/// A byte-packet link to a WCH-Link probe: write a request packet, read its reply. A WinUSB bulk device
/// (endpoints OUT `0x01` / IN `0x81`) is the host implementation; a mock serves tests. (The same
/// two-method shape as `lamella_cmsis_dap::Transport`, but carrying WCH vendor packets, not CMSIS-DAP.)
pub trait Transport {
    /// Sends one request packet to the probe (the command pair, OUT `0x01`).
    fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError>;
    /// Reads one reply packet into `buf`, returning its length in bytes (the command pair, IN `0x81`).
    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError>;
    /// Sends one packet on the probe's data endpoint (OUT `0x02`) -- the fast-program stream carrying the
    /// RAM flash-loader and the image. The default transport has no data endpoint and errors; a transport
    /// that reaches a real probe overrides this. Only [`WchLink::flash`] needs it.
    fn write_data(&mut self, _data: &[u8]) -> Result<(), TransportError> {
        Err(TransportError("transport has no data endpoint".into()))
    }
    /// Reads one packet from the probe's data endpoint (IN `0x82`) -- a fast-program acknowledgement. The
    /// default transport has no data endpoint and errors.
    fn read_data(&mut self, _buf: &mut [u8]) -> Result<usize, TransportError> {
        Err(TransportError("transport has no data endpoint".into()))
    }
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
/// Open the device with `lamella_probe::open_bulk(WCH_VID, WCH_PID_RV, serial)` (it selects the
/// class-0xFF interface's bulk OUT `0x01` / IN `0x81` pipes), then hand it to [`WchLink::new`]. Reads use
/// a fixed one-second timeout, matching the probe's prompt command replies.
///
/// THE SERIAL IS WHAT SAYS WHICH PROBE. Every WCH-Link enumerates as the same `1a86:8010`, so the
/// serial-less `lamella_usbbulk::Device::open(.., None)` takes whichever interface the OS hands
/// over -- an order that changes with plug order and reboots -- and the caller cannot tell.
/// `open_bulk` resolves it through the selection ladder instead: an explicit serial, then
/// `LAMELLA_PROBE_SERIAL`, then the sole probe of that vid/pid, then a REFUSAL naming every
/// candidate. A reader who copies the line above should get the one that refuses.
#[cfg(feature = "usbbulk")]
impl Transport for lamella_usbbulk::Device {
    fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError> {
        lamella_usbbulk::Device::write_packet(self, data).map_err(|e| TransportError(e.to_string()))
    }

    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        lamella_usbbulk::Device::read_packet(self, buf, core::time::Duration::from_millis(1000))
            .map_err(|e| TransportError(e.to_string()))
    }

    fn write_data(&mut self, data: &[u8]) -> Result<(), TransportError> {
        lamella_usbbulk::Device::write_endpoint(self, DATA_ENDPOINT_OUT, data)
            .map_err(|e| TransportError(e.to_string()))
    }

    fn read_data(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        lamella_usbbulk::Device::read_endpoint(
            self,
            DATA_ENDPOINT_IN,
            buf,
            core::time::Duration::from_millis(3000),
        )
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

    /// Flashes `image` to `chip` at `address`, then resets the target so it runs the new image -- the
    /// full WCH-LinkE native fast-program sequence (set family speed, attach, erase, stream the RAM
    /// flash-loader and the image, reset-and-run, detach). `address` is usually
    /// [`ChipFlash::code_flash_start`].
    pub fn flash(&mut self, chip: &ChipFlash, image: &[u8], address: u32) -> Result<(), WchError> {
        self.set_speed(chip.riscvchip, proto::SPEED_HIGH)?;
        self.attach()?;
        self.erase_flash()?;
        self.write_flash(chip, image, address)?;
        self.reset_run()?;
        self.detach()?;
        Ok(())
    }

    /// Sets the probe<->target clock speed for a chip family ([`proto::CMD_SET_SPEED`]).
    pub fn set_speed(&mut self, riscvchip: u8, speed: u8) -> Result<(), WchError> {
        self.command(proto::CMD_SET_SPEED, &[riscvchip, speed]).map(|_| ())
    }

    /// Detaches/releases the target, ending a program session ([`proto::CTRL_DETACH`]).
    pub fn detach(&mut self) -> Result<(), WchError> {
        self.command(proto::CMD_CONTROL, &[proto::CTRL_DETACH]).map(|_| ())
    }

    /// Resets the target and lets it run, so it boots the freshly flashed image ([`proto::RESET_RUN`]).
    pub fn reset_run(&mut self) -> Result<(), WchError> {
        self.command(proto::CMD_RESET, &[proto::RESET_RUN]).map(|_| ())
    }

    /// Erases the target code flash (refusing a read-protected chip), then re-attaches.
    pub fn erase_flash(&mut self) -> Result<(), WchError> {
        if self.config_query(proto::CONFIG_CHECK_READ_PROTECT)? == proto::FLAG_READ_PROTECTED {
            return Err(WchError::FlashProtected);
        }
        self.program(proto::PROG_ERASE_FLASH)?;
        self.attach()?;
        Ok(())
    }

    /// Streams `image` to flash at `address` via the fast-program flow, without resetting (see
    /// [`WchLink::flash`]): a fresh attach, a not-protected check, set the program window, hand the probe
    /// the RAM flash-loader, then stream the image one pack at a time -- each pack acknowledged.
    pub fn write_flash(&mut self, chip: &ChipFlash, image: &[u8], address: u32) -> Result<(), WchError> {
        self.reattach()?;
        if self.config_query(proto::CONFIG_CHECK_READ_PROTECT)? == proto::FLAG_READ_PROTECTED {
            return Err(WchError::FlashProtected);
        }
        if self.config_query(proto::CONFIG_CHECK_WRITE_PROTECT)? == proto::FLAG_WRITE_PROTECTED {
            return Err(WchError::FlashProtected);
        }

        let window = proto::set_address_payload(address, image.len() as u32);
        self.command(proto::CMD_SET_ADDRESS, &window)?;
        self.program(proto::PROG_WRITE_FLASH_OP)?;
        self.write_data_stream(chip.flash_op, chip.data_packet_size)?;
        let commit = self.program(proto::PROG_COMMIT_FLASH_OP)?;
        if commit != proto::PROG_COMMIT_FLASH_OP {
            return Err(WchError::ProgramReply { expected: proto::PROG_COMMIT_FLASH_OP, got: commit });
        }

        self.program(proto::PROG_WRITE_FLASH)?;
        for pack in image.chunks(chip.write_pack_size) {
            self.write_data_stream(pack, chip.data_packet_size)?;
            let ack = self.read_data_ack()?;
            if ack != proto::FASTPROGRAM_ACK {
                return Err(WchError::FastProgram(ack));
            }
        }
        self.program(proto::PROG_END)?;
        Ok(())
    }

    /// Removes flash protection from the target, then detaches. DESTRUCTIVE when the chip is actually
    /// protected: lifting read-protection makes the probe firmware mass-erase the chip (code flash +
    /// option bytes) -- the security contract that stops protection being removed while keeping the
    /// firmware. On an already-unprotected chip it is a safe no-op (the protection queries report clear).
    /// Mirrors the WCH flow: attach; for read- then write-protection, if set, send the unprotect command
    /// and re-attach so the cleared state takes effect.
    pub fn unprotect(&mut self, chip: &ChipFlash) -> Result<(), WchError> {
        self.set_speed(chip.riscvchip, proto::SPEED_HIGH)?;
        self.attach()?;
        if self.config_query(proto::CONFIG_CHECK_READ_PROTECT)? == proto::FLAG_READ_PROTECTED {
            self.command(proto::CMD_CONFIG, &[proto::CONFIG_UNPROTECT])?;
            self.reattach()?;
            if self.config_query(proto::CONFIG_CHECK_READ_PROTECT)? == proto::FLAG_READ_PROTECTED {
                self.detach()?;
                return Err(WchError::FlashProtected);
            }
        }
        if self.config_query(proto::CONFIG_CHECK_WRITE_PROTECT)? == proto::FLAG_WRITE_PROTECTED {
            self.command(proto::CMD_CONFIG, &proto::unprotect_ex_payload())?;
            self.reattach()?;
        }
        self.detach()?;
        Ok(())
    }

    /// Erases the target code flash by cycling its power (WCH-LinkE, which supplies target power) --
    /// recovers a chip too locked to attach normally. Sets the family speed first, as the erase expects.
    pub fn erase_by_power_off(&mut self, chip: &ChipFlash) -> Result<(), WchError> {
        self.set_speed(chip.riscvchip, proto::SPEED_HIGH)?;
        self.command(proto::CMD_CONTROL, &[proto::CTRL_ERASE_POWER_OFF, chip.riscvchip])
            .map(|_| ())
    }

    /// Erases the target code flash by driving its RST pin (requires a RST wire to the target). Sets the
    /// family speed first, as the erase expects.
    pub fn erase_by_rst_pin(&mut self, chip: &ChipFlash) -> Result<(), WchError> {
        self.set_speed(chip.riscvchip, proto::SPEED_HIGH)?;
        self.command(proto::CMD_CONTROL, &[proto::CTRL_ERASE_RST_PIN, chip.riscvchip])
            .map(|_| ())
    }

    /// Re-attaches the target (detach + attach) so a just-changed protection/option state takes effect.
    fn reattach(&mut self) -> Result<(), WchError> {
        self.detach()?;
        self.attach()?;
        Ok(())
    }

    /// Issues a [`proto::CMD_PROGRAM`] subcommand and returns the probe's status/echo byte.
    fn program(&mut self, sub: u8) -> Result<u8, WchError> {
        let payload = self.command(proto::CMD_PROGRAM, &[sub])?;
        payload.first().copied().ok_or(WchError::ShortReply(0))
    }

    /// Issues a [`proto::CMD_CONFIG`] query subcommand and returns the probe's flag byte.
    fn config_query(&mut self, sub: u8) -> Result<u8, WchError> {
        let payload = self.command(proto::CMD_CONFIG, &[sub])?;
        payload.first().copied().ok_or(WchError::ShortReply(0))
    }

    /// Streams `buf` out the data endpoint in `packet_len`-byte packets, padding the final short packet
    /// to `packet_len` with `0xff` -- the probe expects fixed-size data packets.
    fn write_data_stream(&mut self, buf: &[u8], packet_len: usize) -> Result<(), WchError> {
        let mut packet = vec![0u8; packet_len];
        for chunk in buf.chunks(packet_len) {
            packet[..chunk.len()].copy_from_slice(chunk);
            packet[chunk.len()..].fill(0xff);
            self.transport.write_data(&packet).map_err(|e| WchError::Transport(e.0))?;
        }
        Ok(())
    }

    /// Reads one fast-program acknowledgement from the data endpoint and returns its status byte (the
    /// last of the four-byte ack, e.g. `[0x41, 0x01, 0x01, 0x04]`).
    fn read_data_ack(&mut self) -> Result<u8, WchError> {
        let mut ack = [0u8; 64];
        let n = self.transport.read_data(&mut ack).map_err(|e| WchError::Transport(e.0))?;
        if n < 4 {
            return Err(WchError::ShortReply(n));
        }
        Ok(ack[3])
    }

    /// Sends a framed command and returns its reply payload as an owned `Vec` (validating the header).
    fn command(&mut self, cmd: u8, payload: &[u8]) -> Result<Vec<u8>, WchError> {
        let n = self.exchange(cmd, payload)?;
        Ok(proto::parse_reply(&self.reply[..n], cmd)?.to_vec())
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

    /// Where the mock routes the bytes streamed over the data endpoint: the flash-loader phase (after
    /// `WRITE_FLASH_OP`), the image phase (after `WRITE_FLASH`), or neither.
    #[derive(Default, PartialEq)]
    enum DataPhase {
        #[default]
        Idle,
        FlashOp,
        Image,
    }

    /// Shared state a [`FlashMock`] records so a test can inspect it after [`WchLink::flash`] consumes
    /// the transport: the (command, subcommand) log, the flash-loader and image bytes received on the
    /// data endpoint, the pack-acknowledgement count, and the current reply + data phase.
    #[derive(Default)]
    struct FlashState {
        log: Vec<(u8, u8)>,
        flash_op: Vec<u8>,
        image: Vec<u8>,
        acks: usize,
        phase: DataPhase,
        pending: Vec<u8>,
    }

    /// A probe mock that answers the fast-program command sequence and models the data endpoint: it
    /// replies to each command, routes streamed data into the flash-loader/image buffers by phase, and
    /// acknowledges every fast-program pack with `[0x41, 0x01, 0x01, 0x04]`.
    #[derive(Clone, Default)]
    struct FlashMock {
        state: std::rc::Rc<std::cell::RefCell<FlashState>>,
    }

    impl Transport for FlashMock {
        fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError> {
            let (cmd, sub) = (data[1], if data.len() > 3 { data[3] } else { 0 });
            let mut s = self.state.borrow_mut();
            s.log.push((cmd, sub));
            s.pending = match (cmd, sub) {
                (proto::CMD_SET_SPEED, _) => vec![proto::REPLY_OK, cmd, 0x01, 0x01],
                (proto::CMD_CONTROL, proto::CTRL_ATTACH) => {
                    vec![proto::REPLY_OK, cmd, 0x05, 0x09, 0x00, 0x31, 0x05, 0x00]
                }
                (proto::CMD_CONTROL, proto::CTRL_DETACH) => vec![proto::REPLY_OK, cmd, 0x01, 0xff],
                (proto::CMD_CONFIG, proto::CONFIG_CHECK_READ_PROTECT) => {
                    vec![proto::REPLY_OK, cmd, 0x01, proto::FLAG_READ_UNPROTECTED]
                }
                (proto::CMD_CONFIG, proto::CONFIG_CHECK_WRITE_PROTECT) => {
                    vec![proto::REPLY_OK, cmd, 0x01, 0xff]
                }
                (proto::CMD_SET_ADDRESS, _) => vec![proto::REPLY_OK, cmd, 0x01, 0x01],
                (proto::CMD_PROGRAM, s_byte) => {
                    s.phase = match s_byte {
                        proto::PROG_WRITE_FLASH_OP => DataPhase::FlashOp,
                        proto::PROG_WRITE_FLASH => DataPhase::Image,
                        _ => DataPhase::Idle,
                    };
                    vec![proto::REPLY_OK, cmd, 0x01, s_byte]
                }
                (proto::CMD_RESET, _) => vec![proto::REPLY_OK, cmd, 0x01, 0x01],
                _ => vec![proto::REPLY_OK, cmd, 0x00],
            };
            Ok(())
        }

        fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            let pending = std::mem::take(&mut self.state.borrow_mut().pending);
            buf[..pending.len()].copy_from_slice(&pending);
            Ok(pending.len())
        }

        fn write_data(&mut self, data: &[u8]) -> Result<(), TransportError> {
            let mut s = self.state.borrow_mut();
            match s.phase {
                DataPhase::FlashOp => s.flash_op.extend_from_slice(data),
                DataPhase::Image => s.image.extend_from_slice(data),
                DataPhase::Idle => {}
            }
            Ok(())
        }

        fn read_data(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            self.state.borrow_mut().acks += 1;
            buf[..4].copy_from_slice(&[0x41, 0x01, 0x01, proto::FASTPROGRAM_ACK]);
            Ok(4)
        }
    }

    #[test]
    fn flashes_an_image_through_the_fast_program_sequence() {
        let image: Vec<u8> = (0..1372u32).map(|i| i as u8).collect();
        let mock = FlashMock::default();
        let state = mock.state.clone();
        let mut probe = WchLink::new(mock);

        probe.flash(&CH32V003, &image, CH32V003.code_flash_start).unwrap();

        let s = state.borrow();
        assert_eq!(&s.image[..image.len()], &image[..]);
        assert!(s.image[image.len()..].iter().all(|&b| b == 0xff));
        assert_eq!(s.image.len(), 1024 + 384);
        assert_eq!(&s.flash_op[..CH32V003.flash_op.len()], CH32V003.flash_op);
        assert_eq!(s.flash_op.len(), 512);
        assert_eq!(s.acks, 2);
        assert_eq!(s.log[0], (proto::CMD_SET_SPEED, 0x09));
        assert_eq!(s.log[1], (proto::CMD_CONTROL, proto::CTRL_ATTACH));
        assert!(s.log.contains(&(proto::CMD_PROGRAM, proto::PROG_ERASE_FLASH)));
        assert!(s.log.contains(&(proto::CMD_PROGRAM, proto::PROG_COMMIT_FLASH_OP)));
        assert!(s.log.contains(&(proto::CMD_PROGRAM, proto::PROG_END)));
        assert_eq!(s.log[s.log.len() - 2], (proto::CMD_RESET, proto::RESET_RUN));
        assert_eq!(s.log[s.log.len() - 1], (proto::CMD_CONTROL, proto::CTRL_DETACH));
    }

    /// Shared state a [`ProtectMock`] tracks: whether the chip currently reports read-protected, whether
    /// the unprotect command was issued, how many read-protect queries ran, and the current reply.
    #[derive(Default)]
    struct ProtectState {
        read_protected: bool,
        unprotect_sent: bool,
        read_checks: usize,
        pending: Vec<u8>,
    }

    /// A probe mock modelling flash read-protection: it answers the attach/config queries and, when the
    /// unprotect command arrives (config subcommand `0x02`, length 1), records it and clears protection --
    /// so a later read-protect query reports unprotected, exactly as a real chip does after a mass-erase.
    #[derive(Clone, Default)]
    struct ProtectMock {
        state: std::rc::Rc<std::cell::RefCell<ProtectState>>,
    }

    impl Transport for ProtectMock {
        fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError> {
            let (cmd, len) = (data[1], data[2] as usize);
            let sub = if data.len() > 3 { data[3] } else { 0 };
            let mut s = self.state.borrow_mut();
            s.pending = match (cmd, sub) {
                (proto::CMD_SET_SPEED, _) => vec![proto::REPLY_OK, cmd, 0x01, 0x01],
                (proto::CMD_CONTROL, proto::CTRL_ATTACH) => {
                    vec![proto::REPLY_OK, cmd, 0x05, 0x09, 0x00, 0x31, 0x05, 0x00]
                }
                (proto::CMD_CONTROL, proto::CTRL_DETACH) => vec![proto::REPLY_OK, cmd, 0x01, 0xff],
                (proto::CMD_CONFIG, proto::CONFIG_CHECK_READ_PROTECT) => {
                    s.read_checks += 1;
                    let flag = if s.read_protected {
                        proto::FLAG_READ_PROTECTED
                    } else {
                        proto::FLAG_READ_UNPROTECTED
                    };
                    vec![proto::REPLY_OK, cmd, 0x01, flag]
                }
                (proto::CMD_CONFIG, proto::CONFIG_CHECK_WRITE_PROTECT) => {
                    vec![proto::REPLY_OK, cmd, 0x01, proto::FLAG_WRITE_UNPROTECTED]
                }
                (proto::CMD_CONFIG, proto::CONFIG_UNPROTECT) => {
                    if len == 1 {
                        s.unprotect_sent = true;
                        s.read_protected = false;
                    }
                    vec![proto::REPLY_OK, cmd, 0x01, 0x00]
                }
                _ => vec![proto::REPLY_OK, cmd, 0x00],
            };
            Ok(())
        }

        fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            let pending = std::mem::take(&mut self.state.borrow_mut().pending);
            buf[..pending.len()].copy_from_slice(&pending);
            Ok(pending.len())
        }
    }

    #[test]
    fn unprotect_clears_read_protection() {
        let mock = ProtectMock::default();
        mock.state.borrow_mut().read_protected = true;
        let state = mock.state.clone();
        let mut probe = WchLink::new(mock);

        probe.unprotect(&CH32V003).unwrap();

        let s = state.borrow();
        assert!(s.unprotect_sent);
        assert!(!s.read_protected);
        assert!(s.read_checks >= 2);
    }

    #[test]
    fn unprotect_is_a_noop_on_an_unprotected_chip() {
        let mock = ProtectMock::default();
        let state = mock.state.clone();
        let mut probe = WchLink::new(mock);

        probe.unprotect(&CH32V003).unwrap();

        assert!(!state.borrow().unprotect_sent);
    }
}
