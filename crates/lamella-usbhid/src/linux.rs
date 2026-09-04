//! Linux HID backend, against the kernel hidraw interface: sysfs (`/sys/class/hidraw`) for
//! discovery, `/dev/hidrawN` for report I/O. No external crates -- `std::fs` only.

use crate::{DeviceInfo, Error, Result};
use std::fs;
use std::io::{Read, Write};
use std::time::Duration;

/// CMSIS-DAP report payload size (excludes the report-id byte).
const REPORT_MAX: usize = 64;

/// One `/sys/class/hidraw` entry, as much as sysfs states about it.
struct Entry {
    /// The hidraw node name (e.g. `hidraw0`) -- the reopen key for `open_id`.
    name: String,
    vendor_id: u16,
    product_id: u16,
    /// `HID_UNIQ`: the device's own serial, where it reports one.
    serial: Option<String>,
    product: Option<String>,
}

/// Every `/sys/class/hidraw` entry, read from sysfs. Opens nothing.
fn scan() -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(uevent) = fs::read_to_string(entry.path().join("device/uevent")) else {
            continue;
        };
        let (mut vid, mut pid, mut product, mut serial) = (0u16, 0u16, None, None);
        for line in uevent.lines() {
            if let Some(id) = line.strip_prefix("HID_ID=") {
                let mut parts = id.split(':');
                let _bus = parts.next();
                if let (Some(v), Some(p)) = (parts.next(), parts.next()) {
                    vid = u32::from_str_radix(v, 16).unwrap_or(0) as u16;
                    pid = u32::from_str_radix(p, 16).unwrap_or(0) as u16;
                }
            } else if let Some(n) = line.strip_prefix("HID_NAME=") {
                product = Some(n.to_string());
            } else if let Some(u) = line.strip_prefix("HID_UNIQ=") {
                let u = u.trim();
                if !u.is_empty() {
                    serial = Some(u.to_string());
                }
            }
        }
        if vid != 0 {
            out.push(Entry { name, vendor_id: vid, product_id: pid, serial, product });
        }
    }
    out
}

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    Ok(scan()
        .into_iter()
        .map(|entry| DeviceInfo {
            vendor_id: entry.vendor_id,
            product_id: entry.product_id,
            serial_number: entry.serial,
            product: entry.product,
            id: entry.name,
            usage_page: None,
            usage: None,
            input_report_len: None,
            output_report_len: None,
        })
        .collect())
}

pub struct Device {
    file: fs::File,
}

impl Device {
    /// Opens the HID device with `vendor_id`/`product_id`, and with `serial` when one is named.
    ///
    /// **A NAMED SERIAL IS A FILTER, NOT A LABEL.** Several probes of one model answer to the same
    /// vendor and product id, so taking the first match opens whichever the kernel enumerated
    /// first -- and the caller that named a serial is precisely the caller who cannot tolerate
    /// that. A serial matching nothing attached is [`Error::NotFound`]: refusing names a board an
    /// operator can go and plug in, where opening a different one writes to it.
    pub fn open(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        for entry in scan() {
            if entry.vendor_id == vendor_id
                && entry.product_id == product_id
                && crate::candidate_satisfies(serial, entry.serial.as_deref())
            {
                let name = entry.name;
                let path = format!("/dev/{name}");
                let file = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&path)
                    .map_err(|e| Error::Os(format!("open {path}: {e}")))?;
                return Ok(Device { file });
            }
        }
        Err(Error::NotFound)
    }

    pub fn open_id(id: &str) -> Result<Self> {
        if id.is_empty() || id.contains('/') {
            return Err(Error::NotFound);
        }
        let path = format!("/dev/{id}");
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .map_err(|e| Error::Os(format!("open {path}: {e}")))?;
        Ok(Device { file })
    }

    pub fn write_report(&mut self, data: &[u8]) -> Result<()> {
        let mut report = vec![0u8; 1 + REPORT_MAX];
        let n = data.len().min(REPORT_MAX);
        report[1..1 + n].copy_from_slice(&data[..n]);
        self.file
            .write_all(&report)
            .map_err(|e| Error::Os(format!("hidraw write: {e}")))
    }

    /// Reads one input report, waiting at most `timeout`.
    ///
    /// **THE TIMEOUT IS HONOURED WITH `poll(2)` BECAUSE A READ THAT IGNORES IT NEVER RETURNS.**
    /// hidraw blocks until a report arrives, and the reasoning for letting it -- that a CMSIS-DAP
    /// probe answers a request promptly -- holds only where a request was actually sent.
    pub fn read_report(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        use std::os::fd::AsRawFd;
        let mut pfd = libc::pollfd {
            fd: self.file.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let millis = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        let ready = unsafe { libc::poll(&raw mut pfd, 1, millis) };
        if ready < 0 {
            return Err(Error::Os(format!("hidraw poll: {}", std::io::Error::last_os_error())));
        }
        if ready == 0 {
            return Err(Error::Timeout);
        }
        let mut report = vec![0u8; REPORT_MAX];
        let n = self
            .file
            .read(&mut report)
            .map_err(|e| Error::Os(format!("hidraw read: {e}")))?;
        let m = n.min(buf.len());
        buf[..m].copy_from_slice(&report[..m]);
        Ok(m)
    }
}
