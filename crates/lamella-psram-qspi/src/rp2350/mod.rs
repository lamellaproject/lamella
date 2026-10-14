//! The RP2350's QSPI memory interface (the QMI) as the host: the second chip
//! select's pad and function, the direct-mode sequence that wakes the part,
//! memory window 1's registers, and the cache maintenance the proof needs.
//! Register access lives in this module alone.
//!
//! Register facts per the RP2350 datasheet: section 2.2 (the address map),
//! section 4.4 (the XIP subsystem: the two 16 MB windows, one per chip
//! select, at 0x10000000 and 0x11000000; their uncached mirrors at
//! 0x14000000 and 0x15000000; the cache-maintenance mirror at 0x18000000
//! whose three low address bits name the operation and whose bit 13 and
//! bits 12:3 name the way and the line; the XIP CTRL register's WRITABLE_M1
//! bit, which must be set for a RAM part on chip select 1), section 7.5
//! (the subsystem resets), section 9.4 (the function select: function 9 is
//! the QMI's second chip select on GPIO 0, 8, 19 and 47), section 9.11 (the
//! pads: the isolation latch, the output disable, the pulls) and section
//! 12.14 (the QMI: direct mode and its registers, the window registers).
//!
//! Direct mode disconnects the memory-mapped flash window from the bus for
//! as long as it is on, so the code that runs it must not be fetched from
//! that window: the sequence lives in one function the crate places in a
//! `.data` section, which a flash-resident image's map copies to RAM at
//! boot, and that function calls nothing, reads no constant from flash and
//! bounds every poll.

use crate::part::{Grade, Part};
use crate::proof::Memory;
use crate::timing::{direct_clkdiv, read_command, read_format, window_timing, write_command, write_format};
use crate::{Error, Id, Step, Window};

/// The QMI's register block.
pub const QMI_BASE: usize = 0x400d_0000;
const DIRECT_CSR: usize = QMI_BASE;
const DIRECT_TX: usize = QMI_BASE + 0x04;
const DIRECT_RX: usize = QMI_BASE + 0x08;
const M1_TIMING: usize = QMI_BASE + 0x20;
const M1_RFMT: usize = QMI_BASE + 0x24;
const M1_RCMD: usize = QMI_BASE + 0x28;
const M1_WFMT: usize = QMI_BASE + 0x2c;
const M1_WCMD: usize = QMI_BASE + 0x30;

const CSR_EN: u32 = 1 << 0;
const CSR_BUSY: u32 = 1 << 1;
const CSR_ASSERT_CS1N: u32 = 1 << 3;
const CSR_CLKDIV_SHIFT: u32 = 22;

const TX_NOPUSH: u32 = 1 << 20;
const TX_OE: u32 = 1 << 19;
const TX_IWIDTH_QUAD: u32 = 2 << 16;

/// The XIP control block.
pub const XIP_CTRL: usize = 0x400c_8000;
const XIP_CTRL_WRITABLE_M1: u32 = 1 << 11;

/// Memory window 1: the chip select 1 window, cached.
pub const WINDOW1_BASE: usize = 0x1100_0000;
/// The same window, uncached.
pub const WINDOW1_UNCACHED_BASE: usize = 0x1500_0000;
/// The most a chip select window serves.
pub const WINDOW1_BYTES_MAX: usize = 16 * 1024 * 1024;

const CACHE_MAINTENANCE_BASE: usize = 0x1800_0000;
const CACHE_WAYS: usize = 2;
const CACHE_LINES_PER_WAY: usize = 1024;
const MAINT_INVALIDATE_SET_WAY: usize = 0;
const MAINT_CLEAN_SET_WAY: usize = 1;

const RESETS_RESET: usize = 0x4002_0000;
const RESETS_RESET_DONE: usize = 0x4002_0008;
const RESET_IO_BANK0: u32 = 1 << 6;
const RESET_PADS_BANK0: u32 = 1 << 9;
const IO_BANK0: usize = 0x4002_8000;
const PADS_BANK0: usize = 0x4003_8000;
const FUNCSEL_QMI_CS1N: u32 = 9;
const PAD_ISO: u32 = 1 << 8;
const PAD_OD: u32 = 1 << 7;
const PAD_PUE: u32 = 1 << 3;
const PAD_PDE: u32 = 1 << 2;

/// The lines the QMI can route its second chip select to.
pub const SELECT_LINES: [u8; 4] = [0, 8, 19, 47];

/// Reads of the busy flag before a poll gives up: the QMI clocks its frames
/// whether or not a part answers, so the flag clears within a few serial
/// cycles; the bound is a belt against a host that is not what it says.
const POLL_BOUND: u32 = 1_000_000;

/// The host's registers, so the sequence runs against a fake in a test.
pub trait Regs {
    /// # Safety
    /// The address is one of this module's registers.
    unsafe fn write(&mut self, addr: usize, v: u32);
    /// # Safety
    /// The address is one of this module's registers.
    unsafe fn read(&mut self, addr: usize) -> u32;
}

/// The hardware: volatile accesses at the chip's addresses.
pub struct Mmio;

impl Regs for Mmio {
    #[inline(always)]
    unsafe fn write(&mut self, addr: usize, v: u32) {
        unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
    }
    #[inline(always)]
    unsafe fn read(&mut self, addr: usize) -> u32 {
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }
}

impl Memory for Mmio {
    #[inline(always)]
    unsafe fn write(&mut self, addr: usize, v: u32) {
        unsafe { core::ptr::write_volatile(addr as *mut u32, v) }
    }
    #[inline(always)]
    unsafe fn read(&mut self, addr: usize) -> u32 {
        unsafe { core::ptr::read_volatile(addr as *const u32) }
    }
    /// Every line of both ways cleaned, then invalidated, through the
    /// maintenance mirror; 4096 writes.
    unsafe fn flush(&mut self) {
        unsafe {
            maintain_all(MAINT_CLEAN_SET_WAY);
            maintain_all(MAINT_INVALIDATE_SET_WAY);
        }
    }
}

unsafe fn maintain_all(op: usize) {
    for way in 0..CACHE_WAYS {
        for line in 0..CACHE_LINES_PER_WAY {
            let addr = CACHE_MAINTENANCE_BASE | (way << 13) | (line << 3) | op;
            unsafe { core::ptr::write_volatile(addr as *mut u32, 0) }
        }
    }
}

/// What a bring-up needs to know.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// The GPIO line the board wires the part's chip select to: 0, 8, 19 or
    /// 47.
    pub select_line: u8,
    /// The part.
    pub part: &'static Part,
    /// The system clock the QMI runs from, final for this boot.
    pub sys_hz: u32,
    /// The part's temperature grade.
    pub grade: Grade,
    /// The window's size as the board's facts state it (at most 16 MB; the
    /// proof, not the part's identifier, is what proves it).
    pub bytes: usize,
}

/// Bring the part up on the hardware and program window 1.
///
/// # Safety
/// A single core; no interrupt source enabled yet, or every one masked by
/// the caller's own mechanism; the system clock final; window 1 unused; the
/// section `.data.lamella_psram_qspi_nucleus` placed in RAM by the caller's
/// map. The crate adds no interrupt masking of its own.
pub unsafe fn bring_up(cfg: &Config) -> Result<Window, Error> {
    unsafe { bring_up_with(&mut Mmio, cfg) }
}

/// The bring-up against any register implementation.
///
/// # Safety
/// As [`bring_up`] when the implementation is the hardware.
pub unsafe fn bring_up_with<R: Regs>(r: &mut R, cfg: &Config) -> Result<Window, Error> {
    if !SELECT_LINES.contains(&cfg.select_line) {
        return Err(Error::SelectLine(cfg.select_line));
    }
    if cfg.bytes == 0 || cfg.bytes > WINDOW1_BYTES_MAX || cfg.bytes % 4 != 0 {
        return Err(Error::WindowSize(cfg.bytes));
    }
    // Refused before any register is touched.
    let timing = window_timing(cfg.part, cfg.sys_hz, cfg.grade).map_err(Error::Timing)?;
    let dclk = direct_clkdiv(cfg.sys_hz);
    let line = cfg.select_line as usize;

    unsafe {
        // The two blocks out of reset (a flash-boot image has them out
        // already; the step is idempotent so the caller's order is free).
        let mask = RESET_IO_BANK0 | RESET_PADS_BANK0;
        let held = r.read(RESETS_RESET);
        r.write(RESETS_RESET, held & !mask);
        let mut n = 0u32;
        while r.read(RESETS_RESET_DONE) & mask != mask {
            n += 1;
            if n == POLL_BOUND {
                return Err(Error::Stuck(Step::Resets));
            }
        }

        // The pad, configured while isolated, then released: no output
        // disable, the reset pull-down replaced by a pull-up so the chip
        // select idles high whenever the pad is not driven (the part wants
        // its select high before any operation), the function the QMI's
        // second chip select.
        let pad = PADS_BANK0 + 4 + 4 * line;
        let bits = (r.read(pad) & !(PAD_OD | PAD_PDE)) | PAD_PUE;
        r.write(pad, bits | PAD_ISO);
        r.write(IO_BANK0 + 8 * line + 4, FUNCSEL_QMI_CS1N);
        r.write(pad, bits & !PAD_ISO);
    }

    // The command bytes copied out of the part's table here, in flash, so
    // the sequence in RAM reads nothing from flash.
    let cmds = Commands {
        exit_quad: cfg.part.cmd_exit_quad,
        reset_enable: cfg.part.cmd_reset_enable,
        reset: cfg.part.cmd_reset,
        read_id: cfg.part.cmd_read_id,
        enter_quad: cfg.part.cmd_enter_quad,
    };
    let (code, id) = direct_sequence(r, dclk, cmds);
    match code {
        0 => {}
        1 => return Err(Error::Stuck(Step::Enter)),
        2 => return Err(Error::Stuck(Step::Nudge)),
        3 => return Err(Error::Stuck(Step::ResetEnable)),
        4 => return Err(Error::Stuck(Step::Reset)),
        5 => return Err(Error::Stuck(Step::ReadId)),
        _ => return Err(Error::Stuck(Step::EnterQuad)),
    }
    if id.kgd != cfg.part.kgd_pass {
        return Err(Error::NotPresent { vendor: id.vendor, kgd: id.kgd });
    }

    unsafe {
        r.write(M1_TIMING, timing.encode());
        r.write(M1_RFMT, read_format(cfg.part));
        r.write(M1_RCMD, read_command(cfg.part));
        r.write(M1_WFMT, write_format(cfg.part));
        r.write(M1_WCMD, write_command(cfg.part));
        let ctrl = r.read(XIP_CTRL);
        r.write(XIP_CTRL, ctrl | XIP_CTRL_WRITABLE_M1);
    }

    Ok(Window { base: WINDOW1_BASE, bytes: cfg.bytes, uncached_base: WINDOW1_UNCACHED_BASE, id, timing })
}

/// The command bytes the sequence sends, passed by value.
#[derive(Clone, Copy)]
struct Commands {
    exit_quad: u8,
    reset_enable: u8,
    reset: u8,
    read_id: u8,
    enter_quad: u8,
}

/// Wait for the QMI's busy flag to clear; false when the bound is reached.
#[inline(always)]
unsafe fn settle<R: Regs>(r: &mut R) -> bool {
    let mut n = 0u32;
    loop {
        if unsafe { r.read(DIRECT_CSR) } & CSR_BUSY == 0 {
            return true;
        }
        n += 1;
        if n == POLL_BOUND {
            return false;
        }
    }
}

#[inline(always)]
unsafe fn select<R: Regs>(r: &mut R) {
    unsafe {
        let v = r.read(DIRECT_CSR);
        r.write(DIRECT_CSR, v | CSR_ASSERT_CS1N)
    }
}

#[inline(always)]
unsafe fn deselect<R: Regs>(r: &mut R) {
    unsafe {
        let v = r.read(DIRECT_CSR);
        r.write(DIRECT_CSR, v & !CSR_ASSERT_CS1N)
    }
}

/// Push one record and wait for it to be clocked out.
#[inline(always)]
unsafe fn push<R: Regs>(r: &mut R, record: u32) -> bool {
    unsafe {
        r.write(DIRECT_TX, record);
        settle(r)
    }
}

/// One command in its own chip-select frame.
#[inline(always)]
unsafe fn frame<R: Regs>(r: &mut R, record: u32) -> bool {
    unsafe {
        select(r);
        let ok = push(r, record);
        deselect(r);
        ok
    }
}

/// The direct-mode sequence, in RAM, calling nothing: direct mode on at
/// the slow divisor; the exit-quad nudge at four lines (a part left in its
/// four-line mode by a warm reset returns to serial mode; a part in serial
/// mode ignores the two-clock fragment); reset enable and reset; the Read
/// ID frame (the command, three address bytes, three answered bytes); the
/// enter-quad command; direct mode off. Returns the step that stuck, or 0,
/// with the identification bytes.
#[cfg_attr(target_os = "none", unsafe(link_section = ".data.lamella_psram_qspi_nucleus"))]
#[inline(never)]
fn direct_sequence<R: Regs>(r: &mut R, dclk: u32, c: Commands) -> (u8, Id) {
    unsafe {
        r.write(DIRECT_CSR, (dclk << CSR_CLKDIV_SHIFT) | CSR_EN);
        if !settle(r) {
            r.write(DIRECT_CSR, 0);
            return (1, Id::NONE);
        }
        if !frame(r, TX_NOPUSH | TX_OE | TX_IWIDTH_QUAD | c.exit_quad as u32) {
            r.write(DIRECT_CSR, 0);
            return (2, Id::NONE);
        }
        if !frame(r, TX_NOPUSH | c.reset_enable as u32) {
            r.write(DIRECT_CSR, 0);
            return (3, Id::NONE);
        }
        if !frame(r, TX_NOPUSH | c.reset as u32) {
            r.write(DIRECT_CSR, 0);
            return (4, Id::NONE);
        }

        select(r);
        let mut ok = push(r, TX_NOPUSH | c.read_id as u32);
        ok = ok && push(r, TX_NOPUSH);
        ok = ok && push(r, TX_NOPUSH);
        ok = ok && push(r, TX_NOPUSH);
        let mut vendor = 0u32;
        let mut kgd = 0u32;
        let mut eid = 0u32;
        if ok {
            ok = push(r, 0xFF);
            vendor = r.read(DIRECT_RX) & 0xFF;
        }
        if ok {
            ok = push(r, 0xFF);
            kgd = r.read(DIRECT_RX) & 0xFF;
        }
        if ok {
            ok = push(r, 0xFF);
            eid = r.read(DIRECT_RX) & 0xFF;
        }
        deselect(r);
        if !ok {
            r.write(DIRECT_CSR, 0);
            return (5, Id::NONE);
        }
        let id = Id { vendor: vendor as u8, kgd: kgd as u8, eid: eid as u8 };

        if !frame(r, TX_NOPUSH | c.enter_quad as u32) {
            r.write(DIRECT_CSR, 0);
            return (6, id);
        }

        r.write(DIRECT_CSR, 0);
        (0, id)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::timing::TimingError;
    use std::collections::VecDeque;
    use std::vec;
    use std::vec::Vec;

    const PAD47: usize = PADS_BANK0 + 4 + 4 * 47;
    const CTRL47: usize = IO_BANK0 + 8 * 47 + 4;

    /// A register file that answers like the chip and records every write.
    struct Fake {
        writes: Vec<(usize, u32)>,
        csr: u32,
        rx: VecDeque<u32>,
        answers: VecDeque<u32>,
        pushes: usize,
        busy_from_push: Option<usize>,
        pad: u32,
        xip_ctrl: u32,
        resets: u32,
    }

    impl Fake {
        fn new(answers: &[u32]) -> Fake {
            Fake {
                writes: Vec::new(),
                csr: 0,
                rx: VecDeque::new(),
                answers: answers.iter().copied().collect(),
                pushes: 0,
                busy_from_push: None,
                pad: 0x116,
                xip_ctrl: 0x3,
                resets: 0xffff_ffff,
            }
        }
        fn tx_pushes(&self) -> Vec<u32> {
            self.writes.iter().filter(|(a, _)| *a == DIRECT_TX).map(|(_, v)| *v).collect()
        }
        fn window_writes(&self) -> Vec<(usize, u32)> {
            self.writes
                .iter()
                .filter(|(a, _)| matches!(*a, M1_TIMING | M1_RFMT | M1_RCMD | M1_WFMT | M1_WCMD))
                .copied()
                .collect()
        }
    }

    impl Regs for Fake {
        unsafe fn write(&mut self, addr: usize, v: u32) {
            self.writes.push((addr, v));
            match addr {
                DIRECT_CSR => self.csr = v & !CSR_BUSY,
                DIRECT_TX => {
                    self.pushes += 1;
                    if v & TX_NOPUSH == 0 {
                        let a = self.answers.pop_front().unwrap_or(0xFF);
                        self.rx.push_back(a);
                    }
                }
                RESETS_RESET => self.resets = v,
                PAD47 => self.pad = v,
                XIP_CTRL => self.xip_ctrl = v,
                _ => {}
            }
        }
        unsafe fn read(&mut self, addr: usize) -> u32 {
            match addr {
                DIRECT_CSR => match self.busy_from_push {
                    Some(n) if self.pushes >= n => self.csr | CSR_BUSY,
                    _ => self.csr,
                },
                DIRECT_RX => self.rx.pop_front().unwrap_or(0),
                RESETS_RESET => self.resets,
                RESETS_RESET_DONE => !self.resets,
                PAD47 => self.pad,
                XIP_CTRL => self.xip_ctrl,
                _ => 0,
            }
        }
    }

    fn cfg() -> Config {
        Config {
            select_line: 47,
            part: &Part::APS6404L_3SQR,
            sys_hz: 150_000_000,
            grade: Grade::Standard,
            bytes: 8 * 1024 * 1024,
        }
    }

    #[test]
    fn the_sequence_on_a_good_part_programs_the_window() {
        let mut f = Fake::new(&[0x0D, 0x5D, 0x26]);
        let w = unsafe { bring_up_with(&mut f, &cfg()) }.unwrap();
        assert_eq!(w.id, Id { vendor: 0x0D, kgd: 0x5D, eid: 0x26 });
        assert_eq!(w.base, 0x1100_0000);
        assert_eq!(w.uncached_base, 0x1500_0000);
        assert_eq!(w.bytes, 8 * 1024 * 1024);
        assert_eq!(w.timing.encode(), 0x61A2_3202, "the 150 MHz word with the half-cycle of margin");

        // The frames, in order: the nudge at four lines, reset enable,
        // reset, the ID frame's seven records, enter quad.
        assert_eq!(
            f.tx_pushes(),
            vec![
                TX_NOPUSH | TX_OE | TX_IWIDTH_QUAD | 0xF5,
                TX_NOPUSH | 0x66,
                TX_NOPUSH | 0x99,
                TX_NOPUSH | 0x9F,
                TX_NOPUSH,
                TX_NOPUSH,
                TX_NOPUSH,
                0xFF,
                0xFF,
                0xFF,
                TX_NOPUSH | 0x35,
            ]
        );
        // The window's registers after the sequence, in order.
        assert_eq!(
            f.window_writes(),
            vec![(M1_TIMING, 0x61A2_3202), (M1_RFMT, 0x0006_12AA), (M1_RCMD, 0xEB), (M1_WFMT, 0x0000_12AA), (M1_WCMD, 0x38)]
        );
        assert_eq!(f.xip_ctrl, 0x3 | XIP_CTRL_WRITABLE_M1);
        // The pad: released from isolation, no output disable, pull-up not
        // pull-down; the function the second chip select.
        assert_eq!(f.pad & (PAD_ISO | PAD_OD | PAD_PDE | PAD_PUE), PAD_PUE);
        assert!(f.writes.contains(&(CTRL47, FUNCSEL_QMI_CS1N)));
        // Direct mode entered at 10 MHz and left at the end.
        assert!(f.writes.contains(&(DIRECT_CSR, (15 << CSR_CLKDIV_SHIFT) | CSR_EN)));
        let last_csr = f.writes.iter().filter(|(a, _)| *a == DIRECT_CSR).last().unwrap().1;
        assert_eq!(last_csr, 0);
        // The chip select never asserted with direct mode off.
        for (a, v) in &f.writes {
            if *a == DIRECT_CSR && v & CSR_ASSERT_CS1N != 0 {
                assert!(v & CSR_EN != 0, "chip select asserted with direct mode off");
            }
        }
        // The resets released before the pad was written.
        let reset_at = f.writes.iter().position(|(a, _)| *a == RESETS_RESET).unwrap();
        let pad_at = f.writes.iter().position(|(a, _)| *a == PAD47).unwrap();
        assert!(reset_at < pad_at);
    }

    #[test]
    fn a_failing_die_is_refused_and_the_window_stays_unprogrammed() {
        let mut f = Fake::new(&[0x0D, 0x55, 0x26]);
        assert_eq!(unsafe { bring_up_with(&mut f, &cfg()) }, Err(Error::NotPresent { vendor: 0x0D, kgd: 0x55 }));
        assert!(f.window_writes().is_empty());
        assert_eq!(f.xip_ctrl, 0x3);
        let last_csr = f.writes.iter().filter(|(a, _)| *a == DIRECT_CSR).last().unwrap().1;
        assert_eq!(last_csr, 0, "direct mode left");
    }

    #[test]
    fn no_part_answering_reads_as_not_present() {
        let mut f = Fake::new(&[0xFF, 0xFF, 0xFF]);
        assert_eq!(unsafe { bring_up_with(&mut f, &cfg()) }, Err(Error::NotPresent { vendor: 0xFF, kgd: 0xFF }));
    }

    #[test]
    fn a_posture_the_part_cannot_run_at_touches_no_register() {
        let mut f = Fake::new(&[0x0D, 0x5D, 0x26]);
        let mut c = cfg();
        c.sys_hz = 12_000_000;
        assert_eq!(
            unsafe { bring_up_with(&mut f, &c) },
            Err(Error::Timing(TimingError::SelectBound { tcem_cycles: 96, tail_cycles: 38 }))
        );
        assert!(f.writes.is_empty());
    }

    #[test]
    fn a_line_the_second_select_cannot_reach_is_refused() {
        let mut f = Fake::new(&[]);
        let mut c = cfg();
        c.select_line = 5;
        assert_eq!(unsafe { bring_up_with(&mut f, &c) }, Err(Error::SelectLine(5)));
        assert!(f.writes.is_empty());
    }

    #[test]
    fn a_window_past_the_select_or_unaligned_is_refused() {
        let mut f = Fake::new(&[]);
        let mut c = cfg();
        c.bytes = 32 * 1024 * 1024;
        assert_eq!(unsafe { bring_up_with(&mut f, &c) }, Err(Error::WindowSize(32 * 1024 * 1024)));
        c.bytes = 6;
        assert_eq!(unsafe { bring_up_with(&mut f, &c) }, Err(Error::WindowSize(6)));
        assert!(f.writes.is_empty());
    }

    #[test]
    fn a_busy_flag_that_never_clears_names_its_step_and_leaves_direct_mode() {
        let mut f = Fake::new(&[0x0D, 0x5D, 0x26]);
        f.busy_from_push = Some(2);
        assert_eq!(unsafe { bring_up_with(&mut f, &cfg()) }, Err(Error::Stuck(Step::ResetEnable)));
        let last_csr = f.writes.iter().filter(|(a, _)| *a == DIRECT_CSR).last().unwrap().1;
        assert_eq!(last_csr, 0);
        assert!(f.window_writes().is_empty());
    }
}
