//! A serial port on Windows, over the Win32 communications functions, and the attached ports, from
//! SetupAPI and the configuration manager's device tree.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};
use std::time::{Duration, Instant};

use windows_sys::Win32::Devices::Communication::{
    COMMTIMEOUTS, COMSTAT, CLRDTR, CLRRTS, ClearCommError, DCB, EVENPARITY, EscapeCommFunction,
    GetCommState, MARKPARITY, NOPARITY, ODDPARITY, ONE5STOPBITS, ONESTOPBIT, PURGE_RXCLEAR,
    PURGE_TXCLEAR, PurgeComm, SETDTR, SETRTS, SPACEPARITY, SetCommState, SetCommTimeouts,
    TWOSTOPBITS,
};
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_DEVCAP_UNIQUEID, CM_DRP_CAPABILITIES, CM_Get_DevNode_Registry_PropertyW, CM_Get_Device_IDW,
    CM_Get_Parent, CR_SUCCESS, DICS_FLAG_GLOBAL, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, DIREG_DEV,
    HDEVINFO, MAX_DEVICE_ID_LEN, SP_DEVINFO_DATA, SPDRP_FRIENDLYNAME, SetupDiDestroyDeviceInfoList,
    SetupDiEnumDeviceInfo, SetupDiGetClassDevsW, SetupDiGetDeviceRegistryPropertyW,
    SetupDiOpenDevRegKey,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, ERROR_INVALID_NAME, ERROR_NO_MORE_ITEMS,
    GENERIC_READ, GENERIC_WRITE, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FlushFileBuffers, OPEN_EXISTING, ReadFile, WriteFile,
};
use windows_sys::Win32::System::Ioctl::GUID_DEVINTERFACE_COMPORT;
use windows_sys::Win32::System::Registry::{HKEY, KEY_READ, REG_SZ, RegCloseKey, RegQueryValueExW};

use crate::{Buffer, Error, Parity, PortInfo, Settings, StopBits, UsbIdentity, deadline_for, time_left};

/// The device control block's flag bits this crate sets, at the positions `winbase.h` declares
/// them, which Microsoft's C compiler allocates from the low bit up.
const BINARY: u32 = 1 << 0;
const PARITY_CHECKING: u32 = 1 << 1;
const OUTPUT_WAITS_FOR_CTS: u32 = 1 << 2;
const TRANSMIT_CONTINUES_ON_XOFF: u32 = 1 << 7;
const OUTPUT_OBEYS_XON_XOFF: u32 = 1 << 8;
const INPUT_SENDS_XON_XOFF: u32 = 1 << 9;
/// `fRtsControl`, two bits at 12 and 13, set to `RTS_CONTROL_HANDSHAKE` (2): the driver drives RTS.
/// Its "disable" value is zero, and so is `fDtrControl`'s at bits 4 and 5, which is why neither has a
/// constant here: a block built from zero already leaves both lines to the caller.
const RTS_HANDSHAKE: u32 = 2 << 12;

/// The XON and XOFF characters, DC1 and DC3.
///
/// Set even when the line does not use them: `SetCommState` is documented to fail when the two
/// characters are equal, and a block built from zero would make them both zero.
const XON: u8 = 0x11;
const XOFF: u8 = 0x13;

/// The value a `COMMTIMEOUTS` field reads as "no bound" (`MAXDWORD`).
const MAXDWORD: u32 = u32::MAX;

/// The device control block for `settings`.
///
/// `limits` is the driver's own `(XonLim, XoffLim)`, which count bytes in the driver's input
/// buffer, whose size only the driver knows. They are handed back as the driver has them whether
/// or not the line uses flow control: a line without it never reads them, and zeros written in
/// their place would be left in the driver for the next program that reads its block and turns
/// flow control on.
fn control_block(settings: &Settings, limits: (u16, u16)) -> DCB {
    let mut flags = BINARY;
    if settings.parity != Parity::None {
        flags |= PARITY_CHECKING;
    }
    if settings.flow_control.hardware() {
        flags |= OUTPUT_WAITS_FOR_CTS | RTS_HANDSHAKE;
    }
    if settings.flow_control.software() {
        flags |= OUTPUT_OBEYS_XON_XOFF | INPUT_SENDS_XON_XOFF | TRANSMIT_CONTINUES_ON_XOFF;
    }
    let (xon_limit, xoff_limit) = limits;
    DCB {
        DCBlength: u32::try_from(size_of::<DCB>()).expect("a DCB is far under 4 GiB"),
        BaudRate: settings.baud,
        _bitfield: flags,
        wReserved: 0,
        XonLim: xon_limit,
        XoffLim: xoff_limit,
        ByteSize: settings.data_bits.count(),
        Parity: match settings.parity {
            Parity::None => NOPARITY,
            Parity::Odd => ODDPARITY,
            Parity::Even => EVENPARITY,
            Parity::Mark => MARKPARITY,
            Parity::Space => SPACEPARITY,
        },
        StopBits: match settings.stop_bits {
            StopBits::One => ONESTOPBIT,
            StopBits::OnePointFive => ONE5STOPBITS,
            StopBits::Two => TWOSTOPBITS,
        },
        XonChar: XON as i8,
        XoffChar: XOFF as i8,
        ErrorChar: 0,
        EofChar: 0,
        EvtChar: 0,
        wReserved1: 0,
    }
}

/// A duration in whole milliseconds, rounded up, so a bound under a millisecond is still a bound.
fn milliseconds(duration: Duration) -> u32 {
    u32::try_from(duration.as_nanos().div_ceil(1_000_000)).unwrap_or(u32::MAX)
}

/// The three read fields of `COMMTIMEOUTS` for a read bounded by `timeout`.
fn read_timeouts(timeout: Option<Duration>) -> [u32; 3] {
    match timeout {
        Some(bound) if bound.is_zero() => [MAXDWORD, 0, 0],
        Some(bound) => [MAXDWORD, MAXDWORD, milliseconds(bound).clamp(1, MAXDWORD - 1)],
        None => [MAXDWORD, MAXDWORD, MAXDWORD - 1],
    }
}

/// The two write fields of `COMMTIMEOUTS` for a write bounded by `timeout`.
fn write_timeouts(timeout: Option<Duration>) -> [u32; 2] {
    match timeout {
        None => [0, 0],
        Some(bound) => [0, milliseconds(bound).max(1)],
    }
}

/// The calling thread's last Win32 error.
fn last_error() -> u32 {
    unsafe { GetLastError() }
}

/// The device path a port name opens: `\\.\COM8` for `COM8`, and a name that is already a device
/// path as it is.
fn device_path(name: &str) -> String {
    if name.starts_with(r"\\") { name.to_string() } else { format!(r"\\.\{name}") }
}

/// Whether opening `given` opens the port the listing names `listed`.
pub(crate) fn opens_listed(given: &str, listed: &str) -> bool {
    device_path(given).eq_ignore_ascii_case(&device_path(listed))
}

/// A serial port held open.
#[derive(Debug)]
pub(crate) struct Port {
    handle: HANDLE,
    /// The time-outs last given to the driver, read fields then write fields, so a read or write
    /// whose bound has not changed does not set them again.
    applied: Option<[u32; 5]>,
}

unsafe impl Send for Port {}

unsafe impl Sync for Port {}

impl Port {
    /// Opens `name` with `settings`.
    pub(crate) fn open(name: &str, settings: &Settings) -> Result<Port, Error> {
        if name.contains('\0') {
            return Err(Error::Open { name: name.to_string(), code: ERROR_INVALID_NAME });
        }
        let wide: Vec<u16> = OsStr::new(&device_path(name)).encode_wide().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(Error::Open { name: name.to_string(), code: last_error() });
        }
        let mut port = Port { handle, applied: None };
        let current =
            port.current_control_block().map_err(|code| Error::Open { name: name.to_string(), code })?;
        port.configure(settings, &current)?;
        port.apply(read_timeouts(Some(Duration::ZERO)), write_timeouts(Some(crate::WRITE_STALL)))
            .map_err(|code| Error::Configure { what: "the time-outs", code })?;
        Ok(port)
    }

    /// The driver's current control block, or the error code `GetCommState` failed with.
    fn current_control_block(&self) -> Result<DCB, u32> {
        let mut current: DCB = unsafe { std::mem::zeroed() };
        current.DCBlength = u32::try_from(size_of::<DCB>()).expect("a DCB is far under 4 GiB");
        if unsafe { GetCommState(self.handle, &mut current) } == 0 {
            return Err(last_error());
        }
        Ok(current)
    }

    /// Installs `settings` on the line, whose control block is now `current`.
    fn configure(&mut self, settings: &Settings, current: &DCB) -> Result<(), Error> {
        let block = control_block(settings, (current.XonLim, current.XoffLim));
        if unsafe { SetCommState(self.handle, &block) } == 0 {
            return Err(Error::Configure { what: "the line settings", code: last_error() });
        }
        Ok(())
    }

    /// Gives the driver these time-outs unless it already has them, or reports the code
    /// `SetCommTimeouts` failed with, which each caller reports as its own failure: the open's as a
    /// setting not taken, a read's as the read failing, a write's as the write failing.
    fn apply(&mut self, read: [u32; 3], write: [u32; 2]) -> Result<(), u32> {
        let wanted = [read[0], read[1], read[2], write[0], write[1]];
        if self.applied == Some(wanted) {
            return Ok(());
        }
        let timeouts = COMMTIMEOUTS {
            ReadIntervalTimeout: read[0],
            ReadTotalTimeoutMultiplier: read[1],
            ReadTotalTimeoutConstant: read[2],
            WriteTotalTimeoutMultiplier: write[0],
            WriteTotalTimeoutConstant: write[1],
        };
        if unsafe { SetCommTimeouts(self.handle, &timeouts) } == 0 {
            self.applied = None;
            return Err(last_error());
        }
        self.applied = Some(wanted);
        Ok(())
    }

    /// The write fields the driver has now, kept when only the read bound changes.
    fn current_write(&self) -> [u32; 2] {
        self.applied.map_or(write_timeouts(Some(crate::WRITE_STALL)), |all| [all[3], all[4]])
    }

    /// The read fields the driver has now, kept when only the write bound changes.
    fn current_read(&self) -> [u32; 3] {
        self.applied.map_or(read_timeouts(Some(Duration::ZERO)), |all| [all[0], all[1], all[2]])
    }

    pub(crate) fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> Result<usize, Error> {
        let capacity = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let deadline = deadline_for(timeout);
        loop {
            let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            self.apply(read_timeouts(remaining), self.current_write()).map_err(|code| Error::Read { code })?;
            let mut got: u32 = 0;
            let ok = unsafe {
                ReadFile(self.handle, buffer.as_mut_ptr(), capacity, &mut got, null_mut())
            };
            if ok == 0 {
                let code = last_error();
                self.clear_errors();
                return Err(Error::Read { code });
            }
            if got > 0 || !time_left(deadline) {
                return Ok(got as usize);
            }
        }
    }

    pub(crate) fn write(&mut self, bytes: &[u8], timeout: Option<Duration>) -> Result<usize, Error> {
        let deadline = deadline_for(timeout);
        let mut sent = 0;
        while sent < bytes.len() {
            let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            self.apply(self.current_read(), write_timeouts(remaining)).map_err(|code| Error::Write { code })?;
            let length = u32::try_from(bytes.len() - sent).unwrap_or(u32::MAX);
            let mut written: u32 = 0;
            let ok = unsafe {
                WriteFile(self.handle, bytes[sent..].as_ptr(), length, &mut written, null_mut())
            };
            if ok == 0 {
                return Err(Error::Write { code: last_error() });
            }
            sent += written as usize;
            if !time_left(deadline) {
                break;
            }
        }
        if sent == 0 {
            return Err(Error::WriteStalled { after: 0 });
        }
        Ok(sent)
    }

    /// One `EscapeCommFunction` call.
    fn escape(&mut self, function: u32, what: &'static str) -> Result<(), Error> {
        if unsafe { EscapeCommFunction(self.handle, function) } == 0 {
            return Err(Error::Signal { what, code: last_error() });
        }
        Ok(())
    }

    pub(crate) fn set_dtr(&mut self, on: bool) -> Result<(), Error> {
        if on { self.escape(SETDTR, "assert DTR") } else { self.escape(CLRDTR, "clear DTR") }
    }

    pub(crate) fn set_rts(&mut self, on: bool) -> Result<(), Error> {
        if on { self.escape(SETRTS, "assert RTS") } else { self.escape(CLRRTS, "clear RTS") }
    }

    pub(crate) fn discard(&mut self, which: Buffer) -> Result<(), Error> {
        let flags = match which {
            Buffer::Input => PURGE_RXCLEAR,
            Buffer::Output => PURGE_TXCLEAR,
            Buffer::Both => PURGE_RXCLEAR | PURGE_TXCLEAR,
        };
        if unsafe { PurgeComm(self.handle, flags) } == 0 {
            return Err(Error::Buffer { what: "discard the buffers", code: last_error() });
        }
        Ok(())
    }

    /// The driver's status, which also clears its error flags.
    fn status(&mut self) -> Result<COMSTAT, Error> {
        let mut errors: u32 = 0;
        let mut status: COMSTAT = unsafe { std::mem::zeroed() };
        if unsafe { ClearCommError(self.handle, &mut errors, &mut status) } == 0 {
            return Err(Error::Buffer { what: "count the buffers", code: last_error() });
        }
        Ok(status)
    }

    fn clear_errors(&mut self) {
        let _ = self.status();
    }

    pub(crate) fn bytes_to_read(&mut self) -> Result<usize, Error> {
        Ok(self.status()?.cbInQue as usize)
    }

    pub(crate) fn bytes_to_write(&mut self) -> Result<usize, Error> {
        Ok(self.status()?.cbOutQue as usize)
    }

    pub(crate) fn drain(&mut self) -> Result<(), Error> {
        if unsafe { FlushFileBuffers(self.handle) } == 0 {
            return Err(Error::Buffer { what: "drain the output", code: last_error() });
        }
        Ok(())
    }

    /// Sends what is waiting, then closes, leaving nothing open, and reports whether the output was
    /// sent. The port is closed either way.
    pub(crate) fn close_after_sending(&mut self) -> Result<(), Error> {
        if self.handle == INVALID_HANDLE_VALUE {
            return Ok(());
        }
        let sent = self.drain();
        unsafe { CloseHandle(self.handle) };
        self.handle = INVALID_HANDLE_VALUE;
        sent
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        if self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

/// A device information set, destroyed when dropped.
struct DeviceInfoSet(HDEVINFO);

impl Drop for DeviceInfoSet {
    fn drop(&mut self) {
        unsafe { SetupDiDestroyDeviceInfoList(self.0) };
    }
}

/// An open registry key, closed when dropped.
struct RegistryKey(HKEY);

impl Drop for RegistryKey {
    fn drop(&mut self) {
        unsafe { RegCloseKey(self.0) };
    }
}

/// UTF-16 up to its first NUL, or all of it when it has none.
fn wide_to_string(wide: &[u16]) -> String {
    let end = wide.iter().position(|&unit| unit == 0).unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..end])
}

/// Lists the present COM ports.
pub(crate) fn list() -> Result<Vec<PortInfo>, Error> {
    let set = unsafe {
        SetupDiGetClassDevsW(
            &GUID_DEVINTERFACE_COMPORT,
            null(),
            null_mut(),
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    };
    if set == INVALID_HANDLE_VALUE as HDEVINFO {
        return Err(Error::List { what: "SetupDiGetClassDevsW", code: last_error() });
    }
    let set = DeviceInfoSet(set);
    let mut ports = Vec::new();
    for index in 0.. {
        let mut device: SP_DEVINFO_DATA = unsafe { std::mem::zeroed() };
        device.cbSize = u32::try_from(size_of::<SP_DEVINFO_DATA>()).expect("small");
        if unsafe { SetupDiEnumDeviceInfo(set.0, index, &mut device) } == 0 {
            let code = last_error();
            if code == ERROR_NO_MORE_ITEMS {
                break;
            }
            return Err(Error::List { what: "SetupDiEnumDeviceInfo", code });
        }
        let Some(name) = port_name(&set, &device) else { continue };
        let usb = usb_device(device.DevInst).map(|(vendor_id, product_id, serial_number)| {
            UsbIdentity { vendor_id, product_id, serial_number, product: friendly_name(&set, &device) }
        });
        ports.push(PortInfo { name, usb });
    }
    Ok(ports)
}

/// The `PortName` value under the device's hardware key, where the Ports class installer records
/// the name the port opens by (Registry Settings for a Plug and Play Serial Device).
fn port_name(set: &DeviceInfoSet, device: &SP_DEVINFO_DATA) -> Option<String> {
    let key = unsafe { SetupDiOpenDevRegKey(set.0, device, DICS_FLAG_GLOBAL, 0, DIREG_DEV, KEY_READ) };
    if std::ptr::eq(key, INVALID_HANDLE_VALUE) {
        return None;
    }
    let key = RegistryKey(key);
    let value: Vec<u16> = "PortName".encode_utf16().chain(std::iter::once(0)).collect();
    let mut kind: u32 = 0;
    let mut size: u32 = 0;
    let status =
        unsafe { RegQueryValueExW(key.0, value.as_ptr(), null(), &mut kind, null_mut(), &mut size) };
    if status != 0 || kind != REG_SZ || size == 0 {
        return None;
    }
    let mut data = vec![0u16; (size as usize).div_ceil(2) + 1];
    let mut size = u32::try_from(data.len() * 2).unwrap_or(u32::MAX);
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            value.as_ptr(),
            null(),
            &mut kind,
            data.as_mut_ptr().cast::<u8>(),
            &mut size,
        )
    };
    if status != 0 || kind != REG_SZ {
        return None;
    }
    let name = wide_to_string(&data);
    (!name.is_empty()).then_some(name)
}

/// The name Windows shows for the port, `USB Serial Device (COM8)`.
fn friendly_name(set: &DeviceInfoSet, device: &SP_DEVINFO_DATA) -> Option<String> {
    let mut kind: u32 = 0;
    let mut required: u32 = 0;
    let sized = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set.0,
            device,
            SPDRP_FRIENDLYNAME,
            &mut kind,
            null_mut(),
            0,
            &mut required,
        )
    };
    if sized == 0 && last_error() != ERROR_INSUFFICIENT_BUFFER {
        return None;
    }
    let mut data = vec![0u16; (required as usize).div_ceil(2) + 1];
    let capacity = u32::try_from(data.len() * 2).unwrap_or(u32::MAX);
    let ok = unsafe {
        SetupDiGetDeviceRegistryPropertyW(
            set.0,
            device,
            SPDRP_FRIENDLYNAME,
            &mut kind,
            data.as_mut_ptr().cast::<u8>(),
            capacity,
            &mut required,
        )
    };
    if ok == 0 || kind != REG_SZ {
        return None;
    }
    let name = wide_to_string(&data);
    (!name.is_empty()).then_some(name)
}

/// The device instance ID of `node`.
fn device_instance_id(node: u32) -> Option<String> {
    let mut buffer = [0u16; MAX_DEVICE_ID_LEN as usize + 1];
    let length = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
    let status = unsafe { CM_Get_Device_IDW(node, buffer.as_mut_ptr(), length, 0) };
    (status == CR_SUCCESS).then(|| wide_to_string(&buffer))
}

/// The parent of `node` in the device tree.
fn parent(node: u32) -> Option<u32> {
    let mut parent: u32 = 0;
    let status = unsafe { CM_Get_Parent(&mut parent, node, 0) };
    (status == CR_SUCCESS).then_some(parent)
}

/// Whether `node`'s bus reported its instance ID as unique across the system, which a USB hub does
/// exactly when the device carries a valid serial number.
fn instance_id_is_unique(node: u32) -> bool {
    let mut capabilities: u32 = 0;
    let mut length = u32::try_from(size_of::<u32>()).expect("small");
    let status = unsafe {
        CM_Get_DevNode_Registry_PropertyW(
            node,
            CM_DRP_CAPABILITIES,
            null_mut(),
            (&raw mut capabilities).cast(),
            &mut length,
            0,
        )
    };
    status == CR_SUCCESS && capabilities & CM_DEVCAP_UNIQUEID != 0
}

/// What a node of the device tree is, read from its device instance ID.
#[derive(Debug, PartialEq, Eq)]
enum Node<'id> {
    /// A USB device: `USB\VID_v(4)&PID_d(4)\<instance>`.
    UsbDevice { vendor_id: u16, product_id: u16, instance: &'id str },
    /// One interface of a composite USB device: `USB\VID_v(4)&PID_d(4)&MI_z(2)\<instance>`.
    UsbInterface,
    /// Anything else.
    Other,
}

/// Reads a device instance ID by the forms Standard USB identifiers documents.
fn node_kind(instance_id: &str) -> Node<'_> {
    let mut parts = instance_id.splitn(3, '\\');
    let (Some(enumerator), Some(device), Some(instance)) = (parts.next(), parts.next(), parts.next())
    else {
        return Node::Other;
    };
    if !enumerator.eq_ignore_ascii_case("USB") {
        return Node::Other;
    }
    let field = |prefix: &str| {
        device.split('&').find_map(|part| {
            let (key, value) = part.split_once('_')?;
            key.eq_ignore_ascii_case(prefix).then_some(value)
        })
    };
    let hex = |value: &str| (value.len() == 4).then(|| u16::from_str_radix(value, 16).ok()).flatten();
    let (Some(vendor_id), Some(product_id)) = (field("VID").and_then(hex), field("PID").and_then(hex))
    else {
        return Node::Other;
    };
    if field("MI").is_some() {
        return Node::UsbInterface;
    }
    Node::UsbDevice { vendor_id, product_id, instance }
}

/// The USB device the port at `port_node` belongs to: its vendor id, product id and serial number.
///
/// The port belongs to a USB device when it is that device, an interface of it, or a node a driver
/// created for the device or for one of its interfaces. So the walk up the tree may start at a node
/// of any kind and pass through USB interfaces, and it stops at the first other node: a port that a
/// different bus created, such as a Bluetooth serial port, does not belong to the USB radio its bus
/// happens to sit on.
fn usb_device(port_node: u32) -> Option<(u16, u16, Option<String>)> {
    let mut node = port_node;
    for depth in 0..8 {
        let id = device_instance_id(node)?;
        match node_kind(&id) {
            Node::UsbDevice { vendor_id, product_id, instance } => {
                let serial = (instance_id_is_unique(node) && !instance.is_empty())
                    .then(|| instance.to_string());
                return Some((vendor_id, product_id, serial));
            }
            Node::UsbInterface => {}
            Node::Other if depth == 0 => {}
            Node::Other => return None,
        }
        node = parent(node)?;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DataBits, FlowControl, Settings};

    /// The default line is binary mode alone: neither modem line goes to the driver, no flow control,
    /// no parity checking -- and XON and XOFF still differ, which SetCommState requires.
    #[test]
    fn the_default_line_leaves_both_modem_lines_to_the_caller() {
        let block = control_block(&Settings::new(115_200), (2048, 512));
        assert_eq!(block._bitfield, BINARY);
        assert_eq!(block._bitfield & (0b11 << 4), 0, "fDtrControl is DTR_CONTROL_DISABLE");
        assert_eq!(block._bitfield & (0b11 << 12), 0, "fRtsControl is RTS_CONTROL_DISABLE");
        assert_eq!(block.BaudRate, 115_200);
        assert_eq!((block.ByteSize, block.Parity, block.StopBits), (8, NOPARITY, ONESTOPBIT));
        assert_ne!(block.XonChar, block.XoffChar);
        assert_eq!(block.DCBlength as usize, size_of::<DCB>());
        assert_eq!((block.XonLim, block.XoffLim), (2048, 512), "the driver's limits, not zeros");
    }

    /// Every setting reaches its own field, so a line the operating system can express is expressed
    /// exactly: five data bits with one and a half stop bits and mark parity is a legal 8250 line.
    #[test]
    fn every_setting_reaches_its_own_field() {
        let settings = Settings {
            baud: 1_200,
            data_bits: DataBits::Five,
            parity: Parity::Mark,
            stop_bits: StopBits::OnePointFive,
            flow_control: FlowControl::None,
        };
        let block = control_block(&settings, (2048, 512));
        assert_eq!((block.ByteSize, block.Parity, block.StopBits), (5, MARKPARITY, ONE5STOPBITS));
        assert_ne!(block._bitfield & PARITY_CHECKING, 0, "a parity bit is checked");
        for (parity, expected) in [
            (Parity::Odd, ODDPARITY),
            (Parity::Even, EVENPARITY),
            (Parity::Space, SPACEPARITY),
        ] {
            assert_eq!(control_block(&Settings { parity, ..settings }, (2048, 512)).Parity, expected);
        }
    }

    /// Hardware flow control gives RTS to the driver and makes output wait for CTS; software flow
    /// control acts on XON and XOFF in both directions; each carries the driver's own limits.
    #[test]
    fn flow_control_sets_its_own_bits_and_carries_the_drivers_limits() {
        let hardware =
            control_block(&Settings { flow_control: FlowControl::RtsCts, ..Settings::new(9_600) }, (2048, 512));
        assert_eq!(hardware._bitfield & (0b11 << 12), 2 << 12, "RTS_CONTROL_HANDSHAKE");
        assert_ne!(hardware._bitfield & OUTPUT_WAITS_FOR_CTS, 0);
        assert_eq!(hardware._bitfield & (OUTPUT_OBEYS_XON_XOFF | INPUT_SENDS_XON_XOFF), 0);
        assert_eq!((hardware.XonLim, hardware.XoffLim), (2048, 512));

        let software =
            control_block(&Settings { flow_control: FlowControl::XonXoff, ..Settings::new(9_600) }, (10, 20));
        assert_eq!(software._bitfield & (0b11 << 12), 0, "RTS stays the caller's");
        assert_ne!(software._bitfield & OUTPUT_OBEYS_XON_XOFF, 0);
        assert_ne!(software._bitfield & INPUT_SENDS_XON_XOFF, 0);
        assert_eq!((software.XonChar as u8, software.XoffChar as u8), (0x11, 0x13));
    }

    /// The read fields are the three documented shapes: at once, bounded with return-on-first-byte,
    /// and the longest bound for a read without one.
    #[test]
    fn a_read_bound_becomes_the_documented_time_out_shape() {
        assert_eq!(read_timeouts(Some(Duration::ZERO)), [MAXDWORD, 0, 0]);
        assert_eq!(read_timeouts(Some(Duration::from_millis(50))), [MAXDWORD, MAXDWORD, 50]);
        assert_eq!(read_timeouts(Some(Duration::from_micros(1))), [MAXDWORD, MAXDWORD, 1]);
        assert_eq!(read_timeouts(None), [MAXDWORD, MAXDWORD, MAXDWORD - 1]);
        assert_eq!(read_timeouts(Some(Duration::from_secs(u64::MAX))), [MAXDWORD, MAXDWORD, MAXDWORD - 1]);
        assert_eq!(write_timeouts(None), [0, 0]);
        assert_eq!(write_timeouts(Some(Duration::ZERO)), [0, 1]);
        assert_eq!(write_timeouts(Some(Duration::from_secs(5))), [0, 5_000]);
    }

    /// The three forms of a node a port's walk meets, read as Standard USB identifiers writes them.
    #[test]
    fn a_device_instance_id_says_what_its_node_is() {
        assert_eq!(
            node_kind(r"USB\VID_2E8A&PID_000C\0123456789ABCDEF"),
            Node::UsbDevice { vendor_id: 0x2E8A, product_id: 0x000C, instance: "0123456789ABCDEF" }
        );
        assert_eq!(node_kind(r"USB\VID_2E8A&PID_000C&MI_01\8&1A2B3C4D&0&0001"), Node::UsbInterface);
        assert_eq!(node_kind(r"ACPI\PNP0501\0"), Node::Other);
        assert_eq!(node_kind(r"USB\ROOT_HUB30\4&1A2B3C4D&0&0"), Node::Other);
        assert_eq!(node_kind(r"FTDIBUS\VID_0403+PID_6001+A50285BIA\0000"), Node::Other);
        assert_eq!(node_kind("USB"), Node::Other, "an ID with no parts names nothing");
        assert_eq!(node_kind(r"USB\VID_12&PID_000C\X"), Node::Other, "an id is four hex digits");
    }

    /// **A listed port is found by every name that opens it**: the device path the open uses, and
    /// either spelled in any case. Another port's name is not it, though it begins the same.
    #[test]
    fn a_listed_port_is_found_by_every_name_that_opens_it() {
        let listed = crate::PortInfo { name: String::from("COM8"), usb: None };
        for name in ["COM8", r"\\.\COM8", "com8", r"\\.\com8"] {
            assert!(listed.is_opened_by(name), "{name} opens COM8");
        }
        for name in ["COM80", "COM18", r"\\.\COM80", "COM"] {
            assert!(!listed.is_opened_by(name), "{name} does not open COM8");
        }
    }

    /// Opening a port that does not exist is the missing-port failure, with the system's own code.
    #[test]
    fn a_port_that_does_not_exist_is_refused_as_missing() {
        let refused = Port::open("NO-SUCH-SERIAL-PORT", &Settings::new(115_200));
        let Err(error) = refused else { panic!("a port that does not exist opened") };
        assert!(matches!(error, Error::Open { .. }), "got {error:?}");
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound, "got {error:?}");
    }

    /// **A device that is not a serial port is refused as the open failing**, as lib.rs promises,
    /// rather than opening and then failing to take its settings: the null device opens by a
    /// device path as a COM port does, and has no control block to read.
    #[test]
    fn a_device_that_is_not_a_serial_port_is_refused_at_the_open() {
        let refused = Port::open("NUL", &Settings::new(115_200));
        let Err(error) = refused else { panic!("the null device opened as a serial port") };
        assert!(matches!(&error, Error::Open { name, code } if name == "NUL" && *code != 0), "got {error:?}");
    }
}
