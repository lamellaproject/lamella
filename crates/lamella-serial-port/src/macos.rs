//! The attached serial ports on macOS, read from the I/O Registry.
#![allow(non_camel_case_types)]

use std::ffi::{CStr, CString, c_char, c_void};
use std::ptr::null;

use crate::{Error, PortInfo, UsbIdentity};

type kern_return_t = i32;
type mach_port_t = u32;
type io_object_t = mach_port_t;
type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFAllocatorRef = *const c_void;
type CFIndex = isize;
type CFTypeID = usize;
type CFNumberType = CFIndex;
type CFStringEncoding = u32;
type Boolean = u8;

const KERN_SUCCESS: kern_return_t = 0;
/// `kIOMainPortDefault`: "a synonym for NULL", the default main port.
const MAIN_PORT_DEFAULT: mach_port_t = 0;
/// `kCFStringEncodingUTF8`.
const STRING_ENCODING_UTF8: CFStringEncoding = 0x0800_0100;
/// `kCFNumberSInt32Type`.
const NUMBER_SINT32: CFNumberType = 3;
/// The keys a USB device's product string is read from, in order: the value of IOUSBHost's
/// `kUSBHostDevicePropertyProductString`, then the value of USBSpec.h's older `kUSBProductString`.
const PRODUCT_KEYS: [&str; 2] = ["kUSBProductString", "USB Product Name"];
/// The keys a USB device's serial number is read from, in order: the value of IOUSBHost's
/// `kUSBHostDevicePropertySerialNumberString`, then the value of USBSpec.h's older
/// `kUSBSerialNumberString`.
const SERIAL_NUMBER_KEYS: [&str; 2] = ["kUSBSerialNumberString", "USB Serial Number"];

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        alloc: CFAllocatorRef,
        c_str: *const c_char,
        encoding: CFStringEncoding,
    ) -> CFStringRef;
    fn CFStringGetCString(
        the_string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: CFStringEncoding,
    ) -> Boolean;
    fn CFStringGetTypeID() -> CFTypeID;
    fn CFNumberGetValue(number: CFTypeRef, the_type: CFNumberType, value: *mut c_void) -> Boolean;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFGetTypeID(cf: CFTypeRef) -> CFTypeID;
    fn CFRelease(cf: CFTypeRef);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFDictionaryRef;
    fn IOServiceGetMatchingServices(
        main_port: mach_port_t,
        matching: CFDictionaryRef,
        existing: *mut io_object_t,
    ) -> kern_return_t;
    fn IOIteratorNext(iterator: io_object_t) -> io_object_t;
    fn IOObjectRelease(object: io_object_t) -> kern_return_t;
    fn IOObjectConformsTo(object: io_object_t, class_name: *const c_char) -> libc::boolean_t;
    fn IORegistryEntryCreateCFProperty(
        entry: io_object_t,
        key: CFStringRef,
        allocator: CFAllocatorRef,
        options: u32,
    ) -> CFTypeRef;
    fn IORegistryEntryGetParentEntry(
        entry: io_object_t,
        plane: *const c_char,
        parent: *mut io_object_t,
    ) -> kern_return_t;
}

/// An I/O Kit object reference, released when dropped.
struct Object(io_object_t);

impl Drop for Object {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe { IOObjectRelease(self.0) };
        }
    }
}

/// A CoreFoundation object reference, released when dropped.
struct Owned(CFTypeRef);

impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

/// The property `key` of `entry`, or `None` when the entry has none.
fn property(entry: io_object_t, key: &str) -> Option<Owned> {
    let key = CString::new(key).ok()?;
    let key = Owned(unsafe { CFStringCreateWithCString(null(), key.as_ptr(), STRING_ENCODING_UTF8) });
    if key.0.is_null() {
        return None;
    }
    let value = Owned(unsafe { IORegistryEntryCreateCFProperty(entry, key.0, null(), 0) });
    (!value.0.is_null()).then_some(value)
}

/// A string property of `entry`, as the device reported it, or `None` when it is absent, empty or
/// not a string.
///
/// Nothing is trimmed: the Linux listing takes off only the newline sysfs appends to an attribute,
/// so a device whose product string has spaces at an end reads the same on both.
fn string_property(entry: io_object_t, key: &str) -> Option<String> {
    let value = property(entry, key)?;
    if unsafe { CFGetTypeID(value.0) } != unsafe { CFStringGetTypeID() } {
        return None;
    }
    let mut buffer = [0 as c_char; 1024];
    let size = CFIndex::try_from(buffer.len()).unwrap_or(CFIndex::MAX);
    if unsafe { CFStringGetCString(value.0, buffer.as_mut_ptr(), size, STRING_ENCODING_UTF8) } == 0 {
        return None;
    }
    let text = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy().into_owned();
    (!text.is_empty()).then_some(text)
}

/// The string a USB device `entry` has under the first of `keys` it has one under.
fn usb_string(entry: io_object_t, keys: [&str; 2]) -> Option<String> {
    keys.into_iter().find_map(|key| string_property(entry, key))
}

/// A 16-bit numeric property of `entry`, such as `idVendor`.
fn id_property(entry: io_object_t, key: &str) -> Option<u16> {
    let value = property(entry, key)?;
    if unsafe { CFGetTypeID(value.0) } != unsafe { CFNumberGetTypeID() } {
        return None;
    }
    let mut raw: i32 = 0;
    if unsafe { CFNumberGetValue(value.0, NUMBER_SINT32, (&raw mut raw).cast()) } == 0 {
        return None;
    }
    u16::try_from(raw).ok()
}

/// Whether `entry` is an instance of `class` or of a subclass of it.
fn conforms_to(entry: io_object_t, class: &CStr) -> bool {
    unsafe { IOObjectConformsTo(entry, class.as_ptr()) != 0 }
}

/// Lists the serial ports in the I/O Registry.
pub(crate) fn list() -> Result<Vec<PortInfo>, Error> {
    let matching = unsafe { IOServiceMatching(c"IOSerialBSDClient".as_ptr()) };
    if matching.is_null() {
        return Err(Error::List { what: "IOServiceMatching", code: 0 });
    }
    let mut iterator: io_object_t = 0;
    let status = unsafe { IOServiceGetMatchingServices(MAIN_PORT_DEFAULT, matching, &mut iterator) };
    if status != KERN_SUCCESS {
        return Err(Error::List { what: "IOServiceGetMatchingServices", code: status.cast_unsigned() });
    }
    let iterator = Object(iterator);
    let mut ports = Vec::new();
    loop {
        let service = Object(unsafe { IOIteratorNext(iterator.0) });
        if service.0 == 0 {
            break;
        }
        let Some(name) = string_property(service.0, "IOCalloutDevice") else { continue };
        ports.push(PortInfo { name, usb: usb_device(service.0) });
    }
    Ok(ports)
}

/// The USB device the serial port `service` belongs to: the first parent in the IOService plane that
/// is a USB device, with every property read from it alone.
fn usb_device(service: io_object_t) -> Option<UsbIdentity> {
    let mut current: Option<Object> = None;
    for _ in 0..16 {
        let entry = current.as_ref().map_or(service, |object| object.0);
        let mut parent: io_object_t = 0;
        if unsafe { IORegistryEntryGetParentEntry(entry, c"IOService".as_ptr(), &mut parent) }
            != KERN_SUCCESS
        {
            return None;
        }
        let parent = Object(parent);
        if conforms_to(parent.0, c"IOUSBHostDevice") || conforms_to(parent.0, c"IOUSBDevice") {
            return Some(UsbIdentity {
                vendor_id: id_property(parent.0, "idVendor")?,
                product_id: id_property(parent.0, "idProduct")?,
                serial_number: usb_string(parent.0, SERIAL_NUMBER_KEYS),
                product: usb_string(parent.0, PRODUCT_KEYS),
            });
        }
        current = Some(parent);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The keys read first are the ones this system publishes.** Every USB device in the registry
    /// with a product or serial number string under the older key has one under the key read first,
    /// so a misspelled key fails here rather than reading every device through the older key. What
    /// the registry holds depends on the machine, and one with no USB device checks nothing.
    #[test]
    fn every_usb_device_with_an_older_string_has_one_under_the_key_read_first() {
        let matching = unsafe { IOServiceMatching(c"IOUSBHostDevice".as_ptr()) };
        assert!(!matching.is_null(), "IOServiceMatching");
        let mut iterator: io_object_t = 0;
        let status = unsafe { IOServiceGetMatchingServices(MAIN_PORT_DEFAULT, matching, &mut iterator) };
        assert_eq!(status, KERN_SUCCESS, "IOServiceGetMatchingServices");
        let iterator = Object(iterator);
        let mut checked = 0;
        loop {
            let device = Object(unsafe { IOIteratorNext(iterator.0) });
            if device.0 == 0 {
                break;
            }
            for [first, older] in [PRODUCT_KEYS, SERIAL_NUMBER_KEYS] {
                if let Some(text) = string_property(device.0, older) {
                    assert!(string_property(device.0, first).is_some(), "{text:?} only under {older:?}");
                    checked += 1;
                }
            }
        }
        println!("{checked} strings checked");
    }
}
