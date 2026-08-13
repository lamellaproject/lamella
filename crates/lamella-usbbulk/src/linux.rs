//! Linux CMSIS-DAP v2 (USB bulk) backend, against usbfs: sysfs (`/sys/bus/usb/devices`) for
//! discovery, `/dev/bus/usb/BBB/DDD` for I/O via the `USBDEVFS_*` ioctls. libc only -- no external
//! USB crate. The v2 sibling of lamella-usbhid's hidraw backend.

use crate::{Binding, DeviceInfo, Error, Result};
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

/// A discovered v2 probe: its usbfs node, ids, the device's serial/product strings (when sysfs reports
/// them), and the vendor interface's number + bulk endpoints.
struct Found {
    node: String,
    vid: u16,
    pid: u16,
    serial: Option<String>,
    product: Option<String>,
    interface: u8,
    ep_in: u8,
    ep_out: u8,
}

impl Found {
    /// Whether this device is the one `vendor_id`/`product_id`/`serial` asks for.
    ///
    /// Separate from the scan and from the open so it can be exercised without a bus: a selection
    /// rule reachable only through real hardware is a rule that gets tested on whatever happens to
    /// be plugged in, which is one board on a developer's desk and several on a bench.
    fn selected_by(&self, vendor_id: u16, product_id: u16, serial: Option<&str>) -> bool {
        self.vid == vendor_id
            && self.pid == product_id
            && serial.is_none_or(|wanted| self.serial.as_deref() == Some(wanted))
    }
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
/// A sysfs string attribute (`serial`, `product`), trimmed; `None` if absent or empty.
fn read_string(path: &Path) -> Option<String> {
    let value = fs::read_to_string(path).ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
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
                    if ep_in == 0 || addr < ep_in {
                        ep_in = addr;
                    }
                } else if ep_out == 0 || addr < ep_out {
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
        let serial = read_string(&dir.join("serial"));
        let product = read_string(&dir.join("product"));
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
                    serial: serial.clone(),
                    product: product.clone(),
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
            serial_number: f.serial,
            product: f.product,
        })
        .collect())
}

pub fn enumerate_guid(_interface_guid: &str) -> Result<Vec<DeviceInfo>> {
    Err(Error::Unsupported)
}

pub struct Device {
    file: fs::File,
    interface: u8,
    ep_in: u8,
    ep_out: u8,
}

impl Device {
    /// See [`crate::Device::reset_pipes`]. Not implemented on this platform yet.
    pub fn reset_pipes(&mut self) {}

    pub fn reset_endpoint(&mut self, _endpoint: u8) {}

    /// See [`crate::Device::describe_interface`]. Not implemented on this platform yet.
    pub fn describe_interface(&self) -> String {
        format!("endpoints in {:#04x} out {:#04x}", self.ep_in, self.ep_out)
    }

    /// The bulk endpoint addresses negotiated at open time, as `(in, out)`.
    ///
    /// Exposed because probing endpoints blindly is not a viable diagnostic: reading an endpoint a
    /// device does not have can block rather than fail, so a tool that needs to know which pipes
    /// exist must ask instead of sweep.
    pub fn endpoints(&self) -> (u8, u8) {
        (self.ep_in, self.ep_out)
    }

    /// Opens the device with `vendor_id`/`product_id`, and with `serial` when one is named.
    ///
    /// **A NAMED SERIAL IS A FILTER, NOT A LABEL.** Several boards of one model answer to the same
    /// VID/PID, so taking the first match opens whichever the bus happened to enumerate first --
    /// and the caller that named a serial is precisely the caller who cannot tolerate that. A
    /// serial that matches nothing attached is [`Error::NotFound`]: refusing names a board the
    /// operator can go and plug in, where opening a different one writes to it.
    pub fn open(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        let f = crate::select_requested(
            scan().into_iter().filter(|f| f.vid == vendor_id && f.pid == product_id),
            serial,
            |f| f.serial.as_deref(),
        )?;
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

    /// Open by VID/PID; the interface GUID is a Windows concept (usbfs matches by VID/PID + the
    /// vendor class-0xFF interface), so it is ignored here.
    pub fn open_guid(_interface_guid: &str, vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        Self::open(vendor_id, product_id, serial)
    }

    /// One bulk transfer on `endpoint` (the usbfs ioctl carries the direction in the address's 0x80
    /// bit), returning the byte count transferred. Both the packet and endpoint-addressed I/O route here.
    fn bulk(&self, endpoint: u8, data: *mut libc::c_void, len: usize, timeout_ms: u32) -> Result<usize> {
        let mut bt = UsbdevfsBulktransfer {
            ep: u32::from(endpoint),
            len: len as libc::c_uint,
            timeout: timeout_ms,
            data,
        };
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), USBDEVFS_BULK, &mut bt) };
        if rc < 0 {
            return Err(Error::Os(format!(
                "USBDEVFS_BULK (ep 0x{endpoint:02x}): {}",
                std::io::Error::last_os_error()
            )));
        }
        Ok(rc as usize)
    }

    /// Sends one bulk OUT packet on a specific endpoint address (see [`crate::Device::write_endpoint`]).
    pub fn write_endpoint(&mut self, endpoint: u8, data: &[u8]) -> Result<()> {
        self.bulk(endpoint, data.as_ptr() as *mut libc::c_void, data.len(), 1000)
            .map(|_| ())
    }

    /// Reads one bulk IN packet from a specific endpoint address (see [`crate::Device::read_endpoint`]).
    pub fn read_endpoint(&mut self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        let ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        self.bulk(endpoint, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), ms)
    }

    pub fn write_packet(&mut self, data: &[u8]) -> Result<()> {
        self.write_endpoint(self.ep_out, data)
    }

    pub fn read_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.read_endpoint(self.ep_in, buf, timeout)
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

/// See [`crate::diagnose`]. This platform opens USB devices directly, so there is no "bound
/// driver" state to be in: a device that enumerates is reachable. The analogous local failure is
/// permissions (a missing udev rule), which shows up as an open error rather than here.
pub fn diagnose(_interface_guid: &str, vendor_id: u16, product_id: u16) -> Result<Binding> {
    let present = enumerate()?
        .into_iter()
        .any(|device| device.vendor_id == vendor_id && device.product_id == product_id);
    Ok(if present { Binding::Bound } else { Binding::Absent })
}

#[cfg(test)]
mod tests {
    use super::Found;

    const VID: u16 = 0x39e9;
    const PID: u16 = 0x0001;

    fn board(serial: Option<&str>) -> Found {
        Found {
            node: String::from("/dev/bus/usb/001/002"),
            vid: VID,
            pid: PID,
            serial: serial.map(String::from),
            product: None,
            interface: 0,
            ep_in: 0x81,
            ep_out: 0x01,
        }
    }

    #[test]
    fn an_unnamed_open_takes_any_board_of_the_model() {
        assert!(board(Some("AAAA")).selected_by(VID, PID, None));
        assert!(board(None).selected_by(VID, PID, None));
    }

    #[test]
    fn a_named_open_takes_that_board_and_refuses_its_twin() {
        assert!(board(Some("AAAA")).selected_by(VID, PID, Some("AAAA")));
        assert!(!board(Some("BBBB")).selected_by(VID, PID, Some("AAAA")));
    }

    #[test]
    fn a_named_open_refuses_a_board_that_reports_no_serial() {
        assert!(!board(None).selected_by(VID, PID, Some("AAAA")));
    }

    #[test]
    fn the_ids_still_select_when_a_serial_agrees() {
        assert!(!board(Some("AAAA")).selected_by(VID, PID + 1, Some("AAAA")));
        assert!(!board(Some("AAAA")).selected_by(VID + 1, PID, Some("AAAA")));
    }
}
