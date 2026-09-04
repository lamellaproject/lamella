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
    CM_Get_Child, CM_Get_DevNode_PropertyW, CM_Get_DevNode_Registry_PropertyW, CM_Get_Device_IDW,
    CM_Get_Parent, CM_Get_Sibling, CR_SUCCESS,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Devices::Properties::{DEVPROPKEY, DEVPROPTYPE};
use windows_sys::Win32::Devices::Usb::{
    UsbdPipeTypeBulk, WinUsb_ControlTransfer, WinUsb_Free, WinUsb_GetOverlappedResult,
    WinUsb_AbortPipe, WinUsb_Initialize, WinUsb_ResetPipe, WinUsb_QueryInterfaceSettings, WinUsb_QueryPipe, WinUsb_ReadPipe,
    WinUsb_SetPipePolicy, WinUsb_WritePipe, USB_DEVICE_DESCRIPTOR_TYPE, USB_INTERFACE_DESCRIPTOR,
    USB_STRING_DESCRIPTOR_TYPE, WINUSB_INTERFACE_HANDLE, WINUSB_PIPE_INFORMATION,
    WINUSB_SETUP_PACKET,
};
use windows_sys::Win32::Foundation::{WAIT_OBJECT_0,
    CloseHandle, GetLastError, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject};
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
unsafe fn iface_paths(guid: &GUID) -> Vec<(Vec<u16>, u32)> {
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
            let mut devinfo: SP_DEVINFO_DATA = std::mem::zeroed();
            devinfo.cbSize = std::mem::size_of::<SP_DEVINFO_DATA>() as u32;
            if SetupDiGetDeviceInterfaceDetailW(hdev, &ifd, detail, needed, null_mut(), &mut devinfo)
                != 0
            {
                let p = (*detail).DevicePath.as_ptr();
                let mut len = 0usize;
                while *p.add(len) != 0 {
                    len += 1;
                }
                let mut w: Vec<u16> = std::slice::from_raw_parts(p, len).to_vec();
                w.push(0);
                out.push((w, devinfo.DevInst));
            }
        }
        idx += 1;
    }
    SetupDiDestroyDeviceInfoList(hdev);
    out
}

/// A device's serial and product string, read from the PnP tree with **nothing opened**.
///
/// **READING THESE TWO STRINGS OUT OF THE USB DESCRIPTORS MEANS OPENING THE DEVICE, AND OPENING A
/// DEBUG PROBE'S OWN INTERFACE MAKES IT RE-ENUMERATE -- WHICH ASSERTS NRST AND RESETS WHATEVER BOARD
/// IT IS WIRED TO.** A listing must not disturb what it lists, and this one is reached by the
/// command a user runs first when something is already wrong. Windows read both strings itself at
/// enumeration time and cached them, so this asks the cache instead.
///
/// **THE WALK, AND IT IS ONE STEP.** A SIMPLE device's own instance id ends in the serial
/// (`USB\VID_39E9&PID_0001\PICO-RP2040`). A COMPOSITE device's INTERFACE does not -- Windows names
/// it `...&MI_00\7&81C4590&0&0000`, a port-derived id -- but its PARENT is the composite device
/// itself, whose id does end in the serial:
///
/// ```text
/// interface  USB\VID_0483&PID_374B&MI_00\7&81C4590&0&0000    synthesized
/// parent     USB\VID_0483&PID_374B\0000FF000000000000000001  the serial
/// ```
///
/// **THE GUARD IS THE VID/PID CHECK, and it is load-bearing rather than defensive.** One more step
/// up from a composite device is the HUB, whose instance id is also `USB\...` and would parse as a
/// perfectly plausible serial -- so a walk that did not check whose id it was reading would hand
/// every device on one hub the SAME identity, and the selection ladder would then see one probe
/// where several are attached. That is the wrong-board write this crate exists to prevent.
///
/// The product string is `DEVPKEY_Device_BusReportedDeviceDesc`: the USB `iProduct` string as the
/// DEVICE reported it, not the INF's. The difference matters on a shared bench -- the INF-supplied
/// `DEVICEDESC` reads "USB Composite Device" for every composite probe attached, while the
/// bus-reported one distinguishes `STLINK-V3` from `STM32 STLink` from `Debug Probe (CMSIS-DAP)`.
unsafe fn pnp_identity(devinst: u32, vendor_id: u16, product_id: u16) -> PnpIdentity {
    let interface_name = bus_reported_name(devinst);
    let own_id = device_instance_id(devinst);
    let own_serial = own_id.as_deref().and_then(serial_from_instance_id);
    if own_serial.is_some() {
        return PnpIdentity { serial: own_serial, product: interface_name.clone(), interface_name };
    }

    let unresolved =
        || PnpIdentity { serial: None, product: interface_name.clone(), interface_name: interface_name.clone() };
    let mut parent: u32 = 0;
    if CM_Get_Parent(&mut parent, devinst, 0) != CR_SUCCESS {
        return unresolved();
    }
    let Some(parent_id) = device_instance_id(parent) else {
        return unresolved();
    };
    if !instance_id_is_for(&parent_id, vendor_id, product_id) {
        return unresolved();
    }
    let product = bus_reported_name(parent).or_else(|| interface_name.clone());
    PnpIdentity { serial: serial_from_instance_id(&parent_id), product, interface_name }
}

/// What the PnP tree says about one interface, with nothing opened.
struct PnpIdentity {
    /// The DEVICE's serial, from whichever node actually carries it.
    serial: Option<String>,
    /// A name for the whole device, for telling one board from another.
    product: Option<String>,
    /// This interface's own name, for telling what the interface is FOR.
    interface_name: Option<String>,
}

/// The devnode of this device's VENDOR-CLASS (`0xFF`) interface, if it has one.
///
/// **THIS IS HOW WINDOWS ANSWERS "IS THIS A VENDOR-BULK DEVICE" WITHOUT OPENING IT**. The interface
/// class is in the node's COMPATIBLE IDS -- Windows writes `USB\Class_FF&SubClass_xx&Prot_xx` there
/// when it enumerates the device -- so it costs a registry read and no handle.
///
/// Both shapes are checked, and missing the second is an easy mistake to make: a COMPOSITE device
/// gets one child node per interface (`...&MI_00\...`), while a device with a single interface has
/// the driver bound to the device node ITSELF and no interface children at all. Looking only at
/// children would find every probe and miss every single-interface board, including ours.
unsafe fn vendor_class_interface(devinst: u32) -> Option<u32> {
    if compatible_ids(devinst).iter().any(|id| id.to_ascii_uppercase().contains("CLASS_FF")) {
        return Some(devinst);
    }
    let mut child: u32 = 0;
    if CM_Get_Child(&mut child, devinst, 0) != CR_SUCCESS {
        return None;
    }
    loop {
        if compatible_ids(child).iter().any(|id| id.to_ascii_uppercase().contains("CLASS_FF")) {
            return Some(child);
        }
        let mut next: u32 = 0;
        if CM_Get_Sibling(&mut next, child, 0) != CR_SUCCESS {
            return None;
        }
        child = next;
    }
}

/// A devnode's compatible ids -- a `REG_MULTI_SZ`, so a run of NUL-terminated strings.
unsafe fn compatible_ids(devinst: u32) -> Vec<String> {
    const CM_DRP_COMPATIBLEIDS: u32 = 0x03;
    let mut len: u32 = 0;
    CM_Get_DevNode_Registry_PropertyW(devinst, CM_DRP_COMPATIBLEIDS, null_mut(), null_mut(), &mut len, 0);
    if len == 0 {
        return Vec::new();
    }
    let mut buf = vec![0u8; len as usize + 2];
    if CM_Get_DevNode_Registry_PropertyW(
        devinst,
        CM_DRP_COMPATIBLEIDS,
        null_mut(),
        buf.as_mut_ptr().cast(),
        &mut len,
        0,
    ) != CR_SUCCESS
    {
        return Vec::new();
    }
    let wide: &[u16] =
        std::slice::from_raw_parts(buf.as_ptr().cast::<u16>(), (len as usize / 2).min(buf.len() / 2));
    let mut out = Vec::new();
    for part in wide.split(|&c| c == 0) {
        if part.is_empty() {
            continue;
        }
        out.push(String::from_utf16_lossy(part));
    }
    out
}

/// A devnode's instance id, e.g. `USB\VID_0483&PID_374B\0000FF000000000000000001`.
unsafe fn device_instance_id(devinst: u32) -> Option<String> {
    let mut buf = [0u16; 512];
    if CM_Get_Device_IDW(devinst, buf.as_mut_ptr(), buf.len() as u32, 0) != CR_SUCCESS {
        return None;
    }
    Some(wide_string(buf.as_ptr()))
}

/// The serial in an instance id's last segment, or `None` where that segment is Windows' own
/// synthesized id rather than something the device reported.
fn serial_from_instance_id(id: &str) -> Option<String> {
    let last = id.rsplit('\\').next()?;
    (!last.is_empty() && !is_synthesized_instance_id(last)).then(|| last.to_owned())
}

/// Whether an instance id names this exact vendor and product -- the check that keeps a parent walk
/// from reading a HUB's identity as a device's own.
fn instance_id_is_for(id: &str, vendor_id: u16, product_id: u16) -> bool {
    let upper = id.to_ascii_uppercase();
    upper.starts_with("USB\\")
        && upper.contains(&format!("VID_{vendor_id:04X}"))
        && upper.contains(&format!("PID_{product_id:04X}"))
}

/// `DEVPKEY_Device_BusReportedDeviceDesc` -- the USB `iProduct` string the DEVICE reported, cached
/// by the hub driver at enumeration and readable without a handle.
unsafe fn bus_reported_name(devinst: u32) -> Option<String> {
    const KEY: DEVPROPKEY = DEVPROPKEY {
        fmtid: GUID {
            data1: 0x540B_947E,
            data2: 0x8B40,
            data3: 0x45BC,
            data4: [0xA8, 0xA2, 0x6A, 0x0B, 0x89, 0x4C, 0xBD, 0xA2],
        },
        pid: 4,
    };
    let mut ty: DEVPROPTYPE = 0;
    let mut len: u32 = 0;
    CM_Get_DevNode_PropertyW(devinst, &KEY, &mut ty, null_mut(), &mut len, 0);
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize + 2];
    if CM_Get_DevNode_PropertyW(devinst, &KEY, &mut ty, buf.as_mut_ptr(), &mut len, 0) != CR_SUCCESS
    {
        return None;
    }
    let s = wide_string(buf.as_ptr().cast::<u16>());
    (!s.is_empty()).then_some(s)
}

/// A NUL-terminated wide string as a `String`.
unsafe fn wide_string(p: *const u16) -> String {
    let mut len = 0usize;
    while *p.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
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
/// iSerialNumber, Windows uses the serial itself: `\\?\usb#vid_39e9&pid_0001#SERIAL#{guid}`.
/// (A serial-less device gets a synthesized `a&bcdef&0&1`-style id instead; matching against
/// that is harmless -- it simply never equals a real serial.)
fn instance_id_from_path(path: &[u16]) -> Option<String> {
    let s = String::from_utf16_lossy(path);
    let mut parts = s.split('#');
    let _prefix = parts.next()?;
    let _hardware_id = parts.next()?;
    Some(parts.next()?.to_string())
}

use crate::serial_matches;

/// Whether a path's instance-id segment is Windows' SYNTHESIZED id rather than the device's own
/// serial -- which is what decides whether a mismatch against it means anything.
///
/// Windows names an interface of a COMPOSITE device with an id like `6&1a2b3c4d&0&0000`; a simple
/// device that reports an iSerialNumber gets the serial itself. The ampersands are the tell, and
/// this file's own path documentation has always said so.
///
/// NOTE the one way this can be wrong, and which way it falls. A device whose real serial contained
/// an ampersand would be misread as synthesized -- and that is the SAFE direction: it falls back to
/// reading the descriptor, which is what a path making no such distinction does for every device.
/// The costly mistake would be the other way round, and this cannot make it.
fn is_synthesized_instance_id(id: &str) -> bool {
    id.contains('&')
}


/// What a device path alone can say about whether this is the requested board.
enum PathVerdict {
    /// This is the board, or none was requested. Open it.
    Match,
    /// This is NOT the board, and the path was able to say so -- the id it carries is the device's
    /// own serial and it does not match. Nothing further can change that, so do not open it.
    Mismatch,
    /// The path cannot say. The id is Windows' synthesized one, so the real serial lives only in a
    /// descriptor and reaching it costs an open.
    Unknown,
}

/// Judges a path against a requested serial WITHOUT opening the device.
///
/// The three-way answer is the point. Collapsing it to a boolean makes a settled NO indistinguishable
/// from a DO NOT KNOW, and the two want opposite handling: a settled no should skip the device, and
/// only a do-not-know justifies opening one to ask its descriptor. Treating both as "open it" is
/// what made a bench pay a descriptor fetch per non-matching board.
fn judge_path(serial: Option<&str>, path: &[u16]) -> PathVerdict {
    let Some(wanted) = serial else { return PathVerdict::Match };
    match instance_id_from_path(path) {
        Some(id) if serial_matches(wanted, &id) => PathVerdict::Match,
        Some(id) if is_synthesized_instance_id(&id) => PathVerdict::Unknown,
        Some(_) => PathVerdict::Mismatch,
        None => PathVerdict::Unknown,
    }
}

/// How long a descriptor fetch may take before it is abandoned.
///
/// A healthy device answers its own descriptors in microseconds. This bound is not for slowness --
/// it is for a device that never answers at all, which is a thing that ships: one Lamella Link
/// RP2350 returns its descriptors in 0 ms and another takes over ten seconds for the same request.
/// Enumeration reads up to three descriptors per device, so an unbounded fetch multiplies that
/// across every board on the bus.
const DESCRIPTOR_TIMEOUT: Duration = Duration::from_millis(250);

/// A descriptor fetch that CANNOT hang, replacing `WinUsb_GetDescriptor`.
///
/// `WinUsb_GetDescriptor` is synchronous with no timeout and no way to cancel it, so a device that
/// does not answer blocks the caller for the driver's own default -- seconds, per descriptor, with
/// no output. This file's read and write paths both refuse that trade in as many words: *"a hung
/// tool is far worse than an error"*. The control path is the same trade and had not been given the
/// same answer.
///
/// So the request goes out as an overlapped control transfer instead, which the same
/// poll-and-abort loop the pipes use can bound and cancel. The setup packet is the standard
/// GET_DESCRIPTOR that `WinUsb_GetDescriptor` issues internally: direction device-to-host, `wValue`
/// the type and index, `wIndex` the language id.
unsafe fn descriptor_bounded(
    wu: WINUSB_INTERFACE_HANDLE,
    descriptor_type: u8,
    index: u8,
    language: u16,
    buf: &mut [u8],
) -> Option<u32> {
    const REQUEST_TYPE_DEVICE_TO_HOST: u8 = 0x80;
    const REQUEST_GET_DESCRIPTOR: u8 = 0x06;

    let setup = WINUSB_SETUP_PACKET {
        RequestType: REQUEST_TYPE_DEVICE_TO_HOST,
        Request: REQUEST_GET_DESCRIPTOR,
        Value: (u16::from(descriptor_type) << 8) | u16::from(index),
        Index: language,
        Length: buf.len().min(u16::MAX as usize) as u16,
    };
    let event = CreateEventW(null(), 1, 0, null());
    if event.is_null() {
        return None;
    }
    let mut ov: OVERLAPPED = std::mem::zeroed();
    ov.hEvent = event;
    let mut got = 0u32;
    let issued = WinUsb_ControlTransfer(wu, setup, buf.as_mut_ptr(), setup.Length.into(), &mut got, &ov);
    let outcome = if issued != 0 {
        Some(got)
    } else if GetLastError() == ERROR_IO_PENDING {
        let ms = DESCRIPTOR_TIMEOUT.as_millis() as u32;
        if WaitForSingleObject(event, ms) == WAIT_OBJECT_0
            && WinUsb_GetOverlappedResult(wu, &ov, &mut got, 0) != 0
        {
            Some(got)
        } else {
            WinUsb_AbortPipe(wu, 0);
            let _ = WinUsb_GetOverlappedResult(wu, &ov, &mut got, 1);
            None
        }
    } else {
        None
    };
    CloseHandle(event);
    outcome
}

/// One USB string descriptor via WinUSB, decoded to a `String` (descriptor layout: length byte,
/// type byte, UTF-16LE payload). `index` 0 means "none advertised".
unsafe fn string_descriptor(wu: WINUSB_INTERFACE_HANDLE, index: u8) -> Option<String> {
    if index == 0 {
        return None;
    }
    let mut buf = [0u8; 256];
    let got = descriptor_bounded(wu, USB_STRING_DESCRIPTOR_TYPE as u8, index, 0x0409, &mut buf)?;
    if got < 2 {
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
    let Some(got) = descriptor_bounded(wu, USB_DEVICE_DESCRIPTOR_TYPE as u8, 0, 0, &mut desc) else {
        return (None, None);
    };
    if got < 18 {
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

/// Lists the CMSIS-DAP v2 devices -- the same body [`enumerate_guid`] runs, and it must be.
///
/// **THE DESCRIPTOR READ IS NOT OPTIONAL: SKIP IT AND EVERY COMPOSITE PROBE LISTS UNDER A SYNTHESIZED
/// ID INSTEAD OF ITS SERIAL.** Both an RPi Debug Probe and a micro:bit DAPLink are composite, and
/// Windows names an interface of a composite device with a port-derived id (`6&526bcf1&0&0000`) --
/// so `list()` reported two probes whose "serials" changed with the USB port and matched nothing a
/// user could read off the hardware. `open` never had the bug: it already falls back to the
/// descriptor for exactly this reason (see `open_with`). **Listing and opening disagreeing about
/// what a device is CALLED is worse than either being wrong alone** -- a tool selects by the name
/// the list gave it and finds nothing.
pub fn enumerate() -> Result<Vec<DeviceInfo>> {
    unsafe { Ok(enumerate_vendor_class()) }
}

/// Every USB device with a vendor-class (`0xFF`) interface -- the SAME population macOS and Linux
/// return, arrived at without opening anything.
///
/// **THE POPULATION IS THE INTERFACE CLASS, NOT A DRIVER BINDING.** A WinUSB interface GUID lists
/// only devices whose DRIVER registered it, which is a narrower question than the one this function
/// asks -- and a narrower one than the other two backends answer. An ST-Link is vendor-class and
/// binds ST's own driver, so a GUID-based listing cannot see it; neither can it see a board whose
/// interface has no driver bound at all, which is exactly the state a reflash pass has to find.
///
/// **BEING LISTED IS NOT BEING OPENABLE, and keeping those apart is the point.** Opening still goes
/// through the WinUSB interface GUID, because that is what Windows can actually drive; a device
/// listed here with no WinUSB binding fails at `open` with [`crate::diagnose`]'s `PresentUnbound`,
/// which names the remedy. Hiding it instead reported "not attached" for a device sitting on the
/// bus one driver install away from working.
unsafe fn enumerate_vendor_class() -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    for (path, devinst) in iface_paths(&USB_DEVICE_GUID) {
        let Some((vendor_id, product_id)) = vid_pid_from_path(&path) else { continue };
        let Some(interface) = vendor_class_interface(devinst) else { continue };
        let identity = pnp_identity(devinst, vendor_id, product_id);
        out.push(DeviceInfo {
            vendor_id,
            product_id,
            serial_number: identity.serial.or_else(|| instance_id_from_path(&path)),
            product: identity.product,
            interface_name: bus_reported_name(interface).or(identity.interface_name),
        });
    }
    out
}

/// List the devices registered under a caller-supplied interface GUID, with each device's
/// product and serial strings where they can be read.
pub fn enumerate_guid(interface_guid: &str) -> Result<Vec<DeviceInfo>> {
    let guid = guid_from_str(interface_guid).ok_or_else(|| Error::Os("bad interface GUID".into()))?;
    unsafe { Ok(enumerate_iface(&guid)) }
}

/// The shared body. The serial falls back to the device path's instance id when the descriptor
/// cannot be read -- which keeps a device another host is driving listed rather than invisible.
///
/// **That fallback is a LAST RESORT and not an equivalent.** It is the real serial only for a
/// SIMPLE device; for a composite one it is a synthesized, port-dependent id. A caller that needs
/// a stable identity must treat a fallback id as "unnamed", not as a serial.
unsafe fn enumerate_iface(guid: &GUID) -> Vec<DeviceInfo> {
    let mut out = Vec::new();
    unsafe {
        for (path, devinst) in iface_paths(guid) {
            let Some((vendor_id, product_id)) = vid_pid_from_path(&path) else { continue };
            let identity = pnp_identity(devinst, vendor_id, product_id);
            out.push(DeviceInfo {
                vendor_id,
                product_id,
                serial_number: identity.serial.or_else(|| instance_id_from_path(&path)),
                product: identity.product,
                interface_name: identity.interface_name,
            });
        }
    }
    out
}

/// See [`crate::diagnose`]. Checks the requested interface GUID first, then falls back to the
/// all-USB-devices interface class -- a hit there with the same vendor/product id means the device
/// is plugged in but its interface has no driver bound.
pub fn diagnose(interface_guid: &str, vendor_id: u16, product_id: u16) -> Result<Binding> {
    let guid = guid_from_str(interface_guid).ok_or_else(|| Error::Os("bad interface GUID".into()))?;
    let matches = |guid: &GUID| unsafe {
        iface_paths(guid)
            .into_iter()
            .any(|(path, _)| vid_pid_from_path(&path) == Some((vendor_id, product_id)))
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
            for (path, devinst) in iface_paths(guid) {
                if vid_pid_from_path(&path) != Some((vendor_id, product_id)) {
                    continue;
                }
                let settled_by_pnp = match pnp_identity(devinst, vendor_id, product_id).serial {
                    Some(known) => {
                        if !crate::candidate_satisfies(serial, Some(known.as_str())) {
                            continue;
                        }
                        true
                    }
                    None => false,
                };
                let verdict = judge_path(serial, &path);
                if matches!(verdict, PathVerdict::Mismatch) {
                    continue;
                }
                let matched_by_path = settled_by_pnp || matches!(verdict, PathVerdict::Match);
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
                    if !crate::candidate_satisfies(Some(wanted), descriptor_serial.as_deref()) {
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

#[cfg(test)]
mod tests {
    use super::{instance_id_is_for, judge_path, serial_from_instance_id, PathVerdict};

    /// A device-interface path as Windows spells it, wide and null-terminated.
    fn path(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(core::iter::once(0)).collect()
    }

    /// A simple device: Windows puts the device's OWN serial in the instance-id segment.
    const SIMPLE: &str = r"\?\usb#vid_39e9&pid_0001#7A5C9E20D14--with-a-serial#{guid}";
    /// A composite device's interface: the id is SYNTHESIZED and carries no serial at all.
    const COMPOSITE: &str = r"\?\usb#vid_0483&pid_374b#6&1a2b3c4d&0&0000#{guid}";

    #[test]
    fn a_serial_is_taken_from_an_instance_id_only_when_the_device_reported_one() {
        assert_eq!(
            serial_from_instance_id(r"USB\VID_0483&PID_374B\0000FF000000000000000001").as_deref(),
            Some("0000FF000000000000000001"),
            "a device node's last segment is the serial"
        );
        assert_eq!(
            serial_from_instance_id(r"USB\VID_0483&PID_374B&MI_00\7&81C4590&0&0000"),
            None,
            "a synthesized id is not a serial"
        );
        assert_eq!(serial_from_instance_id(""), None, "an empty id is not a serial");
        assert_eq!(serial_from_instance_id(r"USB\VID_0001&PID_0002\"), None, "nor a trailing one");
    }

    #[test]
    fn only_a_node_naming_this_vendor_and_product_can_supply_this_devices_serial() {
        assert!(instance_id_is_for(
            r"USB\VID_0483&PID_374B\0000FF000000000000000001",
            0x0483,
            0x374b
        ));
        assert!(
            !instance_id_is_for(r"USB\VID_05E3&PID_0610\6&1A396AE7&0&4", 0x0483, 0x374b),
            "a hub names its own ids and must not answer for the device below it"
        );
        assert!(!instance_id_is_for(r"USB\VID_0483&PID_374E\0035004831", 0x0483, 0x374b));
        assert!(instance_id_is_for(r"usb\vid_0483&pid_374b\0000FF00", 0x0483, 0x374b));
    }

    #[test]
    fn no_requested_serial_takes_any_board() {
        assert!(matches!(judge_path(None, &path(SIMPLE)), PathVerdict::Match));
        assert!(matches!(judge_path(None, &path(COMPOSITE)), PathVerdict::Match));
    }

    #[test]
    fn a_real_serial_that_matches_needs_no_open() {
        assert!(matches!(judge_path(Some("7A5C9E20D14"), &path(SIMPLE)), PathVerdict::Match));
    }

    #[test]
    fn a_real_serial_that_does_not_match_is_conclusive() {
        assert!(matches!(judge_path(Some("DEADBEEF"), &path(SIMPLE)), PathVerdict::Mismatch));
    }

    #[test]
    fn a_synthesized_id_leaves_the_question_open() {
        assert!(matches!(judge_path(Some("DEADBEEF"), &path(COMPOSITE)), PathVerdict::Unknown));
        assert!(matches!(judge_path(Some("0&0000"), &path(COMPOSITE)), PathVerdict::Match));
    }

    #[test]
    fn a_path_with_no_id_segment_is_not_a_refusal() {
        let truncated = path(r"\\?\usb#vid_0001&pid_0002");
        assert!(matches!(judge_path(Some("ANY"), &truncated), PathVerdict::Unknown));
    }
}
