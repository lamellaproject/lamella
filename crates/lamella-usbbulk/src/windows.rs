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
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW, CM_Get_Parent, CM_Get_Sibling,
    CM_Open_DevNode_Key, CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CM_REGISTRY_HARDWARE, CR_BUFFER_SMALL,
    CR_SUCCESS, RegDisposition_OpenExisting,
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W, SP_DEVINFO_DATA,
};
use windows_sys::Win32::Devices::Properties::{DEVPKEY_Device_Service, DEVPROPKEY, DEVPROPTYPE};
use windows_sys::Win32::Devices::Usb::{
    UsbdPipeTypeBulk, WinUsb_ControlTransfer, WinUsb_Free, WinUsb_GetAssociatedInterface,
    WinUsb_GetOverlappedResult,
    WinUsb_AbortPipe, WinUsb_Initialize, WinUsb_ResetPipe, WinUsb_QueryInterfaceSettings, WinUsb_QueryPipe, WinUsb_ReadPipe,
    WinUsb_SetPipePolicy, WinUsb_WritePipe, USB_DEVICE_DESCRIPTOR_TYPE, USB_INTERFACE_DESCRIPTOR,
    USB_STRING_DESCRIPTOR_TYPE, WINUSB_INTERFACE_HANDLE, WINUSB_PIPE_INFORMATION,
    WINUSB_SETUP_PACKET,
};
use windows_sys::Win32::Foundation::{WAIT_OBJECT_0,
    CloseHandle, GetLastError, ERROR_IO_PENDING, ERROR_SEM_TIMEOUT, ERROR_SUCCESS, GENERIC_READ,
    GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Registry::{
    RegCloseKey, RegQueryValueExW, HKEY, KEY_READ, REG_MULTI_SZ, REG_SZ, REG_VALUE_TYPE,
};
use windows_sys::Win32::System::Threading::{CreateEventW, ResetEvent, WaitForSingleObject, INFINITE};
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
/// **THIS IS HOW WINDOWS ANSWERS "IS THIS A VENDOR-BULK DEVICE" WITHOUT OPENING IT**, matching what
/// the Linux and macOS backends do. The interface class is in the node's COMPATIBLE IDS -- Windows
/// writes `USB\Class_FF&SubClass_xx&Prot_xx` there when it enumerates the device -- so it costs a
/// registry read and no handle.
///
/// Both shapes are checked, and missing the second is an easy mistake to make: a COMPOSITE device
/// gets one child node per interface (`...&MI_00\...`), while a device with a single interface has
/// the driver bound to the device node ITSELF and no interface children at all. Looking only at
/// children would find every probe and miss every single-interface board, including ours.
unsafe fn vendor_class_interface(devinst: u32) -> Option<u32> {
    interface_node(devinst, |ids| ids.iter().any(|id| id.to_ascii_uppercase().contains("CLASS_FF")))
}

/// The devnode whose compatible ids satisfy `wanted`: the device's own node, or one of its children --
/// both shapes, for the reason [`vendor_class_interface`] gives.
unsafe fn interface_node(devinst: u32, wanted: impl Fn(&[String]) -> bool) -> Option<u32> {
    if wanted(&compatible_ids(devinst)) {
        return Some(devinst);
    }
    let mut child: u32 = 0;
    if CM_Get_Child(&mut child, devinst, 0) != CR_SUCCESS {
        return None;
    }
    loop {
        if wanted(&compatible_ids(child)) {
            return Some(child);
        }
        let mut next: u32 = 0;
        if CM_Get_Sibling(&mut next, child, 0) != CR_SUCCESS {
            return None;
        }
        child = next;
    }
}

/// Whether `ids` hold the compatible id Windows gives an interface of `class`,
/// `USB\CLASS_c(2)&SUBCLASS_s(2)&PROT_p(2)` (Standard USB Identifiers) -- whole, and without regard to
/// case. A shorter id, `USB\CLASS_c(2)&SUBCLASS_s(2)`, names every protocol of that subclass, so it
/// does not count.
fn names_class(ids: &[String], class: crate::InterfaceClass) -> bool {
    let wanted = format!(
        "USB\\CLASS_{:02X}&SUBCLASS_{:02X}&PROT_{:02X}",
        class.class, class.subclass, class.protocol
    );
    ids.iter().any(|id| id.eq_ignore_ascii_case(&wanted))
}

/// The `bInterfaceNumber` of the interface on `node`, a devnode of the device whose own node is
/// `device`.
///
/// An interface node of a composite device carries it as the `MI_z(2)` field of its device id
/// (Standard USB Identifiers). A device with a single interface has the one numbered 0: an interface's
/// number is its zero-based index among the configuration's concurrent interfaces (USB 2.0, Table
/// 9-12).
unsafe fn interface_number_of(node: u32, device: u32) -> Option<u8> {
    if node == device {
        return Some(0);
    }
    device_instance_id(node).as_deref().and_then(interface_number_from_instance_id)
}

/// The `MI_z(2)` field of the device id at the front of an instance id
/// (`USB\VID_v(4)&PID_d(4)&MI_z(2)\...`), in hexadecimal. The instance id after the device id is never
/// read, whatever it contains.
fn interface_number_from_instance_id(id: &str) -> Option<u8> {
    let device_id = id.split('\\').nth(1)?;
    device_id.split('&').find_map(|field| {
        let (key, value) = (field.get(..3)?, field.get(3..)?);
        if !key.eq_ignore_ascii_case("MI_") || value.len() != 2 {
            return None;
        }
        u8::from_str_radix(value, 16).ok()
    })
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
    string_property(devinst, &KEY)
}

/// `DEVPKEY_Device_Service`: the name of the service installed for a devnode, readable without a
/// handle.
unsafe fn service_name(devinst: u32) -> Option<String> {
    string_property(devinst, &DEVPKEY_Device_Service)
}

/// A string property of a devnode, read with no handle; `None` when it is absent or empty.
unsafe fn string_property(devinst: u32, key: &DEVPROPKEY) -> Option<String> {
    let mut ty: DEVPROPTYPE = 0;
    let mut len: u32 = 0;
    CM_Get_DevNode_PropertyW(devinst, key, &mut ty, null_mut(), &mut len, 0);
    if len == 0 {
        return None;
    }
    let mut buf = vec![0u8; len as usize];
    if CM_Get_DevNode_PropertyW(devinst, key, &mut ty, buf.as_mut_ptr(), &mut len, 0) != CR_SUCCESS {
        return None;
    }
    let s = utf16_until_nul(&buf[..(len as usize).min(buf.len())]);
    (!s.is_empty()).then_some(s)
}

/// UTF-16LE `bytes` up to the first NUL, or to the end when there is none.
fn utf16_until_nul(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .take_while(|&unit| unit != 0)
        .collect();
    String::from_utf16_lossy(&units)
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
/// only a do-not-know justifies opening one to ask its descriptor. Treating both as "open it" costs
/// a descriptor fetch on every non-matching board.
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
/// it is for a device that never answers at all, which is a thing that ships: two boards of the
/// same model can answer the same request microseconds apart or seconds apart.
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

/// Lists the CMSIS-DAP v2 devices -- the vendor-class population, not a driver-bound one.
///
/// **THE DESCRIPTOR READ IS NOT OPTIONAL: SKIP IT AND EVERY COMPOSITE PROBE LISTS UNDER A SYNTHESIZED
/// ID INSTEAD OF ITS SERIAL.** Both an RPi Debug Probe and a micro:bit DAPLink are composite, and
/// Windows names an interface of a composite device with a port-derived id (`6&526bcf1&0&0000`) --
/// so without it `list()` reports probes whose "serials" change with the USB port and match nothing
/// a user can read off the hardware. `open` does not have that problem: it falls back to the
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
/// which names the remedy. Hiding it instead would report "not attached" for a device sitting on the
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

/// See [`crate::enumerate_class`]. Reads the PnP tree, so nothing is opened.
///
/// An interface is recognized by the compatible id Windows gives it, on the device's own node or on
/// one of its interface nodes, and its device is named as [`enumerate`] names one.
pub fn enumerate_class(class: crate::InterfaceClass) -> Result<Vec<crate::InterfaceInfo>> {
    let mut out = Vec::new();
    unsafe {
        for (path, devinst) in iface_paths(&USB_DEVICE_GUID) {
            let Some((vendor_id, product_id)) = vid_pid_from_path(&path) else { continue };
            let Some(node) = interface_node(devinst, |ids| names_class(ids, class)) else { continue };
            let Some(interface_number) = interface_number_of(node, devinst) else { continue };
            let identity = pnp_identity(devinst, vendor_id, product_id);
            out.push(crate::InterfaceInfo {
                vendor_id,
                product_id,
                serial_number: identity.serial.or_else(|| instance_id_from_path(&path)),
                product: identity.product,
                interface_number,
                interface_name: if node == devinst { None } else { bus_reported_name(node) },
            });
        }
    }
    Ok(out)
}

/// The longest data stage `WinUsb_ControlTransfer` takes: "The length of this buffer must not exceed
/// 4KB."
const LONGEST_CONTROL_DATA_STAGE: u16 = 4 * 1024;

/// Refuses a data stage longer than WinUSB takes, before anything is sent.
fn winusb_takes_length(length: u16) -> Result<()> {
    if length <= LONGEST_CONTROL_DATA_STAGE {
        return Ok(());
    }
    Err(Error::InvalidRequest(format!(
        "WinUSB takes a control transfer's data stage of at most 4 KB ({LONGEST_CONTROL_DATA_STAGE} \
         bytes), and this one is {length}"
    )))
}

/// Whether a request is addressed to an interface or to an endpoint -- recipient 1 or 2 in the low
/// bits of `bmRequestType` (USB 2.0, Table 9-2) -- which WinUSB sends through the handle of the
/// interface concerned; a request to the device, or to another recipient, goes through the handle
/// `WinUsb_Initialize` returned (WinUsb_ControlTransfer).
fn addressed_through_interface(request_type: u8) -> bool {
    matches!(request_type & 0x1F, 1 | 2)
}

/// `timeout_ms` as `WaitForSingleObject` takes it. `INFINITE` is `u32::MAX` and waits without end, so
/// the longest bound is one millisecond short of it.
fn wait_milliseconds(timeout_ms: u32) -> u32 {
    timeout_ms.min(INFINITE - 1)
}

/// A control transfer WinUSB completed with the error `code`.
///
/// `ERROR_SEM_TIMEOUT` is what WinUSB reports for a transfer its timeout policy cancelled
/// (WinUsb_ReadPipe), and the default control pipe carries a five-second policy of its own (WinUSB
/// Functions for Pipe Policy Modification), so here that code is a timeout too.
fn transfer_failed(code: u32) -> Error {
    if code == ERROR_SEM_TIMEOUT {
        return Error::Timeout;
    }
    Error::Os(format!("WinUsb_ControlTransfer failed (error {code})"))
}

/// Why a device that is attached, and has an interface of the class asked for, cannot be opened:
/// WinUSB registered no device interface for it, and `service` is the driver Windows installed for
/// it instead, if any.
fn no_winusb_interface(vendor_id: u16, product_id: u16, service: Option<&str>) -> String {
    let device = format!("{vendor_id:04x}:{product_id:04x}");
    match service {
        Some(name) if name.eq_ignore_ascii_case("WinUSB") => format!(
            "{device} is attached with WinUSB as its driver, and no device interface class is named \
             under its hardware key, so WinUSB registered no interface to open it through"
        ),
        Some(name) => format!(
            "{device} is attached, and Windows has installed the `{name}` driver for it; it can be \
             opened only with WinUSB as its driver"
        ),
        None => format!(
            "{device} is attached, and Windows has installed no driver for it; it can be opened only \
             with WinUSB as its driver"
        ),
    }
}

/// Every GUID in a registry string, or string list, held as UTF-16LE bytes; a string that is not a
/// GUID is skipped, and a list whose terminating NULs are missing is read to its end.
fn guids_from_wide_list(bytes: &[u8]) -> Vec<GUID> {
    let units: Vec<u16> =
        bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
    units
        .split(|&unit| unit == 0)
        .filter(|string| !string.is_empty())
        .filter_map(|string| guid_from_str(&String::from_utf16_lossy(string)))
        .collect()
}

/// The device interface classes WinUSB registers a devnode under: the `DeviceInterfaceGUIDs` or
/// `DeviceInterfaceGUID` value under its hardware key, where an INF's `[.HW]` section and the device's
/// own Microsoft OS descriptor both put it (WinUSB Installation; WinUSB Device).
unsafe fn winusb_interface_classes(devinst: u32) -> Vec<GUID> {
    let mut key: HKEY = null_mut();
    if CM_Open_DevNode_Key(devinst, KEY_READ, 0, RegDisposition_OpenExisting, &mut key, CM_REGISTRY_HARDWARE)
        != CR_SUCCESS
    {
        return Vec::new();
    }
    let mut out = Vec::new();
    for name in ["DeviceInterfaceGUIDs", "DeviceInterfaceGUID"] {
        let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        let mut kind: REG_VALUE_TYPE = 0;
        let mut len: u32 = 0;
        if RegQueryValueExW(key, name.as_ptr(), null(), &mut kind, null_mut(), &mut len) != ERROR_SUCCESS
            || len == 0
        {
            continue;
        }
        let mut buf = vec![0u8; len as usize];
        if RegQueryValueExW(key, name.as_ptr(), null(), &mut kind, buf.as_mut_ptr(), &mut len)
            != ERROR_SUCCESS
        {
            continue;
        }
        if kind == REG_MULTI_SZ || kind == REG_SZ {
            out.extend(guids_from_wide_list(&buf[..(len as usize).min(buf.len())]));
        }
    }
    RegCloseKey(key);
    out
}

/// The present device interfaces of class `guid` that the devnode `devinst` registered, as
/// NUL-terminated paths.
unsafe fn interfaces_of_device(guid: &GUID, devinst: u32) -> Vec<Vec<u16>> {
    let Some(id) = device_instance_id(devinst) else { return Vec::new() };
    let id: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
    for _ in 0..4 {
        let mut len: u32 = 0;
        if CM_Get_Device_Interface_List_SizeW(&mut len, guid, id.as_ptr(), CM_GET_DEVICE_INTERFACE_LIST_PRESENT)
            != CR_SUCCESS
            || len <= 1
        {
            return Vec::new();
        }
        let mut buf = vec![0u16; len as usize];
        match CM_Get_Device_Interface_ListW(guid, id.as_ptr(), buf.as_mut_ptr(), len, CM_GET_DEVICE_INTERFACE_LIST_PRESENT) {
            CR_SUCCESS => {
                return buf
                    .split(|&unit| unit == 0)
                    .filter(|path| !path.is_empty())
                    .map(|path| path.iter().copied().chain(Some(0)).collect())
                    .collect();
            }
            CR_BUFFER_SMALL => continue,
            _ => return Vec::new(),
        }
    }
    Vec::new()
}

/// The `bInterfaceNumber` of the interface WinUSB's handle `handle` reaches, when its class is `class`.
/// Its first alternate setting is the one read, since an interface's default setting is setting zero.
unsafe fn interface_number_if_class(handle: WINUSB_INTERFACE_HANDLE, class: crate::InterfaceClass) -> Option<u8> {
    let mut descriptor: USB_INTERFACE_DESCRIPTOR = std::mem::zeroed();
    if WinUsb_QueryInterfaceSettings(handle, 0, &mut descriptor) == 0 {
        return None;
    }
    let stated = (descriptor.bInterfaceClass, descriptor.bInterfaceSubClass, descriptor.bInterfaceProtocol);
    (stated == (class.class, class.subclass, class.protocol)).then_some(descriptor.bInterfaceNumber)
}

/// The first interface of `class` among those `first` -- the handle `WinUsb_Initialize` returned --
/// reaches, as its `bInterfaceNumber` and its handle: `None` when it is the first interface itself, and
/// otherwise an associated interface's handle, which the caller frees (WinUsb_GetAssociatedInterface).
unsafe fn winusb_interface_of_class(
    first: WINUSB_INTERFACE_HANDLE,
    class: crate::InterfaceClass,
) -> Option<(Option<WINUSB_INTERFACE_HANDLE>, u8)> {
    if let Some(number) = interface_number_if_class(first, class) {
        return Some((None, number));
    }
    for index in 0..=u8::MAX {
        let mut associated: WINUSB_INTERFACE_HANDLE = null_mut();
        if WinUsb_GetAssociatedInterface(first, index, &mut associated) == 0 {
            return None;
        }
        if let Some(number) = interface_number_if_class(associated, class) {
            return Some((Some(associated), number));
        }
        WinUsb_Free(associated);
    }
    None
}

/// An interface opened through WinUSB and driven by `WinUsb_ControlTransfer`.
pub struct ControlInterface {
    file: HANDLE,
    /// The handle `WinUsb_Initialize` returned, for the device's first interface.
    first: WINUSB_INTERFACE_HANDLE,
    /// The opened interface's own handle, when it is not the first interface.
    associated: Option<WINUSB_INTERFACE_HANDLE>,
    event: HANDLE,
    interface: u8,
}

impl ControlInterface {
    /// See [`crate::ControlInterface::open`].
    ///
    /// The device is chosen from the PnP tree as [`enumerate_class`] lists it, and opened through the
    /// device interface WinUSB registered for it. A device WinUSB is not driving is refused, with the
    /// driver Windows installed for it instead named.
    pub fn open(
        vendor_id: u16,
        product_id: u16,
        serial: Option<&str>,
        class: crate::InterfaceClass,
    ) -> Result<Self> {
        unsafe {
            for (path, devinst) in iface_paths(&USB_DEVICE_GUID) {
                if vid_pid_from_path(&path) != Some((vendor_id, product_id)) {
                    continue;
                }
                let Some(node) = interface_node(devinst, |ids| names_class(ids, class)) else { continue };
                let reported =
                    pnp_identity(devinst, vendor_id, product_id).serial.or_else(|| instance_id_from_path(&path));
                if !crate::serial_is(serial, reported.as_deref()) {
                    continue;
                }
                return Self::open_node(node, vendor_id, product_id, class);
            }
        }
        Err(Error::NotFound)
    }

    /// Opens the interface of `class` on the devnode `node` through WinUSB.
    unsafe fn open_node(node: u32, vendor_id: u16, product_id: u16, class: crate::InterfaceClass) -> Result<Self> {
        let Some(path) = winusb_interface_classes(node)
            .iter()
            .find_map(|guid| interfaces_of_device(guid, node).into_iter().next())
        else {
            return Err(Error::Os(no_winusb_interface(vendor_id, product_id, service_name(node).as_deref())));
        };
        let mut opened = ControlInterface {
            file: INVALID_HANDLE_VALUE,
            first: null_mut(),
            associated: None,
            event: null_mut(),
            interface: 0,
        };
        opened.file = CreateFileW(
            path.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
            null_mut(),
        );
        if opened.file == INVALID_HANDLE_VALUE {
            return Err(Error::Os(format!(
                "opening {vendor_id:04x}:{product_id:04x} through WinUSB failed (error {})",
                GetLastError()
            )));
        }
        let mut first: WINUSB_INTERFACE_HANDLE = null_mut();
        if WinUsb_Initialize(opened.file, &mut first) == 0 {
            return Err(Error::Os(format!("WinUsb_Initialize failed (error {})", GetLastError())));
        }
        opened.first = first;
        let Some((associated, interface)) = winusb_interface_of_class(opened.first, class) else {
            return Err(Error::Os(format!(
                "{vendor_id:04x}:{product_id:04x} opened through WinUSB, and no interface it reaches is \
                 of class {:02x}, subclass {:02x}, protocol {:02x}",
                class.class, class.subclass, class.protocol
            )));
        };
        opened.associated = associated;
        opened.interface = interface;
        opened.event = CreateEventW(null(), 1, 0, null());
        if opened.event.is_null() {
            return Err(Error::Os(format!("CreateEventW failed (error {})", GetLastError())));
        }
        Ok(opened)
    }

    /// See [`crate::ControlInterface::interface_number`].
    pub fn interface_number(&self) -> u8 {
        self.interface
    }

    /// See [`crate::ControlInterface::control_in`].
    pub fn control_in(&mut self, setup: crate::Setup, buffer: &mut [u8], timeout_ms: u32) -> Result<usize> {
        unsafe { self.control(setup, buffer.as_mut_ptr(), timeout_ms) }
    }

    /// See [`crate::ControlInterface::control_out`].
    pub fn control_out(&mut self, setup: crate::Setup, data: &[u8], timeout_ms: u32) -> Result<usize> {
        unsafe { self.control(setup, data.as_ptr().cast_mut(), timeout_ms) }
    }

    /// One overlapped `WinUsb_ControlTransfer`, bounded by `timeout_ms`: a transfer still pending at
    /// the bound is aborted on the default pipe and reaped before this returns.
    unsafe fn control(&mut self, setup: crate::Setup, buffer: *mut u8, timeout_ms: u32) -> Result<usize> {
        winusb_takes_length(setup.length)?;
        let handle = if addressed_through_interface(setup.request_type) {
            self.associated.unwrap_or(self.first)
        } else {
            self.first
        };
        let packet = WINUSB_SETUP_PACKET {
            RequestType: setup.request_type,
            Request: setup.request,
            Value: setup.value,
            Index: setup.index,
            Length: setup.length,
        };
        ResetEvent(self.event);
        let mut overlapped: OVERLAPPED = std::mem::zeroed();
        overlapped.hEvent = self.event;
        let mut transferred = 0u32;
        if WinUsb_ControlTransfer(handle, packet, buffer, u32::from(setup.length), &mut transferred, &overlapped) != 0 {
            return Ok(transferred as usize);
        }
        let code = GetLastError();
        if code != ERROR_IO_PENDING {
            return Err(transfer_failed(code));
        }
        let waited = WaitForSingleObject(self.event, wait_milliseconds(timeout_ms));
        if waited == WAIT_OBJECT_0 {
            if WinUsb_GetOverlappedResult(self.first, &overlapped, &mut transferred, 0) != 0 {
                return Ok(transferred as usize);
            }
            return Err(transfer_failed(GetLastError()));
        }
        let wait_failure = (waited != WAIT_TIMEOUT).then(|| GetLastError());
        WinUsb_AbortPipe(handle, 0);
        if WinUsb_GetOverlappedResult(self.first, &overlapped, &mut transferred, 1) != 0 {
            return Ok(transferred as usize);
        }
        Err(match wait_failure {
            None => Error::Timeout,
            Some(code) => Error::Os(format!("waiting for the control transfer failed (error {code})")),
        })
    }
}

impl Drop for ControlInterface {
    fn drop(&mut self) {
        unsafe {
            if !self.event.is_null() {
                CloseHandle(self.event);
            }
            if let Some(associated) = self.associated {
                WinUsb_Free(associated);
            }
            if !self.first.is_null() {
                WinUsb_Free(self.first);
            }
            if self.file != INVALID_HANDLE_VALUE {
                CloseHandle(self.file);
            }
        }
    }
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
                                    // The transfer can finish between the last check and the abort.
                                    // What it delivered is in `buf` and counted in `got`, so it is
                                    // returned; only a read that delivered nothing is a timeout.
                                    if got > 0 {
                                        return Ok(got as usize);
                                    }
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
    use super::{
        addressed_through_interface, guids_from_wide_list, instance_id_is_for,
        interface_number_from_instance_id, judge_path, names_class, no_winusb_interface,
        serial_from_instance_id, transfer_failed, utf16_until_nul, wait_milliseconds,
        winusb_takes_length, Error, PathVerdict, ERROR_SEM_TIMEOUT, INFINITE,
    };
    use windows_sys::Win32::Foundation::ERROR_GEN_FAILURE;

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

    /// Compatible ids as Windows lists them for one interface node.
    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|id| (*id).to_owned()).collect()
    }

    #[test]
    fn a_class_is_named_by_its_whole_compatible_id_in_either_case() {
        let dfu_mode = crate::InterfaceClass { class: 0xFE, subclass: 0x01, protocol: 0x02 };
        assert!(names_class(
            &ids(&[r"USB\Class_FE&SubClass_01&Prot_02", r"USB\Class_FE&SubClass_01", r"USB\Class_FE"]),
            dfu_mode
        ));
        assert!(names_class(&ids(&[r"USB\CLASS_FE&SUBCLASS_01&PROT_02"]), dfu_mode), "in either case");
        assert!(
            !names_class(&ids(&[r"USB\Class_FE&SubClass_01", r"USB\Class_FE"]), dfu_mode),
            "a shorter id names every protocol of the subclass"
        );
        assert!(!names_class(&ids(&[r"USB\Class_FE&SubClass_01&Prot_01"]), dfu_mode), "another protocol");
        assert!(!names_class(&ids(&[r"USB\Class_FE&SubClass_01&Prot_021"]), dfu_mode), "a longer id");
        assert!(!names_class(&[], dfu_mode), "no ids at all");
    }

    #[test]
    fn an_interface_number_is_read_from_the_device_id_alone() {
        assert_eq!(interface_number_from_instance_id(r"USB\VID_0483&PID_374B&MI_02\6&1a2b3c4d&0&0002"), Some(2));
        assert_eq!(
            interface_number_from_instance_id(r"usb\vid_0483&pid_374b&mi_0a\6&1a2b3c4d&0&000a"),
            Some(10),
            "hexadecimal, in either case"
        );
        assert_eq!(interface_number_from_instance_id(r"USB\VID_0483&PID_DF11\SERIAL0001"), None, "no MI field");
        assert_eq!(
            interface_number_from_instance_id(r"USB\VID_0001&PID_0002\A&MI_05"),
            None,
            "the instance id after the device id is not read"
        );
        assert_eq!(interface_number_from_instance_id(r"USB\VID_0001&PID_0002&MI_\X"), None, "an empty field");
        assert_eq!(interface_number_from_instance_id(r"USB\VID_0001&PID_0002&MI_ZZ\X"), None, "not hexadecimal");
        assert_eq!(interface_number_from_instance_id("no separators"), None);
    }

    #[test]
    fn a_request_to_an_interface_or_an_endpoint_goes_through_that_interfaces_handle() {
        assert!(addressed_through_interface(0x21), "a DFU class request, host to device");
        assert!(addressed_through_interface(0xA1), "and device to host");
        assert!(addressed_through_interface(0x02), "a standard request to an endpoint");
        assert!(!addressed_through_interface(0x80), "GET_DESCRIPTOR to the device");
        assert!(!addressed_through_interface(0xC3), "a vendor request to another recipient");
    }

    #[test]
    fn a_data_stage_longer_than_winusb_takes_is_refused_before_anything_is_sent() {
        assert!(winusb_takes_length(0).is_ok());
        assert!(winusb_takes_length(4096).is_ok());
        assert!(matches!(winusb_takes_length(4097), Err(Error::InvalidRequest(_))));
        assert!(matches!(winusb_takes_length(u16::MAX), Err(Error::InvalidRequest(_))));
    }

    #[test]
    fn no_timeout_becomes_a_wait_without_end() {
        assert_eq!(wait_milliseconds(1), 1);
        assert_eq!(wait_milliseconds(5_000), 5_000);
        assert_eq!(wait_milliseconds(u32::MAX), u32::MAX - 1);
        assert_ne!(wait_milliseconds(u32::MAX), INFINITE);
    }

    #[test]
    fn a_policy_timeout_is_a_timeout_and_any_other_code_is_carried() {
        assert!(matches!(transfer_failed(ERROR_SEM_TIMEOUT), Error::Timeout));
        match transfer_failed(ERROR_GEN_FAILURE) {
            Error::Os(text) => assert!(text.contains("error 31"), "carries the code: {text}"),
            other => panic!("expected an OS error, got {other:?}"),
        }
    }

    #[test]
    fn every_guid_in_a_registry_string_or_string_list_is_read() {
        let wide = |text: &str| text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
        let list = wide("{9f543223-cede-4fa3-b376-a25ce9a30e74}\0{D696BFEB-1734-417d-8A04-86D01071C512}\0\0");
        let guids = guids_from_wide_list(&list);
        assert_eq!(guids.len(), 2);
        assert_eq!((guids[0].data1, guids[0].data2, guids[0].data3), (0x9F54_3223, 0xCEDE, 0x4FA3));
        assert_eq!(guids[0].data4, [0xB3, 0x76, 0xA2, 0x5C, 0xE9, 0xA3, 0x0E, 0x74]);
        assert_eq!((guids[1].data1, guids[1].data4[7]), (0xD696_BFEB, 0x12));
        assert_eq!(
            guids_from_wide_list(&wide("{9f543223-cede-4fa3-b376-a25ce9a30e74}")).len(),
            1,
            "a string stored without its terminating NUL"
        );
        assert!(guids_from_wide_list(&wide("not a guid\0\0")).is_empty());
        assert!(guids_from_wide_list(&[0x7B]).is_empty(), "an odd byte is not a character");
    }

    #[test]
    fn a_device_not_driven_by_winusb_is_refused_with_the_driver_it_has() {
        let none = no_winusb_interface(0x0483, 0xDF11, None);
        assert!(none.contains("0483:df11") && none.contains("no driver"), "{none}");
        let other = no_winusb_interface(0x0483, 0xDF11, Some("exampledriver"));
        assert!(other.contains("`exampledriver`"), "names the driver Windows installed: {other}");
        let winusb = no_winusb_interface(0x0483, 0xDF11, Some("WinUSB"));
        assert!(winusb.contains("no device interface class"), "{winusb}");
        assert!(none.contains("WinUSB") && other.contains("WinUSB"), "and what it is opened through");
    }

    #[test]
    fn a_utf16_property_ends_at_its_first_nul() {
        let wide = |text: &str| text.encode_utf16().flat_map(u16::to_le_bytes).collect::<Vec<u8>>();
        assert_eq!(utf16_until_nul(&wide("WinUSB\0trailing")), "WinUSB");
        assert_eq!(utf16_until_nul(&wide("unterminated")), "unterminated");
        assert_eq!(utf16_until_nul(&[]), "");
    }
}
