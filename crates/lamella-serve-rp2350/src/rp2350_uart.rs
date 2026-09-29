//! UART0 bring-up and byte output for the RP2350 firmwares in this
//! crate -- the XOSC, `clk_peri`, the GP0/GP1 pin routing and the PL011 at 115200 8N1, all of it
//! composed from the generated board binding rather than from literals.
//!
//! This is the board's console: GP0 = TX, GP1 = RX, which is what the Raspberry Pi Debug Probe
//! bridges to a host COM port. Every peripheral address, the reset mask, the function select and
//! the baud divisor pair are `board_bindings` constants, so the pins a board actually wires are the
//! ones this drives and a board that wires them elsewhere is a regeneration rather than an edit.
//!
//! Callers include this by `#[path]`, so its `use crate::...` resolves against the including
//! binary's module set: a caller supplies `board_bindings` and `rp2350_instances` and nothing else.


use crate::board_bindings as board;
use crate::rp2350_instances as chip;

fn write_register(address: usize, value: u32) {
    unsafe { core::ptr::write_volatile(address as *mut u32, value) };
}

fn read_register(address: usize) -> u32 {
    unsafe { core::ptr::read_volatile(address as *const u32) }
}

// The crystal is 12 MHz on all four RP2350 boards (the Pico 2 and Pico 2 W, the Pimoroni Pico
// Plus 2 and Plus 2 W); `clk_peri` is pointed straight at it with no PLL, so the UART divisor
// is exact -- and stays exact if a build later moves `clk_sys` onto PLL_SYS, because the two clocks
// are separate. Instance bases are the generated chip rows; the block offsets and the composed
// enable words stay here, where the register manual is the authority.
const XOSC_CTRL: usize = chip::XOSC_BASE as usize;
const XOSC_STATUS: usize = chip::XOSC_BASE as usize + 0x4;
const XOSC_STARTUP: usize = chip::XOSC_BASE as usize + 0xc;
const XOSC_CTRL_ENABLE_1_15MHZ: u32 = 0x00fa_baa0;
const XOSC_STARTUP_DELAY: u32 = 0x00c4;
const XOSC_STABLE: u32 = 1 << 31;
const CLK_PERI_CTRL: usize = chip::CLOCKS_BASE as usize + 0x48;
const CLK_PERI_ENABLE: u32 = 1 << 11;
const CLK_PERI_AUXSRC_XOSC: u32 = 4 << 5;

const RESETS_CLR: usize = chip::RESETS_CLR_BASE as usize;
const RESETS_DONE: usize = chip::RESETS_BASE as usize + 0x8;

const UART0: usize = board::UART0_BASE as usize;
const UART_DR: usize = UART0 + 0x00;
const UART_FR: usize = UART0 + 0x18;
const UART_IBRD: usize = UART0 + 0x24;
const UART_FBRD: usize = UART0 + 0x28;
const UART_LCR_H: usize = UART0 + 0x2c;
const UART_CR: usize = UART0 + 0x30;
const FR_TXFF: u32 = 1 << 5;
/// WLEN 8-bit (`0b11 << 5`) | FEN (`1 << 4`).
const LCR_H_8N1_FIFO: u32 = 0x70;
/// UARTEN (`1 << 0`) | TXE (`1 << 8`) | RXE (`1 << 9`).
const CR_ENABLE: u32 = 0x301;
/// ISO clear (bit 8), so the pad drives at all -- RP2350 pads reset isolated.
const PAD_TX: u32 = 0x04;
/// The TX word plus IE (bit 6) for an input.
const PAD_RX_IE: u32 = 0x40;

/// Brings the crystal, `clk_peri`, the pins and the PL011 up. Idempotent enough to be called once
/// at boot and never again, which is all any caller here does.
pub fn init() {
    write_register(XOSC_STARTUP, XOSC_STARTUP_DELAY);
    write_register(XOSC_CTRL, XOSC_CTRL_ENABLE_1_15MHZ);
    while read_register(XOSC_STATUS) & XOSC_STABLE == 0 {}
    write_register(CLK_PERI_CTRL, CLK_PERI_ENABLE | CLK_PERI_AUXSRC_XOSC);
    let mask = board::UART0_RESET_MASK;
    write_register(RESETS_CLR, mask);
    while read_register(RESETS_DONE) & mask != mask {}
    write_register(board::UART0_IO_TX_CTRL as usize, board::UART0_FUNCSEL);
    write_register(board::UART0_IO_RX_CTRL as usize, board::UART0_FUNCSEL);
    write_register(board::UART0_PADS_TX as usize, PAD_TX);
    write_register(board::UART0_PADS_RX as usize, PAD_RX_IE);
    write_register(UART_IBRD, board::UART0_IBRD_115200_PLL_150_48);
    write_register(UART_FBRD, board::UART0_FBRD_115200_PLL_150_48);
    write_register(UART_LCR_H, LCR_H_8N1_FIFO);
    write_register(UART_CR, CR_ENABLE);
}

/// A RAM copy of everything this console has printed, readable over SWD when the UART is not.
///
/// A probe reaches RAM whether or not a UART lead is seated, wired to the right pins, or wired the
/// right way round -- so mirroring the bytes turns "did the program run and what did it say" from a
/// question about a cable into a memory read.
///
/// Off by default. The symbols are `#[unsafe(no_mangle)]` so a debug tool can find their addresses
/// in the ELF and read them without this crate telling it where they are.
#[cfg(feature = "console-mirror")]
mod mirror {
    /// Bytes kept. A boot banner plus a small program's output fits; beyond that the tail is
    /// dropped rather than wrapped, because a truncated prefix is what a reader can act on and a
    /// ring would leave them holding the end of a sentence with no beginning.
    pub const CAPACITY: usize = 4096;

    #[unsafe(no_mangle)]
    pub static mut LAMELLA_CONSOLE_MIRROR: [u8; CAPACITY] = [0; CAPACITY];

    /// How many of [`LAMELLA_CONSOLE_MIRROR`]'s bytes are real. Read this first: the buffer is
    /// zero-filled, and a reader who cannot tell zeros-because-unwritten from zeros-because-printed
    /// is reading a length out of the data it is describing.
    #[unsafe(no_mangle)]
    pub static mut LAMELLA_CONSOLE_MIRROR_LEN: u32 = 0;

    /// Appends one byte, silently ignoring anything past [`CAPACITY`].
    ///
    /// Volatile so the compiler cannot decide that a buffer nothing in this program reads is dead
    /// -- the reader is a debug probe on the other side of the bus, which no optimizer can see.
    pub fn push(byte: u8) {
        unsafe {
            let len_ptr = &raw mut LAMELLA_CONSOLE_MIRROR_LEN;
            let used = core::ptr::read_volatile(len_ptr) as usize;
            if used < CAPACITY {
                let base = (&raw mut LAMELLA_CONSOLE_MIRROR).cast::<u8>();
                core::ptr::write_volatile(base.add(used), byte);
                core::ptr::write_volatile(len_ptr, (used + 1) as u32);
            }
        }
    }
}

/// One byte, waiting for room in the FIFO.
///
/// The wait is bounded, and that is not caution: this runs on paths that may precede [`init`] -- an
/// early panic -- and an unconfigured PL011 never clears TXFF, so an unbounded spin would hang the
/// board inside the handler whose whole job is to say why it stopped.
pub fn tx(byte: u8) {
    #[cfg(feature = "console-mirror")]
    mirror::push(byte);
    let mut patience = 100_000;
    while read_register(UART_FR) & FR_TXFF != 0 && patience != 0 {
        patience -= 1;
    }
    write_register(UART_DR, u32::from(byte));
}

/// A whole string, allocating nothing -- which is what lets a panic handler print its reason when
/// the panic is an allocation failure. An `alloc::format!` there re-enters the allocator that has
/// just refused.
pub fn str(text: &str) {
    for byte in text.bytes() {
        tx(byte);
    }
}

/// An unsigned decimal, allocation-free, for the same reason as [`str`].
pub fn decimal(mut value: usize) {
    let mut digits = [0u8; 20];
    let mut length = 0;
    loop {
        digits[length] = b'0' + (value % 10) as u8;
        length += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    while length > 0 {
        length -= 1;
        tx(digits[length]);
    }
}
