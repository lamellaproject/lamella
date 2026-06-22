//! macOS CMSIS-DAP v2 (USB bulk) backend via IOKit's IOUSBLib -- the classic COM-style plug-in
//! interface. Hand-rolled FFI to the public IOKit frameworks (no 3rd-party crates, no libc); the
//! vtable layouts are transcribed from IOUSBLib.h (IOUSBDeviceInterface500 / InterfaceInterface500).
//! The v2 sibling of lamella-usbhid's IOKit HID backend: open a probe by VID/PID and exchange raw
//! bulk packets over its vendor interface's bulk IN + OUT pipes (WritePipeTO / ReadPipeTO).
#![allow(non_upper_case_globals, non_snake_case, non_camel_case_types, unsafe_op_in_unsafe_fn)]

use super::{DeviceInfo, Error, Result};
use core::ffi::c_void;
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

#[repr(C)]
#[derive(Clone, Copy)]
struct CFUUIDBytes {
    b: [u8; 16],
}

const kIOReturnSuccess: IOReturn = 0;
const kUSBIn: u8 = 1;
const kUSBOut: u8 = 0;
const kUSBBulk: u8 = 2;
const DONT_CARE: u16 = 0xFFFF;

const PROBE_VIDS: [u16; 4] = [0x2e8a, 0x0d28, 0x1fc9, 0xc251];

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
    GetConfigurationDescriptorPtr: *const c_void,
    GetConfiguration: *const c_void,
    SetConfiguration: extern "C" fn(*mut c_void, u8) -> IOReturn,
    GetBusFrameNumber: *const c_void,
    ResetDevice: *const c_void,
    DeviceRequest: *const c_void,
    DeviceRequestAsync: *const c_void,
    CreateInterfaceIterator: extern "C" fn(*mut c_void, *const IOUSBFindInterfaceRequest, *mut io_iterator_t) -> IOReturn,
}

#[repr(C)]
struct IOUSBInterfaceInterface500 {
    _reserved: *mut c_void,
    QueryInterface: QueryInterfaceFn,
    AddRef: RefFn,
    Release: RefFn,
    CreateInterfaceAsyncEventSource: *const c_void,
    GetInterfaceAsyncEventSource: *const c_void,
    CreateInterfaceAsyncPort: *const c_void,
    GetInterfaceAsyncPort: *const c_void,
    USBInterfaceOpen: extern "C" fn(*mut c_void) -> IOReturn,
    USBInterfaceClose: extern "C" fn(*mut c_void) -> IOReturn,
    GetInterfaceClass: extern "C" fn(*mut c_void, *mut u8) -> IOReturn,
    GetInterfaceSubClass: *const c_void,
    GetInterfaceProtocol: *const c_void,
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
    AbortPipe: *const c_void,
    ResetPipe: *const c_void,
    ClearPipeStall: *const c_void,
    ReadPipe: *const c_void,
    WritePipe: *const c_void,
    ReadPipeAsync: *const c_void,
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
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFDictionaryRef;
    fn IOServiceGetMatchingServices(mainPort: mach_port_t, matching: CFDictionaryRef, existing: *mut io_iterator_t) -> kern_return_t;
    fn IOIteratorNext(iterator: io_iterator_t) -> io_object_t;
    fn IOObjectRelease(object: io_object_t) -> kern_return_t;
    fn IOCreatePlugInInterfaceForService(service: io_service_t, pluginType: CFUUIDRef, interfaceType: CFUUIDRef, theInterface: *mut *mut *mut IOCFPlugInInterface, theScore: *mut i32) -> kern_return_t;
    fn IODestroyPlugInInterface(interface: *mut *mut IOCFPlugInInterface) -> kern_return_t;
}

unsafe fn cfuuid(b: &[u8; 16]) -> CFUUIDRef {
    CFUUIDGetConstantUUIDWithBytes(null(), b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15])
}

pub struct Device {
    dev: *mut *mut IOUSBDeviceInterface500,
    intf: *mut *mut IOUSBInterfaceInterface500,
    ep_in: u8,
    ep_out: u8,
}

impl Device {
    pub fn open(vendor_id: u16, product_id: u16, _serial: Option<&str>) -> Result<Self> {
        unsafe {
            let plugin_id = cfuuid(&ID_CFPLUGIN);
            let dev_user = cfuuid(&ID_DEV_USERCLIENT);
            let intf_user = cfuuid(&ID_INTF_USERCLIENT);
            let dev_iid = CFUUIDBytes { b: ID_DEV_IFACE500 };
            let intf_iid = CFUUIDBytes { b: ID_INTF_IFACE500 };

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
                    let d = try_device(svc, plugin_id, dev_user, intf_user, dev_iid, intf_iid, vendor_id, product_id);
                    IOObjectRelease(svc);
                    if let Some(d) = d {
                        IOObjectRelease(iter);
                        return Ok(d);
                    }
                }
                IOObjectRelease(iter);
            }
        }
        Err(Error::NotFound)
    }

    pub fn write_packet(&mut self, data: &[u8]) -> Result<()> {
        unsafe {
            if ((**self.intf).WritePipeTO)(self.intf as *mut c_void, self.ep_out, data.as_ptr() as *const c_void, data.len() as u32, 1000, 1000) != kIOReturnSuccess {
                return Err(Error::Os("WritePipeTO failed".into()));
            }
            Ok(())
        }
    }

    pub fn read_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        unsafe {
            let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
            let mut size = buf.len() as u32;
            let r = ((**self.intf).ReadPipeTO)(self.intf as *mut c_void, self.ep_in, buf.as_mut_ptr() as *mut c_void, &mut size, ms, ms);
            if r != kIOReturnSuccess {
                return Err(Error::Os("ReadPipeTO failed".into()));
            }
            Ok(size as usize)
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            ((**self.intf).USBInterfaceClose)(self.intf as *mut c_void);
            ((**self.intf).Release)(self.intf as *mut c_void);
            ((**self.dev).USBDeviceClose)(self.dev as *mut c_void);
            ((**self.dev).Release)(self.dev as *mut c_void);
        }
    }
}

/// Find the device's vendor-specific (class 0xFF) interface with bulk IN+OUT pipes, opened.
unsafe fn open_vendor_interface(
    dev: *mut *mut IOUSBDeviceInterface500,
    plugin_id: CFUUIDRef,
    intf_user: CFUUIDRef,
    intf_iid: CFUUIDBytes,
) -> Option<(*mut *mut IOUSBInterfaceInterface500, u8, u8)> {
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
    let mut found = None;
    loop {
        let svc = IOIteratorNext(iter);
        if svc == 0 {
            break;
        }
        let mut plugin: *mut *mut IOCFPlugInInterface = null_mut();
        let mut score = 0i32;
        if IOCreatePlugInInterfaceForService(svc, intf_user, plugin_id, &mut plugin, &mut score) == kIOReturnSuccess && !plugin.is_null() {
            let mut raw: *mut c_void = null_mut();
            ((**plugin).QueryInterface)(plugin as *mut c_void, intf_iid, &mut raw);
            IODestroyPlugInInterface(plugin);
            let intf = raw as *mut *mut IOUSBInterfaceInterface500;
            if !intf.is_null() {
                let mut cls: u8 = 0;
                ((**intf).GetInterfaceClass)(intf as *mut c_void, &mut cls);
                if cls == 0xFF && ((**intf).USBInterfaceOpen)(intf as *mut c_void) == kIOReturnSuccess {
                    let mut n: u8 = 0;
                    ((**intf).GetNumEndpoints)(intf as *mut c_void, &mut n);
                    let (mut ep_in, mut ep_out) = (0u8, 0u8);
                    for pipe in 1..=n {
                        let (mut dir, mut num, mut tt, mut iv): (u8, u8, u8, u8) = (0, 0, 0, 0);
                        let mut mps: u16 = 0;
                        if ((**intf).GetPipeProperties)(intf as *mut c_void, pipe, &mut dir, &mut num, &mut tt, &mut mps, &mut iv) == kIOReturnSuccess
                            && tt == kUSBBulk
                        {
                            if dir == kUSBIn {
                                ep_in = pipe;
                            } else if dir == kUSBOut {
                                ep_out = pipe;
                            }
                        }
                    }
                    if ep_in != 0 && ep_out != 0 {
                        found = Some((intf, ep_in, ep_out));
                    } else {
                        ((**intf).USBInterfaceClose)(intf as *mut c_void);
                        ((**intf).Release)(intf as *mut c_void);
                    }
                } else if !intf.is_null() {
                    ((**intf).Release)(intf as *mut c_void);
                }
            }
        }
        IOObjectRelease(svc);
        if found.is_some() {
            break;
        }
    }
    IOObjectRelease(iter);
    found
}

/// Open the device behind `svc` if it matches `vendor_id`/`product_id` and exposes a vendor bulk interface.
unsafe fn try_device(
    svc: io_service_t,
    plugin_id: CFUUIDRef,
    dev_user: CFUUIDRef,
    intf_user: CFUUIDRef,
    dev_iid: CFUUIDBytes,
    intf_iid: CFUUIDBytes,
    vendor_id: u16,
    product_id: u16,
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

    if ((**dev).USBDeviceOpen)(dev as *mut c_void) != kIOReturnSuccess {
        ((**dev).Release)(dev as *mut c_void);
        return None;
    }
    ((**dev).SetConfiguration)(dev as *mut c_void, 1);

    match open_vendor_interface(dev, plugin_id, intf_user, intf_iid) {
        Some((intf, ep_in, ep_out)) => Some(Device { dev, intf, ep_in, ep_out }),
        None => {
            ((**dev).USBDeviceClose)(dev as *mut c_void);
            ((**dev).Release)(dev as *mut c_void);
            None
        }
    }
}

/// Does this (unopened) device expose a vendor-specific (class 0xFF) interface? That's the CMSIS-DAP
/// v2 signature. Reads interface descriptors only -- opens nothing, so it is safe to probe any device
/// (it won't disturb a v1-only board that happens to share a probe vendor id).
unsafe fn has_vendor_iface(dev: *mut *mut IOUSBDeviceInterface500, plugin_id: CFUUIDRef) -> bool {
    let intf_user = cfuuid(&ID_INTF_USERCLIENT);
    let intf_iid = CFUUIDBytes { b: ID_INTF_IFACE500 };
    let req = IOUSBFindInterfaceRequest {
        bInterfaceClass: DONT_CARE,
        bInterfaceSubClass: DONT_CARE,
        bInterfaceProtocol: DONT_CARE,
        bAlternateSetting: DONT_CARE,
    };
    let mut iter: io_iterator_t = 0;
    if ((**dev).CreateInterfaceIterator)(dev as *mut c_void, &req, &mut iter) != kIOReturnSuccess {
        return false;
    }
    let mut found = false;
    loop {
        let svc = IOIteratorNext(iter);
        if svc == 0 {
            break;
        }
        let mut plugin: *mut *mut IOCFPlugInInterface = null_mut();
        let mut score = 0i32;
        if IOCreatePlugInInterfaceForService(svc, intf_user, plugin_id, &mut plugin, &mut score) == kIOReturnSuccess && !plugin.is_null() {
            let mut raw: *mut c_void = null_mut();
            ((**plugin).QueryInterface)(plugin as *mut c_void, intf_iid, &mut raw);
            IODestroyPlugInInterface(plugin);
            let intf = raw as *mut *mut IOUSBInterfaceInterface500;
            if !intf.is_null() {
                let mut cls: u8 = 0;
                ((**intf).GetInterfaceClass)(intf as *mut c_void, &mut cls);
                ((**intf).Release)(intf as *mut c_void);
                if cls == 0xFF {
                    found = true;
                }
            }
        }
        IOObjectRelease(svc);
        if found {
            break;
        }
    }
    IOObjectRelease(iter);
    found
}

/// The VID/PID of the device behind `svc` if it is a usable CMSIS-DAP v2 probe: a known probe vendor
/// that actually exposes a vendor-specific (class 0xFF) interface.
unsafe fn device_info(svc: io_service_t, plugin_id: CFUUIDRef, dev_user: CFUUIDRef, dev_iid: CFUUIDBytes) -> Option<DeviceInfo> {
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
    let is_v2 = PROBE_VIDS.contains(&vid) && has_vendor_iface(dev, plugin_id);
    ((**dev).Release)(dev as *mut c_void);
    if !is_v2 {
        return None;
    }
    Some(DeviceInfo {
        vendor_id: vid,
        product_id: pid,
        serial_number: None,
        product: None,
    })
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
