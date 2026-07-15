//! Cross-platform USB *bulk* transport for CMSIS-DAP v2 debug probes -- the v2 sibling of
//! `lamella-usbhid`. Same small shape (enumerate, open by vendor/product id, exchange packets),
//! but over a vendor-specific interface's bulk IN/OUT pipes instead of HID reports, so there is no
//! report id or padding. Implemented directly against each OS's native USB API -- WinUSB + SetupAPI on
//! Windows, IOKit IOUSBLib on macOS, sysfs + usbfs on Linux -- with no external USB crates. Enumeration,
//! open-by-VID/PID, serial/product strings, and bulk I/O are supported on all three.
#![allow(unsafe_code)]

use std::time::Duration;

/// An error enumerating, opening, or exchanging packets with a bulk USB device.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// No connected device matched the request.
    NotFound,
    /// The operating system's USB layer failed; carries a description.
    Os(String),
    /// A read returned no packet within the timeout.
    Timeout,
    /// This operating system's backend is not implemented yet.
    Unsupported,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "no matching USB bulk device"),
            Error::Os(msg) => write!(f, "USB error: {msg}"),
            Error::Timeout => write!(f, "USB bulk read timed out"),
            Error::Unsupported => write!(f, "USB bulk backend not implemented on this platform"),
        }
    }
}
impl std::error::Error for Error {}

/// A USB bulk operation result.
pub type Result<T> = std::result::Result<T, Error>;

/// A bulk USB device discovered by [`enumerate`].
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// Serial number string, if the OS reported one.
    pub serial_number: Option<String>,
    /// Product string, if the OS reported one.
    pub product: Option<String>,
}

/// Lists every connected vendor-bulk device -- a CMSIS-DAP v2 probe OR e.g. a Lamella Link board (both
/// expose a vendor-specific class-0xFF interface with bulk IN/OUT) -- with its ids and, where the OS reports
/// them, serial/product strings. No VID filter: a caller keeps the vendor id(s) it wants (a probe consumer
/// filters to probe vendors; the Lamella Link picker keeps its own VID). Cross-platform (Windows/macOS/Linux).
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    imp::enumerate()
}

/// Lists the devices registered under a caller-supplied WinUSB device-interface GUID (a
/// `"{...}"` string) -- e.g. every attached Lamella Link board -- with product and serial
/// strings where the OS can report them. Windows-only for now ([`Error::Unsupported`]
/// elsewhere): macOS and Linux have no interface-GUID registry, and their backends match by
/// VID/PID at open time instead.
pub fn enumerate_interface(interface_guid: &str) -> Result<Vec<DeviceInfo>> {
    imp::enumerate_guid(interface_guid)
}

/// An open bulk USB device that exchanges raw packets with a CMSIS-DAP v2 probe.
pub struct Device(imp::Device);

impl Device {
    /// Opens the first connected device with `vendor_id` and `product_id` (optionally a specific
    /// `serial`) that exposes a CMSIS-DAP v2 vendor interface -- a bulk IN + bulk OUT pipe.
    pub fn open(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        imp::Device::open(vendor_id, product_id, serial).map(Device)
    }

    /// Opens a WinUSB device that registered under a caller-supplied device-interface GUID (a
    /// `"{...}"` string) -- e.g. the Lamella Link carrier rather than a CMSIS-DAP v2 probe. On
    /// Windows the device is found by that interface GUID; macOS and Linux match by VID/PID + the
    /// vendor-specific (class 0xFF) interface, so there the GUID is ignored.
    pub fn open_interface(
        interface_guid: &str,
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
    ) -> Result<Self> {
        imp::Device::open_guid(interface_guid, vendor_id, product_id, serial).map(Device)
    }

    /// Sends one bulk OUT packet (raw -- no report id or padding) on the primary (lowest-address) OUT
    /// endpoint.
    pub fn write_packet(&mut self, data: &[u8]) -> Result<()> {
        self.0.write_packet(data)
    }

    /// Reads one bulk IN packet into `buf` from the primary IN endpoint, returning its length (or
    /// [`Error::Timeout`]).
    pub fn read_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.0.read_packet(buf, timeout)
    }

    /// Sends one bulk OUT packet on a specific endpoint address. A single-pair probe uses only its
    /// primary endpoint ([`write_packet`](Self::write_packet)); a device with more than one bulk pair --
    /// e.g. a WCH-Link, whose command pair is `0x01`/`0x81` and whose flash-stream data pair is
    /// `0x02`/`0x82` -- reaches the others here.
    pub fn write_endpoint(&mut self, endpoint: u8, data: &[u8]) -> Result<()> {
        self.0.write_endpoint(endpoint, data)
    }

    /// Reads one bulk IN packet from a specific endpoint address into `buf`, returning its length (or
    /// [`Error::Timeout`]) -- the companion to [`write_endpoint`](Self::write_endpoint).
    pub fn read_endpoint(&mut self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.0.read_endpoint(endpoint, buf, timeout)
    }
}

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod imp;
#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod imp;
#[cfg(target_os = "windows")]
#[path = "windows.rs"]
mod imp;
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
compile_error!("lamella-usbbulk supports macOS, Linux, and Windows");
