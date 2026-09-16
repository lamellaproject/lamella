//! An ST-Link debug-probe host.

use lamella_probe_core::{CallFrame, CoreMemory, ProbeError, TargetAccess, cortex_m, selection};
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
    /// The second id ST assigns to the ST-LINK/V2-1 generation (TN1235: "374B or 3752 for
    /// ST-LINK/V2-1"). One such probe is an **STLINK-V2EC** -- the V2 generation's USB Type-C
    /// variant, fitted to the most recent boards of that generation.
    ///
    /// Measured on a NUCLEO-C071RB before the document was found, and the two agree: the probe
    /// reports generation 2 (JTAG v45 / SWIM v30) and enumerates a debug interface and a virtual
    /// COM port and nothing else. **That board offers NO drag-drop volume, so SWD is the only way
    /// to program it** -- TN1235 lists mass storage as OPTIONAL for a V2EC, and this one ships with
    /// it off. The mass-storage fallback every other on-board ST-Link here provides does not exist.
    pub const V2EC: u16 = 0x3752;
    /// An STLINK-V3 WITHOUT bridge functions -- the form embedded on demonstration boards.
    ///
    /// THIS ID CANNOT TELL AN **STLINK-V3E** FROM AN **STLINK-V3EC**, and no amount of
    /// enumeration will. TN1235 assigns product ids by whether the BRIDGE is present ("374E or 3754
    /// for STLINK-V3 without bridge functions"), while V3E and V3EC differ by the board's USB
    /// connector -- a V3EC is the variant "managing a USB Type-C connection". Both report
    /// generation 3. Measured before the document was found: a NUCLEO-H755ZI-Q and a
    /// NUCLEO-U5A5ZJ-Q present this id with identical interface layouts, differing only in firmware
    /// version. **So a directive that asks for "V3E and V3EC" separately is asking for a
    /// distinction the USB bus does not carry; the connector on the board is the discriminator.**
    pub const V3E: u16 = 0x374e;
    /// The second id for an STLINK-V3 without bridge functions (TN1235).
    pub const V3_NO_BRIDGE_ALT: u16 = 0x3754;
    /// An STLINK-V3 WITH bridge functions -- the standalone probes (V3SET, V3MODS), which also
    /// carry a second virtual COM port.
    pub const V3S: u16 = 0x374f;
    /// The second id for an STLINK-V3 with bridge functions (TN1235).
    ///
    /// This was previously named for carrying two virtual COM ports, which is a CONSEQUENCE of
    /// being a bridge-capable standalone probe rather than what the id means.
    pub const V3_BRIDGE_ALT: u16 = 0x3753;
    /// STLINK-V3PWR: a V3-family debug probe with an energy-measurement channel.
    ///
    /// Read off the device rather than recalled: it enumerates as a composite whose second virtual
    /// COM port names itself `STLink Virtual COM Port PWR`, which is the probe stating what it is.
    /// The DEBUG interface is an ordinary V3 one, so nothing above this constant changes.
    ///
    /// IT REPORTS MAJOR VERSION **4**, NOT 3, AND THAT IS CORRECT. A V3 reported `4` when read
    /// and it was recorded as a suspected mis-decode of the APIv3 version reply; TN1235 settles it
    /// the other way -- the major version ID is "4 for STLINK-V3PWR". Both gates that read this
    /// field test `>=`, so the behaviour was right either way, but the doubt is retired: a V3PWR
    /// really is a later major version than the rest of the V3 family.
    pub const V3PWR: u16 = 0x3757;

    /// Every V3-generation product id, so a caller can search the family rather than one member.
    ///
    /// DECLARED ONCE BECAUSE THE ALTERNATIVE ALREADY WENT WRONG TWICE. `--v3` once meant `V3E`
    /// alone and answered "no ST-Link found on the USB bus -- plug one in" at a V3S that was
    /// plugged in and enumerating; the list then lived in one example while the crate held the
    /// constants, and a newly released V3PWR was invisible to every tool that had its own
    /// copy. A family is a fact about the hardware, so it belongs beside the ids and not beside a
    /// command-line flag.
    pub const V3_FAMILY: [u16; 5] = [V3E, V3_NO_BRIDGE_ALT, V3S, V3_BRIDGE_ALT, V3PWR];
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
/// APIv3 only: set / read the communication frequency. A V3 has no default and refuses debug mode
/// until one is set -- see [`StLink::enter_swd`].
const DEBUG_APIV3_SET_COM_FREQ: u8 = 0x61;
const DEBUG_APIV3_GET_COM_FREQ: u8 = 0x62;
/// The wire the APIv3 frequency commands are being asked about (0 = SWD, 1 = JTAG).
const COM_FREQ_MODE_SWD: u8 = 0x00;
/// Sub-command of CMD_DFU: leave firmware-update mode.
const DFU_EXIT: u8 = 0x07;

const NRST_LOW: u8 = 0x00;
const NRST_HIGH: u8 = 0x01;
/// The wire to negotiate on entering debug mode.
const DEBUG_ENTER_SWD: u8 = 0xa3;

/// Status byte a debug sub-command returns when it succeeded.
const DEBUG_ERR_OK: u8 = 0x80;

/// The fastest SWD rate [`StLink::enter_swd`] will negotiate on a V3, in kHz.
///
/// A ceiling, not a target: the probe's own list decides the value and this only bounds it. Bring-up
/// runs over flying leads as often as over a board trace, and the fastest rate a probe can name is
/// not one every wire can carry -- a marginal clock fails as unreliable reads, which is a far worse
/// failure than a slower link.
const MAX_NEGOTIATED_KHZ: u32 = 4000;

/// Asks the probe how the LAST read or write actually went.
///
/// This query is not optional bookkeeping: `DEBUG_READMEM_*` and `DEBUG_WRITEMEM_*` report success
/// NOWHERE in their own reply, so without it a failed transfer is indistinguishable from a good
/// one -- the read returns whatever was last in the probe's buffer, and the write returns having
/// done nothing.
///
/// A V3 DOES NOT ANSWER THIS FORM, AND DOES NOT SAY SO. Asked on an ST-Link/V3 it returns `0x42`
/// -- to a good read, to a failed read, and before any transfer has happened at all. See
/// [`DEBUG_APIV2_GETLASTRWSTATUS2`] and [`wide_status_supported`].
const DEBUG_APIV2_GETLASTRWSTATUS: u8 = 0x3b;

/// The wider transfer-status query, and the one every current probe actually answers.
///
/// Same status byte, in a 12-byte reply that additionally carries the FAULTING ADDRESS at offset 4.
const DEBUG_APIV2_GETLASTRWSTATUS2: u8 = 0x3e;

/// The oldest V2 JTAG/SWD firmware that answers [`DEBUG_APIV2_GETLASTRWSTATUS2`].
const FIRST_JTAG_WITH_WIDE_STATUS: u8 = 15;

const DEBUG_ERR_FAULT: u8 = 0x81;
const DEBUG_ERR_GET_IDCODE: u8 = 0x09;
const DEBUG_ERR_WRITE: u8 = 0x0c;
const DEBUG_ERR_WRITE_VERIFY: u8 = 0x0d;
const DEBUG_ERR_AP_WAIT: u8 = 0x10;
const DEBUG_ERR_AP_FAULT: u8 = 0x11;
const DEBUG_ERR_AP_ERROR: u8 = 0x12;
const DEBUG_ERR_AP_PARITY: u8 = 0x13;
const DEBUG_ERR_DP_WAIT: u8 = 0x14;
const DEBUG_ERR_DP_FAULT: u8 = 0x15;
const DEBUG_ERR_DP_ERROR: u8 = 0x16;
const DEBUG_ERR_DP_PARITY: u8 = 0x17;
const DEBUG_ERR_AP_WDATA: u8 = 0x18;
const DEBUG_ERR_AP_STICKY: u8 = 0x19;
const DEBUG_ERR_AP_STICKY_OVERRUN: u8 = 0x1a;
/// The access port named in the transfer does not exist on this target. The one code here that was
/// observed rather than taken from the reference: reading through access port 7 of an STM32H747
/// answers exactly this, and names the address it refused.
const DEBUG_ERR_BAD_AP: u8 = 0x1d;

/// Whether a probe of this firmware vintage answers [`DEBUG_APIV2_GETLASTRWSTATUS2`].
///
/// THE GENERATION IS CHECKED FIRST AND THE JTAG FIELD IS NOT CONSULTED ON A V3, WHICH IS THE
/// WHOLE POINT OF WRITING IT THIS WAY. A V3's JTAG/SWD sub-version is not the V2 counter continued
/// -- a V3S reports `1` -- so a plain `jtag >= 15` test rejects the very probes that ONLY
/// answer the wide form, and does it silently.
///
/// Split out from the transfer path so the DECISION is testable without a probe, the same reason
/// [`rw_status_failure`] is.
fn wide_status_supported(stlink: u8, jtag: u8) -> bool {
    stlink >= 3 || jtag >= FIRST_JTAG_WITH_WIDE_STATUS
}

/// What a flash run concluded, from the two observations that can disagree about it.
///
/// **A PROGRAM STEP AND A READ-BACK ARE SEPARATE OBSERVATIONS, AND EITHER CAN BE WRONG ABOUT THE
/// OTHER.** Reporting one and inferring the other is what makes a flash tool claim a board needs
/// re-writing when the bytes are already on it -- a transport that complains after the last word
/// has landed produces exactly that, and it is the reason these four cases are four rather than
/// two.
///
/// This is a decision rather than a message so that it can be tested: the four cases are the whole
/// content, and the sentences a person reads belong to whichever tool is asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashVerdict {
    /// The program step was clean and every word read back.
    Verified,
    /// The program step was clean and the flash does not match. The part was not programmed.
    Mismatch {
        /// How many words differ.
        wrong: usize,
    },
    /// **The program step reported an error and the flash matches the image anyway.**
    ///
    /// Not success, because an error on the wire is worth chasing and a second attempt may not be
    /// so lucky -- and not failure, because nothing needs re-writing. A caller that collapses this
    /// into either one is throwing away the fact that decides what to do next.
    VerifiedDespiteError,
    /// The program step reported an error and the flash does not match it.
    Failed {
        /// How many words differ.
        wrong: usize,
    },
}

impl FlashVerdict {
    /// The verdict from a program result and a read-back comparison.
    #[must_use]
    pub fn of(program_failed: bool, wrong: usize) -> Self {
        match (program_failed, wrong) {
            (false, 0) => FlashVerdict::Verified,
            (false, wrong) => FlashVerdict::Mismatch { wrong },
            (true, 0) => FlashVerdict::VerifiedDespiteError,
            (true, wrong) => FlashVerdict::Failed { wrong },
        }
    }

    /// The process exit code a bench tool should carry.
    ///
    /// **FOUR OUTCOMES, THREE CODES, AND THE THIRD ONE EARNS ITS KEEP.** A script that treats every
    /// non-zero as "re-flash it" would otherwise re-flash a board that is already correct, which on
    /// a part whose flash is write-once between erases is a second erase for nothing.
    #[must_use]
    pub fn exit_code(self) -> i32 {
        match self {
            FlashVerdict::Verified => 0,
            FlashVerdict::Mismatch { .. } | FlashVerdict::Failed { .. } => 1,
            FlashVerdict::VerifiedDespiteError => 4,
        }
    }

    /// Whether the image is on the part, whatever the wire said while putting it there.
    #[must_use]
    pub fn image_is_on_the_part(self) -> bool {
        matches!(self, FlashVerdict::Verified | FlashVerdict::VerifiedDespiteError)
    }
}

/// Maps a transfer-status code to the failure it names, or `None` when the transfer completed.
///
/// Split out from the transfer path so the DECISION is testable without a probe: the wiring is
/// two lines, this is where a wrong answer would actually live.
fn rw_status_failure(status: u8) -> Option<&'static str> {
    match status {
        DEBUG_ERR_OK => None,
        DEBUG_ERR_FAULT => Some("the ST-Link reported a transfer fault"),
        DEBUG_ERR_GET_IDCODE => Some("the ST-Link could not read the target's IDCODE"),
        DEBUG_ERR_WRITE => Some("the ST-Link reported a memory write error"),
        DEBUG_ERR_WRITE_VERIFY => Some("the ST-Link's post-write verify did not match"),
        DEBUG_ERR_AP_WAIT => Some("the access port answered WAIT and the transfer did not complete"),
        DEBUG_ERR_AP_FAULT => Some("the access port answered FAULT -- the address is unmapped, or the bus refused it"),
        DEBUG_ERR_AP_ERROR => Some("the access port reported an error"),
        DEBUG_ERR_AP_PARITY => Some("the access port's reply failed its parity check"),
        DEBUG_ERR_DP_WAIT => Some("the debug port answered WAIT and the transfer did not complete"),
        DEBUG_ERR_DP_FAULT => Some("the debug port answered FAULT -- check target power and reset"),
        DEBUG_ERR_DP_ERROR => Some("the debug port reported an error"),
        DEBUG_ERR_DP_PARITY => Some("the debug port's reply failed its parity check"),
        DEBUG_ERR_AP_WDATA => Some("the access port reported a write-data error"),
        DEBUG_ERR_AP_STICKY => Some("the access port has a sticky error set from an earlier transfer"),
        DEBUG_ERR_AP_STICKY_OVERRUN => Some("the access port has a sticky overrun set from an earlier transfer"),
        DEBUG_ERR_BAD_AP => Some("the target has no such access port"),
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

/// How long to let the reset line settle either side of driving it.
///
/// Not tuned: this is the value the H755 attach was first measured working at. A shorter one may
/// work and was not tried, because a settle time that is too short fails intermittently, which is
/// the worst way for a flash tool to fail.
const RESET_SETTLE: Duration = Duration::from_millis(50);

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
    /// Whether this probe answers the wider transfer-status query. Resolved from the firmware
    /// version on first use and remembered, because it is asked after EVERY memory access.
    wide_status: Option<bool>,
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
        let selector = match serial {
            Some(requested) if !requested.trim().is_empty() => {
                selection::Selector::by_serial(requested.trim())
            }
            _ => selection::Selector::from_environment(),
        }
        .with_vid_pid(ST_VENDOR_ID, product_id);

        let chosen = choose_debug_interface(
            lamella_usbbulk::enumerate_interface(ST_DEBUG_INTERFACE_GUID),
            lamella_usbbulk::enumerate,
            &selector,
        )?;

        let device =
            Device::open_interface(ST_DEBUG_INTERFACE_GUID, ST_VENDOR_ID, product_id, chosen.as_deref())
                .map_err(|_| ProbeError::Device("could not open the ST-Link debug interface"))?;
        Ok(StLink { device, wide_status: None })
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
    /// A V3 ALSO NEEDS A COMMUNICATION FREQUENCY FIRST, AND REFUSES DEBUG MODE WITHOUT ONE.
    /// A V2/V2-1 carries a default and needs nothing; a V3 has none, so the same call that works on
    /// every other probe answered `ST-Link refused to enter SWD debug mode` on a V3
    /// with the target rail confirmed at 3.00 V -- a probe-side omission that reads exactly like a
    /// dead target. Handled here rather than left to callers because every caller would need it and
    /// the failure it prevents does not name itself.
    pub fn enter_swd(&mut self) -> Result<(), ProbeError> {
        if matches!(self.current_mode(), Ok(Mode::Dfu)) {
            self.exit_dfu()?;
        }
        if self.version().is_ok_and(|v| v.stlink >= 3) {
            self.negotiate_com_freq()?;
        }
        let reply = self.transact(&[CMD_DEBUG, DEBUG_APIV2_ENTER, DEBUG_ENTER_SWD], 2)?;
        if reply[0] != DEBUG_ERR_OK {
            return Err(ProbeError::Device("ST-Link refused to enter SWD debug mode"));
        }
        Ok(())
    }

    /// The SWD frequencies (kHz) this probe reports it supports, fastest first (APIv3 only).
    ///
    /// The 52-byte reply carries the entry count at offset 8 and the frequencies as little-endian
    /// 32-bit words from offset 12.
    pub fn com_frequencies(&mut self) -> Result<Vec<u32>, ProbeError> {
        let reply = self.transact(&[CMD_DEBUG, DEBUG_APIV3_GET_COM_FREQ, COM_FREQ_MODE_SWD], 52)?;
        if reply[0] != DEBUG_ERR_OK {
            return Err(ProbeError::Device("ST-Link refused to report its SWD frequencies"));
        }
        let entries = (reply[8] as usize).min((reply.len() - 12) / 4);
        Ok((0..entries)
            .map(|i| {
                let at = 12 + i * 4;
                u32::from_le_bytes([reply[at], reply[at + 1], reply[at + 2], reply[at + 3]])
            })
            .collect())
    }

    /// Sets the SWD communication frequency in kHz (APIv3 only).
    pub fn set_com_freq(&mut self, khz: u32) -> Result<(), ProbeError> {
        let f = khz.to_le_bytes();
        let reply = self.transact(
            &[
                CMD_DEBUG,
                DEBUG_APIV3_SET_COM_FREQ,
                COM_FREQ_MODE_SWD,
                0,
                f[0],
                f[1],
                f[2],
                f[3],
            ],
            8,
        )?;
        if reply[0] != DEBUG_ERR_OK {
            return Err(ProbeError::Device("ST-Link refused the requested SWD frequency"));
        }
        Ok(())
    }

    /// Picks a frequency from the ones the probe REPORTS, and sets it.
    ///
    /// It does not invent a rate. A number we chose could be one this probe does not offer, and
    /// the refusal would look like the very failure this exists to fix; asking first means the value
    /// is always one the probe named. Of those, the fastest at or below [`MAX_NEGOTIATED_KHZ`] --
    /// bring-up runs over flying leads as often as over a board trace, and the top rate a probe can
    /// name is not one every wire can carry.
    fn negotiate_com_freq(&mut self) -> Result<(), ProbeError> {
        let offered = self.com_frequencies()?;
        let chosen = offered
            .iter()
            .copied()
            .filter(|&khz| khz <= MAX_NEGOTIATED_KHZ)
            .max()
            .or_else(|| offered.iter().copied().min())
            .ok_or(ProbeError::Device(
                "ST-Link reported no SWD frequencies at all, so none can be selected",
            ))?;
        self.set_com_freq(chosen)
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

    /// Whether this probe answers the wider transfer-status query, asking the probe once.
    ///
    /// THE LOOKUP IS SAFE TO PERFORM BETWEEN A TRANSFER AND ITS STATUS CHECK, AND THAT WAS
    /// MEASURED RATHER THAN ASSUMED, because if it were not, a lazily-resolved version would ERASE
    /// the very answer it was resolved to obtain -- and would do it only on the first access of a
    /// session, which is the hardest shape of bug to see. The sequence: a read deliberately failed
    /// through a non-existent access port, then `GET_VERSION`, then the status re-read -- the
    /// failure and its faulting address both survived unchanged (`0x1d at 0x08000000` either side).
    fn uses_wide_status(&mut self) -> bool {
        if let Some(known) = self.wide_status {
            return known;
        }
        let wide = self
            .version()
            .is_ok_and(|version| wide_status_supported(version.stlink, version.jtag));
        self.wide_status = Some(wide);
        wide
    }

    /// Asks the probe whether the last read or write completed, and turns "no" into an error.
    ///
    /// WHICH QUERY IS NOT A DETAIL -- IT IS THE DIFFERENCE BETWEEN READING MEMORY AND NOT. Sending
    /// `GETLASTRWSTATUS` unconditionally looks like a choice not worth a version check, and is not:
    /// **an ST-Link/V3 answers the older form with `0x42` -- to a good read, to a failed read, and
    /// before any transfer has happened at all.**
    /// Every memory access through a V3 therefore returned correct data and was then thrown away by
    /// this function, which is how "the V3 cannot read memory on an H7" came to be believed. It
    /// could read it the whole time; nothing on the wire was ever wrong.
    ///
    /// AND THE OLD FORM CANNOT BE LEFT IN AS A HARMLESS FALLBACK FOR A V3, because a constant
    /// answer fails in BOTH directions: `0x42` is not `OK`, so good transfers are rejected -- and
    /// had the constant happened to be `0x80`, failed transfers would have been ACCEPTED, which is
    /// the silent-stale-data defect this check exists to prevent. Measured, same probe and board:
    /// a read through a non-existent access port returns the previous status byte AS DATA.
    ///
    /// The wider form's 12-byte reply also carries the faulting ADDRESS at offset 4. It is not
    /// surfaced here because [`ProbeError::Device`] carries a fixed string; `stlink-mem-diagnose`
    /// prints it, which is where a faulting address is actually read.
    fn check_last_rw_status(&mut self) -> Result<(), ProbeError> {
        let (opcode, reply_len) = if self.uses_wide_status() {
            (DEBUG_APIV2_GETLASTRWSTATUS2, 12)
        } else {
            (DEBUG_APIV2_GETLASTRWSTATUS, 2)
        };
        let reply = self.transact(&[CMD_DEBUG, opcode], reply_len)?;
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

    /// Enters SWD with the core held in reset, so a target whose running firmware leaves the debug
    /// access port unreachable can still be attached to.
    ///
    /// MEASURED ON A NUCLEO-H755ZI-Q, where the plain [`enter_swd`](Self::enter_swd)
    /// path reaches the DEBUG PORT and no further: `READ_IDCODE` answers `0x6ba02477` and then
    /// every memory access fails, as does opening the access port at all. Holding nRST across the
    /// SWD entry read the CPUID immediately. **A probe that reports an id is not a probe that can
    /// read memory**, which is why the plain path's success is not evidence a target is attached.
    ///
    /// WHAT IS RECORDED IS THE SEQUENCE THAT WORKED, NOT A MECHANISM. Holding reset and re-entering
    /// SWD change together here and were not separated, because the condition does not survive its
    /// own cure: once this has run, the plain path succeeds on the same board until it is
    /// power-cycled. The control that would attribute the fix cannot be run twice in one session,
    /// so the honest claim is the smaller one.
    ///
    /// It leaves reset ASSERTED. The caller decides what happens on release -- ordinarily arming
    /// the reset vector catch and then [`release_reset`](Self::release_reset), which is what
    /// [`attach_under_reset`](Self::attach_under_reset) does.
    pub fn enter_swd_under_reset(&mut self) -> Result<(), ProbeError> {
        self.enter_swd()?;
        self.drive_nrst(true)?;
        std::thread::sleep(RESET_SETTLE);
        self.enter_swd()
    }

    /// Releases a reset asserted by [`enter_swd_under_reset`] and lets the line settle.
    pub fn release_reset(&mut self) -> Result<(), ProbeError> {
        self.drive_nrst(false)?;
        std::thread::sleep(RESET_SETTLE);
        Ok(())
    }

    /// Attaches with the core held in reset and leaves it HALTED at its reset vector -- the state a
    /// flash routine needs before it erases the code the core is running.
    ///
    /// For a target the plain path does not reach: one whose running firmware leaves its access
    /// port unreachable, and one whose firmware sleeps. SWD is entered with reset asserted
    /// ([`enter_swd_under_reset`](Self::enter_swd_under_reset)), and [`attach_held_in_reset`] does
    /// the rest, `low_power_debug`'s bits included.
    ///
    /// # Errors
    /// Those of [`enter_swd_under_reset`](Self::enter_swd_under_reset) and
    /// [`attach_held_in_reset`].
    pub fn attach_under_reset(
        &mut self,
        low_power_debug: Option<LowPowerDebug>,
    ) -> Result<(), ProbeError> {
        self.enter_swd_under_reset()?;
        attach_held_in_reset(self, low_power_debug)
    }

    /// The highest application voltage ANY ST-Link in TN1235 states support for, in volts.
    ///
    /// # "5 V tolerant inputs" is not permission to debug a 5 V target
    ///
    /// TN1235 states two different things and they are easy to read as one. Every probe it
    /// describes has an APPLICATION VOLTAGE SUPPORT range, and the top of that range is 3.6 V on
    /// all of them. Some additionally say "5 V tolerant inputs" -- which is a statement about the
    /// input pins SURVIVING a higher level, not about the interface being specified to operate
    /// there. A probe can be undamaged by a 5 V target and still not be a probe that debugs one.
    ///
    /// ```text
    /// ST-LINK/V2      1.65-3.6 V   and 5 V tolerant inputs
    /// STLINK-V2EC     1.65-3.6 V   (no tolerance clause)
    /// STLINK-V3SET    3-3.6 V      and 5 V tolerant inputs
    /// STLINK-V3MODS   3-3.6 V      and 5 V tolerant inputs
    /// STLINK-V3MINIE  1.65-3.6 V   (no tolerance clause)
    /// STLINK-V3EC     1.65-3.6 V   (no tolerance clause)
    /// STLINK-V3PWR    1.6-3.6 V    (a level shifter, same ceiling)
    /// ```
    ///
    /// **THE CEILING IS THE SAME ON EVERY ROW, WHICH IS WHY THIS IS ONE NUMBER AND NOT A TABLE PER
    /// PROBE.** It could not be a table anyway: the product id cannot tell a V3E from a V3EC from a
    /// V3MINIE -- all three answer 0x374E or 0x3754 -- so a per-model rule would need a model this
    /// bus does not carry.
    ///
    /// The probes WITHOUT the tolerance clause are the ones that reach DOWN to 1.65 V. So the
    /// probe best suited to a low-voltage part is the one least able to survive a high-voltage one,
    /// which is the opposite of the intuition that a newer probe is a more capable probe.
    pub const MAX_APPLICATION_VOLTAGE: f32 = 3.6;

    /// Whether a measured target rail is outside what any ST-Link is specified to drive, and what
    /// to say about it.
    ///
    /// Returns `None` when the reading is inside the specified range or is too low to be a live
    /// rail at all -- a near-zero reading means no target rather than a bad one, and reporting it
    /// as over-voltage would be the wrong complaint entirely.
    #[must_use]
    pub fn application_voltage_warning(volts: f32) -> Option<&'static str> {
        if volts <= Self::MAX_APPLICATION_VOLTAGE {
            return None;
        }
        Some(
            "OUT OF RANGE -- no ST-Link states application voltage support above 3.6 V (TN1235). \
             A probe marked \"5 V tolerant inputs\" may survive this; one without that clause may \
             not, and neither is specified to debug at this level. Disconnect before driving SWD.",
        )
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

/// The CPUID Base Register: the one word of target memory whose shape every Cortex-M architecture
/// fixes (Armv6-M ARM, Table B3-5; Armv7-M ARM, B3.2.3; Armv8-M ARM, D1.2).
const CPUID: u32 = 0xE000_ED00;

/// Reads CPUID and refuses unless the word read has the shape every Cortex-M architecture fixes for
/// it, so a probe whose memory reads are not reaching the target is refused instead of trusted.
///
/// A transfer the probe reports as successful is not, on its own, a read that happened: a probe can
/// answer every memory read with one constant word and report each read as successful. A register
/// whose answer is known tells a real read from that.
///
/// The known answer is the two fields the architecture manuals fix: IMPLEMENTER, `0x41` for a
/// processor implemented by Arm, and ARCHITECTURE, `0xC` (Armv6-M, and Armv8-M without the Main
/// Extension) or `0xF` (Armv7-M, and Armv8-M with the Main Extension). The other fields are
/// implementation defined and are not judged.
///
/// Returns the word read, for a caller that wants the part number too.
///
/// # Errors
/// The read's own error when the read fails, and [`ProbeError::Protocol`] naming the word when the
/// read succeeds with a word no Cortex-M holds.
pub fn check_known_answer<M: CoreMemory + ?Sized>(core: &mut M) -> Result<u32, ProbeError> {
    let word = core.read_word(CPUID)?;
    if is_cortex_m_cpuid(word) {
        return Ok(word);
    }
    Err(ProbeError::Protocol(format!(
        "the probe reported a successful read of CPUID ({CPUID:#010x}) that returned {word:#010x}, a \
         word no Cortex-M holds, so its memory reads are not reaching the target; attaching with the \
         core held in reset may reach it"
    )))
}

/// Whether `word` has the IMPLEMENTER and ARCHITECTURE fields a Cortex-M's CPUID holds.
fn is_cortex_m_cpuid(word: u32) -> bool {
    const ARM: u32 = 0x41;
    let implementer = word >> 24;
    let architecture = (word >> 16) & 0xF;
    implementer == ARM && matches!(architecture, 0xC | 0xF)
}

/// A register whose bits keep a debugger connection working while the target's core is in a
/// low-power mode, and the bits to set in it: on an STM32, `DBGMCU_CR` and its low-power debug
/// bits.
///
/// With those bits clear, a part can stop a clock its debugger connection needs whenever the core
/// sleeps, and memory access through the probe then stops reaching the target while the firmware
/// waits for an interrupt. A part held in reset is not asleep, which is why
/// [`attach_held_in_reset`] sets the bits before it releases the core.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LowPowerDebug {
    /// The register's address. It is written with a 32-bit access.
    pub register: u32,
    /// The bits to set, on top of whatever the register already holds.
    pub bits: u32,
}

/// Finishes an attach that began with the core held in reset, and leaves the core HALTED at its
/// reset vector: reads the known answer, sets `low_power_debug`'s bits, arms the reset vector
/// catch, releases reset, waits for the halt and disarms the catch.
///
/// Everything before the release runs while the core is held. A part held in reset is not asleep,
/// so its memory answers even when its firmware idles in a low-power mode, and the vector catch is
/// armed before the core can execute an instruction.
///
/// The known answer ([`check_known_answer`]) is read first because the low-power write is
/// composed from the word its register reads back, and a word that never came from the target
/// must not be written into it. The bits are set on top of what the register holds and then read
/// back, so a write the probe reports as successful and the part does not keep is refused rather
/// than trusted.
///
/// A step that fails before the release releases reset before returning, so a refused attach does
/// not leave the part held in reset. Bits set in a register that a system reset does not clear
/// stay set after the attach, through later resets.
///
/// # Errors
/// [`check_known_answer`]'s refusal, [`ProbeError::Protocol`] naming the register when the bits
/// do not read back as set, and any read, write or change of the reset line the probe refuses.
pub fn attach_held_in_reset<M: CoreMemory>(
    core: &mut M,
    low_power_debug: Option<LowPowerDebug>,
) -> Result<(), ProbeError> {
    if let Err(refused) = prepare_held_core(core, low_power_debug) {
        let _ = core.set_reset(false);
        return Err(refused);
    }
    core.set_reset(false)?;
    std::thread::sleep(RESET_SETTLE);
    cortex_m::wait_halted(core)?;
    cortex_m::disarm_reset_catch(core)
}

/// The steps of [`attach_held_in_reset`] that run while the core is held: the known answer, the
/// low-power bits and the reset vector catch.
fn prepare_held_core<M: CoreMemory>(
    core: &mut M,
    low_power_debug: Option<LowPowerDebug>,
) -> Result<(), ProbeError> {
    check_known_answer(core)?;
    if let Some(debug) = low_power_debug {
        set_low_power_debug(core, debug)?;
    }
    cortex_m::arm_reset_catch(core)
}

/// Sets `debug`'s bits on top of what its register holds, and refuses unless they read back set.
fn set_low_power_debug<M: CoreMemory>(
    core: &mut M,
    debug: LowPowerDebug,
) -> Result<(), ProbeError> {
    let written = core.read_word(debug.register)? | debug.bits;
    core.write_word(debug.register, written)?;
    let kept = core.read_word(debug.register)?;
    if kept & debug.bits == debug.bits {
        return Ok(());
    }
    Err(ProbeError::Protocol(format!(
        "the part did not keep the low-power debug bits {bits:#010x}: {register:#010x} was written \
         with {written:#010x} and reads back {kept:#010x}",
        bits = debug.bits,
        register = debug.register,
    )))
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
        self.enter_swd()?;
        check_known_answer(self).map(|_| ())
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

    fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
        let mut address = address;
        let mut remaining = out;
        while !remaining.is_empty() {
            let batch = remaining.len().min(MAX_TRANSFER / 4);
            let bytes = self.read_mem32(address, (batch * 4) as u16)?;
            for (slot, word) in remaining[..batch].iter_mut().zip(bytes.chunks_exact(4)) {
                *slot = u32::from_le_bytes([word[0], word[1], word[2], word[3]]);
            }
            address += (batch * 4) as u32;
            remaining = &mut remaining[batch..];
        }
        Ok(())
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

/// The devices a USB listing reports, or why it could not list them.
type Listing = lamella_usbbulk::Result<Vec<lamella_usbbulk::DeviceInfo>>;

/// Which attached ST-Link `selector` names: its serial, or `None` for a sole match that reports no
/// serial and is opened by vendor and product id alone.
///
/// `registered` is the listing of interfaces under [`ST_DEBUG_INTERFACE_GUID`]. A platform with no
/// registry of interface GUIDs answers it with [`lamella_usbbulk::Error::Unsupported`], and there
/// `every_vendor_bulk_device` supplies the candidates instead: every attached device with a
/// vendor-class interface, which the vendor and product id in `selector` narrow to ST-Links of the
/// requested generation. Both listings reach the same [`selection::choose`], so an explicit serial,
/// [`selection::PROBE_SERIAL_ENV`] and the refusal of several matches mean the same thing on every
/// platform.
///
/// # Errors
///
/// [`ProbeError::Device`] when a listing fails or nothing matches; [`ProbeError::Ambiguous`],
/// naming every match, when more than one probe matches.
fn choose_debug_interface(
    registered: Listing,
    every_vendor_bulk_device: impl FnOnce() -> Listing,
    selector: &selection::Selector,
) -> Result<Option<String>, ProbeError> {
    let listed = match registered {
        Err(lamella_usbbulk::Error::Unsupported) => every_vendor_bulk_device(),
        registered => registered,
    }
    .map_err(|_| ProbeError::Device("could not enumerate the ST-Link debug interfaces"))?;

    let candidates: Vec<selection::Candidate> = listed
        .into_iter()
        .map(|found| selection::Candidate {
            vendor_id: found.vendor_id,
            product_id: found.product_id,
            serial: found.serial_number,
        })
        .collect();

    match selection::choose(&candidates, selector) {
        selection::Selection::Unique(found) => Ok(found),
        selection::Selection::NotFound => Err(ProbeError::Device(
            "no ST-Link matched -- check the serial, or the product id for this probe \
             generation (a V3 is a FAMILY of ids, not one)",
        )),
        selection::Selection::Ambiguous(names) => Err(ProbeError::Ambiguous(names)),
    }
}

#[cfg(test)]
mod debug_interface_listing_tests {
    use super::{Listing, ProbeError, ST_VENDOR_ID, choose_debug_interface, product_id, selection};
    use lamella_usbbulk::{DeviceInfo, Error};

    const FIRST: &str = "AAAA1111AAAA1111AAAA1111";
    const SECOND: &str = "BBBB2222BBBB2222BBBB2222";
    const NEITHER: &str = "CCCC3333CCCC3333CCCC3333";

    fn attached(vendor_id: u16, product_id: u16, serial: Option<&str>) -> DeviceInfo {
        DeviceInfo {
            vendor_id,
            product_id,
            serial_number: serial.map(String::from),
            product: None,
            interface_name: None,
        }
    }

    /// Two ST-Link/V2-1s: one model, so only the serial tells them apart.
    fn two_alike() -> Vec<DeviceInfo> {
        vec![
            attached(ST_VENDOR_ID, product_id::V2_1, Some(FIRST)),
            attached(ST_VENDOR_ID, product_id::V2_1, Some(SECOND)),
        ]
    }

    /// The selector `StLink::open` builds for a V2-1, with the environment's answer supplied as a
    /// value so that no test depends on the shell running it.
    fn selector(serial: Option<&str>) -> selection::Selector {
        selection::Selector::named_or(serial, selection::Selector::any())
            .with_vid_pid(ST_VENDOR_ID, product_id::V2_1)
    }

    /// The GUID listing as a platform with no registry of interface GUIDs answers it.
    fn no_registry() -> Listing {
        Err(Error::Unsupported)
    }

    #[test]
    fn without_a_registry_a_serial_picks_the_second_of_two_alike() {
        assert_eq!(
            choose_debug_interface(no_registry(), || Ok(two_alike()), &selector(Some(SECOND))),
            Ok(Some(String::from(SECOND)))
        );
        assert!(matches!(
            choose_debug_interface(no_registry(), || Ok(two_alike()), &selector(Some(NEITHER))),
            Err(ProbeError::Device(why)) if why.starts_with("no ST-Link matched")
        ));
    }

    #[test]
    fn without_a_registry_two_alike_and_no_serial_are_refused() {
        let chosen = choose_debug_interface(no_registry(), || Ok(two_alike()), &selector(None));
        let Err(ProbeError::Ambiguous(names)) = &chosen else {
            panic!("two ST-Links and no serial must be refused, not resolved to one: {chosen:?}");
        };
        assert_eq!(*names, [FIRST, SECOND], "the refusal names both");
    }

    /// Without a registry the listing holds every vendor-class device on the bus -- here a
    /// CMSIS-DAP probe and an ST-Link of another generation beside the one asked for -- and the
    /// vendor and product id leave only that one, so it needs no serial.
    #[test]
    fn without_a_registry_the_ids_leave_only_the_st_link_asked_for() {
        let every_vendor_bulk_device = vec![
            attached(0x2e8a, 0x000c, Some("DDDD4444DDDD4444")),
            attached(ST_VENDOR_ID, product_id::V3E, Some(NEITHER)),
            attached(ST_VENDOR_ID, product_id::V2_1, Some(FIRST)),
        ];
        assert_eq!(
            choose_debug_interface(no_registry(), || Ok(every_vendor_bulk_device), &selector(None)),
            Ok(Some(String::from(FIRST)))
        );
    }

    #[test]
    fn a_registry_that_answers_is_the_only_listing_consulted() {
        assert_eq!(
            choose_debug_interface(
                Ok(vec![attached(ST_VENDOR_ID, product_id::V2_1, Some(FIRST))]),
                || panic!("the vendor-class listing is for a platform with no registry"),
                &selector(None),
            ),
            Ok(Some(String::from(FIRST)))
        );
    }

    #[test]
    fn a_listing_that_fails_is_refused() {
        let refused = |chosen: Result<Option<String>, ProbeError>| {
            matches!(chosen, Err(ProbeError::Device(why)) if why.starts_with("could not enumerate"))
        };
        assert!(refused(choose_debug_interface(
            Err(Error::Os(String::from("the registry could not be read"))),
            || panic!("a registry that failed is refused, not replaced by another listing"),
            &selector(None),
        )));
        assert!(refused(choose_debug_interface(
            no_registry(),
            || Err(Error::Os(String::from("the bus could not be read"))),
            &selector(None),
        )));
    }
}

#[cfg(test)]
mod tests {

    /// The four outcomes, and the one that exists because a board demonstrated it.
    #[test]
    fn a_verdict_keeps_the_two_observations_apart() {
        assert_eq!(FlashVerdict::of(false, 0), FlashVerdict::Verified);
        assert_eq!(FlashVerdict::of(false, 7), FlashVerdict::Mismatch { wrong: 7 });
        assert_eq!(FlashVerdict::of(true, 0), FlashVerdict::VerifiedDespiteError);
        assert_eq!(FlashVerdict::of(true, 7), FlashVerdict::Failed { wrong: 7 });
    }

    /// A clean run is 0, a part that was not programmed is 1, and the write that landed while the
    /// wire complained is its own code -- because a script keying on "non-zero means re-flash"
    /// would re-erase a part that is already correct.
    #[test]
    fn the_exit_code_separates_a_bad_write_from_a_noisy_one() {
        assert_eq!(FlashVerdict::Verified.exit_code(), 0);
        assert_eq!(FlashVerdict::Mismatch { wrong: 1 }.exit_code(), 1);
        assert_eq!(FlashVerdict::Failed { wrong: 1 }.exit_code(), 1);
        assert_eq!(FlashVerdict::VerifiedDespiteError.exit_code(), 4);
        assert_ne!(FlashVerdict::Verified.exit_code(), FlashVerdict::VerifiedDespiteError.exit_code());
    }

    /// The question a caller actually asks -- "do I need to write this board again?" -- and the
    /// answer is NO in both verified cases, which is the whole point of keeping them apart.
    #[test]
    fn the_image_is_on_the_part_in_both_verified_cases() {
        assert!(FlashVerdict::Verified.image_is_on_the_part());
        assert!(FlashVerdict::VerifiedDespiteError.image_is_on_the_part());
        assert!(!FlashVerdict::Mismatch { wrong: 1 }.image_is_on_the_part());
        assert!(!FlashVerdict::Failed { wrong: 1 }.image_is_on_the_part());
    }
    use super::*;

    /// Target memory that answers every read with one word and reports each read as successful --
    /// what a probe whose reads do not reach the target hands back -- recording where it was read.
    struct ConstantMemory {
        word: u32,
        read_at: Vec<u32>,
    }

    impl CoreMemory for ConstantMemory {
        fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
            self.read_at.push(address);
            Ok(self.word)
        }
        fn write_word(&mut self, _address: u32, _value: u32) -> Result<(), ProbeError> {
            Ok(())
        }
        fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
            Ok(0)
        }
    }

    const STALE_ANSWER: u32 = 0x0000_001a;

    #[test]
    fn a_read_answered_by_a_constant_with_success_is_refused_and_names_the_word() {
        let mut memory = ConstantMemory {
            word: STALE_ANSWER,
            read_at: Vec::new(),
        };
        let refused = check_known_answer(&mut memory);
        assert_eq!(
            memory.read_at,
            [CPUID],
            "the known answer is read from CPUID"
        );
        match refused {
            Err(ProbeError::Protocol(text)) => {
                assert!(text.contains("0x0000001a"), "names the word read: {text}");
                assert!(text.contains("CPUID"), "names the register: {text}");
            }
            other => panic!("a constant answered with success must be refused, got {other:?}"),
        }
    }

    #[test]
    fn a_word_with_the_fields_every_cortex_m_architecture_fixes_is_accepted() {
        for word in [0x410C_1234_u32, 0x412F_ABC5, 0x41FC_0000, 0x410F_FFFF] {
            let mut memory = ConstantMemory {
                word,
                read_at: Vec::new(),
            };
            assert_eq!(check_known_answer(&mut memory), Ok(word), "{word:#010x}");
        }
    }

    #[test]
    fn a_word_no_cortex_m_holds_is_refused() {
        for word in [
            0x4107_1230_u32,
            0x550F_1230,
            0xFFFF_FFFF,
            0x0000_0000,
            STALE_ANSWER,
        ] {
            let mut memory = ConstantMemory {
                word,
                read_at: Vec::new(),
            };
            assert!(
                matches!(
                    check_known_answer(&mut memory),
                    Err(ProbeError::Protocol(_))
                ),
                "{word:#010x} must be refused"
            );
        }
    }

    #[test]
    fn a_read_that_fails_is_reported_as_it_failed() {
        struct Failing;
        impl CoreMemory for Failing {
            fn read_word(&mut self, _address: u32) -> Result<u32, ProbeError> {
                Err(ProbeError::Device("the transfer failed"))
            }
            fn write_word(&mut self, _address: u32, _value: u32) -> Result<(), ProbeError> {
                Ok(())
            }
            fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
                Ok(0)
            }
        }
        assert_eq!(
            check_known_answer(&mut Failing),
            Err(ProbeError::Device("the transfer failed"))
        );
    }

    /// One access an attach made, in the order it made them.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Access {
        Read(u32),
        Write(u32, u32),
        Reset(bool),
    }

    const DHCSR: u32 = 0xE000_EDF0;
    const DEMCR: u32 = 0xE000_EDFC;
    const S_HALT: u32 = 1 << 17;
    const VC_CORERESET: u32 = 1 << 0;

    const CORTEX_M_CPUID: u32 = 0x412F_ABC5;

    const TRACE_CLKINEN: u32 = 1 << 5;

    const F7_LOW_POWER_DEBUG: LowPowerDebug = LowPowerDebug {
        register: lamella_cmsis_dap_stm32::STM32F7_DBGMCU_CR,
        bits: lamella_cmsis_dap_stm32::STM32F7_DBGMCU_CR_LOW_POWER_DEBUG,
    };

    /// A part as an attach under reset hands it on: reset asserted, CPUID holding `cpuid`, a
    /// low-power register at [`F7_LOW_POWER_DEBUG`]'s address that keeps only the bits in `keeps`,
    /// and a core that halts when reset is released with the reset vector catch armed.
    struct HeldPart {
        cpuid: u32,
        low_power: u32,
        keeps: u32,
        answers_every_read_with: Option<u32>,
        catch_armed: bool,
        in_reset: bool,
        halted: bool,
        accesses: Vec<Access>,
    }

    impl HeldPart {
        fn new() -> Self {
            Self {
                cpuid: CORTEX_M_CPUID,
                low_power: TRACE_CLKINEN,
                keeps: u32::MAX,
                answers_every_read_with: None,
                catch_armed: false,
                in_reset: true,
                halted: false,
                accesses: Vec::new(),
            }
        }

        fn position(&self, access: Access) -> usize {
            self.accesses
                .iter()
                .position(|made| *made == access)
                .unwrap_or_else(|| panic!("{access:?} was never made: {:?}", self.accesses))
        }

        fn wrote(&self) -> bool {
            self.accesses
                .iter()
                .any(|made| matches!(made, Access::Write(..)))
        }
    }

    impl CoreMemory for HeldPart {
        fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
            self.accesses.push(Access::Read(address));
            if let Some(word) = self.answers_every_read_with {
                return Ok(word);
            }
            Ok(match address {
                CPUID => self.cpuid,
                DHCSR if self.halted => S_HALT,
                address if address == F7_LOW_POWER_DEBUG.register => self.low_power,
                _ => 0,
            })
        }

        fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
            self.accesses.push(Access::Write(address, value));
            if address == F7_LOW_POWER_DEBUG.register {
                self.low_power = (self.low_power & !self.keeps) | (value & self.keeps);
            } else if address == DEMCR {
                self.catch_armed = value & VC_CORERESET != 0;
            }
            Ok(())
        }

        fn set_reset(&mut self, assert: bool) -> Result<u8, ProbeError> {
            self.accesses.push(Access::Reset(assert));
            if !assert && self.in_reset && self.catch_armed {
                self.halted = true;
            }
            self.in_reset = assert;
            Ok(0)
        }
    }

    #[test]
    fn the_low_power_bits_are_set_on_what_the_register_holds_before_the_core_is_released() {
        let mut part = HeldPart::new();
        assert_eq!(
            attach_held_in_reset(&mut part, Some(F7_LOW_POWER_DEBUG)),
            Ok(())
        );
        let composed = TRACE_CLKINEN | F7_LOW_POWER_DEBUG.bits;
        let known_answer = part.position(Access::Read(CPUID));
        let register_read = part.position(Access::Read(F7_LOW_POWER_DEBUG.register));
        let written = part.position(Access::Write(F7_LOW_POWER_DEBUG.register, composed));
        let armed = part.position(Access::Write(DEMCR, VC_CORERESET));
        let released = part.position(Access::Reset(false));
        assert!(
            known_answer < register_read,
            "the register is read only after the known answer: {:?}",
            part.accesses
        );
        assert!(
            register_read < written && written < armed && armed < released,
            "the bits are set and the catch armed while the core is held: {:?}",
            part.accesses
        );
        assert_eq!(
            part.low_power, composed,
            "a bit the attach does not own is kept"
        );
        assert!(part.halted, "the core is left halted at its reset vector");
        assert_eq!(
            part.accesses.last(),
            Some(&Access::Write(DEMCR, 0)),
            "the catch is disarmed last"
        );
    }

    #[test]
    fn a_part_answering_every_read_with_one_word_is_refused_before_any_write_and_released() {
        let mut part = HeldPart::new();
        part.answers_every_read_with = Some(STALE_ANSWER);
        let refused = attach_held_in_reset(&mut part, Some(F7_LOW_POWER_DEBUG));
        assert!(
            matches!(refused, Err(ProbeError::Protocol(_))),
            "a constant answered with success must be refused, got {refused:?}"
        );
        assert!(!part.wrote(), "nothing is written: {:?}", part.accesses);
        assert_eq!(
            part.accesses.last(),
            Some(&Access::Reset(false)),
            "a refused attach does not leave the part held in reset"
        );
    }

    #[test]
    fn low_power_bits_the_part_does_not_keep_are_refused_naming_the_register() {
        let mut part = HeldPart::new();
        part.keeps = 0;
        match attach_held_in_reset(&mut part, Some(F7_LOW_POWER_DEBUG)) {
            Err(ProbeError::Protocol(text)) => {
                assert!(text.contains("0xe0042004"), "names the register: {text}");
            }
            other => panic!("bits that do not read back must be refused, got {other:?}"),
        }
        assert!(
            !part.accesses.contains(&Access::Write(DEMCR, VC_CORERESET)),
            "the catch is not armed: {:?}",
            part.accesses
        );
        assert_eq!(part.accesses.last(), Some(&Access::Reset(false)));
    }

    #[test]
    fn with_no_low_power_register_the_known_answer_is_read_and_only_debug_registers_are_written() {
        let mut part = HeldPart::new();
        assert_eq!(attach_held_in_reset(&mut part, None), Ok(()));
        assert!(
            part.accesses.contains(&Access::Read(CPUID)),
            "the known answer is read: {:?}",
            part.accesses
        );
        for access in &part.accesses {
            if let Access::Write(address, _) = access {
                assert!(matches!(*address, DHCSR | DEMCR), "{access:?}");
            }
        }
        assert!(part.halted);
    }

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
    fn a_v3_takes_the_wide_query_without_consulting_its_jtag_field() {
        assert!(wide_status_supported(3, 1), "a V3 must take the wide query whatever its JTAG field says");
        assert!(wide_status_supported(3, 0));
        assert!(wide_status_supported(4, 0), "a later generation must not fall back either");
    }

    #[test]
    fn a_v2_takes_the_wide_query_from_its_firmware_version() {
        assert!(wide_status_supported(2, 39), "the bench V2-1 (JTAG v39) answers the wide query");
        assert!(wide_status_supported(2, FIRST_JTAG_WITH_WIDE_STATUS));
        assert!(!wide_status_supported(2, FIRST_JTAG_WITH_WIDE_STATUS - 1));
        assert!(!wide_status_supported(2, 0));
    }

    #[test]
    fn a_bad_access_port_is_reported_as_a_failure() {
        assert_eq!(rw_status_failure(DEBUG_ERR_BAD_AP), Some("the target has no such access port"));
    }

    #[test]
    fn the_v3s_constant_answer_is_not_success() {
        assert!(rw_status_failure(0x42).is_some());
    }

    #[test]
    fn mode_bytes_decode() {
        assert_eq!(Mode::from_byte(0x00), Mode::Dfu);
        assert_eq!(Mode::from_byte(0x01), Mode::Mass);
        assert_eq!(Mode::from_byte(0x02), Mode::Debug);
        assert_eq!(Mode::from_byte(0x42), Mode::Other(0x42));
    }
}

#[cfg(test)]
mod application_voltage_tests {
    use super::StLink;

    #[test]
    fn a_five_volt_target_is_named_out_of_range() {
        assert!(StLink::application_voltage_warning(5.0).is_some());
        assert!(StLink::application_voltage_warning(3.61).is_some());
    }

    #[test]
    fn every_rail_an_st_link_is_specified_for_passes_silently() {
        for volts in [1.65, 1.8, 2.5, 3.0, 3.3, 3.6] {
            assert!(
                StLink::application_voltage_warning(volts).is_none(),
                "{volts} V is inside TN1235's range and must not warn"
            );
        }
    }

    #[test]
    fn no_target_is_not_reported_as_an_over_voltage() {
        assert!(StLink::application_voltage_warning(0.0).is_none());
    }
}
