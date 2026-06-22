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
    UsbdPipeTypeBulk, WinUsb_Free, WinUsb_GetOverlappedResult, WinUsb_Initialize,
    WinUsb_QueryInterfaceSettings, WinUsb_QueryPipe, WinUsb_ReadPipe, WinUsb_WritePipe,
    USB_INTERFACE_DESCRIPTOR, WINUSB_INTERFACE_HANDLE, WINUSB_PIPE_INFORMATION,
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

pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    let mut out = Vec::new();
    unsafe {
        for path in iface_paths(&DAP_V2_GUID) {
            if let Some((vendor_id, product_id)) = vid_pid_from_path(&path) {
                out.push(DeviceInfo {
                    vendor_id,
                    product_id,
                    serial_number: None,
                    product: None,
                });
            }
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
    pub fn open(vendor_id: u16, product_id: u16, _serial: Option<&str>) -> Result<Self> {
        unsafe {
            for path in iface_paths(&DAP_V2_GUID) {
                if vid_pid_from_path(&path) != Some((vendor_id, product_id)) {
                    continue;
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

    pub fn read_packet(&mut self, buf: &mut [u8], _timeout: Duration) -> Result<usize> {
        unsafe {
            ResetEvent(self.ev);
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.ev;
            let mut got = 0u32;
            if WinUsb_ReadPipe(self.wu, self.ep_in, buf.as_mut_ptr(), buf.len() as u32, &mut got, &mut ov) == 0 {
                if GetLastError() == ERROR_IO_PENDING {
                    if WinUsb_GetOverlappedResult(self.wu, &ov, &mut got, 1) == 0 {
                        return Err(Error::Os("WinUsb read failed".into()));
                    }
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
