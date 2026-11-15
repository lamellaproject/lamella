//! Each board's descriptor: the facts its family driver reads, named from the board's generated
//! bindings.
//!
//! On a bare-metal target only the selected board is compiled. On the host every board is, so the
//! tests read each board's real facts.

#[cfg(any(feature = "board-adafruit-feather-rp2350-hstx", not(target_os = "none")))]
pub mod adafruit_feather_rp2350_hstx;
#[cfg(any(feature = "board-adafruit-feather-rp2350-hstx-psram", not(target_os = "none")))]
pub mod adafruit_feather_rp2350_hstx_psram;
#[cfg(any(feature = "board-adafruit-metro-rp2350", not(target_os = "none")))]
pub mod adafruit_metro_rp2350;
#[cfg(any(feature = "board-adafruit-metro-rp2350-psram", not(target_os = "none")))]
pub mod adafruit_metro_rp2350_psram;
#[cfg(any(feature = "board-pimoroni-pico-lipo-2", not(target_os = "none")))]
pub mod pimoroni_pico_lipo_2;
#[cfg(any(feature = "board-pimoroni-pico-lipo-2-xl-w", not(target_os = "none")))]
pub mod pimoroni_pico_lipo_2_xl_w;
#[cfg(any(feature = "board-pimoroni-pico-plus-2", not(target_os = "none")))]
pub mod pimoroni_pico_plus_2;
#[cfg(any(feature = "board-pimoroni-pico-plus-2-w", not(target_os = "none")))]
pub mod pimoroni_pico_plus_2_w;
#[cfg(any(feature = "board-raspberrypi-pico-2", not(target_os = "none")))]
pub mod raspberrypi_pico_2;
#[cfg(any(feature = "board-raspberrypi-pico-2-w", not(target_os = "none")))]
pub mod raspberrypi_pico_2_w;
#[cfg(any(feature = "board-sparkfun-iot-redboard-rp2350", not(target_os = "none")))]
pub mod sparkfun_iot_redboard_rp2350;
