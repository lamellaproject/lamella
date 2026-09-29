//! The host battery: recorded exchanges replayed against the driver.

mod attach_core;
mod attach_gspi;
mod attach_sdio;
mod control;
mod control_tie;
mod cores;
mod data;
mod data_tie;
mod download;
mod download_tie;
mod encoding;
mod event;
mod f2_sdio;
mod frame;
mod link;
#[cfg(feature = "rp2350-pio")]
mod rp2350;
mod scan;
mod sdio_encoding;
#[cfg(feature = "smoltcp")]
mod smoltcp;
mod station;
mod station_tie;
#[cfg(feature = "stm32h7-sdmmc")]
mod stm32h7;
mod trace;
mod tune;
