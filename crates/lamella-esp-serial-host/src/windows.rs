//! A serial port on Windows, over the Win32 calls directly.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr::null_mut;

use windows_sys::Win32::Devices::Communication::{
    ClearCommError, EscapeCommFunction, PurgeComm, SetCommState, SetCommTimeouts, COMMTIMEOUTS,
    COMSTAT, CLRDTR, CLRRTS, DCB, PURGE_RXCLEAR, PURGE_TXCLEAR, SETDTR, SETRTS,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, ReadFile, WriteFile, FILE_ATTRIBUTE_NORMAL, OPEN_EXISTING,
};

use crate::{Port, PortError};

/// `fBinary`, bit 0 of the device control block's bit field.
///
/// The other bits are left at zero deliberately, and two of them matter enormously:
/// `fDtrControl` (bits 4-5) and `fRtsControl` (bits 12-13) are both zero, which is their
/// "disable" value -- **the driver leaves both signals alone and this program owns them.** Any other
/// value there makes the driver assert or flow-control a signal underneath the reset sequence.
const DCB_BINARY: u32 = 1 << 0;

/// A serial port held open.
#[derive(Debug)]
pub struct WindowsPort {
    handle: HANDLE,
    /// The name as given, so a reopen at a different rate can find the same port.
    name: String,
    /// The rate currently configured, reported for diagnostics.
    baud: u32,
}

unsafe impl Send for WindowsPort {}

impl WindowsPort {
    /// Opens `name` (e.g. `COM25`) at `baud`.
    ///
    /// # Errors
    /// [`PortError::Open`] with the Win32 error code when the port cannot be opened, and
    /// [`PortError::Configure`] when it opens but cannot be configured.
    pub fn open(name: &str, baud: u32) -> Result<WindowsPort, PortError> {
        let path = format!(r"\\.\{name}");
        let wide: Vec<u16> = OsStr::new(&path).encode_wide().chain(std::iter::once(0)).collect();
        let handle = unsafe {
            CreateFileW(
                wide.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                0,
                null_mut(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                null_mut(),
            )
        };
        if handle == INVALID_HANDLE_VALUE {
            return Err(PortError::Open { name: name.to_string(), code: unsafe { GetLastError() } });
        }
        let port = WindowsPort { handle, name: name.to_string(), baud };
        port.configure(baud)?;
        Ok(port)
    }

    /// Installs the rate, the frame format, and the read timeout.
    fn configure(&self, baud: u32) -> Result<(), PortError> {
        let mut dcb: DCB = unsafe { std::mem::zeroed() };
        dcb.DCBlength = u32::try_from(size_of::<DCB>()).expect("a DCB is far under 4 GiB");
        dcb.BaudRate = baud;
        dcb.ByteSize = 8;
        dcb.Parity = 0;
        dcb.StopBits = 0;
        dcb._bitfield = DCB_BINARY;
        if unsafe { SetCommState(self.handle, &dcb) } == 0 {
            return Err(PortError::Configure {
                what: "comm state",
                code: unsafe { GetLastError() },
            });
        }
        self.set_read_timeout(100)
    }

    /// Sets the read timeout for subsequent reads.
    ///
    /// # Why the two per-byte fields are saturated rather than zero
    ///
    /// A read here asks for a bufferful and the target answers with one small frame, so **what a read
    /// must do is return as soon as ANYTHING has arrived, having waited at most `ms` for the first
    /// byte.** With the per-byte fields at zero the platform uses only the total bound, and a read that
    /// asked for 4,096 bytes and got 14 waits out the WHOLE bound before handing them over -- so every
    /// exchange in a write costs the timeout, and the transfer rate becomes a function of the timeout
    /// rather than of the line.
    ///
    /// That is not a tuning choice: the platform documents this exact combination -- both per-byte
    /// fields saturated, the total a positive value below saturation -- as "return immediately with
    /// whatever is buffered; if nothing is buffered, wait for one byte and then return; and time out if
    /// none arrives within the total".
    ///
    /// A read may therefore return a PARTIAL frame, which is correct and already required: the frame
    /// reader reassembles across reads because a serial line splits frames wherever it likes.
    fn set_read_timeout(&self, ms: u32) -> Result<(), PortError> {
        /// The saturated value the platform reads as "no per-byte bound".
        const SATURATED: u32 = u32::MAX;
        let timeouts = COMMTIMEOUTS {
            ReadIntervalTimeout: SATURATED,
            ReadTotalTimeoutMultiplier: SATURATED,
            ReadTotalTimeoutConstant: ms.clamp(1, SATURATED - 1),
            WriteTotalTimeoutMultiplier: 0,
            WriteTotalTimeoutConstant: 5_000,
        };
        if unsafe { SetCommTimeouts(self.handle, &timeouts) } == 0 {
            return Err(PortError::Configure { what: "timeouts", code: unsafe { GetLastError() } });
        }
        Ok(())
    }

    /// One `EscapeCommFunction` call, named by what it is for.
    fn escape(&self, code: u32, what: &'static str) -> Result<(), PortError> {
        if unsafe { EscapeCommFunction(self.handle, code) } == 0 {
            return Err(PortError::Signal { what, code: unsafe { GetLastError() } });
        }
        Ok(())
    }
}

impl Drop for WindowsPort {
    fn drop(&mut self) {
        if self.handle != INVALID_HANDLE_VALUE {
            unsafe { CloseHandle(self.handle) };
        }
    }
}

impl Port for WindowsPort {
    fn write(&mut self, bytes: &[u8]) -> Result<(), PortError> {
        let mut offset = 0;
        while offset < bytes.len() {
            let mut written: u32 = 0;
            let remaining = u32::try_from(bytes.len() - offset).unwrap_or(u32::MAX);
            let ok = unsafe {
                WriteFile(
                    self.handle,
                    bytes[offset..].as_ptr(),
                    remaining,
                    &mut written,
                    null_mut(),
                )
            };
            if ok == 0 {
                return Err(PortError::Write { code: unsafe { GetLastError() } });
            }
            if written == 0 {
                return Err(PortError::WriteStalled { after: offset });
            }
            offset += written as usize;
        }
        Ok(())
    }

    fn read(&mut self, buffer: &mut [u8], timeout_ms: u32) -> Result<usize, PortError> {
        self.set_read_timeout(timeout_ms)?;
        let mut got: u32 = 0;
        let capacity = u32::try_from(buffer.len()).unwrap_or(u32::MAX);
        let ok = unsafe {
            ReadFile(self.handle, buffer.as_mut_ptr(), capacity, &mut got, null_mut())
        };
        if ok == 0 {
            let code = unsafe { GetLastError() };
            let mut errors: u32 = 0;
            let mut stat: COMSTAT = unsafe { std::mem::zeroed() };
            unsafe { ClearCommError(self.handle, &mut errors, &mut stat) };
            return Err(PortError::Read { code });
        }
        Ok(got as usize)
    }

    fn set_dtr(&mut self, on: bool) -> Result<(), PortError> {
        match on {
            true => self.escape(SETDTR, "assert DTR"),
            false => self.escape(CLRDTR, "clear DTR"),
        }
    }

    fn set_rts(&mut self, on: bool) -> Result<(), PortError> {
        match on {
            true => self.escape(SETRTS, "assert RTS"),
            false => self.escape(CLRRTS, "clear RTS"),
        }
    }

    fn reopen(&mut self, baud: u32) -> Result<(), PortError> {
        let name = self.name.clone();
        unsafe { CloseHandle(self.handle) };
        self.handle = INVALID_HANDLE_VALUE;
        let fresh = WindowsPort::open(&name, baud)?;
        self.handle = fresh.handle;
        self.baud = baud;
        std::mem::forget(fresh);
        Ok(())
    }

    fn discard_buffers(&mut self) -> Result<(), PortError> {
        if unsafe { PurgeComm(self.handle, PURGE_RXCLEAR | PURGE_TXCLEAR) } == 0 {
            return Err(PortError::Signal { what: "purge", code: unsafe { GetLastError() } });
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("{} at {} baud", self.name, self.baud)
    }
}
