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

/// How many stale replies [`Dap::command`] will discard while looking for its own echo. Small on
/// purpose: it only has to cover the replies an abandoned session can leave queued, and each read
/// past the backlog costs a full transport timeout.
const STALE_REPLY_LIMIT: usize = 4;

/// The 128-bit dormant-to-SWD SELECTION ALERT (Arm ADIv5.1 / ADIv6): the fixed value every SWD-DP
/// recognizes to leave the dormant state, least-significant bit first. Followed by the 8-bit SWD
/// activation code `0x1A`. Used by [`Dap::connect_swd_from_dormant`].
const DORMANT_TO_SWD_ALERT: [u8; 16] = [
    0x92, 0xf3, 0x09, 0x62, 0x95, 0x2d, 0x85, 0x86, 0xe9, 0xaf, 0xdd, 0xe3, 0xa2, 0x0e, 0xbc, 0x19,
];

/// MEM-AP CSW for 32-bit memory accesses with single auto-increment: reserved bit +
/// master-type debug + HPROT data + DbgStatus + size-word + single-increment.
const CSW_WORD: u32 = 0x2300_0052;

/// MEM-AP CSW for sub-word accesses: [`CSW_WORD`]'s master/HPROT shape with Size = 8- or 16-bit
/// and auto-increment off. Sub-word DRW data rides the byte lane of its address (ADIv5 B2.2.2):
/// a byte at address A occupies DRW bits `[8*(A&3) +: 8]`, a halfword `[8*(A&2) +: 16]`.
const CSW_BYTE: u32 = 0x2300_0040;
const CSW_HALF: u32 = 0x2300_0041;

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


const AIRCR: u32 = 0xe000_ed0c;
const AIRCR_SYSRESETREQ: u32 = 0x05fa_0004;
const DEMCR: u32 = 0xe000_edfc;
const VC_CORERESET: u32 = 1 << 0;

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

#[cfg(feature = "usbbulk")]
impl Transport for lamella_usbbulk::Device {
    fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError> {
        lamella_usbbulk::Device::write_packet(self, data).map_err(|e| TransportError(e.to_string()))
    }
    fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        lamella_usbbulk::Device::read_packet(self, buf, std::time::Duration::from_millis(1000))
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
    /// The probe does not implement the command sent (it replied `0xFF` rather than echoing).
    Unsupported {
        /// The command id the probe refused.
        command: u8,
    },
    /// The probe accepted the command and reported that it FAILED: a non-`DAP_OK` status byte.
    Status {
        /// The command id sent.
        command: u8,
        /// The status byte returned (`0xFF` is `DAP_ERROR`; other values are undefined).
        status: u8,
    },
    /// An operation polled past its limit without completing (names what was awaited).
    Timeout(&'static str),
    /// The target device reported an operation failure (names the device-side condition) --
    /// e.g. a flash controller refusing a command or failing its post-write verify.
    Device(&'static str),
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
            DapError::Unsupported { command } => write!(
                f,
                "the probe does not implement command {command:#04x}"
            ),
            DapError::Status { command, status } => {
                let meaning = if *status == proto::DAP_ERROR { " (DAP_ERROR)" } else { "" };
                write!(f, "command {command:#04x} failed: status {status:#04x}{meaning}")
            }
            DapError::Timeout(what) => write!(f, "timed out waiting for {what}"),
            DapError::Device(what) => write!(f, "target device error: {what}"),
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

/// The scratch frame a [`Dap::call_target`] invocation runs in: a stack top and a return-trap address,
/// both in the TARGET's RAM (chip-specific, so the caller supplies them -- e.g. `0x2004_0000` /
/// `0x2000_0000` on an RP2350), plus the halt-poll budget (raise it for a long-running callee such as a
/// flash erase, which can take many polls to return).
#[derive(Debug, Clone, Copy)]
pub struct CallFrame {
    /// Scratch stack top (full-descending SP), in target RAM.
    pub sp: u32,
    /// Return-trap address, in target RAM: [`Dap::call_target`] plants a `BKPT` here and points LR at it.
    /// The word at this address is clobbered.
    pub trap: u32,
    /// How many times to poll for the return halt before giving up.
    pub poll_tries: u32,
}

impl CallFrame {
    /// A frame with a default halt-poll budget; set `poll_tries` higher for a long-running callee.
    pub fn new(sp: u32, trap: u32) -> CallFrame {
        CallFrame {
            sp,
            trap,
            poll_tries: 8000,
        }
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

    /// The underlying transport, for inspection (e.g. a test mock's record of sent packets).
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Sends a command and returns the reply slice, checking the command-id echo AND the reply's
    /// status.
    ///
    /// A MISMATCHED ECHO IS RESYNCHRONIZED, NOT REPORTED, and that is what makes a probe
    /// recoverable. Replies are strictly ordered one per command, so an echo that names a
    /// different command means the pipe still holds a reply nobody read -- which is what an
    /// earlier session leaves behind when it is killed between writing and reading. Failing on
    /// that error leaves the stale reply queued, so the NEXT session reads it, fails the same way,
    /// and leaves its own: the probe stays poisoned until it is physically unplugged, and the
    /// symptom is a pair of errors that alternate as each run shifts the queue by one. Discarding
    /// replies until the echo matches consumes the backlog and ends it.
    fn command(&mut self, request: &[u8]) -> Result<&[u8], DapError> {
        self.transport.write_packet(request)?;
        let want = request.first().copied().unwrap_or(0);
        let mut got = 0;
        let mut matched = None;
        let mut refused = false;
        for _ in 0..=STALE_REPLY_LIMIT {
            let n = match self.transport.read_packet(&mut self.reply) {
                Ok(n) => n,
                Err(_) if refused => return Err(DapError::Unsupported { command: want }),
                Err(e) => return Err(e.into()),
            };
            got = self.reply.first().copied().unwrap_or(0);
            if got == want {
                matched = Some(n);
                break;
            }
            refused |= got == proto::INVALID_COMMAND;
        }
        let n = match matched {
            Some(n) => n,
            None if refused => return Err(DapError::Unsupported { command: want }),
            None => return Err(DapError::Unexpected { expected: want, got }),
        };
        if proto::has_status_byte(want) {
            let status = self.reply.get(1).copied().unwrap_or(proto::DAP_ERROR);
            if status != proto::DAP_OK {
                return Err(DapError::Status { command: want, status });
            }
        }
        Ok(&self.reply[..n])
    }

    /// Reads a `DAP_Info` string from the probe itself (no target involved): `id` 0x01 vendor,
    /// 0x02 product, 0x03 serial, 0x04 CMSIS-DAP protocol version, 0x09 firmware version on
    /// probes that report one. Empty when the probe does not populate the id.
    pub fn info_string(&mut self, id: u8) -> Result<String, DapError> {
        let bytes = self.info_bytes(id)?;
        let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
        Ok(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }

    /// Reads a raw `DAP_Info` value -- the info block after the length byte -- for the numeric ids:
    /// 0xF0 Capabilities (a bitfield: bit 0 SWD, bit 1 JTAG, ...), 0xFB packet count, 0xFF packet
    /// size (u16, little-endian). The string ids are better read through
    /// [`info_string`](Self::info_string). Empty when the probe does not populate the id.
    pub fn info_bytes(&mut self, id: u8) -> Result<Vec<u8>, DapError> {
        let reply = self.command(&proto::info(id))?;
        let len = reply.get(1).copied().unwrap_or(0) as usize;
        Ok(reply.get(2..2 + len).unwrap_or(&[]).to_vec())
    }

    /// Asks the probe for `port` and confirms it actually selected it.
    ///
    /// `DAP_Connect` answers with the port it initialized rather than a status byte, and
    /// [`proto::CONNECT_FAILED`] is how it reports that it could not. That is a refusal in a field
    /// nothing was reading: a probe with no SWD support, or one whose mode is pinned to JTAG,
    /// answered "failed" and every subsequent transaction then failed for its own reasons, none of
    /// which named the connect.
    fn connect_port(&mut self, port: Port) -> Result<(), DapError> {
        let reply = self.command(&proto::connect(port))?;
        match reply.get(1).copied().unwrap_or(proto::CONNECT_FAILED) {
            proto::CONNECT_FAILED => Err(DapError::Device("probe could not initialize SWD mode")),
            got if got != port as u8 => Err(DapError::Device("probe selected a different port")),
            _ => Ok(()),
        }
    }

    /// Connects to the target over SWD: select the port, set the clock, then send the
    /// line-reset and JTAG-to-SWD switch sequence (ADIv5). Clocks at 1 MHz; see
    /// [`connect_swd_at`](Self::connect_swd_at) to choose the rate.
    pub fn connect_swd(&mut self) -> Result<(), DapError> {
        self.connect_swd_at(1_000_000)
    }

    /// [`connect_swd`](Self::connect_swd) at a chosen SWCLK frequency.
    ///
    /// Worth reaching for on a hand-wired link. Jumper leads with no ground return per signal are
    /// poor transmission lines, and a target that will not acknowledge at MHz rates often answers
    /// perfectly at a few hundred kHz -- a failure that looks exactly like bad wiring while the
    /// wiring is fine.
    pub fn connect_swd_at(&mut self, clock_hz: u32) -> Result<(), DapError> {
        self.connect_port(Port::Swd)?;
        self.command(&proto::swj_clock(clock_hz))?;
        self.command(&proto::swj_sequence(51, &[0xff; 7]))?;
        self.command(&proto::swj_sequence(16, &[0x9e, 0xe7]))?;
        self.command(&proto::swj_sequence(51, &[0xff; 7]))?;
        self.command(&proto::swj_sequence(8, &[0x00]))?;
        Ok(())
    }

    /// Connects to the target over SWD when it powers up in the DORMANT state (the Arm ADIv5.1 / ADIv6
    /// low-power debug state modern parts -- e.g. the RP2350 -- boot into, where the DP ignores the plain
    /// JTAG-to-SWD switch [`connect_swd`] sends). Leaving dormant takes the fixed 128-bit SELECTION ALERT
    /// followed by the SWD activation code, after which the DP is a normal reset-state SWD-DP and a
    /// [`read_idcode`](Self::read_idcode) confirms the link.
    ///
    /// The alert value and the `0x1A` activation code are Arm-architectural constants (every SWD-DP
    /// recognizes them); the bit stream is least-significant-bit-first, matching every other SWD sequence.
    ///
    /// This wakes the bus, not a particular port. On a part that puts several debug ports on ONE SWD bus
    /// every port stays tristated until one is addressed, so follow this with
    /// [`select_multidrop_target`](Self::select_multidrop_target). An ADIv6 DP, which addresses its APs
    /// by address rather than by the ADIv5 `APSEL` field, is not supported: this crate does not ship wire
    /// protocol it has not exercised against the part, because a plausible-looking sequence that is wrong
    /// costs more to find than an absent one.
    pub fn connect_swd_from_dormant(&mut self) -> Result<(), DapError> {
        self.connect_port(Port::Swd)?;
        self.command(&proto::swj_clock(1_000_000))?;
        self.command(&proto::swj_sequence(8, &[0xff]))?;
        self.command(&proto::swj_sequence(128, &DORMANT_TO_SWD_ALERT))?;
        self.command(&proto::swj_sequence(12, &[0xa0, 0x01]))?;
        self.command(&proto::swj_sequence(51, &[0xff; 7]))?;
        self.command(&proto::swj_sequence(8, &[0x00]))?;
        Ok(())
    }

    /// Releases the debug port: the probe stops driving SWCLK/SWDIO and the target is left to run.
    ///
    /// **A diagnostic that connects and never releases can leave a board unusable**, and it looks
    /// like the board's fault rather than the tool's: a debug port abandoned mid-transaction can
    /// stop the application running and the device enumerating, so the next person sees dead
    /// hardware rather than a tool that did not tidy up. Any tool that only inspects should end
    /// here, and one whose sequence may fail partway should end here on the failing path too.
    pub fn release(&mut self) -> Result<(), DapError> {
        self.command(&proto::disconnect())?;
        Ok(())
    }

    /// Reads the current levels of the probe's SWD pins, without driving anything.
    ///
    /// The one diagnostic that separates "the target is not answering" from "there is no target":
    /// every transaction failure looks identical on a bus with nothing attached, so a run that
    /// concludes anything about a wire protocol should be able to say the wire exists first.
    ///
    /// Bit 0 is SWCLK, bit 1 SWDIO, bit 7 nRESET (CMSIS-DAP `DAP_SWJ_Pins`).
    pub fn read_swd_pins(&mut self) -> Result<u8, DapError> {
        let reply = self.command(&proto::swj_pins(0, 0, 0))?;
        Ok(reply.get(1).copied().unwrap_or(0))
    }

    /// Selects ONE debug port on a shared multi-drop SWD bus, then confirms it answers.
    ///
    /// # Why this exists, and why it is a bit sequence rather than a transfer
    ///
    /// A part that puts several debug ports on one SWD bus gives each an address, and **every port
    /// tristates its outputs until one is addressed** -- so on such a part nothing responds at all
    /// until this runs, including the `DPIDR` read that would normally prove the link.
    ///
    /// The select write is the one DP write **nobody acknowledges**: the ports that were not
    /// addressed have gone quiet, and the one that was has not yet taken over the bus, so the three
    /// acknowledge bits are driven by no one. A normal `DAP_Transfer` reports that as a protocol
    /// fault, which is why this is driven as a raw bit sequence instead.
    ///
    /// # The sequence, and the one thing that has to be RELEASED rather than driven
    ///
    /// A line reset first (a select is only honored from the reset state) and at least two idle
    /// cycles, then the 8-bit write request for DP register `0x0C`, then five bits of
    /// turnaround-and-acknowledge, then the 32-bit address with even parity.
    pub fn select_multidrop_target(&mut self, target_id: u32) -> Result<u32, DapError> {
        use proto::SwdPhase;
        const SELECT_REQUEST: u8 = 0x99;

        self.command(&proto::swj_sequence(51, &[0xff; 7]))?;
        self.command(&proto::swj_sequence(8, &[0x00]))?;

        let mut data = target_id.to_le_bytes().to_vec();
        data.push(u8::from(target_id.count_ones() % 2 == 1));
        self.command(&proto::swd_sequence(&[
            SwdPhase::Out { cycles: 8, data: &[SELECT_REQUEST] },
            SwdPhase::In { cycles: 5 },
            SwdPhase::Out { cycles: 33, data: &data },
        ]))?;

        self.read_idcode()
    }

    /// Reads the Debug Port `IDCODE` (`DPIDR`) -- the first transaction after connecting,
    /// and the proof the link is alive.
    pub fn read_idcode(&mut self) -> Result<u32, DapError> {
        self.read_dp(0x0)
    }

    /// Writes `DAPABORT` to the DP ABORT register, aborting a stalled AP transaction. An AP
    /// transaction interrupted by a target reset mid-transfer survives even a line reset and
    /// leaves the DP answering WAIT to everything -- including the post-connect `DPIDR` read.
    /// The ABORT register is the architected way out (the DP accepts it while stalled); call
    /// this when a fresh connect sees a persistent WAIT, then retry.
    pub fn abort_stalled_transaction(&mut self) -> Result<(), DapError> {
        self.write_dp(0x0, 0x1)
    }

    /// Powers up the debug and system domains and configures the MEM-AP for 32-bit
    /// access. Call once after connecting, before any memory access.
    pub fn init_mem(&mut self) -> Result<(), DapError> {
        self.init_mem_select(0x0000_0000)
    }

    /// [`init_mem`](Self::init_mem) with a caller-supplied DP `SELECT` value -- an ADIv6 DP
    /// addresses its MEM-AP by base ADDRESS (plus the AP's register-file offset) instead of the
    /// ADIv5 `APSEL` field, e.g. `0x2d00` for the RP2350's core-0 AP at `0x2000` + the MEM-AP
    /// register file at `0xd00`.
    pub fn init_mem_select(&mut self, select: u32) -> Result<(), DapError> {
        self.write_dp(0x0, 0x0000_001e)?;
        self.write_dp(0x8, select)?;
        self.write_dp(0x4, 0x5000_0000)?;
        for _ in 0..128 {
            if self.read_dp(0x4)? & 0xa000_0000 == 0xa000_0000 {
                return self.write_ap(0x0, CSW_WORD);
            }
        }
        Err(DapError::Timeout("debug power-up"))
    }

    /// Configures the probe's transfer handling: `wait_retry` is how many times it retries a
    /// transfer answered `WAIT` before giving up -- raise it before an operation that stalls the
    /// target's DP briefly (a reset catch, a slow flash helper).
    pub fn configure_transfers(&mut self, idle_cycles: u8, wait_retry: u16, match_retry: u16) -> Result<(), DapError> {
        self.command(&proto::transfer_configure(idle_cycles, wait_retry, match_retry))?;
        Ok(())
    }

    /// Writes `words` to consecutive addresses starting at `address` through the MEM-AP,
    /// streaming them with `DAP_TransferBlock` -- the bulk path for staging an image in target
    /// RAM. The MEM-AP auto-increments `TAR` per word; the increment is only architecturally
    /// guaranteed within a 1 KB window (ADIv5), so `TAR` is rewritten at every 1 KB boundary,
    /// and blocks are sized to the probe packet.
    pub fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), DapError> {
        /// 64-byte packet: 5 header bytes + 14 x 4-byte values.
        const WORDS_PER_PACKET: usize = 14;
        let mut address = address;
        let mut remaining = words;
        while !remaining.is_empty() {
            let to_boundary = ((0x400 - (address & 0x3ff)) / 4) as usize;
            let count = remaining.len().min(WORDS_PER_PACKET).min(to_boundary);
            self.write_ap(0x4, address)?;
            let reply = self.command(&proto::transfer_block_write(proto::ap_write(0xC), &remaining[..count]))?;
            let (done, ack) = proto::parse_block_write(reply)?;
            if ack != Ack::Ok || done as usize != count {
                return Err(DapError::Ack(ack));
            }
            address += (count * 4) as u32;
            remaining = &remaining[count..];
        }
        Ok(())
    }

    /// Reads `count` consecutive words starting at `address` through the MEM-AP, streaming them
    /// with `DAP_TransferBlock` -- the bulk path for verifying a programmed image. Chunked like
    /// [`write_words`](Self::write_words) (probe packet size, 1 KB auto-increment windows).
    pub fn read_words(&mut self, address: u32, count: usize) -> Result<Vec<u32>, DapError> {
        /// 64-byte reply packet: 4 header bytes + 14 x 4-byte values (a `DAP_Transfer` block
        /// read resolves the posted AP read, so values come back in the same reply).
        const WORDS_PER_PACKET: usize = 14;
        let mut out = Vec::with_capacity(count);
        let mut address = address;
        let mut remaining = count;
        while remaining > 0 {
            let to_boundary = ((0x400 - (address & 0x3ff)) / 4) as usize;
            let batch = remaining.min(WORDS_PER_PACKET).min(to_boundary);
            self.write_ap(0x4, address)?;
            let reply = self.command(&proto::transfer_block_read(proto::ap_read(0xC), batch as u16))?;
            let (done, ack) = proto::parse_block_read(reply, &mut out)?;
            if ack != Ack::Ok || done as usize != batch {
                return Err(DapError::Ack(ack));
            }
            address += (batch * 4) as u32;
            remaining -= batch;
        }
        Ok(out)
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

    /// Reads one byte from target memory -- a true 8-bit bus access, which registers with
    /// byte-access semantics require (e.g. the SAMD21 GCLK's ID-indexed read windows, where a
    /// 32-bit access would clobber neighboring registers). Switches the MEM-AP CSW to byte size
    /// for the access and restores the 32-bit CSW afterward, even on failure.
    pub fn read_byte(&mut self, address: u32) -> Result<u8, DapError> {
        self.write_ap(0x0, CSW_BYTE)?;
        let lanes = self.write_ap(0x4, address).and_then(|()| self.read_ap(0xc));
        self.write_ap(0x0, CSW_WORD)?;
        Ok((lanes? >> (8 * (address & 3))) as u8)
    }

    /// Writes one byte to target memory (8-bit bus access, byte-lane placed). See
    /// [`read_byte`](Self::read_byte).
    pub fn write_byte(&mut self, address: u32, value: u8) -> Result<(), DapError> {
        self.write_ap(0x0, CSW_BYTE)?;
        let put = self
            .write_ap(0x4, address)
            .and_then(|()| self.write_ap(0xc, u32::from(value) << (8 * (address & 3))));
        self.write_ap(0x0, CSW_WORD)?;
        put
    }

    /// Reads a halfword from target memory (16-bit bus access, byte-lane shifted). See
    /// [`read_byte`](Self::read_byte).
    pub fn read_halfword(&mut self, address: u32) -> Result<u16, DapError> {
        self.write_ap(0x0, CSW_HALF)?;
        let lanes = self.write_ap(0x4, address).and_then(|()| self.read_ap(0xc));
        self.write_ap(0x0, CSW_WORD)?;
        Ok((lanes? >> (8 * (address & 2))) as u16)
    }

    /// Writes a halfword to target memory (16-bit bus access) -- e.g. a 16-bit peripheral data
    /// register a 32-bit write would overshoot. See [`read_byte`](Self::read_byte).
    pub fn write_halfword(&mut self, address: u32, value: u16) -> Result<(), DapError> {
        self.write_ap(0x0, CSW_HALF)?;
        let put = self
            .write_ap(0x4, address)
            .and_then(|()| self.write_ap(0xc, u32::from(value) << (8 * (address & 2))));
        self.write_ap(0x0, CSW_WORD)?;
        put
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
        self.write_word(FP_CTRL, 0b10)?;
        self.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_HALT | C_MASKINTS)?;
        self.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_STEP | C_MASKINTS)?;
        self.poll_dhcsr(S_HALT, "core halt")?;
        self.write_word(DHCSR, DBGKEY | C_DEBUGEN | C_HALT)?;
        self.write_word(FP_CTRL, 0b11)
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

    /// Resets the core (SYSRESETREQ) and resumes it, so it restarts from the reset
    /// vector -- the run step after flashing a fresh image.
    pub fn reset_and_run(&mut self) -> Result<(), DapError> {
        let _ = self.write_word(AIRCR, AIRCR_SYSRESETREQ);
        self.resume()
    }

    /// Arms halting debug and the reset VECTOR CATCH (`DEMCR.VC_CORERESET`, Armv6-M ARM
    /// DDI0419 C1.6 debug support): the next core reset -- from any source -- halts at the
    /// reset vector before the first instruction runs. Disarm with
    /// [`Dap::disarm_reset_catch`] once caught, or later resets keep halting.
    pub fn arm_reset_catch(&mut self) -> Result<(), DapError> {
        self.write_word(DHCSR, DBGKEY | C_DEBUGEN)?;
        self.write_word(DEMCR, VC_CORERESET)
    }

    /// Disarms the reset vector catch, so subsequent resets boot freely.
    pub fn disarm_reset_catch(&mut self) -> Result<(), DapError> {
        self.write_word(DEMCR, 0)
    }

    /// Waits (a bounded poll) for the core to report halted.
    pub fn wait_halted(&mut self) -> Result<(), DapError> {
        self.poll_dhcsr(S_HALT, "core halt")
    }

    /// Resets the core and CATCHES it halted at the reset vector, before the first
    /// instruction runs -- the attach for a target whose RUNNING firmware defeats a plain
    /// halt request, e.g. an armed watchdog resetting straight through one. The catch is
    /// disarmed again before returning, so a later [`Dap::reset_and_run`] boots freely.
    ///
    /// The arm happens under the probe's `nRESET` when the line works (the core is held --
    /// nothing runs, so nothing can race the arm-and-release); a probe with no reset line
    /// wired falls through to racing arm+`SYSRESETREQ` rounds. A family whose debug unit
    /// parks the core after an external reset needs its device-specific release instead
    /// (the SAM D21's cold-plugging reset extension lives in `lamella-cmsis-dap-sam`).
    pub fn reset_and_halt(&mut self) -> Result<(), DapError> {
        let _ = self.set_reset(true);
        let armed_held = self.arm_reset_catch().is_ok();
        let _ = self.set_reset(false);
        if armed_held && self.poll_dhcsr(S_HALT, "reset catch").is_ok() {
            return self.disarm_reset_catch();
        }
        for _ in 0..8 {
            if self.arm_reset_catch().is_ok() {
                let _ = self.write_word(AIRCR, AIRCR_SYSRESETREQ);
                if self.poll_dhcsr(S_HALT, "reset catch").is_ok() {
                    return self.disarm_reset_catch();
                }
            }
        }
        Err(DapError::Timeout("reset catch"))
    }

    /// Drives the target reset line (`nRESET`) via `DAP_SWJ_Pins`: `assert = true` holds the core in
    /// reset (drives the line low), `false` releases it (drives it high). Some probes -- the MCU-Link
    /// among them -- assert nRESET while idle, holding the target stopped even after a good SWD
    /// attach; releasing it lets the core run again (or, with halting debug already armed, boot
    /// straight into a halt). Waits up to 100 ms for the line to settle.
    pub fn set_reset(&mut self, assert: bool) -> Result<u8, DapError> {
        self.swj_pins(if assert { 0 } else { proto::PIN_NRESET }, proto::PIN_NRESET, 100_000)
    }

    /// Drives the SWJ pins named in `select` to the levels in `output`, waits up to `wait_us` for
    /// them to settle, and returns the read-back of ALL pins (`DAP_SWJ_Pins`).
    /// [`set_reset`](Self::set_reset) is this restricted to nRESET.
    ///
    /// The general form matters for diagnostics. Reading pins alone cannot tell a connected line
    /// from a disconnected one, nor establish that the probe's level shifters are enabled -- for
    /// that you must DRIVE a line and observe that it moved. Driving SWDIO low against a target's
    /// pull-up is the test: it succeeds only if the probe is really driving.
    pub fn swj_pins(&mut self, output: u8, select: u8, wait_us: u32) -> Result<u8, DapError> {
        let reply = self.command(&proto::swj_pins(output, select, wait_us))?;
        Ok(reply.get(1).copied().unwrap_or(0))
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

    /// Invokes the function at `addr` on the core and returns its `r0` -- the standard debugger technique
    /// for calling a target routine (e.g. an on-chip ROM or flash helper). Sets up an ARM call frame:
    /// `args` in r0-r3 (up to four used), a scratch stack at `frame.sp`, and LR pointing at a two-halfword
    /// `BKPT` planted at `frame.trap`; when the callee returns to LR it executes the `BKPT`, which -- with
    /// halting debug enabled -- raises a debug halt rather than a fault, and this catches it. Core-agnostic
    /// (the planted `BKPT` is the return catch, so it needs no chip-specific breakpoint unit) and it reuses
    /// only the already-verified halt/register/resume primitives.
    ///
    /// The caller supplies `frame.sp`/`frame.trap` because they are addresses in the target's RAM; the trap
    /// word is clobbered. It must also ensure no hardware breakpoint is armed inside the callee, which would
    /// halt it before the return trap. Core state is disrupted -- reset the core afterward to run normally.
    pub fn call_target(&mut self, addr: u32, args: &[u32], frame: &CallFrame) -> Result<u32, DapError> {
        self.halt()?;
        self.write_word(frame.trap, 0xbe00_be00)?;
        for i in 0..4u8 {
            self.write_core_reg(i, args.get(i as usize).copied().unwrap_or(0))?;
        }
        self.write_core_reg(13, frame.sp)?;
        self.write_core_reg(14, frame.trap | 1)?;
        self.write_core_reg(15, addr)?;
        self.write_core_reg(16, 0x0100_0000)?;
        self.resume()?;
        for _ in 0..frame.poll_tries {
            if self.is_halted()? {
                return self.read_core_reg(0);
            }
        }
        Err(DapError::Timeout("call_target: the callee did not return"))
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

impl From<Ack> for lamella_probe_core::Ack {
    fn from(ack: Ack) -> Self {
        match ack {
            Ack::Ok => lamella_probe_core::Ack::Ok,
            Ack::Wait => lamella_probe_core::Ack::Wait,
            Ack::Fault => lamella_probe_core::Ack::Fault,
            Ack::NoAck => lamella_probe_core::Ack::NoAck,
            Ack::Unknown(value) => lamella_probe_core::Ack::Unknown(value),
        }
    }
}

impl From<proto::ProtoError> for lamella_probe_core::ProbeError {
    fn from(error: proto::ProtoError) -> Self {
        lamella_probe_core::ProbeError::Protocol(format!("{error:?}"))
    }
}

impl From<DapError> for lamella_probe_core::ProbeError {
    fn from(error: DapError) -> Self {
        use lamella_probe_core::ProbeError as P;
        match error {
            DapError::Transport(e) => P::Transport(e.to_string()),
            DapError::Proto(e) => P::Protocol(format!("{e:?}")),
            DapError::Unexpected { expected, got } => {
                P::Protocol(format!("probe echoed command {got:#04x}, expected {expected:#04x}"))
            }
            DapError::Ack(ack) => P::Ack(ack.into()),
            DapError::Timeout(what) => P::Timeout(what),
            DapError::Device(what) => P::Device(what),
            refusal @ (DapError::Unsupported { .. } | DapError::Status { .. }) => {
                P::Protocol(refusal.to_string())
            }
        }
    }
}

/// The CMSIS-DAP probe as a raw ADIv5 DP/AP accessor -- the low-level seam.
///
/// Everything above this (MEM-AP memory access, Cortex-M run control) lives ONCE in
/// [`lamella_probe_core::ArmDap`] and is shared with every other low-level probe family, so an
/// FTDI-MPSSE JTAG probe gets it for free by implementing this trait alone.
impl<T: Transport> lamella_probe_core::DapAccess for Dap<T> {
    fn connect(&mut self) -> Result<(), lamella_probe_core::ProbeError> {
        Ok(self.connect_swd()?)
    }

    fn read_dp(&mut self, address: u8) -> Result<u32, lamella_probe_core::ProbeError> {
        Ok(Dap::read_dp(self, address)?)
    }

    fn write_dp(&mut self, address: u8, value: u32) -> Result<(), lamella_probe_core::ProbeError> {
        Ok(Dap::write_dp(self, address, value)?)
    }

    fn read_ap(&mut self, address: u8) -> Result<u32, lamella_probe_core::ProbeError> {
        Ok(Dap::read_ap(self, address)?)
    }

    fn write_ap(&mut self, address: u8, value: u32) -> Result<(), lamella_probe_core::ProbeError> {
        Ok(Dap::write_ap(self, address, value)?)
    }

    fn set_reset(&mut self, assert: bool) -> Result<u8, lamella_probe_core::ProbeError> {
        Ok(Dap::set_reset(self, assert)?)
    }

    /// Streams the values with `DAP_TransferBlock`, chunked to the probe's packet. The MEM-AP's
    /// 1 KB `TAR` auto-increment window is NOT handled here -- that is the caller's (MEM-AP) concern;
    /// this is purely "write these values to one AP register in as few round-trips as possible".
    fn write_ap_block(
        &mut self,
        address: u8,
        values: &[u32],
    ) -> Result<(), lamella_probe_core::ProbeError> {
        /// 64-byte packet: 5 header bytes + 14 x 4-byte values.
        const WORDS_PER_PACKET: usize = 14;
        for chunk in values.chunks(WORDS_PER_PACKET) {
            let reply = self.command(&proto::transfer_block_write(proto::ap_write(address), chunk))?;
            let (done, ack) = proto::parse_block_write(reply)?;
            if ack != Ack::Ok || done as usize != chunk.len() {
                return Err(lamella_probe_core::ProbeError::Ack(ack.into()));
            }
        }
        Ok(())
    }

    /// The read counterpart of [`write_ap_block`](Self::write_ap_block), streaming straight into the
    /// caller's buffer so a bulk read allocates nothing at any layer.
    fn read_ap_block_into(
        &mut self,
        address: u8,
        out: &mut [u32],
    ) -> Result<(), lamella_probe_core::ProbeError> {
        /// 64-byte reply packet: 4 header bytes + 14 x 4-byte values.
        const WORDS_PER_PACKET: usize = 14;
        let mut remaining = out;
        while !remaining.is_empty() {
            let batch = remaining.len().min(WORDS_PER_PACKET);
            let reply =
                self.command(&proto::transfer_block_read(proto::ap_read(address), batch as u16))?;
            let (done, ack) = proto::parse_block_read(reply, &mut remaining[..batch])?;
            if ack != Ack::Ok || done as usize != batch {
                return Err(lamella_probe_core::ProbeError::Ack(ack.into()));
            }
            remaining = &mut remaining[batch..];
        }
        Ok(())
    }
}

/// Test scaffolding for the probe protocol, shared by this crate's own tests and the device-specific
/// extension crates (`lamella-cmsis-dap-sam`, `lamella-cmsis-dap-nrf`), to which it is exposed behind the
/// `test-util` feature.
#[cfg(any(test, feature = "test-util"))]
pub mod testing {
    use crate::{Transport, TransportError};
    use std::collections::VecDeque;

    /// A [`Transport`] that returns canned reply packets and records every packet sent, so a test
    /// can drive a `Dap` against scripted probe replies and assert on the bytes it emitted. Read the
    /// log of sent packets through `Dap::transport`.
    pub struct Mock {
        replies: VecDeque<Vec<u8>>,
        /// Every packet written to the probe, in order.
        pub sent: Vec<Vec<u8>>,
    }

    impl Mock {
        /// Creates a mock that replies with `replies` in order.
        pub fn new(replies: Vec<Vec<u8>>) -> Mock {
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

    /// Builds a reply packet: a command id `id` followed by `rest`.
    pub fn echo(id: u8, rest: &[u8]) -> Vec<u8> {
        let mut v = vec![id];
        v.extend_from_slice(rest);
        v
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{Mock, echo};
    use super::*;
    use lamella_probe_core::DapAccess;

    /// A `DAP_TransferBlock` read reply: the command byte, the completed count, an OK acknowledge,
    /// then one little-endian word per completed transfer.
    fn block_reply(words: &[u32]) -> Vec<u8> {
        let mut v = vec![proto::cmd::TRANSFER_BLOCK];
        v.extend_from_slice(&(words.len() as u16).to_le_bytes());
        v.push(0x01);
        for word in words {
            v.extend_from_slice(&word.to_le_bytes());
        }
        v
    }

    /// A block read LONGER THAN ONE PROBE PACKET must land every word in its own slot, in order,
    /// across the packet boundary.
    ///
    /// This is the bulk path every flash program and verify rides, and nothing exercised
    /// it: an off-by-one in the chunking would corrupt a deploy while every unit test stayed green,
    /// and the first instrument to notice would be a flash verify failing on real silicon.
    #[test]
    fn block_read_spans_packets_and_preserves_order() {
        const PER_PACKET: usize = 14;
        const TOTAL: usize = 20;

        let expected: Vec<u32> = (0..TOTAL as u32).map(|i| 0xa000_0000 + i).collect();
        let replies = vec![block_reply(&expected[..PER_PACKET]), block_reply(&expected[PER_PACKET..])];

        let mut dap = Dap::new(Mock::new(replies));
        let mut out = [0u32; TOTAL];
        dap.read_ap_block_into(0x0c, &mut out).unwrap();

        assert_eq!(out.to_vec(), expected, "words must arrive in order across the packet boundary");

        let sent = &dap.transport().sent;
        assert_eq!(sent.len(), 2, "20 words is two packets");
        assert_eq!(sent[0], proto::transfer_block_read(proto::ap_read(0x0c), PER_PACKET as u16).to_vec());
        assert_eq!(
            sent[1],
            proto::transfer_block_read(proto::ap_read(0x0c), (TOTAL - PER_PACKET) as u16).to_vec()
        );
    }

    /// A reply carrying more words than the caller's buffer is refused, never written past the end.
    #[test]
    fn block_read_refuses_a_reply_longer_than_the_buffer() {
        let mut out = [0u32; 2];
        let reply = block_reply(&[1, 2, 3, 4]);
        assert!(proto::parse_block_read(&reply, &mut out).is_err());
    }

    #[test]
    fn block_read_refuses_a_short_transfer_and_leaves_no_partial_fill() {
        const SENTINEL: u32 = 0xdead_beef;
        let mut out = [SENTINEL; 4];
        let reply = block_reply(&[0x1111_1111, 0x2222_2222]);
        let mut dap = Dap::new(Mock::new(vec![reply]));

        let result = dap.read_ap_block_into(0x0c, &mut out);

        assert!(result.is_err(), "a transfer count below the request must not be reported as success");
        assert_eq!(
            out[2..],
            [SENTINEL, SENTINEL],
            "the tail the probe never sent must not be presented as data"
        );
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
    fn dormant_connect_emits_the_selection_alert_then_reads_idcode() {
        let replies = vec![
            echo(proto::cmd::CONNECT, &[Port::Swd as u8]),
            echo(proto::cmd::SWJ_CLOCK, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            echo(proto::cmd::SWJ_SEQUENCE, &[0x00]),
            vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x77, 0x14, 0xb1, 0x0b],
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.connect_swd_from_dormant().unwrap();
        assert_eq!(dap.read_idcode().unwrap(), 0x0bb1_1477);
        let alert = &dap.transport.sent[3];
        assert_eq!(alert[0], proto::cmd::SWJ_SEQUENCE);
        assert_eq!(alert[1], 128, "bit count");
        assert_eq!(&alert[2..18], &DORMANT_TO_SWD_ALERT, "the Arm dormant-to-SWD alert value");
        let activation = &dap.transport.sent[4];
        assert_eq!(activation[1], 12, "bit count");
        assert_eq!(&activation[2..4], &[0xa0, 0x01], "4 low + activation 0x1A");
    }

    #[test]
    fn a_stale_reply_is_discarded_and_the_command_still_answers() {
        let replies = vec![
            echo(proto::cmd::SWJ_CLOCK, &[0x00]),
            vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x77, 0x14, 0xb1, 0x0b],
        ];
        let mut dap = Dap::new(Mock::new(replies));
        assert_eq!(dap.read_idcode().unwrap(), 0x0bb1_1477);
    }

    #[test]
    fn a_stale_refusal_is_still_only_backlog_when_the_real_echo_is_behind_it() {
        let replies = vec![
            vec![proto::INVALID_COMMAND, 0, 0],
            vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x77, 0x14, 0xb1, 0x0b],
        ];
        let mut dap = Dap::new(Mock::new(replies));
        assert_eq!(dap.read_idcode().unwrap(), 0x0bb1_1477);
    }

    #[test]
    fn a_probe_that_never_echoes_the_command_is_still_an_error() {
        let replies = vec![vec![proto::cmd::TRANSFER_BLOCK, 0, 0]; STALE_REPLY_LIMIT + 1];
        let mut dap = Dap::new(Mock::new(replies));
        assert!(matches!(dap.read_idcode(), Err(DapError::Unexpected { .. })));
    }

    #[test]
    fn an_unimplemented_command_is_named_rather_than_timing_out() {
        let mut dap = Dap::new(Mock::new(vec![
            echo(proto::cmd::SWJ_SEQUENCE, &[proto::DAP_OK]),
            echo(proto::cmd::SWJ_SEQUENCE, &[proto::DAP_OK]),
            vec![proto::INVALID_COMMAND, 0x00],
        ]));
        let err = dap.select_multidrop_target(0x0100_2927).unwrap_err();
        assert!(
            matches!(err, DapError::Unsupported { command } if command == proto::cmd::SWD_SEQUENCE),
            "expected an Unsupported naming DAP_SWD_Sequence, got: {err}"
        );
    }

    #[test]
    fn a_dap_error_status_fails_the_command_that_earned_it() {
        let replies = vec![
            echo(proto::cmd::CONNECT, &[Port::Swd as u8]),
            echo(proto::cmd::SWJ_CLOCK, &[proto::DAP_ERROR]),
        ];
        let mut dap = Dap::new(Mock::new(replies));
        let err = dap.connect_swd().unwrap_err();
        assert!(
            matches!(err, DapError::Status { command, status }
                if command == proto::cmd::SWJ_CLOCK && status == proto::DAP_ERROR),
            "expected a Status naming DAP_SWJ_Clock, got: {err}"
        );
    }

    #[test]
    fn the_status_check_does_not_fire_on_a_reply_whose_second_byte_is_not_a_status() {
        let mut dap = Dap::new(Mock::new(vec![
            echo(proto::cmd::INFO, &[0x04, b'v', b'1', b'.', b'0']),
            echo(proto::cmd::SWJ_PINS, &[proto::PIN_SWDIO]),
            vec![proto::cmd::TRANSFER, 0x01, 0x01, 0x77, 0x14, 0xb1, 0x0b],
            echo(proto::cmd::CONNECT, &[Port::Swd as u8]),
        ]));
        assert_eq!(dap.info_string(0x01).unwrap(), "v1.0");
        assert_eq!(dap.read_swd_pins().unwrap(), proto::PIN_SWDIO);
        assert_eq!(dap.read_idcode().unwrap(), 0x0bb1_1477);
        dap.connect_port(Port::Swd).unwrap();
    }

    #[test]
    fn reading_the_swd_pins_returns_the_levels_and_not_the_echoed_command_id() {
        let mut dap = Dap::new(Mock::new(vec![echo(
            proto::cmd::SWJ_PINS,
            &[proto::PIN_SWDIO | proto::PIN_NRESET],
        )]));
        let pins = dap.read_swd_pins().unwrap();
        assert_eq!(pins, proto::PIN_SWDIO | proto::PIN_NRESET);
        assert_ne!(pins, proto::cmd::SWJ_PINS, "the echoed command id, not the pin levels");
    }

    #[test]
    fn a_probe_that_could_not_select_swd_says_so_at_the_connect() {
        let mut dap = Dap::new(Mock::new(vec![echo(
            proto::cmd::CONNECT,
            &[proto::CONNECT_FAILED],
        )]));
        assert!(matches!(dap.connect_swd(), Err(DapError::Device(_))));
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
    fn call_target_sets_the_frame_and_returns_r0() {
        fn ack(r: &mut Vec<Vec<u8>>) {
            r.push(vec![proto::cmd::TRANSFER, 0x01, 0x01]);
        }
        fn read(r: &mut Vec<Vec<u8>>, v: u32) {
            let mut p = vec![proto::cmd::TRANSFER, 0x01, 0x01];
            p.extend_from_slice(&v.to_le_bytes());
            r.push(p);
        }
        fn write_word(r: &mut Vec<Vec<u8>>) {
            ack(r);
            ack(r);
        }
        fn read_word(r: &mut Vec<Vec<u8>>, v: u32) {
            ack(r);
            read(r, v);
        }
        fn write_core_reg(r: &mut Vec<Vec<u8>>) {
            write_word(r);
            write_word(r);
            read_word(r, 0x0001_0000);
        }

        let mut replies: Vec<Vec<u8>> = Vec::new();
        write_word(&mut replies);
        write_word(&mut replies);
        for _ in 0..8 {
            write_core_reg(&mut replies);
        }
        write_word(&mut replies);
        read_word(&mut replies, 0x0002_0000);
        write_word(&mut replies);
        read_word(&mut replies, 0x0001_0000);
        read_word(&mut replies, 42);

        let frame = CallFrame {
            sp: 0x2004_0000,
            trap: 0x2000_0000,
            poll_tries: 4,
        };
        let mut dap = Dap::new(Mock::new(replies));
        assert_eq!(dap.call_target(0x0000_1000, &[7], &frame).unwrap(), 42);

        let sent = &dap.transport().sent;
        let wrote = |v: u32| sent.iter().any(|p| p.len() >= 8 && p[4..8] == v.to_le_bytes());
        assert!(wrote(0xbe00_be00), "planted BKPT;BKPT at the return trap");
        assert!(wrote(0x0000_1000), "PC = the function address");
        assert!(wrote(0x2000_0001), "LR = trap | Thumb bit");
        assert!(wrote(0x2004_0000), "SP = the scratch stack");
        assert!(wrote(0x0100_0000), "xPSR = Thumb state");
        assert!(wrote(7), "r0 = the first argument");
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
            ack.clone(),
            ack.clone(),
            halted,
            ack.clone(),
            ack.clone(),
            ack.clone(),
            ack.clone(),
        ];
        let mut dap = Dap::new(Mock::new(replies));
        dap.step().unwrap();
        assert_eq!(&dap.transport.sent[3][4..8], &0xa05f_000bu32.to_le_bytes());
        assert_eq!(&dap.transport.sent[5][4..8], &0xa05f_000du32.to_le_bytes());
        assert_eq!(&dap.transport.sent[9][4..8], &0xa05f_0003u32.to_le_bytes());
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
