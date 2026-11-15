//! The RP2350 boot ROM's PICOBOOT interface: the USB commands that read, erase and write a board's
//! flash while the board waits in BOOTSEL (RP2350 datasheet 5.6), over its two bulk pipes and the
//! two control requests the interface answers.
//!
//! Every command is 32 bytes on the bulk OUT pipe. A command that moves data is followed by that
//! data in its direction and ends with an empty packet in the other; one that moves none ends with
//! an empty IN packet. A command the boot ROM refuses stalls both pipes, so the client asks the
//! interface why (GET_COMMAND_STATUS) and resets it (INTERFACE_RESET) before it says so.

use lamella_stored_settings::{PAGE, RegionFlash};

/// `dMagic`, the first word of every command (datasheet 5.6.4, Table 458).
pub const MAGIC: u32 = 0x431f_d10b;

/// The bytes of every command.
pub const COMMAND_LEN: usize = 32;

/// EXCLUSIVE_ACCESS (5.6.4.1).
const EXCLUSIVE_ACCESS: u8 = 0x01;
/// FLASH_ERASE (5.6.4.3).
const FLASH_ERASE: u8 = 0x03;
/// READ (5.6.4.4): the top bit set, since its data comes IN.
const READ: u8 = 0x84;
/// WRITE (5.6.4.5).
const WRITE: u8 = 0x05;
/// REBOOT2 (5.6.4.10).
const REBOOT2: u8 = 0x0a;
/// EXIT_XIP (5.6.4.6). The datasheet calls it a no-op kept for RP2040. On the RP2350's boot ROM it
/// clears the flash abort that a host's USB configuration leaves, without which every READ of the
/// flash answers zeros and nothing is programmed.
const EXIT_XIP: u8 = 0x06;

/// EXCLUSIVE_ACCESS's `EXCLUSIVE`: writes through the BOOTSEL drive are refused while the client
/// works, so a file copied to the drive cannot interleave with its writes.
const EXCLUSIVE: u8 = 1;

/// EXCLUSIVE_ACCESS's `NOT_EXCLUSIVE`: no restriction on the BOOTSEL drive, so a file copied to it is
/// taken again (Table 459).
const NOT_EXCLUSIVE: u8 = 0;

/// REBOOT2's `REBOOT_TYPE_NORMAL`: the board starts by its normal boot path (datasheet 5.4.8.24).
const REBOOT_TYPE_NORMAL: u32 = 0;

/// Where the flash lies in the addresses the commands take: the XIP window's start.
const FLASH_BASE: u32 = 0x1000_0000;

/// The flash the commands reach: chip select 0's 16 MiB.
const FLASH_BYTES: u32 = 0x0100_0000;

/// The erase unit FLASH_ERASE takes: a 4 KiB sector.
const SECTOR: u32 = 4096;

/// The milliseconds REBOOT2 waits before the board starts again, so the command's answer reaches
/// the host first.
const REBOOT_DELAY_MS: u32 = 500;

/// What the client needs of the PICOBOOT interface: its two bulk pipes and its two control requests.
pub trait PicobootPipe {
    /// Sends `data` on the bulk OUT pipe. An empty `data` is the empty packet that ends a command.
    ///
    /// # Errors
    /// The transfer did not complete, a stalled pipe among the reasons; the text says why.
    fn send(&mut self, data: &[u8]) -> Result<(), String>;

    /// Receives one transfer from the bulk IN pipe into `buffer`, returning how many bytes came: 0
    /// for the empty packet that ends a command.
    ///
    /// # Errors
    /// The transfer did not complete, a stalled pipe among the reasons; the text says why.
    fn receive(&mut self, buffer: &mut [u8]) -> Result<usize, String>;

    /// INTERFACE_RESET (5.6.5.1): clears a stall on both pipes, abandons a command in progress and
    /// ends exclusive access.
    ///
    /// # Errors
    /// The request did not complete; the text says why.
    fn reset_interface(&mut self) -> Result<(), String>;

    /// GET_COMMAND_STATUS (5.6.5.2): the 16 bytes of the last command's status.
    ///
    /// # Errors
    /// The request did not complete; the text says why.
    fn command_status(&mut self) -> Result<[u8; 16], String>;
}

impl<T: PicobootPipe + ?Sized> PicobootPipe for &mut T {
    fn send(&mut self, data: &[u8]) -> Result<(), String> {
        (**self).send(data)
    }

    fn receive(&mut self, buffer: &mut [u8]) -> Result<usize, String> {
        (**self).receive(buffer)
    }

    fn reset_interface(&mut self) -> Result<(), String> {
        (**self).reset_interface()
    }

    fn command_status(&mut self) -> Result<[u8; 16], String> {
        (**self).command_status()
    }
}

impl<T: PicobootPipe + ?Sized> PicobootPipe for Box<T> {
    fn send(&mut self, data: &[u8]) -> Result<(), String> {
        (**self).send(data)
    }

    fn receive(&mut self, buffer: &mut [u8]) -> Result<usize, String> {
        (**self).receive(buffer)
    }

    fn reset_interface(&mut self) -> Result<(), String> {
        (**self).reset_interface()
    }

    fn command_status(&mut self) -> Result<[u8; 16], String> {
        (**self).command_status()
    }
}

/// A PICOBOOT client: one command at a time, each with a token of its own.
pub struct Picoboot<P: PicobootPipe> {
    pipe: P,
    token: u32,
}

impl<P: PicobootPipe> Picoboot<P> {
    /// A client over `pipe`, whose interface is reset first, so a stall an earlier session left
    /// does not answer for this one, and whose flash abort is then cleared with EXIT_XIP, so the
    /// flash answers what it holds.
    ///
    /// # Errors
    /// The reset or EXIT_XIP did not complete; the text says why.
    pub fn open(mut pipe: P) -> Result<Self, String> {
        pipe.reset_interface().map_err(|why| format!("resetting the PICOBOOT interface: {why}"))?;
        let mut client = Picoboot { pipe, token: 0 };
        client.command(EXIT_XIP, &[], Data::None)?;
        Ok(client)
    }

    /// Claims exclusive access, so the BOOTSEL drive takes no write while this client works.
    ///
    /// # Errors
    /// The boot ROM refused, or the command did not complete; the text says which.
    pub fn exclusive(&mut self) -> Result<(), String> {
        self.command(EXCLUSIVE_ACCESS, &[EXCLUSIVE], Data::None)
    }

    /// Releases exclusive access, so the BOOTSEL drive takes a file again.
    ///
    /// # Errors
    /// The boot ROM refused, or the command did not complete; the text says which.
    pub fn release(&mut self) -> Result<(), String> {
        self.command(EXCLUSIVE_ACCESS, &[NOT_EXCLUSIVE], Data::None)
    }

    /// Erases the `len` bytes of flash at `address`: whole 4 KiB sectors from a sector's start.
    ///
    /// # Errors
    /// A span that is not whole sectors, refused before it is sent; the boot ROM's refusal, and a
    /// command that did not complete.
    pub fn erase(&mut self, address: u32, len: u32) -> Result<(), String> {
        if len == 0 || !address.is_multiple_of(SECTOR) || !len.is_multiple_of(SECTOR) {
            return Err(String::from("an erase covers whole 4 KiB sectors, from a sector's start"));
        }
        self.command(FLASH_ERASE, &address_and_size(address, len), Data::None)
    }

    /// The `len` bytes at `address`.
    ///
    /// # Errors
    /// The boot ROM's refusal, and a command that did not complete.
    pub fn read(&mut self, address: u32, len: usize) -> Result<Vec<u8>, String> {
        let size = u32::try_from(len).map_err(|_| String::from("a read is shorter than 4 GiB"))?;
        let mut data = vec![0u8; len];
        self.command(READ, &address_and_size(address, size), Data::In(&mut data))?;
        Ok(data)
    }

    /// Writes `data` at `address`, a 256-byte page's start, where the flash reads erased.
    ///
    /// # Errors
    /// An address that is not a page's start, refused before it is sent; the boot ROM's refusal, and
    /// a command that did not complete.
    pub fn write(&mut self, address: u32, data: &[u8]) -> Result<(), String> {
        if !address.is_multiple_of(PAGE as u32) {
            return Err(String::from("a write starts at a 256-byte page's start"));
        }
        let size = u32::try_from(data.len()).map_err(|_| String::from("a write is shorter than 4 GiB"))?;
        self.command(WRITE, &address_and_size(address, size), Data::Out(data))
    }

    /// Starts the board by its normal boot path, [`REBOOT_DELAY_MS`] after the command is answered.
    ///
    /// # Errors
    /// The boot ROM's refusal, and a command that did not complete.
    pub fn reboot(&mut self) -> Result<(), String> {
        let mut args = [0u8; 16];
        args[0..4].copy_from_slice(&REBOOT_TYPE_NORMAL.to_le_bytes());
        args[4..8].copy_from_slice(&REBOOT_DELAY_MS.to_le_bytes());
        self.command(REBOOT2, &args, Data::None)
    }

    /// Sends the command `id` with `args`, moves `data`, and takes the command's end; a failure is
    /// worded with the interface's own status for it, after the interface is reset.
    fn command(&mut self, id: u8, args: &[u8], data: Data<'_>) -> Result<(), String> {
        self.token = self.token.wrapping_add(1);
        let transfer = match &data {
            Data::None => 0,
            Data::In(buffer) => buffer.len(),
            Data::Out(bytes) => bytes.len(),
        };
        let mut packet = [0u8; COMMAND_LEN];
        packet[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        packet[4..8].copy_from_slice(&self.token.to_le_bytes());
        packet[8] = id;
        packet[9] = u8::try_from(args.len()).unwrap_or(u8::MAX);
        packet[12..16].copy_from_slice(&u32::try_from(transfer).unwrap_or(u32::MAX).to_le_bytes());
        packet[16..16 + args.len()].copy_from_slice(args);
        let exchanged = self.exchange(&packet, data);
        exchanged.map_err(|why| self.refusal(id, &why))
    }

    /// The command `packet` on the bulk OUT pipe, its `data` in its direction, and its end: an empty
    /// IN packet, or after IN data an empty OUT packet (datasheet 5.6.4).
    fn exchange(&mut self, packet: &[u8; COMMAND_LEN], data: Data<'_>) -> Result<(), String> {
        self.pipe.send(packet)?;
        match data {
            Data::None => self.end_in(),
            Data::Out(bytes) => {
                self.pipe.send(bytes)?;
                self.end_in()
            }
            Data::In(buffer) => {
                let mut filled = 0;
                while filled < buffer.len() {
                    match self.pipe.receive(&mut buffer[filled..])? {
                        0 => {
                            return Err(format!(
                                "the device ended the data after {filled} of {} bytes",
                                buffer.len()
                            ));
                        }
                        got => filled += got,
                    }
                }
                self.pipe.send(&[])
            }
        }
    }

    /// Receives the empty IN packet that ends a command.
    fn end_in(&mut self) -> Result<(), String> {
        let mut spare = [0u8; 64];
        match self.pipe.receive(&mut spare)? {
            0 => Ok(()),
            got => Err(format!("the device answered with {got} bytes where a command ends with an empty packet")),
        }
    }

    /// `why`, the failure of the command `id`, with the interface's status for it when the status
    /// is about that command, once the interface is reset.
    fn refusal(&mut self, id: u8, why: &str) -> String {
        let status = self.pipe.command_status().ok().and_then(|bytes| {
            let token = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            let code = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
            (token == self.token && code != 0).then_some(code)
        });
        let mut text = match status {
            Some(code) => format!("{}: the boot ROM refused it, {} ({why})", command_name(id), status_text(code)),
            None => format!("{}: {why}", command_name(id)),
        };
        if let Err(reset) = self.pipe.reset_interface() {
            text.push_str(&format!("; resetting the interface failed too ({reset})"));
        }
        text
    }
}

/// The data a command moves.
enum Data<'a> {
    /// None.
    None,
    /// IN, into this buffer, whole.
    In(&'a mut [u8]),
    /// OUT, these bytes.
    Out(&'a [u8]),
}

/// The `dAddr` and `dSize` arguments FLASH_ERASE, READ and WRITE take.
fn address_and_size(address: u32, size: u32) -> [u8; 8] {
    let mut args = [0u8; 8];
    args[..4].copy_from_slice(&address.to_le_bytes());
    args[4..].copy_from_slice(&size.to_le_bytes());
    args
}

/// A command's name, as the datasheet gives it.
fn command_name(id: u8) -> &'static str {
    match id {
        EXCLUSIVE_ACCESS => "EXCLUSIVE_ACCESS",
        FLASH_ERASE => "FLASH_ERASE",
        READ => "READ",
        WRITE => "WRITE",
        REBOOT2 => "REBOOT2",
        EXIT_XIP => "EXIT_XIP",
        _ => "a PICOBOOT command",
    }
}

/// GET_COMMAND_STATUS's `dStatusCode`, in the datasheet's words (5.6.5.2, Table 471).
fn status_text(code: u32) -> String {
    let text = match code {
        1 => "unknown command",
        2 => "invalid command length",
        3 => "invalid transfer length",
        4 => "invalid address",
        5 => "bad alignment",
        6 => "interleaved write: a write through the BOOTSEL drive interfered",
        7 => "rebooting",
        8 => "unknown error",
        9 => "invalid state",
        10 => "not permitted",
        11 => "invalid argument",
        12 => "buffer too small",
        13 => "precondition not met",
        14 => "modified data",
        15 => "invalid data",
        16 => "not found",
        17 => "unsupported modification",
        _ => return format!("status {code}"),
    };
    text.to_owned()
}

/// The vendor id a board waiting in BOOTSEL answers to (datasheet 5.6.1, Table 456).
pub const BOOTSEL_VENDOR_ID: u16 = 0x2e8a;
/// The product id a board waiting in BOOTSEL answers to (Table 456).
pub const BOOTSEL_PRODUCT_ID: u16 = 0x000f;

/// The PICOBOOT interface's class, subclass and protocol: vendor-specific, zero and zero (datasheet
/// 5.6.2, Table 457). The interface is found by these, never by its number, which depends on whether
/// the boot ROM offers its drive.
pub const INTERFACE_CLASS: lamella_usbbulk::InterfaceClass =
    lamella_usbbulk::InterfaceClass { class: 0xff, subclass: 0, protocol: 0 };

/// A board waiting in BOOTSEL, as a USB listing reports its boot ROM.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InBootsel {
    /// The serial number the boot ROM answers with, which names the board on `--device`; `None` where
    /// the listing reports none, so that nothing can name the board.
    pub serial: Option<String>,
    /// The product name the boot ROM reports, where the listing carries one.
    pub product: Option<String>,
}

/// Each board waiting in BOOTSEL in `interfaces`, a USB listing of PICOBOOT's interface class, in the
/// listing's order.
#[must_use]
pub fn in_bootsel(interfaces: &[lamella_usbbulk::InterfaceInfo]) -> Vec<InBootsel> {
    interfaces
        .iter()
        .filter(|interface| (interface.vendor_id, interface.product_id) == (BOOTSEL_VENDOR_ID, BOOTSEL_PRODUCT_ID))
        .map(|interface| InBootsel { serial: interface.serial_number.clone(), product: interface.product.clone() })
        .collect()
}

/// Every board this computer lists waiting in BOOTSEL. A listing opens nothing; one the operating
/// system refuses lists no board.
#[must_use]
pub fn waiting_boards() -> Vec<InBootsel> {
    lamella_usbbulk::enumerate_class(INTERFACE_CLASS).map(|interfaces| in_bootsel(&interfaces)).unwrap_or_default()
}

/// Whether the board whose serial number is `serial` is listed waiting in BOOTSEL, with its PICOBOOT
/// interface. A listing opens nothing.
pub fn waiting_in_bootsel(serial: &str) -> bool {
    lamella_usbbulk::enumerate_class(INTERFACE_CLASS).is_ok_and(|interfaces| {
        interfaces.iter().any(|interface| {
            (interface.vendor_id, interface.product_id) == (BOOTSEL_VENDOR_ID, BOOTSEL_PRODUCT_ID)
                && interface.serial_number.as_deref().is_some_and(|reported| reported.eq_ignore_ascii_case(serial))
        })
    })
}

/// Asks `listed` every `pause` until it says no, for at most `limit`, and answers whether it did.
pub(crate) fn until_gone(mut listed: impl FnMut() -> bool, limit: std::time::Duration, pause: std::time::Duration) -> bool {
    let started = std::time::Instant::now();
    loop {
        if !listed() {
            return true;
        }
        if started.elapsed() >= limit {
            return false;
        }
        std::thread::sleep(pause);
    }
}

/// INTERFACE_RESET's `bRequest` (5.6.5.1, Table 469).
const INTERFACE_RESET: u8 = 0x41;
/// GET_COMMAND_STATUS's `bRequest` (5.6.5.2, Table 470).
const GET_COMMAND_STATUS: u8 = 0x42;

/// The longest a bulk transfer waits: past any one command's erase, which the boot ROM answers when
/// the erase is done.
const TRANSFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// The longest a control request waits.
const CONTROL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// A PICOBOOT control request, `request`, to `interface`: vendor-defined and addressed to the interface
/// (Tables 469 and 470, `bmRequestType` 01000001b and 11000001b; the direction is the call's).
fn interface_request(request: u8, interface: u8) -> lamella_usbbulk::ControlRequest {
    lamella_usbbulk::ControlRequest {
        kind: lamella_usbbulk::RequestKind::Vendor,
        recipient: lamella_usbbulk::Recipient::Interface,
        request,
        value: 0,
        index: u16::from(interface),
    }
}

/// Whether a bulk transfer's failure is evidence that an endpoint may be halted at the host's end,
/// which INTERFACE_RESET does not reach: a transfer the OS reports as failed, such as one the boot ROM
/// stalled. One that ran out of time was cancelled, and one refused before it was sent touched nothing.
fn leaves_a_halt(failure: &lamella_usbbulk::Error) -> bool {
    matches!(failure, lamella_usbbulk::Error::Os(_))
}

/// A [`PicobootPipe`] over the PICOBOOT interface of a board waiting in BOOTSEL, opened through USB.
pub struct UsbPicoboot {
    interface: lamella_usbbulk::ControlInterface,
    /// Whether a bulk transfer failed since the interface was last reset ([`leaves_a_halt`]).
    failed: bool,
}

impl UsbPicoboot {
    /// Drives the PICOBOOT interface that `interface` opened, found by [`INTERFACE_CLASS`].
    pub fn new(interface: lamella_usbbulk::ControlInterface) -> Self {
        UsbPicoboot { interface, failed: false }
    }
}

impl UsbPicoboot {
    /// `outcome`, a bulk transfer's, as the client takes it, with a failure kept as evidence of a halt
    /// when it is one ([`leaves_a_halt`]).
    fn transferred<T>(&mut self, outcome: lamella_usbbulk::Result<T>) -> Result<T, String> {
        outcome.map_err(|failure| {
            self.failed |= leaves_a_halt(&failure);
            failure.to_string()
        })
    }
}

impl PicobootPipe for UsbPicoboot {
    fn send(&mut self, data: &[u8]) -> Result<(), String> {
        let outcome = self.interface.write_bulk(data, TRANSFER_TIMEOUT);
        self.transferred(outcome)
    }

    fn receive(&mut self, buffer: &mut [u8]) -> Result<usize, String> {
        let outcome = self.interface.read_bulk(buffer, TRANSFER_TIMEOUT);
        self.transferred(outcome)
    }

    /// INTERFACE_RESET, and after a transfer that failed, both endpoints cleared at both ends.
    ///
    /// INTERFACE_RESET "clears the HALT condition (if set) on each of the bulk endpoints" at the boot
    /// ROM's end (5.6.5.1), and the host's end of a halted endpoint is the host's own to clear. The
    /// clear also returns both data toggles to DATA0 at both ends, which keeps the next command from
    /// being taken by the boot ROM as a packet it already has.
    fn reset_interface(&mut self) -> Result<(), String> {
        let request = interface_request(INTERFACE_RESET, self.interface.interface_number());
        self.interface.control_out(request, &[], CONTROL_TIMEOUT).map_err(|why| why.to_string())?;
        if std::mem::take(&mut self.failed) {
            self.interface
                .clear_bulk_halts()
                .map_err(|why| format!("clearing the bulk endpoints after a failed transfer: {why}"))?;
        }
        Ok(())
    }

    fn command_status(&mut self) -> Result<[u8; 16], String> {
        let request = interface_request(GET_COMMAND_STATUS, self.interface.interface_number());
        let mut status = [0u8; 16];
        let got = self.interface.control_in(request, &mut status, CONTROL_TIMEOUT).map_err(|why| why.to_string())?;
        if got != status.len() {
            return Err(format!("the status is 16 bytes, and the boot ROM returned {got}"));
        }
        Ok(status)
    }
}

/// A board's flash through PICOBOOT, at offsets from its start, for its stored settings.
pub struct PicobootFlash<P: PicobootPipe> {
    client: Picoboot<P>,
}

impl<P: PicobootPipe> PicobootFlash<P> {
    /// The flash of the board waiting in BOOTSEL behind `pipe`, with exclusive access claimed.
    ///
    /// # Errors
    /// The interface could not be reset, or exclusive access was refused; the text says why.
    pub fn open(pipe: P) -> Result<Self, String> {
        let mut client = Picoboot::open(pipe)?;
        client.exclusive()?;
        Ok(PicobootFlash { client })
    }

    /// Starts the board by its normal boot path.
    ///
    /// # Errors
    /// The command did not complete; the text says why.
    pub fn close(mut self) -> Result<(), String> {
        self.client.reboot()
    }

    /// Ends the session with the board still waiting in BOOTSEL and its drive taking a file again.
    ///
    /// # Errors
    /// The command did not complete; the text says why.
    pub fn release(mut self) -> Result<(), String> {
        self.client.release()
    }
}

/// A session with a board's flash through its boot ROM, which a write of flash regions ends one of
/// two ways: with the drive let go of for the image's UF2, or with the board started.
pub trait BootselSession: RegionFlash {
    /// Ends the session with the board still waiting in BOOTSEL and its drive taking a file again.
    ///
    /// # Errors
    /// The boot ROM did not answer; the text says why.
    fn release(self: Box<Self>) -> Result<(), String>;

    /// Ends the session by starting the board by its normal boot path.
    ///
    /// # Errors
    /// The boot ROM did not answer; the text says why.
    fn restart(self: Box<Self>) -> Result<(), String>;
}

impl<P: PicobootPipe> BootselSession for PicobootFlash<P> {
    fn release(self: Box<Self>) -> Result<(), String> {
        PicobootFlash::release(*self)
    }

    fn restart(self: Box<Self>) -> Result<(), String> {
        PicobootFlash::close(*self)
    }
}

/// The address the commands take for the `len` bytes at `offset` into the flash, when they lie in
/// the flash's 16 MiB.
fn flash_address(offset: u32, len: u32) -> Result<u32, String> {
    offset
        .checked_add(len)
        .filter(|&end| end <= FLASH_BYTES)
        .map(|_| FLASH_BASE + offset)
        .ok_or_else(|| String::from("the span passes the end of the flash's 16 MiB"))
}

/// The refusal of a read that came back as zeros across a whole sector.
const DEAD_READ: &str = "the board's boot ROM read its flash as zeros, which erased flash never reads and no stored \
                         setting is, so nothing it reads can be trusted; restart the board into BOOTSEL by holding its \
                         BOOTSEL button while plugging it in, and run the command again";

impl<P: PicobootPipe> RegionFlash for PicobootFlash<P> {
    /// The `len` bytes at `offset`; a read of a whole sector or more that comes back as zeros is
    /// refused, since erased flash reads 0xFF and a stored record begins with its magic.
    fn read(&mut self, offset: u32, len: usize) -> Result<Vec<u8>, String> {
        let length = u32::try_from(len).map_err(|_| String::from("the span passes the end of the flash's 16 MiB"))?;
        let address = flash_address(offset, length)?;
        let bytes = self.client.read(address, len)?;
        if len >= SECTOR as usize && bytes.iter().all(|&byte| byte == 0) {
            return Err(String::from(DEAD_READ));
        }
        Ok(bytes)
    }

    fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
        let address = flash_address(offset, len)?;
        self.client.erase(address, len)
    }

    fn program(&mut self, offset: u32, page: &[u8; PAGE]) -> Result<(), String> {
        let address = flash_address(offset, PAGE as u32)?;
        self.client.write(address, page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_net_core::wifi::{BootConnection, RecordWrite, Reconnection, security};
    use lamella_stored_settings::{Record, RegionStore};
    use std::collections::VecDeque;

    /// What the simulated boot ROM waits for next.
    enum Expect {
        /// A 32-byte command.
        Command,
        /// WRITE's data, for the address and length its command gave.
        Data(u32, u32),
        /// The empty OUT packet that ends a READ.
        EndOfRead,
    }

    /// A command as the simulated boot ROM parsed it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Parsed {
        token: u32,
        id: u8,
        size: u8,
        transfer: u32,
        args: [u8; 16],
    }

    /// The RP2350 boot ROM's PICOBOOT interface, simulated over a Pico 2 W's 4 MB of flash: it checks
    /// every command as the datasheet states it and stalls what it would refuse, with the status
    /// GET_COMMAND_STATUS reports.
    struct BootRom {
        flash: Vec<u8>,
        expect: Expect,
        replies: VecDeque<Vec<u8>>,
        /// The largest transfer one IN read yields: a full-speed bulk packet's 64 bytes.
        packet: usize,
        stalled: bool,
        status: u32,
        last: Option<Parsed>,
        commands: Vec<Parsed>,
        resets: usize,
        exclusive: u8,
        rebooted: Option<(u32, u32)>,
        /// A command the boot ROM refuses, and the status it gives.
        refuse: Option<(u8, u32)>,
        /// Whether READ answers zeros whatever the flash holds, as a boot ROM whose flash path is dead
        /// does.
        dead_reads: bool,
        /// Whether the flash is aborted, as the boot ROM leaves it once a host has configured its USB
        /// device: READ answers zeros and erase and program change nothing, until EXIT_XIP clears it.
        aborted: bool,
    }

    impl BootRom {
        fn new() -> Self {
            BootRom {
                flash: vec![0xFF; 0x40_0000],
                expect: Expect::Command,
                replies: VecDeque::new(),
                packet: 64,
                stalled: false,
                status: 0,
                last: None,
                commands: Vec::new(),
                resets: 0,
                exclusive: 0,
                rebooted: None,
                refuse: None,
                dead_reads: false,
                aborted: false,
            }
        }

        /// Where `address` lies in the flash, for `len` bytes, or `None` outside it.
        fn at(&self, address: u32, len: u32) -> Option<std::ops::Range<usize>> {
            let start = address.checked_sub(FLASH_BASE)? as usize;
            let end = start.checked_add(len as usize)?;
            (end <= self.flash.len()).then_some(start..end)
        }

        /// Stalls both pipes with `status`, as a refused command does.
        fn stall(&mut self, status: u32) {
            self.stalled = true;
            self.status = status;
            self.expect = Expect::Command;
        }

        fn command(&mut self, packet: &[u8]) {
            let word = |at: usize| u32::from_le_bytes(packet[at..at + 4].try_into().expect("a word"));
            assert_eq!(packet.len(), COMMAND_LEN, "every command is 32 bytes");
            assert_eq!(word(0), MAGIC, "every command begins with dMagic");
            assert_eq!(&packet[10..12], &[0, 0], "the reserved half-word is zero");
            let parsed = Parsed {
                token: word(4),
                id: packet[8],
                size: packet[9],
                transfer: word(12),
                args: packet[16..32].try_into().expect("sixteen bytes"),
            };
            assert!(parsed.args[usize::from(parsed.size)..].iter().all(|&byte| byte == 0), "args padded with zeros");
            self.commands.push(parsed.clone());
            self.last = Some(parsed.clone());
            self.status = 0;
            if let Some((id, status)) = self.refuse
                && id == parsed.id
            {
                return self.stall(status);
            }
            let (address, size) = (word(16), word(20));
            match parsed.id {
                EXCLUSIVE_ACCESS => {
                    assert_eq!((parsed.size, parsed.transfer), (1, 0));
                    self.exclusive = parsed.args[0];
                    self.replies.push_back(Vec::new());
                }
                FLASH_ERASE => {
                    assert_eq!((parsed.size, parsed.transfer), (8, 0));
                    if address % SECTOR != 0 || size % SECTOR != 0 {
                        return self.stall(5);
                    }
                    let Some(range) = self.at(address, size) else { return self.stall(4) };
                    if !self.aborted {
                        self.flash[range].fill(0xFF);
                    }
                    self.replies.push_back(Vec::new());
                }
                EXIT_XIP_COMMAND => {
                    assert_eq!((parsed.size, parsed.transfer), (0, 0), "EXIT_XIP takes no argument and moves no data");
                    self.aborted = false;
                    self.replies.push_back(Vec::new());
                }
                READ => {
                    assert_eq!(parsed.size, 8);
                    assert_eq!(parsed.transfer, size, "READ's transfer length is its size");
                    let Some(range) = self.at(address, size) else { return self.stall(4) };
                    let dead = self.dead_reads || self.aborted;
                    let data = if dead { vec![0; range.len()] } else { self.flash[range].to_vec() };
                    for chunk in data.chunks(self.packet) {
                        self.replies.push_back(chunk.to_vec());
                    }
                    self.expect = Expect::EndOfRead;
                }
                WRITE => {
                    assert_eq!(parsed.size, 8);
                    assert_eq!(parsed.transfer, size, "WRITE's transfer length is its size");
                    if address % 256 != 0 {
                        return self.stall(5);
                    }
                    self.expect = Expect::Data(address, size);
                }
                REBOOT2 => {
                    assert_eq!((parsed.size, parsed.transfer), (16, 0));
                    self.rebooted = Some((address, size));
                    self.replies.push_back(Vec::new());
                }
                _ => self.stall(1),
            }
        }
    }

    impl PicobootPipe for BootRom {
        fn send(&mut self, data: &[u8]) -> Result<(), String> {
            if self.stalled {
                return Err(String::from("the pipe is stalled"));
            }
            match std::mem::replace(&mut self.expect, Expect::Command) {
                Expect::Command => self.command(data),
                Expect::Data(address, size) => {
                    assert_eq!(data.len(), size as usize, "WRITE's data is its whole size");
                    let Some(range) = self.at(address, size) else {
                        self.stall(4);
                        return Ok(());
                    };
                    if !self.aborted {
                        for (cell, byte) in self.flash[range].iter_mut().zip(data) {
                            *cell &= byte;
                        }
                    }
                    self.replies.push_back(Vec::new());
                }
                Expect::EndOfRead => assert!(data.is_empty(), "a READ ends with an empty OUT packet"),
            }
            Ok(())
        }

        fn receive(&mut self, buffer: &mut [u8]) -> Result<usize, String> {
            if self.stalled {
                return Err(String::from("the pipe is stalled"));
            }
            let reply = self.replies.pop_front().ok_or_else(|| String::from("the device sent nothing"))?;
            assert!(reply.len() <= buffer.len(), "a reply fits the buffer the host gave");
            buffer[..reply.len()].copy_from_slice(&reply);
            Ok(reply.len())
        }

        fn reset_interface(&mut self) -> Result<(), String> {
            self.resets += 1;
            self.stalled = false;
            self.exclusive = 0;
            self.expect = Expect::Command;
            self.replies.clear();
            Ok(())
        }

        fn command_status(&mut self) -> Result<[u8; 16], String> {
            let mut status = [0u8; 16];
            if let Some(last) = &self.last {
                status[0..4].copy_from_slice(&last.token.to_le_bytes());
                status[8] = last.id;
            }
            status[4..8].copy_from_slice(&self.status.to_le_bytes());
            Ok(status)
        }
    }

    /// Where a Pico 2 W keeps its stored settings, and one slot.
    const SETTINGS_AT: u32 = 0x3F_E000;
    const UNIT: u32 = 0x1000;

    fn secured(name: &[u8], secret: &[u8]) -> Record {
        Record::new(name, secret, security::WPA2 | security::WPA3, Reconnection::Automatic, BootConnection::Background)
            .expect("a network")
    }

    /// **A NETWORK STORED THROUGH PICOBOOT LANDS AT THE BOARD's PLACE**: the interface reset first,
    /// exclusive access claimed, both slots read, one page written at the first slot, and the board
    /// started by its normal boot path at the end.
    #[test]
    fn a_network_stored_through_picoboot_lands_at_the_place_and_the_board_starts_again() {
        let mut rom = BootRom::new();
        let mut flash = PicobootFlash::open(&mut rom).expect("opens");
        {
            let mut slots = RegionStore::open(&mut flash, SETTINGS_AT, UNIT).expect("both slots read");
            assert_eq!(lamella_stored_settings::store(&mut slots, &secured(b"Home Net", b"hunter2hunter2")), RecordWrite::Written);
            assert_eq!(slots.error(), None);
        }
        flash.close().expect("the board starts again");
        let at = SETTINGS_AT as usize;
        assert_eq!(&rom.flash[at..at + 4], b"LWFR");
        assert_eq!(rom.resets, 1, "the interface is reset once, first");
        let ids: Vec<u8> = rom.commands.iter().map(|command| command.id).collect();
        assert_eq!(ids, [EXIT_XIP, EXCLUSIVE_ACCESS, READ, READ, WRITE, READ, REBOOT2], "{:?}", rom.commands);
        assert_eq!(rom.exclusive, EXCLUSIVE);
        assert_eq!(rom.rebooted, Some((REBOOT_TYPE_NORMAL, REBOOT_DELAY_MS)));
        let write = &rom.commands[4];
        assert_eq!(u32::from_le_bytes(write.args[0..4].try_into().unwrap()), FLASH_BASE + SETTINGS_AT, "the flash's own address");
        assert_eq!(write.transfer, 256);
    }

    /// A region of two sectors for a Pico 2 W's corner: a directory, then one image.
    fn two_sector_region() -> crate::region::RegionSpan {
        let mut bytes = vec![0xFF; 2 * UNIT as usize];
        bytes[..8].copy_from_slice(&[0x4c, 0x57, 0x46, 0x44, 0x01, 0x38, 0x00, 0x01]);
        bytes[UNIT as usize..UNIT as usize + 300].fill(0x5A);
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&lamella_pe::sha256::sha256(&bytes));
        crate::region::RegionSpan {
            name: String::from("wifi.region.bin"),
            base: FLASH_BASE + 0x3C_0000,
            bytes,
            sha256,
            header: UNIT,
            notices: Vec::new(),
        }
    }

    /// **A REGION WRITTEN THROUGH PICOBOOT GOES DIRECTORY FIRST AND LAST, AND THE DRIVE IS LET GO OF**:
    /// the region read whole, the directory's sector erased before the image's, the image's pages written
    /// before the directory's, the region read back, and exclusive access released with NOT_EXCLUSIVE --
    /// never REBOOT2, so the board still waits in BOOTSEL for the image's UF2.
    #[test]
    fn a_region_written_through_picoboot_goes_directory_first_and_last_and_lets_the_drive_go() {
        let region = two_sector_region();
        let mut rom = BootRom::new();
        {
            let mut flash = PicobootFlash::open(&mut rom).expect("opens");
            let outcome = crate::region::write_through(&mut flash, &region, FLASH_BASE, &mut |_| {}).expect("written");
            assert_eq!(outcome, crate::region::RegionOutcome::Written { sectors: 2, bytes: 2 * UNIT as usize });
            BootselSession::release(Box::new(flash)).expect("released");
        }
        let at = 0x3C_0000;
        assert_eq!(&rom.flash[at..at + region.bytes.len()], &region.bytes[..]);
        let addressed = |command: &Parsed| u32::from_le_bytes(command.args[0..4].try_into().unwrap()).wrapping_sub(FLASH_BASE);
        let steps: Vec<(u8, u32)> = rom.commands.iter().map(|command| (command.id, addressed(command))).collect();
        let (corner, image) = (0x3C_0000, 0x3C_1000);
        assert_eq!(
            steps[2..],
            [
                (READ, corner),
                (FLASH_ERASE, corner),
                (FLASH_ERASE, image),
                (WRITE, image),
                (WRITE, image + 0x100),
                (WRITE, corner),
                (READ, corner),
                (EXCLUSIVE_ACCESS, 0xF000_0000),
            ],
            "{:?}",
            rom.commands
        );
        assert_eq!((rom.exclusive, rom.rebooted), (NOT_EXCLUSIVE, None), "released, and the board not started");
    }

    /// Every command carries a token of its own, so a status names the command it is about.
    #[test]
    fn every_command_carries_a_token_of_its_own() {
        let mut rom = BootRom::new();
        let mut client = Picoboot::open(&mut rom).expect("opens");
        client.exclusive().expect("exclusive");
        client.read(FLASH_BASE, 16).expect("reads");
        client.erase(FLASH_BASE, SECTOR).expect("erases");
        let tokens: Vec<u32> = rom.commands.iter().map(|command| command.token).collect();
        assert!(tokens.windows(2).all(|pair| pair[0] != pair[1]), "{tokens:?}");
    }

    /// A read longer than one packet is collected whole, and ends with the empty OUT packet.
    #[test]
    fn a_read_longer_than_a_packet_is_collected_whole() {
        let mut rom = BootRom::new();
        rom.flash[0x100..0x100 + 300].iter_mut().enumerate().for_each(|(index, byte)| *byte = index as u8);
        let mut client = Picoboot::open(&mut rom).expect("opens");
        let read = client.read(FLASH_BASE + 0x100, 300).expect("reads");
        assert_eq!(read.len(), 300);
        assert!(read.iter().enumerate().all(|(index, &byte)| byte == index as u8));
        client.erase(FLASH_BASE, SECTOR).expect("the next command finds the read ended");
    }

    /// **A COMMAND THE BOOT ROM REFUSES IS NAMED BY ITS STATUS**, and the interface is reset, so the
    /// next command is answered.
    #[test]
    fn a_refused_command_is_named_by_its_status_and_the_interface_is_reset() {
        let mut rom = BootRom::new();
        rom.refuse = Some((FLASH_ERASE, 10));
        {
            let mut client = Picoboot::open(&mut rom).expect("opens");
            let why = client.erase(FLASH_BASE + SETTINGS_AT, UNIT).expect_err("refused");
            assert!(why.contains("not permitted"), "{why}");
            assert!(why.contains("FLASH_ERASE"), "{why}");
            client.read(FLASH_BASE, 4).expect("the interface answers again");
        }
        assert_eq!(rom.resets, 2, "reset at the open and after the refusal");
    }

    /// **THE TWO CONTROL REQUESTS ARE THE DATASHEET's:** vendor-defined and addressed to the PICOBOOT
    /// interface by its number, INTERFACE_RESET as 0x41 and GET_COMMAND_STATUS as 0x42 (Tables 469 and
    /// 470), on the interface found by its class alone (Table 457).
    #[test]
    fn the_two_control_requests_go_to_the_interface_as_the_datasheet_states_them() {
        assert_eq!((INTERFACE_RESET, GET_COMMAND_STATUS), (0x41, 0x42));
        for (request, interface) in [(INTERFACE_RESET, 1u8), (GET_COMMAND_STATUS, 0u8)] {
            let sent = interface_request(request, interface);
            assert_eq!(sent.kind, lamella_usbbulk::RequestKind::Vendor);
            assert_eq!(sent.recipient, lamella_usbbulk::Recipient::Interface);
            assert_eq!((sent.request, sent.value, sent.index), (request, 0, u16::from(interface)));
        }
        assert_eq!((INTERFACE_CLASS.class, INTERFACE_CLASS.subclass, INTERFACE_CLASS.protocol), (0xff, 0, 0));
    }

    /// **A FAILED TRANSFER MAY LEAVE AN ENDPOINT HALTED AT THE HOST's END**, which INTERFACE_RESET does
    /// not reach, so the next reset clears both endpoints; a transfer that ran out of time was
    /// cancelled and leaves none, and a request refused before it was sent touched nothing.
    #[test]
    fn a_failed_transfer_is_evidence_of_a_halt_and_a_timed_out_one_is_not() {
        assert!(leaves_a_halt(&lamella_usbbulk::Error::Os(String::from("the device stalled"))));
        assert!(!leaves_a_halt(&lamella_usbbulk::Error::Timeout));
        assert!(!leaves_a_halt(&lamella_usbbulk::Error::InvalidRequest(String::from("no bulk endpoint"))));
    }

    /// EXIT_XIP's `bCmdId` (datasheet 5.6.4.6), as the simulated boot ROM takes it.
    const EXIT_XIP_COMMAND: u8 = 0x06;

    /// **THE FLASH ABORT A USB CONFIGURATION LEAVES IS CLEARED BEFORE THE FLASH IS TOUCHED:** the boot
    /// ROM of a board waiting in BOOTSEL answers every READ of its flash with zeros and programs nothing
    /// once a host has configured it, until EXIT_XIP; so the client sends EXIT_XIP first, and a network
    /// stored through it lands.
    #[test]
    fn the_flash_abort_a_usb_configuration_leaves_is_cleared_before_the_flash_is_touched() {
        let mut rom = BootRom::new();
        rom.aborted = true;
        {
            let mut flash = PicobootFlash::open(&mut rom).expect("opens");
            let mut slots = RegionStore::open(&mut flash, SETTINGS_AT, UNIT).expect("both slots read");
            assert_eq!(lamella_stored_settings::store(&mut slots, &secured(b"Home Net", b"hunter2hunter2")), RecordWrite::Written);
        }
        let at = SETTINGS_AT as usize;
        assert_eq!(&rom.flash[at..at + 4], b"LWFR", "the record landed");
        let first_flash_command = rom.commands.iter().position(|command| [READ, WRITE, FLASH_ERASE].contains(&command.id));
        let exit = rom.commands.iter().position(|command| command.id == EXIT_XIP_COMMAND);
        assert!(exit.is_some() && exit < first_flash_command, "EXIT_XIP before the flash is touched: {:?}", rom.commands);
    }

    /// **A WHOLE SLOT READ AS ZEROS IS REFUSED, NEVER REPORTED:** erased flash reads 0xFF and a stored
    /// record begins with its magic, so zeros across a slot mean the boot ROM's READ did not reach the
    /// flash, and `show` must not say "stores no network" from them. The refusal says what to do, in
    /// plain words; nothing is written.
    #[test]
    fn a_whole_slot_read_as_zeros_is_refused_and_never_reported() {
        let mut rom = BootRom::new();
        rom.dead_reads = true;
        {
            let mut flash = PicobootFlash::open(&mut rom).expect("opens");
            let why = match RegionStore::open(&mut flash, SETTINGS_AT, UNIT) {
                Ok(_) => panic!("a dead read was taken for the slots"),
                Err(why) => why,
            };
            assert!(why.contains("zeros") && why.contains("BOOTSEL button"), "{why}");
        }
        let ids: Vec<u8> = rom.commands.iter().map(|command| command.id).collect();
        assert!(!ids.contains(&WRITE) && !ids.contains(&FLASH_ERASE), "nothing was written: {ids:?}");
    }

    /// **A BOARD THAT REBOOT2 RESTARTS IS WAITED FOR UNTIL IT HAS LEFT BOOTSEL**, which it does 500 ms
    /// after the boot ROM answers: a listing that still shows it is asked again, one that no longer
    /// does ends the wait at once, and one that keeps showing it ends the wait at its bound, saying so.
    #[test]
    fn a_board_restarted_by_reboot2_is_waited_for_until_it_has_left_bootsel() {
        let mut asked = 0;
        let gone = until_gone(
            || {
                asked += 1;
                asked < 4
            },
            std::time::Duration::from_secs(5),
            std::time::Duration::ZERO,
        );
        assert!(gone, "the board left on the fourth listing");
        assert_eq!(asked, 4, "and was not asked again after it had gone");
        let started = std::time::Instant::now();
        let stayed = until_gone(|| true, std::time::Duration::from_millis(30), std::time::Duration::from_millis(5));
        assert!(!stayed, "a board that stays in BOOTSEL ends the wait at its bound");
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "and the bound is kept");
    }

    /// An erase that is not whole sectors from a sector's start, and a write that does not start a
    /// page, are refused before anything is sent; so is a span past the flash's 16 MiB.
    #[test]
    fn a_span_the_boot_rom_would_refuse_is_refused_before_it_is_sent() {
        let mut rom = BootRom::new();
        {
            let mut flash = PicobootFlash::open(&mut rom).expect("opens");
            assert!(flash.erase(SETTINGS_AT + 0x100, UNIT).is_err());
            assert!(flash.erase(SETTINGS_AT, 0x800).is_err());
            assert!(flash.program(SETTINGS_AT + 0x10, &[0; 256]).is_err());
            assert!(flash.read(0xFF_FFFF, 2).is_err());
        }
        let ids: Vec<u8> = rom.commands.iter().map(|command| command.id).collect();
        assert_eq!(ids, [EXIT_XIP, EXCLUSIVE_ACCESS], "nothing after the claim was sent: {:?}", rom.commands);
    }

    /// One interface of a USB listing.
    fn listed(vendor_id: u16, product_id: u16, serial: Option<&str>, product: Option<&str>) -> lamella_usbbulk::InterfaceInfo {
        lamella_usbbulk::InterfaceInfo {
            vendor_id,
            product_id,
            serial_number: serial.map(str::to_owned),
            product: product.map(str::to_owned),
            interface_number: 1,
            interface_name: None,
        }
    }

    /// The boards a listing shows waiting in BOOTSEL are the boot ROMs among its devices, each with the
    /// serial number and the product it reports, in the listing's order; one that reports no serial is
    /// listed too, so the listing can say that nothing names it.
    #[test]
    fn the_boards_in_bootsel_are_the_boot_roms_of_a_listing_with_their_serial_numbers() {
        let listing = [
            listed(BOOTSEL_VENDOR_ID, BOOTSEL_PRODUCT_ID, Some("9B1F2A3C4D5E6F70"), Some("RP2350 Boot")),
            listed(BOOTSEL_VENDOR_ID, 0x000c, Some("E6640000000000A1"), Some("Debugprobe on Pico (CMSIS-DAP)")),
            listed(0x0d28, 0x0204, Some("9904360258994e45"), Some("BBC micro:bit CMSIS-DAP")),
            listed(BOOTSEL_VENDOR_ID, BOOTSEL_PRODUCT_ID, None, None),
        ];
        assert_eq!(
            in_bootsel(&listing),
            [
                InBootsel { serial: Some("9B1F2A3C4D5E6F70".to_owned()), product: Some("RP2350 Boot".to_owned()) },
                InBootsel { serial: None, product: None },
            ]
        );
        assert!(in_bootsel(&listing[1..3]).is_empty(), "a probe of the same vendor is no boot ROM");
    }
}
