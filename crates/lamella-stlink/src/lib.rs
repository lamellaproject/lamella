//! An ST-Link debug-probe host.

use lamella_probe_core::{CallFrame, CoreMemory, ProbeError, TargetAccess, cortex_m};
use lamella_usbbulk::{Binding, Device};
use std::time::Duration;

/// The device-interface GUID ST's official driver (STSW-LINK009) registers for the debug interface.
pub const ST_DEBUG_INTERFACE_GUID: &str = "{DBCE1CD9-A320-4b51-A365-A0C3F3C5FB29}";

/// STMicroelectronics' USB vendor id.
pub const ST_VENDOR_ID: u16 = 0x0483;

/// Product ids for the ST-Link variants that speak this protocol.
pub mod product_id {
    /// ST-Link/V2 (the standalone dongle).
    pub const V2: u16 = 0x3748;
    /// ST-Link/V2-1 (on-board on Nucleo/Discovery/EVAL boards; composite with MSD + VCP).
    pub const V2_1: u16 = 0x374b;
    /// ST-Link/V3 in its various interface combinations.
    pub const V3E: u16 = 0x374e;
    /// ST-Link/V3S.
    pub const V3S: u16 = 0x374f;
    /// ST-Link/V3 with two virtual COM ports.
    pub const V3_2VCP: u16 = 0x3753;
}

const CMD_GET_VERSION: u8 = 0xf1;
/// The V3's own version query. A V3 answers [`CMD_GET_VERSION`] too, but with its sub-version
/// fields empty, so the old command silently under-reports rather than failing.
const CMD_GET_VERSION_APIV3: u8 = 0xfb;
const CMD_DEBUG: u8 = 0xf2;
const CMD_DFU: u8 = 0xf3;
const CMD_GET_CURRENT_MODE: u8 = 0xf5;
const CMD_GET_TARGET_VOLTAGE: u8 = 0xf7;

const DEBUG_APIV2_ENTER: u8 = 0x30;
const DEBUG_APIV2_READ_IDCODES: u8 = 0x31;
const DEBUG_READMEM_32BIT: u8 = 0x07;
const DEBUG_WRITEMEM_32BIT: u8 = 0x08;
const DEBUG_WRITEMEM_8BIT: u8 = 0x0d;
const DEBUG_APIV2_DRIVE_NRST: u8 = 0x3c;
const DEBUG_EXIT: u8 = 0x21;
/// Sub-command of CMD_DFU: leave firmware-update mode.
const DFU_EXIT: u8 = 0x07;

const NRST_LOW: u8 = 0x00;
const NRST_HIGH: u8 = 0x01;
/// The wire to negotiate on entering debug mode.
const DEBUG_ENTER_SWD: u8 = 0xa3;

/// Status byte a debug sub-command returns when it succeeded.
const DEBUG_ERR_OK: u8 = 0x80;

/// Asks the probe how the LAST read or write actually went.
///
/// This query is not optional bookkeeping: `DEBUG_READMEM_*` and `DEBUG_WRITEMEM_*` report success
/// NOWHERE in their own reply, so without it a failed transfer is indistinguishable from a good
/// one -- the read returns whatever was last in the probe's buffer, and the write returns having
/// done nothing.
const DEBUG_APIV2_GETLASTRWSTATUS: u8 = 0x3b;

const DEBUG_ERR_FAULT: u8 = 0x81;
const DEBUG_ERR_WRITE: u8 = 0x0c;
const DEBUG_ERR_WRITE_VERIFY: u8 = 0x0d;
const DEBUG_ERR_AP_WAIT: u8 = 0x10;
const DEBUG_ERR_AP_FAULT: u8 = 0x11;
const DEBUG_ERR_AP_ERROR: u8 = 0x12;
const DEBUG_ERR_DP_WAIT: u8 = 0x14;
const DEBUG_ERR_DP_FAULT: u8 = 0x15;
const DEBUG_ERR_DP_ERROR: u8 = 0x16;

/// Maps a `GETLASTRWSTATUS` code to the failure it names, or `None` when the transfer completed.
///
/// Split out from the transfer path so the DECISION is testable without a probe: the wiring is
/// two lines, this is where a wrong answer would actually live.
fn rw_status_failure(status: u8) -> Option<&'static str> {
    match status {
        DEBUG_ERR_OK => None,
        DEBUG_ERR_FAULT => Some("the ST-Link reported a transfer fault"),
        DEBUG_ERR_WRITE => Some("the ST-Link reported a memory write error"),
        DEBUG_ERR_WRITE_VERIFY => Some("the ST-Link's post-write verify did not match"),
        DEBUG_ERR_AP_WAIT => Some("the access port answered WAIT and the transfer did not complete"),
        DEBUG_ERR_AP_FAULT => Some("the access port answered FAULT -- the address is unmapped, or the bus refused it"),
        DEBUG_ERR_AP_ERROR => Some("the access port reported an error"),
        DEBUG_ERR_DP_WAIT => Some("the debug port answered WAIT and the transfer did not complete"),
        DEBUG_ERR_DP_FAULT => Some("the debug port answered FAULT -- check target power and reset"),
        DEBUG_ERR_DP_ERROR => Some("the debug port reported an error"),
        _ => Some("the ST-Link reported an unrecognized transfer status"),
    }
}

const MODE_DFU: u8 = 0x00;
const MODE_MASS: u8 = 0x01;
const MODE_DEBUG: u8 = 0x02;

/// Every ST-Link command is sent as a fixed-size packet, short commands zero-padded. Sending the
/// bare opcode alone leaves the firmware waiting for the rest.
const COMMAND_LEN: usize = 16;

/// The most bytes to move in one memory command. The probe's own buffer bounds this, and the
/// length field is 16 bits regardless; 1 KiB keeps well inside both while still amortising the USB
/// round trip over a useful block.
const MAX_TRANSFER: usize = 1024;

/// How long to wait for a reply before calling the probe unresponsive.
const REPLY_TIMEOUT: Duration = Duration::from_millis(1000);

/// Which operating mode the probe is currently in. It boots into whichever mode it was left in, so
/// a probe sitting in DFU or mass-storage mode must be switched to debug before it will answer
/// debug commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Firmware-update mode.
    Dfu,
    /// Mass-storage (drag-drop programming) mode.
    Mass,
    /// Debug mode -- the one that answers SWD/JTAG commands.
    Debug,
    /// A mode this crate does not name, carried verbatim.
    Other(u8),
}

impl Mode {
    fn from_byte(byte: u8) -> Self {
        match byte {
            MODE_DFU => Mode::Dfu,
            MODE_MASS => Mode::Mass,
            MODE_DEBUG => Mode::Debug,
            other => Mode::Other(other),
        }
    }
}

/// The probe's firmware version. Read via `GET_VERSION` on a V2-family probe and via the V3's own
/// `GET_VERSION_APIV3` on a V3, which report the same facts in different shapes.
#[derive(Debug, Clone, Copy)]
pub struct Version {
    /// ST-Link hardware generation (2 for V2 and V2-1; a V3 reports 3 and has a richer command).
    pub stlink: u8,
    /// JTAG/SWD firmware version -- what actually gates which debug commands exist.
    pub jtag: u8,
    /// SWIM firmware version (0 on a probe with no SWIM support).
    pub swim: u8,
    /// USB vendor id the probe reports for itself.
    pub vendor_id: u16,
    /// USB product id the probe reports for itself.
    pub product_id: u16,
}

/// A connected ST-Link.
pub struct StLink {
    device: Device,
}

impl StLink {
    /// Opens an ST-Link by product id, optionally selecting one of several by serial.
    ///
    /// On a failure this consults [`diagnose`] first, so an unbound debug interface reports what to
    /// do about it instead of a bare "not found".
    pub fn open(product_id: u16, serial: Option<&str>) -> Result<Self, ProbeError> {
        match lamella_usbbulk::diagnose(ST_DEBUG_INTERFACE_GUID, ST_VENDOR_ID, product_id) {
            Ok(Binding::Absent) => {
                return Err(ProbeError::Device(
                    "no ST-Link found on the USB bus -- plug one in, or an ST board with one on board",
                ));
            }
            Ok(Binding::PresentUnbound) => {
                return Err(ProbeError::Device(
                    "an ST-Link is plugged in but its debug interface has NO DRIVER BOUND, so it \
                     cannot be opened. ST ships no MS-OS descriptors, so unlike a CMSIS-DAP v2 probe \
                     it will not bind WinUSB by itself. Install ST's official driver (STSW-LINK009 \
                     on st.com) -- it is WinUSB-based, so we can open the interface and ST's own \
                     tools keep working; or install a Lamella-signed WinUSB INF with `pnputil \
                     /add-driver`, which displaces ST's driver on that probe",
                ));
            }
            Ok(Binding::Bound) => {}
            Err(_) => {}
        }
        let device = Device::open_interface(ST_DEBUG_INTERFACE_GUID, ST_VENDOR_ID, product_id, serial)
            .map_err(|_| ProbeError::Device("could not open the ST-Link debug interface"))?;
        Ok(StLink { device })
    }

    /// Sends a command and reads `reply_len` bytes back. `command` is zero-padded to the packet size
    /// the firmware expects.
    fn transact(&mut self, command: &[u8], reply_len: usize) -> Result<Vec<u8>, ProbeError> {
        let mut packet = [0u8; COMMAND_LEN];
        let n = command.len().min(COMMAND_LEN);
        packet[..n].copy_from_slice(&command[..n]);
        self.device
            .write_packet(&packet)
            .map_err(|_| ProbeError::Device("ST-Link command write failed"))?;
        if reply_len == 0 {
            return Ok(Vec::new());
        }
        let mut buf = vec![0u8; reply_len];
        let got = self
            .device
            .read_packet(&mut buf, REPLY_TIMEOUT)
            .map_err(|_| ProbeError::Device("ST-Link reply read failed or timed out"))?;
        if got < reply_len {
            return Err(ProbeError::Device("ST-Link reply was shorter than expected"));
        }
        buf.truncate(got);
        Ok(buf)
    }

    /// Reads the probe's firmware version.
    ///
    /// The reply packs three versions into a BIG-endian 16-bit word (the rest of the protocol is
    /// little-endian, which is exactly the kind of detail worth stating): bits 15:12 the ST-Link
    /// generation, 11:6 the JTAG/SWD firmware, 5:0 SWIM. The ids that follow are little-endian.
    pub fn version(&mut self) -> Result<Version, ProbeError> {
        let reply = self.transact(&[CMD_GET_VERSION], 6)?;
        let word = u16::from_be_bytes([reply[0], reply[1]]);
        let generation = ((word >> 12) & 0x0f) as u8;
        if generation < 3 {
            return Ok(Version {
                stlink: generation,
                jtag: ((word >> 6) & 0x3f) as u8,
                swim: (word & 0x3f) as u8,
                vendor_id: u16::from_le_bytes([reply[2], reply[3]]),
                product_id: u16::from_le_bytes([reply[4], reply[5]]),
            });
        }

        let reply = self.transact(&[CMD_GET_VERSION_APIV3], 12)?;
        Ok(Version {
            stlink: reply[0],
            vendor_id: u16::from_le_bytes([reply[8], reply[9]]),
            product_id: u16::from_le_bytes([reply[10], reply[11]]),
            swim: reply[1],
            jtag: reply[2],
        })
    }

    /// Reads which mode the probe is currently in.
    pub fn current_mode(&mut self) -> Result<Mode, ProbeError> {
        let reply = self.transact(&[CMD_GET_CURRENT_MODE], 2)?;
        Ok(Mode::from_byte(reply[0]))
    }

    /// Leaves firmware-update (DFU) mode.
    ///
    /// A STANDALONE ST-Link/V2 dongle boots into DFU, where the debug commands do not exist -- even
    /// GET_TARGET_VOLTAGE times out. On-board V2-1s boot serving their mass-storage drive instead,
    /// so this is the difference between the variants that actually bites.
    pub fn exit_dfu(&mut self) -> Result<(), ProbeError> {
        self.transact(&[CMD_DFU, DFU_EXIT], 0)?;
        Ok(())
    }

    /// Switches the probe into debug mode over SWD.
    ///
    /// Not a formality: an ST-Link boots into whichever mode it was last left in -- an on-board one
    /// serves its drag-drop drive, a standalone dongle sits in DFU -- and answers no debug command
    /// until switched. Leaves DFU first when found there, since the debug command that would do the
    /// switching is itself unavailable in DFU. Idempotent.
    pub fn enter_swd(&mut self) -> Result<(), ProbeError> {
        if matches!(self.current_mode(), Ok(Mode::Dfu)) {
            self.exit_dfu()?;
        }
        let reply = self.transact(&[CMD_DEBUG, DEBUG_APIV2_ENTER, DEBUG_ENTER_SWD], 2)?;
        if reply[0] != DEBUG_ERR_OK {
            return Err(ProbeError::Device("ST-Link refused to enter SWD debug mode"));
        }
        Ok(())
    }

    /// Reads the target's debug-port IDCODE -- the first thing that proves a TARGET is on the other
    /// end of the wire, as opposed to merely a working probe.
    ///
    /// Requires debug mode; call [`enter_swd`](Self::enter_swd) first.
    pub fn read_idcode(&mut self) -> Result<u32, ProbeError> {
        let reply = self.transact(&[CMD_DEBUG, DEBUG_APIV2_READ_IDCODES], 12)?;
        if reply[0] != DEBUG_ERR_OK {
            return Err(ProbeError::Device(
                "ST-Link could not read the target IDCODE -- is a target connected and powered?",
            ));
        }
        Ok(u32::from_le_bytes([reply[4], reply[5], reply[6], reply[7]]))
    }

    /// Reads `byte_len` bytes of target memory from `address`.
    ///
    /// This is where a high-level probe earns its place in the design: the ADIv5 MEM-AP dance a
    /// CMSIS-DAP host performs per access -- select the AP, set CSW, write TAR, read DRW -- happens
    /// inside the ST-Link's firmware, so a block read is ONE USB round trip rather than several per
    /// word. That is exactly why `TargetAccess` is the seam and `DapAccess` is not: forcing an
    /// ST-Link to expose DP/AP registers would throw this away.
    ///
    /// Address and length must both be 32-bit aligned; the command is a word-access primitive.
    ///
    /// The read is FOLLOWED BY A STATUS QUERY, and that is load-bearing rather than diligence. The
    /// reply to `DEBUG_READMEM_32BIT` carries no success indication at all, so an AP fault -- an
    /// unmapped address, a powered-down bus, a target in reset -- returns the probe's PREVIOUS
    /// buffer contents. That failure is silent, data-dependent and maximally misleading: the bytes
    /// are a real register value, just the wrong one, so they do not look wrong. It reached us as a
    /// stall triage where four fault registers all read back as the DHCSR sampled just before them.
    pub fn read_mem32(&mut self, address: u32, byte_len: u16) -> Result<Vec<u8>, ProbeError> {
        if address % 4 != 0 || byte_len % 4 != 0 {
            return Err(ProbeError::Device("32-bit memory access must be word-aligned in address and length"));
        }
        let address_bytes = address.to_le_bytes();
        let len = byte_len.to_le_bytes();
        let command = [
            CMD_DEBUG,
            DEBUG_READMEM_32BIT,
            address_bytes[0], address_bytes[1], address_bytes[2], address_bytes[3],
            len[0], len[1],
        ];
        let data = self.transact(&command, byte_len as usize)?;
        self.check_last_rw_status()?;
        Ok(data)
    }

    /// Asks the probe whether the last read or write completed, and turns "no" into an error.
    ///
    /// Uses `GETLASTRWSTATUS` (APIv2) rather than the newer `GETLASTRWSTATUS2`: the whole crate
    /// already speaks APIv2 unconditionally, whereas the wider query sits behind a firmware feature
    /// flag we do not currently interrogate. The 12-byte form additionally reports the faulting
    /// ADDRESS, which is worth having later -- but not at the cost of a version check this fix does
    /// not otherwise need.
    fn check_last_rw_status(&mut self) -> Result<(), ProbeError> {
        let reply = self.transact(&[CMD_DEBUG, DEBUG_APIV2_GETLASTRWSTATUS], 2)?;
        match rw_status_failure(reply[0]) {
            None => Ok(()),
            Some(reason) => Err(ProbeError::Device(reason)),
        }
    }

    /// Writes `data` to target memory at `address`.
    ///
    /// Two phases on the wire: the command packet names the address and length, then the payload
    /// follows as its own bulk transfer. Neither phase replies, so the write is confirmed by the
    /// same status query the reads use -- without it a refused write completes silently and the
    /// caller has no way to know the memory never changed.
    pub fn write_mem32(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        if address % 4 != 0 || data.len() % 4 != 0 {
            return Err(ProbeError::Device("32-bit memory access must be word-aligned in address and length"));
        }
        let byte_len = u16::try_from(data.len())
            .map_err(|_| ProbeError::Device("a single ST-Link memory write cannot exceed 65535 bytes"))?;
        let address_bytes = address.to_le_bytes();
        let len = byte_len.to_le_bytes();
        let command = [
            CMD_DEBUG,
            DEBUG_WRITEMEM_32BIT,
            address_bytes[0], address_bytes[1], address_bytes[2], address_bytes[3],
            len[0], len[1],
        ];
        self.transact(&command, 0)?;
        self.device
            .write_packet(data)
            .map_err(|_| ProbeError::Device("ST-Link memory write payload failed"))?;
        self.check_last_rw_status()
    }

    /// Writes bytes to target memory with the native 8-bit command -- a TRUE byte-wide bus access,
    /// unlike emulating one by read-modify-writing a word.
    pub fn write_mem8(&mut self, address: u32, data: &[u8]) -> Result<(), ProbeError> {
        let byte_len = u16::try_from(data.len())
            .map_err(|_| ProbeError::Device("a single ST-Link memory write cannot exceed 65535 bytes"))?;
        let address_bytes = address.to_le_bytes();
        let len = byte_len.to_le_bytes();
        let command = [
            CMD_DEBUG,
            DEBUG_WRITEMEM_8BIT,
            address_bytes[0], address_bytes[1], address_bytes[2], address_bytes[3],
            len[0], len[1],
        ];
        self.transact(&command, 0)?;
        self.device
            .write_packet(data)
            .map_err(|_| ProbeError::Device("ST-Link byte-write payload failed"))?;
        self.check_last_rw_status()
    }

    /// Reads one 32-bit word of target memory.
    pub fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        let bytes = self.read_mem32(address, 4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Writes one 32-bit word of target memory.
    pub fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        self.write_mem32(address, &value.to_le_bytes())
    }

    /// Leaves debug mode, RELEASING the SWD wire.
    ///
    /// Worth calling rather than just dropping the handle. A probe in debug mode drives SWCLK and
    /// SWDIO, so on a board where an on-board probe and an external header SHARE those pins -- an ST
    /// EVAL or Nucleo, say -- an ST-Link left in debug mode makes the target unreachable to anything
    /// else. Leaving debug mode hands the wire back.
    pub fn exit_debug(&mut self) -> Result<(), ProbeError> {
        self.transact(&[CMD_DEBUG, DEBUG_EXIT], 0)?;
        Ok(())
    }

    /// Drives the target reset line: `assert` holds the core in reset, clearing it releases.
    ///
    /// Returns 0 rather than a pin read-back -- unlike a CMSIS-DAP probe, an ST-Link reports no pin
    /// state here, so a caller cannot use the return value to sense the line. Callers that need to
    /// KNOW the line moved must observe the target instead.
    pub fn drive_nrst(&mut self, assert: bool) -> Result<u8, ProbeError> {
        let level = if assert { NRST_LOW } else { NRST_HIGH };
        let reply = self.transact(&[CMD_DEBUG, DEBUG_APIV2_DRIVE_NRST, level], 2)?;
        if reply[0] != DEBUG_ERR_OK {
            return Err(ProbeError::Device("ST-Link refused to drive nRST"));
        }
        Ok(0)
    }

    /// Measures the TARGET's supply voltage, in volts.
    ///
    /// The probe returns two ADC readings rather than a voltage: a reading of its own 1.2 V internal
    /// reference, and one of the target rail through a divide-by-two. Scaling the second by the
    /// first cancels the ADC's own reference error, which is why it is reported this way. A zero
    /// reference reading means the measurement is unavailable rather than that the target is at 0 V.
    pub fn target_voltage(&mut self) -> Result<f32, ProbeError> {
        let reply = self.transact(&[CMD_GET_TARGET_VOLTAGE], 8)?;
        let reference = u32::from_le_bytes([reply[0], reply[1], reply[2], reply[3]]);
        let target = u32::from_le_bytes([reply[4], reply[5], reply[6], reply[7]]);
        if reference == 0 {
            return Err(ProbeError::Device("ST-Link returned no ADC reference reading"));
        }
        Ok(2.0 * target as f32 * 1.2 / reference as f32)
    }
}

/// Memory is all the shared Cortex-M run control needs; everything else it derives.
impl CoreMemory for StLink {
    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        StLink::read_word(self, address)
    }

    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        StLink::write_word(self, address, value)
    }

    fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError> {
        StLink::drive_nrst(self, assert)
    }
}

/// The payoff of the two-seam split: an ST-Link implements [`TargetAccess`] DIRECTLY -- no
/// `DapAccess`, no MEM-AP bridge -- because its firmware already performs that layer. Every flash
/// algorithm and diagnostic written against the seam therefore drives a target through an ST-Link
/// with no change at all.
///
/// Note where the work actually differs from the CMSIS-DAP path, and where it does not. MEMORY is
/// native here: one USB command per block, versus the per-access MEM-AP sequence `ArmDap` performs.
/// RUN CONTROL is not native and does not need to be -- halting, stepping, core registers and
/// breakpoints are all reads and writes of Cortex-M debug registers, so this delegates to the
/// SHARED [`cortex_m`] logic rather than reimplementing it.
impl TargetAccess for StLink {
    fn connect(&mut self) -> Result<(), ProbeError> {
        self.enter_swd()
    }

    fn read_idcode(&mut self) -> Result<u32, ProbeError> {
        StLink::read_idcode(self)
    }

    fn init_mem(&mut self) -> Result<(), ProbeError> {
        Ok(())
    }

    fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
        StLink::read_word(self, address)
    }

    fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
        StLink::write_word(self, address, value)
    }

    fn read_words(&mut self, address: u32, count: usize) -> Result<Vec<u32>, ProbeError> {
        let mut out = Vec::with_capacity(count);
        let mut address = address;
        let mut remaining = count;
        while remaining > 0 {
            let batch = remaining.min(MAX_TRANSFER / 4);
            let bytes = self.read_mem32(address, (batch * 4) as u16)?;
            out.extend(
                bytes
                    .chunks_exact(4)
                    .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]])),
            );
            address += (batch * 4) as u32;
            remaining -= batch;
        }
        Ok(out)
    }

    fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
        let mut address = address;
        for chunk in words.chunks(MAX_TRANSFER / 4) {
            let bytes: Vec<u8> = chunk.iter().flat_map(|word| word.to_le_bytes()).collect();
            self.write_mem32(address, &bytes)?;
            address += (chunk.len() * 4) as u32;
        }
        Ok(())
    }

    fn read_byte(&mut self, address: u32) -> Result<u8, ProbeError> {
        let word = StLink::read_word(self, address & !3)?;
        Ok((word >> (8 * (address & 3))) as u8)
    }

    fn write_byte(&mut self, address: u32, value: u8) -> Result<(), ProbeError> {
        self.write_mem8(address, &[value])
    }

    fn read_halfword(&mut self, address: u32) -> Result<u16, ProbeError> {
        let word = StLink::read_word(self, address & !3)?;
        Ok((word >> (8 * (address & 2))) as u16)
    }

    /// NOT a single 16-bit bus transaction -- see the note on this impl block. The ST-Link command
    /// set has no 16-bit access, so this is two native BYTE writes: correct and non-disturbing for
    /// ordinary memory, but not equivalent to one half-word bus cycle.
    fn write_halfword(&mut self, address: u32, value: u16) -> Result<(), ProbeError> {
        self.write_mem8(address, &value.to_le_bytes())
    }

    fn halt(&mut self) -> Result<(), ProbeError> {
        cortex_m::halt(self)
    }

    fn resume(&mut self) -> Result<(), ProbeError> {
        cortex_m::resume(self)
    }

    fn step(&mut self) -> Result<(), ProbeError> {
        cortex_m::step(self)
    }

    fn is_halted(&mut self) -> Result<bool, ProbeError> {
        cortex_m::is_halted(self)
    }

    fn wait_halted(&mut self) -> Result<(), ProbeError> {
        cortex_m::wait_halted(self)
    }

    fn reset_and_run(&mut self) -> Result<(), ProbeError> {
        cortex_m::reset_and_run(self)
    }

    fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
        cortex_m::reset_and_halt(self)
    }

    fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError> {
        self.drive_nrst(assert)
    }

    fn read_core_reg(&mut self, selector: u8) -> Result<u32, ProbeError> {
        cortex_m::read_core_reg(self, selector)
    }

    fn write_core_reg(&mut self, selector: u8, value: u32) -> Result<(), ProbeError> {
        cortex_m::write_core_reg(self, selector, value)
    }

    fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
        cortex_m::arm_reset_catch(self)
    }

    fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
        cortex_m::disarm_reset_catch(self)
    }

    fn set_breakpoint(&mut self, address: u32) -> Result<(), ProbeError> {
        cortex_m::set_breakpoint(self, address)
    }

    fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
        cortex_m::clear_breakpoint(self)
    }

    fn set_breakpoints(&mut self, addresses: &[u32]) -> Result<(), ProbeError> {
        cortex_m::set_breakpoints(self, addresses)
    }

    fn call_target(&mut self, address: u32, args: &[u32], frame: &CallFrame) -> Result<u32, ProbeError> {
        cortex_m::call_target(self, address, args, frame)
    }
}

/// Classifies whether an ST-Link is reachable, and if not, why. See
/// [`lamella_usbbulk::diagnose`].
pub fn diagnose(product_id: u16) -> Result<Binding, ProbeError> {
    lamella_usbbulk::diagnose(ST_DEBUG_INTERFACE_GUID, ST_VENDOR_ID, product_id)
        .map_err(|_| ProbeError::Device("could not classify the ST-Link's driver binding"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_word_unpacks_big_endian_fields() {
        let word = 0x2436u16;
        assert_eq!((word >> 12) & 0x0f, 2);
        assert_eq!((word >> 6) & 0x3f, 16);
        assert_eq!(word & 0x3f, 54);
    }


    #[test]
    fn ok_status_is_the_only_success() {
        assert!(rw_status_failure(DEBUG_ERR_OK).is_none());
    }

    #[test]
    fn every_failure_code_is_reported_as_a_failure() {
        for status in [
            DEBUG_ERR_FAULT,
            DEBUG_ERR_WRITE,
            DEBUG_ERR_WRITE_VERIFY,
            DEBUG_ERR_AP_WAIT,
            DEBUG_ERR_AP_FAULT,
            DEBUG_ERR_AP_ERROR,
            DEBUG_ERR_DP_WAIT,
            DEBUG_ERR_DP_FAULT,
            DEBUG_ERR_DP_ERROR,
        ] {
            assert!(
                rw_status_failure(status).is_some(),
                "status 0x{status:02x} must not be treated as success"
            );
        }
    }

    #[test]
    fn an_unrecognized_status_is_a_failure_not_a_pass() {
        for status in [0x00u8, 0x01, 0x7f, 0x82, 0xff] {
            assert!(
                rw_status_failure(status).is_some(),
                "unrecognized status 0x{status:02x} must not be treated as success"
            );
        }
    }

    #[test]
    fn mode_bytes_decode() {
        assert_eq!(Mode::from_byte(0x00), Mode::Dfu);
        assert_eq!(Mode::from_byte(0x01), Mode::Mass);
        assert_eq!(Mode::from_byte(0x02), Mode::Debug);
        assert_eq!(Mode::from_byte(0x42), Mode::Other(0x42));
    }
}
