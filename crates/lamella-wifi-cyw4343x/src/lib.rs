//! A host driver for the CYW43439 and CYW4343W WiFi radios, written on
//! `core` alone: the chip's bring-up behind one transport trait that a bus
//! implements, pumped by the caller so that every wait is a deadline handed
//! back and never a sleep inside. The one dependency, the `smoltcp` crate,
//! comes with the `smoltcp` feature and its device adapter.
//!
//! The driver is a state machine. The caller records a request (`attach`,
//! then `upload` with the firmware image and the settings image it
//! stages, then `open` with the regulatory blob and the version string
//! recorded beside them), then calls [`Driver::poll`] with the bus and its
//! clock; each call performs a bounded amount of bus work and returns a
//! [`Wake`] saying when to call again, or an [`Outcome`]. Once ready the
//! driver services the packet channel on every poll and hands each event
//! the firmware delivers to the caller as an outcome; the caller then
//! brings the radio up (`up`), lists the networks in range (`scan`, each
//! record carrying what the network advertises about its security) and
//! joins one (`join` with a WPA2 passphrase or none; `join_with` for a
//! WPA3 password too, the firmware running the SAE exchange), and the
//! driver keeps the link joined, returning to a scan and re-entering the
//! join on every loss until `disconnect`; the firmware's own capability
//! string can be read once ready (`capabilities`). While
//! the link is up the driver hands every data frame's Ethernet frame to
//! the caller in place and writes the one Ethernet frame the caller stages
//! (`stage`, `flush`, `send`) under the chip's credit window; the device
//! adapter behind the `smoltcp` feature presents both halves to the
//! caller's stack. A failure stops the machine at a named stage with the
//! status word it saw: a [`Refusal`].
//!
//! The bus is behind [`Transport`], the operations the gSPI and SDIO
//! transports both implement. The gSPI transport, [`gspi::Gspi`], sits
//! above [`gspi::GspiWire`], four operations a hardware engine or a test
//! fake implements. The SDIO transport, [`sdio::Sdio`], sits above
//! [`sdio::SdioHost`], seven operations a controller or a test fake
//! implements; the STM32H747's SDMMC1 controller is one such host, behind
//! the `stm32h7-sdmmc` feature, and the RP2350's programmable I/O block is
//! one such engine, behind the `rp2350-pio` feature. The bus-agnostic core
//! sees functions, addresses and bytes. A wrapper wire, [`trace::Traced`],
//! records every wire operation for replay.

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

#[cfg(test)]
extern crate std;

pub mod backplane;
pub mod clock;
pub mod control;
pub mod cores;
pub mod data;
pub mod download;
pub mod driver;
pub mod error;
pub mod event;
pub mod frame;
pub mod gspi;
pub mod link;
pub mod scan;
pub mod sdio;
pub mod station;
pub mod trace;
pub mod transport;

#[cfg(feature = "stm32h7-sdmmc")]
pub mod stm32h7;

#[cfg(feature = "rp2350-pio")]
pub mod rp2350;

#[cfg(feature = "smoltcp")]
pub mod smoltcp;

#[cfg(any(test, feature = "fixture"))]
pub mod fixture;

#[cfg(test)]
mod tests;

pub use clock::{Clock, Micros};
pub use control::Version;
pub use driver::{Driver, Outcome, Wake};
pub use error::Refusal;
pub use event::{Event, EventMask, Link};
pub use frame::Frames;
pub use link::{Credential, JoinFailure, LinkState, Network, Passphrase, SaePassword, Security};
pub use scan::{Advertised, ScanEnd, ScanRecord};
pub use sdio::{Sdio, SdioHost};
pub use station::Capabilities;
pub use transport::{Attach, Func, Part, Transport, Tune};
