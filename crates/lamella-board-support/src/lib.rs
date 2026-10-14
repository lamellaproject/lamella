//! The board's half of a deployed image: the startup its clock plan names, the console byte sink,
//! and the monotonic millisecond clock.
//!
//! The runtime archive's board build compiles this library in, for one board, selected with one
//! `board-*` feature. The archive calls three symbols it does not define, and this library defines
//! them:
//!
//! - `lamella_board_init`, once at reset, after RAM is cleared and before the program's first
//!   instruction, applies the board's default clock plan and starts the millisecond clock, then
//!   brings up the PSRAM the board fits, if it fits one, and proves and wipes it;
//! - `lamella_board_console_putc` writes one console byte to the board's serial port, bringing the
//!   port up on the first byte, so a program that never prints never takes its pin;
//! - `lamella_board_now_ms` reads the clock in milliseconds, counted from 1 at the startup, or 0
//!   before the startup has run.
//!
//! `psram_window`, a Rust function, answers the PSRAM's window for the image's heap to take: its
//! base and size in bytes once the startup has brought it up, or `None` on a board without one, or
//! when the part did not come up.
//!
//! Every register address and value comes from the generated chip and board facts under `csp/` and
//! `bsp/`, except those the chip facts do not describe, each cited to the datasheet where it is
//! defined. The code here is written once per chip family, and a board contributes only the
//! descriptor that names its facts.
//!
//! Nothing here enables an interrupt, and every wait for the hardware is bounded. A startup that
//! finds no crystal leaves the clocks as it found them, and a byte the serial port does not take in
//! time is dropped. Bytes go out exactly as given, with no newline translation.
//!
//! The library defines no panic handler: the archive it is compiled into has the image's one.
#![no_std]

pub mod boards;
pub mod psram_flash;
pub mod registers;
pub mod rp2350;

/// How many `board-*` features this build selects.
#[cfg(target_os = "none")]
const BOARDS_SELECTED: u32 = cfg!(feature = "board-rpi-pico2") as u32
    + cfg!(feature = "board-rpi-pico2-w") as u32
    + cfg!(feature = "board-pimoroni-pico-plus-2") as u32
    + cfg!(feature = "board-pimoroni-pico-plus-2-w") as u32;

#[cfg(target_os = "none")]
const _: () = assert!(
    BOARDS_SELECTED == 1,
    "build this library for exactly one board: enable one `board-*` feature"
);

#[cfg(target_os = "none")]
pub use seams::psram_window;

/// The three symbols the runtime archive calls, for the board this library is built for, and the
/// PSRAM's window for the image's heap.
#[cfg(target_os = "none")]
mod seams {
    #[cfg(feature = "board-pimoroni-pico-plus-2")]
    use crate::boards::pimoroni_pico_plus_2::BOARD;
    #[cfg(feature = "board-pimoroni-pico-plus-2-w")]
    use crate::boards::pimoroni_pico_plus_2_w::BOARD;
    #[cfg(feature = "board-rpi-pico2")]
    use crate::boards::rpi_pico2::BOARD;
    #[cfg(feature = "board-rpi-pico2-w")]
    use crate::boards::rpi_pico2_w::BOARD;
    use crate::registers::Mmio;
    use crate::rp2350::{self, PsramRecord, State};

    /// What the board remembers between calls. It lives in the RAM the startup clears before
    /// `lamella_board_init` runs, so it starts as "console down, clock not started".
    static STATE: State = State::new();

    /// What became of the board's PSRAM at the startup, on a board that fits one. It lives in the
    /// RAM the startup clears, so it starts with nothing recorded.
    static PSRAM: PsramRecord = PsramRecord::new();

    /// The part's own registers.
    fn registers() -> Mmio {
        #[allow(unsafe_code)]
        unsafe {
            Mmio::new()
        }
    }

    /// Applies the board's default clock plan and starts the millisecond clock, then brings up the
    /// PSRAM the board fits, if it fits one.
    #[allow(unsafe_code)]
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_board_init() {
        let applied = rp2350::init(&BOARD, &mut registers(), &STATE);
        if BOARD.psram.is_some() {
            let salt = rp2350::rosc_salt(&mut registers());
            with_interrupts_masked(|| unsafe {
                rp2350::attach_psram(&BOARD, applied, &PSRAM, salt)
            });
        }
    }

    /// The PSRAM's window, as the base of its cached view and its size in bytes, once
    /// `lamella_board_init` has brought it up; `None` on a board without a PSRAM, before the
    /// startup, or when the part did not come up.
    #[must_use]
    pub fn psram_window() -> Option<(usize, usize)> {
        BOARD.psram?;
        PSRAM.window()
    }

    /// Runs `f` with every interrupt masked, and unmasks them after only if they were unmasked
    /// before.
    #[allow(unsafe_code)]
    fn with_interrupts_masked<T>(f: impl FnOnce() -> T) -> T {
        let primask: u32;
        // SAFETY: reads PRIMASK and then sets it, which masks every interrupt of configurable
        // priority. The asm is a barrier to memory accesses, so none moves out of the masked span.
        unsafe {
            core::arch::asm!(
                "mrs {0}, PRIMASK",
                "cpsid i",
                out(reg) primask,
                options(nostack, preserves_flags),
            )
        };
        let answer = f();
        if primask & 1 == 0 {
            // SAFETY: clears PRIMASK, which was clear before; the same barrier as above.
            unsafe { core::arch::asm!("cpsie i", options(nostack, preserves_flags)) };
        }
        answer
    }

    /// Writes one console byte to the board's serial port.
    #[allow(unsafe_code)]
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_board_console_putc(byte: u8) {
        let _ = rp2350::console_putc(&BOARD, &mut registers(), &STATE, byte);
    }

    /// The monotonic clock in milliseconds, or 0 before `lamella_board_init` has run.
    #[allow(unsafe_code)]
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_board_now_ms() -> u64 {
        rp2350::now_ms(&mut registers(), &STATE)
    }
}
