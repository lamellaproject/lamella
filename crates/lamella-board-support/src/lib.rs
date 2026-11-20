//! The board's half of a deployed image: the startup its clock plan names, the console byte sink,
//! and the monotonic millisecond clock.
//!
//! The runtime archive's board build compiles this library in, for one board, selected with one
//! `board-*` feature. An RP2350 board's archive calls three symbols it does not define, and this
//! library defines them for it:
//!
//! - `lamella_board_init`, once at reset, after RAM is cleared and before the program's first
//!   instruction, applies the board's default clock plan and starts the millisecond clock, then
//!   brings up the PSRAM the board fits, if it fits one, and proves and wipes it;
//! - `lamella_board_console_putc` writes one console byte to the board's serial port, bringing the
//!   port up on the first byte, so a program that never prints never takes its pin;
//! - `lamella_board_now_ms` reads the clock in milliseconds, counted from 1 at the startup, or 0
//!   before the startup has run.
//!
//! An STM32 board's archive keeps its console in the ring a debug probe reads and calls the first
//! and the last: its `lamella_board_init` applies no clock plan and starts the millisecond clock
//! (`stm32`), on the NUCLEO-U5A5ZJ-Q after validating the board's VDDIO2 rail (`stm32u5`).
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
#[cfg(any(feature = "rp2350", not(target_os = "none")))]
pub mod psram_flash;
pub mod registers;
#[cfg(any(feature = "rp2350", not(target_os = "none")))]
pub mod rp2350;
#[cfg(any(feature = "stm32f7", feature = "stm32u5", not(target_os = "none")))]
pub mod stm32;
#[cfg(any(feature = "stm32u5", not(target_os = "none")))]
pub mod stm32u5;

/// How many `board-*` features this build selects.
#[cfg(target_os = "none")]
const BOARDS_SELECTED: u32 = cfg!(feature = "board-raspberrypi-pico-2") as u32
    + cfg!(feature = "board-raspberrypi-pico-2-w") as u32
    + cfg!(feature = "board-pimoroni-pico-plus-2") as u32
    + cfg!(feature = "board-pimoroni-pico-plus-2-w") as u32
    + cfg!(feature = "board-adafruit-feather-rp2350-hstx") as u32
    + cfg!(feature = "board-adafruit-feather-rp2350-hstx-psram") as u32
    + cfg!(feature = "board-adafruit-metro-rp2350") as u32
    + cfg!(feature = "board-adafruit-metro-rp2350-psram") as u32
    + cfg!(feature = "board-pimoroni-pico-lipo-2") as u32
    + cfg!(feature = "board-pimoroni-pico-lipo-2-xl-w") as u32
    + cfg!(feature = "board-sparkfun-iot-redboard-rp2350") as u32
    + cfg!(feature = "board-st-nucleo-u5a5zj-q") as u32
    + cfg!(feature = "board-st-stm32f746g-disco") as u32
    + cfg!(feature = "board-st-stm32f769i-disco") as u32;

#[cfg(target_os = "none")]
const _: () = assert!(
    BOARDS_SELECTED == 1,
    "build this library for exactly one board: enable one `board-*` feature"
);

#[cfg(all(target_os = "none", feature = "rp2350"))]
pub use seams::psram_window;

/// Runs `f` with every interrupt masked, and unmasks them after only if they were unmasked before.
#[cfg(target_os = "none")]
#[allow(unsafe_code)]
#[inline(always)]
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

/// The three symbols the runtime archive calls, for the RP2350 board this library is built for, and
/// the PSRAM's window for the image's heap.
#[cfg(all(target_os = "none", feature = "rp2350"))]
mod seams {
    #[cfg(feature = "board-adafruit-feather-rp2350-hstx")]
    use crate::boards::adafruit_feather_rp2350_hstx::BOARD;
    #[cfg(feature = "board-adafruit-feather-rp2350-hstx-psram")]
    use crate::boards::adafruit_feather_rp2350_hstx_psram::BOARD;
    #[cfg(feature = "board-adafruit-metro-rp2350")]
    use crate::boards::adafruit_metro_rp2350::BOARD;
    #[cfg(feature = "board-adafruit-metro-rp2350-psram")]
    use crate::boards::adafruit_metro_rp2350_psram::BOARD;
    #[cfg(feature = "board-pimoroni-pico-lipo-2")]
    use crate::boards::pimoroni_pico_lipo_2::BOARD;
    #[cfg(feature = "board-pimoroni-pico-lipo-2-xl-w")]
    use crate::boards::pimoroni_pico_lipo_2_xl_w::BOARD;
    #[cfg(feature = "board-pimoroni-pico-plus-2")]
    use crate::boards::pimoroni_pico_plus_2::BOARD;
    #[cfg(feature = "board-pimoroni-pico-plus-2-w")]
    use crate::boards::pimoroni_pico_plus_2_w::BOARD;
    #[cfg(feature = "board-raspberrypi-pico-2")]
    use crate::boards::raspberrypi_pico_2::BOARD;
    #[cfg(feature = "board-raspberrypi-pico-2-w")]
    use crate::boards::raspberrypi_pico_2_w::BOARD;
    #[cfg(feature = "board-sparkfun-iot-redboard-rp2350")]
    use crate::boards::sparkfun_iot_redboard_rp2350::BOARD;
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
            crate::with_interrupts_masked(|| unsafe {
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

/// The two symbols the runtime archive calls for the STM32 board this library is built for, whose
/// image's console is the ring a debug probe reads.
#[cfg(all(target_os = "none", any(feature = "stm32f7", feature = "stm32u5")))]
mod stm32_seams {
    #[cfg(feature = "board-st-nucleo-u5a5zj-q")]
    use crate::boards::st_nucleo_u5a5zj_q::{CLOCK, VDDIO2};
    #[cfg(feature = "board-st-stm32f746g-disco")]
    use crate::boards::st_stm32f746g_disco::CLOCK;
    #[cfg(feature = "board-st-stm32f769i-disco")]
    use crate::boards::st_stm32f769i_disco::CLOCK;
    use crate::registers::Mmio;
    use crate::stm32::{self, Clock};

    /// What the clock remembers between reads. It lives in the RAM the startup clears before
    /// `lamella_board_init` runs, so it starts as "clock not started".
    static STATE: Clock = Clock::new();

    /// The part's own registers.
    fn registers() -> Mmio {
        #[allow(unsafe_code)]
        unsafe {
            Mmio::new()
        }
    }

    /// Starts the board's millisecond clock, on the NUCLEO-U5A5ZJ-Q after validating its VDDIO2 rail,
    /// so the pads it powers (PG2 to PG15) answer the program.
    #[allow(unsafe_code)]
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_board_init() {
        #[cfg(feature = "board-st-nucleo-u5a5zj-q")]
        crate::stm32u5::apply(&mut registers(), VDDIO2);
        stm32::start(&CLOCK, &mut registers(), &STATE);
    }

    /// The monotonic clock in milliseconds, or 0 before `lamella_board_init` has run. Each read is
    /// made with every interrupt masked, so no read can interrupt another.
    #[allow(unsafe_code)]
    #[unsafe(no_mangle)]
    pub extern "C" fn lamella_board_now_ms() -> u64 {
        crate::with_interrupts_masked(|| stm32::now_ms(&CLOCK, &mut registers(), &STATE))
    }
}
