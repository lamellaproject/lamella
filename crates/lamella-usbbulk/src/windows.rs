//! Windows CMSIS-DAP v2 (USB bulk) backend via WinUSB (windows-sys) -- the v2 sibling of the HID
//! backend in lamella-usbhid. Finds a probe through its WinUSB device-interface (the CMSIS-DAP v2
//! interface GUID), opens the one matching the requested VID/PID, and exchanges raw bulk packets
//! over its IN/OUT pipes with overlapped I/O. No 3rd-party USB crate.

#![allow(unsafe_op_in_unsafe_fn)]

use super::{DeviceInfo, Error, Result};
use std::ptr::{null, null_mut};
use std::time::Duration;
use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Devices::Usb::{
    UsbdPipeTypeBulk, WinUsb_Free, WinUsb_GetDescriptor, WinUsb_GetOverlappedResult,
    WinUsb_Initialize, WinUsb_QueryInterfaceSettings, WinUsb_QueryPipe, WinUsb_ReadPipe,
    WinUsb_SetPipePolicy, WinUsb_WritePipe, USB_DEVICE_DESCRIPTOR_TYPE, USB_INTERFACE_DESCRIPTOR,
    USB_STRING_DESCRIPTOR_TYPE, WINUSB_INTERFACE_HANDLE, WINUSB_PIPE_INFORMATION,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent};
use windows_sys::Win32::System::IO::OVERLAPPED;

const DAP_V2_GUID: GUID = GUID {
    data1: 0xCDB3B5AD,
    data2: 0x293B,
    data3: 0x4663,
    data4: [0xAA, 0x36, 0x1A, 0xAE, 0x46, 0x46, 0x37, 0x76],
};

/// Device-interface paths (wide, null-terminated) for an interface-class GUID.
unsafe fn iface_paths(guid: &GUID) -> Vec<Vec<u16>> {
    let mut out = Vec::new();
    let hdev = SetupDiGetClassDevsW(guid, null(), null_mut(), DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
    if hdev == INVALID_HANDLE_VALUE as isize {
        return out;
    }
    let mut idx = 0u32;
    loop {
        let mut ifd: SP_DEVICE_INTERFACE_DATA = std::mem::zeroed();
        ifd.cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;
        if SetupDiEnumDeviceInterfaces(hdev, null_mut(), guid, idx, &mut ifd) == 0 {
            break;
        }
        let mut needed = 0u32;
        SetupDiGetDeviceInterfaceDetailW(hdev, &mut ifd, null_mut(), 0, &mut needed, null_mut());
        if needed > 0 {
            let mut buf = vec![0u8; needed as usize];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(hdev, &mut ifd, detail, needed, null_mut(), null_mut()) != 0 {
                let p = (*detail).DevicePath.as_ptr();
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let mut w: Vec<u16> = std::slice::from_raw_parts(p, len).to_vec();
                w.push(0);
                out.push(w);
            }
        }
        idx += 1;
    }
    SetupDiDestroyDeviceInfoList(hdev);
    out
}

/// The VID and PID embedded in a device path ("...VID_XXXX&PID_YYYY...").
fn vid_pid_from_path(path: &[u16]) -> Option<(u16, u16)> {
    let s = String::from_utf16_lossy(path).to_ascii_uppercase();
    let vi = s.find("VID_")? + 4;
    let vid = u16::from_str_radix(s.get(vi..vi + 4)?, 16).ok()?;
    let pi = s.find("PID_")? + 4;
    let pid = u16::from_str_radix(s.get(pi..pi + 4)?, 16).ok()?;
    Some((vid, pid))
}

/// The instance-id segment of a device-interface path -- for a device reporting an
/// iSerialNumber, Windows uses the serial itself: `\\?\usb#vid_1209&pid_0001#SERIAL#{guid}`.
/// (A serial-less device gets a synthesized `a&bcdef&0&1`-style id instead; matching against
/// that is harmless -- it simply never equals a real serial.)
fn instance_id_from_path(path: &[u16]) -> Option<String> {
    let s = String::from_utf16_lossy(path);
    let mut parts = s.split('#');
    let _prefix = parts.next()?;
    let _hardware_id = parts.next()?;
    Some(parts.next()?.to_string())
}

/// Case-insensitive substring match of a requested serial against a device's serial.
fn serial_matches(wanted: &str, actual: &str) -> bool {
    actual.to_ascii_uppercase().contains(&wanted.to_ascii_uppercase())
}

/// One USB string descriptor via WinUSB, decoded to a `String` (descriptor layout: length byte,
/// type byte, UTF-16LE payload). `index` 0 means "none advertised".
unsafe fn string_descriptor(wu: WINUSB_INTERFACE_HANDLE, index: u8) -> Option<String> {
    if index == 0 {
        return None;
    }
    let mut buf = [0u8; 256];
    let mut got = 0u32;
    if WinUsb_GetDescriptor(
        wu,
        USB_STRING_DESCRIPTOR_TYPE as u8,
        index,
        0x0409,
        buf.as_mut_ptr(),
        buf.len() as u32,
        &mut got,
    ) == 0
        || got < 2
    {
        return None;
    }
    let len = (buf[0] as usize).min(got as usize);
    let utf16: Vec<u16> = buf[2..len]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    Some(String::from_utf16_lossy(&utf16))
}

/// The product and serial-number strings of an open WinUSB interface, via its device descriptor
/// (which names the string indices).
unsafe fn product_and_serial(wu: WINUSB_INTERFACE_HANDLE) -> (Option<String>, Option<String>) {
    let mut desc = [0u8; 18];
    let mut got = 0u32;
    if WinUsb_GetDescriptor(
        wu,
        USB_DEVICE_DESCRIPTOR_TYPE as u8,
        0,
        0,
        desc.as_mut_ptr(),
        desc.len() as u32,
        &mut got,
    ) == 0
        || got < 18
    {
        return (None, None);
    }
    (string_descriptor(wu, desc[15]), string_descriptor(wu, desc[16]))
}

/// Parse a `"{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}"` string into a WinUSB interface GUID.
fn guid_from_str(s: &str) -> Option<GUID> {
    let body = s.trim_matches(|c| c == '{' || c == '}');
    let mut it = body.split('-');
    let data1 = u32::from_str_radix(it.next()?, 16).ok()?;
    let data2 = u16::from_str_radix(it.next()?, 16).ok()?;
    let data3 = u16::from_str_radix(it.next()?, 16).ok()?;
    let hi = u16::from_str_radix(it.next()?, 16).ok()?;
    let lo = u64::from_str_radix(it.next()?, 16).ok()?;
    if it.next().is_some() {
        return None;
    }
    let mut data4 = [0u8; 8];
    data4[0] = (hi >> 8) as u8;
    data4[1] = hi as u8;
    data4[2..8].copy_from_slice(&lo.to_be_bytes()[2..8]);
    Some(GUID { data1, data2, data3, data4 })
}

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    unsafe {
        for path in iface_paths(&DAP_V2_GUID) {
            if let Some((vendor_id, product_id)) = vid_pid_from_path(&path) {
                out.push(DeviceInfo {
                    vendor_id,
                    product_id,
                    serial_number: instance_id_from_path(&path),
                    product: None,
                });
            }
        }
    }
    Ok(out)
}

/// List the devices registered under a caller-supplied interface GUID, with each device's
/// product and serial strings where they can be read. The serial always falls back to the
/// device path's instance id (= the serial for a serial-bearing device), so a device that is
/// busy in another host still lists identifiably; the product string needs a brief read-only
/// open of the interface.
pub fn enumerate_guid(interface_guid: &str) -> Result<Vec<DeviceInfo>> {
    let guid = guid_from_str(interface_guid).ok_or_else(|| Error::Os("bad interface GUID".into()))?;
    let mut out = Vec::new();
    unsafe {
        for path in iface_paths(&guid) {
            let Some((vendor_id, product_id)) = vid_pid_from_path(&path) else { continue };
            let mut info = DeviceInfo {
                vendor_id,
                product_id,
                serial_number: instance_id_from_path(&path),
                product: None,
            };
            let h = CreateFileW(
                path.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                null_mut(),
            );
            if h != INVALID_HANDLE_VALUE {
                let mut wu: WINUSB_INTERFACE_HANDLE = null_mut();
                if WinUsb_Initialize(h, &mut wu) != 0 {
                    let (product, serial) = product_and_serial(wu);
                    info.product = product;
                    if serial.is_some() {
                        info.serial_number = serial;
                    }
                    WinUsb_Free(wu);
                }
                CloseHandle(h);
            }
            out.push(info);
        }
    }
    Ok(out)
}

pub struct Device {
    h: HANDLE,
    wu: WINUSB_INTERFACE_HANDLE,
    ev: HANDLE,
    ep_out: u8,
    ep_in: u8,
}

impl Device {
    pub fn open(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        Self::open_with(&DAP_V2_GUID, vendor_id, product_id, serial)
    }

    /// Open a device registered under a caller-supplied interface GUID (a `"{...}"` string).
    pub fn open_guid(interface_guid: &str, vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        let guid = guid_from_str(interface_guid).ok_or_else(|| Error::Os("bad interface GUID".into()))?;
        Self::open_with(&guid, vendor_id, product_id, serial)
    }

    fn open_with(guid: &GUID, vendor_id: u16, product_id: u16, serial: Option<&str>) -> Result<Self> {
        unsafe {
            for path in iface_paths(guid) {
                if vid_pid_from_path(&path) != Some((vendor_id, product_id)) {
                    continue;
                }
                if let Some(wanted) = serial {
                    match instance_id_from_path(&path) {
                        Some(id) if serial_matches(wanted, &id) => {}
                        _ => continue,
                    }
                }
                let h = CreateFileW(
                    path.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    null(),
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
                    null_mut(),
                );
                if h == INVALID_HANDLE_VALUE {
                    continue;
                }
                let mut wu: WINUSB_INTERFACE_HANDLE = null_mut();
                if WinUsb_Initialize(h, &mut wu) == 0 {
                    CloseHandle(h);
                    continue;
                }
                let mut iface: USB_INTERFACE_DESCRIPTOR = std::mem::zeroed();
                WinUsb_QueryInterfaceSettings(wu, 0, &mut iface);
                let (mut ep_in, mut ep_out) = (0u8, 0u8);
                for pipe in 0..iface.bNumEndpoints {
                    let mut pi: WINUSB_PIPE_INFORMATION = std::mem::zeroed();
                    if WinUsb_QueryPipe(wu, 0, pipe, &mut pi) != 0 && pi.PipeType == UsbdPipeTypeBulk {
                        if pi.PipeId & 0x80 != 0 {
                            ep_in = pi.PipeId;
                        } else {
                            ep_out = pi.PipeId;
                        }
                    }
                }
                if ep_in == 0 || ep_out == 0 {
                    WinUsb_Free(wu);
                    CloseHandle(h);
                    continue;
                }
                let ev = CreateEventW(null(), 1, 0, null());
                return Ok(Device { h, wu, ev, ep_out, ep_in });
            }
        }
        Err(Error::NotFound)
    }

    pub fn write_packet(&mut self, data: &[u8]) -> Result<()> {
        unsafe {
            ResetEvent(self.ev);
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.ev;
            let mut n = 0u32;
            if WinUsb_WritePipe(self.wu, self.ep_out, data.as_ptr(), data.len() as u32, &mut n, &mut ov) == 0 {
                if GetLastError() == ERROR_IO_PENDING {
                    WinUsb_GetOverlappedResult(self.wu, &ov, &mut n, 1);
                } else {
                    return Err(Error::Os("WinUsb_WritePipe failed".into()));
                }
            }
            Ok(())
        }
    }

    pub fn read_packet(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        const PIPE_TRANSFER_TIMEOUT: u32 = 0x03;
        const ERROR_SEM_TIMEOUT: u32 = 121;
        let ms: u32 = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        unsafe {
            WinUsb_SetPipePolicy(
                self.wu,
                self.ep_in,
                PIPE_TRANSFER_TIMEOUT,
                4,
                (&ms as *const u32).cast::<core::ffi::c_void>(),
            );
            ResetEvent(self.ev);
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.ev;
            let mut got = 0u32;
            if WinUsb_ReadPipe(self.wu, self.ep_in, buf.as_mut_ptr(), buf.len() as u32, &mut got, &mut ov) == 0 {
                let err = GetLastError();
                if err == ERROR_IO_PENDING {
                    if WinUsb_GetOverlappedResult(self.wu, &ov, &mut got, 1) == 0 {
                        return if GetLastError() == ERROR_SEM_TIMEOUT {
                            Err(Error::Timeout)
                        } else {
                            Err(Error::Os("WinUsb read failed".into()))
                        };
                    }
                } else if err == ERROR_SEM_TIMEOUT {
                    return Err(Error::Timeout);
                } else {
                    return Err(Error::Os("WinUsb_ReadPipe failed".into()));
                }
            }
            Ok(got as usize)
        }
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.ev);
            WinUsb_Free(self.wu);
            CloseHandle(self.h);
        }
    }
}
