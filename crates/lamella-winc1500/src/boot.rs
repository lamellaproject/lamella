//! WINC firmware boot: bring the module from its boot ROM to the running M2M Wi-Fi firmware
//! (stored in the module's own serial flash) and read the firmware's version word -- the step
//! between the raw SPI protocol and the HIF message layer every Wi-Fi operation rides.

use crate::SpiBus;
use crate::spi::{Link, SpiError};

/// Efuse-loading status; bit 31 = done.
pub const EFUSE_STATUS: u32 = 0x1014;
/// `NMI_STATE_REG`: the driver announces its version here pre-start; the firmware posts its
/// init-done marker here.
pub const NMI_STATE: u32 = 0x108c;
/// Pin-mux bank 0: bit 8 selects the interrupt function onto the IRQN pin.
pub const NMI_PIN_MUX_0: u32 = 0x1408;
/// Interrupt enable bank: bit 16 enables the host interrupt.
pub const NMI_INTR_ENABLE: u32 = 0x1a00;
/// General-purpose register 1: the chip configuration word.
pub const NMI_GP_REG_1: u32 = 0x14a0;
/// `NMI_REV_REG`: after firmware start, the packed version word -- firmware version in the low
/// half, the minimum driver version that firmware requires in the high half.
pub const NMI_REV: u32 = 0x207ac;
/// `M2M_WAIT_FOR_HOST_REG`: bit 0 set = the boot ROM skips its ready-marker handshake.
pub const WAIT_FOR_HOST: u32 = 0x207bc;
/// `BOOTROM_REG`: the boot ROM's ready marker, then the host's firmware-start command.
pub const BOOTROM: u32 = 0xc000c;

pub const FINISH_BOOT_ROM: u32 = 0x10ad_d09e;
pub const START_FIRMWARE: u32 = 0xef52_2f61;
pub const FINISH_INIT_STATE: u32 = 0x0253_2636;

const HAVE_USE_PMU_BIT: u32 = 1 << 1;
const HAVE_RESERVED1_BIT: u32 = 1 << 8;
/// Silicon revision (chip id low 12 bits) from which the PMU configuration bit applies.
const REV_3A0: u32 = 0x3a0;
/// Poll budget for the boot ROM / firmware-start waits (the reference driver's TIMEOUT).
const POLL_BUDGET: u32 = 0x2000;
/// Retry budget for the configuration-word read-back (unbounded in the reference; bounded here
/// so a dead bus cannot hang the boot).
const CONF_RETRIES: u32 = 64;

const fn make_version(major: u8, minor: u8, patch: u8) -> u16 {
    ((major as u16) << 8) | (((minor as u16) & 0xf) << 4) | ((patch as u16) & 0xf)
}

/// The version word this driver announces in [`NMI_STATE`] before starting the firmware:
/// driver release 19.6.1 (the vendor host-driver release this implementation mirrors) in the
/// low half, its oldest-supported firmware 19.3.0 in the high half.
pub const DRIVER_VERSION_INFO: u32 =
    ((make_version(19, 3, 0) as u32) << 16) | make_version(19, 6, 1) as u32;

/// The module's firmware identity, decoded from [`NMI_REV`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirmwareVersion {
    /// The running Wi-Fi firmware's release (major, minor, patch).
    pub firmware: (u8, u8, u8),
    /// The oldest host-driver release that firmware supports.
    pub min_driver: (u8, u8, u8),
}

impl FirmwareVersion {
    fn decode(word: u32) -> Self {
        let half = |v: u32| (((v >> 8) & 0xff) as u8, ((v >> 4) & 0xf) as u8, (v & 0xf) as u8);
        Self { firmware: half(word & 0xffff), min_driver: half(word >> 16) }
    }
}

/// A boot failure: a bus error, or a poll that exhausted its budget (naming the wait).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootError {
    Spi(SpiError),
    EfuseTimeout,
    BootRomTimeout { last: u32 },
    ConfigReadbackTimeout,
    FirmwareStartTimeout { last: u32 },
}

impl From<SpiError> for BootError {
    fn from(error: SpiError) -> Self {
        BootError::Spi(error)
    }
}

/// 32-bit module-register access -- what the boot sequence actually needs, so it can be driven
/// by the real [`Link`] or a scripted fake in tests.
pub trait Registers {
    fn read_reg(&mut self, addr: u32) -> Result<u32, SpiError>;
    fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), SpiError>;
}

impl<S: SpiBus> Registers for Link<S> {
    fn read_reg(&mut self, addr: u32) -> Result<u32, SpiError> {
        Link::read_reg(self, addr)
    }
    fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), SpiError> {
        Link::write_reg(self, addr, value)
    }
}

/// Boots the module's Wi-Fi firmware. `chip_rev` is the chip id's low 12 bits (from
/// [`crate::spi::NMI_CHIPID`]); `delay_ms` supplies the polling cadence. On success the
/// firmware is running, its interrupt output is enabled, and [`firmware_version`] is readable.
pub fn boot_firmware<R: Registers>(
    regs: &mut R,
    mut delay_ms: impl FnMut(u32),
    chip_rev: u32,
) -> Result<(), BootError> {
    let mut efuse_done = false;
    for _ in 0..POLL_BUDGET {
        if regs.read_reg(EFUSE_STATUS)? & 0x8000_0000 != 0 {
            efuse_done = true;
            break;
        }
        delay_ms(1);
    }
    if !efuse_done {
        return Err(BootError::EfuseTimeout);
    }

    if regs.read_reg(WAIT_FOR_HOST)? & 1 == 0 {
        let mut marker = 0;
        let mut ready = false;
        for _ in 0..POLL_BUDGET {
            marker = regs.read_reg(BOOTROM)?;
            if marker == FINISH_BOOT_ROM {
                ready = true;
                break;
            }
            delay_ms(1);
        }
        if !ready {
            return Err(BootError::BootRomTimeout { last: marker });
        }
    }

    regs.write_reg(NMI_STATE, DRIVER_VERSION_INFO)?;

    let mut conf = HAVE_RESERVED1_BIT;
    if (chip_rev & 0xfff) >= REV_3A0 {
        conf |= HAVE_USE_PMU_BIT;
    }
    let mut applied = false;
    for _ in 0..CONF_RETRIES {
        regs.write_reg(NMI_GP_REG_1, conf)?;
        if regs.read_reg(NMI_GP_REG_1)? == conf {
            applied = true;
            break;
        }
    }
    if !applied {
        return Err(BootError::ConfigReadbackTimeout);
    }

    regs.write_reg(BOOTROM, START_FIRMWARE)?;

    let mut state = 0;
    for _ in 0..POLL_BUDGET {
        delay_ms(2);
        state = regs.read_reg(NMI_STATE)?;
        if state == FINISH_INIT_STATE {
            break;
        }
    }
    if state != FINISH_INIT_STATE {
        return Err(BootError::FirmwareStartTimeout { last: state });
    }
    regs.write_reg(NMI_STATE, 0)?;

    let pin_mux = regs.read_reg(NMI_PIN_MUX_0)?;
    regs.write_reg(NMI_PIN_MUX_0, pin_mux | (1 << 8))?;
    let intr = regs.read_reg(NMI_INTR_ENABLE)?;
    regs.write_reg(NMI_INTR_ENABLE, intr | (1 << 16))?;
    Ok(())
}

/// Reads and decodes the running firmware's version word.
pub fn firmware_version<R: Registers>(regs: &mut R) -> Result<FirmwareVersion, SpiError> {
    Ok(FirmwareVersion::decode(regs.read_reg(NMI_REV)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// A scripted register file: reads pop per-address sequences (the last value repeats),
    /// writes are logged and become readable back.
    #[derive(Default)]
    struct FakeRegs {
        reads: Vec<(u32, Vec<u32>)>,
        written: Vec<(u32, u32)>,
    }

    impl FakeRegs {
        fn script(mut self, addr: u32, values: &[u32]) -> Self {
            self.reads.push((addr, values.to_vec()));
            self
        }
    }

    impl Registers for FakeRegs {
        fn read_reg(&mut self, addr: u32) -> Result<u32, SpiError> {
            if let Some((_, values)) = self.reads.iter_mut().find(|(a, _)| *a == addr) {
                let value = if values.len() > 1 { values.remove(0) } else { values[0] };
                return Ok(value);
            }
            Ok(self
                .written
                .iter()
                .rev()
                .find(|(a, _)| *a == addr)
                .map(|(_, v)| *v)
                .unwrap_or(0))
        }
        fn write_reg(&mut self, addr: u32, value: u32) -> Result<(), SpiError> {
            self.written.push((addr, value));
            Ok(())
        }
    }

    #[test]
    fn boots_in_order_and_starts_the_firmware() {
        let mut regs = FakeRegs::default()
            .script(EFUSE_STATUS, &[0, 0x8000_0000])
            .script(WAIT_FOR_HOST, &[0])
            .script(BOOTROM, &[0, FINISH_BOOT_ROM])
            .script(NMI_STATE, &[0, FINISH_INIT_STATE]);
        boot_firmware(&mut regs, |_| {}, 0x3a0).expect("boot");
        let writes: Vec<(u32, u32)> = regs.written.clone();
        assert_eq!(writes[0], (NMI_STATE, DRIVER_VERSION_INFO));
        assert_eq!(writes[1], (NMI_GP_REG_1, (1 << 8) | (1 << 1)));
        assert_eq!(writes[2], (BOOTROM, START_FIRMWARE));
        assert_eq!(writes[3], (NMI_STATE, 0));
        assert_eq!(writes[4], (NMI_PIN_MUX_0, 1 << 8));
        assert_eq!(writes[5], (NMI_INTR_ENABLE, 1 << 16));
    }

    #[test]
    fn pre_3a0_silicon_omits_the_pmu_bit() {
        let mut regs = FakeRegs::default()
            .script(EFUSE_STATUS, &[0x8000_0000])
            .script(WAIT_FOR_HOST, &[0])
            .script(BOOTROM, &[FINISH_BOOT_ROM])
            .script(NMI_STATE, &[FINISH_INIT_STATE]);
        boot_firmware(&mut regs, |_| {}, 0x2a0).expect("boot");
        assert!(regs.written.contains(&(NMI_GP_REG_1, 1 << 8)));
    }

    #[test]
    fn host_wait_marker_skips_the_bootrom_poll() {
        let mut regs = FakeRegs::default()
            .script(EFUSE_STATUS, &[0x8000_0000])
            .script(WAIT_FOR_HOST, &[1])
            .script(BOOTROM, &[0])
            .script(NMI_STATE, &[FINISH_INIT_STATE]);
        boot_firmware(&mut regs, |_| {}, 0x3a0).expect("boot skips bootrom wait");
    }

    #[test]
    fn firmware_start_timeout_reports_last_state() {
        let mut regs = FakeRegs::default()
            .script(EFUSE_STATUS, &[0x8000_0000])
            .script(WAIT_FOR_HOST, &[0])
            .script(BOOTROM, &[FINISH_BOOT_ROM])
            .script(NMI_STATE, &[0x1234]);
        assert_eq!(
            boot_firmware(&mut regs, |_| {}, 0x3a0),
            Err(BootError::FirmwareStartTimeout { last: 0x1234 })
        );
    }

    #[test]
    fn version_word_decodes_both_halves() {
        let mut regs = FakeRegs::default().script(NMI_REV, &[(0x1330 << 16) | 0x1361]);
        let version = firmware_version(&mut regs).expect("version");
        assert_eq!(version.firmware, (19, 6, 1));
        assert_eq!(version.min_driver, (19, 3, 0));
    }
}
