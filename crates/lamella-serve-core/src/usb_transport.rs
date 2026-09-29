//! The native-USB (driverless WinUSB) Lamella Link carrier, generic over
//! the device controller -- each chip's controller (the RP2350's is `rp2350_usb`) plugs in here, so
//! the descriptors and the frame-level carrier are written once.
//!
//! Three layers, all glued to the shared identity in `lamella_wire::usb` (firmware, host and
//! browser read the same constants, so they cannot drift):
//! - [`WinUsbClass`]: the `usb-device` class exposing one vendor-specific interface (bulk IN 0x81 /
//!   OUT 0x01) whose BOS + Microsoft OS 2.0 descriptors make Windows auto-bind `winusb.sys` and
//!   register the interface GUID -- no INF, no Zadig.
//! - [`UsbCarrier`]: the frame-level `lamella_wire::Transport` over the bulk pipes, polled from the
//!   firmware's main loop (no USB interrupt: enumeration is serviced by the same polls that watch
//!   the wire, so the vector table is untouched). A firmware that listens on this and a UART puts
//!   both into a `lamella_runner::carriers::CarrierSet`, which arbitrates one session between them.
//!   This file carries no multiplexer of its own -- two of them is one too many.
//! - [`build_device`]: the device identity -- shared VID/PID, a per-board product string, and a
//!   per-board serial (the RP2350 derives its serial from the chip's unique id, so a host picker
//!   can tell two boards apart).
//!
//! The chip-specific halves -- clock trees and the `usb_device::bus::UsbBus` implementations --
//! live with each family's firmware (for the RP2350, `rp2350_usb.rs` in `lamella-serve-rp2350`) and
//! are selected by the firmware binary, which `#[path]`-includes this carrier from
//! `lamella-serve-core`.
// Shared by several binaries that each use a subset (the default-endpoint builder vs the
// explicit-endpoint one), so unused-item lints are per-bin noise.
#![allow(dead_code)]

extern crate alloc;

use lamella_wire::usb as ids;
use lamella_wire::{Frame, FrameReader, Transport, TransportError, encode_frame};
use usb_device::UsbError;
use usb_device::bus::{InterfaceNumber, UsbBus, UsbBusAllocator};
use usb_device::class::{ControlIn, UsbClass};
use usb_device::control::{Recipient, RequestType};
use usb_device::descriptor::{BosWriter, DescriptorWriter, capability_type};
use usb_device::device::{
    StringDescriptors, UsbDevice, UsbDeviceBuilder, UsbDeviceState, UsbVidPid,
};
use usb_device::endpoint::{EndpointAddress, EndpointIn, EndpointOut, EndpointType};

// ---- critical section ---------------------------------------------------------------------

// ---- the WinUSB vendor class ----------------------------------------------------------------

/// The single vendor-specific interface carrying lamella-wire frames over bulk pipes, plus the
/// descriptors that make it driverless on Windows: the BOS platform capabilities (appended in
/// `get_bos_descriptors`; usb-device emits the BOS because the device reports bcdUSB 0x0210) and
/// the Microsoft OS 2.0 descriptor set (answered in `control_in`), whose `CompatibleID "WINUSB"`
/// binds `winusb.sys` and whose registry property registers the interface GUID a host opens.
pub struct WinUsbClass<'a, B: UsbBus> {
    iface: InterfaceNumber,
    ep_in: EndpointIn<'a, B>,
    ep_out: EndpointOut<'a, B>,
}

impl<'a, B: UsbBus> WinUsbClass<'a, B> {
    pub fn new(alloc: &'a UsbBusAllocator<B>) -> Self {
        // The default endpoint addresses (the shared identity's 0x81/0x01): pinned
        // explicitly rather than trusting allocation order.
        Self::with_endpoints(alloc, ids::BULK_IN_EP, ids::BULK_OUT_EP)
    }

    /// [`Self::new`] with explicit bulk endpoint addresses, for a controller whose endpoints
    /// are unidirectional per endpoint number (the SAM4S UDP: one FIFO per number, the type
    /// selects IN or OUT), where IN and OUT cannot share number 1. Hosts are unaffected:
    /// every opener (lamella-usbbulk, WebUSB) discovers the bulk pipes from the interface
    /// descriptor rather than assuming fixed addresses.
    pub fn with_endpoints(alloc: &'a UsbBusAllocator<B>, in_ep: u8, out_ep: u8) -> Self {
        Self {
            iface: alloc.interface(),
            ep_in: alloc
                .alloc(
                    Some(EndpointAddress::from(in_ep)),
                    EndpointType::Bulk,
                    ids::BULK_MAX_PACKET,
                    0,
                )
                .expect("bulk IN endpoint"),
            ep_out: alloc
                .alloc(
                    Some(EndpointAddress::from(out_ep)),
                    EndpointType::Bulk,
                    ids::BULK_MAX_PACKET,
                    0,
                )
                .expect("bulk OUT endpoint"),
        }
    }

    fn write_packet(&self, packet: &[u8]) -> usb_device::Result<usize> {
        self.ep_in.write(packet)
    }

    fn read_packet(&self, buf: &mut [u8]) -> usb_device::Result<usize> {
        self.ep_out.read(buf)
    }
}

impl<B: UsbBus> UsbClass<B> for WinUsbClass<'_, B> {
    fn get_configuration_descriptors(&self, writer: &mut DescriptorWriter) -> usb_device::Result<()> {
        dbg_bump(0);
        // Class 0xFF (vendor-specific): Windows never auto-claims it as CDC, and it is the one
        // interface class WebUSB is allowed to claim.
        writer.interface(self.iface, 0xFF, 0x00, 0x00)?;
        writer.endpoint(&self.ep_in)?;
        writer.endpoint(&self.ep_out)?;
        Ok(())
    }

    fn get_bos_descriptors(&self, writer: &mut BosWriter) -> usb_device::Result<()> {
        dbg_bump(1);
        writer.capability(capability_type::PLATFORM, &ids::MS_OS_20_PLATFORM_CAPABILITY)?;
        writer.capability(capability_type::PLATFORM, &ids::WEBUSB_PLATFORM_CAPABILITY)?;
        Ok(())
    }

    fn reset(&mut self) {
        dbg_bump(6);
    }

    fn control_out(&mut self, xfer: usb_device::class::ControlOut<B>) {
        // Observation only (standard OUT requests -- SET_ADDRESS, SET_CONFIGURATION -- are
        // usb-device's to handle); the telemetry shows how far a host's enumeration reached.
        let req = *xfer.request();
        dbg_bump(3);
        dbg_set(
            7,
            (u32::from(req.request) << 24)
                | ((req.request_type as u32) << 16)
                | u32::from(req.value),
        );
    }

    fn control_in(&mut self, xfer: ControlIn<B>) {
        let req = *xfer.request();
        dbg_bump(2);
        dbg_set(
            5,
            (u32::from(req.request) << 24)
                | ((req.request_type as u32) << 16)
                | u32::from(req.value),
        );
        // GET MS OS 2.0 descriptor set: Vendor|Device, bRequest = bMS_VendorCode, wIndex = 7.
        if req.request_type == RequestType::Vendor
            && req.recipient == Recipient::Device
            && req.request == ids::MS_VENDOR_CODE
            && req.index == ids::MS_OS_20_DESCRIPTOR_INDEX
        {
            if xfer.accept_with_static(&ids::MS_OS_20_DESCRIPTOR_SET).is_ok() {
                dbg_bump(4);
            }
        }
    }
}

/// Device release (bcdDevice). Windows caches its Microsoft-OS-descriptor verdict per
/// (VID, PID, bcdDevice) under `usbflags` in the registry -- a failed early query is replayed
/// forever for that triple. After any descriptor change, bump this so the host re-queries
/// instead of trusting a stale verdict. Shared by every board serving this identity: the cached
/// verdict for the triple is "fetch the MS OS 2.0 set", which each board answers itself.
const DEVICE_RELEASE_BCD: u16 = 0x0013;

/// Bring-up telemetry, read live over the debug port's MEM-AP with no core halt: counts of the
/// descriptor callbacks + the last control requests seen, so a failed enumeration shows which
/// exchange never happened. Slots: [0] config-descriptor reads, [1] BOS reads, [2] any
/// control_in, [3] any control_out, [4] MS OS 2.0 hits accepted, [5] last IN request as
/// request<<24 | reqtype<<16 | value, [6] bus resets, [7] last OUT request in the same packing.
#[unsafe(no_mangle)]
static mut USB_DBG: [u32; 8] = [0; 8];

fn dbg_bump(slot: usize) {
    unsafe {
        let p = core::ptr::addr_of_mut!(USB_DBG).cast::<u32>().add(slot);
        core::ptr::write_volatile(p, core::ptr::read_volatile(p).wrapping_add(1));
    }
}

fn dbg_set(slot: usize, value: u32) {
    unsafe {
        let p = core::ptr::addr_of_mut!(USB_DBG).cast::<u32>().add(slot);
        core::ptr::write_volatile(p, value);
    }
}

/// Build the class + device over an allocator from the chip module's `usb_bus`. `build` enables
/// the core and connects to the bus, so enumeration starts here -- serviced by [`UsbCarrier`]
/// polls. bcdUSB is 0x0210 (usb-device's default `UsbRev::Usb210`), which is what makes Windows
/// fetch the BOS. `product` and `serial` are the board's identity: the shared VID/PID names the
/// Lamella Link function, the product string names the board, and the serial (unique per unit --
/// derive it from a chip id where the part has one) is what lets a host picker address one of
/// several attached boards.
pub fn build_device<'a, B: UsbBus>(
    alloc: &'a UsbBusAllocator<B>,
    product: &'a str,
    serial: &'a str,
) -> (WinUsbClass<'a, B>, UsbDevice<'a, B>) {
    build_device_with_endpoints(alloc, product, serial, ids::BULK_IN_EP, ids::BULK_OUT_EP)
}

/// [`build_device`] with explicit bulk endpoint addresses (see
/// [`WinUsbClass::with_endpoints`]) for controllers with unidirectional endpoint numbers.
pub fn build_device_with_endpoints<'a, B: UsbBus>(
    alloc: &'a UsbBusAllocator<B>,
    product: &'a str,
    serial: &'a str,
    in_ep: u8,
    out_ep: u8,
) -> (WinUsbClass<'a, B>, UsbDevice<'a, B>) {
    let class = WinUsbClass::with_endpoints(alloc, in_ep, out_ep);
    let dev = UsbDeviceBuilder::new(alloc, UsbVidPid(ids::VID, ids::PID))
        .strings(&[StringDescriptors::default()
            .manufacturer("Lamella")
            .product(product)
            .serial_number(serial)])
        .expect("one language")
        .device_class(0xFF)
        .device_release(DEVICE_RELEASE_BCD)
        .max_packet_size_0(64)
        .expect("64 is a valid EP0 size")
        .build();
    (class, dev)
}

// ---- the frame-level carriers ----------------------------------------------------------------

/// The frame carrier over the WinUSB bulk pipes -- the USB sibling of a firmware's UART
/// transport. Borrows the boot-built device state (which must persist across requests:
/// enumeration survives), owns only the per-request frame reader.
pub struct UsbCarrier<'d, 'b, B: UsbBus> {
    dev: &'d mut UsbDevice<'b, B>,
    class: &'d mut WinUsbClass<'b, B>,
    reader: FrameReader,
    /// Run once, the first time this carrier observes the device reach `Configured`.
    ///
    /// Some controllers need telling: the SAM4S UDP wants `GLB_STAT.CONFG` set by software once
    /// enumeration completes, and nothing else on the board knows when that happened.
    ///
    /// It lives here because the carrier is the only thing that can still see the device. Polling
    /// `dev.state()` from a main loop stops being possible the moment the carrier is handed to
    /// something that owns it for the board's whole life -- and it answers a question the carrier
    /// already knows.
    on_configured: Option<fn()>,
    configured: bool,
}

impl<'d, 'b, B: UsbBus> UsbCarrier<'d, 'b, B> {
    pub fn new(dev: &'d mut UsbDevice<'b, B>, class: &'d mut WinUsbClass<'b, B>) -> Self {
        Self { dev, class, reader: FrameReader::new(), on_configured: None, configured: false }
    }

    /// The same carrier, with something to run the first time the device reaches `Configured`.
    #[must_use]
    pub fn on_configured(mut self, hook: fn()) -> Self {
        self.on_configured = Some(hook);
        self
    }

    /// Whether the device has reached `Configured` since this carrier was built.
    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.configured
    }

    /// Service the device (enumeration + endpoint events) and drain every pending bulk OUT
    /// packet into the frame reader.
    fn pump(&mut self) {
        while self.dev.poll(&mut [self.class]) {
            let mut packet = [0u8; ids::BULK_MAX_PACKET as usize];
            match self.class.read_packet(&mut packet) {
                Ok(n) => self.reader.push(&packet[..n]),
                Err(_) => break,
            }
        }
        if !self.configured && self.dev.state() == UsbDeviceState::Configured {
            self.configured = true;
            if let Some(hook) = self.on_configured {
                hook();
            }
        }
    }

    /// One bulk IN packet, pumping the device while a previous packet drains. Bounded, so a host
    /// that vanished mid-reply cannot hang the firmware's main loop forever.
    fn write_packet_blocking(&mut self, packet: &[u8]) -> Result<(), TransportError> {
        let mut patience: u32 = 3_000_000;
        loop {
            match self.class.write_packet(packet) {
                Ok(_) => return Ok(()),
                Err(UsbError::WouldBlock) => {
                    self.dev.poll(&mut [self.class]);
                    patience -= 1;
                    if patience == 0 {
                        return Err(TransportError::Carrier);
                    }
                }
                Err(_) => return Err(TransportError::Carrier),
            }
        }
    }
}

impl<B: UsbBus> Transport for UsbCarrier<'_, '_, B> {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        if self.dev.state() != UsbDeviceState::Configured {
            return Err(TransportError::Closed); // no host on this carrier
        }
        let frame = encode_frame(msg_type, seq, payload).ok_or(TransportError::PayloadTooLarge)?;
        for packet in frame.chunks(ids::BULK_MAX_PACKET as usize) {
            self.write_packet_blocking(packet)?;
        }
        if frame.len() % ids::BULK_MAX_PACKET as usize == 0 {
            // A ZLP terminates an exact-multiple transfer for a host that posts reads larger
            // than one packet (WebUSB transferIn). The frame codec itself never relies on
            // transfer boundaries -- this only unblocks such a read promptly.
            self.write_packet_blocking(&[])?;
        }
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
        self.pump();
        Ok(self.reader.next_frame())
    }

    /// The first packet is the probe. If the endpoint will not take it, nothing has been written
    /// and the caller is told so; if it does take it, the host is draining and the remainder is
    /// written the ordinary way rather than risking a frame that stops half way.
    ///
    /// That split is the whole design. Attempting every packet non-blockingly could leave a
    /// truncated frame on the wire when the host stalls midway, and the far side reassembles by
    /// length -- so it would pay a resynchronization for a frame this call had already decided not
    /// to finish. One packet is enough to answer the only question being asked: is anybody reading?
    ///
    /// This is the carrier the seam's `try_send` exists for. Its transmit path is flow-controlled
    /// by the host: a device that is still `Configured` with nobody draining its bulk IN endpoint
    /// accepts nothing, and a blocking write spends its whole patience discovering that.
    fn try_send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<bool, TransportError> {
        if self.dev.state() != UsbDeviceState::Configured {
            return Err(TransportError::Closed); // no host on this carrier
        }
        let frame = encode_frame(msg_type, seq, payload).ok_or(TransportError::PayloadTooLarge)?;
        let mut packets = frame.chunks(ids::BULK_MAX_PACKET as usize);
        let Some(first) = packets.next() else {
            return Ok(true);
        };
        match self.class.write_packet(first) {
            Ok(_) => {}
            Err(UsbError::WouldBlock) => return Ok(false),
            Err(_) => return Err(TransportError::Carrier),
        }
        for packet in packets {
            self.write_packet_blocking(packet)?;
        }
        if frame.len() % ids::BULK_MAX_PACKET as usize == 0 {
            self.write_packet_blocking(&[])?;
        }
        Ok(true)
    }
}

