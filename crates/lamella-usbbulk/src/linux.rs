//! Linux CMSIS-DAP v2 (USB bulk) backend, against usbfs: sysfs (`/sys/bus/usb/devices`) for
//! discovery, `/dev/bus/usb/BBB/DDD` for I/O via the `USBDEVFS_*` ioctls. libc only -- no external
//! USB crate. The v2 sibling of lamella-usbhid's hidraw backend.

use crate::open_error::linux_open_failure as open_failure;
use crate::{Binding, DeviceInfo, Error, InterfaceClass, InterfaceInfo, Result, Setup, hand_over, take_left};
use std::fs;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;
use std::time::{Duration, Instant};

const USBDEVFS_CLAIMINTERFACE: libc::c_ulong = 0x8004_550f;
const USBDEVFS_RELEASEINTERFACE: libc::c_ulong = 0x8004_5510;
const USBDEVFS_BULK: libc::c_ulong = 0xc018_5502;
const USBDEVFS_CONTROL: libc::c_ulong =
    ioctl_read_write(b'U', 0, std::mem::size_of::<UsbdevfsCtrltransfer>());
const USBDEVFS_SUBMITURB: libc::c_ulong = ioctl_encoded(IOC_READ, b'U', 10, std::mem::size_of::<UsbdevfsUrb>());
const USBDEVFS_REAPURBNDELAY: libc::c_ulong =
    ioctl_encoded(IOC_WRITE, b'U', 13, std::mem::size_of::<*mut UsbdevfsUrb>());
const URB_TYPE_BULK: libc::c_uchar = 3;

const IOC_WRITE: libc::c_ulong = 1;
const IOC_READ: libc::c_ulong = 2;

/// An ioctl number in the asm-generic encoding of `include/uapi/asm-generic/ioctl.h`: the two direction
/// bits on top, then fourteen bits of size, eight of type and eight of number.
const fn ioctl_encoded(direction: libc::c_ulong, kind: u8, number: u8, size: usize) -> libc::c_ulong {
    (direction << 30) | ((size as libc::c_ulong) << 16) | ((kind as libc::c_ulong) << 8) | number as libc::c_ulong
}

/// `_IOWR(type, nr, size)` in that encoding.
const fn ioctl_read_write(kind: u8, number: u8, size: usize) -> libc::c_ulong {
    ioctl_encoded(IOC_READ | IOC_WRITE, kind, number, size)
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

/// `struct usbdevfs_urb` of `include/uapi/linux/usbdevice_fs.h`, for a bulk transfer: it has no
/// `iso_frame_desc`, the flexible array the structure ends in. The kernel fills `status` and
/// `actual_length`, and the buffer, when the URB is reaped.
#[repr(C)]
struct UsbdevfsUrb {
    kind: libc::c_uchar,
    endpoint: libc::c_uchar,
    status: libc::c_int,
    flags: libc::c_uint,
    buffer: *mut libc::c_void,
    buffer_length: libc::c_int,
    actual_length: libc::c_int,
    start_frame: libc::c_int,
    stream_id: libc::c_uint,
    error_count: libc::c_int,
    signr: libc::c_uint,
    usercontext: *mut libc::c_void,
}

/// A bulk IN transfer handed to usbfs and not yet reaped. Boxed, because the kernel holds the URB's
/// address, and the buffer's, from the submit until the reap.
struct OutstandingUrb {
    urb: UsbdevfsUrb,
    buffer: Vec<u8>,
    /// Whether it has been reaped, which is when its status and bytes became good. A reap answers for
    /// the whole file descriptor, so one endpoint's poll can reap another endpoint's transfer.
    reaped: bool,
}

/// What one endpoint's polls have in hand between calls.
#[derive(Default)]
struct EndpointReads {
    outstanding: Option<Box<OutstandingUrb>>,
    /// Bytes a finished transfer delivered that the buffer collecting it had no room for.
    left: Vec<u8>,
}

/// A device with an interface a scan accepted: its usbfs node, ids, the device's serial/product strings
/// (when sysfs reports them), what sysfs says of each accepted interface, and the one chosen to drive
/// (see [`SysfsInterface::rank`]).
struct Found {
    node: String,
    vid: u16,
    pid: u16,
    serial: Option<String>,
    product: Option<String>,
    /// One name for all the accepted interfaces ([`crate::vendor_interfaces_name`]).
    interface_name: Option<String>,
    /// The chosen interface's own name.
    chosen_name: Option<String>,
    vendor_interfaces: Vec<crate::VendorInterface>,
    composite: Option<bool>,
    product_is_absent: bool,
    interface: u8,
    ep_in: u8,
    ep_out: u8,
}

/// What sysfs says of one interface, reading files, so no device is opened.
struct SysfsInterface {
    number: u8,
    subclass_protocol: Option<(u8, u8)>,
    /// Its own name, from `.../interface`: the `iInterface` string the kernel read at enumeration.
    name: Option<String>,
    /// Its bulk endpoints' addresses, in descriptor order where the device's descriptors can be read.
    endpoints: Vec<u8>,
}

impl SysfsInterface {
    fn spec_shape(&self) -> bool {
        self.subclass_protocol.is_some_and(|(subclass, protocol)| crate::is_spec_shape(subclass, protocol))
    }

    /// The order to choose among a device's interfaces by, which `read_dir`'s order has no say in.
    fn rank(&self) -> ((bool, bool, bool), u8) {
        (crate::open_rank(Some(crate::has_bulk_pair(&self.endpoints)), self.name.as_deref(), self.spec_shape()), self.number)
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

/// The error for a polled transfer on `endpoint` that finished with the status `status`, a negated
/// errno, which is not success.
///
/// **A TIMEOUT'S CODE IS A FAILURE HERE TOO.** This transfer was given no timer, so a status saying
/// one ran out came from below, and reading it as a quiet device would start another transfer behind it,
/// again and again.
fn urb_failure(endpoint: u8, status: libc::c_int) -> Error {
    let error = std::io::Error::from_raw_os_error(-status);
    Error::Os(format!("a bulk transfer on ep 0x{endpoint:02x} failed: {error}"))
}

/// The error for a bulk transfer on `endpoint` that failed with `error`.
///
/// **Only a transfer whose timeout expired is [`Error::Timeout`]** (`ETIMEDOUT`, in
/// `Documentation/driver-api/usb/error-codes.rst`). Any other failure is [`Error::Os`], and that is
/// how a device that has gone away arrives (`ENODEV`), which a caller must not read as a quiet one.
fn bulk_failure(endpoint: u8, error: &std::io::Error) -> Error {
    match error.raw_os_error() {
        Some(libc::ETIMEDOUT) => Error::Timeout,
        _ => Error::Os(format!("USBDEVFS_BULK (ep 0x{endpoint:02x}): {error}")),
    }
}

/// What sysfs says of the name of the interface in directory `iface`, whose descriptor among the
/// device's raw `descriptors` is `described`: its name, an empty string for an interface with none, or
/// `None` for one sysfs does not settle.
///
/// The kernel creates `interface` only for an interface whose string it holds
/// (`usb_create_sysfs_intf_files` in `drivers/usb/core/sysfs.c`), so a missing file is either an
/// interface with no string or a string the kernel could not read. The `iInterface` in the device's
/// raw `descriptors` tells them apart. **A file that is there and cannot be read says nothing about
/// the interface**, so a refused read is `None`, never a name that is absent.
fn interface_name_reading(iface: &Path, described: Option<&crate::DescribedInterface>) -> Option<String> {
    match fs::read_to_string(iface.join("interface")) {
        Ok(name) => Some(name.trim().to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::named_or_unnamed(None, Some(u16::from(described?.string_index)))
        }
        Err(_) => None,
    }
}

/// This interface's bulk endpoints' addresses from its `ep_XX` sysfs subdirectories, lowest first:
/// `read_dir`'s order is unspecified, and these directories do not say what the descriptors' order was.
fn bulk_endpoints_in_directory(iface_dir: &Path) -> Vec<u8> {
    let mut endpoints = Vec::new();
    if let Ok(eps) = fs::read_dir(iface_dir) {
        for ep in eps.flatten() {
            if !ep.file_name().to_string_lossy().starts_with("ep_") {
                continue;
            }
            let addr = read_hex8(&ep.path().join("bEndpointAddress")).unwrap_or(0);
            let attr = read_hex8(&ep.path().join("bmAttributes")).unwrap_or(0);
            if attr & 0x03 == 0x02 {
                endpoints.push(addr);
            }
        }
    }
    endpoints.sort_unstable();
    endpoints
}

/// What sysfs says of the interface in directory `iface`, given the interfaces the device's raw
/// `descriptors` state for its active configuration.
fn read_interface(iface: &Path, described: &[crate::DescribedInterface]) -> SysfsInterface {
    let number = read_hex8(&iface.join("bInterfaceNumber")).unwrap_or(0);
    let alternate = read_dec(&iface.join("bAlternateSetting")).unwrap_or(0);
    let own = described.iter().find(|interface| (interface.number, interface.alternate) == (number, alternate));
    SysfsInterface {
        number,
        subclass_protocol: read_hex8(&iface.join("bInterfaceSubClass"))
            .zip(read_hex8(&iface.join("bInterfaceProtocol")))
            .or_else(|| own.map(|interface| (interface.subclass, interface.protocol))),
        name: interface_name_reading(iface, own),
        endpoints: own.map_or_else(|| bulk_endpoints_in_directory(iface), |interface| interface.bulk_endpoints.clone()),
    }
}

/// Whether a device's raw `descriptors`, which begin with its device descriptor (USB 2.0, Table 9-8),
/// state no product string.
fn descriptors_state_no_product(raw: &[u8]) -> bool {
    matches!(raw, [18, 1, _, _, _, _, _, _, _, _, _, _, _, _, _, 0, ..])
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
/// interface's directory -- one entry per device, with every such interface asked, since `read_dir`'s
/// order decides nothing.
///
/// **THE INTERFACE TO DRIVE IS THE ONE [`SysfsInterface::rank`] PUTS FIRST**, so a listing's reason and
/// the interface an open claims are one interface: a device naming CMSIS-DAP on one interface and a
/// bridge on another is driven through the first, whichever `read_dir` gives first.
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
        let raw = fs::read(dir.join("descriptors")).ok();
        let described = match (&raw, read_dec(&dir.join("bConfigurationValue"))) {
            (Some(raw), Some(configuration)) => crate::described_interfaces(raw, Some(configuration)),
            _ => Vec::new(),
        };
        let Ok(ifaces) = fs::read_dir(&dir) else {
            continue;
        };
        let mut accepted: Vec<SysfsInterface> = ifaces
            .flatten()
            .filter(|iface| {
                iface.file_name().to_string_lossy().starts_with(&format!("{name}:")) && wanted(&iface.path())
            })
            .map(|iface| read_interface(&iface.path(), &described))
            .collect();
        accepted.sort_by_key(SysfsInterface::rank);
        let Some(chosen) = accepted.first() else {
            continue;
        };
        let (ep_in, ep_out) = crate::bulk_pair(&chosen.endpoints, chosen.spec_shape());
        out.push(Found {
            node: format!("/dev/bus/usb/{busnum:03}/{devnum:03}"),
            vid,
            pid,
            product_is_absent: product.is_none() && raw.as_deref().is_some_and(descriptors_state_no_product),
            serial,
            product,
            interface_name: crate::vendor_interfaces_name(accepted.iter().map(|interface| interface.name.clone())),
            chosen_name: chosen.name.clone(),
            composite: read_dec(&dir.join("bNumInterfaces")).map(|count| count > 1),
            interface: chosen.number,
            ep_in,
            ep_out,
            vendor_interfaces: accepted
                .iter()
                .map(|interface| crate::VendorInterface {
                    subclass_protocol: interface.subclass_protocol,
                    name: interface.name.clone(),
                    bulk_pair: Some(crate::has_bulk_pair(&interface.endpoints)),
                    spec_guid: None,
                })
                .collect(),
        });
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
            vendor_interfaces: f.vendor_interfaces,
            composite: f.composite,
            product_is_absent: f.product_is_absent,
            port_id: None,
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
            interface_name: f.chosen_name.filter(|name| !name.is_empty()),
        })
        .collect()
}

pub struct Device {
    file: fs::File,
    interface: u8,
    ep_in: u8,
    ep_out: u8,
    /// What each polled endpoint has in hand, by endpoint address.
    reads: Vec<(u8, EndpointReads)>,
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
            reads: Vec::new(),
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
            return Err(bulk_failure(endpoint, &std::io::Error::last_os_error()));
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
        if self.reads.iter().any(|(known, reads)| *known == endpoint && (reads.outstanding.is_some() || !reads.left.is_empty())) {
            return self.poll_endpoint(endpoint, buf, timeout);
        }
        let ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        self.bulk(endpoint, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), ms)
    }

    pub fn write_packet(&mut self, data: &[u8]) -> Result<()> {
        self.write_endpoint(self.ep_out, data)
    }

    pub fn read_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.read_endpoint(self.ep_in, buf, timeout)
    }

    /// See [`crate::Device::poll_packet`].
    pub fn poll_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.poll_endpoint(self.ep_in, buf, timeout)
    }

    /// A read for a caller that polls: an endpoint with nothing to deliver in `timeout` is
    /// [`Error::Timeout`], and a transfer under way is not touched.
    ///
    /// **THE TRANSFER STAYS OUTSTANDING ACROSS A TIMEOUT, AND THE NEXT POLL COLLECTS IT.** `USBDEVFS_BULK`
    /// kills a transfer that runs out of time and copies nothing of it back (`do_proc_bulk` in
    /// `drivers/usb/core/devio.c`), so bytes arriving just as a poll times out would be acknowledged to
    /// the device and thrown away. A URB submitted here runs until it finishes or the device is closed.
    fn poll_endpoint(&mut self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let fd = self.file.as_raw_fd();
        let index = match self.reads.iter().position(|(known, _)| *known == endpoint) {
            Some(index) => index,
            None => {
                self.reads.push((endpoint, EndpointReads::default()));
                self.reads.len() - 1
            }
        };
        let reads = &mut self.reads[index].1;
        if !reads.left.is_empty() {
            return Ok(take_left(&mut reads.left, buf));
        }
        if reads.outstanding.is_none() {
            reads.outstanding = Some(submit_read(fd, endpoint, buf.len())?);
        }
        let mut failure = None;
        if !self.reads[index].1.outstanding.as_ref().is_some_and(|read| read.reaped) {
            wait_for_completion(fd, timeout)?;
            failure = reap_finished(fd, &mut self.reads).err();
        }
        let reads = &mut self.reads[index].1;
        if !reads.outstanding.as_ref().is_some_and(|read| read.reaped) {
            return Err(failure.unwrap_or(Error::Timeout));
        }
        let finished = reads.outstanding.take().expect("this read was outstanding");
        if finished.urb.status != 0 {
            return Err(urb_failure(endpoint, finished.urb.status));
        }
        let delivered = usize::try_from(finished.urb.actual_length).unwrap_or(0).min(finished.buffer.len());
        Ok(hand_over(&finished.buffer[..delivered], buf, &mut reads.left))
    }
}

/// A bulk IN transfer of `length` bytes on `endpoint`, handed to usbfs.
fn submit_read(fd: RawFd, endpoint: u8, length: usize) -> Result<Box<OutstandingUrb>> {
    let mut read = Box::new(OutstandingUrb {
        urb: UsbdevfsUrb {
            kind: URB_TYPE_BULK,
            endpoint,
            status: 0,
            flags: 0,
            buffer: std::ptr::null_mut(),
            buffer_length: libc::c_int::try_from(length).unwrap_or(libc::c_int::MAX),
            actual_length: 0,
            start_frame: 0,
            stream_id: 0,
            error_count: 0,
            signr: 0,
            usercontext: std::ptr::null_mut(),
        },
        buffer: vec![0; length],
        reaped: false,
    });
    read.urb.buffer = read.buffer.as_mut_ptr().cast();
    let urb: *mut UsbdevfsUrb = &raw mut read.urb;
    let rc = unsafe { libc::ioctl(fd, USBDEVFS_SUBMITURB, urb) };
    if rc < 0 {
        let error = std::io::Error::last_os_error();
        return Err(Error::Os(format!("USBDEVFS_SUBMITURB (ep 0x{endpoint:02x}): {error}")));
    }
    Ok(read)
}

/// Waits until a transfer on `fd` has finished or `timeout` has passed, and returns either way. usbfs
/// reports a finished URB as writable and a device that has gone as a hangup (`usbdev_poll` in
/// `drivers/usb/core/devio.c`), and a reap then says which it was.
fn wait_for_completion(fd: RawFd, timeout: Duration) -> Result<()> {
    let until = Instant::now() + timeout;
    loop {
        let mut watched = libc::pollfd { fd, events: libc::POLLOUT | libc::POLLWRNORM, revents: 0 };
        let left = until.saturating_duration_since(Instant::now());
        let ms = libc::c_int::try_from(left.as_micros().div_ceil(1000)).unwrap_or(libc::c_int::MAX);
        let rc = unsafe { libc::poll(&raw mut watched, 1, ms) };
        if rc >= 0 {
            return Ok(());
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(Error::Os(format!("waiting for a bulk transfer: {error}")));
        }
    }
}

/// Reaps every URB on `fd` that has finished, marking each in `reads`, until none is left.
///
/// # Errors
/// The device has gone and nothing is left to reap (`ENODEV`), or the reap failed some other way.
/// A URB reaped before that is marked, so what it delivered still reaches its reader first.
fn reap_finished(fd: RawFd, reads: &mut [(u8, EndpointReads)]) -> Result<()> {
    loop {
        let mut reaped: *mut UsbdevfsUrb = std::ptr::null_mut();
        let rc = unsafe { libc::ioctl(fd, USBDEVFS_REAPURBNDELAY, &raw mut reaped) };
        if rc < 0 {
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(libc::EAGAIN) => Ok(()),
                _ => Err(Error::Os(format!("USBDEVFS_REAPURBNDELAY: {error}"))),
            };
        }
        for (_, endpoint) in reads.iter_mut() {
            if let Some(read) = endpoint.outstanding.as_mut()
                && std::ptr::eq(&raw const read.urb, reaped)
            {
                read.reaped = true;
            }
        }
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
        .map_err(|error| open_failure(node, &error))?;
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
    use crate::{InterfaceClass, VendorInterface};
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

    /// A vendor-class interface directory with no `interface` file, as the kernel leaves one whose
    /// string it does not hold.
    fn nameless_vendor_interface(device: &Path, number: u8) -> PathBuf {
        let device_name = device.file_name().unwrap().to_string_lossy().into_owned();
        let dir = device.join(format!("{device_name}:1.{number}"));
        FakeSysfs::write(&dir, "bInterfaceNumber", &format!("{number:02x}\n"));
        FakeSysfs::write(&dir, "bInterfaceClass", "ff\n");
        FakeSysfs::write(&dir, "bAlternateSetting", " 0\n");
        dir
    }

    /// `device`'s `descriptors` and `bConfigurationValue`: a device descriptor, then configuration 1
    /// with one interface, number 0, whose `iInterface` is `string_index`.
    fn descriptors_of_one_interface(device: &Path, string_index: u8) {
        let mut raw = vec![18, 1];
        raw.resize(18, 0);
        raw.extend_from_slice(&[9, 2, 18, 0, 1, 1, 0, 0x80, 50]);
        raw.extend_from_slice(&[9, 4, 0, 0, 2, 0xFF, 0, 0, string_index]);
        std::fs::write(device.join("descriptors"), raw).unwrap();
        FakeSysfs::write(device, "bConfigurationValue", "1\n");
    }

    /// The name `enumerate` would report for the only device under `root`.
    fn only_vendor_name(root: &Path) -> Option<String> {
        let found = super::scan_where(root, |iface| {
            super::read_hex8(&iface.join("bInterfaceClass")) == Some(0xFF)
        });
        assert_eq!(found.len(), 1);
        found.into_iter().next().unwrap().interface_name
    }

    /// **THE DESCRIPTOR SETTLES A MISSING FILE.** `iInterface` zero is an interface with no name, which
    /// is a settled answer. A non-zero index is a string the kernel could not read, and that says
    /// nothing about what the device is.
    #[test]
    fn a_missing_interface_file_is_settled_by_the_interfaces_own_string_index() {
        let sysfs = FakeSysfs::new("nameless");
        let device = sysfs.device("1-5", 0x0403, 0x6001, "DDDD4444");
        nameless_vendor_interface(&device, 0);
        descriptors_of_one_interface(&device, 0);
        assert_eq!(only_vendor_name(&sysfs.root).as_deref(), Some(""));

        descriptors_of_one_interface(&device, 5);
        assert_eq!(only_vendor_name(&sysfs.root), None);
    }

    /// **WHAT COULD NOT BE READ IS NOT ABSENT.** With no way to read the descriptors, or with an
    /// `interface` attribute that is there and refuses the read, the name stays unknown, so a probe
    /// is not lost to a permission. A directory in its place fails the read as a refusal does, whoever
    /// runs the test, and `chmod` does not stop root.
    #[test]
    fn a_name_that_could_not_be_read_stays_unknown() {
        let sysfs = FakeSysfs::new("unreadable");
        let device = sysfs.device("1-6", 0x0403, 0x6001, "EEEE5555");
        let interface = nameless_vendor_interface(&device, 0);
        assert_eq!(only_vendor_name(&sysfs.root), None, "no descriptors file");

        descriptors_of_one_interface(&device, 0);
        std::fs::create_dir(interface.join("interface")).unwrap();
        assert_eq!(only_vendor_name(&sysfs.root), None, "an attribute that refuses the read");
    }

    /// A device that names any of its vendor-class interfaces has that name, whichever of them
    /// `read_dir` happens to give first.
    #[test]
    fn a_nameless_interface_does_not_hide_a_named_one_beside_it() {
        let sysfs = FakeSysfs::new("two");
        let device = sysfs.device("1-7", 0x2e8a, 0x000c, "FFFF6666");
        nameless_vendor_interface(&device, 0);
        descriptors_of_one_interface(&device, 0);
        FakeSysfs::interface(&device, 1, (0xFF, 0x00, 0x00), "CMSIS-DAP v2 Interface");
        assert_eq!(only_vendor_name(&sysfs.root).as_deref(), Some("CMSIS-DAP v2 Interface"));
    }

    /// `device`'s `descriptors`, `bConfigurationValue` and `bNumInterfaces`: a device descriptor whose
    /// `iProduct` is `product_index`, then configuration 1 with `interfaces`, each its class, its
    /// `iInterface` and its bulk endpoints in descriptor order.
    fn descriptors_of(device: &Path, product_index: u8, interfaces: &[(u8, (u8, u8, u8), u8, &[u8])]) {
        let mut raw = vec![18, 1];
        raw.resize(18, 0);
        raw[15] = product_index;
        raw.extend_from_slice(&[9, 2, 0, 0, interfaces.len() as u8, 1, 0, 0x80, 50]);
        for (number, class, name_index, endpoints) in interfaces {
            raw.extend_from_slice(&[9, 4, *number, 0, endpoints.len() as u8, class.0, class.1, class.2, *name_index]);
            for address in *endpoints {
                raw.extend_from_slice(&[7, 5, *address, 2, 64, 0, 0]);
            }
        }
        std::fs::write(device.join("descriptors"), raw).unwrap();
        FakeSysfs::write(device, "bConfigurationValue", "1\n");
        FakeSysfs::write(device, "bNumInterfaces", &format!("{:2}\n", interfaces.len()));
    }

    /// The only vendor-class device under `root`, as a scan finds it.
    fn only_vendor_device(root: &Path) -> Found {
        let mut found = super::scan_where(root, |iface| {
            super::read_hex8(&iface.join("bInterfaceClass")) == Some(0xFF)
        });
        assert_eq!(found.len(), 1);
        found.pop().unwrap()
    }

    /// **THE INTERFACE CHOSEN IS THE ONE THAT NAMES ITSELF, WHATEVER ITS NUMBER,** and each interface is
    /// reported as what it is. A scan cannot make `read_dir` give another order, so the numbers are what
    /// changes here; the scripted device's test on the real kernel opens both.
    #[test]
    fn the_interface_naming_cmsis_dap_is_chosen_with_its_own_endpoints_whichever_its_number() {
        for (bridge, dap) in [(0u8, 1u8), (1, 0)] {
            let sysfs = FakeSysfs::new(&format!("two-names-{bridge}"));
            let device = sysfs.device("1-8", 0x1209, 0x0001, "ABCD1234");
            FakeSysfs::interface(&device, bridge, (0xFF, 0x00, 0x00), "Bridge");
            FakeSysfs::interface(&device, dap, (0xFF, 0x00, 0x00), "Acme CMSIS-DAP v2");
            let bridge_endpoints: &[u8] = &[0x01, 0x81];
            let dap_endpoints: &[u8] = &[0x02, 0x82];
            let mut interfaces = [
                (bridge, (0xFF, 0x00, 0x00), 4u8, bridge_endpoints),
                (dap, (0xFF, 0x00, 0x00), 5u8, dap_endpoints),
            ];
            interfaces.sort_by_key(|interface| interface.0);
            descriptors_of(&device, 2, &interfaces);
            let found = only_vendor_device(&sysfs.root);
            assert_eq!(found.interface, dap);
            assert_eq!((found.ep_in, found.ep_out), (0x82, 0x02));
            assert_eq!(found.chosen_name.as_deref(), Some("Acme CMSIS-DAP v2"));
            assert_eq!(found.interface_name.as_deref(), Some("Acme CMSIS-DAP v2"));
            assert_eq!(found.composite, Some(true));
            let mut names: Vec<_> = found.vendor_interfaces.iter().filter_map(|interface| interface.name.clone()).collect();
            names.sort();
            assert_eq!(names, ["Acme CMSIS-DAP v2", "Bridge"]);
        }
    }

    /// With no name to go by, the shape the specification requires comes first, then any pair, and a
    /// device's lowest number breaks a tie; an interface with no pair is chosen only when none has one.
    #[test]
    fn with_no_name_the_spec_shape_with_a_pair_is_chosen_and_then_any_pair() {
        let sysfs = FakeSysfs::new("no-names");
        let device = sysfs.device("1-9", 0x1209, 0x0002, "ABCD1235");
        for (number, class) in [(0u8, (0xFFu8, 0x10u8, 0xFFu8)), (1, (0xFF, 0x00, 0x00)), (2, (0xFF, 0x00, 0x00))] {
            let directory = nameless_vendor_interface(&device, number);
            FakeSysfs::write(&directory, "bInterfaceSubClass", &format!("{:02x}\n", class.1));
            FakeSysfs::write(&directory, "bInterfaceProtocol", &format!("{:02x}\n", class.2));
        }
        let none: &[u8] = &[];
        let pair: &[u8] = &[0x01, 0x81];
        let other: &[u8] = &[0x03, 0x83];
        descriptors_of(&device, 0, &[(0, (0xFF, 0x10, 0xFF), 0, pair), (1, (0xFF, 0, 0), 0, none), (2, (0xFF, 0, 0), 0, other)]);
        let found = only_vendor_device(&sysfs.root);
        assert_eq!(found.interface, 2, "the spec's shape with a pair");
        assert_eq!((found.ep_in, found.ep_out), (0x83, 0x03));
        assert_eq!(found.interface_name.as_deref(), Some(""), "every one is known to have no name");
        assert!(found.product_is_absent, "iProduct is zero and there is no product file");
        let shapes: Vec<_> = {
            let mut interfaces = found.vendor_interfaces.clone();
            interfaces.sort_by_key(|interface| interface.subclass_protocol);
            interfaces.iter().map(|interface| (interface.subclass_protocol, interface.bulk_pair)).collect()
        };
        assert_eq!(shapes, [(Some((0, 0)), Some(true)), (Some((0, 0)), Some(false)), (Some((0x10, 0xFF)), Some(true))]);
    }

    /// **THE ENDPOINTS ARE TAKEN IN DESCRIPTOR ORDER,** so a v2 probe whose SWO endpoint has the lower
    /// address reads replies from the endpoint the specification puts second.
    #[test]
    fn a_spec_shaped_interface_keeps_the_descriptors_order_of_endpoints() {
        let sysfs = FakeSysfs::new("swo");
        let device = sysfs.device("1-10", 0x1209, 0x0003, "ABCD1236");
        FakeSysfs::interface(&device, 0, (0xFF, 0x00, 0x00), "CMSIS-DAP v2");
        descriptors_of(&device, 2, &[(0, (0xFF, 0, 0), 4, &[0x02, 0x83, 0x81])]);
        let found = only_vendor_device(&sysfs.root);
        assert_eq!((found.ep_in, found.ep_out), (0x83, 0x02));
        assert_eq!(found.composite, Some(false));
    }

    /// A string the descriptors promise and the kernel does not hold is not a string that is absent.
    #[test]
    fn a_missing_product_file_is_absent_only_where_the_descriptors_state_no_string() {
        let sysfs = FakeSysfs::new("product");
        let device = sysfs.device("1-11", 0x1209, 0x0004, "ABCD1237");
        FakeSysfs::interface(&device, 0, (0xFF, 0x00, 0x00), "Interface");
        descriptors_of(&device, 2, &[(0, (0xFF, 0, 0), 4, &[0x01, 0x81])]);
        assert!(!only_vendor_device(&sysfs.root).product_is_absent, "the index promises one the kernel could not read");
        descriptors_of(&device, 0, &[(0, (0xFF, 0, 0), 4, &[0x01, 0x81])]);
        assert!(only_vendor_device(&sysfs.root).product_is_absent);
        FakeSysfs::write(&device, "product", "A product\n");
        assert!(!only_vendor_device(&sysfs.root).product_is_absent, "there is a string");
        std::fs::remove_file(device.join("product")).unwrap();
        std::fs::remove_file(device.join("descriptors")).unwrap();
        assert!(!only_vendor_device(&sysfs.root).product_is_absent, "no way to say");
    }

    /// An interface reported by `interfaces_of_class` carries its own name, not the name of its device's
    /// others.
    #[test]
    fn a_listed_interface_has_its_own_name_and_number() {
        let sysfs = FakeSysfs::new("own-name");
        let device = sysfs.device("1-12", 0x1209, 0x0005, "ABCD1238");
        FakeSysfs::interface(&device, 0, (0xFF, 0x42, 0x01), "Zeta");
        FakeSysfs::interface(&device, 1, (0xFF, 0x42, 0x01), "Alpha");
        let class = InterfaceClass { class: 0xFF, subclass: 0x42, protocol: 0x01 };
        let listed = super::interfaces_of_class(&sysfs.root, class);
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].interface_number, listed[0].interface_name.as_deref()), (0, Some("Zeta")));
    }

    /// **A PERMISSION REFUSED NAMES ITS CAUSE,** since a reader on Linux meets it first and nothing in the
    /// operating system's words says what to do; any other failure stays as the OS said it.
    #[test]
    fn a_permission_refused_at_the_open_names_the_udev_rule() {
        let refused = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let said = super::open_failure("/dev/bus/usb/001/002", &refused).to_string();
        assert!(said.contains("/dev/bus/usb/001/002") && said.contains("udev rule"), "{said}");
        let other = std::io::Error::from(std::io::ErrorKind::NotFound);
        assert!(!super::open_failure("/dev/bus/usb/001/002", &other).to_string().contains("udev"));
    }

    /// **ONLY A TIMEOUT IS A TIMEOUT.** A quiet device keeps a session; a device that has gone away
    /// must end it, so its code stays an error that names it.
    #[test]
    fn a_bulk_timeout_is_a_timeout_and_a_lost_device_is_not() {
        let failed = |code| super::bulk_failure(0x81, &std::io::Error::from_raw_os_error(code));
        assert!(matches!(failed(libc::ETIMEDOUT), crate::Error::Timeout));
        for code in [libc::ENODEV, libc::ESHUTDOWN, libc::EPIPE, libc::EPROTO] {
            match failed(code) {
                crate::Error::Os(message) => assert!(message.contains("ep 0x81"), "{message}"),
                other => panic!("errno {code} should be an OS error, not {other:?}"),
            }
        }
    }

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

    /// The URB calls' numbers on a 64-bit host, as a C program including `linux/usbdevice_fs.h` prints
    /// them, and the structure's layout they carry.
    #[test]
    #[cfg(target_pointer_width = "64")]
    fn the_urb_requests_are_the_numbers_the_header_gives() {
        use std::mem::{offset_of, size_of};
        assert_eq!(super::USBDEVFS_SUBMITURB, 0x8038_550a);
        assert_eq!(super::USBDEVFS_REAPURBNDELAY, 0x4008_550d);
        assert_eq!(size_of::<super::UsbdevfsUrb>(), 56);
        assert_eq!(offset_of!(super::UsbdevfsUrb, buffer), 16);
        assert_eq!(offset_of!(super::UsbdevfsUrb, actual_length), 28);
        assert_eq!(offset_of!(super::UsbdevfsUrb, usercontext), 48);
    }

    /// **A POLLED TRANSFER THAT FAILED IS THAT FAILURE, A TIMEOUT'S CODE TOO:** it was given no timer, and
    /// reading the code as a quiet device would start another transfer behind it.
    #[test]
    fn a_polled_transfer_that_failed_keeps_its_code_and_is_never_a_timeout() {
        for code in [libc::ETIMEDOUT, libc::ENODEV, libc::ESHUTDOWN, libc::EPIPE, libc::EPROTO] {
            match super::urb_failure(0x81, -code) {
                crate::Error::Os(message) => {
                    assert!(message.contains("ep 0x81"), "{message}");
                    assert!(message.contains(&std::io::Error::from_raw_os_error(code).to_string()), "{message}");
                }
                other => panic!("errno {code} should be an OS error, not {other:?}"),
            }
        }
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
            chosen_name: None,
            vendor_interfaces: Vec::<VendorInterface>::new(),
            composite: None,
            product_is_absent: false,
            interface: 0,
            ep_in: 0x81,
            ep_out: 0x01,
        }
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
