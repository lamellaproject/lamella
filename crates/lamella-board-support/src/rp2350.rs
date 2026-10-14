//! The RP2350 family: the clock plan its boards state, UART0 as the console, TIMER0 as the
//! millisecond clock, and the QSPI PSRAM a board may fit.
//!
//! Every RP2350 board states the same kind of plan: a crystal that feeds clk_ref and clk_peri
//! directly, PLL_SYS for clk_sys, and PLL_USB for clk_usb and clk_adc. The startup applies it from
//! any state the part was left in, so it parks clk_sys on the crystal before it resets the PLL
//! that clk_sys may be running on.
//!
//! A board that fits a QSPI PSRAM on the QSPI memory interface's second chip select states it, and
//! [`psram_bring_up`] brings the part up, proves the window by reading back what it wrote, and
//! wipes it, for an image's startup to call once clk_sys is final. The image's heap then takes the
//! window it returns. [`attach_psram`] is that call as a startup makes it after [`init`], keeping
//! what became of the part in a [`PsramRecord`].

use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use lamella_psram_qspi::proof::{self, Memory, Mismatch};
use lamella_psram_qspi::rp2350::{self as qmi, Regs};
use lamella_psram_qspi::{Grade, Part, Window};

use crate::registers::Registers;

#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_instances.rs"]
mod instances;
#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_clocks_layout.rs"]
mod clocks;
#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_pll_layout.rs"]
mod pll;
#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_xosc_layout.rs"]
mod xosc;
#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_resets_layout.rs"]
mod resets;
#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_uart_layout.rs"]
mod uart;
#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_pads_bank0_layout.rs"]
mod pads;
#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_ticks_layout.rs"]
mod ticks;
#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_timer_layout.rs"]
mod timer;

/// How many times the startup polls for the crystal before it leaves every clock as it found them.
///
/// The crystal is given 196 x 256 of its own cycles to settle, about 4.2 ms at 12 MHz, and each poll
/// takes at least four cycles of clk_sys. So this waits at least 53 ms at 150 MHz, the family's
/// rated clk_sys.
pub const CRYSTAL_POLLS: u32 = 2_000_000;

/// How many times the startup polls a PLL for lock before it leaves that PLL unused.
///
/// At least four cycles of clk_sys per poll: at least 26 ms at 150 MHz.
pub const PLL_LOCK_POLLS: u32 = 1_000_000;

/// How many times a clock switch, or a block entering or leaving reset, is polled before the code
/// moves on.
///
/// Each completes within a few cycles of the slower clock involved. At least four cycles of clk_sys
/// per poll: at least 2.6 ms at 150 MHz.
pub const SWITCH_POLLS: u32 = 100_000;

/// How many times a console write polls for room in the transmit FIFO before it drops the byte.
///
/// One byte takes about 87 microseconds at 115200 baud. At least four cycles of clk_sys per poll:
/// at least 2.6 ms at 150 MHz, and at least 33 ms at the crystal's 12 MHz.
pub const TX_POLLS: u32 = 100_000;

/// The ring oscillator's RANDOMBIT register, whose bit 0 is the oscillator's output: the ROSC block
/// at 0x400e8000 and the register at its offset 0x20 (RP2350 datasheet, section 2.2 and table 614).
pub const ROSC_RANDOMBIT: u32 = 0x400e_8020;

/// A board's console: UART0 and the pin its transmit line is routed to, with the divisor for the
/// console's baud under the board's plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Console {
    /// The UART instance's base address.
    pub uart_base: u32,
    /// The RESETS bits the console needs out of reset: its UART, and the IO and pad banks its pins
    /// sit in.
    pub release_mask: u32,
    /// The address of the transmit pin's IO_BANK0 CTRL register.
    pub io_tx_ctrl: u32,
    /// The address of the transmit pin's PADS_BANK0 register.
    pub pads_tx: u32,
    /// The function select that routes the transmit pin to the UART.
    pub funcsel: u32,
    /// The integer part of the baud divisor.
    pub ibrd: u32,
    /// The fractional part of the baud divisor, in 64ths.
    pub fbrd: u32,
    /// The rate of clk_peri the divisor was computed for.
    pub clk_peri_hz: u32,
}

/// A board's default clock plan: the crystal's rate, and the two PLLs' settings, each stated and
/// verified by the generator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Plan {
    /// The crystal's rate, which clk_ref and clk_peri run at.
    pub xosc_hz: u32,
    /// PLL_SYS's feedback divisor.
    pub pll_sys_fbdiv: u32,
    /// PLL_SYS's PRIM word: its two post dividers.
    pub pll_sys_prim: u32,
    /// PLL_USB's feedback divisor.
    pub pll_usb_fbdiv: u32,
    /// PLL_USB's PRIM word: its two post dividers.
    pub pll_usb_prim: u32,
}

impl Plan {
    /// clk_sys under this plan once PLL_SYS has locked: the crystal's rate times the feedback
    /// divisor over the two post dividers. The startup sets the reference divider to 1.
    #[must_use]
    pub const fn pll_sys_hz(&self) -> u32 {
        let postdiv1 = (self.pll_sys_prim & pll::PRIM_POSTDIV1) >> pll::PRIM_POSTDIV1_LSB;
        let postdiv2 = (self.pll_sys_prim & pll::PRIM_POSTDIV2) >> pll::PRIM_POSTDIV2_LSB;
        let vco_hz = self.xosc_hz as u64 * self.pll_sys_fbdiv as u64;
        (vco_hz / (postdiv1 as u64 * postdiv2 as u64)) as u32
    }
}

/// A QSPI PSRAM a board fits on the QSPI memory interface's second chip select, which answers as
/// RAM in the interface's second window once [`psram_bring_up`] has run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Psram {
    /// The GPIO line the board wires the part's chip select to.
    pub select_line: u8,
    /// The part's datasheet facts.
    pub part: &'static Part,
    /// The temperature grade the part is ordered in.
    pub grade: Grade,
    /// The window's base in the cached view.
    pub base: usize,
    /// The window's size in bytes.
    pub bytes: usize,
}

/// The PSRAM parts this driver knows, by the ordering code a board's facts name: each code with
/// the part's datasheet facts and the temperature grade the code orders. The APS6404L-3SQR
/// datasheet's ordering table lists APS6404L-3SQR-ZR as the part for -40 C to +85 C.
const PSRAM_PARTS: [(&str, &Part, Grade); 1] =
    [("APS6404L-3SQR-ZR", &Part::APS6404L_3SQR, Grade::Standard)];

impl Psram {
    /// The PSRAM a board's facts state: the line its chip select is wired to, the part's ordering
    /// code, and the window's base and size.
    ///
    /// # Panics
    ///
    /// For an ordering code this driver does not know, or a line no GPIO has, at compile time
    /// when evaluated in a constant.
    #[must_use]
    pub const fn from_facts(select_line: u32, ordering_code: &str, base: u32, bytes: u32) -> Self {
        assert!(select_line <= u8::MAX as u32, "no GPIO line has that number");
        let mut index = 0;
        while index < PSRAM_PARTS.len() {
            let (code, part, grade) = PSRAM_PARTS[index];
            if same_bytes(code.as_bytes(), ordering_code.as_bytes()) {
                return Psram {
                    select_line: select_line as u8,
                    part,
                    grade,
                    base: base as usize,
                    bytes: bytes as usize,
                };
            }
            index += 1;
        }
        panic!("the board's PSRAM part is not one this driver knows")
    }
}

/// Whether two byte strings are equal, in a constant.
const fn same_bytes(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut index = 0;
    while index < a.len() {
        if a[index] != b[index] {
            return false;
        }
        index += 1;
    }
    true
}

/// Whether the QSPI memory interface can route its second chip select to `line`.
const fn routable(line: u8) -> bool {
    let mut index = 0;
    while index < qmi::SELECT_LINES.len() {
        if qmi::SELECT_LINES[index] == line {
            return true;
        }
        index += 1;
    }
    false
}

/// A board on this family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Board {
    /// The board's console.
    pub console: Console,
    /// The board's default clock plan.
    pub plan: Plan,
    /// The QSPI PSRAM the board fits, if it fits one.
    pub psram: Option<Psram>,
}

impl Board {
    /// Refuses, at compile time when evaluated in a constant, a board whose facts this driver
    /// cannot apply as stated.
    ///
    /// The console's divisor must have been computed for clk_peri on the crystal, which is where
    /// the startup puts it. The crystal must divide into whole microseconds within the tick
    /// generator's 9-bit count. And a PSRAM must answer in the QSPI memory interface's second
    /// window, on a line the interface can route its second chip select to, with a window of whole
    /// words no larger than the part or the interface's window.
    ///
    /// # Panics
    ///
    /// When any of these does not hold.
    #[must_use]
    pub const fn checked(self) -> Self {
        assert!(
            self.console.clk_peri_hz == self.plan.xosc_hz,
            "the console's divisor must be computed for clk_peri on the crystal"
        );
        assert!(
            self.plan.xosc_hz.is_multiple_of(1_000_000),
            "the crystal must divide into whole microseconds"
        );
        let cycles = self.plan.xosc_hz / 1_000_000;
        assert!(
            cycles != 0 && cycles <= ticks::TIMER0_CYCLES_CYCLES,
            "a microsecond must fit the tick generator's count"
        );
        if let Some(psram) = self.psram {
            assert!(
                psram.base == qmi::WINDOW1_BASE,
                "the PSRAM must answer in the QSPI memory interface's second window"
            );
            assert!(
                routable(psram.select_line),
                "the QSPI memory interface cannot route its second chip select to the PSRAM's line"
            );
            assert!(
                psram.bytes != 0
                    && psram.bytes.is_multiple_of(4)
                    && psram.bytes <= psram.part.bytes
                    && psram.bytes <= qmi::WINDOW1_BYTES_MAX,
                "the PSRAM's window must be whole words, no larger than the part or the interface's window"
            );
        }
        self
    }

    /// clk_ref cycles per microsecond tick: the crystal's rate in MHz.
    #[must_use]
    pub const fn tick_cycles(&self) -> u32 {
        self.plan.xosc_hz / 1_000_000
    }
}

/// What the startup achieved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// The crystal started. When it did not, every clock is as the startup found it.
    pub crystal: bool,
    /// PLL_SYS locked and clk_sys runs from it. When it did not, clk_sys runs from the crystal.
    pub pll_sys: bool,
    /// PLL_USB locked, and clk_usb and clk_adc run from it. When it did not, both are stopped.
    pub pll_usb: bool,
}

/// What the board remembers between calls. All zero means "console down, clock not started",
/// which is what the RAM a startup clears holds.
#[derive(Debug)]
pub struct State {
    console_up: AtomicBool,
    clock_started: AtomicBool,
}

impl State {
    /// The state before the startup has run.
    #[must_use]
    pub const fn new() -> Self {
        State { console_up: AtomicBool::new(false), clock_started: AtomicBool::new(false) }
    }
}

impl Default for State {
    fn default() -> Self {
        Self::new()
    }
}

/// Applies the board's default plan, starts TIMER0's tick and restarts TIMER0 from zero, once at
/// reset, before any program code runs. It touches no console register.
pub fn init<R: Registers>(board: &Board, regs: &mut R, state: &State) -> Applied {
    let crystal = start_crystal(regs);
    let (pll_sys, pll_usb) = if crystal {
        let parked = park_on_crystal(regs);
        clk_peri_on_crystal(regs);
        if parked { (clk_sys_on_pll(board, regs), clk_usb_and_adc_on_pll(board, regs)) } else { (false, false) }
    } else {
        (false, false)
    };
    start_timer(board, regs);
    state.clock_started.store(true, Ordering::Relaxed);
    Applied { crystal, pll_sys, pll_usb }
}

/// Writes one console byte, bringing the console up first if this is the first byte. Answers
/// whether the byte was sent; one the UART does not take in time is dropped.
pub fn console_putc<R: Registers>(board: &Board, regs: &mut R, state: &State, byte: u8) -> bool {
    let console = &board.console;
    if !state.console_up.load(Ordering::Relaxed) {
        bring_up_console(console, regs);
        state.console_up.store(true, Ordering::Relaxed);
    }
    let flags = console.uart_base + uart::UARTFR_OFF;
    if !poll(regs, flags, uart::UARTFR_TXFF, 0, TX_POLLS) {
        return false;
    }
    regs.write(console.uart_base + uart::UARTDR_OFF, u32::from(byte));
    true
}

/// The monotonic clock in milliseconds, counted from 1 at the startup, or 0 before [`init`] has run.
///
/// A reading in the first millisecond is 1 rather than 0, because the runtime reads 0 as "no clock".
pub fn now_ms<R: Registers>(regs: &mut R, state: &State) -> u64 {
    if !state.clock_started.load(Ordering::Relaxed) {
        return 0;
    }
    (read_microseconds(regs) / 1000).wrapping_add(1)
}

/// Why a board's PSRAM is not in use.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PsramFault {
    /// The bring-up refused, and programmed nothing of the window: the part's limits cannot be met
    /// at this clk_sys, the interface does not serve the line or the size, a step never finished,
    /// or no good die answered.
    BringUp(lamella_psram_qspi::Error),
    /// A word the stride proof wrote did not read back: the window is not RAM as the board states.
    Proof(Mismatch),
}

/// Brings up the board's PSRAM at clk_sys `sys_hz`, proves the window with the stride walk, and
/// wipes it, so every word reads zero through either view and nothing of it is cached. Answers the
/// window, whose cached view is the program's RAM, or why there is none.
///
/// The proof writes its pattern salted with `salt`, so a pattern left by an earlier boot cannot
/// pass for this one; any value that differs between boots serves.
///
/// # Safety
///
/// One core runs; every interrupt is masked; clk_sys is final at `sys_hz`; nothing uses the
/// interface's second window; and the image's map places the section
/// `.data.lamella_psram_qspi_nucleus` in RAM, because the direct-mode sequence that wakes the part
/// takes the flash window off the bus while it runs.
#[allow(unsafe_code)]
pub unsafe fn psram_bring_up(psram: &Psram, sys_hz: u32, salt: u32) -> Result<Window, PsramFault> {
    // SAFETY: the caller guarantees this function's contract, and `Mmio` is the hardware it names.
    unsafe { psram_bring_up_with(&mut qmi::Mmio, &mut qmi::Mmio, psram, sys_hz, salt) }
}

/// [`psram_bring_up`] against any registers and memory, so a test runs it against models of both.
///
/// # Safety
///
/// As [`psram_bring_up`] when either is the hardware.
#[allow(unsafe_code)]
pub unsafe fn psram_bring_up_with<R: Regs, M: Memory>(
    regs: &mut R,
    memory: &mut M,
    psram: &Psram,
    sys_hz: u32,
    salt: u32,
) -> Result<Window, PsramFault> {
    let config = qmi::Config {
        select_line: psram.select_line,
        part: psram.part,
        sys_hz,
        grade: psram.grade,
        bytes: psram.bytes,
    };
    // SAFETY: the caller guarantees this function's contract, which is the bring-up's.
    let window = unsafe { qmi::bring_up_with(regs, &config) }.map_err(PsramFault::BringUp)?;
    // SAFETY: the bring-up just programmed the window, and nothing else uses it.
    unsafe { proof::stride_walk(memory, &window, salt) }.map_err(PsramFault::Proof)?;
    // SAFETY: the same window, still used by nothing else.
    unsafe { wipe(memory, &window) };
    Ok(window)
}

/// Writes zero to every word of the window through its uncached view, then cleans and drops every
/// cache line, so the window reads zero through either view.
///
/// # Safety
///
/// The window is the caller's to write, and nothing else uses it or the cache meanwhile.
#[allow(unsafe_code)]
pub unsafe fn wipe<M: Memory>(memory: &mut M, window: &Window) {
    let mut offset = 0;
    while offset < window.bytes {
        // SAFETY: the offset lies inside the window, which the caller lets this write.
        unsafe { memory.write(window.uncached_base + offset, 0) };
        offset += 4;
    }
    // SAFETY: nothing else uses the cache, by the caller's word.
    unsafe { memory.flush() };
}

/// What became of a board's PSRAM at the startup: nothing yet, the window it serves, or why it
/// serves none. An outcome is recorded once. All zero means nothing is recorded, which is what the
/// RAM a startup clears holds.
pub struct PsramRecord {
    phase: AtomicU8,
    outcome: UnsafeCell<MaybeUninit<Result<Window, PsramFault>>>,
}

/// [`PsramRecord`]'s phases: nothing recorded, an outcome being made, and an outcome recorded.
const EMPTY: u8 = 0;
const MAKING: u8 = 1;
const RECORDED: u8 = 2;

// SAFETY: the outcome is written only by the one call that moved the phase from EMPTY to MAKING,
// and read only once the phase is RECORDED, which that call stores after its write, with Release
// against the readers' Acquire. Nothing writes it again.
#[allow(unsafe_code)]
unsafe impl Sync for PsramRecord {}

impl PsramRecord {
    /// Nothing recorded.
    #[must_use]
    pub const fn new() -> Self {
        PsramRecord {
            phase: AtomicU8::new(EMPTY),
            outcome: UnsafeCell::new(MaybeUninit::uninit()),
        }
    }

    /// Runs `bring_up` and records what it answers, unless an outcome is already recorded or being
    /// made, in which case `bring_up` does not run. Answers whether it ran.
    #[allow(unsafe_code)]
    pub fn record_with(&self, bring_up: impl FnOnce() -> Result<Window, PsramFault>) -> bool {
        if self
            .phase
            .compare_exchange(EMPTY, MAKING, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        let outcome = bring_up();
        // SAFETY: moving the phase from EMPTY made this call the outcome's one writer, and no
        // reader reads it before the phase is RECORDED.
        unsafe { (*self.outcome.get()).write(outcome) };
        self.phase.store(RECORDED, Ordering::Release);
        true
    }

    /// The recorded outcome, or `None` while nothing is recorded.
    #[allow(unsafe_code)]
    #[must_use]
    pub fn outcome(&self) -> Option<Result<Window, PsramFault>> {
        if self.phase.load(Ordering::Acquire) != RECORDED {
            return None;
        }
        // SAFETY: the phase is RECORDED only once the outcome is written, and it is never written
        // again.
        Some(unsafe { (*self.outcome.get()).assume_init_read() })
    }

    /// The window's base in the cached view and its size in bytes, when the PSRAM came up.
    #[must_use]
    pub fn window(&self) -> Option<(usize, usize)> {
        match self.outcome() {
            Some(Ok(window)) => Some((window.base, window.bytes)),
            _ => None,
        }
    }
}

impl Default for PsramRecord {
    fn default() -> Self {
        Self::new()
    }
}

impl core::fmt::Debug for PsramRecord {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PsramRecord")
            .field("outcome", &self.outcome())
            .finish()
    }
}

/// A salt for the PSRAM's proof that differs between boots: 32 reads of the ring oscillator's
/// output, one bit each, as the datasheet's section 8.3.8 describes for a value that needs no
/// cryptographic strength. The bits are random while the system clocks run from the crystal, as
/// they do once [`init`] has applied the plan.
pub fn rosc_salt<R: Registers>(regs: &mut R) -> u32 {
    let mut salt = 0;
    for _ in 0..32 {
        salt = (salt << 1) | (regs.read(ROSC_RANDOMBIT) & 1);
    }
    salt
}

/// Brings up the board's PSRAM once [`init`] has applied the plan, as [`psram_bring_up`] does, and
/// records the window or why there is none. Nothing runs on a board without a PSRAM; when clk_sys
/// does not run from PLL_SYS, the one clk_sys rate the plan states; or when `record` already holds
/// an outcome or is making one. The record then stays as it was.
///
/// # Safety
///
/// As [`psram_bring_up`], with clk_sys final at the rate `applied` reports.
#[allow(unsafe_code)]
pub unsafe fn attach_psram(board: &Board, applied: Applied, record: &PsramRecord, salt: u32) {
    // SAFETY: the caller guarantees this function's contract, and `Mmio` is the hardware it names.
    unsafe { attach_psram_with(&mut qmi::Mmio, &mut qmi::Mmio, board, applied, record, salt) }
}

/// [`attach_psram`] against any registers and memory, so a test runs it against models of both.
///
/// # Safety
///
/// As [`attach_psram`] when either is the hardware.
#[allow(unsafe_code)]
pub unsafe fn attach_psram_with<R: Regs, M: Memory>(
    regs: &mut R,
    memory: &mut M,
    board: &Board,
    applied: Applied,
    record: &PsramRecord,
    salt: u32,
) {
    let Some(psram) = board.psram else { return };
    if !applied.pll_sys {
        return;
    }
    let sys_hz = board.plan.pll_sys_hz();
    // SAFETY: the caller guarantees the bring-up's contract, and clk_sys runs at the plan's rate.
    let _ =
        record.record_with(|| unsafe { psram_bring_up_with(regs, memory, &psram, sys_hz, salt) });
}

/// Starts the crystal and waits, boundedly, for it to report stable.
fn start_crystal<R: Registers>(regs: &mut R) -> bool {
    regs.write(instances::XOSC_BASE + xosc::STARTUP_OFF, xosc::STARTUP_DELAY_RESET);
    regs.write(
        instances::XOSC_BASE + xosc::CTRL_OFF,
        (xosc::CTRL_ENABLE_MAGIC << xosc::CTRL_ENABLE_LSB) | xosc::CTRL_FREQ_RANGE_1_15MHZ,
    );
    let status = instances::XOSC_BASE + xosc::STATUS_OFF;
    poll(regs, status, xosc::STATUS_STABLE, xosc::STATUS_STABLE, CRYSTAL_POLLS)
}

/// Puts clk_ref on the crystal and clk_sys on clk_ref, each divided by 1, so no PLL is reset while
/// a clock still runs from it. Answers whether clk_sys reported clk_ref.
///
/// EACH GLITCHLESS SWITCH MOVES WITH ITS AUX SELECT UNTOUCHED. The aux mux glitches when its select
/// changes, and a startup cannot know which aux source it was left on: the bootrom's USB bootloader
/// derives clk_sys from PLL_USB (datasheet erratum RP2350-E12), which is an aux source, and a reset
/// of the core does not reset the clock generators. So the
/// select changes only after SELECTED reports clk_ref (datasheet 8.1.3.2) -- here clk_sys's is left
/// as found, and [`clk_sys_on_pll`] sets it.
fn park_on_crystal<R: Registers>(regs: &mut R) -> bool {
    let base = instances::CLOCKS_BASE;
    let ref_ctrl = regs.read(base + clocks::CLK_REF_CTRL_OFF);
    regs.write(base + clocks::CLK_REF_CTRL_OFF, (ref_ctrl & !clocks::CLK_REF_CTRL_SRC) | clocks::CLK_REF_SRC_XOSC);
    let _ = poll(
        regs,
        base + clocks::CLK_REF_SELECTED_OFF,
        clocks::CLK_REF_XOSC_SELECTED,
        clocks::CLK_REF_XOSC_SELECTED,
        SWITCH_POLLS,
    );
    regs.write(base + clocks::CLK_REF_DIV_OFF, 1 << clocks::CLK_REF_DIV_INT_LSB);
    let sys_ctrl = regs.read(base + clocks::CLK_SYS_CTRL_OFF);
    regs.write(base + clocks::CLK_SYS_CTRL_OFF, sys_ctrl & !clocks::CLK_SYS_CTRL_SRC);
    let parked = poll(
        regs,
        base + clocks::CLK_SYS_SELECTED_OFF,
        clocks::CLK_SYS_REF_SELECTED,
        clocks::CLK_SYS_REF_SELECTED,
        SWITCH_POLLS,
    );
    if parked {
        regs.write(base + clocks::CLK_SYS_DIV_OFF, 1 << clocks::CLK_SYS_DIV_INT_LSB);
    }
    parked
}

/// Runs clk_peri from the crystal, divided by 1. clk_peri has no glitchless switch, so it is
/// stopped while its source changes.
fn clk_peri_on_crystal<R: Registers>(regs: &mut R) {
    let base = instances::CLOCKS_BASE;
    let source = clocks::CLK_PERI_AUXSRC_XOSC << clocks::CLK_PERI_CTRL_AUXSRC_LSB;
    regs.write(base + clocks::CLK_PERI_CTRL_OFF, 0);
    regs.write(base + clocks::CLK_PERI_CTRL_OFF, source);
    regs.write(base + clocks::CLK_PERI_DIV_OFF, 1 << clocks::CLK_PERI_DIV_INT_LSB);
    regs.write(base + clocks::CLK_PERI_CTRL_OFF, source | clocks::CLK_PERI_CTRL_ENABLE);
}

/// Brings PLL_SYS up and, once it locks, moves clk_sys onto it. Answers whether it locked.
fn clk_sys_on_pll<R: Registers>(board: &Board, regs: &mut R) -> bool {
    let plan = &board.plan;
    let locked = start_pll(
        regs,
        instances::PLL_SYS_BASE,
        instances::PLL_SYS_RESET_MASK,
        plan.pll_sys_fbdiv,
        plan.pll_sys_prim,
    );
    if locked {
        let base = instances::CLOCKS_BASE;
        regs.write(base + clocks::CLK_SYS_CTRL_OFF, 0);
        regs.write(base + clocks::CLK_SYS_CTRL_OFF, clocks::CLK_SYS_SRC_AUX);
        let _ = poll(
            regs,
            base + clocks::CLK_SYS_SELECTED_OFF,
            clocks::CLK_SYS_AUX_SELECTED,
            clocks::CLK_SYS_AUX_SELECTED,
            SWITCH_POLLS,
        );
    }
    locked
}

/// Stops clk_usb and clk_adc, brings PLL_USB up and, once it locks, runs both from it, divided by
/// 1. Answers whether it locked; when it did not, both stay stopped.
fn clk_usb_and_adc_on_pll<R: Registers>(board: &Board, regs: &mut R) -> bool {
    let base = instances::CLOCKS_BASE;
    let adc_ctrl = base + clocks::CLK_ADC_CTRL_OFF;
    regs.write(base + clocks::CLK_USB_CTRL_OFF, 0);
    regs.write(adc_ctrl, 0);
    let _ = poll(regs, adc_ctrl, clocks::CLK_ADC_CTRL_ENABLED, 0, SWITCH_POLLS);

    let plan = &board.plan;
    let locked = start_pll(
        regs,
        instances::PLL_USB_BASE,
        instances::PLL_USB_RESET_MASK,
        plan.pll_usb_fbdiv,
        plan.pll_usb_prim,
    );
    if locked {
        regs.write(base + clocks::CLK_USB_DIV_OFF, 1 << clocks::CLK_USB_DIV_INT_LSB);
        regs.write(base + clocks::CLK_USB_CTRL_OFF, clocks::CLK_USB_CTRL_ENABLE);
        regs.write(base + clocks::CLK_ADC_DIV_OFF, 1 << clocks::CLK_ADC_DIV_INT_LSB);
        regs.write(
            adc_ctrl,
            (clocks::CLK_ADC_AUXSRC_PLL_USB << clocks::CLK_ADC_CTRL_AUXSRC_LSB)
                | clocks::CLK_ADC_CTRL_ENABLE,
        );
        let _ = poll(
            regs,
            adc_ctrl,
            clocks::CLK_ADC_CTRL_ENABLED,
            clocks::CLK_ADC_CTRL_ENABLED,
            SWITCH_POLLS,
        );
    }
    locked
}

/// Resets one PLL and starts it with the crystal as its reference: the feedback divisor, the VCO
/// powered, then, once it locks, the post dividers. Answers whether it locked; when it did not, its
/// post dividers stay powered down.
fn start_pll<R: Registers>(regs: &mut R, base: u32, reset_mask: u32, fbdiv: u32, prim: u32) -> bool {
    let _ = reset_cycle(regs, reset_mask);
    regs.write(base + pll::CS_OFF, 1 << pll::CS_REFDIV_LSB);
    regs.write(base + pll::FBDIV_INT_OFF, fbdiv);
    regs.write(base + pll::PWR_CLR_OFF, pll::PWR_PD | pll::PWR_VCOPD);
    if !poll(regs, base + pll::CS_OFF, pll::CS_LOCK, pll::CS_LOCK, PLL_LOCK_POLLS) {
        return false;
    }
    regs.write(base + pll::PRIM_OFF, prim);
    regs.write(base + pll::PWR_CLR_OFF, pll::PWR_POSTDIVPD);
    true
}

/// Starts TIMER0's microsecond tick, then restarts TIMER0 from zero through its reset.
///
/// The tick generator is stopped while its count changes. The reset returns TIMER0 to its reset
/// values whatever ran before: counting ticks, not paused, and not locked.
fn start_timer<R: Registers>(board: &Board, regs: &mut R) {
    let ctrl = instances::TICKS_BASE + ticks::TIMER0_CTRL_OFF;
    regs.write(ctrl, 0);
    regs.write(instances::TICKS_BASE + ticks::TIMER0_CYCLES_OFF, board.tick_cycles());
    regs.write(ctrl, ticks::TIMER0_CTRL_ENABLE);
    let _ = reset_cycle(regs, instances::TIMER0_RESET_MASK);
}

/// Brings the console's UART up from its reset state: the UART alone is reset, the banks its pin
/// sits in are only released, the transmit pad is connected and routed, and the divisor is set
/// before the line format that latches it. Only the transmit side is enabled, and no interrupt.
fn bring_up_console<R: Registers>(console: &Console, regs: &mut R) {
    let banks = instances::IO_BANK0_RESET_MASK | instances::PADS_BANK0_RESET_MASK;
    let _ = reset_cycle(regs, console.release_mask & !banks);
    let _ = release(regs, console.release_mask);
    regs.write(console.pads_tx, pads::GPIO0_IE);
    regs.write(console.io_tx_ctrl, console.funcsel);
    let base = console.uart_base;
    regs.write(base + uart::UARTIBRD_OFF, console.ibrd);
    regs.write(base + uart::UARTFBRD_OFF, console.fbrd);
    regs.write(
        base + uart::UARTLCR_H_OFF,
        (uart::WLEN_8BIT << uart::UARTLCR_H_WLEN_LSB) | uart::UARTLCR_H_FEN,
    );
    regs.write(base + uart::UARTCR_OFF, uart::UARTCR_UARTEN | uart::UARTCR_TXE);
}

/// Reads TIMER0's 64-bit count without its latch: the high word, the low word, then the high word
/// again, until the two high words agree.
///
/// Each extra pass needs the high word to have moved between the two reads, which happens once
/// every 2^32 microseconds, so a second pass is rare and a third needs 71 minutes inside one read.
fn read_microseconds<R: Registers>(regs: &mut R) -> u64 {
    let high_address = instances::TIMER0_BASE + timer::TIMERAWH_OFF;
    let low_address = instances::TIMER0_BASE + timer::TIMERAWL_OFF;
    let mut high = regs.read(high_address);
    loop {
        let low = regs.read(low_address);
        let again = regs.read(high_address);
        if again == high {
            return (u64::from(high) << 32) | u64::from(low);
        }
        high = again;
    }
}

/// Puts the blocks in `mask` into reset and takes them out again, waiting boundedly for each edge.
/// They are taken out even when the first edge was not seen. Answers whether both edges were.
fn reset_cycle<R: Registers>(regs: &mut R, mask: u32) -> bool {
    regs.write(instances::RESETS_BASE + resets::RESET_SET_OFF, mask);
    let done = instances::RESETS_BASE + resets::RESET_DONE_OFF;
    let entered = poll(regs, done, mask, 0, SWITCH_POLLS);
    let left = release(regs, mask);
    entered && left
}

/// Takes the blocks in `mask` out of reset, waiting boundedly until each reports done. Answers
/// whether all did.
fn release<R: Registers>(regs: &mut R, mask: u32) -> bool {
    regs.write(instances::RESETS_CLR_BASE + resets::RESET_OFF, mask);
    let done = instances::RESETS_BASE + resets::RESET_DONE_OFF;
    poll(regs, done, mask, mask, SWITCH_POLLS)
}

/// Reads `address` until the bits in `mask` equal `want`, at most `polls` times. Answers whether
/// they did.
fn poll<R: Registers>(regs: &mut R, address: u32, mask: u32, want: u32, polls: u32) -> bool {
    let mut left = polls;
    while left != 0 {
        if regs.read(address) & mask == want {
            return true;
        }
        left -= 1;
    }
    false
}
