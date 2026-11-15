//! The chip behind a debug probe, read without disturbing it.
//!
//! A read halts nothing and resets nothing, writes nothing to target memory, and leaves the debug
//! port as it found it, but for an error flag an earlier session left there, which it clears so the
//! access port can be read. It stops at the first level that names the chip:
//!
//! - The debug port alone: `DPIDR` and `TARGETID`, with no debug power request and nothing on the
//!   system bus. This names an RP2350 and an RP2040, and it is all a chip in a low-power state sees.
//! - The system bus, when the caller asks for more and the part says its core domain is up: which
//!   package an RP2350 is, and its chip id.

use lamella_cmsis_dap::{Dap, DapError, Transport};
use lamella_cmsis_dap_rp2040 as rp2040;
use lamella_cmsis_dap_rp2350 as rp2350;
use lamella_probe_core::{DapAccess, ProbeError};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// DP register offsets (ADIv5.2, IHI 0031G, B2.2; ADIv6.0, IHI 0074E, B2.2).
const DP_ABORT: u8 = 0x0;
const DP_CTRL_STAT: u8 = 0x4;
const DP_TARGET_ID: u8 = 0x4;
const DP_SELECT: u8 = 0x8;

/// `SELECT.DPBANKSEL` for `TARGETID`.
const BANK_TARGET_ID: u32 = 0x2;

/// `CTRL/STAT.CDBGPWRUPREQ`.
const CDBGPWRUPREQ: u32 = 1 << 28;
/// `CTRL/STAT.CDBGPWRUPACK`.
const CDBGPWRUPACK: u32 = 1 << 29;
/// The bits of `CTRL/STAT` a write sets: every bit but the read-only flags and acknowledges.
const CTRL_STAT_WRITABLE: u32 = 0x57ff_ff0d;
/// The sticky error flags of `CTRL/STAT`: `STICKYORUN`, `STICKYCMP`, `STICKYERR`, `WDATAERR`.
const STICKY_FLAGS: u32 = (1 << 1) | (1 << 4) | (1 << 5) | (1 << 7);
/// `ABORT`'s clears for those flags: `STKCMPCLR`, `STKERRCLR`, `WDERRCLR`, `ORUNERRCLR`.
const CLEAR_STICKY: u32 = 0x1e;

/// The `DPIDR` an RP2040's rescue port answers.
///
/// Never a port to power up: a debug power request on it resets the chip (RP2040 datasheet 2.3.4.2).
const RP2040_RESCUE_DPIDR: u32 = 0x1021_2927;

/// `TARGETID` with its revision or instance cleared, which is what names a chip: `TPARTNO` and
/// `TDESIGNER` (IHI 0074E, B2.2.12).
const TARGET_ID_PART_MASK: u32 = 0x0fff_ffff;
/// An RP2350's `TARGETID`, by part and designer.
const RP2350_TARGET_ID: u32 = 0x0004_0927;
/// An RP2040's core 0 `TARGETID`, by part and designer.
const RP2040_TARGET_ID: u32 = rp2040::CORE0_TARGET_ID & TARGET_ID_PART_MASK;

/// The version field of a `DPIDR`.
fn dp_version(dpidr: u32) -> u32 {
    (dpidr >> 12) & 0xf
}

/// How far a read may go.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Depth {
    /// The debug port only: no power request and nothing on the system bus.
    DebugPort,
    /// Also the system bus, where the part says its core domain is up.
    SystemBus,
}

/// A step of a read, as it is reached, for a caller that traces or times one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// The wire is up and the plain connect has been tried.
    Connected,
    /// The port was asleep and has been woken, or a multidrop port addressed.
    Woken,
    /// `DPIDR`, `TARGETID` and `CTRL/STAT` have been read.
    PortRead,
    /// The part's own power state has been read.
    PowerRead,
    /// The debug power request has been acknowledged.
    BusUp,
    /// The words on the system bus have been read.
    BusRead,
    /// `CTRL/STAT` holds what it was found holding.
    Restored,
    /// A port found dormant has been put back.
    Dormant,
    /// The probe has let go of the wire.
    Released,
}

/// A chip, as far as a read named it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Chip {
    /// The family, as a board's `family` spells it: `rp2350`.
    pub family: &'static str,
    /// The part, as a board's `part` spells it, where the read went far enough to name it.
    pub part: Option<&'static str>,
    /// The chip's own id, where it has one and the read went far enough: an RP2350's, which is also
    /// the USB serial of the Lamella Link on the same board.
    pub chip_id: Option<u64>,
}

/// What a read found, in the words it read, so a log can show every one.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Reading {
    /// The chip, where the read named one.
    pub chip: Option<Chip>,
    /// `DPIDR`, where a port answered.
    pub dpidr: Option<u32>,
    /// `TARGETID`, where the port has one.
    pub target_id: Option<u32>,
    /// `CTRL/STAT` as found.
    pub ctrl_stat_found: Option<u32>,
    /// `CTRL/STAT` as left.
    pub ctrl_stat_left: Option<u32>,
    /// Whether the port was dormant when the read began.
    pub was_dormant: bool,
    /// The part's own word for its core domain's power state, where it has one.
    pub core_power: Option<u32>,
    /// `SYSINFO.CHIP_ID`, where the system bus was read.
    pub sysinfo_chip_id: Option<u32>,
    /// `SYSINFO.PACKAGE_SEL`, where the system bus was read.
    pub package_sel: Option<u32>,
    /// Why the read stopped short, where it did: the words a person is told.
    pub stopped: Option<String>,
}

/// Reads the chip behind the probe `dap` holds, then lets go of the wire.
///
/// `step` is told each step as it is reached.
pub fn read<T: Transport>(dap: &mut Dap<T>, depth: Depth, step: &mut dyn FnMut(Step)) -> Reading {
    let mut reading = Reading::default();
    let read = connect(dap, &mut reading, step).and_then(|()| read_port(dap, depth, &mut reading, step));
    if let Err(stopped) = read {
        reading.stopped = Some(stopped);
    }
    if reading.was_dormant && dap.swd_to_dormant().is_ok() {
        step(Step::Dormant);
    }
    if dap.release().is_ok() {
        step(Step::Released);
    }
    reading
}

/// Brings the wire up and gets a `DPIDR` from a port: as it stands, then from dormant, then by
/// addressing an RP2040's core 0, whose port stays quiet until it is addressed.
fn connect<T: Transport>(dap: &mut Dap<T>, reading: &mut Reading, step: &mut dyn FnMut(Step)) -> Result<(), String> {
    dap.connect_swd().map_err(|error| error.to_string())?;
    step(Step::Connected);
    if let Some(dpidr) = answer(dap.read_idcode())? {
        reading.dpidr = Some(dpidr);
        return Ok(());
    }
    dap.connect_swd_from_dormant().map_err(|error| error.to_string())?;
    let woken = match answer(dap.read_idcode())? {
        Some(dpidr) => Some(dpidr),
        None => answer(dap.select_multidrop_target(rp2040::CORE0_TARGET_ID))?,
    };
    match woken {
        Some(dpidr) => {
            reading.dpidr = Some(dpidr);
            reading.was_dormant = true;
            step(Step::Woken);
            Ok(())
        }
        None => Err("no debug port answered".to_owned()),
    }
}

/// What a read of a debug port register came to: its word, none if the target gave no good answer, and
/// an error if the probe did not carry the exchange.
fn answer(read: Result<u32, DapError>) -> Result<Option<u32>, String> {
    match read {
        Ok(word) => Ok(Some(word)),
        Err(DapError::Ack(_)) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Reads the port that answered, and the system bus if `depth` allows and the chip is one this knows.
///
/// **THE DECISION IS BY WHOLE `DPIDR`, BEFORE ANYTHING IS WRITTEN.** An RP2040's rescue port may have
/// answered, and `CTRL/STAT` is never written on any port this does not name.
fn read_port<T: Transport>(
    dap: &mut Dap<T>,
    depth: Depth,
    reading: &mut Reading,
    step: &mut dyn FnMut(Step),
) -> Result<(), String> {
    if reading.dpidr == Some(RP2040_RESCUE_DPIDR) {
        match answer(dap.select_multidrop_target(rp2040::CORE0_TARGET_ID))? {
            Some(core) => reading.dpidr = Some(core),
            None => {
                reading.chip = Some(Chip { family: "rp2040", ..Chip::default() });
                return Err("an RP2040's rescue port answered, and its core 0 did not".to_owned());
            }
        }
    }
    let dpidr = reading.dpidr.unwrap_or_default();
    reading.target_id = read_target_id(dap, dpidr)?;
    let part = reading.target_id.map(|word| word & TARGET_ID_PART_MASK);
    if part == Some(RP2040_TARGET_ID) && dpidr == rp2040::RP2040_DPIDR {
        step(Step::PortRead);
        reading.chip = Some(Chip { family: "rp2040", part: Some("rp2040"), ..Chip::default() });
        return Ok(());
    }
    if part != Some(RP2350_TARGET_ID) || dpidr != rp2350::DPIDR {
        step(Step::PortRead);
        return Err("this read does not know this debug port".to_owned());
    }
    reading.chip = Some(Chip { family: "rp2350", ..Chip::default() });
    let found = read_dp(dap, DP_CTRL_STAT)?;
    reading.ctrl_stat_found = Some(found);
    reading.ctrl_stat_left = Some(found);
    step(Step::PortRead);
    if found & STICKY_FLAGS != 0 {
        write_dp(dap, DP_ABORT, CLEAR_STICKY)?;
        reading.ctrl_stat_left = read_dp(dap, DP_CTRL_STAT).ok();
    }
    write_dp(dap, DP_SELECT, rp2350::RP_AP_SELECT)?;
    let power = DapAccess::read_ap(dap, rp2350::RP_AP_DBG_POW_STATE_SWCORE).map_err(|error| error.to_string());
    write_dp(dap, DP_SELECT, 0)?;
    let power = power?;
    reading.core_power = Some(power);
    step(Step::PowerRead);
    if depth == Depth::SystemBus {
        if power & rp2350::POW_STATE_IS_PU == 0 {
            reading.stopped = Some("the core domain is powered down, and it was not woken".to_owned());
        } else {
            read_rp2350_bus(dap, found, reading, step)?;
        }
    }
    Ok(())
}

/// `TARGETID`, where the port has one, with the bank put back.
fn read_target_id<T: Transport>(dap: &mut Dap<T>, dpidr: u32) -> Result<Option<u32>, String> {
    if dp_version(dpidr) < 2 {
        return Ok(None);
    }
    write_dp(dap, DP_SELECT, BANK_TARGET_ID)?;
    let word = DapAccess::read_dp(dap, DP_TARGET_ID);
    write_dp(dap, DP_SELECT, 0)?;
    match word {
        Ok(word) => Ok(Some(word)),
        Err(ProbeError::Ack(_)) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}

/// Reads an RP2350's `SYSINFO` and chip id over the system bus: `CDBGPWRUPREQ` alone, never
/// `CSYSPWRUPREQ`, which stops a program powering anything down for as long as it stays set (RP2350
/// datasheet 6.2.3.3). `CTRL/STAT` then gets back the word it was `found` with, whatever the reads did.
fn read_rp2350_bus<T: Transport>(
    dap: &mut Dap<T>,
    found: u32,
    reading: &mut Reading,
    step: &mut dyn FnMut(Step),
) -> Result<(), String> {
    let requested = found & CDBGPWRUPREQ != 0;
    if !requested {
        write_dp(dap, DP_CTRL_STAT, (found & CTRL_STAT_WRITABLE) | CDBGPWRUPREQ)?;
    }
    let read = read_rp2350_words(dap, reading, step);
    let selected = write_dp(dap, DP_SELECT, 0);
    let restored = if requested { Ok(()) } else { write_dp(dap, DP_CTRL_STAT, found & CTRL_STAT_WRITABLE) };
    reading.ctrl_stat_left = read_dp(dap, DP_CTRL_STAT).ok();
    step(Step::Restored);
    read.and(selected).and(restored)
}

/// Waits for the debug power request's acknowledge, then reads the words from core 0's MEM-AP.
fn read_rp2350_words<T: Transport>(
    dap: &mut Dap<T>,
    reading: &mut Reading,
    step: &mut dyn FnMut(Step),
) -> Result<(), String> {
    /// How many reads of `CTRL/STAT` the acknowledge gets, each one a round trip through the probe. The
    /// domain is already up here, so the acknowledge has nothing to wait for.
    const ACK_POLLS: usize = 64;
    let mut acknowledged = false;
    for _ in 0..ACK_POLLS {
        if read_dp(dap, DP_CTRL_STAT)? & CDBGPWRUPACK != 0 {
            acknowledged = true;
            break;
        }
    }
    if !acknowledged {
        return Err("the debug power request was not acknowledged".to_owned());
    }
    step(Step::BusUp);
    write_dp(dap, DP_SELECT, rp2350::CORE0_MEM_AP_SELECT)?;
    dap.configure_mem_ap().map_err(|error| error.to_string())?;
    let word = |dap: &mut Dap<T>, address| dap.read_word(address).map_err(|error: DapError| error.to_string());
    reading.sysinfo_chip_id = Some(word(dap, rp2350::SYSINFO_CHIP_ID)?);
    let package = word(dap, rp2350::SYSINFO_PACKAGE_SEL)?;
    reading.package_sel = Some(package);
    let low = word(dap, rp2350::OTP_CHIP_ID)?;
    let high = word(dap, rp2350::OTP_CHIP_ID + 4)?;
    step(Step::BusRead);
    if let Some(chip) = reading.chip.as_mut() {
        chip.part = Some(if package & 1 == 0 { "rp2350b" } else { "rp2350a" });
        chip.chip_id = Some((u64::from(high) << 32) | u64::from(low));
    }
    Ok(())
}

/// Whether a listing reads the chip behind a probe with these USB ids.
///
/// **A KIND OF PROBE IS READ ONLY ONCE A READ THROUGH IT IS SHOWN TO DISTURB NOTHING:** not the chip,
/// not a program running on it, and not a probe another program holds. That is shown for the Raspberry
/// Pi Debug Probe in front of an RP2350 or an RP2040, so it is the one read; every other probe is listed
/// with its chip unknown, and is not opened.
#[must_use]
pub fn listing_reads(vendor_id: u16, product_id: u16) -> bool {
    (vendor_id, product_id) == (0x2e8a, 0x000c)
}

/// How long a listing waits for the reads of the probes it lists, all of them at once: long enough for an
/// open that waits out another program's read and then reads, and short enough that a probe that does
/// not answer costs a listing a second at most.
pub const LISTING_BOUND: Duration = Duration::from_millis(1000);

/// What a listing knows of the chip behind one probe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Behind {
    /// The read named the chip.
    Chip(Chip),
    /// Another program holds the probe, so nothing was read.
    InUse,
    /// The chip is not known: the probe is not one a listing reads, it could not be held alone, or its
    /// read named no chip, or not in time. Carries why.
    Unknown(String),
}

/// Reads the chip behind each of `probes`, each on a thread of its own and all at once, at the debug port
/// alone ([`Depth::DebugPort`]), and waits at most `bound` for them: what each came to, in the order
/// given. A probe no listing reads ([`listing_reads`]) is not opened.
///
/// Every debug interface of a probe is held before anything is sent ([`lamella_probe::Listed::hold_alone`]),
/// and a probe another program holds is [`Behind::InUse`] with nothing sent to it.
#[must_use]
pub fn read_listed(probes: &[lamella_probe::Listed], bound: Duration) -> Vec<Behind> {
    let jobs: Vec<Option<Box<dyn FnOnce() -> Behind + Send>>> = probes
        .iter()
        .map(|probe| {
            listing_reads(probe.info.vendor_id, probe.info.product_id).then(|| {
                let probe = probe.clone();
                Box::new(move || read_one(&probe)) as Box<dyn FnOnce() -> Behind + Send>
            })
        })
        .collect();
    gathered(jobs, bound, || Behind::Unknown("its read did not finish in time".to_owned()))
        .into_iter()
        .map(|done| done.unwrap_or_else(|| Behind::Unknown("a listing does not read the chip behind this probe".to_owned())))
        .collect()
}

/// One probe's read, for [`read_listed`].
fn read_one(probe: &lamella_probe::Listed) -> Behind {
    let mut alone = match probe.hold_alone() {
        Ok(alone) => alone,
        Err(lamella_probe::NotAlone::InUse) => return Behind::InUse,
        Err(lamella_probe::NotAlone::Cannot(why)) => return Behind::Unknown(why),
    };
    let reading = read(alone.dap(), Depth::DebugPort, &mut |_| {});
    match reading.chip {
        Some(chip) => Behind::Chip(chip),
        None => Behind::Unknown(reading.stopped.unwrap_or_else(|| "the read named no chip".to_owned())),
    }
}

/// Runs each job on a thread of its own, all at once, and waits at most `bound` for them: what each
/// gave, in order, `late()` for one that had not finished by then, and `None` where there was no job.
///
/// A thread still running when the bound passes is left to finish on its own, and what it gives is
/// dropped.
fn gathered<T: Send + 'static>(
    jobs: Vec<Option<Box<dyn FnOnce() -> T + Send>>>,
    bound: Duration,
    late: impl Fn() -> T,
) -> Vec<Option<T>> {
    let until = Instant::now() + bound;
    let (sender, receiver) = mpsc::channel();
    let mut pending: Vec<bool> = Vec::with_capacity(jobs.len());
    for (at, job) in jobs.into_iter().enumerate() {
        pending.push(job.is_some());
        let Some(job) = job else { continue };
        let sender = sender.clone();
        std::thread::spawn(move || {
            let _ = sender.send((at, job()));
        });
    }
    drop(sender);
    let mut results: Vec<Option<T>> = pending.iter().map(|_| None).collect();
    while pending.contains(&true) {
        let Some(left) = until.checked_duration_since(Instant::now()) else { break };
        match receiver.recv_timeout(left) {
            Ok((at, done)) => {
                results[at] = Some(done);
                pending[at] = false;
            }
            Err(_) => break,
        }
    }
    for (result, still) in results.iter_mut().zip(&pending) {
        if *still {
            *result = Some(late());
        }
    }
    results
}

/// A DP register, for a port already known to answer: a refusal now is the read's failure.
fn read_dp<T: Transport>(dap: &mut Dap<T>, register: u8) -> Result<u32, String> {
    DapAccess::read_dp(dap, register).map_err(|error| error.to_string())
}

/// A DP register write.
fn write_dp<T: Transport>(dap: &mut Dap<T>, register: u8, value: u32) -> Result<(), String> {
    DapAccess::write_dp(dap, register, value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cmsis_dap::TransportError;
    use lamella_cmsis_dap::proto::{self, cmd};
    use std::collections::VecDeque;

    /// `CTRL/STAT.CSYSPWRUPREQ`, which a read never sets, and the two acknowledges.
    const CSYSPWRUPREQ: u32 = 1 << 30;
    const CSYSPWRUPACK: u32 = 1 << 31;
    /// `CTRL/STAT.STICKYERR`, which a faulted access sets.
    const STICKYERR: u32 = 1 << 5;
    /// What an RP2350's `CTRL/STAT` reads when no session has left a request or a flag set: `READOK`
    /// alone.
    const FRESH: u32 = 0x0000_0040;
    /// `DBG_POW_STATE_SWCORE` with the core domain up.
    const CORE_UP: u32 = 0x0000_095e;
    /// A chip id for the model's OTP, which holds it low word first.
    const CHIP_ID: u64 = 0x0123_4567_89ab_cdef;

    /// One debug port's identity: its `DPIDR` and its `TARGETID`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct PortId {
        dpidr: u32,
        target_id: u32,
    }

    const RP2350_PORT: PortId = PortId { dpidr: 0x4c01_3477, target_id: 0x0004_0927 };
    const RP2040_CORE0: PortId = PortId { dpidr: 0x0bc1_2477, target_id: 0x0100_2927 };
    const RP2040_CORE1: PortId = PortId { dpidr: 0x0bc1_2477, target_id: 0x1100_2927 };
    const RP2040_RESCUE: PortId = PortId { dpidr: 0x1021_2927, target_id: 0xf100_2927 };

    /// What sits behind the mock probe.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Model {
        /// An RP2350's one SW-DP.
        Rp2350,
        /// A running RP2040, whose core 0 answers a connect that addresses no port.
        Rp2040Running,
        /// An RP2040 with its cores held in reset, where only the rescue port answers.
        Rp2040HeldInReset,
        /// Some other part's debug port.
        Other(PortId),
    }

    /// A write that reached a port: a DP register and its word, or an AP register with the `SELECT` that
    /// addressed it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Write {
        Dp(u8, u32),
        Ap(u32, u8, u32),
    }

    /// A Raspberry Pi Debug Probe and the debug port behind it, as far as a read reaches. Every CMSIS-DAP
    /// command a read sends is decoded and answered from the model, and every write that reaches a port
    /// is kept, as is every `TARGETSEL`, so a test can say exactly what touched the chip.
    struct Wire {
        model: Model,
        dormant: bool,
        /// The port the last `TARGETSEL` addressed, if one has since the last line reset.
        selected: Option<u32>,
        select: u32,
        /// `CTRL/STAT`'s requests and flags; its acknowledges follow the requests when read.
        ctrl_stat: u32,
        pow_state: u32,
        tar: u32,
        fault_at: Option<u32>,
        alerted: bool,
        after_line_reset: bool,
        writes: Vec<Write>,
        targetsel: Vec<u32>,
        released: bool,
        /// Every packet the probe was sent, in order.
        sent: Vec<Vec<u8>>,
        replies: VecDeque<Vec<u8>>,
    }

    impl Wire {
        fn new(model: Model) -> Wire {
            Wire {
                model,
                dormant: false,
                selected: None,
                select: 0,
                ctrl_stat: FRESH,
                pow_state: CORE_UP,
                tar: 0,
                fault_at: None,
                alerted: false,
                after_line_reset: false,
                writes: Vec::new(),
                targetsel: Vec::new(),
                released: false,
                sent: Vec::new(),
                replies: VecDeque::new(),
            }
        }

        fn dormant(mut self) -> Wire {
            self.dormant = true;
            self
        }

        fn found(mut self, ctrl_stat: u32) -> Wire {
            self.ctrl_stat = ctrl_stat & !(CDBGPWRUPACK | CSYSPWRUPACK);
            self
        }

        fn power(mut self, pow_state: u32) -> Wire {
            self.pow_state = pow_state;
            self
        }

        fn fault_at(mut self, address: u32) -> Wire {
            self.fault_at = Some(address);
            self
        }

        /// The port driving the wire now, if any: none while dormant, and after a `TARGETSEL` only the
        /// port it named.
        fn answering(&self) -> Option<PortId> {
            if self.dormant {
                return None;
            }
            let (unaddressed, ports): (Option<PortId>, &[PortId]) = match self.model {
                Model::Rp2350 => (Some(RP2350_PORT), &[RP2350_PORT]),
                Model::Rp2040Running => (Some(RP2040_CORE0), &[RP2040_CORE0, RP2040_CORE1, RP2040_RESCUE]),
                Model::Rp2040HeldInReset => (Some(RP2040_RESCUE), &[RP2040_RESCUE]),
                Model::Other(port) => (Some(port), &[]),
            };
            match self.selected {
                None => unaddressed,
                Some(id) => ports.iter().copied().find(|port| port.target_id == id),
            }
        }

        fn ctrl_stat_read(&self) -> u32 {
            self.ctrl_stat | ((self.ctrl_stat & CDBGPWRUPREQ) << 1) | ((self.ctrl_stat & CSYSPWRUPREQ) << 1)
        }

        fn memory(&self, address: u32) -> u32 {
            match address {
                rp2350::SYSINFO_CHIP_ID => 0x2000_4927,
                rp2350::SYSINFO_PACKAGE_SEL => 1,
                rp2350::OTP_CHIP_ID => CHIP_ID as u32,
                address if address == rp2350::OTP_CHIP_ID + 4 => (CHIP_ID >> 32) as u32,
                _ => 0,
            }
        }

        fn transfer(&mut self, request: u8, value: Option<u32>) -> Vec<u8> {
            const OK: u8 = 0b001;
            const FAULT: u8 = 0b100;
            const NO_ACK: u8 = 0b111;
            let word = |word: u32| {
                let mut reply = vec![cmd::TRANSFER, 1, OK];
                reply.extend_from_slice(&word.to_le_bytes());
                reply
            };
            let Some(port) = self.answering() else {
                return vec![cmd::TRANSFER, 0, NO_ACK];
            };
            let access_port = request & 1 != 0;
            let register = request & 0x0c;
            let ap = self.select & !0xf;
            match (access_port, value) {
                (false, None) => word(match register {
                    0x0 => port.dpidr,
                    0x4 if self.select & 0xf == BANK_TARGET_ID => port.target_id,
                    0x4 => self.ctrl_stat_read(),
                    _ => 0,
                }),
                (false, Some(value)) => {
                    self.writes.push(Write::Dp(register, value));
                    match register {
                        0x0 if value & CLEAR_STICKY == CLEAR_STICKY => self.ctrl_stat &= !STICKY_FLAGS,
                        0x4 => self.ctrl_stat = (self.ctrl_stat & !CTRL_STAT_WRITABLE) | (value & CTRL_STAT_WRITABLE),
                        0x8 => self.select = value,
                        _ => {}
                    }
                    vec![cmd::TRANSFER, 1, OK]
                }
                (true, _) if self.ctrl_stat & STICKY_FLAGS != 0 => vec![cmd::TRANSFER, 0, FAULT],
                (true, Some(value)) => {
                    self.writes.push(Write::Ap(ap, register, value));
                    if ap == rp2350::CORE0_MEM_AP_SELECT && register == 0x4 {
                        self.tar = value;
                    }
                    vec![cmd::TRANSFER, 1, OK]
                }
                (true, None) if ap == rp2350::RP_AP_SELECT && register == rp2350::RP_AP_DBG_POW_STATE_SWCORE => {
                    word(self.pow_state)
                }
                (true, None) if ap == rp2350::CORE0_MEM_AP_SELECT && register == 0xc => {
                    if self.ctrl_stat & CDBGPWRUPREQ == 0 || self.fault_at == Some(self.tar) {
                        self.ctrl_stat |= STICKYERR;
                        return vec![cmd::TRANSFER, 0, FAULT];
                    }
                    word(self.memory(self.tar))
                }
                (true, None) => word(0),
            }
        }

        /// A `DAP_SWJ_Sequence`: a line reset forgets the last `TARGETSEL`; a dormant port wakes only to
        /// the selection alert followed by the activation code, and a line reset followed by the
        /// SWD-to-dormant select puts it back to sleep.
        fn sequence(&mut self, bits: u8, data: &[u8]) {
            let line_reset = bits >= 50 && data.iter().all(|&byte| byte == 0xff);
            let activation = bits == 12 && data == [0xa0, 0x01];
            let to_dormant = bits == 16 && data == [0xbc, 0xe3];
            if self.dormant {
                if activation && self.alerted {
                    self.dormant = false;
                    self.selected = None;
                }
            } else if line_reset {
                self.selected = None;
            } else if to_dormant && self.after_line_reset {
                self.dormant = true;
            }
            self.alerted = bits == 128;
            self.after_line_reset = line_reset;
        }

        /// A `DAP_SWD_Sequence`, which carries a `TARGETSEL` write: an 8-bit request, five released
        /// cycles, then the 32-bit id and its parity.
        fn swd_sequence(&mut self, packet: &[u8]) -> Vec<u8> {
            let mut at = 2;
            let mut driven: Vec<&[u8]> = Vec::new();
            let mut reply = vec![cmd::SWD_SEQUENCE, proto::DAP_OK];
            for _ in 0..packet[1] {
                let info = packet[at];
                at += 1;
                let cycles = match info & 0x3f {
                    0 => 64,
                    cycles => usize::from(cycles),
                };
                let bytes = cycles.div_ceil(8);
                if info & 0x80 != 0 {
                    reply.extend(std::iter::repeat_n(0xff, bytes));
                } else {
                    driven.push(&packet[at..at + bytes]);
                    at += bytes;
                }
            }
            if let [[0x99], [a, b, c, d, _parity]] = driven.as_slice()
                && !self.dormant
            {
                let id = u32::from_le_bytes([*a, *b, *c, *d]);
                self.targetsel.push(id);
                self.selected = Some(id);
            }
            reply
        }

        fn ctrl_stat_writes(&self) -> Vec<u32> {
            self.writes
                .iter()
                .filter_map(|write| match write {
                    Write::Dp(0x4, value) => Some(*value),
                    _ => None,
                })
                .collect()
        }
    }

    impl Transport for Wire {
        fn write_packet(&mut self, data: &[u8]) -> Result<(), TransportError> {
            self.sent.push(data.to_vec());
            let reply = match data[0] {
                cmd::INFO if data[1] == 0xff => vec![cmd::INFO, 2, 0x40, 0x00],
                cmd::INFO => vec![cmd::INFO, 0],
                cmd::CONNECT => vec![cmd::CONNECT, data[1]],
                cmd::DISCONNECT => {
                    self.released = true;
                    vec![cmd::DISCONNECT, proto::DAP_OK]
                }
                id @ (cmd::SWJ_CLOCK | cmd::SWD_CONFIGURE | cmd::TRANSFER_CONFIGURE) => vec![id, proto::DAP_OK],
                cmd::SWJ_SEQUENCE => {
                    self.sequence(data[1], &data[2..]);
                    vec![cmd::SWJ_SEQUENCE, proto::DAP_OK]
                }
                cmd::SWD_SEQUENCE => self.swd_sequence(data),
                cmd::TRANSFER => {
                    assert_eq!(data[2], 1, "a read sends one transfer at a time");
                    let request = data[3];
                    let value = (request & 0b10 == 0).then(|| u32::from_le_bytes([data[4], data[5], data[6], data[7]]));
                    self.transfer(request, value)
                }
                _ => vec![proto::INVALID_COMMAND],
            };
            self.replies.push_back(reply);
            Ok(())
        }

        fn read_packet(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
            let reply = self.replies.pop_front().ok_or_else(|| TransportError("nothing was asked".into()))?;
            buf[..reply.len()].copy_from_slice(&reply);
            Ok(reply.len())
        }
    }

    /// A read of `wire` at `depth`: what it found, the steps it took, and the probe, to look at after.
    fn run(wire: Wire, depth: Depth) -> (Reading, Vec<Step>, Dap<Wire>) {
        let mut dap = Dap::new(wire);
        let mut steps = Vec::new();
        let reading = read(&mut dap, depth, &mut |step| steps.push(step));
        (reading, steps, dap)
    }

    /// **AN RP2350 IS NAMED AT THE DEBUG PORT WITH NOTHING WRITTEN BUT `SELECT`:** no power request and
    /// nothing on the system bus, and `CTRL/STAT` read, never written.
    #[test]
    fn an_rp2350_is_named_at_the_debug_port_with_nothing_written_but_select() {
        let (reading, steps, dap) = run(Wire::new(Model::Rp2350), Depth::DebugPort);
        assert_eq!(reading.chip, Some(Chip { family: "rp2350", part: None, chip_id: None }));
        assert_eq!((reading.dpidr, reading.target_id), (Some(RP2350_PORT.dpidr), Some(RP2350_PORT.target_id)));
        assert_eq!((reading.ctrl_stat_found, reading.ctrl_stat_left), (Some(FRESH), Some(FRESH)));
        assert_eq!(reading.core_power, Some(CORE_UP));
        assert_eq!(reading.stopped, None);
        assert_eq!(steps, [Step::Connected, Step::PortRead, Step::PowerRead, Step::Released]);
        let wire = dap.transport();
        assert_eq!(
            wire.writes,
            [Write::Dp(0x8, BANK_TARGET_ID), Write::Dp(0x8, 0), Write::Dp(0x8, rp2350::RP_AP_SELECT), Write::Dp(0x8, 0)]
        );
        assert_eq!((wire.select, wire.ctrl_stat_read(), wire.released), (0, FRESH, true));
    }

    /// **A RUNNING RP2040 IS NAMED BY ITS CORE 0, WITH NOTHING WRITTEN BUT `SELECT`:** a connect that
    /// addresses no port reaches core 0, and its `TARGETID` names the part. Nothing else is read.
    #[test]
    fn a_running_rp2040_is_named_by_its_core_0_with_nothing_written_but_select() {
        let (reading, steps, dap) = run(Wire::new(Model::Rp2040Running), Depth::SystemBus);
        assert_eq!(reading.chip, Some(Chip { family: "rp2040", part: Some("rp2040"), chip_id: None }));
        assert_eq!((reading.dpidr, reading.target_id), (Some(RP2040_CORE0.dpidr), Some(RP2040_CORE0.target_id)));
        assert_eq!((reading.ctrl_stat_found, reading.stopped), (None, None));
        assert_eq!(steps, [Step::Connected, Step::PortRead, Step::Released]);
        let wire = dap.transport();
        assert_eq!(wire.writes, [Write::Dp(0x8, BANK_TARGET_ID), Write::Dp(0x8, 0)]);
        assert!(wire.targetsel.is_empty(), "{:x?}", wire.targetsel);
    }

    /// **AN RP2040 HELD IN RESET IS NAMED FROM ITS RESCUE PORT, AND NOTHING AT ALL IS WRITTEN TO IT.** A
    /// debug power request on the rescue port resets the chip, so the read addresses core 0 instead.
    /// When core 0 does not answer, the family is named and the read stops there.
    #[test]
    fn an_rp2040_held_in_reset_is_named_from_its_rescue_port_and_nothing_is_written() {
        let (reading, steps, dap) = run(Wire::new(Model::Rp2040HeldInReset), Depth::SystemBus);
        assert_eq!(reading.chip, Some(Chip { family: "rp2040", part: None, chip_id: None }));
        assert_eq!(reading.dpidr, Some(RP2040_RESCUE.dpidr));
        assert_eq!(reading.stopped.as_deref(), Some("an RP2040's rescue port answered, and its core 0 did not"));
        assert_eq!(steps, [Step::Connected, Step::Released]);
        let wire = dap.transport();
        assert!(wire.writes.is_empty(), "no write of any register reaches a port: {:x?}", wire.writes);
        assert_eq!(wire.targetsel, [rp2040::CORE0_TARGET_ID]);
        assert!(wire.released);
    }

    /// **A DEBUG PORT THIS READ DOES NOT KNOW IS NAMED NOTHING, AND ITS `CTRL/STAT` IS NEVER TOUCHED.** A
    /// version 1 port has no `TARGETID`, so nothing is written to it at all. A later one has its bank put
    /// back. The RP2350's `DPIDR` is Arm's, so another part's `TARGETID` behind it is not an RP2350.
    #[test]
    fn a_debug_port_this_read_does_not_know_is_named_nothing() {
        let version_1 = PortId { dpidr: 0x2ba0_1477, target_id: 0 };
        let other_part = PortId { dpidr: 0x6ba0_2477, target_id: 0x1234_5477 };
        let arm_port_other_part = PortId { dpidr: RP2350_PORT.dpidr, target_id: 0x1234_5477 };
        for (port, writes) in [
            (version_1, vec![]),
            (other_part, vec![Write::Dp(0x8, BANK_TARGET_ID), Write::Dp(0x8, 0)]),
            (arm_port_other_part, vec![Write::Dp(0x8, BANK_TARGET_ID), Write::Dp(0x8, 0)]),
        ] {
            let (reading, steps, dap) = run(Wire::new(Model::Other(port)), Depth::SystemBus);
            assert_eq!(reading.chip, None, "{port:x?}");
            assert_eq!(reading.stopped.as_deref(), Some("this read does not know this debug port"), "{port:x?}");
            assert_eq!(reading.ctrl_stat_found, None, "{port:x?}");
            assert_eq!(steps, [Step::Connected, Step::PortRead, Step::Released], "{port:x?}");
            assert_eq!(dap.transport().writes, writes, "{port:x?}");
            assert!(dap.transport().released, "{port:x?}");
        }
    }

    /// **A PORT FOUND DORMANT IS WOKEN, READ, AND PUT BACK TO SLEEP.** It ignores the plain connect, so
    /// it answers only after the selection alert, and the read ends by putting it back.
    #[test]
    fn a_port_found_dormant_is_woken_read_and_put_back_to_sleep() {
        let (reading, steps, dap) = run(Wire::new(Model::Rp2350).dormant(), Depth::DebugPort);
        assert!(reading.was_dormant);
        assert_eq!(reading.chip, Some(Chip { family: "rp2350", part: None, chip_id: None }));
        assert_eq!(
            steps,
            [Step::Connected, Step::Woken, Step::PortRead, Step::PowerRead, Step::Dormant, Step::Released]
        );
        assert!(dap.transport().dormant, "the port is left as it was found: dormant");

        let (reading, steps, dap) = run(Wire::new(Model::Rp2350), Depth::DebugPort);
        assert!(!reading.was_dormant);
        assert!(!steps.contains(&Step::Dormant), "{steps:?}");
        assert!(!dap.transport().dormant, "a port found awake is not put to sleep");
    }

    /// **THE BUS IS READ WITH THE DEBUG POWER REQUEST ALONE, AND `CTRL/STAT` GETS BACK THE WORD IT WAS
    /// FOUND WITH.** The system power request is never made, and a request another session left set is
    /// left set rather than written.
    #[test]
    fn the_bus_is_read_with_the_debug_power_request_alone_and_ctrl_stat_is_put_back() {
        let (reading, steps, dap) = run(Wire::new(Model::Rp2350), Depth::SystemBus);
        assert_eq!(reading.chip, Some(Chip { family: "rp2350", part: Some("rp2350a"), chip_id: Some(CHIP_ID) }));
        assert_eq!((reading.sysinfo_chip_id, reading.package_sel), (Some(0x2000_4927), Some(1)));
        assert_eq!((reading.ctrl_stat_found, reading.ctrl_stat_left), (Some(FRESH), Some(FRESH)));
        assert_eq!(reading.stopped, None);
        assert_eq!(
            steps,
            [
                Step::Connected,
                Step::PortRead,
                Step::PowerRead,
                Step::BusUp,
                Step::BusRead,
                Step::Restored,
                Step::Released
            ]
        );
        let wire = dap.transport();
        assert_eq!(wire.ctrl_stat_writes(), [CDBGPWRUPREQ, 0]);
        assert!(wire.writes.iter().all(|write| !matches!(write, Write::Dp(0x4, value) if value & CSYSPWRUPREQ != 0)));
        assert_eq!((wire.select, wire.ctrl_stat_read()), (0, FRESH));

        let left_set = 0xf000_0040;
        let (reading, _, dap) = run(Wire::new(Model::Rp2350).found(left_set), Depth::SystemBus);
        assert_eq!(reading.chip.and_then(|chip| chip.chip_id), Some(CHIP_ID));
        assert_eq!((reading.ctrl_stat_found, reading.ctrl_stat_left), (Some(left_set), Some(left_set)));
        assert!(dap.transport().ctrl_stat_writes().is_empty(), "a request already made is not written again");
    }

    /// **A BUS READ THAT FAILS STILL PUTS THE POWER REQUEST BACK,** and says what it left.
    #[test]
    fn a_bus_read_that_fails_still_puts_the_power_request_back() {
        let wire = Wire::new(Model::Rp2350).fault_at(rp2350::OTP_CHIP_ID);
        let (reading, steps, dap) = run(wire, Depth::SystemBus);
        assert!(reading.stopped.is_some(), "{reading:?}");
        assert_eq!(reading.chip.as_ref().and_then(|chip| chip.part), None);
        assert!(steps.ends_with(&[Step::Restored, Step::Released]), "{steps:?}");
        let wire = dap.transport();
        assert_eq!(wire.ctrl_stat_writes(), [CDBGPWRUPREQ, 0]);
        assert_eq!(wire.ctrl_stat & (CDBGPWRUPREQ | CSYSPWRUPREQ), 0, "no power request is left set");
        assert_eq!(reading.ctrl_stat_left, Some(wire.ctrl_stat_read()), "the word left is the word read back");
        assert_eq!(wire.select, 0);
    }

    /// **A CORE DOMAIN THAT IS POWERED DOWN IS NOT WOKEN FOR THE BUS:** the chip is named from its port
    /// alone, and no power request is made.
    #[test]
    fn a_core_domain_that_is_powered_down_is_not_woken_for_the_bus() {
        let (reading, _, dap) = run(Wire::new(Model::Rp2350).power(0), Depth::SystemBus);
        assert_eq!(reading.chip, Some(Chip { family: "rp2350", part: None, chip_id: None }));
        assert_eq!(reading.stopped.as_deref(), Some("the core domain is powered down, and it was not woken"));
        assert!(dap.transport().ctrl_stat_writes().is_empty(), "{:x?}", dap.transport().writes);
    }

    /// **A STICKY FLAG AN EARLIER SESSION LEFT IS CLEARED BEFORE THE ACCESS PORT IS READ, AND THE WORD
    /// LEFT IS THE WORD THERE NOW,** not the one found.
    #[test]
    fn a_sticky_flag_left_by_an_earlier_session_is_cleared_and_the_word_left_is_said() {
        let found = FRESH | STICKYERR;
        let (reading, _, dap) = run(Wire::new(Model::Rp2350).found(found), Depth::DebugPort);
        assert_eq!(reading.core_power, Some(CORE_UP), "{reading:?}");
        let wire = dap.transport();
        assert_eq!(wire.writes.first(), Some(&Write::Dp(0x8, BANK_TARGET_ID)));
        assert!(wire.writes.contains(&Write::Dp(0x0, CLEAR_STICKY)), "{:x?}", wire.writes);
        assert_eq!(reading.ctrl_stat_found, Some(found));
        assert_eq!(reading.ctrl_stat_left, Some(FRESH));
        assert_eq!(wire.ctrl_stat_read(), FRESH);
    }

    /// **A READ SENDS THESE PACKETS AND NO OTHERS, AND ITS LAST IS THE DISCONNECT:** at the debug port of
    /// each family, and on the system bus, where the read puts `CTRL/STAT` back itself. So a release that
    /// gives back the power requests a session raised finds nothing to give back after any of them.
    #[test]
    fn a_read_sends_these_packets_and_no_others() {
        const DPIDR: u8 = 0x0;
        let dp_read = |register| proto::transfer_one(proto::dp_read(register), None);
        let dp_write = |register, value| proto::transfer_one(proto::dp_write(register), Some(value));
        let ap_read = |register| proto::transfer_one(proto::ap_read(register), None);
        let ap_write = |register, value| proto::transfer_one(proto::ap_write(register), Some(value));
        let connect = [
            proto::connect(proto::Port::Swd).to_vec(),
            proto::swj_clock(lamella_cmsis_dap::CONNECT_CLOCK_HZ).to_vec(),
            proto::swd_configure(0, false).to_vec(),
            proto::transfer_configure(0, 250, 100).to_vec(),
            proto::info(0xff).to_vec(),
            proto::swj_sequence(51, &[0xff; 7]),
            proto::swj_sequence(16, &[0x9e, 0xe7]),
            proto::swj_sequence(51, &[0xff; 7]),
            proto::swj_sequence(8, &[0x00]),
            dp_read(DPIDR),
        ];
        let rp2350_port = [
            dp_write(DP_SELECT, BANK_TARGET_ID),
            dp_read(DP_TARGET_ID),
            dp_write(DP_SELECT, 0),
            dp_read(DP_CTRL_STAT),
            dp_write(DP_SELECT, rp2350::RP_AP_SELECT),
            ap_read(rp2350::RP_AP_DBG_POW_STATE_SWCORE),
            dp_write(DP_SELECT, 0),
        ];
        let rp2040_port = [dp_write(DP_SELECT, BANK_TARGET_ID), dp_read(DP_TARGET_ID), dp_write(DP_SELECT, 0)];
        let mut bus = vec![
            dp_write(DP_CTRL_STAT, CDBGPWRUPREQ),
            dp_read(DP_CTRL_STAT),
            dp_write(DP_SELECT, rp2350::CORE0_MEM_AP_SELECT),
            ap_write(0x0, 0x2300_0052),
        ];
        for address in [rp2350::SYSINFO_CHIP_ID, rp2350::SYSINFO_PACKAGE_SEL, rp2350::OTP_CHIP_ID, rp2350::OTP_CHIP_ID + 4] {
            bus.extend([ap_write(0x4, address), ap_read(0xc)]);
        }
        bus.extend([dp_write(DP_SELECT, 0), dp_write(DP_CTRL_STAT, 0), dp_read(DP_CTRL_STAT)]);
        let disconnect = proto::disconnect().to_vec();

        for (model, depth, port, bus) in [
            (Model::Rp2350, Depth::DebugPort, &rp2350_port[..], &[][..]),
            (Model::Rp2040Running, Depth::DebugPort, &rp2040_port[..], &[][..]),
            (Model::Rp2350, Depth::SystemBus, &rp2350_port[..], &bus[..]),
        ] {
            let expected: Vec<Vec<u8>> =
                connect.iter().chain(port).chain(bus).chain([&disconnect]).cloned().collect();
            let (_, _, dap) = run(Wire::new(model), depth);
            assert_eq!(dap.transport().sent, expected, "{model:?} at {depth:?}");
        }
    }

    /// **ONLY THE PROBE A READ IS SHOWN SAFE THROUGH IS READ.** The Raspberry Pi Debug Probe is; a
    /// micro:bit's interface chip, an EDBG and a Pico in its bootloader are not.
    #[test]
    fn only_the_probe_a_read_is_shown_safe_through_is_read() {
        assert!(listing_reads(0x2e8a, 0x000c));
        for (vendor_id, product_id) in [(0x0d28, 0x0204), (0x03eb, 0x2111), (0x2e8a, 0x0003), (0x1fc9, 0x0143)] {
            assert!(!listing_reads(vendor_id, product_id), "{vendor_id:04x}:{product_id:04x}");
        }
    }

    /// **THE READS RUN AT ONCE, AND THE LISTING WAITS NO LONGER THAN ITS BOUND.** Each answer keeps its
    /// place whichever finishes first; one still running at the bound is late, and a probe with no read
    /// has no answer.
    #[test]
    fn the_reads_run_at_once_and_the_listing_waits_no_longer_than_its_bound() {
        type Job = Option<Box<dyn FnOnce() -> &'static str + Send>>;
        let after = |wait: u64, said: &'static str| -> Job {
            Some(Box::new(move || {
                std::thread::sleep(Duration::from_millis(wait));
                said
            }))
        };
        let started = Instant::now();
        let done = gathered(vec![after(200, "slow"), None, after(0, "fast"), after(200, "also slow")], Duration::from_secs(5), || "late");
        assert_eq!(done, [Some("slow"), None, Some("fast"), Some("also slow")]);
        assert!(started.elapsed() < Duration::from_millis(390), "at once, not one after another: {:?}", started.elapsed());

        let started = Instant::now();
        let done = gathered(vec![after(0, "fast"), after(2_000, "stuck")], Duration::from_millis(100), || "late");
        assert_eq!(done, [Some("fast"), Some("late")]);
        let waited = started.elapsed();
        assert!(waited >= Duration::from_millis(100) && waited < Duration::from_millis(1_000), "{waited:?}");
    }
}
