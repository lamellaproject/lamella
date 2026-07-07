//! DEVICE networking backend for the interpreter's `NetBackend` seam, targeting the Microchip
//! ATWINC1500 Wi-Fi network CONTROLLER. Unlike `lamella-net-smoltcp` (which runs a Rust TCP/IP stack
//! over a raw MAC), the ATWINC1500 hosts its OWN TCP/IP stack on-module and exposes a SOCKET API over
//! SPI -- the WINC "HIF" host-interface protocol. So this backend maps each seam op straight
//! onto a WINC HIF socket request (no smoltcp), and the module's IRQ-signalled response events drive
//! the readiness reactor.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

pub mod boot;
pub mod hif;
pub mod net;
pub mod socket;
pub mod spi;
pub mod wifi;

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
    /// Sleeps at least `ms` milliseconds -- the power-up sequence's settle times. A firmware
    /// board spins a timer; a host-side board sleeps the thread.
    fn delay_ms(&mut self, ms: u32);
}

/// A driver for one ATWINC1500 reached over an SPI bus `S` and its control pins `C`: the bring-up
/// half (power, SPI protocol, firmware boot) -- [`net::WincNet`] takes the parts over for the
/// socket-serving half via [`into_net_parts`](Self::into_net_parts).
pub struct Winc1500<S, C> {
    link: spi::Link<S>,
    ctrl: C,
}

impl<S, C> core::fmt::Debug for Winc1500<S, C> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Winc1500").finish_non_exhaustive()
    }
}

impl<S: SpiBus, C: WincControl> Winc1500<S, C> {
    /// Wraps the SPI bus + control pins. Does NOT touch the module yet -- [`start`](Self::start)
    /// runs the power-up sequence before any register access is valid.
    pub fn new(spi: S, ctrl: C) -> Self {
        Self { link: spi::Link::new(spi), ctrl }
    }

    /// Releases the SPI bus and control pins.
    pub fn into_parts(self) -> (S, C) {
        (self.link.into_bus(), self.ctrl)
    }

    /// Powers the module up and brings the SPI protocol to its CRC-less operating state: the
    /// vendor power-up sequence (CHIP_EN low + RESET_N low, 1 ms; CHIP_EN high, 10 ms; RESET_N
    /// high, then a settle) followed by the protocol-config handshake. After this, registers are
    /// readable -- the firmware boot (HIF) is a later slice.
    pub fn start(&mut self) -> Result<(), spi::SpiError> {
        self.ctrl.set_chip_enable(false);
        self.ctrl.set_reset(true);
        self.ctrl.delay_ms(1);
        self.ctrl.set_chip_enable(true);
        self.ctrl.delay_ms(10);
        self.ctrl.set_reset(false);
        self.ctrl.delay_ms(10);
        self.link.init()?;
        Ok(())
    }

    /// Reads the module's chip-identity register (`NMI_CHIPID`) -- the WINC first-light readout:
    /// an ATWINC1500 answers an id in the 0x1500xx/0x1503xx family, and any sane value proves the
    /// SPI wiring + protocol end to end.
    pub fn chip_id(&mut self) -> Result<u32, spi::SpiError> {
        self.link.read_reg(spi::NMI_CHIPID)
    }

    /// The module-memory bus (registers + blocks) -- what the [`hif`]/[`wifi`] request and
    /// event functions drive.
    pub fn bus(&mut self) -> &mut spi::Link<S> {
        &mut self.link
    }

    /// The board's millisecond delay, for event-poll pacing.
    pub fn delay_ms(&mut self, ms: u32) {
        self.ctrl.delay_ms(ms);
    }

    /// The module's IRQN line (active low): `true` when the WINC signals a pending
    /// response/event -- the vendor-faithful gate for [`hif::poll_event`] on a fast transport
    /// (hammering the interrupt register between events costs bus time for nothing).
    pub fn irq_asserted(&mut self) -> bool {
        self.ctrl.irq_asserted()
    }

    /// Boots the module's Wi-Fi firmware (from its own serial flash) to M2M readiness and
    /// returns its version. Call after [`start`](Self::start).
    pub fn boot_firmware(&mut self) -> Result<boot::FirmwareVersion, boot::BootError> {
        let chip_rev = self.link.read_reg(spi::NMI_CHIPID)? & 0xfff;
        let Self { link, ctrl } = self;
        boot::boot_firmware(link, |ms| ctrl.delay_ms(ms), chip_rev)?;
        boot::firmware_version(link).map_err(boot::BootError::Spi)
    }

    /// Splits the driver into the protocol link + control pins -- the parts
    /// [`net::WincNet::new`] takes over once the bring-up (start / firmware boot / join) is
    /// done. The association lives on the module, so it survives the handoff.
    pub fn into_net_parts(self) -> (spi::Link<S>, C) {
        (self.link, self.ctrl)
    }
}

