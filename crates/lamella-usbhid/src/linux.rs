//! Linux HID backend, against the kernel hidraw interface: sysfs (`/sys/class/hidraw`) for
//! discovery, `/dev/hidrawN` for report I/O. No external crates -- `std::fs` only.

use crate::{DeviceInfo, Error, Result};
use std::fs;
use std::io::{Read, Write};
use std::time::Duration;

/// CMSIS-DAP report payload size (excludes the report-id byte).
const REPORT_MAX: usize = 64;

/// `(hidraw node name, vendor id, product id, product string)` for each `/sys/class/hidraw` entry.
fn scan() -> Vec<(String, u16, u16, Option<String>)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/class/hidraw") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(uevent) = fs::read_to_string(entry.path().join("device/uevent")) else {
            continue;
        };
        let (mut vid, mut pid, mut product) = (0u16, 0u16, None);
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
            }
        }
        if vid != 0 {
            out.push((name, vid, pid, product));
        }
    }
    out
}

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    Ok(scan()
        .into_iter()
        .map(|(_, vendor_id, product_id, product)| DeviceInfo {
            vendor_id,
            product_id,
            serial_number: None,
            product,
        })
        .collect())
}

pub struct Device {
    file: fs::File,
}

impl Device {
    pub fn open(vendor_id: u16, product_id: u16, _serial: Option<&str>) -> Result<Self> {
        for (name, vid, pid, _) in scan() {
            if vid == vendor_id && pid == product_id {
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

    pub fn write_report(&mut self, data: &[u8]) -> Result<()> {
        let mut report = vec![0u8; 1 + REPORT_MAX];
        let n = data.len().min(REPORT_MAX);
        report[1..1 + n].copy_from_slice(&data[..n]);
        self.file
            .write_all(&report)
            .map_err(|e| Error::Os(format!("hidraw write: {e}")))
    }

    pub fn read_report(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
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
