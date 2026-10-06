//! The RP2350 clock tree the native-USB carrier needs: clk_sys at 150 MHz from PLL_SYS and clk_usb
//! at 48 MHz from PLL_USB, both from the 12 MHz crystal (datasheet 8.1, 8.2, 8.6). The controller
//! needs clk_usb = 48 MHz, and clk_sys must run at least 10 % faster than clk_usb while the
//! peripheral is in use (erratum RP2350-E12), so this brings clk_sys to the rated 150 MHz. clk_peri
//! is untouched: the UART bring-up has already put it on the crystal.
//!
//! Once clk_ref runs from the crystal, TIMER0 is started counting microseconds: the boot ROM leaves
//! its tick generator stopped, and the I2C driver times its waits on the bus against that count.
//!
//! The sequence runs over [`Registers`] rather than over raw addresses, so a host test runs this
//! same code over a model of the clock generators. What can go wrong here is the order of the
//! writes, and no read of the finished registers shows an order.
//!
//! # Each glitchless switch moves with its aux select untouched
//!
//! clk_ref and clk_sys each choose between glitchless sources and an aux mux, and the aux mux
//! glitches when its select changes. So the switch leaves aux first, SELECTED is polled until it
//! reports the new source, and only then may the aux select change (datasheet 8.1.3.2). A startup
//! cannot know which source it was left on: the bootrom's USB bootloader runs clk_sys from PLL_USB
//! through the aux mux (erratum RP2350-E12), and a reset of the core leaves the clock generators as
//! they were. Writing CLK_SYS_CTRL whole there moves the select to a stopped PLL_SYS in the same
//! write that asks the switch to leave aux, and clk_sys stops for good. So each switch here is a
//! read-modify-write of its SRC field alone, every wait is bounded, and a clk_sys that never reports
//! clk_ref keeps both PLLs, since either could be the source it still runs from.

use crate::board_bindings as board;
use crate::rp2350_instances as chip;

#[path = "../../../csp/rp2350/rust/rp2350_xosc_layout.rs"]
#[allow(dead_code)]
mod xosc;

/// The registers the clock sequence reads and writes: the chip's own on the board, a model in a
/// host test.
pub trait Registers {
    /// Reads the 32-bit register at `address`.
    fn read(&mut self, address: usize) -> u32;
    /// Writes `value` to the 32-bit register at `address`.
    fn write(&mut self, address: usize, value: u32);
}

/// clk_sys after [`init`]: the plan's rated speed, and comfortably past the RP2350-E12
/// requirement (clk_sys >= 1.1 x clk_usb).
pub const CLK_SYS_HZ: u32 = board::CLK_SYS_HZ_PLL_150_48;

// Instance bases and the clock plan's chosen values are generated constants; block offsets and
// composed block words stay in this file. The generated names carry their plan as a suffix
// because a board may declare several operating points; a firmware build is one (wire,
// operating point) pair, so the names used here need no suffix -- there is only ever the one
// to mean.

// XOSC (the board's crystal is 12 MHz), programmed as in the UART bring-up.
pub(crate) const XOSC_CTRL: usize = chip::XOSC_BASE as usize;
pub(crate) const XOSC_STATUS: usize = chip::XOSC_BASE as usize + 0x4;
pub(crate) const XOSC_STARTUP: usize = chip::XOSC_BASE as usize + 0xc;
const XOSC_CTRL_ENABLE_1_15MHZ: u32 = 0x00fa_baa0;
const XOSC_STARTUP_DELAY: u32 = xosc::STARTUP_DELAY_RESET;
pub(crate) const XOSC_STABLE: u32 = 1 << 31;

// RESETS: set alias +0x2000 puts a peripheral into reset, the placed clear alias takes it
// out; RESET_DONE (+0x8) confirms. The PLL masks are instance rows.
pub(crate) const RESETS_SET: usize = chip::RESETS_BASE as usize + 0x2000;
pub(crate) const RESETS_CLR: usize = chip::RESETS_CLR_BASE as usize;
pub(crate) const RESETS_DONE: usize = chip::RESETS_BASE as usize + 0x8;
pub(crate) const RESET_PLL_SYS: u32 = chip::PLL_SYS_RESET_MASK;
pub(crate) const RESET_PLL_USB: u32 = chip::PLL_USB_RESET_MASK;
const RESET_TIMER0: u32 = chip::TIMER0_RESET_MASK;

// TICKS: the generator that clocks TIMER0 ticks once every TIMER0_CYCLES periods of clk_ref; it is
// stopped while its count changes (datasheet 8.5).
const TICKS_TIMER0_CTRL: usize = chip::TICKS_BASE as usize + 0x18;
const TICKS_TIMER0_CYCLES: usize = chip::TICKS_BASE as usize + 0x1c;
const TICKS_ENABLE: u32 = 1 << 0;

// The two PLLs (8.6.5): CS (REFDIV[5:0], LOCK bit 31), PWR (PD bit 0, POSTDIVPD bit 3,
// VCOPD bit 5), FBDIV_INT, PRIM (POSTDIV1[18:16], POSTDIV2[14:12]).
pub(crate) const PLL_SYS_BASE: usize = chip::PLL_SYS_BASE as usize;
pub(crate) const PLL_USB_BASE: usize = chip::PLL_USB_BASE as usize;
// The post dividers stay local words composed into PRIM at runtime; the plan's generated,
// generation-verified PRIM words pin that composition at compile time, so a drift between the
// plan and this file refuses to build.
const PLL_SYS_POSTDIV1: u32 = 5;
const PLL_SYS_POSTDIV2: u32 = 2;
const PLL_USB_POSTDIV1: u32 = 5;
const PLL_USB_POSTDIV2: u32 = 5;
const _: () = assert!((PLL_SYS_POSTDIV1 << 16) | (PLL_SYS_POSTDIV2 << 12) == board::PLL_SYS_PRIM_PLL_150_48);
const _: () = assert!((PLL_USB_POSTDIV1 << 16) | (PLL_USB_POSTDIV2 << 12) == board::PLL_USB_PRIM_PLL_150_48);
pub(crate) const PLL_CS: usize = 0x00;
const PLL_PWR: usize = 0x04;
const PLL_FBDIV_INT: usize = 0x08;
const PLL_PRIM: usize = 0x0c;
pub(crate) const PLL_CS_LOCK: u32 = 1 << 31;
const PLL_PWR_PD: u32 = 1 << 0;
const PLL_PWR_POSTDIVPD: u32 = 1 << 3;
const PLL_PWR_VCOPD: u32 = 1 << 5;
const ATOMIC_CLR: usize = 0x3000;

// CLOCKS: the clock generators this init touches, at the placed base. clk_ref's SRC is bits 1:0
// and its SELECTED is one-hot on SRC; clk_sys's SRC is bit 0 (0 = clk_ref, 1 = the aux mux),
// AUXSRC bits 7:5, and its SELECTED is bit 0 for clk_ref and bit 1 for the aux mux.
pub(crate) const CLK_REF_CTRL: usize = chip::CLOCKS_BASE as usize + 0x30;
pub(crate) const CLK_REF_SELECTED: usize = chip::CLOCKS_BASE as usize + 0x38;
// clk_ref's divider: INT in bits 23:16, where 0 means 256.
const CLK_REF_DIV: usize = chip::CLOCKS_BASE as usize + 0x34;
const CLK_REF_DIV_INT_LSB: u32 = 16;
pub(crate) const CLK_SYS_CTRL: usize = chip::CLOCKS_BASE as usize + 0x3c;
pub(crate) const CLK_SYS_SELECTED: usize = chip::CLOCKS_BASE as usize + 0x44;
pub(crate) const CLK_USB_CTRL: usize = chip::CLOCKS_BASE as usize + 0x60;
const CLK_REF_CTRL_SRC: u32 = 0x3;
pub(crate) const CLK_REF_SRC_XOSC: u32 = 0x2;
const CLK_SYS_CTRL_SRC: u32 = 0x1;
pub(crate) const CLK_SYS_SRC_AUX: u32 = 0x1;
pub(crate) const CLK_SYS_SELECTED_REF: u32 = 1 << 0;
pub(crate) const CLK_SYS_SELECTED_AUX: u32 = 1 << 1;
const CLK_SYS_AUX_PLL_SYS: u32 = 0; // AUXSRC[7:5] = 0
const CLK_USB_AUX_PLL_USB: u32 = 0; // AUXSRC[7:5] = 0
const CLK_USB_ENABLE: u32 = 1 << 11;

/// How many times a wait reads its register before it gives up: far past a crystal's start-up, a
/// PLL's lock or a glitchless switch, each of which takes microseconds.
const PATIENCE: u32 = 1_000_000;

/// Reads `address` until every bit of `mask` is set, at most [`PATIENCE`] times, and answers
/// whether they were.
fn wait_for<R: Registers>(regs: &mut R, address: usize, mask: u32) -> bool {
    (0..PATIENCE).any(|_| regs.read(address) & mask == mask)
}

/// Pulse a RESETS bit with both edges confirmed: RESET_DONE deasserts while the block is held
/// in reset and reasserts once it is released. A fire-and-forget pulse is not enough -- the
/// deassert takes cycles to propagate (the USB controller spans the clk_sys / clk_usb domains),
/// and an unreset block would carry stale pre-reset state (e.g. a leftover device address).
pub fn reset_cycle<R: Registers>(regs: &mut R, reset_bit: u32) {
    regs.write(RESETS_SET, reset_bit);
    while regs.read(RESETS_DONE) & reset_bit != 0 {}
    regs.write(RESETS_CLR, reset_bit);
    while regs.read(RESETS_DONE) & reset_bit == 0 {}
}

/// Program one PLL: reset-cycle it, load REFDIV = 1 + `fbdiv`, power the VCO, wait for LOCK
/// (bounded), then set the post dividers and power them. The 8.6.4 sequence.
fn pll_init<R: Registers>(
    regs: &mut R,
    base: usize,
    reset_bit: u32,
    fbdiv: u32,
    postdiv1: u32,
    postdiv2: u32,
) -> bool {
    reset_cycle(regs, reset_bit);
    regs.write(base + PLL_CS, 1); // REFDIV = 1: the 12 MHz crystal feeds the VCO directly
    regs.write(base + PLL_FBDIV_INT, fbdiv);
    regs.write(base + PLL_PWR + ATOMIC_CLR, PLL_PWR_PD | PLL_PWR_VCOPD);
    if !wait_for(regs, base + PLL_CS, PLL_CS_LOCK) {
        return false;
    }
    regs.write(base + PLL_PRIM, (postdiv1 << 16) | (postdiv2 << 12));
    regs.write(base + PLL_PWR + ATOMIC_CLR, PLL_PWR_POSTDIVPD);
    true
}

/// Start TIMER0 counting microseconds from clk_ref, which runs from the crystal: a tick is the
/// crystal's rate over clk_ref's divider, in MHz, periods of clk_ref. The divider is read rather than
/// set, because something else on this board may run from clk_ref; the boot ROM leaves it at 4 on a
/// part booted from flash, so clk_ref is 3 MHz and a tick three of its periods. A divider that leaves
/// no whole number of periods in a microsecond leaves the generator as found, and TIMER0 then reads
/// as not counting to whatever times on it, where a tick of the wrong length would mismeasure every
/// wait. TIMER0 is released from reset if it is held, and its count is not reset.
fn start_microsecond_tick<R: Registers>(regs: &mut R) {
    let divider = match (regs.read(CLK_REF_DIV) >> CLK_REF_DIV_INT_LSB) & 0xff {
        0 => 256,
        int => int,
    };
    let ref_hz = board::XOSC_HZ_PLL_150_48 / divider;
    if board::XOSC_HZ_PLL_150_48 % divider != 0 || ref_hz % 1_000_000 != 0 || ref_hz == 0 {
        return;
    }
    regs.write(RESETS_CLR, RESET_TIMER0);
    let _ = wait_for(regs, RESETS_DONE, RESET_TIMER0);
    regs.write(TICKS_TIMER0_CTRL, 0);
    regs.write(TICKS_TIMER0_CYCLES, ref_hz / 1_000_000);
    regs.write(TICKS_TIMER0_CTRL, TICKS_ENABLE);
}

/// Bring the USB clock tree up: XOSC -> PLL_SYS -> clk_sys at 150 MHz, XOSC -> PLL_USB ->
/// clk_usb at 48 MHz. Returns `false` if the crystal, a glitchless switch or a PLL never comes
/// ready, degrading to UART-only operation instead of bricking the board behind an infinite wait.
/// Until clk_sys reports clk_ref, no PLL has been touched, so that failure leaves the clock tree
/// the board booted with running.
pub fn init<R: Registers>(regs: &mut R) -> bool {
    // XOSC up (idempotent when the UART bring-up already enabled it), bounded.
    regs.write(XOSC_STARTUP, XOSC_STARTUP_DELAY);
    regs.write(XOSC_CTRL, XOSC_CTRL_ENABLE_1_15MHZ);
    if !wait_for(regs, XOSC_STATUS, XOSC_STABLE) {
        return false;
    }

    // clk_ref onto the crystal, then clk_sys onto clk_ref, each switch moving with its aux select
    // untouched, so the glitchless parents are stable before either PLL is touched.
    let ref_ctrl = regs.read(CLK_REF_CTRL);
    regs.write(CLK_REF_CTRL, (ref_ctrl & !CLK_REF_CTRL_SRC) | CLK_REF_SRC_XOSC);
    if !wait_for(regs, CLK_REF_SELECTED, 1 << CLK_REF_SRC_XOSC) {
        return false;
    }
    start_microsecond_tick(regs);
    let sys_ctrl = regs.read(CLK_SYS_CTRL);
    regs.write(CLK_SYS_CTRL, sys_ctrl & !CLK_SYS_CTRL_SRC);
    if !wait_for(regs, CLK_SYS_SELECTED, CLK_SYS_SELECTED_REF) {
        return false;
    }

    // PLL_SYS: 12 MHz x 125 = 1500 MHz VCO / 5 / 2 = 150 MHz (the SDK's rated configuration;
    // the FBDIV is the plan's generated value, the postdivs the PRIM-pinned local words).
    if !pll_init(regs, PLL_SYS_BASE, RESET_PLL_SYS, board::PLL_SYS_FBDIV_PLL_150_48, PLL_SYS_POSTDIV1, PLL_SYS_POSTDIV2) {
        return false;
    }
    // clk_sys: aux = PLL_SYS, selected while the switch reports clk_ref, then the switch moves.
    regs.write(CLK_SYS_CTRL, CLK_SYS_AUX_PLL_SYS << 5);
    regs.write(CLK_SYS_CTRL, (CLK_SYS_AUX_PLL_SYS << 5) | CLK_SYS_SRC_AUX);
    if !wait_for(regs, CLK_SYS_SELECTED, CLK_SYS_SELECTED_AUX) {
        return false;
    }

    // PLL_USB: 12 MHz x 100 = 1200 MHz VCO / 5 / 5 = the controller's required 48 MHz.
    if !pll_init(regs, PLL_USB_BASE, RESET_PLL_USB, board::PLL_USB_FBDIV_PLL_150_48, PLL_USB_POSTDIV1, PLL_USB_POSTDIV2) {
        return false;
    }
    // clk_usb: aux = PLL_USB while disabled, then enable the generator.
    regs.write(CLK_USB_CTRL, CLK_USB_AUX_PLL_USB << 5);
    regs.write(CLK_USB_CTRL, (CLK_USB_AUX_PLL_USB << 5) | CLK_USB_ENABLE);

    true
}
