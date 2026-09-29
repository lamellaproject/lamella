//! The gSPI transport: the chip's serial host interface, as wired on the
//! Pico family boards, above a four-operation wire that an engine clocks.
//!
//! Every transfer is one chip-select frame: a 32-bit command word, then data
//! out or data in (Infineon CYW43439 datasheet, section 4.2.1). The bus
//! moves 16- or 32-bit words with a configurable byte order; at power-up it
//! is in the 16-bit little-endian mode, and the attach switches it to the
//! 32-bit big-endian mode, in which a byte stream travels in natural order
//! (datasheet section 4.2.1, Table 6). Function-1 reads carry a
//! response-delay word before their data (Table 6, the response delay
//! registers). The bus's state -- the mode, the attach phase -- is this
//! transport's alone; the wire below it knows nothing but bytes and the two
//! control lines.
//!
//! A wire whose engine can move its sample instant and its bit rate offers
//! those as settings, and this transport tunes them once the chip's RAM is
//! writable: the eye of the data line is measured on real function-1
//! traffic and the fastest setting with enough margin on both sides of the
//! sample instant is kept.

use crate::clock::Micros;
use crate::error::Refusal;
use crate::transport::{Attach, Func, Part, Transport, Tune};

/// One bus setting a wire offers for tuning: the sample step (one cycle of
/// the engine's clock) in picoseconds, the steps in one bit period, the
/// earliest sample phase in steps relative to the clock's rising edge
/// (negative before it), and the number of phases from `first` upward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setting {
    /// The sample step in picoseconds.
    pub step_ps: u32,
    /// The steps in one bit period.
    pub steps_per_bit: u8,
    /// The earliest sample phase, in steps relative to the rising edge.
    pub first: i8,
    /// The number of phases offered, from `first` upward.
    pub phases: u8,
}

impl Setting {
    /// No setting: a wire with nothing to tune.
    pub const NONE: Setting = Setting {
        step_ps: 0,
        steps_per_bit: 0,
        first: 0,
        phases: 0,
    };

    /// The bit period in picoseconds.
    pub const fn period_ps(&self) -> u32 {
        self.step_ps * self.steps_per_bit as u32
    }

    /// The last sample phase offered.
    pub const fn last(&self) -> i8 {
        (self.first as i16 + self.phases as i16 - 1) as i8
    }
}

/// The wire: one chip-select frame at a time, clocked by an engine (a PIO
/// state machine, an SPI peripheral, a bit-bang) or replayed by a fake.
pub trait GspiWire {
    /// Drive the module's power-enable line.
    fn set_power(&mut self, on: bool);

    /// Hold the shared data line low as the interface-mode strap (the chip
    /// samples it at power-on), or release the line to the bus.
    fn strap(&mut self, hold: bool);

    /// One chip-select frame: the command word and `tx` clocked out in wire
    /// order, the line turned around, and `rx.len()` bytes clocked in. Byte
    /// counts are exact; the engine rounds the wire to whole words and pads
    /// or discards.
    fn frame(&mut self, cmd: [u8; 4], tx: &[u8], rx: &mut [u8]) -> Result<(), Refusal>;

    /// Whether the data line is high while the bus is idle: the chip's
    /// interrupt.
    fn irq_asserted(&mut self) -> bool;

    /// How many bus settings the wire offers for tuning, slowest first; 0
    /// when there is nothing to tune (the default).
    fn settings(&self) -> u8 {
        0
    }

    /// The `index`th setting.
    fn setting(&self, index: u8) -> Setting {
        let _ = index;
        Setting::NONE
    }

    /// Select a setting and a sample phase for every frame that follows.
    fn select(&mut self, index: u8, phase: i8) {
        let _ = (index, phase);
    }

    /// The setting and phase in force.
    fn selected(&self) -> (u8, i8) {
        (0, 0)
    }
}

/// The bytes of the tuning pattern: one function-1 transfer.
pub const TUNE_BYTES: usize = 64;
/// The read-backs of the pattern at each sample phase.
pub const TUNE_READS: u8 = 8;
/// The margin the sample instant must have on both sides of a bit's valid
/// window, in picoseconds: 24 ns.
pub const TUNE_MARGIN_PS: u32 = 24_000;
/// The most settings the tuning keeps a row for.
pub const TUNE_SETTINGS_MAX: usize = 4;
/// The most phases a setting may offer.
pub const TUNE_PHASES_MAX: usize = 16;
/// The stage name of a tuning that failed its proof, or a wire whose
/// settings exceed the rows.
pub const STAGE_TUNING: &str = "gSPI tuning";

const TUNE_TAG_SWEEP: u8 = 0x40;
const TUNE_TAG_PROOF: u8 = 0x3C;

/// The tuning pattern for a tag: every bit position toggles within a few
/// bytes and no byte repeats its neighbour.
pub fn tune_pattern(tag: u8) -> [u8; TUNE_BYTES] {
    let mut pattern = [0u8; TUNE_BYTES];
    for (i, b) in pattern.iter_mut().enumerate() {
        let salt = if (i / 2) % 2 == 0 { 0x0F } else { 0xF0 };
        *b = (i as u8).wrapping_mul(99) ^ tag ^ salt;
    }
    pattern
}

/// The eye's verdict on one setting: the phase to sample at and the margins
/// before and after the sample instant, in picoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    /// The sample phase chosen.
    pub phase: i8,
    /// The margin from the window's start to the sample instant.
    pub before_ps: u32,
    /// The margin from the sample instant to the window's end.
    pub after_ps: u32,
}

fn div_floor(a: i64, b: i64) -> i64 {
    let q = a / b;
    if a % b != 0 && (a < 0) != (b < 0) {
        q - 1
    } else {
        q
    }
}

/// The rule, on one setting's row (the mismatched bytes per phase from
/// `setting.first` upward; 0xFFFF a refused read). The chip drives each bit
/// from about 25 ns after the previous rising edge until about 25 ns after
/// its own, so a bit's valid window is one period wide and ends a little
/// after its edge. The longest clean run's last phase marks the window's end
/// (the transition lies within one step after it) and it must be seen: a
/// run that reaches the family's last phase leaves the end unmeasured and
/// the setting is rejected. The window's start is one period earlier; the
/// sample is placed at the phase nearest the window's centre (a tie toward
/// the earlier phase), clamped into the clean run; the setting is accepted
/// when both margins reach [`TUNE_MARGIN_PS`].
pub fn judge(setting: Setting, row: &[u16]) -> Option<Verdict> {
    let n = row.len().min(setting.phases as usize);
    let (mut best_start, mut best_len, mut run_start, mut run_len) =
        (0usize, 0usize, 0usize, 0usize);
    for (i, &m) in row[..n].iter().enumerate() {
        if m == 0 {
            if run_len == 0 {
                run_start = i;
            }
            run_len += 1;
            if run_len > best_len {
                best_start = run_start;
                best_len = run_len;
            }
        } else {
            run_len = 0;
        }
    }
    if best_len == 0 || best_start + best_len >= n {
        return None;
    }
    let step = i64::from(setting.step_ps);
    let period = i64::from(setting.period_ps());
    let run_first = i64::from(setting.first) + best_start as i64;
    let run_last = run_first + best_len as i64 - 1;
    let end = run_last * step;
    let centre = end - period / 2;
    let mut phase = div_floor(centre + step / 2 - 1, step);
    if phase < run_first {
        phase = run_first;
    }
    if phase > run_last {
        phase = run_last;
    }
    let after = end - phase * step;
    let before = phase * step - (end - period);
    if after < i64::from(TUNE_MARGIN_PS) || before < i64::from(TUNE_MARGIN_PS) {
        return None;
    }
    Some(Verdict {
        phase: phase as i8,
        before_ps: before as u32,
        after_ps: after as u32,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TunePhase {
    Idle,
    Write { s: u8 },
    Read { s: u8, p: u8, read: u8 },
    Choose,
    Prove { n: u8 },
}

#[derive(Clone, Copy, Debug)]
struct Tuning {
    phase: TunePhase,
    rows: [[u16; TUNE_PHASES_MAX]; TUNE_SETTINGS_MAX],
    count: u8,
    fallback: (u8, i8),
    chosen: Option<(u8, i8)>,
}

impl Tuning {
    const fn new() -> Self {
        Tuning {
            phase: TunePhase::Idle,
            rows: [[0; TUNE_PHASES_MAX]; TUNE_SETTINGS_MAX],
            count: 0,
            fallback: (0, 0),
            chosen: None,
        }
    }
}

/// Function-0 registers: the gSPI register file (datasheet Table 6).
pub mod f0 {
    /// Configuration: word length, byte order, high-speed mode, interrupt
    /// polarity, wake-up (four bytes spanning 0x00 to 0x03).
    pub const CONFIG: u32 = 0x00;
    /// The 16-bit interrupt register, write-1-to-clear.
    pub const INTERRUPT: u32 = 0x04;
    /// The 16-bit interrupt enable register.
    pub const INTERRUPT_ENABLE: u32 = 0x06;
    /// The 32-bit status register.
    pub const STATUS: u32 = 0x08;
    /// The 16-bit function-1 information register.
    pub const F1_INFO: u32 = 0x0C;
    /// The 16-bit function-2 information register.
    pub const F2_INFO: u32 = 0x0E;
    /// The read-only test register, fixed at 0xFEEDBEAD.
    pub const TEST_READ: u32 = 0x14;
    /// The read-write test register.
    pub const TEST_SCRATCH: u32 = 0x18;
    /// The function-1 response delay in bytes.
    pub const RESPONSE_DELAY_F1: u32 = 0x1D;
}

/// Bits of the status word (datasheet Table 5).
pub mod status {
    /// The requested read data is not available.
    pub const DATA_NOT_AVAILABLE: u32 = 1 << 0;
    /// FIFO underflow on the current function-2 read.
    pub const UNDERFLOW: u32 = 1 << 1;
    /// FIFO overflow on the current write.
    pub const OVERFLOW: u32 = 1 << 2;
    /// The function-2 channel interrupt.
    pub const F2_INTERRUPT: u32 = 1 << 3;
    /// The chip's function-2 receive FIFO can accept host data.
    pub const F2_RX_READY: u32 = 1 << 5;
    /// A function-2 frame is ready for the host to read.
    pub const F2_PACKET_AVAILABLE: u32 = 1 << 8;
    /// The shift of the available frame's length.
    pub const F2_PACKET_LEN_SHIFT: u32 = 9;
    /// The mask of the available frame's length, 11 bits.
    pub const F2_PACKET_LEN_MASK: u32 = 0x7FF;
}

/// Bits of the function-2 information register (datasheet Table 6,
/// register 0x0E).
pub mod f2info {
    /// Function 2 is enabled.
    pub const ENABLED: u16 = 1 << 0;
    /// Function 2 is ready for data transfer.
    pub const READY: u16 = 1 << 1;
}

/// The test register's fixed pattern.
pub const TEST_PATTERN: u32 = 0xFEED_BEAD;

/// The configuration dword written at attach: byte 0 selects 32-bit words,
/// big-endian wire order, high-speed mode, an active-high interrupt and the
/// wake-up; byte 2 selects the interrupt with status and leaves the appended
/// status word off, so the status word is read on demand.
pub const CONFIG_DWORD: u32 = 0x0002_00B3;

/// The interrupt latches cleared at attach: data not available, command
/// error, data error, function-1 overflow.
pub const INTERRUPT_CLEAR: u16 = 0x0099;

/// The one interrupt enabled at attach: a function-2 frame is available.
pub const INTERRUPT_ENABLE_F2: u16 = 0x0020;

/// The function-1 response delay in bytes, made explicit at attach.
pub const RESPONSE_DELAY_F1: u8 = 4;

/// The value written to and read back from the scratch register at attach.
pub const SCRATCH_PATTERN: u32 = 0x1F2E_3D4C;

/// The stage name of a test register that never returned its pattern.
pub const STAGE_TEST_PATTERN: &str = "gSPI test pattern";
/// The stage name of a bus that failed the test and scratch pair after the
/// configuration write.
pub const STAGE_BUS_VERIFICATION: &str = "gSPI bus verification";
/// The stage name of an interrupt enable that did not read back.
pub const STAGE_INTERRUPT_ENABLE: &str = "gSPI interrupt enable";
/// The stage name of a transfer the transport refused before the wire.
pub const STAGE_TRANSFER: &str = "gSPI transfer";

/// The power-off hold before the module is powered.
const POWER_OFF_HOLD_US: Micros = 50_000;
/// The settle after power-on before the first bus access.
const POWER_ON_SETTLE_US: Micros = 50_000;
/// Test-pattern polls: ten tries, ten milliseconds apart.
const PATTERN_TRIES: u8 = 10;
const PATTERN_RETRY_US: Micros = 10_000;

/// The longest transfer the command word can name.
const MAX_LEN: usize = 2047;
/// The longest function-1 transfer.
const F1_MAX: usize = 64;

/// The command word (datasheet section 4.2.1, Figure 12): bit 31 write, bit
/// 30 incremental address, bits 29:28 the function, bits 27:11 the address,
/// bits 10:0 the length in bytes.
pub(crate) const fn command_word(
    write: bool,
    incr: bool,
    func: Func,
    addr: u32,
    len: usize,
) -> u32 {
    (if write { 1 << 31 } else { 0 })
        | (if incr { 1 << 30 } else { 0 })
        | ((func as u8 as u32) << 28)
        | ((addr & 0x1_FFFF) << 11)
        | (len as u32 & 0x7FF)
}

/// The power-up wire order, 16-bit little-endian: for a value whose bytes
/// are B3 B2 B1 B0, the wire carries B1 B0 B3 B2.
pub(crate) const fn encode_default(value: u32) -> [u8; 4] {
    let b = value.to_le_bytes();
    [b[1], b[0], b[3], b[2]]
}

/// The inverse of `encode_default`.
pub(crate) const fn decode_default(wire: [u8; 4]) -> u32 {
    u32::from_le_bytes([wire[1], wire[0], wire[3], wire[2]])
}

/// The configured wire order, 32-bit big-endian: the wire carries
/// B0 B1 B2 B3, the least significant byte first.
pub(crate) const fn encode_configured(value: u32) -> [u8; 4] {
    value.to_le_bytes()
}

/// The inverse of `encode_configured`.
pub(crate) const fn decode_configured(wire: [u8; 4]) -> u32 {
    u32::from_le_bytes(wire)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Start,
    PowerOff { until: Micros },
    PowerOn { until: Micros },
    Pattern { tries: u8, next: Micros },
    Config,
    Verify,
    Interrupts,
    Ready,
}

/// The gSPI transport above a wire.
#[derive(Debug)]
pub struct Gspi<W: GspiWire> {
    wire: W,
    configured: bool,
    phase: Phase,
    part: Part,
    tuning: Tuning,
}

impl<W: GspiWire> Gspi<W> {
    /// A transport above `wire` to a CYW43439, in the power-up state.
    pub const fn new(wire: W) -> Self {
        Self::for_part(wire, Part::Cyw43439)
    }

    /// A transport above `wire` to `part`, in the power-up state.
    pub const fn for_part(wire: W, part: Part) -> Self {
        Gspi {
            wire,
            configured: false,
            phase: Phase::Start,
            part,
            tuning: Tuning::new(),
        }
    }

    /// The part this transport is wired to.
    pub fn part(&self) -> Part {
        self.part
    }

    /// The eye measured by the last tuning: the mismatched bytes per
    /// setting and phase (0xFFFF a refused read); the rows beyond the wire's
    /// settings and phases are zero.
    pub fn eye(&self) -> &[[u16; TUNE_PHASES_MAX]; TUNE_SETTINGS_MAX] {
        &self.tuning.rows
    }

    /// The setting and phase the last tuning chose; `None` when no setting
    /// passed the rule and the wire's own setting stands.
    pub fn tuned(&self) -> Option<(u8, i8)> {
        self.tuning.chosen
    }

    /// One step of the tuning sub-machine: one frame per call.
    fn tune_step(&mut self, scratch: u32, now: Micros) -> Result<Tune, Refusal> {
        let again = Ok(Tune::Pending { until: now });
        match self.tuning.phase {
            TunePhase::Idle => {
                let count = self.wire.settings();
                if count == 0 {
                    return Ok(Tune::Done);
                }
                if count as usize > TUNE_SETTINGS_MAX {
                    return Err(Refusal::new(STAGE_TUNING, u32::from(count)));
                }
                for s in 0..count {
                    let setting = self.wire.setting(s);
                    let bad = setting.phases == 0
                        || setting.phases as usize > TUNE_PHASES_MAX
                        || setting.step_ps == 0
                        || setting.steps_per_bit == 0;
                    if bad {
                        return Err(Refusal::new(STAGE_TUNING, u32::from(s)));
                    }
                }
                self.tuning = Tuning {
                    phase: TunePhase::Write { s: 0 },
                    rows: [[0; TUNE_PHASES_MAX]; TUNE_SETTINGS_MAX],
                    count,
                    fallback: self.wire.selected(),
                    chosen: None,
                };
                again
            }
            TunePhase::Write { s } => {
                let setting = self.wire.setting(s);
                self.wire.select(s, setting.first);
                let pattern = tune_pattern(TUNE_TAG_SWEEP + s);
                self.write_extended(Func::F1, scratch, true, &pattern)?;
                self.tuning.phase = TunePhase::Read { s, p: 0, read: 0 };
                again
            }
            TunePhase::Read { s, p, read } => {
                let setting = self.wire.setting(s);
                if read == 0 && p != 0 {
                    self.wire
                        .select(s, (i16::from(setting.first) + i16::from(p)) as i8);
                }
                let pattern = tune_pattern(TUNE_TAG_SWEEP + s);
                let mut buf = [0u8; TUNE_BYTES];
                let phase_done = match self.read_extended(Func::F1, scratch, true, &mut buf) {
                    Ok(()) => {
                        let bad = buf
                            .iter()
                            .zip(pattern.iter())
                            .filter(|(a, b)| a != b)
                            .count();
                        let row = &mut self.tuning.rows[s as usize][p as usize];
                        *row = row.saturating_add(bad as u16);
                        read + 1 >= TUNE_READS
                    }
                    Err(_) => {
                        self.tuning.rows[s as usize][p as usize] = 0xFFFF;
                        true
                    }
                };
                self.tuning.phase = if !phase_done {
                    TunePhase::Read {
                        s,
                        p,
                        read: read + 1,
                    }
                } else if p + 1 < setting.phases {
                    TunePhase::Read {
                        s,
                        p: p + 1,
                        read: 0,
                    }
                } else if s + 1 < self.tuning.count {
                    TunePhase::Write { s: s + 1 }
                } else {
                    TunePhase::Choose
                };
                again
            }
            TunePhase::Choose => {
                let mut chosen = None;
                for s in 0..self.tuning.count {
                    let setting = self.wire.setting(s);
                    let row = &self.tuning.rows[s as usize][..setting.phases as usize];
                    if let Some(verdict) = judge(setting, row) {
                        chosen = Some((s, verdict.phase));
                    }
                }
                let (s, phase) = chosen.unwrap_or(self.tuning.fallback);
                self.wire.select(s, phase);
                self.tuning.chosen = chosen;
                self.tuning.phase = TunePhase::Prove { n: 0 };
                again
            }
            TunePhase::Prove { n } => {
                let pattern = tune_pattern(TUNE_TAG_PROOF);
                match n {
                    0 => self.write_extended(Func::F1, scratch, true, &pattern)?,
                    1 | 2 => {
                        let mut buf = [0u8; TUNE_BYTES];
                        self.read_extended(Func::F1, scratch, true, &mut buf)?;
                        if let Some(i) = (0..TUNE_BYTES).find(|&i| buf[i] != pattern[i]) {
                            self.tuning.phase = TunePhase::Idle;
                            return Err(Refusal::new(STAGE_TUNING, scratch + i as u32));
                        }
                    }
                    _ => {
                        let test = self.read32(f0::TEST_READ)?;
                        self.tuning.phase = TunePhase::Idle;
                        if test != TEST_PATTERN {
                            return Err(Refusal::new(STAGE_TUNING, test));
                        }
                        return Ok(Tune::Done);
                    }
                }
                self.tuning.phase = TunePhase::Prove { n: n + 1 };
                again
            }
        }
    }

    /// The wire.
    pub fn wire(&self) -> &W {
        &self.wire
    }

    /// The wire, mutably.
    pub fn wire_mut(&mut self) -> &mut W {
        &mut self.wire
    }

    /// Give the wire back.
    pub fn into_wire(self) -> W {
        self.wire
    }

    /// Whether the bus is in the configured 32-bit big-endian mode.
    pub fn is_configured(&self) -> bool {
        self.configured
    }

    fn cmd(&self, write: bool, func: Func, addr: u32, len: usize) -> [u8; 4] {
        let word = command_word(write, true, func, addr, len);
        if self.configured {
            encode_configured(word)
        } else {
            encode_default(word)
        }
    }

    fn read32_default(&mut self, addr: u32) -> Result<u32, Refusal> {
        let cmd = encode_default(command_word(false, true, Func::F0, addr, 4));
        let mut rx = [0u8; 4];
        self.wire.frame(cmd, &[], &mut rx)?;
        Ok(decode_default(rx))
    }

    fn write32_default(&mut self, addr: u32, value: u32) -> Result<(), Refusal> {
        let cmd = encode_default(command_word(true, true, Func::F0, addr, 4));
        self.wire.frame(cmd, &encode_default(value), &mut [])
    }

    /// A 32-bit function-0 register read in the configured order.
    pub fn read32(&mut self, addr: u32) -> Result<u32, Refusal> {
        let cmd = self.cmd(false, Func::F0, addr, 4);
        let mut rx = [0u8; 4];
        self.wire.frame(cmd, &[], &mut rx)?;
        Ok(decode_configured(rx))
    }

    /// A 32-bit function-0 register write in the configured order.
    pub fn write32(&mut self, addr: u32, value: u32) -> Result<(), Refusal> {
        let cmd = self.cmd(true, Func::F0, addr, 4);
        self.wire.frame(cmd, &encode_configured(value), &mut [])
    }

    /// A 16-bit function-0 register read in the configured order.
    pub fn read16(&mut self, addr: u32) -> Result<u16, Refusal> {
        let cmd = self.cmd(false, Func::F0, addr, 2);
        let mut rx = [0u8; 2];
        self.wire.frame(cmd, &[], &mut rx)?;
        Ok(u16::from_le_bytes(rx))
    }

    /// A 16-bit function-0 register write in the configured order.
    pub fn write16(&mut self, addr: u32, value: u16) -> Result<(), Refusal> {
        let cmd = self.cmd(true, Func::F0, addr, 2);
        self.wire.frame(cmd, &value.to_le_bytes(), &mut [])
    }

    fn attach_step(&mut self, now: Micros) -> Result<Attach, Refusal> {
        match self.phase {
            Phase::Start => {
                self.configured = false;
                self.tuning.phase = TunePhase::Idle;
                self.wire.strap(true);
                self.wire.set_power(false);
                let until = now + POWER_OFF_HOLD_US;
                self.phase = Phase::PowerOff { until };
                Ok(Attach::Pending { until })
            }
            Phase::PowerOff { until } => {
                if now < until {
                    return Ok(Attach::Pending { until });
                }
                self.wire.set_power(true);
                let until = now + POWER_ON_SETTLE_US;
                self.phase = Phase::PowerOn { until };
                Ok(Attach::Pending { until })
            }
            Phase::PowerOn { until } => {
                if now < until {
                    return Ok(Attach::Pending { until });
                }
                self.wire.strap(false);
                self.phase = Phase::Pattern {
                    tries: 0,
                    next: now,
                };
                Ok(Attach::Pending { until: now })
            }
            Phase::Pattern { tries, next } => {
                if now < next {
                    return Ok(Attach::Pending { until: next });
                }
                let value = self.read32_default(f0::TEST_READ)?;
                if value == TEST_PATTERN {
                    self.phase = Phase::Config;
                    return Ok(Attach::Pending { until: now });
                }
                let tries = tries + 1;
                if tries >= PATTERN_TRIES {
                    return Err(Refusal::new(STAGE_TEST_PATTERN, value));
                }
                let next = now + PATTERN_RETRY_US;
                self.phase = Phase::Pattern { tries, next };
                Ok(Attach::Pending { until: next })
            }
            Phase::Config => {
                self.write32_default(f0::CONFIG, CONFIG_DWORD)?;
                self.configured = true;
                self.write_direct(Func::F0, f0::RESPONSE_DELAY_F1, RESPONSE_DELAY_F1)?;
                self.phase = Phase::Verify;
                Ok(Attach::Pending { until: now })
            }
            Phase::Verify => {
                let test = self.read32(f0::TEST_READ)?;
                if test != TEST_PATTERN {
                    return Err(Refusal::new(STAGE_BUS_VERIFICATION, test));
                }
                self.write32(f0::TEST_SCRATCH, SCRATCH_PATTERN)?;
                let scratch = self.read32(f0::TEST_SCRATCH)?;
                if scratch != SCRATCH_PATTERN {
                    return Err(Refusal::new(STAGE_BUS_VERIFICATION, scratch));
                }
                self.write32(f0::TEST_SCRATCH, 0)?;
                self.phase = Phase::Interrupts;
                Ok(Attach::Pending { until: now })
            }
            Phase::Interrupts => {
                self.write16(f0::INTERRUPT, INTERRUPT_CLEAR)?;
                self.write16(f0::INTERRUPT_ENABLE, INTERRUPT_ENABLE_F2)?;
                let enabled = self.read16(f0::INTERRUPT_ENABLE)?;
                if enabled != INTERRUPT_ENABLE_F2 {
                    return Err(Refusal::new(STAGE_INTERRUPT_ENABLE, u32::from(enabled)));
                }
                self.phase = Phase::Ready;
                Ok(Attach::Ready)
            }
            Phase::Ready => {
                self.phase = Phase::Start;
                self.attach_step(now)
            }
        }
    }
}

impl<W: GspiWire> Transport for Gspi<W> {
    const F1_CHUNK: usize = F1_MAX;

    fn chip_id(&self) -> u16 {
        self.part.chip_id()
    }

    /// The attach, one phase per call: the strap held and the module powered
    /// off, a hold, the module powered on, a settle, the strap released; the
    /// test register polled in the power-up order until it reads its
    /// pattern; the configuration dword and the response delay written; the
    /// test and scratch pair verified in the configured order; the interrupt
    /// latches cleared and the function-2 interrupt enabled. A refusal
    /// returns the phase to the start, and a call after `Ready` starts
    /// over, so either way a further attach powers the module again.
    fn attach(&mut self, now: Micros) -> Result<Attach, Refusal> {
        let step = self.attach_step(now);
        if step.is_err() {
            self.phase = Phase::Start;
            self.configured = false;
        }
        step
    }

    fn read_direct(&mut self, func: Func, addr: u32) -> Result<u8, Refusal> {
        let cmd = self.cmd(false, func, addr, 1);
        match func {
            Func::F1 => {
                let mut rx = [0u8; RESPONSE_DELAY_F1 as usize + 1];
                self.wire.frame(cmd, &[], &mut rx)?;
                Ok(rx[RESPONSE_DELAY_F1 as usize])
            }
            _ => {
                let mut rx = [0u8; 1];
                self.wire.frame(cmd, &[], &mut rx)?;
                Ok(rx[0])
            }
        }
    }

    fn write_direct(&mut self, func: Func, addr: u32, value: u8) -> Result<(), Refusal> {
        let cmd = self.cmd(true, func, addr, 1);
        self.wire.frame(cmd, &[value], &mut [])
    }

    fn read_extended(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        buf: &mut [u8],
    ) -> Result<(), Refusal> {
        let len = buf.len();
        check_len(func, len)?;
        let word = command_word(false, incr, func, addr, len);
        let cmd = if self.configured {
            encode_configured(word)
        } else {
            encode_default(word)
        };
        match func {
            Func::F1 => {
                let mut rx = [0u8; RESPONSE_DELAY_F1 as usize + F1_MAX];
                let total = RESPONSE_DELAY_F1 as usize + len;
                self.wire.frame(cmd, &[], &mut rx[..total])?;
                buf.copy_from_slice(&rx[RESPONSE_DELAY_F1 as usize..total]);
                Ok(())
            }
            _ => self.wire.frame(cmd, &[], buf),
        }
    }

    fn write_extended(
        &mut self,
        func: Func,
        addr: u32,
        incr: bool,
        data: &[u8],
    ) -> Result<(), Refusal> {
        check_len(func, data.len())?;
        let word = command_word(true, incr, func, addr, data.len());
        let cmd = if self.configured {
            encode_configured(word)
        } else {
            encode_default(word)
        };
        self.wire.frame(cmd, data, &mut [])
    }

    fn status(&mut self) -> Result<u32, Refusal> {
        self.read32(f0::STATUS)
    }

    fn take_interrupt(&mut self) -> Result<u16, Refusal> {
        let latched = self.read16(f0::INTERRUPT)?;
        if latched != 0 {
            self.write16(f0::INTERRUPT, latched)?;
        }
        Ok(latched)
    }

    /// Nothing: the bus has no abort of its own.
    fn abort_f2(&mut self) -> Result<(), Refusal> {
        Ok(())
    }

    fn f2_frame_available(&mut self) -> Result<Option<usize>, Refusal> {
        let word = self.status()?;
        if word & status::F2_PACKET_AVAILABLE == 0 {
            return Ok(None);
        }
        Ok(Some(
            ((word >> status::F2_PACKET_LEN_SHIFT) & status::F2_PACKET_LEN_MASK) as usize,
        ))
    }

    /// One function-2 read at address 0 whose command word carries the exact
    /// announced length; the wire rounds the transfer to whole words.
    fn f2_read(&mut self, len: usize, frame: &mut [u8]) -> Result<usize, Refusal> {
        if len == 0 || len > MAX_LEN || len > frame.len() {
            return Err(Refusal::new(STAGE_TRANSFER, len as u32));
        }
        let cmd = self.cmd(false, Func::F2, 0, len);
        self.wire.frame(cmd, &[], &mut frame[..len])?;
        Ok(len)
    }

    /// One function-2 write at address 0, gated on the status word's
    /// receive-ready bit.
    fn f2_write(&mut self, frame: &[u8]) -> Result<bool, Refusal> {
        if frame.is_empty() || frame.len() > MAX_LEN {
            return Err(Refusal::new(STAGE_TRANSFER, frame.len() as u32));
        }
        if self.status()? & status::F2_RX_READY == 0 {
            return Ok(false);
        }
        let cmd = self.cmd(true, Func::F2, 0, frame.len());
        self.wire.frame(cmd, frame, &mut [])?;
        Ok(true)
    }

    /// The function-2 information register's ready bit (datasheet Table 6,
    /// register 0x0E, bit 1).
    fn f2_ready(&mut self) -> Result<bool, Refusal> {
        Ok(self.read16(f0::F2_INFO)? & f2info::READY != 0)
    }

    /// The eye measured on function-1 traffic, one frame per call: for each
    /// setting the wire offers, slowest first, a 64-byte pattern is written
    /// at the scratch address and read back eight times at every sample
    /// phase; the mismatch counts are the eye, the rule of [`judge`] picks
    /// the phase and accepts or rejects the setting, the fastest accepted
    /// setting is selected (or the wire's own setting restored), and the
    /// choice is proven by a fresh pattern read back twice and the test
    /// register. A wire with no settings is done at once with nothing on
    /// the wire.
    fn tune(&mut self, f1_scratch: u32, now: Micros) -> Result<Tune, Refusal> {
        self.tune_step(f1_scratch, now)
    }
}

fn check_len(func: Func, len: usize) -> Result<(), Refusal> {
    let max = if func == Func::F1 { F1_MAX } else { MAX_LEN };
    if len == 0 || len > max {
        return Err(Refusal::new(STAGE_TRANSFER, len as u32));
    }
    Ok(())
}
