//! Cross-platform USB *bulk* transport for CMSIS-DAP v2 debug probes -- the v2 sibling of
//! `lamella-usbhid`. Same small shape (enumerate, open by vendor/product id, exchange packets),
//! but over a vendor-specific interface's bulk IN/OUT pipes instead of HID reports, so there is no
//! report id or padding. Implemented directly against each OS's native USB API (WinUSB on Windows,
//! IOKit IOUSBLib on macOS) with no external crates. Linux (usbfs) is a stub for now; HID-on-Linux
//! is covered by `lamella-usbhid` (hidraw).
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

/// Lists the connected CMSIS-DAP v2 (bulk) devices.
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    imp::enumerate()
}

/// An open bulk USB device that exchanges raw packets with a CMSIS-DAP v2 probe.
pub struct Device(imp::Device);

impl Device {
    /// Opens the first connected device with `vendor_id` and `product_id` (optionally a specific
    /// `serial`) that exposes a CMSIS-DAP v2 vendor interface -- a bulk IN + bulk OUT pipe.
    pub fn open(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        imp::Device::open(vendor_id, product_id, serial).map(Device)
    }

    /// Sends one bulk OUT packet (raw -- no report id or padding).
    pub fn write_packet(&mut self, data: &[u8]) -> Result<()> {
        self.0.write_packet(data)
    }

    /// Reads one bulk IN packet into `buf`, returning its length (or [`Error::Timeout`]).
    pub fn read_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.0.read_packet(buf, timeout)
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
