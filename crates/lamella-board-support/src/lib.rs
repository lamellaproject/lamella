//! The board's half of a deployed image: the startup its clock plan names, the console byte sink,
//! and the monotonic millisecond clock.
//!
//! The runtime archive's board build compiles this library in, for one board, selected with one
//! `board-*` feature. The archive calls three symbols it does not define, and this library defines
//! them:
//!
//! - `lamella_board_init`, once at reset, after RAM is cleared and before the program's first
//!   instruction, applies the board's default clock plan and starts the millisecond clock;
//! - `lamella_board_console_putc` writes one console byte to the board's serial port, bringing the
//!   port up on the first byte, so a program that never prints never takes its pin;
//! - `lamella_board_now_ms` reads the clock in milliseconds, counted from 1 at the startup, or 0
//!   before the startup has run.
//!
//! Every register address and value comes from the generated chip and board facts under `csp/` and
//! `bsp/`. The code here is written once per chip family, and a board contributes only the
//! descriptor that names its facts.
//!
//! Nothing here enables an interrupt, and every wait for the hardware is bounded. A startup that
//! finds no crystal leaves the clocks as it found them, and a byte the serial port does not take in
//! time is dropped. Bytes go out exactly as given, with no newline translation.
//!
//! The library defines no panic handler: the archive it is compiled into has the image's one.
#![no_std]

pub mod boards;
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

/// The three symbols the runtime archive calls, for the board this library is built for.
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
    use crate::rp2350::{self, State};

    /// What the board remembers between calls. It lives in the RAM the startup clears before
    /// `lamella_board_init` runs, so it starts as "console down, clock not started".
    static STATE: State = State::new();

    /// The part's own registers.
    fn registers() -> Mmio {
        #[allow(unsafe_code)]
        unsafe {
            Mmio::new()
        }
    }

    /// Applies the board's default clock plan and starts the millisecond clock.
    #[allow(unsafe_code)]
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_board_init() {
        let _ = rp2350::init(&BOARD, &mut registers(), &STATE);
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
