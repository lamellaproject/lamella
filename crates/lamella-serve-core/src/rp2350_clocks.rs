//! What rate the RP2350's `clk_sys` is actually running at, read off the
//! chip's clock tree instead of declared by the firmware.
//!
//! # Why a firmware has to derive this rather than state it
//!
//! SysTick counts `clk_sys`, and the rate handed to `systick_clock::install` is what turns those
//! counts into milliseconds -- so that one number is the board's sense of time. `DateTime`,
//! `Environment.TickCount`, `Thread.Sleep` and every timeout built on them scale by it.
//!
//! An RP2350 firmware build that does not raise the PLL never programs the clock tree at all: it
//! runs on whatever the boot ROM left behind, so a constant describing that rate would be a claim
//! about the boot ROM rather than about this code. The crystal's 12 MHz is the tempting constant,
//! and it is wrong: the boot ROM can leave `clk_sys` on PLL_USB at 48 MHz, where a board that
//! assumed 12 MHz would report every duration four times too long.
//!
//! Nothing on the board can see an error like that, by construction. A self-timed benchmark and the
//! `Thread.Sleep` it measures are scaled by the same number, so they agree with each other at any
//! rate; only a clock off the board can show that both are wrong. Reading the mux answers what it
//! is selecting now, which is the one property a constant cannot have.
//!
//! # The single rule everything here follows
//!
//! A rate is derivable only when it traces back to the crystal. The XOSC, and either PLL while
//! it is locked and fed from the XOSC, have rates that are arithmetic. Every other source is refused
//! with `None` rather than given a plausible number.
//!
//! The ROSC is the case that makes the rule worth stating: it is a ring oscillator whose frequency
//! varies with process, voltage and temperature, and RP2350 A3 randomizes it by default, so the
//! datasheet's guarantee for the boot configuration is a range -- 18.4 MHz to 96.0 MHz (datasheet
//! 8.3.1). There is no constant that describes it, which is why the honest answer is a refusal and
//! not a default. It is also the chip's reset selection for `clk_sys`, so this is a state real
//! silicon can be in and not a defensive branch.

/// One PLL's four configuration words, in register order (datasheet 8.6.5).
#[derive(Clone, Copy)]
pub struct Pll {
    /// `CS`: LOCK (31), BYPASS (8), REFDIV (5:0).
    pub cs: u32,
    /// `PWR`: VCOPD (5), POSTDIVPD (3), DSMPD (2), PD (0).
    pub pwr: u32,
    /// `FBDIV_INT`: the feedback divisor. This PLL has no fractional division.
    pub fbdiv_int: u32,
    /// `PRIM`: POSTDIV1 (18:16), POSTDIV2 (14:12).
    pub prim: u32,
}

/// Everything the derivation reads, captured together.
///
/// A struct rather than a series of register reads inside the arithmetic, for the reason the pure
/// functions in `systick_clock.rs` exist: this is the code whose failure is a board that keeps
/// perfect time in the wrong units, so the arithmetic is worth proving on the host against words
/// read off real silicon.
#[derive(Clone, Copy)]
pub struct ClockTree {
    /// `CLOCKS: CLK_REF_CTRL` -- SRC (1:0), AUXSRC (6:5).
    pub clk_ref_ctrl: u32,
    /// `CLOCKS: CLK_REF_DIV` -- INT (23:16). This generator has no fractional part.
    pub clk_ref_div: u32,
    /// `CLOCKS: CLK_SYS_CTRL` -- SRC (0), AUXSRC (7:5).
    pub clk_sys_ctrl: u32,
    /// `CLOCKS: CLK_SYS_DIV` -- INT (31:16), FRAC (15:0).
    pub clk_sys_div: u32,
    pub pll_sys: Pll,
    pub pll_usb: Pll,
}

/// Register offsets from `CLOCKS_BASE` and from each PLL's base. The bases are generated CSP facts
/// and arrive as arguments; these offsets are the chip's register layout and belong with the code
/// that decodes the words.
const CLK_REF_CTRL: usize = 0x30;
const CLK_REF_DIV: usize = 0x34;
const CLK_SYS_CTRL: usize = 0x3c;
const CLK_SYS_DIV: usize = 0x40;
const PLL_CS: usize = 0x00;
const PLL_PWR: usize = 0x04;
const PLL_FBDIV_INT: usize = 0x08;
const PLL_PRIM: usize = 0x0c;

const PLL_CS_LOCK: u32 = 1 << 31;
const PLL_CS_BYPASS: u32 = 1 << 8;
const PLL_CS_REFDIV: u32 = 0x3f;
/// The three powerdowns that stop the output. `DSMPD` (bit 2) is deliberately not among them --
/// the datasheet's own note is that nothing is achieved by clearing it, and the boot ROM leaves it
/// set on a PLL that is running.
const PLL_PWR_OFF: u32 = (1 << 5) | (1 << 3) | (1 << 0);

/// `CLK_SYS_CTRL.SRC`: 0 selects `clk_ref`, 1 the auxiliary mux.
const CLK_SYS_SRC_AUX: u32 = 1;
/// `CLK_SYS_CTRL.AUXSRC` values that trace back to the crystal. 2 is the ROSC and 4/5 are the GPIO
/// inputs; both are refused.
const CLK_SYS_AUX_PLL_SYS: u32 = 0;
const CLK_SYS_AUX_PLL_USB: u32 = 1;
const CLK_SYS_AUX_XOSC: u32 = 3;
/// `CLK_REF_CTRL.SRC`: 1 selects the auxiliary mux, 2 the crystal. 0 is the phase-shifted ROSC and
/// 3 the 32 kHz LPOSC; both are refused.
const CLK_REF_SRC_AUX: u32 = 1;
const CLK_REF_SRC_XOSC: u32 = 2;
/// `CLK_REF_CTRL.AUXSRC`: 0 is PLL_USB. 1/2 are the GPIO inputs and 3 is a test path.
const CLK_REF_AUX_PLL_USB: u32 = 0;

/// A PLL's output rate, or `None` when the hardware says the output is not a number.
///
/// `LOCK` is treated as the authority on whether the configuration is sane, rather than re-checking
/// the datasheet's VCO and feedback ranges here: the PLL asserting lock is the chip's own statement
/// that it is running at the frequency these words describe, and a range check that disagreed would
/// take a working board's clock away over a spec boundary. What is checked instead is the set of
/// words that would make the arithmetic meaningless -- a zero divisor, and the powerdowns.
pub(crate) fn pll_hz(pll: &Pll, xosc_hz: u32) -> Option<u32> {
    if pll.cs & PLL_CS_LOCK == 0 {
        return None;
    }
    if pll.cs & PLL_CS_BYPASS != 0 {
        return None;
    }
    if pll.pwr & PLL_PWR_OFF != 0 {
        return None;
    }
    let refdiv = pll.cs & PLL_CS_REFDIV;
    let fbdiv = pll.fbdiv_int;
    let postdiv1 = (pll.prim >> 16) & 0x7;
    let postdiv2 = (pll.prim >> 12) & 0x7;
    if refdiv == 0 || fbdiv == 0 || postdiv1 == 0 || postdiv2 == 0 {
        return None;
    }
    let vco = u64::from(xosc_hz) * u64::from(fbdiv) / u64::from(refdiv);
    u32::try_from(vco / (u64::from(postdiv1) * u64::from(postdiv2))).ok()
}

/// Apply `CLK_SYS_DIV`, which is an INT.FRAC divisor with 16 fractional bits.
pub(crate) fn divide_clk_sys(src_hz: u32, div_word: u32) -> Option<u32> {
    let int = match (div_word >> 16) & 0xffff {
        0 => 1u64 << 16,
        n => u64::from(n),
    };
    let denominator = (int << 16) + u64::from(div_word & 0xffff);
    u32::try_from((u64::from(src_hz) << 16) / denominator).ok()
}

/// Apply `CLK_REF_DIV`, which is integer only -- bits 23:16, with no fractional part. Reading it
/// with `CLK_SYS_DIV`'s layout gives the right answer for the small divisors met in practice and
/// the wrong one for a large divisor, which is the kind of near-miss worth spelling out.
pub(crate) fn divide_clk_ref(src_hz: u32, div_word: u32) -> Option<u32> {
    let int = match (div_word >> 16) & 0xff {
        0 => 256u64,
        n => u64::from(n),
    };
    u32::try_from(u64::from(src_hz) / int).ok()
}

/// The rate `clk_ref` is running at, or `None` if its source does not trace back to the crystal.
pub fn clk_ref_hz_from(tree: &ClockTree, xosc_hz: u32) -> Option<u32> {
    let source = match tree.clk_ref_ctrl & 0x3 {
        CLK_REF_SRC_AUX => match (tree.clk_ref_ctrl >> 5) & 0x3 {
            CLK_REF_AUX_PLL_USB => pll_hz(&tree.pll_usb, xosc_hz)?,
            _ => return None,
        },
        CLK_REF_SRC_XOSC => xosc_hz,
        _ => return None,
    };
    divide_clk_ref(source, tree.clk_ref_div)
}

/// The rate `clk_sys` -- and therefore SysTick, and therefore the board's clock -- is running at.
///
/// `None` means the tree selects a source this firmware cannot put a number on. A caller must not
/// substitute one: a number substituted for a rate nobody read is exactly the error this file
/// exists to prevent.
pub fn clk_sys_hz_from(tree: &ClockTree, xosc_hz: u32) -> Option<u32> {
    let source = if tree.clk_sys_ctrl & 1 == CLK_SYS_SRC_AUX {
        match (tree.clk_sys_ctrl >> 5) & 0x7 {
            CLK_SYS_AUX_PLL_SYS => pll_hz(&tree.pll_sys, xosc_hz)?,
            CLK_SYS_AUX_PLL_USB => pll_hz(&tree.pll_usb, xosc_hz)?,
            CLK_SYS_AUX_XOSC => xosc_hz,
            _ => return None,
        }
    } else {
        clk_ref_hz_from(tree, xosc_hz)?
    };
    divide_clk_sys(source, tree.clk_sys_div)
}

/// Name the source `clk_sys` is running from, for the boot line a refusal has to print. A board
/// that will not install a clock has to say which source it could not price, or the message sends
/// its reader to look at the firmware that is behaving correctly.
#[must_use]
pub fn clk_sys_source_name(tree: &ClockTree) -> &'static str {
    if tree.clk_sys_ctrl & 1 != CLK_SYS_SRC_AUX {
        return match tree.clk_ref_ctrl & 0x3 {
            CLK_REF_SRC_AUX => match (tree.clk_ref_ctrl >> 5) & 0x3 {
                CLK_REF_AUX_PLL_USB => "clk_ref/pll_usb",
                1 => "clk_ref/gpin0",
                2 => "clk_ref/gpin1",
                _ => "clk_ref/pll_usb_primary_ref_opcg",
            },
            CLK_REF_SRC_XOSC => "clk_ref/xosc",
            0 => "clk_ref/rosc",
            _ => "clk_ref/lposc",
        };
    }
    match (tree.clk_sys_ctrl >> 5) & 0x7 {
        CLK_SYS_AUX_PLL_SYS => "pll_sys",
        CLK_SYS_AUX_PLL_USB => "pll_usb",
        2 => "rosc",
        CLK_SYS_AUX_XOSC => "xosc",
        4 => "gpin0",
        5 => "gpin1",
        _ => "reserved",
    }
}

#[cfg(target_os = "none")]
fn read_register(address: usize) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

/// Sample the live clock tree. The three bases are the generated CSP instance addresses, passed in
/// so this file stays compilable (and testable) on a host that has no RP2350.
#[cfg(target_os = "none")]
#[must_use]
pub fn read_tree(clocks_base: usize, pll_sys_base: usize, pll_usb_base: usize) -> ClockTree {
    let pll = |base: usize| Pll {
        cs: read_register(base + PLL_CS),
        pwr: read_register(base + PLL_PWR),
        fbdiv_int: read_register(base + PLL_FBDIV_INT),
        prim: read_register(base + PLL_PRIM),
    };
    ClockTree {
        clk_ref_ctrl: read_register(clocks_base + CLK_REF_CTRL),
        clk_ref_div: read_register(clocks_base + CLK_REF_DIV),
        clk_sys_ctrl: read_register(clocks_base + CLK_SYS_CTRL),
        clk_sys_div: read_register(clocks_base + CLK_SYS_DIV),
        pll_sys: pll(pll_sys_base),
        pll_usb: pll(pll_usb_base),
    }
}
