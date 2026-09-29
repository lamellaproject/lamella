//! Each board's descriptor: the facts its family driver reads, named from the board's generated
//! bindings.
//!
//! On a bare-metal target only the selected board is compiled. On the host every board is, so the
//! tests read each board's real facts.

#[cfg(any(feature = "board-pimoroni-pico-plus-2", not(target_os = "none")))]
pub mod pimoroni_pico_plus_2;
#[cfg(any(feature = "board-pimoroni-pico-plus-2-w", not(target_os = "none")))]
pub mod pimoroni_pico_plus_2_w;
#[cfg(any(feature = "board-rpi-pico2", not(target_os = "none")))]
pub mod rpi_pico2;
#[cfg(any(feature = "board-rpi-pico2-w", not(target_os = "none")))]
pub mod rpi_pico2_w;
