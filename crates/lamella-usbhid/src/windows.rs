//! Windows HID backend, against the Win32 SetupAPI + HID.dll via the `windows-sys` bindings.

#![allow(unsafe_op_in_unsafe_fn)]

use crate::{DeviceInfo, Error, Result};
use std::mem;
use std::ptr;
use std::time::Duration;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_DevNode_PropertyW, CM_Get_Device_IDW, CM_Get_Device_Interface_PropertyW, CM_Get_Parent,
    CM_LOCATE_DEVNODE_NORMAL, CM_Locate_DevNodeW, CR_NO_SUCH_DEVNODE, CR_NO_SUCH_VALUE, CR_SUCCESS,
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, MAX_DEVICE_ID_LEN, SP_DEVICE_INTERFACE_DATA,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, SetupDiGetDeviceInterfacePropertyW,
};
use windows_sys::Win32::Devices::Properties::{
    DEVPKEY_Device_BusReportedDeviceDesc, DEVPKEY_Device_InstanceId, DEVPROPTYPE,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FreePreparsedData, HidD_GetAttributes,
    HidD_GetHidGuid, HidD_GetPreparsedData, HidD_GetProductString, HidD_GetSerialNumberString,
    HidP_GetCaps, PHIDP_PREPARSED_DATA,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, DEVPROPKEY, ERROR_IO_PENDING, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::core::GUID;

const GENERIC_READ: u32 = 0x8000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

/// The HID device-interface class GUID.
unsafe fn hid_guid() -> GUID {
    let mut guid: GUID = mem::zeroed();
    HidD_GetHidGuid(&mut guid);
    guid
}

/// The device path of one enumerated interface (a NUL-terminated wide string for `CreateFileW`).
unsafe fn device_path(
    info_set: HDEVINFO,
    iface: *const SP_DEVICE_INTERFACE_DATA,
) -> Option<Vec<u16>> {
    let mut required: u32 = 0;
    SetupDiGetDeviceInterfaceDetailW(
        info_set,
        iface,
        ptr::null_mut(),
        0,
        &mut required,
        ptr::null_mut(),
    );
    if required < 6 {
        return None;
    }
    let mut buffer = vec![0u8; required as usize];
    let cbsize: u32 = if cfg!(target_pointer_width = "64") {
        8
    } else {
        6
    };
    ptr::write_unaligned(buffer.as_mut_ptr().cast::<u32>(), cbsize);
    if SetupDiGetDeviceInterfaceDetailW(
        info_set,
        iface,
        buffer.as_mut_ptr().cast(),
        required,
        ptr::null_mut(),
        ptr::null_mut(),
    ) == 0
    {
        return None;
    }
    let mut path: Vec<u16> = buffer[4..]
        .chunks_exact(2)
        .map(|c| u16::from_ne_bytes([c[0], c[1]]))
        .take_while(|&w| w != 0)
        .collect();
    path.push(0);
    Some(path)
}

/// A HID string descriptor (product or serial), if the device reports one.
unsafe fn hid_string(handle: HANDLE, serial: bool) -> Option<String> {
    let mut buf = [0u16; 128];
    let bytes = (buf.len() * 2) as u32;
    let ok = if serial {
        HidD_GetSerialNumberString(handle, buf.as_mut_ptr().cast(), bytes)
    } else {
        HidD_GetProductString(handle, buf.as_mut_ptr().cast(), bytes)
    };
    if !ok {
        return None;
    }
    let len = buf.iter().position(|&w| w == 0).unwrap_or(buf.len());
    (len != 0).then(|| String::from_utf16_lossy(&buf[..len]))
}

/// Visits every present HID interface, calling `visit` with its path and a read-only query handle.
/// The HID usage page and usage of one device interface, from the PnP tree.
///
/// **THE POINT IS THAT THIS NEEDS NO HANDLE.** `HidP_GetCaps` answers the same question and wants
/// an open device, and opening a debug probe's interface can make it re-enumerate -- which asserts
/// NRST and resets the board wired to it. Windows already recorded these two numbers when the
/// device arrived, so a listing can tell a CMSIS-DAP interface from a keyboard's media-key
/// collection without touching either.
///
/// `DEVPKEY_DeviceInterface_HID_UsagePage` and `..._UsageId`, both `DEVPROP_TYPE_UINT16`.
unsafe fn interface_usage(
    info_set: HDEVINFO,
    iface: *const SP_DEVICE_INTERFACE_DATA,
) -> (Option<u16>, Option<u16>) {
    const HID_FMTID: GUID = GUID {
        data1: 0xCBF3_8310,
        data2: 0x4A17,
        data3: 0x4310,
        data4: [0xA1, 0xEB, 0x24, 0x7F, 0x0B, 0x67, 0x59, 0x3B],
    };
    const USAGE_PAGE: DEVPROPKEY = DEVPROPKEY { fmtid: HID_FMTID, pid: 2 };
    const USAGE_ID: DEVPROPKEY = DEVPROPKEY { fmtid: HID_FMTID, pid: 3 };
    (
        interface_u16(info_set, iface, &USAGE_PAGE),
        interface_u16(info_set, iface, &USAGE_ID),
    )
}

/// One `UINT16` device-interface property, or `None` where it is absent.
unsafe fn interface_u16(
    info_set: HDEVINFO,
    iface: *const SP_DEVICE_INTERFACE_DATA,
    key: *const DEVPROPKEY,
) -> Option<u16> {
    let mut ty: DEVPROPTYPE = 0;
    let mut value: u16 = 0;
    let mut needed: u32 = 0;
    let ok = SetupDiGetDeviceInterfacePropertyW(
        info_set,
        iface,
        key,
        &mut ty,
        (&raw mut value).cast::<u8>(),
        std::mem::size_of::<u16>() as u32,
        &mut needed,
        0,
    );
    (ok != 0).then_some(value)
}

/// Walks the HID interfaces, opening only those `want` accepts.
///
/// `want` is handed the usage page and usage read from the PnP tree, before any handle exists. A
/// caller that is LISTING passes a filter and never disturbs what it passes over; a caller that is
/// looking for one specific device accepts everything, because it has to read attributes to find it.
unsafe fn for_each_hid(
    want: impl Fn(Option<u16>, Option<u16>) -> bool,
    mut visit: impl FnMut(&[u16], HANDLE),
) -> Result<()> {
    let guid = hid_guid();
    let info_set = SetupDiGetClassDevsW(
        &guid,
        ptr::null(),
        ptr::null_mut(),
        DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
    );
    if info_set == INVALID_HANDLE_VALUE as isize {
        return Err(Error::Os("SetupDiGetClassDevs failed".into()));
    }
    let mut index: u32 = 0;
    loop {
        let mut iface: SP_DEVICE_INTERFACE_DATA = mem::zeroed();
        iface.cbSize = mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        if SetupDiEnumDeviceInterfaces(info_set, ptr::null(), &guid, index, &mut iface) == 0 {
            break;
        }
        index += 1;
        let Some(path) = device_path(info_set, &iface) else {
            continue;
        };
        let (usage_page, usage) = interface_usage(info_set, &iface);
        if !want(usage_page, usage) {
            continue;
        }
        let handle = CreateFileW(
            path.as_ptr(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            0,
            ptr::null_mut(),
        );
        if handle != INVALID_HANDLE_VALUE {
            visit(&path, handle);
            CloseHandle(handle);
        }
    }
    SetupDiDestroyDeviceInfoList(info_set);
    Ok(())
}

/// Reads a device's HID attributes (vendor/product id), or `None` if the query fails.
unsafe fn attributes(handle: HANDLE) -> Option<HIDD_ATTRIBUTES> {
    let mut attrs: HIDD_ATTRIBUTES = mem::zeroed();
    attrs.Size = mem::size_of::<HIDD_ATTRIBUTES>() as u32;
    HidD_GetAttributes(handle, &mut attrs).then_some(attrs)
}

/// The top-level HID usage and the report byte-lengths (each including the leading report-id byte),
/// from the device's preparsed report descriptor. The report lengths are the exact sizes
/// `ReadFile`/`WriteFile` require (a CMSIS-DAP probe is 64 or 512 bytes); the usage is its selection
/// signature (CMSIS-DAP v1 is the vendor-defined page `0xFF00`, usage `0x01`).
struct HidCaps {
    usage_page: u16,
    usage: u16,
    input_len: usize,
    output_len: usize,
}

unsafe fn hid_caps(handle: HANDLE) -> Option<HidCaps> {
    let mut preparsed: PHIDP_PREPARSED_DATA = mem::zeroed();
    if !HidD_GetPreparsedData(handle, &mut preparsed) {
        return None;
    }
    let mut caps: HIDP_CAPS = mem::zeroed();
    let status = HidP_GetCaps(preparsed, &mut caps);
    HidD_FreePreparsedData(preparsed);
    (status == HIDP_STATUS_SUCCESS).then_some(HidCaps {
        usage_page: caps.UsagePage,
        usage: caps.Usage,
        input_len: caps.InputReportByteLength as usize,
        output_len: caps.OutputReportByteLength as usize,
    })
}

/// The reopen id ([`DeviceInfo::id`]) for a device-interface path: the NUL-terminated wide string as
/// a `String`. Windows device paths are ASCII, so this round-trips losslessly through
/// [`Device::open_id`], which re-encodes it.
fn path_string(path: &[u16]) -> String {
    let end = path.iter().position(|&w| w == 0).unwrap_or(path.len());
    String::from_utf16_lossy(&path[..end])
}

/// What the device tree says about the USB device above a HID interface, read with no handle.
#[derive(Default)]
struct UsbContext {
    on_usb: Option<bool>,
    interface_name: Option<String>,
    device_name: Option<String>,
}

/// Which kind of USB node an instance id names.
#[derive(Debug, PartialEq, Eq)]
enum UsbNode {
    /// A whole device of one interface.
    Device,
    /// One interface of a composite device, whose parent is the device.
    Interface,
}

/// The USB node an instance id names, or `None` if it names something else.
fn usb_node(instance_id: &str) -> Option<UsbNode> {
    let id = instance_id.to_ascii_uppercase();
    id.starts_with("USB\\").then(|| if id.contains("&MI_") { UsbNode::Interface } else { UsbNode::Device })
}

/// Reads a HID interface's USB context by walking up the device tree from it.
///
/// **THE WALK SAYS "NOT ON USB" ONLY FROM THE ROOT**: every other way it can stop has read nothing, and
/// reports nothing. `HidD_GetProductString` gives one string for an interface, and which one depends on
/// the device, so the strings are read here from the nodes that hold them. A device of one interface
/// has no interface node, so Windows holds no name apart from the device's, and its interface reads as
/// having none.
unsafe fn usb_context(path: &[u16]) -> UsbContext {
    const DEEPEST: usize = 32;
    let Some(mut node) = interface_node(path) else {
        return UsbContext::default();
    };
    for _ in 0..DEEPEST {
        let mut parent = 0u32;
        match CM_Get_Parent(&mut parent, node, 0) {
            CR_SUCCESS => node = parent,
            CR_NO_SUCH_DEVNODE => return UsbContext { on_usb: Some(false), ..UsbContext::default() },
            _ => return UsbContext::default(),
        }
        let Some(id) = instance_id(node) else {
            return UsbContext::default();
        };
        match usb_node(&id) {
            None => {}
            Some(UsbNode::Device) => {
                return UsbContext {
                    on_usb: Some(true),
                    interface_name: Some(String::new()),
                    device_name: bus_reported_name(node),
                };
            }
            Some(UsbNode::Interface) => {
                let mut device = 0u32;
                let device_name = if CM_Get_Parent(&mut device, node, 0) == CR_SUCCESS {
                    bus_reported_name(device)
                } else {
                    None
                };
                return UsbContext { on_usb: Some(true), interface_name: bus_reported_name(node), device_name };
            }
        }
    }
    UsbContext::default()
}

/// The device-tree node of a HID device-interface path (NUL-terminated).
unsafe fn interface_node(path: &[u16]) -> Option<u32> {
    let mut ty: DEVPROPTYPE = 0;
    let mut id = [0u16; MAX_DEVICE_ID_LEN as usize + 1];
    let mut len = mem::size_of_val(&id) as u32;
    if CM_Get_Device_Interface_PropertyW(
        path.as_ptr(),
        &DEVPKEY_Device_InstanceId,
        &mut ty,
        id.as_mut_ptr().cast::<u8>(),
        &mut len,
        0,
    ) != CR_SUCCESS
    {
        return None;
    }
    let mut node = 0u32;
    (CM_Locate_DevNodeW(&mut node, id.as_ptr(), CM_LOCATE_DEVNODE_NORMAL) == CR_SUCCESS).then_some(node)
}

/// A device-tree node's instance id.
unsafe fn instance_id(node: u32) -> Option<String> {
    let mut id = [0u16; MAX_DEVICE_ID_LEN as usize + 1];
    (CM_Get_Device_IDW(node, id.as_mut_ptr(), id.len() as u32, 0) == CR_SUCCESS).then(|| path_string(&id))
}

/// `DEVPKEY_Device_BusReportedDeviceDesc`: the string the USB bus reported for a node, cached at
/// enumeration. Empty when the node has none, and `None` when the read failed.
unsafe fn bus_reported_name(node: u32) -> Option<String> {
    let mut ty: DEVPROPTYPE = 0;
    let mut name = [0u16; 128];
    let mut len = mem::size_of_val(&name) as u32;
    match CM_Get_DevNode_PropertyW(
        node,
        &DEVPKEY_Device_BusReportedDeviceDesc,
        &mut ty,
        name.as_mut_ptr().cast::<u8>(),
        &mut len,
        0,
    ) {
        CR_SUCCESS => Some(path_string(&name)),
        CR_NO_SUCH_VALUE => Some(String::new()),
        _ => None,
    }
}

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    unsafe {
        for_each_hid(|page, _| page.is_none_or(|page| page >= 0xFF00), |path, handle| {
            if let Some(attrs) = attributes(handle) {
                let caps = hid_caps(handle);
                let usb = usb_context(path);
                out.push(DeviceInfo {
                    vendor_id: attrs.VendorID,
                    product_id: attrs.ProductID,
                    serial_number: hid_string(handle, true),
                    product: hid_string(handle, false),
                    interface_name: usb.interface_name,
                    device_name: usb.device_name,
                    on_usb: usb.on_usb,
                    id: path_string(path),
                    usage_page: caps.as_ref().map(|c| c.usage_page),
                    usage: caps.as_ref().map(|c| c.usage),
                    input_report_len: caps.as_ref().map(|c| c.input_len as u16),
                    output_report_len: caps.as_ref().map(|c| c.output_len as u16),
                });
            }
        })?;
    }
    Ok(out)
}

pub struct Device {
    handle: HANDLE,
    event: HANDLE,
    in_len: usize,
    out_len: usize,
}

impl Device {
    pub fn open(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        let mut chosen: Option<Vec<u16>> = None;
        unsafe {
            for_each_hid(|_, _| true, |path, handle| {
                if chosen.is_some() {
                    return;
                }
                let matches = attributes(handle).is_some_and(|a| {
                    a.VendorID == vendor_id
                        && a.ProductID == product_id
                        && crate::candidate_satisfies(serial, hid_string(handle, true).as_deref())
                });
                if matches {
                    chosen = Some(path.to_vec());
                }
            })?;
            let Some(path) = chosen else {
                return Err(Error::NotFound);
            };
            Self::open_path(&path)
        }
    }

    pub fn open_id(id: &str) -> Result<Self> {
        let mut path: Vec<u16> = id.encode_utf16().collect();
        path.push(0);
        unsafe { Self::open_path(&path) }
    }

    /// Opens the HID interface at a device-interface `path` (a NUL-terminated wide string) for
    /// overlapped read/write, allocating the completion event and caching the report lengths. The
    /// shared tail of [`open`](Self::open) (path found by vid/pid/serial) and [`open_id`](Self::open_id)
    /// (path from an enumerated id).
    unsafe fn open_path(path: &[u16]) -> Result<Self> {
        let handle = CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            ptr::null_mut(),
        );
        if handle == INVALID_HANDLE_VALUE {
            return Err(Error::Os(format!("CreateFile (read/write) failed: {}", std::io::Error::last_os_error())));
        }
        let event = CreateEventW(ptr::null(), 1, 0, ptr::null());
        if event.is_null() {
            CloseHandle(handle);
            return Err(Error::Os("CreateEvent failed".into()));
        }
        let fallback = 1 + crate::DEFAULT_REPORT_LEN;
        let caps = hid_caps(handle);
        let in_len = crate::report_len_or_default(caps.as_ref().map(|c| c.input_len), fallback);
        let out_len = crate::report_len_or_default(caps.as_ref().map(|c| c.output_len), fallback);
        Ok(Device {
            handle,
            event,
            in_len,
            out_len,
        })
    }

    pub fn write_report(&mut self, data: &[u8]) -> Result<()> {
        unsafe {
            let mut report = vec![0u8; self.out_len];
            let n = data.len().min(self.out_len.saturating_sub(1));
            report[1..1 + n].copy_from_slice(&data[..n]);
            let mut ov: OVERLAPPED = mem::zeroed();
            ov.hEvent = self.event;
            let mut written: u32 = 0;
            if WriteFile(
                self.handle,
                report.as_ptr(),
                report.len() as u32,
                &mut written,
                &mut ov,
            ) == 0
            {
                let err = GetLastError();
                if err != ERROR_IO_PENDING {
                    return Err(Error::Os(format!("WriteFile failed (error {err})")));
                }
                if WaitForSingleObject(self.event, 1000) != WAIT_OBJECT_0 {
                    CancelIo(self.handle);
                }
                if GetOverlappedResult(self.handle, &ov, &mut written, 1) == 0 {
                    return Err(Error::Os("WriteFile did not complete".into()));
                }
            }
            Ok(())
        }
    }

    pub fn read_report(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        unsafe {
            let mut report = vec![0u8; self.in_len];
            let mut ov: OVERLAPPED = mem::zeroed();
            ov.hEvent = self.event;
            let mut read: u32 = 0;
            if ReadFile(
                self.handle,
                report.as_mut_ptr(),
                report.len() as u32,
                &mut read,
                &mut ov,
            ) == 0
            {
                let err = GetLastError();
                if err != ERROR_IO_PENDING {
                    return Err(Error::Os(format!("ReadFile failed (error {err})")));
                }
                let ms = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
                match WaitForSingleObject(self.event, ms) {
                    WAIT_OBJECT_0 => {
                        if GetOverlappedResult(self.handle, &ov, &mut read, 1) == 0 {
                            return Err(Error::Os("ReadFile did not complete".into()));
                        }
                    }
                    WAIT_TIMEOUT => {
                        CancelIo(self.handle);
                        if GetOverlappedResult(self.handle, &ov, &mut read, 1) == 0 {
                            return Err(Error::Timeout);
                        }
                    }
                    _ => {
                        let failure = std::io::Error::last_os_error();
                        CancelIo(self.handle);
                        let _ = GetOverlappedResult(self.handle, &ov, &mut read, 1);
                        return Err(Error::Os(format!("WaitForSingleObject failed: {failure}")));
                    }
                }
            }
            let data_len = (read as usize).saturating_sub(1);
            let n = data_len.min(buf.len());
            buf[..n].copy_from_slice(&report[1..1 + n]);
            Ok(n)
        }
    }
}

#[cfg(test)]
mod tests {
    /// **NO TRANSFER IS LEFT IN THE KERNEL'S HANDS WHEN ITS BUFFER IS FREED:** each `CancelIo` is followed by the
    /// wait for what it cancelled. No test can hold a pending transfer without a device, so this reads the source.
    #[test]
    fn a_cancelled_transfer_is_waited_for() {
        let source = include_str!("windows.rs");
        let device = &source[source.find("impl Device {").unwrap()..source.find("#[cfg(test)]").unwrap()];
        let cancels: Vec<usize> = device.match_indices("CancelIo(self.handle);").map(|(at, _)| at).collect();
        assert_eq!(cancels.len(), 3, "the write's, the timed-out read's and the failed wait's");
        for at in cancels {
            let after = &device[at..device.len().min(at + 400)];
            let wait = after.find("GetOverlappedResult(self.handle, &ov, &mut").unwrap_or_else(|| panic!("no wait after: {after}"));
            assert!(!after[..wait].contains("return "), "it returned first: {after}");
            assert!(after[wait..].lines().next().unwrap().contains(", 1)"), "it did not wait: {after}");
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
            CloseHandle(self.event);
        }
    }
}
