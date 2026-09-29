//! The RP2350 family: the clock plan its boards state, UART0 as the console, and TIMER0 as the
//! millisecond clock.
//!
//! Every RP2350 board states the same kind of plan: a crystal that feeds clk_ref and clk_peri
//! directly, PLL_SYS for clk_sys, and PLL_USB for clk_usb and clk_adc. The startup applies it from
//! any state the part was left in, so it parks clk_sys on the crystal before it resets the PLL
//! that clk_sys may be running on.

use core::sync::atomic::{AtomicBool, Ordering};

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

/// A board on this family.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Board {
    /// The board's console.
    pub console: Console,
    /// The board's default clock plan.
    pub plan: Plan,
}

impl Board {
    /// Refuses, at compile time when evaluated in a constant, a board whose facts this driver
    /// cannot apply as stated.
    ///
    /// The console's divisor must have been computed for clk_peri on the crystal, which is where
    /// the startup puts it. And the crystal must divide into whole microseconds within the tick
    /// generator's 9-bit count.
    ///
    /// # Panics
    ///
    /// When either does not hold.
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

/// Starts the crystal and waits, boundedly, for it to report stable.
fn start_crystal<R: Registers>(regs: &mut R) -> bool {
    regs.write(instances::XOSC_BASE + xosc::STARTUP_OFF, xosc::STARTUP_DELAY_1MS);
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
