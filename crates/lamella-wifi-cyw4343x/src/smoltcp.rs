//! The device adapter: the driver's data plane presented to the caller's
//! `smoltcp` stack as a network device -- the received Ethernet frame
//! handed to the stack in place, the stack's frame staged in the driver's
//! transmit buffer and written under the credit window, the medium
//! Ethernet with a unit of 1,514 bytes and a burst of one.
//!
//! The adapter borrows the driver and the bus for the span of one poll of
//! the interface; the caller builds one around each call. A received frame
//! is handed to the stack only while the driver could take the stack's
//! reply -- the link up and no frame staged -- since the stack answers
//! inside the receive and the driver holds one frame; a transmit token is
//! offered only when the frame could be written now. After a poll of the
//! interface the caller polls the driver again at once when a frame is
//! held: the next poll writes it.

use ::smoltcp::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, RxToken, TxToken};
use ::smoltcp::time::Instant;
use ::smoltcp::wire::{EthernetAddress, HardwareAddress};

use crate::data::ETHERNET_MAX;
use crate::driver::{Driver, Sender};
use crate::transport::Transport;

/// The driver and its bus as a `smoltcp` device, for the span of one poll
/// of the interface.
pub struct Adapter<'a, 'b, T: Transport> {
    driver: &'a mut Driver<'b>,
    bus: &'a mut T,
}

/// The receive token: the Ethernet frame the driver's last poll handed
/// back, read in place.
pub struct Receive<'t> {
    frame: &'t [u8],
    taken: &'t mut bool,
}

/// The transmit token: a slot in the driver's transmit buffer, written
/// under the credit window once filled.
pub struct Transmit<'t, 'b, T: Transport> {
    sender: Sender<'t, 'b>,
    bus: &'t mut T,
}

impl<'a, 'b, T: Transport> Adapter<'a, 'b, T> {
    /// An adapter around `driver` and `bus`.
    pub fn new(driver: &'a mut Driver<'b>, bus: &'a mut T) -> Self {
        Adapter { driver, bus }
    }

    /// The driver.
    pub fn driver(&self) -> &Driver<'b> {
        self.driver
    }

    /// The hardware address of the interface: the chip's own address, once
    /// the radio-up request read it back.
    pub fn hardware_address(&self) -> Option<HardwareAddress> {
        self.driver
            .address()
            .map(|address| HardwareAddress::Ethernet(EthernetAddress(address)))
    }
}

impl<'b, T: Transport> Device for Adapter<'_, 'b, T> {
    type RxToken<'t>
        = Receive<'t>
    where
        Self: 't;
    type TxToken<'t>
        = Transmit<'t, 'b, T>
    where
        Self: 't;

    /// The frame the driver's last poll handed back and a token for the
    /// stack's reply, while the frame was not yet taken and the driver
    /// could take a reply.
    fn receive(&mut self, _timestamp: Instant) -> Option<(Receive<'_>, Transmit<'_, 'b, T>)> {
        let (received, sender) = self.driver.split();
        let (frame, taken) = received?;
        if !sender.can_stage() {
            return None;
        }
        Some((
            Receive { frame, taken },
            Transmit {
                sender,
                bus: &mut *self.bus,
            },
        ))
    }

    /// A token for a frame, while one could be staged and written now.
    fn transmit(&mut self, _timestamp: Instant) -> Option<Transmit<'_, 'b, T>> {
        let (_, sender) = self.driver.split();
        if !(sender.can_stage() && sender.can_write()) {
            return None;
        }
        Some(Transmit {
            sender,
            bus: &mut *self.bus,
        })
    }

    /// Ethernet; the unit 1,514 bytes; a burst of one; every checksum the
    /// stack's.
    fn capabilities(&self) -> DeviceCapabilities {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ethernet;
        capabilities.max_transmission_unit = ETHERNET_MAX;
        capabilities.max_burst_size = Some(1);
        capabilities.checksum = ChecksumCapabilities::default();
        capabilities
    }
}

impl RxToken for Receive<'_> {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let result = f(self.frame);
        *self.taken = true;
        result
    }
}

impl<T: Transport> TxToken for Transmit<'_, '_, T> {
    /// The slot staged for `len` bytes, filled by `f`, then written under
    /// the window or held for the next poll. The token exists only while a
    /// frame can be staged, and the stack never asks for more than the
    /// unit it was told; a stack that does has broken its contract.
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let Transmit { mut sender, bus } = self;
        let slot = sender
            .stage(len)
            .expect("a transmit token is offered only while a frame of at most the stated unit can be staged");
        let result = f(slot);
        let _ = sender.flush(bus);
        result
    }
}
