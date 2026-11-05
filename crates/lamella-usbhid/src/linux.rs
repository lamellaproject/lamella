//! Linux HID backend, against the kernel hidraw interface: sysfs (`/sys/class/hidraw`) for
//! discovery, `/dev/hidrawN` for report I/O. No external crates -- `std::fs` only.

use crate::open_error::linux_open_failure as open_failure;
use crate::{DeviceInfo, Error, Result};
use std::fs;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

/// One `/sys/class/hidraw` entry, as much as sysfs states about it.
struct Entry {
    /// The hidraw node name (e.g. `hidraw0`) -- the reopen key for `open_id`.
    name: String,
    vendor_id: u16,
    product_id: u16,
    /// `HID_UNIQ`: the device's own serial, where it reports one.
    serial: Option<String>,
    product: Option<String>,
    /// The USB interface's `interface` attribute, and the USB device's `product` attribute.
    interface_name: Option<String>,
    device_name: Option<String>,
    on_usb: Option<bool>,
}

/// `BUS_USB` in the kernel's `HID_ID`.
const BUS_USB: u32 = 3;

/// Every `/sys/class/hidraw` entry, read from sysfs. Opens nothing.
fn scan() -> Vec<Entry> {
    scan_in(Path::new("/sys/class/hidraw"))
}

/// A sysfs string attribute, or `None` if the kernel did not create it or it could not be read.
fn attribute(path: &Path) -> Option<String> {
    fs::read_to_string(path).ok().map(|text| text.trim().to_owned())
}

/// [`scan`] of the hidraw class folder at `root`.
fn scan_in(root: &Path) -> Vec<Entry> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Ok(uevent) = fs::read_to_string(entry.path().join("device/uevent")) else {
            continue;
        };
        let (mut vid, mut pid, mut product, mut serial, mut bus) = (0u16, 0u16, None, None, None);
        for line in uevent.lines() {
            if let Some(id) = line.strip_prefix("HID_ID=") {
                let mut parts = id.split(':');
                bus = parts.next().and_then(|b| u32::from_str_radix(b, 16).ok());
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
            let on_usb = bus.map(|bus| bus == BUS_USB);
            let (interface_name, device_name) = if on_usb == Some(true) {
                (attribute(&entry.path().join("device/../interface")), attribute(&entry.path().join("device/../../product")))
            } else {
                (None, None)
            };
            out.push(Entry { name, vendor_id: vid, product_id: pid, serial, product, interface_name, device_name, on_usb });
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
            interface_name: entry.interface_name,
            device_name: entry.device_name,
            on_usb: entry.on_usb,
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

/// The device whose hidraw node is `path`, opened to read and write.
fn open_node(path: &str) -> Result<Device> {
    let file = fs::OpenOptions::new().read(true).write(true).open(path).map_err(|error| open_failure(path, &error))?;
    Ok(Device { file })
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
                return open_node(&format!("/dev/{name}"));
            }
        }
        Err(Error::NotFound)
    }

    pub fn open_id(id: &str) -> Result<Self> {
        if id.is_empty() || id.contains('/') {
            return Err(Error::NotFound);
        }
        open_node(&format!("/dev/{id}"))
    }

    pub fn write_report(&mut self, data: &[u8]) -> Result<()> {
        let mut report = vec![0u8; 1 + data.len().max(crate::DEFAULT_REPORT_LEN)];
        report[1..1 + data.len()].copy_from_slice(data);
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
        self.file
            .read(buf)
            .map_err(|e| Error::Os(format!("hidraw read: {e}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hidraw class folder holding one node. In sysfs the node's `device` is a link to the HID device,
    /// under the USB interface, under the USB device, so `device/..` and `device/../..` are where the
    /// two attributes live; plain folders stand in for that here.
    fn sysfs(test: &str, hid_id: &str, interface: Option<&str>, product: Option<&str>) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("lamella-usbhid-{}-{test}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let device = root.join("hidraw0").join("device");
        fs::create_dir_all(&device).expect("a scratch folder");
        fs::write(device.join("uevent"), format!("HID_ID={hid_id}\nHID_NAME=Acme Debug\nHID_UNIQ=A1\n")).expect("uevent");
        if let Some(interface) = interface {
            fs::write(root.join("hidraw0").join("interface"), format!("{interface}\n")).expect("interface");
        }
        if let Some(product) = product {
            fs::write(root.join("product"), format!("{product}\n")).expect("product");
        }
        root
    }

    /// **A USB HID NODE CARRIES THE STRINGS OF THE INTERFACE AND THE DEVICE ABOVE IT, AND ITS BUS.** The
    /// kernel creates an attribute only for a string it holds, so one that is missing reads as not said.
    #[test]
    fn a_usb_node_reads_the_strings_above_it() {
        let root = sysfs("usb", "0003:00001234:00005678", Some("Acme CMSIS-DAP"), Some("Acme Debug"));
        let entries = scan_in(&root);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].on_usb, Some(true));
        assert_eq!(entries[0].product.as_deref(), Some("Acme Debug"));
        assert_eq!(entries[0].interface_name.as_deref(), Some("Acme CMSIS-DAP"));
        assert_eq!(entries[0].device_name.as_deref(), Some("Acme Debug"));

        let root = sysfs("usb-nameless", "0003:00001234:00005678", None, None);
        let entries = scan_in(&root);
        assert_eq!(entries[0].on_usb, Some(true));
        assert_eq!(entries[0].interface_name, None);
        assert_eq!(entries[0].device_name, None);
    }

    /// **A NODE ON ANOTHER BUS IS SAID NOT TO BE ON USB**, and the folders above it are not read as USB's.
    #[test]
    fn a_node_on_another_bus_is_not_on_usb() {
        let root = sysfs("i2c", "0018:00001234:00005678", Some("not an interface"), Some("not a device"));
        let entries = scan_in(&root);
        assert_eq!(entries[0].on_usb, Some(false));
        assert_eq!(entries[0].interface_name, None);
        assert_eq!(entries[0].device_name, None);
    }
}
