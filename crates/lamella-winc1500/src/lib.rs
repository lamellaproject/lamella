//! DEVICE networking backend for the interpreter's [`NetBackend`] seam, targeting the Microchip
//! ATWINC1500 Wi-Fi network CONTROLLER. Unlike `lamella-net-smoltcp` (which runs a Rust TCP/IP stack
//! over a raw MAC), the ATWINC1500 hosts its OWN TCP/IP stack on-module and exposes a SOCKET API over
//! SPI -- the WINC "HIF" host-interface protocol. So this backend maps each [`NetBackend`] op straight
//! onto a WINC HIF socket request (no smoltcp), and the module's IRQ-signalled response events drive
//! the readiness reactor.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::vec::Vec;
use lamella_cil_runtime::net::{Interest, NetBackend, NetResult, SocketHandle};

/// A host-agnostic, full-duplex SPI link to the ATWINC1500: every byte clocked out clocks a byte in.
/// A board supplies the concrete bus (a SAMD21 SERCOM-SPI on the MKR1000; a test double in unit
/// tests). Chip-select is asserted for the duration of a [`transfer`](SpiBus::transfer) -- the single
/// primitive the WINC SPI command set is built from.
pub trait SpiBus {
    /// The bus's error type.
    type Error: core::fmt::Debug;
    /// Clocks `tx` out while capturing the simultaneously-received bytes into `rx` (equal lengths),
    /// with chip-select asserted for the transfer.
    fn transfer(&mut self, tx: &[u8], rx: &mut [u8]) -> Result<(), Self::Error>;
}

/// The ATWINC1500's out-of-band control pins, driven by the host board alongside the [`SpiBus`]:
/// CHIP_EN + RESET_N bring the module out of power-down, and IRQN (active low) tells the host the WINC
/// has a response or event ready to read -- the edge the readiness reactor keys on.
pub trait WincControl {
    /// Drives CHIP_EN (module power-enable): `true` enables the module.
    fn set_chip_enable(&mut self, enabled: bool);
    /// Drives RESET_N (active-low reset): `true` holds the module in reset.
    fn set_reset(&mut self, asserted: bool);
    /// Reads IRQN (active low): `true` when the WINC is signalling that a response/event is waiting.
    fn irq_asserted(&mut self) -> bool;
}

/// A driver for one ATWINC1500 reached over an SPI bus `S` and its control pins `C`. Owns both, plus
/// (as later slices land) the driver-side socket table that maps [`SocketHandle`]s to WINC sockets.
pub struct Winc1500<S, C> {
    spi: S,
    ctrl: C,
}

impl<S, C> core::fmt::Debug for Winc1500<S, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Winc1500").finish_non_exhaustive()
    }
}

impl<S: SpiBus, C: WincControl> Winc1500<S, C> {
    /// Wraps the SPI bus + control pins. Does NOT touch the module yet -- `start` (next slice) toggles
    /// CHIP_EN / RESET_N and waits for the firmware-ready handshake before any socket op is valid.
    pub fn new(spi: S, ctrl: C) -> Self {
        Self { spi, ctrl }
    }

    /// Releases the SPI bus and control pins.
    pub fn into_parts(self) -> (S, C) {
        (self.spi, self.ctrl)
    }

}

/// Every socket op is stubbed against the seam until the SPI + HIF layers land: each reports
/// [`NetResult::Error`] (the WINC is not driven yet), so a program can link + run against this backend
/// without panicking, and the readiness reactor ([`NetBackend::poll`]) yields nothing until HIF events
/// are wired to [`WincControl::irq_asserted`].
impl<S: SpiBus, C: WincControl> NetBackend for Winc1500<S, C> {
    fn resolve(&mut self, _host: &str) -> Vec<Vec<u8>> {
        Vec::new()
    }

    fn tcp_connect(&mut self, _addr: &[u8], _port: u16) -> NetResult<SocketHandle> {
        NetResult::Error
    }

    fn connect_check(&mut self, _socket: SocketHandle) -> NetResult<()> {
        NetResult::Error
    }

    fn tcp_listen(&mut self, _addr: &[u8], _port: u16, _backlog: i32) -> NetResult<SocketHandle> {
        NetResult::Error
    }

    fn accept(&mut self, _listener: SocketHandle) -> NetResult<SocketHandle> {
        NetResult::Error
    }

    fn recv(&mut self, _socket: SocketHandle, _buf: &mut [u8]) -> NetResult<usize> {
        NetResult::Error
    }

    fn send(&mut self, _socket: SocketHandle, _buf: &[u8]) -> NetResult<usize> {
        NetResult::Error
    }

    fn udp_bind(&mut self, _addr: &[u8], _port: u16) -> NetResult<SocketHandle> {
        NetResult::Error
    }

    fn udp_send_to(
        &mut self,
        _socket: SocketHandle,
        _buf: &[u8],
        _addr: &[u8],
        _port: u16,
    ) -> NetResult<usize> {
        NetResult::Error
    }

    fn udp_recv_from(
        &mut self,
        _socket: SocketHandle,
        _buf: &mut [u8],
        _sender_addr: &mut [u8],
    ) -> NetResult<(usize, usize, u16)> {
        NetResult::Error
    }

    fn local_port(&mut self, _socket: SocketHandle) -> Option<u16> {
        None
    }

    fn close(&mut self, _socket: SocketHandle) {}

    fn register(&mut self, _socket: SocketHandle, _interest: Interest) {}

    fn deregister(&mut self, _socket: SocketHandle) {}

    fn poll(&mut self, _timeout_ms: Option<u64>) -> Vec<SocketHandle> {
        Vec::new()
    }
}
