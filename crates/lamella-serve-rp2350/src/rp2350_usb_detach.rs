//! Taking the RP2350's USB device off the bus before the processors reset, long enough for the host
//! to see it leave, so the host enumerates it afresh when the next boot attaches it again.
//!
//! The firmware resets itself with Arm's SYSRESETREQ. On the RP2350 that is a warm reset of the
//! processors (datasheet 13.9, ARCHSEL: "The source of the warm reset could be the system power-up
//! state machine, the watchdog timer, Arm SYSRESETREQ or from RISC-V hartresetreq"), and none of the
//! chip-level resets that the datasheet's Table 528 lists. The USB controller is one of the
//! components the reset controller holds for software to release (7.5.1), so a warm reset leaves it
//! running, with its D+ pull-up (`SIE_CTRL.PULLUP_EN`) set; so does a debug probe's reset of the
//! processors. If the host misses that the device left, it keeps addressing the device at its old
//! address while the controller, reset again by the next boot, answers only at the default one: the
//! hazard USB 2.0 11.5.2.2 describes, a device at the Default Address while its port stays enabled.
//!
//! So the pull-up drops before the reset, once a packet the device still holds for the host has had
//! up to [`DELIVERY_US`] of real time to be taken, and stays down for [`HOLD_US`]. A hub sees a disconnect once SE0 lasts
//! TDDIS, 2.0 to 2.5 us, while it is not driving the lines (USB 2.0 7.1.7.3, Table 7-13). Its
//! disconnect timer is held reset for 4 ms after its port enters the Suspended or Disabled state
//! (11.5.2.2). A full-speed frame is 1.000 ms (7.1.12), and once the host stops addressing a device
//! that no longer answers, the lines are idle between one frame's start and the next. [`HOLD_US`]
//! outlasts that 4 ms by whole frames.
//!
//! The sequence runs over [`Board`] rather than over raw addresses, so a host test runs this same
//! code over a model of the registers and of elapsed time.

use crate::board_bindings as board;
use crate::rp2350_instances as chip;

#[path = "../../../csp/rp2350/rust/rp2350_xosc_layout.rs"]
#[allow(dead_code)]
mod xosc;

/// The registers the sequence reads and writes, and the one wait it makes: the chip's own on the
/// board, a model in a host test.
pub trait Board {
    /// Reads the 32-bit register at `address`.
    fn read(&mut self, address: usize) -> u32;
    /// Writes `value` to the 32-bit register at `address`.
    fn write(&mut self, address: usize, value: u32);
    /// Returns no sooner than `micros` microseconds after it is called.
    fn wait_at_least_us(&mut self, micros: u32);
}

/// How long the pull-up stays down before the device may attach again: the 4 ms a hub's port
/// ignores a disconnect after entering the Suspended or Disabled state (USB 2.0 11.5.2.2), and six
/// whole 1 ms frames (7.1.12) beyond it.
pub const HOLD_US: u32 = 10_000;

// RESETS (datasheet 7.5.3): RESET_DONE, at +0x8, has a block's bit set once that block is out of
// reset. The USB controller is bit 28; the chip's instance rows do not include it, so it is written
// here.
const RESETS_DONE: usize = chip::RESETS_BASE as usize + 0x8;
pub(crate) const RESET_USBCTRL: u32 = 1 << 28;

// The USB controller's SIE_CTRL (datasheet 12.7, controller registers at 0x5011_0000) and its atomic
// clear alias at +0x3000, which clears the bits written and no others (datasheet 2.1.3, "Atomic
// register access"; the RP2350 datasheet of build-version d126e9e). PULLUP_EN, bit 16, is the D+
// pull-up that presents a full-speed device.
const USBCTRL_REGS: usize = 0x5011_0000;
pub(crate) const SIE_CTRL: usize = USBCTRL_REGS + 0x4c;
const SIE_CTRL_CLR: usize = SIE_CTRL + 0x3000;
pub(crate) const SIE_CTRL_PULLUP_EN: u32 = 1 << 16;

// AIRCR (Armv8-M ARM, DDI 0553B.y, D1.2.3): a write must carry VECTKEY, 0x05FA, in bits [31:16];
// SYSRESETREQ, bit [2], requests the system reset.
const SCB_AIRCR: usize = 0xe000_ed0c;
const AIRCR_SYSRESETREQ: u32 = 0x05fa_0004;

// The IN buffer-control words in the controller's DPSRAM (datasheet 12.7.3.7.2): endpoint n's at
// 0x5010_0080 + 8n, for its 16 endpoints. Their AVAILABLE bit, 10, says who owns the buffer: the
// processor sets it to hand a packet to the controller, and the controller clears it when it has
// finished with the buffer (12.7.3.7.1), for an IN packet once the host has acknowledged it
// (12.7.3.8.2).
const DPRAM_IN_BUF_CTRL: usize = 0x5010_0080;
const ENDPOINTS: usize = 16;
const BUF_CTRL_AVAILABLE: u32 = 1 << 10;

/// The longest the device stays on the bus, in real time, for the host to take a packet it still
/// holds, such as the acknowledgement a host is waiting for when the reset that follows it begins:
/// five full-speed frames (USB 2.0 7.1.12). USB 2.0 promises a bulk endpoint no rate of service
/// (5.8.4), so this is a limit and not a guarantee: a packet the host has not taken by then is lost.
pub const DELIVERY_US: u32 = 5_000;

/// How long the device waits between two looks at its buffers and its count while a packet is still
/// held.
const DELIVERY_STEP_US: u32 = 100;

// The crystal oscillator (datasheet 8.2): STATUS's STABLE bit says it is running and stable (Table
// 600), and COUNT is a down counter at the crystal's own rate: a write starts it, it reads 1 while it
// counts and 0 once it has finished, and it counts at least 4 periods (Table 603).
const XOSC_STATUS: usize = chip::XOSC_BASE as usize + xosc::STATUS_OFF as usize;
const XOSC_STABLE: u32 = xosc::STATUS_STABLE;
const XOSC_COUNT: usize = chip::XOSC_BASE as usize + xosc::COUNT_OFF as usize;

/// [`DELIVERY_US`] in periods of the board's crystal, which the crystal's counter counts.
const DELIVERY_PERIODS: u32 = (DELIVERY_US as u64 * board::XOSC_HZ_PLL_150_48 as u64 / 1_000_000) as u32;
const _: () = assert!(
    DELIVERY_PERIODS >= 4 && DELIVERY_PERIODS <= xosc::COUNT_COUNT,
    "COUNT counts from 4 periods to its field's largest value"
);

/// Whether an IN buffer still holds a packet for the host.
fn holds_a_packet_for_the_host<B: Board>(board: &mut B) -> bool {
    (0..ENDPOINTS).any(|endpoint| board.read(DPRAM_IN_BUF_CTRL + 8 * endpoint) & BUF_CTRL_AVAILABLE != 0)
}

/// Gives a packet an IN buffer still holds up to [`DELIVERY_US`] of real time to be taken. The time
/// is the crystal's count and not the waits': a wait lasts at least what it asks, and on the chip as
/// much longer as clk_sys runs below the rate its passes are counted for, which at a boot is whatever
/// the reset left. A device whose crystal is not running does not wait: its PLLs take their reference
/// from the crystal (datasheet 8.6.3) and the ring oscillator cannot clock USB (8.3.1), so it has no
/// clk_usb to send a packet with.
fn deliver<B: Board>(board: &mut B) {
    if board.read(XOSC_STATUS) & XOSC_STABLE == 0 {
        return;
    }
    board.write(XOSC_COUNT, DELIVERY_PERIODS);
    // The looks are bounded as well, so a count that never finished could not hold the board here.
    // Each step lasts at least DELIVERY_STEP_US, so together they last at least DELIVERY_US and never
    // end the wait before the count does.
    for _ in 0..DELIVERY_US / DELIVERY_STEP_US {
        if !holds_a_packet_for_the_host(board) {
            return;
        }
        board.wait_at_least_us(DELIVERY_STEP_US);
        if board.read(XOSC_COUNT) == 0 {
            return;
        }
    }
}

/// Takes the device off the bus and keeps it off for [`HOLD_US`], whenever the USB controller is out
/// of reset. Answers whether it was, and so held.
///
/// When its pull-up is set, a packet an IN buffer still holds is first given up to [`DELIVERY_US`]
/// of real time to be taken, so a host that was told something just before the reset can hear it;
/// then the pull-up drops. When it is already clear, the hold is held all the same: a reset that cut an earlier hold
/// short, such as a probe's, leaves the controller just as a finished hold does, and the two cannot
/// be told apart.
///
/// A controller still held in reset, as after a power-on or any chip-level reset, is left alone and
/// costs nothing: its pull-up is off, and the datasheet asks that a block's reset be released before
/// the block is used (7.5.1).
pub fn detach<B: Board>(board: &mut B) -> bool {
    if board.read(RESETS_DONE) & RESET_USBCTRL == 0 {
        return false;
    }
    if board.read(SIE_CTRL) & SIE_CTRL_PULLUP_EN != 0 {
        deliver(board);
        board.write(SIE_CTRL_CLR, SIE_CTRL_PULLUP_EN);
    }
    board.wait_at_least_us(HOLD_US);
    true
}

/// [`detach`]es the device, then resets the processors with SYSRESETREQ. A host that saw the device
/// sees it leave before the reset, and enumerates it again once the next boot sets the pull-up.
///
/// The architecture does not guarantee that the reset takes place immediately (Armv8-M ARM, DDI
/// 0553B.y, B1.1), so a caller follows this with a loop that never ends.
pub fn detach_and_reset<B: Board>(board: &mut B) {
    detach(board);
    board.write(SCB_AIRCR, AIRCR_SYSRESETREQ);
}
