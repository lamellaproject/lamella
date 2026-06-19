//! macOS HID backend, implemented against the IOKit HID Manager (`IOHIDManager`) and
//! CoreFoundation through direct framework FFI.

use crate::{DeviceInfo, Error, Result};
use std::ffi::{CStr, CString, c_char, c_void};
use std::time::{Duration, Instant};

type CFTypeRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFStringRef = *const c_void;
type CFNumberRef = *const c_void;
type CFSetRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFRunLoopRef = *const c_void;
type CFIndex = isize;
type IOHIDManagerRef = *const c_void;
type IOHIDDeviceRef = *const c_void;
type IOReturn = i32;
type IOOptionBits = u32;
type Boolean = u8;
type CFTimeInterval = f64;
type CFStringEncoding = u32;

const KERN_SUCCESS: IOReturn = 0;
const REPORT_TYPE_OUTPUT: u32 = 1;
const CF_NUMBER_SINT32: CFIndex = 3;
const UTF8: CFStringEncoding = 0x0800_0100;
const REPORT_MAX: usize = 64;

/// The C input-report callback: `(context, result, sender, type, reportID, report, length)`.
type InputReportCallback =
    extern "C" fn(*mut c_void, IOReturn, *mut c_void, u32, u32, *mut u8, CFIndex);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFAllocatorDefault: CFAllocatorRef;
    static kCFRunLoopDefaultMode: CFStringRef;
    fn CFRelease(cf: CFTypeRef);
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        cstr: *const c_char,
        encoding: CFStringEncoding,
    ) -> CFStringRef;
    fn CFStringGetCString(
        s: CFStringRef,
        buffer: *mut c_char,
        size: CFIndex,
        encoding: CFStringEncoding,
    ) -> Boolean;
    fn CFNumberGetValue(num: CFNumberRef, the_type: CFIndex, value: *mut c_void) -> Boolean;
    fn CFSetGetCount(set: CFSetRef) -> CFIndex;
    fn CFSetGetValues(set: CFSetRef, values: *mut CFTypeRef);
    fn CFRunLoopGetCurrent() -> CFRunLoopRef;
    fn CFRunLoopRunInMode(
        mode: CFStringRef,
        seconds: CFTimeInterval,
        return_after_source_handled: Boolean,
    ) -> i32;
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOHIDManagerCreate(allocator: CFAllocatorRef, options: IOOptionBits) -> IOHIDManagerRef;
    fn IOHIDManagerSetDeviceMatching(manager: IOHIDManagerRef, matching: CFDictionaryRef);
    fn IOHIDManagerCopyDevices(manager: IOHIDManagerRef) -> CFSetRef;
    fn IOHIDDeviceGetProperty(device: IOHIDDeviceRef, key: CFStringRef) -> CFTypeRef;
    fn IOHIDDeviceOpen(device: IOHIDDeviceRef, options: IOOptionBits) -> IOReturn;
    fn IOHIDDeviceSetReport(
        device: IOHIDDeviceRef,
        report_type: u32,
        report_id: CFIndex,
        report: *const u8,
        length: CFIndex,
    ) -> IOReturn;
    fn IOHIDDeviceRegisterInputReportCallback(
        device: IOHIDDeviceRef,
        report: *mut u8,
        length: CFIndex,
        callback: InputReportCallback,
        context: *mut c_void,
    );
    fn IOHIDDeviceScheduleWithRunLoop(
        device: IOHIDDeviceRef,
        run_loop: CFRunLoopRef,
        mode: CFStringRef,
    );
    fn IOHIDDeviceUnscheduleFromRunLoop(
        device: IOHIDDeviceRef,
        run_loop: CFRunLoopRef,
        mode: CFStringRef,
    );
}

/// Creates an owned CFString from a Rust string; the caller releases it.
unsafe fn cfstr(s: &str) -> CFStringRef {
    let c = CString::new(s).unwrap();
    unsafe { CFStringCreateWithCString(kCFAllocatorDefault, c.as_ptr(), UTF8) }
}

/// Reads a 32-bit integer device property (e.g. `VendorID`), narrowed to `u16`.
unsafe fn device_u16(device: IOHIDDeviceRef, key: &str) -> Option<u16> {
    unsafe {
        let k = cfstr(key);
        let prop = IOHIDDeviceGetProperty(device, k);
        CFRelease(k);
        if prop.is_null() {
            return None;
        }
        let mut value: i32 = 0;
        let ok = CFNumberGetValue(prop, CF_NUMBER_SINT32, (&mut value as *mut i32).cast());
        (ok != 0).then_some(value as u16)
    }
}

/// Reads a string device property (e.g. `SerialNumber`).
unsafe fn device_string(device: IOHIDDeviceRef, key: &str) -> Option<String> {
    unsafe {
        let k = cfstr(key);
        let prop = IOHIDDeviceGetProperty(device, k);
        CFRelease(k);
        if prop.is_null() {
            return None;
        }
        let mut buf = [0 as c_char; 256];
        if CFStringGetCString(prop, buf.as_mut_ptr(), buf.len() as CFIndex, UTF8) == 0 {
            return None;
        }
        Some(CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

/// Collects every device the manager matched into a Rust vector.
unsafe fn manager_devices(manager: IOHIDManagerRef) -> Vec<IOHIDDeviceRef> {
    unsafe {
        let set = IOHIDManagerCopyDevices(manager);
        if set.is_null() {
            return Vec::new();
        }
        let count = CFSetGetCount(set);
        let mut raw = vec![std::ptr::null() as CFTypeRef; count.max(0) as usize];
        CFSetGetValues(set, raw.as_mut_ptr());
        CFRelease(set);
        raw.into_iter().map(|p| p as IOHIDDeviceRef).collect()
    }
}

/// Creates an `IOHIDManager` matching every HID device. The manager is not opened:
/// enumeration only needs the matched set, and the one device we want is opened
/// individually with `IOHIDDeviceOpen` -- so we never touch devices we cannot access.
unsafe fn matching_manager() -> Result<IOHIDManagerRef> {
    unsafe {
        let manager = IOHIDManagerCreate(kCFAllocatorDefault, 0);
        if manager.is_null() {
            return Err(Error::Os("IOHIDManagerCreate failed".into()));
        }
        IOHIDManagerSetDeviceMatching(manager, std::ptr::null());
        Ok(manager)
    }
}

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    unsafe {
        let manager = matching_manager()?;
        let infos = manager_devices(manager)
            .into_iter()
            .filter_map(|d| {
                Some(DeviceInfo {
                    vendor_id: device_u16(d, "VendorID")?,
                    product_id: device_u16(d, "ProductID")?,
                    serial_number: device_string(d, "SerialNumber"),
                    product: device_string(d, "Product"),
                })
            })
            .collect();
        CFRelease(manager);
        Ok(infos)
    }
}

/// The input state shared with the IOKit callback. Boxed so its address is stable: the
/// report buffer is registered with IOKit and `context` points back at this struct.
struct Inner {
    report: [u8; REPORT_MAX],
    received: CFIndex,
}

extern "C" fn on_input_report(
    context: *mut c_void,
    _result: IOReturn,
    _sender: *mut c_void,
    _report_type: u32,
    _report_id: u32,
    _report: *mut u8,
    length: CFIndex,
) {
    unsafe {
        (*context.cast::<Inner>()).received = length;
    }
}

pub struct Device {
    manager: IOHIDManagerRef,
    device: IOHIDDeviceRef,
    inner: Box<Inner>,
}

impl Device {
    pub fn open(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        unsafe {
            let manager = matching_manager()?;
            let chosen = manager_devices(manager).into_iter().find(|&d| {
                device_u16(d, "VendorID") == Some(vendor_id)
                    && device_u16(d, "ProductID") == Some(product_id)
                    && serial.is_none_or(|want| {
                        device_string(d, "SerialNumber").as_deref() == Some(want)
                    })
            });
            let Some(device) = chosen else {
                CFRelease(manager);
                return Err(Error::NotFound);
            };
            if IOHIDDeviceOpen(device, 0) != KERN_SUCCESS {
                CFRelease(manager);
                return Err(Error::Os("IOHIDDeviceOpen failed".into()));
            }
            let mut inner = Box::new(Inner {
                report: [0; REPORT_MAX],
                received: -1,
            });
            let context: *mut c_void = (&mut *inner as *mut Inner).cast();
            IOHIDDeviceRegisterInputReportCallback(
                device,
                inner.report.as_mut_ptr(),
                REPORT_MAX as CFIndex,
                on_input_report,
                context,
            );
            IOHIDDeviceScheduleWithRunLoop(device, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
            Ok(Device {
                manager,
                device,
                inner,
            })
        }
    }

    pub fn write_report(&mut self, data: &[u8]) -> Result<()> {
        unsafe {
            let r = IOHIDDeviceSetReport(
                self.device,
                REPORT_TYPE_OUTPUT,
                0,
                data.as_ptr(),
                data.len() as CFIndex,
            );
            if r == KERN_SUCCESS {
                Ok(())
            } else {
                Err(Error::Os(format!("IOHIDDeviceSetReport failed: {r:#010x}")))
            }
        }
    }

    pub fn read_report(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        self.inner.received = -1;
        let deadline = Instant::now() + timeout;
        unsafe {
            loop {
                if self.inner.received >= 0 {
                    let n = (self.inner.received as usize).min(buf.len());
                    buf[..n].copy_from_slice(&self.inner.report[..n]);
                    return Ok(n);
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(Error::Timeout);
                }
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, remaining.as_secs_f64().min(0.1), 1);
            }
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            IOHIDDeviceUnscheduleFromRunLoop(
                self.device,
                CFRunLoopGetCurrent(),
                kCFRunLoopDefaultMode,
            );
            CFRelease(self.manager);
        }
    }
}
