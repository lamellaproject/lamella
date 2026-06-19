//! Cross-platform USB-HID transport for CMSIS-DAP debug probes.
#![allow(unsafe_code)]

use std::time::Duration;

/// An error enumerating or exchanging reports with a HID device.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// No connected device matched the request.
    NotFound,
    /// The operating system's HID layer failed; carries a description.
    Os(String),
    /// A read returned no report within the timeout.
    Timeout,
    /// This operating system's backend is not implemented yet.
    Unsupported,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotFound => write!(f, "no matching HID device"),
            Error::Os(msg) => write!(f, "HID error: {msg}"),
            Error::Timeout => write!(f, "HID read timed out"),
            Error::Unsupported => write!(f, "HID backend not implemented on this platform"),
        }
    }
}
impl std::error::Error for Error {}

/// A HID operation result.
pub type Result<T> = std::result::Result<T, Error>;

/// A HID device discovered by [`enumerate`].
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

/// Lists the connected HID devices.
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    imp::enumerate()
}

/// An open HID device that exchanges fixed-size reports with a probe.
pub struct Device(imp::Device);

impl Device {
    /// Opens the first connected device with `vendor_id` and `product_id`, optionally
    /// requiring a specific `serial` number.
    pub fn open(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        imp::Device::open(vendor_id, product_id, serial).map(Device)
    }

    /// Sends one output report. The report id (0) is supplied by the backend.
    pub fn write_report(&mut self, data: &[u8]) -> Result<()> {
        self.0.write_report(data)
    }

    /// Reads one input report into `buf`, returning its length, or [`Error::Timeout`].
    pub fn read_report(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.0.read_report(buf, timeout)
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
compile_error!("lamella-usbhid supports macOS, Linux, and Windows");
