//! Windows HID backend, against the Win32 SetupAPI + HID.dll via the `windows-sys` bindings.

#![allow(unsafe_op_in_unsafe_fn)]

use crate::{DeviceInfo, Error, Result};
use std::mem;
use std::ptr;
use std::time::Duration;

use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, HidD_FreePreparsedData, HidD_GetAttributes,
    HidD_GetHidGuid, HidD_GetPreparsedData, HidD_GetProductString, HidD_GetSerialNumberString,
    HidP_GetCaps, PHIDP_PREPARSED_DATA,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, GetLastError, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, ReadFile,
    WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIo, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows_sys::core::GUID;

/// CMSIS-DAP report payload size (excludes the report-id byte).
const REPORT_MAX: usize = 64;
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
    if ok == 0 {
        return None;
    }
    let len = buf.iter().position(|&w| w == 0).unwrap_or(buf.len());
    (len != 0).then(|| String::from_utf16_lossy(&buf[..len]))
}

/// Visits every present HID interface, calling `visit` with its path and a read-only query handle.
unsafe fn for_each_hid(mut visit: impl FnMut(&[u16], HANDLE)) -> Result<()> {
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
    (HidD_GetAttributes(handle, &mut attrs) != 0).then_some(attrs)
}

/// The device's (input, output) HID report byte-lengths, each including the leading report-id
/// byte -- the exact sizes `ReadFile`/`WriteFile` require (a CMSIS-DAP probe is 64 or 512 bytes).
unsafe fn report_lengths(handle: HANDLE) -> Option<(usize, usize)> {
    let mut preparsed: PHIDP_PREPARSED_DATA = mem::zeroed();
    if HidD_GetPreparsedData(handle, &mut preparsed) == 0 {
        return None;
    }
    let mut caps: HIDP_CAPS = mem::zeroed();
    let status = HidP_GetCaps(preparsed, &mut caps);
    HidD_FreePreparsedData(preparsed);
    (status == HIDP_STATUS_SUCCESS).then_some((
        caps.InputReportByteLength as usize,
        caps.OutputReportByteLength as usize,
    ))
}

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    unsafe {
        for_each_hid(|_path, handle| {
            if let Some(attrs) = attributes(handle) {
                out.push(DeviceInfo {
                    vendor_id: attrs.VendorID,
                    product_id: attrs.ProductID,
                    serial_number: hid_string(handle, true),
                    product: hid_string(handle, false),
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
            for_each_hid(|path, handle| {
                if chosen.is_some() {
                    return;
                }
                let matches = attributes(handle).is_some_and(|a| {
                    a.VendorID == vendor_id
                        && a.ProductID == product_id
                        && serial
                            .is_none_or(|want| hid_string(handle, true).as_deref() == Some(want))
                });
                if matches {
                    chosen = Some(path.to_vec());
                }
            })?;
            let Some(path) = chosen else {
                return Err(Error::NotFound);
            };
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
                return Err(Error::Os("CreateFile (read/write) failed".into()));
            }
            let event = CreateEventW(ptr::null(), 1, 0, ptr::null());
            if event.is_null() {
                CloseHandle(handle);
                return Err(Error::Os("CreateEvent failed".into()));
            }
            let (in_len, out_len) =
                report_lengths(handle).unwrap_or((1 + REPORT_MAX, 1 + REPORT_MAX));
            Ok(Device {
                handle,
                event,
                in_len,
                out_len,
            })
        }
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
                if WaitForSingleObject(self.event, 1000) != WAIT_OBJECT_0
                    || GetOverlappedResult(self.handle, &ov, &mut written, 1) == 0
                {
                    CancelIo(self.handle);
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
                        return Err(Error::Timeout);
                    }
                    _ => {
                        CancelIo(self.handle);
                        return Err(Error::Os("WaitForSingleObject failed".into()));
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

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
            CloseHandle(self.event);
        }
    }
}
