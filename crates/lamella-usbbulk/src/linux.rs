//! Linux CMSIS-DAP v2 (USB bulk) backend, against usbfs: sysfs (`/sys/bus/usb/devices`) for
//! discovery, `/dev/bus/usb/BBB/DDD` for I/O via the `USBDEVFS_*` ioctls. libc only -- no external
//! USB crate. The v2 sibling of lamella-usbhid's hidraw backend.

use crate::{Binding, DeviceInfo, Error, InterfaceClass, InterfaceInfo, Result, Setup};
use std::fs;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::Duration;

const USBDEVFS_CLAIMINTERFACE: libc::c_ulong = 0x8004_550f;
const USBDEVFS_RELEASEINTERFACE: libc::c_ulong = 0x8004_5510;
const USBDEVFS_BULK: libc::c_ulong = 0xc018_5502;
const USBDEVFS_CONTROL: libc::c_ulong =
    ioctl_read_write(b'U', 0, std::mem::size_of::<UsbdevfsCtrltransfer>());

/// `_IOWR(type, nr, size)` in the asm-generic encoding of `include/uapi/asm-generic/ioctl.h`: the two
/// direction bits on top, then fourteen bits of size, eight of type and eight of number.
const fn ioctl_read_write(kind: u8, number: u8, size: usize) -> libc::c_ulong {
    const IOC_WRITE: libc::c_ulong = 1;
    const IOC_READ: libc::c_ulong = 2;
    ((IOC_READ | IOC_WRITE) << 30)
        | ((size as libc::c_ulong) << 16)
        | ((kind as libc::c_ulong) << 8)
        | number as libc::c_ulong
}

#[repr(C)]
struct UsbdevfsBulktransfer {
    ep: libc::c_uint,
    len: libc::c_uint,
    timeout: libc::c_uint,
    data: *mut libc::c_void,
}

/// `struct usbdevfs_ctrltransfer`: the eight bytes of a setup stage in host byte order, then the
/// timeout and the buffer the data stage reads from or writes into.
#[repr(C)]
struct UsbdevfsCtrltransfer {
    request_type: u8,
    request: u8,
    value: u16,
    index: u16,
    length: u16,
    timeout: u32,
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
    /// The vendor-class interface's own name, from sysfs `.../interface` -- the `iInterface`
    /// string the kernel read at enumeration. Reading a file, so no device is opened.
    interface_name: Option<String>,
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

/// Where sysfs lists every USB device and interface.
const SYSFS_USB_DEVICES: &str = "/sys/bus/usb/devices";

/// Scan for USB devices exposing a vendor (class 0xFF) interface with bulk IN + OUT -- the v2 shape.
fn scan() -> Vec<Found> {
    scan_where(Path::new(SYSFS_USB_DEVICES), |iface| {
        read_hex8(&iface.join("bInterfaceClass")) == Some(0xFF)
    })
}

/// Whether the interface whose sysfs directory is `iface` is of `class`, by its `bInterfaceClass`,
/// `bInterfaceSubClass` and `bInterfaceProtocol` attributes (`drivers/usb/core/sysfs.c`).
fn has_class(iface: &Path, class: InterfaceClass) -> bool {
    read_hex8(&iface.join("bInterfaceClass")) == Some(class.class)
        && read_hex8(&iface.join("bInterfaceSubClass")) == Some(class.subclass)
        && read_hex8(&iface.join("bInterfaceProtocol")) == Some(class.protocol)
}

/// Every USB device listed under `root` with an interface that `wanted` accepts, given that
/// interface's directory -- one entry per device, for the first such interface.
fn scan_where(root: &Path, wanted: impl Fn(&Path) -> bool) -> Vec<Found> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(root) else {
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
            if !wanted(&iface.path()) {
                continue;
            }
            let (ep_in, ep_out) = bulk_endpoints(&iface.path());
            {
                out.push(Found {
                    node: format!("/dev/bus/usb/{busnum:03}/{devnum:03}"),
                    vid,
                    pid,
                    serial: serial.clone(),
                    product: product.clone(),
                    interface_name: read_string(&iface.path().join("interface")),
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
            interface_name: f.interface_name,
        })
        .collect())
}

pub fn enumerate_guid(_interface_guid: &str) -> Result<Vec<DeviceInfo>> {
    Err(Error::Unsupported)
}

/// See [`crate::enumerate_class`]. Reads sysfs, so nothing is opened.
pub fn enumerate_class(class: InterfaceClass) -> Result<Vec<InterfaceInfo>> {
    Ok(interfaces_of_class(Path::new(SYSFS_USB_DEVICES), class))
}

/// Every device listed under the sysfs directory `root` with an interface of `class`, as
/// [`crate::enumerate_class`] lists them.
fn interfaces_of_class(root: &Path, class: InterfaceClass) -> Vec<InterfaceInfo> {
    scan_where(root, |iface| has_class(iface, class))
        .into_iter()
        .map(|f| InterfaceInfo {
            vendor_id: f.vid,
            product_id: f.pid,
            serial_number: f.serial,
            product: f.product,
            interface_number: f.interface,
            interface_name: f.interface_name,
        })
        .collect()
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
        if f.ep_in == 0 || f.ep_out == 0 {
            return Err(Error::Os(format!(
                "{:04x}:{:04x} has a vendor-class interface but no bulk IN/OUT pair, so this \
                 transport cannot drive it",
                f.vid, f.pid
            )));
        }
        let file = open_claimed(&f.node, f.interface)?;
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
        release(&self.file, self.interface);
    }
}

/// Opens the usbfs node `node` for I/O and claims interface `interface` on it.
fn open_claimed(node: &str, interface: u8) -> Result<fs::File> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(node)
        .map_err(|e| Error::Os(format!("open {node}: {e}")))?;
    let iface = libc::c_uint::from(interface);
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), USBDEVFS_CLAIMINTERFACE, &iface) };
    if rc < 0 {
        return Err(Error::Os(format!(
            "USBDEVFS_CLAIMINTERFACE: {}",
            std::io::Error::last_os_error()
        )));
    }
    Ok(file)
}

/// Releases the claim [`open_claimed`] made on `interface`.
fn release(file: &fs::File, interface: u8) {
    let iface = libc::c_uint::from(interface);
    unsafe {
        libc::ioctl(file.as_raw_fd(), USBDEVFS_RELEASEINTERFACE, &iface);
    }
}

/// The device [`ControlInterface::open`] opens among the devices `found`: the first with `vendor_id`
/// and `product_id` whose serial is `serial`, when one is named.
fn control_candidate(
    found: Vec<Found>,
    vendor_id: u16,
    product_id: u16,
    serial: Option<&str>,
) -> Result<Found> {
    crate::select_by_whole_serial(
        found.into_iter().filter(|f| f.vid == vendor_id && f.pid == product_id),
        serial,
        |f| f.serial.as_deref(),
    )
}

/// An interface claimed on a usbfs node and driven by `USBDEVFS_CONTROL`.
pub struct ControlInterface {
    file: fs::File,
    interface: u8,
}

impl ControlInterface {
    /// See [`crate::ControlInterface::open`].
    pub fn open(
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
        class: InterfaceClass,
    ) -> Result<Self> {
        let f = control_candidate(
            scan_where(Path::new(SYSFS_USB_DEVICES), |iface| has_class(iface, class)),
            vendor_id,
            product_id,
            serial,
        )?;
        let file = open_claimed(&f.node, f.interface)?;
        Ok(ControlInterface { file, interface: f.interface })
    }

    /// See [`crate::ControlInterface::interface_number`].
    pub fn interface_number(&self) -> u8 {
        self.interface
    }

    /// See [`crate::ControlInterface::control_in`].
    pub fn control_in(&mut self, setup: Setup, buffer: &mut [u8], timeout_ms: u32) -> Result<usize> {
        self.control(setup, buffer.as_mut_ptr().cast(), timeout_ms)
    }

    /// See [`crate::ControlInterface::control_out`].
    pub fn control_out(&mut self, setup: Setup, data: &[u8], timeout_ms: u32) -> Result<usize> {
        self.control(setup, data.as_ptr().cast_mut().cast(), timeout_ms)
    }

    /// One `USBDEVFS_CONTROL`, which answers the number of bytes transferred (`do_proc_control`).
    fn control(&self, setup: Setup, data: *mut libc::c_void, timeout_ms: u32) -> Result<usize> {
        let mut transfer = UsbdevfsCtrltransfer {
            request_type: setup.request_type,
            request: setup.request,
            value: setup.value,
            index: setup.index,
            length: setup.length,
            timeout: timeout_ms,
            data,
        };
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), USBDEVFS_CONTROL, &mut transfer) };
        if rc >= 0 {
            return Ok(rc as usize);
        }
        let error = std::io::Error::last_os_error();
        Err(match error.raw_os_error() {
            Some(libc::ETIMEDOUT) => Error::Timeout,
            Some(libc::EPIPE) => crate::stalled(setup),
            _ => Error::Os(format!("USBDEVFS_CONTROL: {error}")),
        })
    }
}

impl Drop for ControlInterface {
    fn drop(&mut self) {
        release(&self.file, self.interface);
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
    use crate::InterfaceClass;
    use std::path::{Path, PathBuf};

    /// A sysfs tree in a fresh directory, each attribute written as `drivers/usb/core/sysfs.c`
    /// formats it.
    struct FakeSysfs {
        root: PathBuf,
    }

    impl FakeSysfs {
        fn new(tag: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("lamella-usbbulk-sysfs-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            FakeSysfs { root }
        }

        fn write(dir: &Path, name: &str, value: &str) {
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(dir.join(name), value).unwrap();
        }

        /// A device directory with its ids, its bus position and its serial.
        fn device(&self, name: &str, vid: u16, pid: u16, serial: &str) -> PathBuf {
            let dir = self.root.join(name);
            Self::write(&dir, "idVendor", &format!("{vid:04x}\n"));
            Self::write(&dir, "idProduct", &format!("{pid:04x}\n"));
            Self::write(&dir, "busnum", "1\n");
            Self::write(&dir, "devnum", "7\n");
            Self::write(&dir, "serial", &format!("{serial}\n"));
            dir
        }

        /// An interface directory inside `device`: its number, its class, subclass and protocol, and
        /// its name.
        fn interface(device: &Path, number: u8, class: (u8, u8, u8), name: &str) {
            let device_name = device.file_name().unwrap().to_string_lossy().into_owned();
            let dir = device.join(format!("{device_name}:1.{number}"));
            Self::write(&dir, "bInterfaceNumber", &format!("{number:02x}\n"));
            Self::write(&dir, "bInterfaceClass", &format!("{:02x}\n", class.0));
            Self::write(&dir, "bInterfaceSubClass", &format!("{:02x}\n", class.1));
            Self::write(&dir, "bInterfaceProtocol", &format!("{:02x}\n", class.2));
            Self::write(&dir, "interface", &format!("{name}\n"));
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// DFU mode's class, subclass and protocol (DFU 1.1, Table 4.4).
    const DFU_MODE: InterfaceClass = InterfaceClass { class: 0xFE, subclass: 0x01, protocol: 0x02 };

    #[test]
    /// A DFU interface is listed with its device's ids and serial and its own number and name, and a
    /// device whose only interface is of another class is not listed.
    fn a_dfu_interface_is_listed_by_its_class_and_a_probe_beside_it_is_not() {
        let sysfs = FakeSysfs::new("class");
        let bootloader = sysfs.device("1-2", 0x0483, 0xdf11, "AAAA1111");
        FakeSysfs::interface(&bootloader, 0, (0xFE, 0x01, 0x02), "@Internal Flash");
        let probe = sysfs.device("1-3", 0x2e8a, 0x000c, "BBBB2222");
        FakeSysfs::interface(&probe, 0, (0xFF, 0x00, 0x00), "CMSIS-DAP v2 Interface");

        let listed = super::interfaces_of_class(&sysfs.root, DFU_MODE);
        assert_eq!(listed.len(), 1, "{listed:?}");
        let dfu = &listed[0];
        assert_eq!((dfu.vendor_id, dfu.product_id), (0x0483, 0xdf11));
        assert_eq!(dfu.serial_number.as_deref(), Some("AAAA1111"));
        assert_eq!(dfu.interface_number, 0);
        assert_eq!(dfu.interface_name.as_deref(), Some("@Internal Flash"));
    }

    #[test]
    /// Class, subclass and protocol must all agree: an interface in DFU's run-time protocol (DFU 1.1,
    /// section 4.1) is not one in DFU mode.
    fn every_part_of_the_class_must_agree() {
        let sysfs = FakeSysfs::new("protocol");
        let application = sysfs.device("1-4", 0x0483, 0x5740, "CCCC3333");
        FakeSysfs::interface(&application, 2, (0xFE, 0x01, 0x01), "DFU run-time");
        assert!(super::interfaces_of_class(&sysfs.root, DFU_MODE).is_empty());
        let run_time = InterfaceClass { protocol: 0x01, ..DFU_MODE };
        assert_eq!(super::interfaces_of_class(&sysfs.root, run_time)[0].interface_number, 2);
    }

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn the_control_request_number_is_encoded_as_the_bulk_one_is() {
        use std::mem::size_of;
        assert_eq!(
            super::ioctl_read_write(b'U', 2, size_of::<super::UsbdevfsBulktransfer>()),
            super::USBDEVFS_BULK
        );
        assert_eq!(size_of::<super::UsbdevfsCtrltransfer>(), 24);
        assert_eq!(super::USBDEVFS_CONTROL, 0xc018_5500);
    }

    #[test]
    /// The control structure is laid out as `include/uapi/linux/usbdevice_fs.h` declares it: the
    /// eight bytes of the setup stage, the timeout, then the data pointer at its own alignment.
    fn the_control_structure_puts_the_setup_stage_first() {
        use std::mem::{align_of, offset_of};
        type Transfer = super::UsbdevfsCtrltransfer;
        assert_eq!(offset_of!(Transfer, request_type), 0);
        assert_eq!(offset_of!(Transfer, request), 1);
        assert_eq!(offset_of!(Transfer, value), 2);
        assert_eq!(offset_of!(Transfer, index), 4);
        assert_eq!(offset_of!(Transfer, length), 6);
        assert_eq!(offset_of!(Transfer, timeout), 8);
        assert_eq!(offset_of!(Transfer, data), 12usize.next_multiple_of(align_of::<*mut libc::c_void>()));
    }

    const VID: u16 = 0x39e9;
    const PID: u16 = 0x0001;

    fn board(serial: Option<&str>) -> Found {
        Found {
            node: String::from("/dev/bus/usb/001/002"),
            vid: VID,
            pid: PID,
            serial: serial.map(String::from),
            product: None,
            interface_name: None,
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

    #[test]
    /// Two attached bootloaders of one model whose serials nest, the longer one listed first: the
    /// interface opened for a serial is the device that reports that serial whole, never one whose
    /// serial only contains it.
    fn of_two_boards_whose_serials_nest_the_one_named_whole_is_opened() {
        let listed = || vec![board(Some("ABC1")), board(Some("ABC"))];
        let opened = |serial| super::control_candidate(listed(), VID, PID, Some(serial)).map(|f| f.serial);
        assert_eq!(opened("ABC").unwrap().as_deref(), Some("ABC"), "the one named, not the one listed first");
        assert_eq!(opened("abc").unwrap().as_deref(), Some("ABC"), "without regard to case");
        assert_eq!(opened("ABC1").unwrap().as_deref(), Some("ABC1"));
        assert!(matches!(opened("BC"), Err(crate::Error::NotFound)), "a part of a serial names no board");
    }
}
