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

/// Whether one candidate device is eligible for an open -- the single decision all three backends
/// make, so that they make it the same way.
///
/// `reported` NONE with a serial requested is a NO: **a device that does not say who it is cannot be
/// the device you named, and a missing string is not a wildcard.** `wanted` NONE is a yes for
/// everything; choosing among several unnamed candidates belongs to the layer that knows what the
/// caller asked for, which is where an ambiguous unnamed open is refused.
///
pub(crate) fn candidate_satisfies(wanted: Option<&str>, reported: Option<&str>) -> bool {
    match wanted {
        None => true,
        Some(wanted) => reported
            .is_some_and(|actual| actual.eq_ignore_ascii_case(wanted)),
    }
}

/// A HID device discovered by [`enumerate`].
///
/// A physical probe can expose SEVERAL HID interfaces (a composite device -- e.g. the NXP MCU-Link
/// presents a CMSIS-DAP interface alongside a trace interface and a vendor bridge), and all of them
/// share one vid/pid/serial. [`id`](Self::id) is the per-interface handle that tells them apart and
/// re-opens a specific one with [`Device::open_id`]; [`usage_page`](Self::usage_page) /
/// [`usage`](Self::usage) and the report lengths are the HID signature a caller uses to pick the DAP
/// interface out of the set (CMSIS-DAP v1 is a vendor-defined usage page with 64-byte reports).
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
    /// An opaque, backend-specific handle that re-identifies THIS interface for [`Device::open_id`].
    /// It is the discriminator a composite probe needs, since every interface of one physical probe
    /// reports the same vid/pid/serial. Backend-specific and not to be parsed (Windows: the
    /// device-interface path; Linux: the hidraw node name; macOS: a location-derived key).
    pub id: String,
    /// The top-level HID usage page, when the backend reads the report descriptor. CMSIS-DAP v1 uses
    /// a vendor-defined page (`0xFF00`), which separates the DAP interface from a probe's other HID
    /// interfaces. `None` where a backend does not yet report it (currently Linux).
    pub usage_page: Option<u16>,
    /// The top-level HID usage within [`usage_page`](Self::usage_page) (CMSIS-DAP v1: `0x01`).
    pub usage: Option<u16>,
    /// The OS-reported input report byte length, when known -- a CMSIS-DAP v1 interface carries
    /// 64-byte reports. On Windows this INCLUDES the leading report-id byte (so 65 for a 64-byte
    /// report); macOS reports the payload size. Treat "64 or 65" as the 64-byte class.
    pub input_report_len: Option<u16>,
    /// The OS-reported output report byte length; see [`input_report_len`](Self::input_report_len).
    pub output_report_len: Option<u16>,
}

/// Lists the connected HID devices.
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    imp::enumerate()
}

/// An open HID device that exchanges fixed-size reports with a probe.
pub struct Device(imp::Device);

impl Device {
    /// Opens the first connected device with `vendor_id` and `product_id`, optionally requiring a
    /// specific `serial` number. On a composite probe (several HID interfaces on one vid/pid/serial)
    /// this reaches whichever interface enumerates first, which need not be the DAP one -- use
    /// [`enumerate`] + [`open_id`](Self::open_id) to select a specific interface. Queued input reports
    /// are drained before the device is handed out (see [`drained`](Self::drained)).
    pub fn open(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        let device = imp::Device::open(vendor_id, product_id, serial).map(Device)?;
        Ok(Self::drained(device))
    }

    /// Opens the exact interface named by an enumerated [`DeviceInfo::id`] -- the way to reach ONE
    /// interface of a composite probe (e.g. a CMSIS-DAP interface sitting beside a trace or
    /// vendor-bridge interface on the same vid/pid/serial), which [`open`](Self::open) cannot single
    /// out. Like [`open`](Self::open), any stale queued input reports are drained first.
    pub fn open_id(id: &str) -> Result<Self> {
        let device = imp::Device::open_id(id).map(Device)?;
        Ok(Self::drained(device))
    }

    /// Drains any input reports already queued before the device is handed out: a probe protocol is
    /// strict request/reply, so a report waiting at open can only be a stale reply a crashed session
    /// never read -- left in place it shifts every later exchange off by one (each command reads the
    /// PREVIOUS command's reply; observed on an EDBG with a six-figure backlog after an aborted flash
    /// write).
    fn drained(mut device: Device) -> Device {
        let mut stale = [0u8; 64];
        while device
            .read_report(&mut stale, Duration::from_millis(100))
            .is_ok()
        {}
        device
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
