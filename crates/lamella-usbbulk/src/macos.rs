//! macOS CMSIS-DAP v2 (USB bulk) backend via IOKit's IOUSBLib -- the classic COM-style plug-in
//! interface. Hand-rolled FFI to the public IOKit frameworks (no 3rd-party crates, no libc); the
//! vtable layouts are transcribed from IOUSBLib.h (IOUSBDeviceInterface500 / InterfaceInterface500).
//! The v2 sibling of lamella-usbhid's IOKit HID backend: open a probe by VID/PID and exchange raw
//! bulk packets over its vendor interface's bulk IN + OUT pipes (WritePipeTO / ReadPipeTO).
#![allow(non_upper_case_globals, non_snake_case, non_camel_case_types, unsafe_op_in_unsafe_fn)]

use super::iokit_status::{finished_read, is_timeout, kIOUSBPipeStalled, open_refused, pipe_failure};
use super::take_left;
use super::{Binding, DeviceInfo, Error, Result};
use core::ffi::c_void;
use std::cell::Cell;
use std::os::raw::c_char;
use std::ptr::{null, null_mut};
use std::time::Duration;

type kern_return_t = i32;
type IOReturn = i32;
type io_object_t = u32;
type io_iterator_t = u32;
type io_service_t = u32;
type mach_port_t = u32;
type CFAllocatorRef = *const c_void;
type CFUUIDRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFStringRef = *const c_void;
type CFTypeRef = *const c_void;
type CFRunLoopRef = *const c_void;
type CFRunLoopSourceRef = *const c_void;
type IOAsyncCallback1 = extern "C" fn(*mut c_void, IOReturn, *mut c_void);

#[repr(C)]
#[derive(Clone, Copy)]
struct CFUUIDBytes {
    b: [u8; 16],
}

const kIOReturnSuccess: IOReturn = 0;
const kUSBIn: u8 = 1;
#[allow(dead_code)]
const kUSBOut: u8 = 0;
const kUSBBulk: u8 = 2;
const DONT_CARE: u16 = 0xFFFF;
const kCFStringEncodingUTF8: u32 = 0x0800_0100;

const ID_CFPLUGIN: [u8; 16] = [0xC2, 0x44, 0xE8, 0x58, 0x10, 0x9C, 0x11, 0xD4, 0x91, 0xD4, 0x00, 0x50, 0xE4, 0xC6, 0x42, 0x6F];
const ID_DEV_USERCLIENT: [u8; 16] = [0x9d, 0xc7, 0xb7, 0x80, 0x9e, 0xc0, 0x11, 0xD4, 0xa5, 0x4f, 0x00, 0x0a, 0x27, 0x05, 0x28, 0x61];
const ID_INTF_USERCLIENT: [u8; 16] = [0x2d, 0x97, 0x86, 0xc6, 0x9e, 0xf3, 0x11, 0xD4, 0xad, 0x51, 0x00, 0x0a, 0x27, 0x05, 0x28, 0x61];
const ID_DEV_IFACE500: [u8; 16] = [0xA3, 0x3C, 0xF0, 0x47, 0x4B, 0x5B, 0x48, 0xE2, 0xB5, 0x7D, 0x02, 0x07, 0xFC, 0xEA, 0xE1, 0x3B];
const ID_INTF_IFACE500: [u8; 16] = [0x6C, 0x0D, 0x38, 0xC3, 0xB0, 0x93, 0x4E, 0xA7, 0x80, 0x9B, 0x09, 0xFB, 0x5D, 0xDD, 0xAC, 0x16];

#[repr(C)]
struct IOUSBFindInterfaceRequest {
    bInterfaceClass: u16,
    bInterfaceSubClass: u16,
    bInterfaceProtocol: u16,
    bAlternateSetting: u16,
}

type QueryInterfaceFn = extern "C" fn(*mut c_void, CFUUIDBytes, *mut *mut c_void) -> i32;
type RefFn = extern "C" fn(*mut c_void) -> u32;

#[repr(C)]
struct IOCFPlugInInterface {
    _reserved: *mut c_void,
    QueryInterface: QueryInterfaceFn,
    AddRef: RefFn,
    Release: RefFn,
}

#[repr(C)]
struct IOUSBDeviceInterface500 {
    _reserved: *mut c_void,
    QueryInterface: QueryInterfaceFn,
    AddRef: RefFn,
    Release: RefFn,
    CreateDeviceAsyncEventSource: *const c_void,
    GetDeviceAsyncEventSource: *const c_void,
    CreateDeviceAsyncPort: *const c_void,
    GetDeviceAsyncPort: *const c_void,
    USBDeviceOpen: extern "C" fn(*mut c_void) -> IOReturn,
    USBDeviceClose: extern "C" fn(*mut c_void) -> IOReturn,
    GetDeviceClass: *const c_void,
    GetDeviceSubClass: *const c_void,
    GetDeviceProtocol: *const c_void,
    GetDeviceVendor: extern "C" fn(*mut c_void, *mut u16) -> IOReturn,
    GetDeviceProduct: extern "C" fn(*mut c_void, *mut u16) -> IOReturn,
    GetDeviceReleaseNumber: *const c_void,
    GetDeviceAddress: *const c_void,
    GetDeviceBusPowerAvailable: *const c_void,
    GetDeviceSpeed: *const c_void,
    GetNumberOfConfigurations: *const c_void,
    GetLocationID: *const c_void,
    GetConfigurationDescriptorPtr: extern "C" fn(*mut c_void, u8, *mut *const u8) -> IOReturn,
    GetConfiguration: extern "C" fn(*mut c_void, *mut u8) -> IOReturn,
    SetConfiguration: extern "C" fn(*mut c_void, u8) -> IOReturn,
    GetBusFrameNumber: *const c_void,
    ResetDevice: *const c_void,
    DeviceRequest: *const c_void,
    DeviceRequestAsync: *const c_void,
    CreateInterfaceIterator: extern "C" fn(*mut c_void, *const IOUSBFindInterfaceRequest, *mut io_iterator_t) -> IOReturn,
    USBDeviceOpenSeize: *const c_void,
    DeviceRequestTO: extern "C" fn(*mut c_void, *mut IOUSBDevRequestTO) -> IOReturn,
}

/// `IOUSBDevRequestTO` from IOUSBFamily's `USB.h`: a control request with its two timeouts, in
/// milliseconds. The header declares it outside its `#pragma pack(1)` regions, so each field sits
/// at its natural alignment.
#[repr(C)]
struct IOUSBDevRequestTO {
    bmRequestType: u8,
    bRequest: u8,
    wValue: u16,
    wIndex: u16,
    wLength: u16,
    pData: *mut c_void,
    wLenDone: u32,
    noDataTimeout: u32,
    completionTimeout: u32,
}

#[repr(C)]
struct IOUSBInterfaceInterface500 {
    _reserved: *mut c_void,
    QueryInterface: QueryInterfaceFn,
    AddRef: RefFn,
    Release: RefFn,
    CreateInterfaceAsyncEventSource: extern "C" fn(*mut c_void, *mut CFRunLoopSourceRef) -> IOReturn,
    GetInterfaceAsyncEventSource: *const c_void,
    CreateInterfaceAsyncPort: *const c_void,
    GetInterfaceAsyncPort: *const c_void,
    USBInterfaceOpen: extern "C" fn(*mut c_void) -> IOReturn,
    USBInterfaceClose: extern "C" fn(*mut c_void) -> IOReturn,
    GetInterfaceClass: extern "C" fn(*mut c_void, *mut u8) -> IOReturn,
    GetInterfaceSubClass: extern "C" fn(*mut c_void, *mut u8) -> IOReturn,
    GetInterfaceProtocol: extern "C" fn(*mut c_void, *mut u8) -> IOReturn,
    GetDeviceVendor: *const c_void,
    GetDeviceProduct: *const c_void,
    GetDeviceReleaseNumber: *const c_void,
    GetConfigurationValue: *const c_void,
    GetInterfaceNumber: *const c_void,
    GetAlternateSetting: *const c_void,
    GetNumEndpoints: extern "C" fn(*mut c_void, *mut u8) -> IOReturn,
    GetLocationID: *const c_void,
    GetDevice: *const c_void,
    SetAlternateInterface: *const c_void,
    GetBusFrameNumber: *const c_void,
    ControlRequest: *const c_void,
    ControlRequestAsync: *const c_void,
    GetPipeProperties: extern "C" fn(*mut c_void, u8, *mut u8, *mut u8, *mut u8, *mut u16, *mut u8) -> IOReturn,
    GetPipeStatus: *const c_void,
    AbortPipe: extern "C" fn(*mut c_void, u8) -> IOReturn,
    ResetPipe: *const c_void,
    ClearPipeStall: *const c_void,
    ReadPipe: *const c_void,
    WritePipe: *const c_void,
    ReadPipeAsync: extern "C" fn(*mut c_void, u8, *mut c_void, u32, IOAsyncCallback1, *mut c_void) -> IOReturn,
    WritePipeAsync: *const c_void,
    ReadIsochPipeAsync: *const c_void,
    WriteIsochPipeAsync: *const c_void,
    ControlRequestTO: *const c_void,
    ControlRequestAsyncTO: *const c_void,
    ReadPipeTO: extern "C" fn(*mut c_void, u8, *mut c_void, *mut u32, u32, u32) -> IOReturn,
    WritePipeTO: extern "C" fn(*mut c_void, u8, *const c_void, u32, u32, u32) -> IOReturn,
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFUUIDGetConstantUUIDWithBytes(
        alloc: CFAllocatorRef, b0: u8, b1: u8, b2: u8, b3: u8, b4: u8, b5: u8, b6: u8, b7: u8,
        b8: u8, b9: u8, b10: u8, b11: u8, b12: u8, b13: u8, b14: u8, b15: u8,
    ) -> CFUUIDRef;
    fn CFStringCreateWithCString(alloc: CFAllocatorRef, c_str: *const c_char, encoding: u32) -> CFStringRef;
    fn CFStringGetCString(the_string: CFStringRef, buffer: *mut c_char, buffer_size: isize, encoding: u32) -> u8;
    fn CFRelease(cf: CFTypeRef);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopAddSource(run_loop: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRemoveSource(run_loop: CFRunLoopRef, source: CFRunLoopSourceRef, mode: CFStringRef);
    fn CFRunLoopRunInMode(mode: CFStringRef, seconds: f64, return_after_source_handled: u8) -> i32;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFDictionaryRef;
    fn IOServiceGetMatchingServices(mainPort: mach_port_t, matching: CFDictionaryRef, existing: *mut io_iterator_t) -> kern_return_t;
    fn IOIteratorNext(iterator: io_iterator_t) -> io_object_t;
    fn IOObjectRelease(object: io_object_t) -> kern_return_t;
    fn IOCreatePlugInInterfaceForService(service: io_service_t, pluginType: CFUUIDRef, interfaceType: CFUUIDRef, theInterface: *mut *mut *mut IOCFPlugInInterface, theScore: *mut i32) -> kern_return_t;
    fn IODestroyPlugInInterface(interface: *mut *mut IOCFPlugInInterface) -> kern_return_t;
    fn IORegistryEntryCreateCFProperty(entry: io_object_t, key: CFStringRef, allocator: CFAllocatorRef, options: u32) -> CFTypeRef;
    fn CFNumberGetValue(number: CFTypeRef, theType: i32, valuePtr: *mut c_void) -> u8;
    fn IORegistryEntryGetChildIterator(entry: io_object_t, plane: *const c_char, iterator: *mut io_iterator_t) -> kern_return_t;
}

/// One NUMERIC IORegistry property of a device entry -- `idVendor`, `idProduct`, `bInterfaceClass`.
///
/// The kernel publishes these when the device enumerates, so reading them costs no user client and
/// touches the device not at all. `kCFNumberSInt32Type` is 3; the values here are all byte or word
/// sized, so a 32-bit read that does not fit a `u16` is a property that is not what was asked for.
unsafe fn registry_u16(entry: io_object_t, key: &str) -> Option<u16> {
    let key_c = std::ffi::CString::new(key).ok()?;
    let cf_key = CFStringCreateWithCString(null(), key_c.as_ptr(), kCFStringEncodingUTF8);
    if cf_key.is_null() {
        return None;
    }
    let value = IORegistryEntryCreateCFProperty(entry, cf_key, null(), 0);
    CFRelease(cf_key);
    if value.is_null() {
        return None;
    }
    let mut raw: i32 = 0;
    let ok = CFNumberGetValue(value, 3, (&raw mut raw).cast::<c_void>());
    CFRelease(value);
    (ok != 0).then(|| u16::try_from(raw).ok()).flatten()
}

/// What the IORegistry says of a device's vendor-specific (class `0xFF`) interfaces -- the CMSIS-DAP v2
/// and Lamella Link shape -- instead of a configuration descriptor, and whether it has more than one
/// interface of any class.
///
/// **An empty list is a settled no; `None` means the registry did not say**, which is a different
/// thing and is why this is an `Option`. The interfaces are child entries of the device on the
/// IOService plane and each publishes `bInterfaceClass`; a device whose children are not yet
/// published answers `None` and the caller falls back rather than concluding it is not a probe.
unsafe fn vendor_interfaces_in_registry(svc: io_object_t) -> Option<(Vec<crate::VendorInterface>, bool)> {
    let plane = std::ffi::CString::new("IOService").ok()?;
    let mut iter: io_iterator_t = 0;
    if IORegistryEntryGetChildIterator(svc, plane.as_ptr(), &mut iter) != kIOReturnSuccess {
        return None;
    }
    let mut interfaces = 0usize;
    let mut vendor = Vec::new();
    loop {
        let child = IOIteratorNext(iter);
        if child == 0 {
            break;
        }
        if let Some(class) = registry_u16(child, "bInterfaceClass") {
            interfaces += 1;
            if class == 0xFF {
                vendor.push(crate::VendorInterface {
                    subclass_protocol: registry_shape(child),
                    name: crate::named_or_unnamed(registry_string(child, "kUSBString"), registry_u16(child, "iInterface")),
                    bulk_pair: registry_u16(child, "bNumEndpoints").and_then(|count| (count < 2).then_some(false)),
                    spec_guid: None,
                });
            }
        }
        IOObjectRelease(child);
    }
    IOObjectRelease(iter);
    (interfaces > 0).then_some((vendor, interfaces > 1))
}

/// `bInterfaceSubClass` and `bInterfaceProtocol`, from an interface's IORegistry entry.
unsafe fn registry_shape(entry: io_object_t) -> Option<(u8, u8)> {
    let subclass = u8::try_from(registry_u16(entry, "bInterfaceSubClass")?).ok()?;
    let protocol = u8::try_from(registry_u16(entry, "bInterfaceProtocol")?).ok()?;
    Some((subclass, protocol))
}

/// Read a string property (`"USB Serial Number"`, `"USB Product Name"`) from a device's IORegistry entry,
/// without opening the device. `None` if absent, empty, or not a string.
unsafe fn registry_string(entry: io_object_t, key: &str) -> Option<String> {
    let key_c = std::ffi::CString::new(key).ok()?;
    let cf_key = CFStringCreateWithCString(null(), key_c.as_ptr(), kCFStringEncodingUTF8);
    if cf_key.is_null() {
        return None;
    }
    let value = IORegistryEntryCreateCFProperty(entry, cf_key, null(), 0);
    CFRelease(cf_key);
    if value.is_null() {
        return None;
    }
    let mut buf = [0 as c_char; 256];
    let ok = CFStringGetCString(value, buf.as_mut_ptr(), buf.len() as isize, kCFStringEncodingUTF8);
    CFRelease(value);
    if ok == 0 {
        return None;
    }
    let text = std::ffi::CStr::from_ptr(buf.as_ptr()).to_str().ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

unsafe fn cfuuid(b: &[u8; 16]) -> CFUUIDRef {
    CFUUIDGetConstantUUIDWithBytes(null(), b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15])
}

pub struct Device {
    dev: *mut *mut IOUSBDeviceInterface500,
    intf: *mut *mut IOUSBInterfaceInterface500,
    ep_in: u8,
    ep_out: u8,
    pipes: Vec<(u8, u8)>,
    /// The event source a poll's completion arrives through. Null until the first poll.
    source: CFRunLoopSourceRef,
    /// The run loop mode a poll waits in. A mode is matched by its name, so every device shares this one,
    /// and a device's source is in it only during that device's wait, so waiting services nothing else.
    /// Null until the first poll.
    mode: CFStringRef,
    /// What each polled pipe has in hand, by pipe reference.
    reads: Vec<(u8, PipeRead)>,
}

/// A bulk IN transfer handed to IOKit and not yet collected. Boxed, because IOKit holds its address
/// from the call until the completion arrives.
struct OutstandingRead {
    buffer: Vec<u8>,
    /// The transfer's status and the bytes it delivered, set by [`read_finished`].
    finished: Cell<Option<(IOReturn, usize)>>,
}

/// What one pipe's reads have in hand between calls.
#[derive(Default)]
struct PipeRead {
    outstanding: Option<Box<OutstandingRead>>,
    /// Bytes a finished transfer delivered that the buffer collecting it had no room for.
    left: Vec<u8>,
}

/// Records a read's completion. IOKit calls this on the thread that runs the event source, which
/// [`wait_for`] does, inside `CFRunLoopRunInMode`.
extern "C" fn read_finished(refcon: *mut c_void, status: IOReturn, delivered: *mut c_void) {
    let read = unsafe { &*refcon.cast::<OutstandingRead>() };
    read.finished.set(Some((status, delivered as usize)));
}

/// Runs `source` until `read` has finished or `timeout` has passed, and returns either way.
unsafe fn wait_for(read: &OutstandingRead, source: CFRunLoopSourceRef, mode: CFStringRef, timeout: Duration) {
    let run_loop = CFRunLoopGetCurrent();
    CFRunLoopAddSource(run_loop, source, mode);
    let started = std::time::Instant::now();
    while read.finished.get().is_none() {
        let left = timeout.saturating_sub(started.elapsed());
        CFRunLoopRunInMode(mode, left.as_secs_f64(), 1);
        if left.is_zero() {
            break;
        }
    }
    CFRunLoopRemoveSource(run_loop, source, mode);
}

impl Device {
    /// See [`crate::Device::reset_pipes`]. Not implemented on this platform yet.
    pub fn reset_pipes(&mut self) {}

    pub fn reset_endpoint(&mut self, _endpoint: u8) {}

    /// See [`crate::Device::describe_interface`]. Not implemented on this platform yet.
    pub fn describe_interface(&self) -> String {
        format!("endpoints in {:#04x} out {:#04x}", self.ep_in, self.ep_out)
    }

    /// The bulk pipe reference numbers negotiated at open time, as `(in, out)`.
    ///
    /// **These are indices, not endpoint addresses.** IOUSBLib addresses a pipe by a 1-based
    /// reference number, so an interface whose IN endpoint is `0x81` and OUT endpoint is `0x01`
    /// reports `(1, 2)`. See [`crate::Device::endpoints`] for what a caller may assume.
    pub fn endpoints(&self) -> (u8, u8) {
        (self.ep_in, self.ep_out)
    }

    pub fn open(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        unsafe {
            let plugin_id = cfuuid(&ID_CFPLUGIN);
            let dev_user = cfuuid(&ID_DEV_USERCLIENT);
            let intf_user = cfuuid(&ID_INTF_USERCLIENT);
            let dev_iid = CFUUIDBytes { b: ID_DEV_IFACE500 };
            let intf_iid = CFUUIDBytes { b: ID_INTF_IFACE500 };
            let mut refused = None;

            for class in ["IOUSBHostDevice\0", "IOUSBDevice\0"] {
                let matching = IOServiceMatching(class.as_ptr() as *const c_char);
                if matching.is_null() {
                    continue;
                }
                let mut iter: io_iterator_t = 0;
                if IOServiceGetMatchingServices(0, matching, &mut iter) != kIOReturnSuccess {
                    continue;
                }
                loop {
                    let svc = IOIteratorNext(iter);
                    if svc == 0 {
                        break;
                    }
                    let reported = registry_string(svc, "USB Serial Number");
                    if !crate::candidate_satisfies(serial, reported.as_deref()) {
                        IOObjectRelease(svc);
                        continue;
                    }
                    let d = try_device(svc, plugin_id, dev_user, intf_user, dev_iid, intf_iid, vendor_id, product_id, &mut refused);
                    IOObjectRelease(svc);
                    if let Some(d) = d {
                        IOObjectRelease(iter);
                        return Ok(d);
                    }
                }
                IOObjectRelease(iter);
            }
            Err(refused.map_or(Error::NotFound, Error::Os))
        }
    }

    /// Open by VID/PID; the interface GUID is a Windows concept (macOS matches the vendor class-0xFF
    /// interface directly), so it is ignored here.
    pub fn open_guid(_interface_guid: &str, vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        Self::open(vendor_id, product_id, serial)
    }

    /// The IOUSBLib pipe reference number for a USB endpoint address, or [`Error::NotFound`] if this
    /// interface exposes no such bulk endpoint.
    fn pipe_ref(&self, endpoint: u8) -> Result<u8> {
        self.pipes
            .iter()
            .find_map(|&(addr, pipe)| (addr == endpoint).then_some(pipe))
            .ok_or(Error::NotFound)
    }

    /// Sends one bulk OUT packet on a specific endpoint address (see [`crate::Device::write_endpoint`]).
    pub fn write_endpoint(&mut self, endpoint: u8, data: &[u8]) -> Result<()> {
        let pipe = self.pipe_ref(endpoint)?;
        self.write_pipe(pipe, data)
    }

    /// Reads one bulk IN packet from a specific endpoint address (see [`crate::Device::read_endpoint`]).
    pub fn read_endpoint(&mut self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        let pipe = self.pipe_ref(endpoint)?;
        self.read_pipe(pipe, buf, timeout)
    }

    /// See [`crate::Device::poll_packet`].
    pub fn poll_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.poll_pipe(self.ep_in, buf, timeout)
    }

    pub fn write_packet(&mut self, data: &[u8]) -> Result<()> {
        self.write_pipe(self.ep_out, data)
    }

    pub fn read_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.read_pipe(self.ep_in, buf, timeout)
    }

    /// Writes `data` to the pipe with reference `pipe`, within a second.
    fn write_pipe(&mut self, pipe: u8, data: &[u8]) -> Result<()> {
        let status = unsafe {
            ((**self.intf).WritePipeTO)(self.intf as *mut c_void, pipe, data.as_ptr() as *const c_void, data.len() as u32, 1000, 1000)
        };
        if status != kIOReturnSuccess {
            return Err(pipe_failure("WritePipeTO", status));
        }
        Ok(())
    }

    /// Reads one transfer from the pipe with reference `pipe` into `buf`, waiting up to `timeout` for
    /// it. A pipe with nothing to deliver in that time is [`Error::Timeout`], and what the transfer
    /// held by then is gone: IOUSBLib says that when a read returns an error, the buffer holds no
    /// valid data. That suits a request's reply, whose timeout is a failure. A caller that expects to
    /// time out polls instead.
    fn read_pipe(&mut self, pipe: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        if self.reads.iter().any(|(known, reads)| *known == pipe && (reads.outstanding.is_some() || !reads.left.is_empty())) {
            return self.poll_pipe(pipe, buf, timeout);
        }
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let mut size = buf.len() as u32;
        let status = unsafe {
            ((**self.intf).ReadPipeTO)(self.intf as *mut c_void, pipe, buf.as_mut_ptr() as *mut c_void, &mut size, ms, ms)
        };
        if status != kIOReturnSuccess {
            return Err(pipe_failure("ReadPipeTO", status));
        }
        Ok(size as usize)
    }

    /// [`read_pipe`](Self::read_pipe) for a caller that polls: a pipe with nothing to deliver in
    /// `timeout` is [`Error::Timeout`], and a transfer under way is not touched.
    ///
    /// **THE TRANSFER STAYS OUTSTANDING ACROSS A TIMEOUT, AND THE NEXT POLL COLLECTS IT, SO A QUIET
    /// PIPE LOSES NOTHING.** `ReadPipeTO` aborts a transfer that runs out of time and keeps nothing,
    /// so bytes arriving just as a poll times out would be acknowledged to the device and thrown away,
    /// without a sign. This aborts only when the device is closed. It is not how every read goes
    /// because a completion through a run loop costs each round trip more than a synchronous read:
    /// about 2% of a write over a debug probe, measured, for nothing a probe's replies need.
    fn poll_pipe(&mut self, pipe: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.source.is_null() {
            let mut source: CFRunLoopSourceRef = null();
            let status = unsafe { ((**self.intf).CreateInterfaceAsyncEventSource)(self.intf.cast(), &mut source) };
            if status != kIOReturnSuccess || source.is_null() {
                return Err(pipe_failure("CreateInterfaceAsyncEventSource", status));
            }
            self.source = source;
            self.mode = unsafe { CFStringCreateWithCString(null(), c"lamella-usbbulk".as_ptr(), kCFStringEncodingUTF8) };
        }
        let (intf, source, mode) = (self.intf, self.source, self.mode);
        let reads = match self.reads.iter().position(|(known, _)| *known == pipe) {
            Some(index) => &mut self.reads[index].1,
            None => {
                self.reads.push((pipe, PipeRead::default()));
                &mut self.reads.last_mut().expect("just pushed").1
            }
        };
        if !reads.left.is_empty() {
            return Ok(take_left(&mut reads.left, buf));
        }
        let outstanding = match &mut reads.outstanding {
            Some(read) => read,
            empty => {
                let mut read =
                    Box::new(OutstandingRead { buffer: vec![0; buf.len()], finished: Cell::new(None) });
                let at = read.buffer.as_mut_ptr().cast::<c_void>();
                let refcon = (&raw const *read).cast_mut().cast::<c_void>();
                let status = unsafe {
                    ((**intf).ReadPipeAsync)(intf.cast(), pipe, at, buf.len() as u32, read_finished, refcon)
                };
                if status != kIOReturnSuccess {
                    return Err(pipe_failure("ReadPipeAsync", status));
                }
                empty.insert(read)
            }
        };
        unsafe { wait_for(outstanding, source, mode, timeout) };
        let Some((status, delivered)) = outstanding.finished.get() else {
            return Err(Error::Timeout);
        };
        let finished = reads.outstanding.take().expect("this read was outstanding");
        let delivered = delivered.min(finished.buffer.len());
        finished_read(status, &finished.buffer[..delivered], buf, &mut reads.left)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            for (pipe, reads) in &mut self.reads {
                if let Some(read) = reads.outstanding.take() {
                    ((**self.intf).AbortPipe)(self.intf as *mut c_void, *pipe);
                    wait_for(&read, self.source, self.mode, Duration::from_secs(1));
                    if read.finished.get().is_none() {
                        std::mem::forget(read);
                    }
                }
            }
            ((**self.intf).USBInterfaceClose)(self.intf as *mut c_void);
            ((**self.intf).Release)(self.intf as *mut c_void);
            ((**self.dev).USBDeviceClose)(self.dev as *mut c_void);
            ((**self.dev).Release)(self.dev as *mut c_void);
            if !self.source.is_null() {
                CFRelease(self.source);
            }
            if !self.mode.is_null() {
                CFRelease(self.mode);
            }
        }
    }
}

/// Find the device's vendor-specific (class 0xFF) interface with bulk IN+OUT pipes, opened.
///
/// **THE INTERFACES ARE RANKED FROM THE REGISTRY BEFORE ANY IS OPENED,** so a device naming CMSIS-DAP on
/// one interface and a bridge on another is driven through the first, whichever the iterator gives first.
unsafe fn open_vendor_interface(
    dev: *mut *mut IOUSBDeviceInterface500,
    plugin_id: CFUUIDRef,
    intf_user: CFUUIDRef,
    intf_iid: CFUUIDBytes,
    refused: &mut Option<String>,
) -> Option<(*mut *mut IOUSBInterfaceInterface500, u8, u8, Vec<(u8, u8)>)> {
    let req = IOUSBFindInterfaceRequest {
        bInterfaceClass: DONT_CARE,
        bInterfaceSubClass: DONT_CARE,
        bInterfaceProtocol: DONT_CARE,
        bAlternateSetting: DONT_CARE,
    };
    let mut iter: io_iterator_t = 0;
    if ((**dev).CreateInterfaceIterator)(dev as *mut c_void, &req, &mut iter) != kIOReturnSuccess {
        return None;
    }
    let mut services = Vec::new();
    loop {
        let svc = IOIteratorNext(iter);
        if svc == 0 {
            break;
        }
        let name = registry_string(svc, "kUSBString");
        let spec_shape = registry_shape(svc).is_some_and(|(subclass, protocol)| crate::is_spec_shape(subclass, protocol));
        let has_pair = registry_u16(svc, "bNumEndpoints").and_then(|count| (count < 2).then_some(false));
        let number = registry_u16(svc, "bInterfaceNumber").unwrap_or(u16::MAX);
        services.push(((crate::open_rank(has_pair, name.as_deref(), spec_shape), number), svc));
    }
    IOObjectRelease(iter);
    services.sort_by_key(|(rank, _)| *rank);
    let walk = crate::first_that_opens(services.iter().map(|(_, svc)| ((), *svc)), |svc| {
        let mut said = None;
        match open_if_it_has_a_bulk_pair(svc, plugin_id, intf_user, intf_iid, &mut said) {
            Some(opened) => crate::Attempt::Opened(opened),
            None => said.map_or(crate::Attempt::Passed, crate::Attempt::Refused),
        }
    });
    for (_, svc) in services {
        IOObjectRelease(svc);
    }
    match walk {
        Ok(found) => found,
        Err(why) => {
            *refused = Some(why);
            None
        }
    }
}

/// The interface behind `svc`, opened, if it is vendor-specific (class 0xFF) with bulk IN+OUT pipes.
unsafe fn open_if_it_has_a_bulk_pair(
    svc: io_service_t,
    plugin_id: CFUUIDRef,
    intf_user: CFUUIDRef,
    intf_iid: CFUUIDBytes,
    refused: &mut Option<String>,
) -> Option<(*mut *mut IOUSBInterfaceInterface500, u8, u8, Vec<(u8, u8)>)> {
    let mut plugin: *mut *mut IOCFPlugInInterface = null_mut();
    let mut score = 0i32;
    if IOCreatePlugInInterfaceForService(svc, intf_user, plugin_id, &mut plugin, &mut score) != kIOReturnSuccess || plugin.is_null() {
        return None;
    }
    let mut raw: *mut c_void = null_mut();
    ((**plugin).QueryInterface)(plugin as *mut c_void, intf_iid, &mut raw);
    IODestroyPlugInInterface(plugin);
    let intf = raw as *mut *mut IOUSBInterfaceInterface500;
    if intf.is_null() {
        return None;
    }
    let mut cls: u8 = 0;
    ((**intf).GetInterfaceClass)(intf as *mut c_void, &mut cls);
    if cls != 0xFF {
        ((**intf).Release)(intf as *mut c_void);
        return None;
    }
    let opened = ((**intf).USBInterfaceOpen)(intf as *mut c_void);
    if opened != kIOReturnSuccess {
        *refused = Some(open_refused("USBInterfaceOpen", opened));
        ((**intf).Release)(intf as *mut c_void);
        return None;
    }
    let (mut subclass, mut protocol) = (0u8, 0u8);
    ((**intf).GetInterfaceSubClass)(intf as *mut c_void, &mut subclass);
    ((**intf).GetInterfaceProtocol)(intf as *mut c_void, &mut protocol);
    let mut n: u8 = 0;
    ((**intf).GetNumEndpoints)(intf as *mut c_void, &mut n);
    let mut pipes: Vec<(u8, u8)> = Vec::new();
    for pipe in 1..=n {
        let (mut dir, mut num, mut tt, mut iv): (u8, u8, u8, u8) = (0, 0, 0, 0);
        let mut mps: u16 = 0;
        if ((**intf).GetPipeProperties)(intf as *mut c_void, pipe, &mut dir, &mut num, &mut tt, &mut mps, &mut iv) == kIOReturnSuccess
            && tt == kUSBBulk
        {
            let addr = if dir == kUSBIn { 0x80 | num } else { num };
            pipes.push((addr, pipe));
        }
    }
    let addresses: Vec<u8> = pipes.iter().map(|(address, _)| *address).collect();
    let (in_address, out_address) = crate::bulk_pair(&addresses, crate::is_spec_shape(subclass, protocol));
    let pipe_of = |wanted: u8| pipes.iter().find(|(address, _)| *address == wanted).map(|(_, pipe)| *pipe);
    match (pipe_of(in_address), pipe_of(out_address)) {
        (Some(ep_in), Some(ep_out)) => Some((intf, ep_in, ep_out, pipes)),
        _ => {
            ((**intf).USBInterfaceClose)(intf as *mut c_void);
            ((**intf).Release)(intf as *mut c_void);
            None
        }
    }
}

/// Open the device behind `svc` if it matches `vendor_id`/`product_id` and exposes a vendor
/// bulk interface.
unsafe fn try_device(
    svc: io_service_t,
    plugin_id: CFUUIDRef,
    dev_user: CFUUIDRef,
    intf_user: CFUUIDRef,
    dev_iid: CFUUIDBytes,
    intf_iid: CFUUIDBytes,
    vendor_id: u16,
    product_id: u16,
    refused: &mut Option<String>,
) -> Option<Device> {
    let mut plugin: *mut *mut IOCFPlugInInterface = null_mut();
    let mut score = 0i32;
    if IOCreatePlugInInterfaceForService(svc, dev_user, plugin_id, &mut plugin, &mut score) != kIOReturnSuccess || plugin.is_null() {
        return None;
    }
    let mut raw: *mut c_void = null_mut();
    ((**plugin).QueryInterface)(plugin as *mut c_void, dev_iid, &mut raw);
    IODestroyPlugInInterface(plugin);
    let dev = raw as *mut *mut IOUSBDeviceInterface500;
    if dev.is_null() {
        return None;
    }

    let mut vid: u16 = 0;
    ((**dev).GetDeviceVendor)(dev as *mut c_void, &mut vid);
    let mut pid: u16 = 0;
    ((**dev).GetDeviceProduct)(dev as *mut c_void, &mut pid);
    if vid != vendor_id || pid != product_id {
        ((**dev).Release)(dev as *mut c_void);
        return None;
    }

    let opened = ((**dev).USBDeviceOpen)(dev as *mut c_void);
    if opened != kIOReturnSuccess {
        *refused = Some(open_refused("USBDeviceOpen", opened));
        ((**dev).Release)(dev as *mut c_void);
        return None;
    }
    configure_once(dev);

    match open_vendor_interface(dev, plugin_id, intf_user, intf_iid, refused) {
        Some((intf, ep_in, ep_out, pipes)) => {
            Some(Device { dev, intf, ep_in, ep_out, pipes, source: null(), mode: null(), reads: Vec::new() })
        }
        None => {
            ((**dev).USBDeviceClose)(dev as *mut c_void);
            ((**dev).Release)(dev as *mut c_void);
            None
        }
    }
}

/// What this device declares of its vendor-specific (class 0xFF) interfaces -- the CMSIS-DAP v2 / Lamella Link
/// bulk shape -- and whether it has more than one interface of any class. Reads the CONFIGURATION DESCRIPTOR the
/// OS cached at enumeration (`GetConfigurationDescriptorPtr`), NOT the live interface nodes: a vendor-only device
/// the OS never configured (no class driver to claim it -- e.g. the Pico's native-USB Lamella Link carrier)
/// exposes NO live interface objects to iterate, but its config descriptor is always present. Opens nothing, so
/// it is safe to probe any device.
unsafe fn vendor_interfaces_declared(dev: *mut *mut IOUSBDeviceInterface500) -> (Vec<crate::VendorInterface>, bool) {
    let mut desc: *const u8 = null();
    if ((**dev).GetConfigurationDescriptorPtr)(dev as *mut c_void, 0, &mut desc) != kIOReturnSuccess || desc.is_null() {
        return (Vec::new(), false);
    }
    let total = usize::from(u16::from_le_bytes([*desc.add(2), *desc.add(3)]));
    crate::vendor_interfaces_of_configuration(std::slice::from_raw_parts(desc, total))
}

/// The device behind `svc` if it exposes a vendor-specific (class 0xFF) interface.
unsafe fn device_info(svc: io_service_t, plugin_id: CFUUIDRef, dev_user: CFUUIDRef, dev_iid: CFUUIDBytes) -> Option<DeviceInfo> {
    if let (Some(vendor_id), Some(product_id), Some((vendor_interfaces, composite))) = (
        registry_u16(svc, "idVendor"),
        registry_u16(svc, "idProduct"),
        vendor_interfaces_in_registry(svc),
    ) {
        return described_device(svc, vendor_id, product_id, vendor_interfaces, composite);
    }

    let mut plugin: *mut *mut IOCFPlugInInterface = null_mut();
    let mut score = 0i32;
    if IOCreatePlugInInterfaceForService(svc, dev_user, plugin_id, &mut plugin, &mut score) != kIOReturnSuccess || plugin.is_null() {
        return None;
    }
    let mut raw: *mut c_void = null_mut();
    ((**plugin).QueryInterface)(plugin as *mut c_void, dev_iid, &mut raw);
    IODestroyPlugInInterface(plugin);
    let dev = raw as *mut *mut IOUSBDeviceInterface500;
    if dev.is_null() {
        return None;
    }
    let mut vid: u16 = 0;
    ((**dev).GetDeviceVendor)(dev as *mut c_void, &mut vid);
    let mut pid: u16 = 0;
    ((**dev).GetDeviceProduct)(dev as *mut c_void, &mut pid);
    let (vendor_interfaces, composite) = vendor_interfaces_declared(dev);
    ((**dev).Release)(dev as *mut c_void);
    described_device(svc, vid, pid, vendor_interfaces, composite)
}

/// The listing's entry for the device behind `svc`, with the interfaces found, if any is vendor-specific.
unsafe fn described_device(
    svc: io_service_t,
    vendor_id: u16,
    product_id: u16,
    vendor_interfaces: Vec<crate::VendorInterface>,
    composite: bool,
) -> Option<DeviceInfo> {
    if vendor_interfaces.is_empty() {
        return None;
    }
    let product = registry_string(svc, "USB Product Name");
    Some(DeviceInfo {
        vendor_id,
        product_id,
        serial_number: registry_string(svc, "USB Serial Number"),
        interface_name: crate::vendor_interfaces_name(vendor_interfaces.iter().map(|interface| interface.name.clone())),
        vendor_interfaces,
        composite: Some(composite),
        product_is_absent: product.is_none() && registry_u16(svc, "iProduct") == Some(0),
        product,
        port_id: None,
    })
}

pub fn enumerate_guid(_interface_guid: &str) -> Result<Vec<DeviceInfo>> {
    Err(Error::Unsupported)
}

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    unsafe {
        let plugin_id = cfuuid(&ID_CFPLUGIN);
        let dev_user = cfuuid(&ID_DEV_USERCLIENT);
        let dev_iid = CFUUIDBytes { b: ID_DEV_IFACE500 };
        for class in ["IOUSBHostDevice\0", "IOUSBDevice\0"] {
            let matching = IOServiceMatching(class.as_ptr() as *const c_char);
            if matching.is_null() {
                continue;
            }
            let mut iter: io_iterator_t = 0;
            if IOServiceGetMatchingServices(0, matching, &mut iter) != kIOReturnSuccess {
                continue;
            }
            loop {
                let svc = IOIteratorNext(iter);
                if svc == 0 {
                    break;
                }
                if let Some(info) = device_info(svc, plugin_id, dev_user, dev_iid) {
                    out.push(info);
                }
                IOObjectRelease(svc);
            }
            IOObjectRelease(iter);
            if !out.is_empty() {
                break;
            }
        }
    }
    Ok(out)
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

/// The device user client for the device behind `svc`, or `None` where one cannot be created.
/// Creating it opens nothing.
unsafe fn device_interface(
    svc: io_service_t,
    plugin_id: CFUUIDRef,
    dev_user: CFUUIDRef,
    dev_iid: CFUUIDBytes,
) -> Option<*mut *mut IOUSBDeviceInterface500> {
    let mut plugin: *mut *mut IOCFPlugInInterface = null_mut();
    let mut score = 0i32;
    if IOCreatePlugInInterfaceForService(svc, dev_user, plugin_id, &mut plugin, &mut score) != kIOReturnSuccess || plugin.is_null() {
        return None;
    }
    let mut raw: *mut c_void = null_mut();
    ((**plugin).QueryInterface)(plugin as *mut c_void, dev_iid, &mut raw);
    IODestroyPlugInInterface(plugin);
    let dev = raw as *mut *mut IOUSBDeviceInterface500;
    (!dev.is_null()).then_some(dev)
}

/// Selects configuration 1 unless the device already has it.
///
unsafe fn configure_once(dev: *mut *mut IOUSBDeviceInterface500) {
    let mut current: u8 = 0;
    let known = ((**dev).GetConfiguration)(dev as *mut c_void, &mut current) == kIOReturnSuccess;
    if !known || current != 1 {
        ((**dev).SetConfiguration)(dev as *mut c_void, 1);
    }
}

/// The first interface of `class` in the device's first configuration descriptor, as its number and
/// string index.
unsafe fn configuration_interface(
    dev: *mut *mut IOUSBDeviceInterface500,
    class: crate::InterfaceClass,
) -> Option<(u8, u8)> {
    let mut desc: *const u8 = null();
    if ((**dev).GetConfigurationDescriptorPtr)(dev as *mut c_void, 0, &mut desc) != kIOReturnSuccess || desc.is_null() {
        return None;
    }
    let total = usize::from(u16::from_le_bytes([*desc.add(2), *desc.add(3)]));
    crate::interface_of_class(std::slice::from_raw_parts(desc, total), class)
}

/// The first interface entry of the device behind `svc` in the IORegistry whose class is `class`, as
/// its number and name. `None` when no interface entry published a class at all: the registry did
/// not say, which is not a no.
unsafe fn registry_interface(
    svc: io_object_t,
    class: crate::InterfaceClass,
) -> Option<Option<(u8, Option<String>)>> {
    let plane = std::ffi::CString::new("IOService").ok()?;
    let mut iter: io_iterator_t = 0;
    if IORegistryEntryGetChildIterator(svc, plane.as_ptr(), &mut iter) != kIOReturnSuccess {
        return None;
    }
    let mut saw_any = false;
    let mut found = None;
    loop {
        let child = IOIteratorNext(iter);
        if child == 0 {
            break;
        }
        if let Some(interface_class) = registry_u16(child, "bInterfaceClass") {
            saw_any = true;
            if found.is_none()
                && interface_class == u16::from(class.class)
                && registry_u16(child, "bInterfaceSubClass") == Some(u16::from(class.subclass))
                && registry_u16(child, "bInterfaceProtocol") == Some(u16::from(class.protocol))
            {
                found = registry_u16(child, "bInterfaceNumber")
                    .and_then(|number| u8::try_from(number).ok())
                    .map(|number| (number, registry_string(child, "kUSBString")));
            }
        }
        IOObjectRelease(child);
    }
    IOObjectRelease(iter);
    saw_any.then_some(found)
}

/// What [`crate::enumerate_class`] lists for the device behind `svc`, or `None` when it has no
/// interface of `class`.
unsafe fn class_interface_info(
    svc: io_service_t,
    class: crate::InterfaceClass,
    plugin_id: CFUUIDRef,
    dev_user: CFUUIDRef,
    dev_iid: CFUUIDBytes,
) -> Option<crate::InterfaceInfo> {
    let published = (
        registry_u16(svc, "idVendor"),
        registry_u16(svc, "idProduct"),
        registry_interface(svc, class),
    );
    let (vendor_id, product_id, interface_number, interface_name) = match published {
        (Some(vendor_id), Some(product_id), Some(found)) => {
            let (number, name) = found?;
            (vendor_id, product_id, number, name)
        }
        _ => {
            let dev = device_interface(svc, plugin_id, dev_user, dev_iid)?;
            let mut vendor_id: u16 = 0;
            ((**dev).GetDeviceVendor)(dev as *mut c_void, &mut vendor_id);
            let mut product_id: u16 = 0;
            ((**dev).GetDeviceProduct)(dev as *mut c_void, &mut product_id);
            let found = configuration_interface(dev, class);
            ((**dev).Release)(dev as *mut c_void);
            (vendor_id, product_id, found?.0, None)
        }
    };
    Some(crate::InterfaceInfo {
        vendor_id,
        product_id,
        serial_number: registry_string(svc, "USB Serial Number"),
        product: registry_string(svc, "USB Product Name"),
        interface_number,
        interface_name,
    })
}

/// See [`crate::enumerate_class`].
pub fn enumerate_class(class: crate::InterfaceClass) -> Result<Vec<crate::InterfaceInfo>> {
    let mut out = Vec::new();
    unsafe {
        let plugin_id = cfuuid(&ID_CFPLUGIN);
        let dev_user = cfuuid(&ID_DEV_USERCLIENT);
        let dev_iid = CFUUIDBytes { b: ID_DEV_IFACE500 };
        for service_class in ["IOUSBHostDevice\0", "IOUSBDevice\0"] {
            let matching = IOServiceMatching(service_class.as_ptr() as *const c_char);
            if matching.is_null() {
                continue;
            }
            let mut iter: io_iterator_t = 0;
            if IOServiceGetMatchingServices(0, matching, &mut iter) != kIOReturnSuccess {
                continue;
            }
            loop {
                let svc = IOIteratorNext(iter);
                if svc == 0 {
                    break;
                }
                if let Some(info) = class_interface_info(svc, class, plugin_id, dev_user, dev_iid) {
                    out.push(info);
                }
                IOObjectRelease(svc);
            }
            IOObjectRelease(iter);
            if !out.is_empty() {
                break;
            }
        }
    }
    Ok(out)
}

/// An opened device, one of whose interfaces is driven through the default control pipe by
/// `DeviceRequestTO`.
pub struct ControlInterface {
    dev: *mut *mut IOUSBDeviceInterface500,
    interface: u8,
}

impl ControlInterface {
    /// See [`crate::ControlInterface::open`].
    pub fn open(
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
        class: crate::InterfaceClass,
    ) -> Result<Self> {
        unsafe {
            let plugin_id = cfuuid(&ID_CFPLUGIN);
            let dev_user = cfuuid(&ID_DEV_USERCLIENT);
            let dev_iid = CFUUIDBytes { b: ID_DEV_IFACE500 };
            for service_class in ["IOUSBHostDevice\0", "IOUSBDevice\0"] {
                let matching = IOServiceMatching(service_class.as_ptr() as *const c_char);
                if matching.is_null() {
                    continue;
                }
                let mut iter: io_iterator_t = 0;
                if IOServiceGetMatchingServices(0, matching, &mut iter) != kIOReturnSuccess {
                    continue;
                }
                loop {
                    let svc = IOIteratorNext(iter);
                    if svc == 0 {
                        break;
                    }
                    let reported = registry_string(svc, "USB Serial Number");
                    let opened = if crate::serial_is(serial, reported.as_deref()) {
                        open_control_interface(svc, plugin_id, dev_user, dev_iid, vendor_id, product_id, class)
                    } else {
                        None
                    };
                    IOObjectRelease(svc);
                    if let Some(opened) = opened {
                        IOObjectRelease(iter);
                        return Ok(opened);
                    }
                }
                IOObjectRelease(iter);
            }
        }
        Err(Error::NotFound)
    }

    /// See [`crate::ControlInterface::interface_number`].
    pub fn interface_number(&self) -> u8 {
        self.interface
    }

    /// See [`crate::ControlInterface::control_in`].
    pub fn control_in(&mut self, setup: crate::Setup, buffer: &mut [u8], timeout_ms: u32) -> Result<usize> {
        self.request(setup, buffer.as_mut_ptr().cast(), timeout_ms)
    }

    /// See [`crate::ControlInterface::control_out`].
    pub fn control_out(&mut self, setup: crate::Setup, data: &[u8], timeout_ms: u32) -> Result<usize> {
        self.request(setup, data.as_ptr().cast_mut().cast(), timeout_ms)
    }

    /// One `DeviceRequestTO`, whose `wLenDone` is the number of bytes transferred (`USB.h`).
    fn request(&mut self, setup: crate::Setup, data: *mut c_void, timeout_ms: u32) -> Result<usize> {
        let mut request = IOUSBDevRequestTO {
            bmRequestType: setup.request_type,
            bRequest: setup.request,
            wValue: setup.value,
            wIndex: setup.index,
            wLength: setup.length,
            pData: data,
            wLenDone: 0,
            noDataTimeout: timeout_ms,
            completionTimeout: timeout_ms,
        };
        let status = unsafe { ((**self.dev).DeviceRequestTO)(self.dev as *mut c_void, &mut request) };
        if status == kIOReturnSuccess {
            Ok(request.wLenDone as usize)
        } else if is_timeout(status) {
            Err(Error::Timeout)
        } else if status == kIOUSBPipeStalled {
            Err(crate::stalled(setup))
        } else {
            Err(Error::Os(format!("DeviceRequestTO returned {:#010x}", status as u32)))
        }
    }
}

impl Drop for ControlInterface {
    fn drop(&mut self) {
        unsafe {
            ((**self.dev).USBDeviceClose)(self.dev as *mut c_void);
            ((**self.dev).Release)(self.dev as *mut c_void);
        }
    }
}

/// Opens the device behind `svc` for `DeviceRequestTO`, configured, if it is `vendor_id` and
/// `product_id` and has an interface of `class`.
unsafe fn open_control_interface(
    svc: io_service_t,
    plugin_id: CFUUIDRef,
    dev_user: CFUUIDRef,
    dev_iid: CFUUIDBytes,
    vendor_id: u16,
    product_id: u16,
    class: crate::InterfaceClass,
) -> Option<ControlInterface> {
    let dev = device_interface(svc, plugin_id, dev_user, dev_iid)?;
    let mut vid: u16 = 0;
    ((**dev).GetDeviceVendor)(dev as *mut c_void, &mut vid);
    let mut pid: u16 = 0;
    ((**dev).GetDeviceProduct)(dev as *mut c_void, &mut pid);
    let found = if vid == vendor_id && pid == product_id { configuration_interface(dev, class) } else { None };
    let Some((interface, _)) = found else {
        ((**dev).Release)(dev as *mut c_void);
        return None;
    };
    if ((**dev).USBDeviceOpen)(dev as *mut c_void) != kIOReturnSuccess {
        ((**dev).Release)(dev as *mut c_void);
        return None;
    }
    configure_once(dev);
    Some(ControlInterface { dev, interface })
}
