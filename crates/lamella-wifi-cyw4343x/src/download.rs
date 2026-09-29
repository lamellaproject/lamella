//! The download: the firmware image and the settings image written into
//! the chip's RAM, the firmware started, the packet channel ready and the
//! chip's sleep engine armed -- the driver's states from an attached chip
//! to a running one, one bounded step per poll.
//!
//! The chip is put into upload mode (its ARM core held in reset, its RAM
//! core reset and every bank made visible), the firmware is written to the
//! bottom of RAM in transport-sized pieces and read back, the settings
//! image is written at the top of RAM under a trailer that carries its
//! length in 32-bit words with the complement in the high half, the ARM is
//! released after the SDIO device core's interrupt status is cleared, the
//! packet channel is polled ready, and the save/restore engine's presence
//! decides whether the keep-bus-on request is made. Every millisecond
//! wait is a deadline returned to the caller.

use crate::backplane::{CHIPCOMMON, Window, clkcsr, f1, sleep, wakeup};
use crate::clock::{Clock, Micros};
use crate::cores::{
    ARM_CM3_WRAPPER, CoreReset, CoreStep, SDIOD, SOCSRAM, SOCSRAM_WRAPPER, chipcommon, running,
    sdiod, socsram,
};
use crate::driver::Wake;
use crate::error::Refusal;
use crate::transport::{Func, Transport, Tune};

/// The chip's RAM base: the firmware image is written from here.
pub const RAM_BASE: u32 = 0;
/// The chip's RAM size, 512 KB on both parts (the CYW43439 datasheet,
/// section 10.1).
pub const RAM_SIZE: u32 = 0x0008_0000;
/// The settings image's length is rounded up to a multiple of this.
pub const SETTINGS_ALIGN: usize = 64;
/// Bytes read back per poll in the firmware's verification pass: one gSPI
/// transfer, one SDIO block.
pub const READ_BACK: usize = 64;

/// The stage name of a firmware image that is empty, or does not fit
/// under the settings image.
pub const STAGE_FIRMWARE_SIZE: &str = "firmware image size";
/// The stage name of a settings image that is empty or does not fit.
pub const STAGE_SETTINGS_SIZE: &str = "settings image size";
/// The stage name of a firmware byte that read back differently.
pub const STAGE_READ_BACK: &str = "firmware read-back";
/// The stage name of the RAM core reading down before the ARM release.
pub const STAGE_SOCSRAM: &str = "SOCSRAM core";
/// The stage name of the ARM core not running after its release.
pub const STAGE_ARM_START: &str = "ARM core start";
/// The stage name of the packet channel never reporting ready.
pub const STAGE_F2_READY: &str = "function 2 ready";
/// The stage name of the keep-bus-on request never granted.
pub const STAGE_KSO: &str = "KSO enable";

/// The settle after the chip enters upload mode.
const UPLOAD_MODE_SETTLE_US: Micros = 50_000;
/// The settle before the ARM release, and the one after it.
const ARM_SETTLE_US: Micros = 10_000;
/// Packet-channel readiness polls: one hundred reads, ten milliseconds
/// apart.
const F2_TRIES: u8 = 100;
const F2_RETRY_US: Micros = 10_000;
/// The settle after the packet channel reports ready.
const F2_SETTLE_US: Micros = 100_000;
/// Keep-bus-on polls: two hundred tries, one hundred milliseconds apart.
const KSO_TRIES: u8 = 200;
const KSO_RETRY_US: Micros = 100_000;

/// The zero bytes of the settings padding.
static ZEROS: [u8; 32] = [0; 32];

/// The settings image's length rounded up to a multiple of 64.
pub const fn padded_len(len: usize) -> usize {
    (len + SETTINGS_ALIGN - 1) & !(SETTINGS_ALIGN - 1)
}

/// The address the padded settings image is written at: under the
/// trailer word at the top of RAM.
pub const fn settings_base(padded: usize) -> u32 {
    RAM_BASE + RAM_SIZE - 4 - padded as u32
}

/// The trailer word at the top of RAM: the padded length in 32-bit words
/// in the low half, its bitwise complement in the high half.
pub const fn settings_trailer(padded: usize) -> u32 {
    let token = (padded / 4) as u32;
    (!token << 16) | (token & 0xFFFF)
}

/// The next piece of the settings padding: the largest power of two not
/// exceeding `remaining`, at most 32 bytes.
pub const fn pad_piece(remaining: usize) -> usize {
    if remaining >= 32 {
        32
    } else if remaining == 0 {
        0
    } else {
        1 << (usize::BITS - 1 - remaining.leading_zeros())
    }
}

/// The address of the settings trailer word.
const TRAILER: u32 = RAM_BASE + RAM_SIZE - 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Check,
    ArmDisable(CoreReset),
    SocsramReset(CoreReset),
    RemapDisable,
    Tune {
        until: Micros,
    },
    Firmware {
        at: usize,
    },
    ReadBack {
        at: usize,
    },
    Settings {
        at: usize,
    },
    SettingsPad {
        at: usize,
    },
    Trailer,
    SocsramCheck {
        until: Micros,
    },
    InterruptClear,
    ArmReset(CoreReset),
    ArmCheck {
        until: Micros,
    },
    F2Ready {
        tries: u8,
        next: Micros,
    },
    SrProbe {
        until: Micros,
    },
    WakeupCtrl,
    KsoRequest {
        tries: u8,
    },
    /// The keep-bus-on request is granted when either the keep-bus-on bit
    /// or the device-on bit reads set; the request is written again before
    /// each poll.
    KsoPoll {
        tries: u8,
        next: Micros,
    },
}

/// What a step of the download asks of the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Progress {
    /// A wake to return.
    Wake(Wake),
    /// The download is complete.
    Done {
        /// Whether the chip's save/restore engine is present and the
        /// keep-bus-on request is on.
        save_restore: bool,
    },
}

/// The download's state, holding the two images.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Download<'b> {
    firmware: &'b [u8],
    settings: &'b [u8],
    padded: usize,
    phase: Phase,
}

impl<'b> Download<'b> {
    pub(crate) const fn new(firmware: &'b [u8], settings: &'b [u8]) -> Self {
        Download {
            firmware,
            settings,
            padded: padded_len(settings.len()),
            phase: Phase::Check,
        }
    }

    fn check(&self) -> Result<(), Refusal> {
        let firmware = self.firmware.len();
        let settings = self.settings.len();
        if settings == 0 || self.padded as u32 + 4 > RAM_SIZE {
            return Err(Refusal::new(STAGE_SETTINGS_SIZE, settings as u32));
        }
        let base = settings_base(self.padded);
        if firmware == 0 || firmware as u32 + READ_BACK as u32 > base {
            return Err(Refusal::new(STAGE_FIRMWARE_SIZE, firmware as u32));
        }
        Ok(())
    }

    /// One bounded step.
    pub(crate) fn step<T: Transport, C: Clock>(
        &mut self,
        bus: &mut T,
        clock: &mut C,
        now: Micros,
        window: &mut Window,
    ) -> Result<Progress, Refusal> {
        let wake = |w: Wake| Ok(Progress::Wake(w));
        match self.phase {
            Phase::Check => {
                self.check()?;
                self.phase = Phase::ArmDisable(CoreReset::disable(ARM_CM3_WRAPPER, 0, 0));
                self.step(bus, clock, now, window)
            }
            Phase::ArmDisable(mut core) => match core.step(window, bus, clock, now)? {
                CoreStep::Again => {
                    self.phase = Phase::ArmDisable(core);
                    wake(Wake::Again)
                }
                CoreStep::At(t) => {
                    self.phase = Phase::ArmDisable(core);
                    wake(Wake::At(t))
                }
                CoreStep::Done => {
                    self.phase = Phase::SocsramReset(CoreReset::reset(SOCSRAM_WRAPPER, 0, 0, 0));
                    wake(Wake::Again)
                }
            },
            Phase::SocsramReset(mut core) => match core.step(window, bus, clock, now)? {
                CoreStep::Again => {
                    self.phase = Phase::SocsramReset(core);
                    wake(Wake::Again)
                }
                CoreStep::At(t) => {
                    self.phase = Phase::SocsramReset(core);
                    wake(Wake::At(t))
                }
                CoreStep::Done => {
                    self.phase = Phase::RemapDisable;
                    wake(Wake::Again)
                }
            },
            Phase::RemapDisable => {
                window.write32(bus, SOCSRAM + socsram::BANK_INDEX, 3)?;
                window.write32(bus, SOCSRAM + socsram::BANK_PDA, 0)?;
                let until = now + UPLOAD_MODE_SETTLE_US;
                self.phase = Phase::Tune { until };
                wake(Wake::At(until))
            }
            Phase::Tune { until } => {
                if now < until {
                    return wake(Wake::At(until));
                }
                window.select(bus, RAM_BASE)?;
                match bus.tune(RAM_BASE & f1::OFFSET_MASK, now)? {
                    Tune::Pending { until } => {
                        self.phase = Phase::Tune { until };
                        wake(if until > now {
                            Wake::At(until)
                        } else {
                            Wake::Again
                        })
                    }
                    Tune::Done => {
                        self.phase = Phase::Firmware { at: 0 };
                        wake(Wake::Again)
                    }
                }
            }
            Phase::Firmware { at } => {
                let n = (self.firmware.len() - at).min(T::F1_CHUNK);
                let addr = RAM_BASE + at as u32;
                window.select(bus, addr)?;
                bus.write_extended(
                    Func::F1,
                    addr & f1::OFFSET_MASK,
                    true,
                    &self.firmware[at..at + n],
                )?;
                let at = at + n;
                self.phase = if at == self.firmware.len() {
                    Phase::ReadBack { at: 0 }
                } else {
                    Phase::Firmware { at }
                };
                wake(Wake::Again)
            }
            Phase::ReadBack { at } => {
                let n = (self.firmware.len() - at).min(READ_BACK);
                let addr = RAM_BASE + at as u32;
                let mut buf = [0u8; READ_BACK];
                window.select(bus, addr)?;
                bus.read_extended(Func::F1, addr & f1::OFFSET_MASK, true, &mut buf[..n])?;
                let want = &self.firmware[at..at + n];
                if let Some(i) = (0..n).find(|&i| buf[i] != want[i]) {
                    return Err(Refusal::new(STAGE_READ_BACK, addr + i as u32));
                }
                let at = at + n;
                self.phase = if at == self.firmware.len() {
                    Phase::Settings { at: 0 }
                } else {
                    Phase::ReadBack { at }
                };
                wake(Wake::Again)
            }
            Phase::Settings { at } => {
                let n = (self.settings.len() - at).min(T::F1_CHUNK);
                let addr = settings_base(self.padded) + at as u32;
                window.select(bus, addr)?;
                bus.write_extended(
                    Func::F1,
                    addr & f1::OFFSET_MASK,
                    true,
                    &self.settings[at..at + n],
                )?;
                let at = at + n;
                self.phase = if at == self.settings.len() {
                    Phase::SettingsPad { at }
                } else {
                    Phase::Settings { at }
                };
                wake(Wake::Again)
            }
            Phase::SettingsPad { at } => {
                if at == self.padded {
                    self.phase = Phase::Trailer;
                    return self.step(bus, clock, now, window);
                }
                let piece = pad_piece(self.padded - at);
                let addr = settings_base(self.padded) + at as u32;
                window.select(bus, addr)?;
                bus.write_extended(Func::F1, addr & f1::OFFSET_MASK, true, &ZEROS[..piece])?;
                self.phase = Phase::SettingsPad { at: at + piece };
                wake(Wake::Again)
            }
            Phase::Trailer => {
                window.write32(bus, TRAILER, settings_trailer(self.padded))?;
                let until = now + ARM_SETTLE_US;
                self.phase = Phase::SocsramCheck { until };
                wake(Wake::At(until))
            }
            Phase::SocsramCheck { until } => {
                if now < until {
                    return wake(Wake::At(until));
                }
                if let Err(word) = running(window, bus, SOCSRAM_WRAPPER)? {
                    return Err(Refusal::new(STAGE_SOCSRAM, word));
                }
                self.phase = Phase::InterruptClear;
                wake(Wake::Again)
            }
            Phase::InterruptClear => {
                window.write32(bus, SDIOD + sdiod::INTSTATUS, 0xFFFF_FFFF)?;
                self.phase = Phase::ArmReset(CoreReset::reset(ARM_CM3_WRAPPER, 0, 0, 0));
                wake(Wake::Again)
            }
            Phase::ArmReset(mut core) => match core.step(window, bus, clock, now)? {
                CoreStep::Again => {
                    self.phase = Phase::ArmReset(core);
                    wake(Wake::Again)
                }
                CoreStep::At(t) => {
                    self.phase = Phase::ArmReset(core);
                    wake(Wake::At(t))
                }
                CoreStep::Done => {
                    // The release ends with microsecond settles; the
                    // settle before the running check starts after them.
                    let until = clock.now_us() + ARM_SETTLE_US;
                    self.phase = Phase::ArmCheck { until };
                    wake(Wake::At(until))
                }
            },
            Phase::ArmCheck { until } => {
                if now < until {
                    return wake(Wake::At(until));
                }
                if let Err(word) = running(window, bus, ARM_CM3_WRAPPER)? {
                    return Err(Refusal::new(STAGE_ARM_START, word));
                }
                self.phase = Phase::F2Ready {
                    tries: 0,
                    next: now,
                };
                wake(Wake::Again)
            }
            Phase::F2Ready { tries, next } => {
                if now < next {
                    return wake(Wake::At(next));
                }
                if bus.f2_ready()? {
                    let until = now + F2_SETTLE_US;
                    self.phase = Phase::SrProbe { until };
                    return wake(Wake::At(until));
                }
                let tries = tries + 1;
                if tries >= F2_TRIES {
                    return Err(Refusal::new(STAGE_F2_READY, u32::from(tries)));
                }
                let next = now + F2_RETRY_US;
                self.phase = Phase::F2Ready { tries, next };
                wake(Wake::At(next))
            }
            Phase::SrProbe { until } => {
                if now < until {
                    return wake(Wake::At(until));
                }
                let word = window.read32(bus, CHIPCOMMON + chipcommon::SR_CONTROL1)?;
                if word == 0 {
                    return Ok(Progress::Done {
                        save_restore: false,
                    });
                }
                self.phase = Phase::WakeupCtrl;
                wake(Wake::Again)
            }
            Phase::WakeupCtrl => {
                let control = bus.read_direct(Func::F1, f1::WAKEUP_CTRL)?;
                bus.write_direct(Func::F1, f1::WAKEUP_CTRL, control | wakeup::HT_WAIT)?;
                bus.wake_on_command()?;
                bus.write_direct(Func::F1, f1::CHIPCLKCSR, clkcsr::FORCE_HT)?;
                self.phase = Phase::KsoRequest { tries: 0 };
                wake(Wake::Again)
            }
            Phase::KsoRequest { tries } => {
                bus.write_direct(Func::F1, f1::SLEEP_CSR, sleep::KSO | sleep::DEVON)?;
                let next = now + KSO_RETRY_US;
                self.phase = Phase::KsoPoll { tries, next };
                wake(Wake::At(next))
            }
            Phase::KsoPoll { tries, next } => {
                if now < next {
                    return wake(Wake::At(next));
                }
                let csr = bus.read_direct(Func::F1, f1::SLEEP_CSR)?;
                if csr & (sleep::KSO | sleep::DEVON) != 0 {
                    return Ok(Progress::Done { save_restore: true });
                }
                let tries = tries + 1;
                if tries >= KSO_TRIES {
                    return Err(Refusal::new(STAGE_KSO, u32::from(csr)));
                }
                self.phase = Phase::KsoRequest { tries };
                wake(Wake::Again)
            }
        }
    }
}
