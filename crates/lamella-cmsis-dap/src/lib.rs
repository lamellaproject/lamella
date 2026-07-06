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

/// The 128-bit dormant-to-SWD SELECTION ALERT (Arm ADIv5.1 / ADIv6): the fixed value every SWD-DP
/// recognizes to leave the dormant state, least-significant bit first. Followed by the 8-bit SWD
/// activation code `0x1A`. Used by [`Dap::connect_swd_from_dormant`].
const DORMANT_TO_SWD_ALERT: [u8; 16] = [
    0x92, 0xf3, 0x09, 0x62, 0x95, 0x2d, 0x85, 0x86, 0xe9, 0xaf, 0xdd, 0xe3, 0xa2, 0x0e, 0xbc, 0x19,
];

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

    /// Connects to the target over SWD when it powers up in the DORMANT state (the Arm ADIv5.1 / ADIv6
    /// low-power debug state modern parts -- e.g. the RP2350 -- boot into, where the DP ignores the plain
    /// JTAG-to-SWD switch [`connect_swd`] sends). Leaving dormant takes the fixed 128-bit SELECTION ALERT
    /// followed by the SWD activation code, after which the DP is a normal reset-state SWD-DP and a
    /// [`read_idcode`](Self::read_idcode) confirms the link.
    ///
    /// The alert value and the `0x1A` activation code are Arm-architectural constants (every SWD-DP
    /// recognizes them); the bit stream is least-significant-bit-first, matching every other SWD sequence.
    /// This wakes a SINGLE debug port; a part that puts several targets on ONE SWD bus additionally needs a
    /// DP `TARGETSEL` write to pick one (multi-drop), and an ADIv6 DP addresses its APs by address rather
    /// than the ADIv5 `APSEL` field.
    pub fn connect_swd_from_dormant(&mut self) -> Result<(), DapError> {
        self.command(&proto::connect(Port::Swd))?;
        self.command(&proto::swj_clock(1_000_000))?;
        self.command(&proto::swj_sequence(8, &[0xff]))?;
        self.command(&proto::swj_sequence(128, &DORMANT_TO_SWD_ALERT))?;
        self.command(&proto::swj_sequence(12, &[0xa0, 0x01]))?;
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
