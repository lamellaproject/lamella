//! Linux CMSIS-DAP v2 (USB bulk) backend, against usbfs: sysfs (`/sys/bus/usb/devices`) for
//! discovery, `/dev/bus/usb/BBB/DDD` for I/O via the `USBDEVFS_*` ioctls. libc only -- no external
//! USB crate. The v2 sibling of lamella-usbhid's hidraw backend.

use crate::{DeviceInfo, Error, Result};
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Duration;

const USBDEVFS_CLAIMINTERFACE: libc::c_ulong = 0x8004_550f;
const USBDEVFS_RELEASEINTERFACE: libc::c_ulong = 0x8004_5510;
const USBDEVFS_BULK: libc::c_ulong = 0xc018_5502;

#[repr(C)]
struct UsbdevfsBulktransfer {
    ep: libc::c_uint,
    len: libc::c_uint,
    timeout: libc::c_uint,
    data: *mut libc::c_void,
}

/// A discovered v2 probe: its usbfs node, ids, and the vendor interface's number + bulk endpoints.
struct Found {
    node: String,
    vid: u16,
    pid: u16,
    interface: u8,
    ep_in: u8,
    ep_out: u8,
}

fn read_hex16(path: &Path) -> Option<u16> {
    u16::from_str_radix(fs::read_to_string(path).ok()?.trim(), 16).ok()
}
fn read_hex8(path: &Path) -> Option<u8> {
    u8::from_str_radix(fs::read_to_string(path).ok()?.trim(), 16).ok()
}
fn read_dec(path: &Path) -> Option<u8> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// Find this interface's bulk IN + OUT endpoint addresses from its `ep_XX` sysfs subdirectories.
fn bulk_endpoints(iface_dir: &Path) -> (u8, u8) {
    let (mut ep_in, mut ep_out) = (0u8, 0u8);
    if let Ok(eps) = fs::read_dir(iface_dir) {
        for ep in eps.flatten() {
            if !ep.file_name().to_string_lossy().starts_with("ep_") {
                continue;
            }
            let addr = read_hex8(&ep.path().join("bEndpointAddress")).unwrap_or(0);
            let attr = read_hex8(&ep.path().join("bmAttributes")).unwrap_or(0);
            if attr & 0x03 == 0x02 {
                if addr & 0x80 != 0 {
                    ep_in = addr;
                } else {
                    ep_out = addr;
                }
            }
        }
    }
    (ep_in, ep_out)
}

/// Scan for USB devices exposing a vendor (class 0xFF) interface with bulk IN + OUT -- the v2 shape.
fn scan() -> Vec<Found> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir("/sys/bus/usb/devices") else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.contains(':') || name.starts_with("usb") {
            continue;
        }
        let dir = entry.path();
        let (Some(vid), Some(pid)) = (
            read_hex16(&dir.join("idVendor")),
            read_hex16(&dir.join("idProduct")),
        ) else {
            continue;
        };
        let (Some(busnum), Some(devnum)) =
            (read_dec(&dir.join("busnum")), read_dec(&dir.join("devnum")))
        else {
            continue;
        };
        let Ok(ifaces) = fs::read_dir(&dir) else {
            continue;
        };
        for iface in ifaces.flatten() {
            let iname = iface.file_name().to_string_lossy().into_owned();
            if !iname.starts_with(&format!("{name}:")) {
                continue;
            }
            if read_hex8(&iface.path().join("bInterfaceClass")) != Some(0xFF) {
                continue;
            }
            let (ep_in, ep_out) = bulk_endpoints(&iface.path());
            if ep_in != 0 && ep_out != 0 {
                out.push(Found {
                    node: format!("/dev/bus/usb/{busnum:03}/{devnum:03}"),
                    vid,
                    pid,
                    interface: read_hex8(&iface.path().join("bInterfaceNumber")).unwrap_or(0),
                    ep_in,
                    ep_out,
                });
                break;
            }
        }
    }
    out
}

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    Ok(scan()
        .into_iter()
        .map(|f| DeviceInfo {
            vendor_id: f.vid,
            product_id: f.pid,
            serial_number: None,
            product: None,
        })
        .collect())
}

pub struct Device {
    file: fs::File,
    interface: u8,
    ep_in: u8,
    ep_out: u8,
}

impl Device {
    pub fn open(vendor_id: u16, product_id: u16, _serial: Option<&str>) -> Result<Self> {
        let f = scan()
            .into_iter()
            .find(|f| f.vid == vendor_id && f.pid == product_id)
            .ok_or(Error::NotFound)?;
        let file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&f.node)
            .map_err(|e| Error::Os(format!("open {}: {e}", f.node)))?;
        let iface = f.interface as libc::c_uint;
        let rc = unsafe { libc::ioctl(file.as_raw_fd(), USBDEVFS_CLAIMINTERFACE, &iface) };
        if rc < 0 {
            return Err(Error::Os(format!(
                "USBDEVFS_CLAIMINTERFACE: {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(Device {
            file,
            interface: f.interface,
            ep_in: f.ep_in,
            ep_out: f.ep_out,
        })
    }

    pub fn write_packet(&mut self, data: &[u8]) -> Result<()> {
        let mut bt = UsbdevfsBulktransfer {
            ep: u32::from(self.ep_out),
            len: data.len() as libc::c_uint,
            timeout: 1000,
            data: data.as_ptr() as *mut libc::c_void,
        };
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), USBDEVFS_BULK, &mut bt) };
        if rc < 0 {
            return Err(Error::Os(format!(
                "USBDEVFS_BULK (write): {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(())
    }

    pub fn read_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        let mut bt = UsbdevfsBulktransfer {
            ep: u32::from(self.ep_in),
            len: buf.len() as libc::c_uint,
            timeout: timeout.as_millis().min(u128::from(u32::MAX)) as libc::c_uint,
            data: buf.as_mut_ptr() as *mut libc::c_void,
        };
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), USBDEVFS_BULK, &mut bt) };
        if rc < 0 {
            return Err(Error::Os(format!(
                "USBDEVFS_BULK (read): {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(rc as usize)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        let iface = self.interface as libc::c_uint;
        unsafe {
            libc::ioctl(self.file.as_raw_fd(), USBDEVFS_RELEASEINTERFACE, &iface);
        }
    }
}
