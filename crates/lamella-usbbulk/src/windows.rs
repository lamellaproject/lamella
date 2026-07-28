//! Windows CMSIS-DAP v2 (USB bulk) backend via WinUSB (windows-sys) -- the v2 sibling of the HID
//! backend in lamella-usbhid. Finds a probe through its WinUSB device-interface (the CMSIS-DAP v2
//! interface GUID), opens the one matching the requested VID/PID, and exchanges raw bulk packets
//! over its IN/OUT pipes with overlapped I/O. No 3rd-party USB crate.

#![allow(unsafe_op_in_unsafe_fn)]

use super::{Binding, DeviceInfo, Error, Result};
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
    WinUsb_AbortPipe, WinUsb_Initialize, WinUsb_ResetPipe, WinUsb_QueryInterfaceSettings, WinUsb_QueryPipe, WinUsb_ReadPipe,
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

const USB_DEVICE_GUID: GUID = GUID {
    data1: 0xA5DCBF10,
    data2: 0x6530,
    data3: 0x11D2,
    data4: [0x90, 0x1F, 0x00, 0xC0, 0x4F, 0xB9, 0x51, 0xED],
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
        SetupDiGetDeviceInterfaceDetailW(hdev, &ifd, null_mut(), 0, &mut needed, null_mut());
        if needed > 0 {
            let mut buf = vec![0u8; needed as usize];
            let detail = buf.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;
            (*detail).cbSize = std::mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(hdev, &ifd, detail, needed, null_mut(), null_mut()) != 0 {
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
    let payload = &buf[2..len];
    let utf16: Vec<u16> = payload
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let text = String::from_utf16_lossy(&utf16);

    if text.chars().all(|c| c.is_ascii_graphic() || c == ' ') && !text.is_empty() {
        return Some(text);
    }
    Some(payload.iter().map(|byte| format!("{byte:02X}")).collect())
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

/// See [`crate::diagnose`]. Checks the requested interface GUID first, then falls back to the
/// all-USB-devices interface class -- a hit there with the same vendor/product id means the device
/// is plugged in but its interface has no driver bound.
pub fn diagnose(interface_guid: &str, vendor_id: u16, product_id: u16) -> Result<Binding> {
    let guid = guid_from_str(interface_guid).ok_or_else(|| Error::Os("bad interface GUID".into()))?;
    let matches = |guid: &GUID| unsafe {
        iface_paths(guid)
            .into_iter()
            .any(|path| vid_pid_from_path(&path) == Some((vendor_id, product_id)))
    };
    if matches(&guid) {
        return Ok(Binding::Bound);
    }
    if matches(&USB_DEVICE_GUID) {
        return Ok(Binding::PresentUnbound);
    }
    Ok(Binding::Absent)
}

pub struct Device {
    h: HANDLE,
    wu: WINUSB_INTERFACE_HANDLE,
    ev: HANDLE,
    ep_out: u8,
    ep_in: u8,
    /// The device-interface path this handle came from. Kept for diagnostics: on a composite probe
    /// several interfaces can register the SAME GUID, so "which one did we actually open" is a
    /// question that comes up and should not need guessing.
    path: String,
}

impl Device {
    /// Clears any stall on both pipes. A failed transfer can leave an endpoint halted, and every
    /// later transfer then fails for a reason that has nothing to do with what it was trying to do
    /// -- so a diagnostic that tries several things in a row must reset between them, or it reports
    /// the wreckage of its first attempt over and over.
    pub fn reset_pipes(&mut self) {
        unsafe {
            WinUsb_ResetPipe(self.wu, self.ep_in);
            WinUsb_ResetPipe(self.wu, self.ep_out);
        }
    }

    /// Clears one named endpoint -- DIAGNOSTIC ONLY, and the reason it exists is that a reset is
    /// not always harmless.
    ///
    /// [`reset_pipes`](Self::reset_pipes) touches the two COMMAND pipes, so when those two are the
    /// only ones failing, "the reset broke them" and "they were already broken" predict exactly the
    /// same observation. Being able to reset a *third*, working pipe turns that into an experiment:
    /// if resetting it makes it fail too, the reset is the cause.
    pub fn reset_endpoint(&mut self, endpoint: u8) {
        unsafe {
            WinUsb_ResetPipe(self.wu, endpoint);
        }
    }

    /// A human-readable dump of the interface and every pipe on it -- DIAGNOSTIC ONLY.
    ///
    /// When a device opens cleanly but will not carry traffic, the next question is always what we
    /// are actually attached to: the right interface? the right alternate setting? are the pipes
    /// the types and sizes expected? Guessing at that from a failing transfer is how a session gets
    /// spent, so make the descriptor readable instead.
    pub fn describe_interface(&self) -> String {
        unsafe {
            let mut out = String::new();
            out.push_str(&format!("path: {}", self.path));
            for alt in 0..8u8 {
                let mut iface: USB_INTERFACE_DESCRIPTOR = std::mem::zeroed();
                if WinUsb_QueryInterfaceSettings(self.wu, alt, &mut iface) == 0 {
                    break;
                }
                out.push_str(&format!(
                    "
interface {} alt {} class {:#04x}/{:#04x}/{:#04x}, {} endpoint(s)",
                    iface.bInterfaceNumber,
                    iface.bAlternateSetting,
                    iface.bInterfaceClass,
                    iface.bInterfaceSubClass,
                    iface.bInterfaceProtocol,
                    iface.bNumEndpoints,
                ));
                for pipe in 0..iface.bNumEndpoints {
                    let mut pi: WINUSB_PIPE_INFORMATION = std::mem::zeroed();
                    if WinUsb_QueryPipe(self.wu, alt, pipe, &mut pi) == 0 {
                        continue;
                    }
                    let kind = match pi.PipeType {
                        t if t == UsbdPipeTypeBulk => "bulk",
                        0 => "control",
                        1 => "isochronous",
                        3 => "interrupt",
                        _ => "unknown",
                    };
                    out.push_str(&format!(
                        "
  pipe {pipe}: id {:#04x} {kind} maxpacket {}",
                        pi.PipeId, pi.MaximumPacketSize
                    ));
                }
            }
            out
        }
    }

    /// The bulk endpoint addresses negotiated at open time, as `(in, out)`.
    ///
    /// Exposed because probing endpoints blindly is not a viable diagnostic: reading an endpoint a
    /// device does not have can block rather than fail, so a tool that needs to know which pipes
    /// exist must ask instead of sweep.
    pub fn endpoints(&self) -> (u8, u8) {
        (self.ep_in, self.ep_out)
    }

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
                let matched_by_path = match (serial, instance_id_from_path(&path)) {
                    (None, _) => true,
                    (Some(wanted), Some(id)) => serial_matches(wanted, &id),
                    (Some(_), None) => false,
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
                if h == INVALID_HANDLE_VALUE {
                    continue;
                }
                let mut wu: WINUSB_INTERFACE_HANDLE = null_mut();
                if WinUsb_Initialize(h, &mut wu) == 0 {
                    CloseHandle(h);
                    continue;
                }
                if !matched_by_path {
                    let wanted = serial.expect("only reachable when a serial was requested");
                    let descriptor_serial = product_and_serial(wu).1;
                    let ok = descriptor_serial
                        .as_deref()
                        .is_some_and(|actual| serial_matches(wanted, actual));
                    if !ok {
                        WinUsb_Free(wu);
                        CloseHandle(h);
                        continue;
                    }
                }
                let mut iface: USB_INTERFACE_DESCRIPTOR = std::mem::zeroed();
                WinUsb_QueryInterfaceSettings(wu, 0, &mut iface);
                let (mut ep_in, mut ep_out) = (0u8, 0u8);
                for pipe in 0..iface.bNumEndpoints {
                    let mut pi: WINUSB_PIPE_INFORMATION = std::mem::zeroed();
                    if WinUsb_QueryPipe(wu, 0, pipe, &mut pi) != 0 && pi.PipeType == UsbdPipeTypeBulk {
                        let slot = if pi.PipeId & 0x80 != 0 { &mut ep_in } else { &mut ep_out };
                        if *slot == 0 || pi.PipeId < *slot {
                            *slot = pi.PipeId;
                        }
                    }
                }
                if ep_in == 0 || ep_out == 0 {
                    WinUsb_Free(wu);
                    CloseHandle(h);
                    continue;
                }
                let ev = CreateEventW(null(), 1, 0, null());
                let opened = String::from_utf16_lossy(&path[..path.len().saturating_sub(1)]);
                return Ok(Device { h, wu, ev, ep_out, ep_in, path: opened });
            }
        }
        Err(Error::NotFound)
    }

    /// Sends one bulk OUT packet on a specific endpoint address (WinUSB's `PipeID` is the endpoint
    /// address) -- see [`crate::Device::write_endpoint`]. [`write_packet`](Self::write_packet) is this
    /// on the primary OUT endpoint.
    pub fn write_endpoint(&mut self, endpoint: u8, data: &[u8]) -> Result<()> {
        unsafe {
            ResetEvent(self.ev);
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.ev;
            let mut n = 0u32;
            if WinUsb_WritePipe(self.wu, endpoint, data.as_ptr(), data.len() as u32, &mut n, &ov) == 0 {
                if GetLastError() == ERROR_IO_PENDING {
                    const WRITE_TIMEOUT: Duration = Duration::from_millis(2000);
                    const ERROR_IO_INCOMPLETE: u32 = 996;
                    let deadline = std::time::Instant::now() + WRITE_TIMEOUT;
                    loop {
                        if WinUsb_GetOverlappedResult(self.wu, &ov, &mut n, 0) != 0 {
                            break;
                        }
                        let code = GetLastError();
                        if code != ERROR_IO_INCOMPLETE {
                            return Err(Error::Os(format!("WinUsb write failed (error {code})")));
                        }
                        if std::time::Instant::now() >= deadline {
                            WinUsb_AbortPipe(self.wu, endpoint);
                            let _ = WinUsb_GetOverlappedResult(self.wu, &ov, &mut n, 1);
                            return Err(Error::Timeout);
                        }
                        std::thread::sleep(Duration::from_millis(1));
                    }
                } else {
                    let code = GetLastError();
                    return Err(Error::Os(format!("WinUsb_WritePipe failed (error {code})")));
                }
            }
            Ok(())
        }
    }

    /// Reads one bulk IN packet from a specific endpoint address into `buf` -- see
    /// [`crate::Device::read_endpoint`]. [`read_packet`](Self::read_packet) is this on the primary IN
    /// endpoint.
    pub fn read_endpoint(&mut self, endpoint: u8, buf: &mut [u8], timeout: Duration) -> Result<usize> {
        const PIPE_TRANSFER_TIMEOUT: u32 = 0x03;
        const ERROR_SEM_TIMEOUT: u32 = 121;
        let ms: u32 = timeout.as_millis().min(u128::from(u32::MAX)) as u32;
        unsafe {
            WinUsb_SetPipePolicy(
                self.wu,
                endpoint,
                PIPE_TRANSFER_TIMEOUT,
                4,
                (&ms as *const u32).cast::<core::ffi::c_void>(),
            );
            ResetEvent(self.ev);
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = self.ev;
            let mut got = 0u32;
            if WinUsb_ReadPipe(self.wu, endpoint, buf.as_mut_ptr(), buf.len() as u32, &mut got, &ov) == 0 {
                let err = GetLastError();
                if err == ERROR_IO_PENDING {
                    const ERROR_IO_INCOMPLETE: u32 = 996;
                    let deadline = std::time::Instant::now() + timeout;
                    loop {
                        if WinUsb_GetOverlappedResult(self.wu, &ov, &mut got, 0) != 0 {
                            break;
                        }
                        match GetLastError() {
                            ERROR_IO_INCOMPLETE => {
                                if std::time::Instant::now() >= deadline {
                                    WinUsb_AbortPipe(self.wu, endpoint);
                                    let _ = WinUsb_GetOverlappedResult(self.wu, &ov, &mut got, 1);
                                    return Err(Error::Timeout);
                                }
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            ERROR_SEM_TIMEOUT => return Err(Error::Timeout),
                            code => return Err(Error::Os(format!("WinUsb read failed (error {code})"))),
                        }
                    }
                } else if err == ERROR_SEM_TIMEOUT {
                    return Err(Error::Timeout);
                } else {
                    return Err(Error::Os(format!("WinUsb_ReadPipe failed (error {err})")));
                }
            }
            Ok(got as usize)
        }
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
        unsafe {
            CloseHandle(self.ev);
            WinUsb_Free(self.wu);
            CloseHandle(self.h);
        }
    }
}
