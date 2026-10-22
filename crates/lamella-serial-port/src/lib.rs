//! The operating system's serial port: open one by its name with the line settings it should
//! carry, read and write with time-outs, set the modem control lines, discard and count what is
//! buffered, and list the attached ports with the USB identity of the device each belongs to.

#![allow(unsafe_code)]

use std::time::Duration;
#[cfg(any(windows, unix))]
use std::time::Instant;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as sys;

#[cfg(unix)]
mod posix;
#[cfg(unix)]
use posix as sys;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

#[cfg(not(any(windows, unix)))]
mod unsupported;
#[cfg(not(any(windows, unix)))]
use unsupported as sys;

/// The number of data bits in each character.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DataBits {
    /// Five.
    Five,
    /// Six.
    Six,
    /// Seven.
    Seven,
    /// Eight.
    Eight,
}

impl DataBits {
    /// The count as a number, 5 to 8.
    #[must_use]
    pub const fn count(self) -> u8 {
        match self {
            DataBits::Five => 5,
            DataBits::Six => 6,
            DataBits::Seven => 7,
            DataBits::Eight => 8,
        }
    }
}

/// The parity bit each character carries, if any.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Parity {
    /// No parity bit.
    None,
    /// The parity bit makes the count of ones odd.
    Odd,
    /// The parity bit makes the count of ones even.
    Even,
    /// The parity bit is always one.
    Mark,
    /// The parity bit is always zero.
    Space,
}

/// The number of stop bits after each character.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum StopBits {
    /// One.
    One,
    /// One and a half, which a line with five data bits can carry.
    OnePointFive,
    /// Two.
    Two,
}

/// How the two ends of the line pause each other.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FlowControl {
    /// Neither end pauses the other. DTR and RTS belong to the caller.
    None,
    /// In-band: an XOFF character (0x13) pauses the other end and XON (0x11) resumes it, so neither
    /// character can be sent as data.
    XonXoff,
    /// RTS and CTS: the driver drives RTS, and transmission waits while CTS is clear.
    RtsCts,
    /// Both of the above.
    RtsCtsAndXonXoff,
}

impl FlowControl {
    /// Whether the driver runs RTS and CTS.
    #[must_use]
    pub const fn hardware(self) -> bool {
        matches!(self, FlowControl::RtsCts | FlowControl::RtsCtsAndXonXoff)
    }

    /// Whether the driver acts on XON and XOFF characters.
    #[must_use]
    pub const fn software(self) -> bool {
        matches!(self, FlowControl::XonXoff | FlowControl::RtsCtsAndXonXoff)
    }
}

/// The settings a line carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Settings {
    /// The rate, in bits per second.
    pub baud: u32,
    /// The data bits per character.
    pub data_bits: DataBits,
    /// The parity bit.
    pub parity: Parity,
    /// The stop bits.
    pub stop_bits: StopBits,
    /// The flow control.
    pub flow_control: FlowControl,
}

impl Settings {
    /// `baud` with eight data bits, no parity, one stop bit and no flow control.
    #[must_use]
    pub const fn new(baud: u32) -> Settings {
        Settings {
            baud,
            data_bits: DataBits::Eight,
            parity: Parity::None,
            stop_bits: StopBits::One,
            flow_control: FlowControl::None,
        }
    }
}

/// Which of a port's two buffers an operation acts on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Buffer {
    /// What has arrived and not been read.
    Input,
    /// What has been written and not yet sent.
    Output,
    /// Both.
    Both,
}

/// What went wrong with a serial port.
///
/// A `code` is the operating system's own error number for the failure (`GetLastError` on Windows,
/// `errno` elsewhere), or zero where the system reported success and the result was still wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The port could not be opened.
    Open {
        /// The name as given.
        name: String,
        /// The operating system's error code.
        code: u32,
    },
    /// The port did not take a setting: it opened and refused one, or, on Linux and macOS, the
    /// setting is one the system cannot express, which is refused before the port is opened.
    Configure {
        /// Which setting.
        what: &'static str,
        /// The operating system's error code.
        code: u32,
    },
    /// A modem control line could not be changed.
    Signal {
        /// Which line, and in which direction.
        what: &'static str,
        /// The operating system's error code.
        code: u32,
    },
    /// A write failed.
    Write {
        /// The operating system's error code.
        code: u32,
    },
    /// **A write moved nothing within its bound**: the transmitter never became ready, which is flow
    /// control holding it or a line that is not draining, rather than a bad port.
    WriteStalled {
        /// How many bytes had gone out before it stalled.
        after: usize,
    },
    /// A read failed.
    Read {
        /// The operating system's error code.
        code: u32,
    },
    /// A buffer could not be counted, discarded or drained.
    Buffer {
        /// Which operation.
        what: &'static str,
        /// The operating system's error code.
        code: u32,
    },
    /// The attached ports could not be listed.
    List {
        /// Which step of the listing.
        what: &'static str,
        /// The operating system's error code.
        code: u32,
    },
    /// This operating system has no serial port implementation here.
    UnsupportedHost,
}

impl Error {
    /// The operating system's error code the failure carries, if it carries a nonzero one.
    #[must_use]
    pub fn os_code(&self) -> Option<u32> {
        let code = match self {
            Error::Open { code, .. }
            | Error::Configure { code, .. }
            | Error::Signal { code, .. }
            | Error::Write { code }
            | Error::Read { code }
            | Error::Buffer { code, .. }
            | Error::List { code, .. } => *code,
            Error::WriteStalled { .. } | Error::UnsupportedHost => 0,
        };
        (code != 0).then_some(code)
    }

    /// The failure in the standard library's terms: the kind it gives the operating system's error
    /// code, `TimedOut` for a stalled write, `Unsupported` for a host with no implementation, and
    /// `Other` for a failure that carries no code.
    ///
    /// A port that does not exist is `NotFound` on every platform, and one that is in use or may not
    /// be opened is `PermissionDenied` or `ResourceBusy`.
    #[must_use]
    pub fn kind(&self) -> std::io::ErrorKind {
        match self {
            Error::WriteStalled { .. } => std::io::ErrorKind::TimedOut,
            Error::UnsupportedHost => std::io::ErrorKind::Unsupported,
            _ => match self.os_code().and_then(|code| i32::try_from(code).ok()) {
                Some(code) => std::io::Error::from_raw_os_error(code).kind(),
                None => std::io::ErrorKind::Other,
            },
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Open { name, code } => write!(f, "cannot open {name} (error {code})"),
            Error::Configure { what, code } => write!(f, "cannot set {what} (error {code})"),
            Error::Signal { what, code } => write!(f, "cannot {what} (error {code})"),
            Error::Write { code } => write!(f, "write failed (error {code})"),
            Error::WriteStalled { after } => {
                write!(f, "write stalled after {after} bytes -- the transmitter never became ready")
            }
            Error::Read { code } => write!(f, "read failed (error {code})"),
            Error::Buffer { what, code } => write!(f, "cannot {what} (error {code})"),
            Error::List { what, code } => {
                write!(f, "cannot list the serial ports: {what} failed (error {code})")
            }
            Error::UnsupportedHost => {
                write!(f, "serial ports are not implemented for this operating system")
            }
        }
    }
}

impl std::error::Error for Error {}

/// How long [`Port::write_all`] lets a single write wait for the transmitter before calling the write
/// stalled.
///
/// It bounds each wait rather than the whole write, because the two failures differ: a transmitter
/// that never becomes ready is flow control or a dead line, while a large write moving slowly is a
/// slow line and not a fault.
pub const WRITE_STALL: Duration = Duration::from_secs(5);

/// A serial port held open.
///
/// Opening is exclusive: while one `Port` holds a port, another [`Port::open`] of it fails, in this
/// process or another, and so does any other program's open, except on Linux and macOS one by a
/// privileged process. **On Linux and macOS a program that opens the port in the moment before this
/// open has made it exclusive is let in**, unless it locks the port (`flock`) as this crate does.
/// Dropping the `Port` closes it. On Linux, and on macOS through the `/dev/cu.*` node the listing
/// gives, the close leaves DTR and RTS as they were last set, as every other change to them is the
/// caller's own: clear them first where the line must end low. (macOS lowers both at the close of
/// the `/dev/tty.*` node when the carrier line is clear.)
///
/// A `Port` can be moved to another thread and shared between threads on every platform. Reading,
/// writing and every other operation on the line take `&mut self`, so one shared by reference gives
/// out only its name and settings.
#[derive(Debug)]
pub struct Port {
    inner: sys::Port,
    name: String,
    settings: Settings,
}

impl Port {
    /// Opens the port `name` with `settings`.
    ///
    /// The name is the operating system's: `COM8` on Windows (any number, without the `\\.\` prefix
    /// the system needs above COM9, which is added here), and on Linux and macOS a device node, given
    /// either whole (`/dev/ttyACM0`, `/dev/cu.usbmodem1101`) or as a name in `/dev` (`ttyACM0`).
    ///
    /// After the open, DTR and RTS are clear on Windows, except that with hardware flow control RTS
    /// is the driver's, which "raises the RTS line when the 'type-ahead' (input) buffer is less than
    /// one-half full" (DCB), as it is with nothing yet received. On Linux and macOS the terminal
    /// interface asserts both as a port opens (Linux does not when the port's rate is still zero
    /// from its last close), **so there a board that wires either line to its reset can be reset
    /// by the open itself**, before any call here can clear them. Set them with [`Port::set_dtr`]
    /// and [`Port::set_rts`].
    ///
    /// # Errors
    /// [`Error::Open`] when the port cannot be opened or is not a serial port, and
    /// [`Error::Configure`] when it cannot carry `settings` -- among them a setting this operating
    /// system cannot express, which is refused rather than approximated. On Linux and macOS that
    /// refusal comes before the port is opened, so no line is touched for a setting that cannot be
    /// installed, and there it is [`Error::Configure`] even for a name that opens nothing.
    pub fn open(name: &str, settings: &Settings) -> Result<Port, Error> {
        let inner = sys::Port::open(name, settings)?;
        Ok(Port { inner, name: name.to_string(), settings: *settings })
    }

    /// The name the port was opened by.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The settings the line carries.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Reads whatever has arrived into `buffer`, waiting up to `timeout` for the first byte, and
    /// returns how many bytes it read.
    ///
    /// **It returns as soon as anything has arrived**, without waiting out the bound to fill the
    /// buffer. Zero is the bound elapsing with nothing on the line, which is a normal outcome. `None`
    /// waits for the first byte without a bound, and a zero `timeout` takes only what is already
    /// buffered.
    ///
    /// # Errors
    /// [`Error::Read`] when the read fails, and on Linux and macOS when the line has gone away (a
    /// device unplugged or reset, or a hangup), which their terminals report as end-of-file.
    pub fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> Result<usize, Error> {
        if buffer.is_empty() {
            return Ok(0);
        }
        self.inner.read(buffer, timeout)
    }

    /// Writes as much of `bytes` as the line takes within `timeout`, and returns how many bytes it
    /// wrote, at least one. `None` waits until every byte is written.
    ///
    /// # Errors
    /// [`Error::WriteStalled`] when not one byte went out within the bound, and [`Error::Write`]
    /// when the write fails.
    pub fn write(&mut self, bytes: &[u8], timeout: Option<Duration>) -> Result<usize, Error> {
        if bytes.is_empty() {
            return Ok(0);
        }
        self.inner.write(bytes, timeout)
    }

    /// Writes every byte of `bytes`, however long the line takes, unless it stops taking them: a
    /// wait of [`WRITE_STALL`] with nothing sent fails the write.
    ///
    /// # Errors
    /// [`Error::WriteStalled`], with how many bytes went out before the stall, and [`Error::Write`]
    /// when a write fails.
    pub fn write_all(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let mut sent = 0;
        while sent < bytes.len() {
            match self.write(&bytes[sent..], Some(WRITE_STALL)) {
                Ok(count) => sent += count,
                Err(Error::WriteStalled { .. }) => return Err(Error::WriteStalled { after: sent }),
                Err(other) => return Err(other),
            }
        }
        Ok(())
    }

    /// Asserts or clears DTR, in one operating system call.
    ///
    /// # Errors
    /// [`Error::Signal`] when the line cannot be changed.
    pub fn set_dtr(&mut self, on: bool) -> Result<(), Error> {
        self.inner.set_dtr(on)
    }

    /// Asserts or clears RTS, in one operating system call.
    ///
    /// # Errors
    /// [`Error::Signal`] when the line cannot be changed, which includes a port opened with hardware
    /// flow control on Windows, where the driver owns RTS.
    pub fn set_rts(&mut self, on: bool) -> Result<(), Error> {
        self.inner.set_rts(on)
    }

    /// Discards what `which` holds: received bytes not yet read, written bytes not yet sent, or both.
    ///
    /// # Errors
    /// [`Error::Buffer`] when the operating system refuses.
    pub fn discard(&mut self, which: Buffer) -> Result<(), Error> {
        self.inner.discard(which)
    }

    /// How many received bytes are waiting to be read.
    ///
    /// # Errors
    /// [`Error::Buffer`] when the operating system cannot say.
    pub fn bytes_to_read(&mut self) -> Result<usize, Error> {
        self.inner.bytes_to_read()
    }

    /// How many written bytes are still waiting to be sent.
    ///
    /// # Errors
    /// [`Error::Buffer`] when the operating system cannot say.
    pub fn bytes_to_write(&mut self) -> Result<usize, Error> {
        self.inner.bytes_to_write()
    }

    /// Waits until every written byte has been sent.
    ///
    /// # Errors
    /// [`Error::Buffer`] when the wait fails.
    pub fn drain(&mut self) -> Result<(), Error> {
        self.inner.drain()
    }

    /// Closes the port and opens it again by the same name with `settings`.
    ///
    /// Output still waiting is sent before the close. A reopen whose waiting output cannot be sent,
    /// or whose open fails, leaves the port closed: every later operation on it fails, except a
    /// reopen, which tries the open again.
    ///
    /// # Errors
    /// [`Error::Buffer`] when the output still waiting cannot be sent, and what [`Port::open`]
    /// reports.
    pub fn reopen(&mut self, settings: &Settings) -> Result<(), Error> {
        self.inner.close_after_sending()?;
        self.inner = sys::Port::open(&self.name, settings)?;
        self.settings = *settings;
        Ok(())
    }
}

/// A serial port the operating system reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortInfo {
    /// The name that opens it with [`Port::open`]: `COM8`, `/dev/ttyACM0`, `/dev/cu.usbmodem1101`.
    pub name: String,
    /// The USB device the port belongs to, when it belongs to one.
    pub usb: Option<UsbIdentity>,
}

impl PortInfo {
    /// Whether [`Port::open`] given `name` opens this port: by the name listed, and by the other
    /// names the operating system gives the same port -- on Windows its device path, `\\.\COM8`, each
    /// in any case; on Linux and macOS a name in `/dev` given without it, `ttyACM0`, and a link to
    /// the device, such as one in `/dev/serial/by-id`; and on macOS the port's dial-in node,
    /// `/dev/tty.*`.
    #[must_use]
    pub fn is_opened_by(&self, name: &str) -> bool {
        sys::opens_listed(name, &self.name)
    }
}

/// The USB device a serial port belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbIdentity {
    /// The vendor id.
    pub vendor_id: u16,
    /// The product id.
    pub product_id: u16,
    /// The serial number, when the device reports one.
    pub serial_number: Option<String>,
    /// What the port is called: on Windows the name Windows gives the port (`USB Serial Device
    /// (COM8)`), and on Linux and macOS the product string the device reports.
    pub product: Option<String>,
}

/// Lists the serial ports attached now, with the USB identity of each one that belongs to a USB
/// device, in the order of their names with any number in a name compared as a number (`COM3`
/// before `COM11`). Nothing is opened.
///
/// # Errors
/// [`Error::List`] when the operating system's device list cannot be read, and
/// [`Error::UnsupportedHost`] on an operating system this crate cannot list ports on.
pub fn list() -> Result<Vec<PortInfo>, Error> {
    let mut ports = listed()?;
    ports.sort_by(|a, b| name_order(&a.name, &b.name));
    Ok(ports)
}

/// The ports this operating system reports, in its own order.
#[cfg(windows)]
fn listed() -> Result<Vec<PortInfo>, Error> {
    windows::list()
}

/// The ports this operating system reports, in its own order.
#[cfg(target_os = "linux")]
fn listed() -> Result<Vec<PortInfo>, Error> {
    linux::list()
}

/// The ports this operating system reports, in its own order.
#[cfg(target_os = "macos")]
fn listed() -> Result<Vec<PortInfo>, Error> {
    macos::list()
}

/// An operating system this crate cannot list ports on.
#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn listed() -> Result<Vec<PortInfo>, Error> {
    Err(Error::UnsupportedHost)
}

/// The order two port names are listed in: run by run, a run of digits against a run of digits as
/// the numbers they spell and any other pair of runs as text, and the whole names last, so the
/// order is total.
fn name_order(a: &str, b: &str) -> std::cmp::Ordering {
    let (mut left, mut right) = (runs(a), runs(b));
    loop {
        match (left.next(), right.next()) {
            (Some(x), Some(y)) => match run_order(x, y) {
                std::cmp::Ordering::Equal => {}
                decided => return decided,
            },
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return std::cmp::Ordering::Less,
            (Some(_), None) => return std::cmp::Ordering::Greater,
        }
    }
}

/// `name` in runs, each all ASCII digits or all other characters.
fn runs(name: &str) -> impl Iterator<Item = &str> {
    let mut rest = name;
    std::iter::from_fn(move || {
        let digits = rest.chars().next()?.is_ascii_digit();
        let end = rest.find(|c: char| c.is_ascii_digit() != digits).unwrap_or(rest.len());
        let (run, after) = rest.split_at(end);
        rest = after;
        Some(run)
    })
}

/// The order of two runs: as numbers when both are digits, and as text otherwise.
fn run_order(x: &str, y: &str) -> std::cmp::Ordering {
    let number = |run: &str| run.starts_with(|c: char| c.is_ascii_digit());
    if !(number(x) && number(y)) {
        return x.cmp(y);
    }
    let (x, y) = (x.trim_start_matches('0'), y.trim_start_matches('0'));
    x.len().cmp(&y.len()).then_with(|| x.cmp(y))
}

/// When a wait bounded by `timeout` must end: never for `None`, nor for a bound too long to be a
/// point in time, such as `Duration::MAX` given as "forever".
#[cfg(any(windows, unix))]
fn deadline_for(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|bound| Instant::now().checked_add(bound))
}

/// Whether a wait that ends at `deadline` has time left, which a wait without one always has.
///
/// The one rule, on every platform, for whether a bounded read or write goes on: after a wait that
/// ended or was interrupted before its bound did, after a read or a write that found nothing to
/// move though the line was ready for it, and after a write that moved some.
#[cfg(any(windows, unix))]
fn time_left(deadline: Option<Instant>) -> bool {
    deadline.is_none_or(|deadline| Instant::now() < deadline)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defaults are the line almost every device here speaks, so a caller that names only a rate
    /// gets it.
    #[test]
    fn a_rate_alone_is_eight_data_bits_no_parity_one_stop_bit_and_no_flow_control() {
        let settings = Settings::new(115_200);
        assert_eq!(settings.baud, 115_200);
        assert_eq!(settings.data_bits, DataBits::Eight);
        assert_eq!(settings.parity, Parity::None);
        assert_eq!(settings.stop_bits, StopBits::One);
        assert_eq!(settings.flow_control, FlowControl::None);
    }

    /// The two halves of flow control are read independently, so the combined setting has both and
    /// neither single one has the other.
    #[test]
    fn flow_control_says_which_halves_it_runs() {
        assert!(!FlowControl::None.hardware() && !FlowControl::None.software());
        assert!(FlowControl::RtsCts.hardware() && !FlowControl::RtsCts.software());
        assert!(!FlowControl::XonXoff.hardware() && FlowControl::XonXoff.software());
        assert!(FlowControl::RtsCtsAndXonXoff.hardware() && FlowControl::RtsCtsAndXonXoff.software());
    }

    /// A stalled write reads as a time-out and a host without an implementation as unsupported,
    /// whatever their codes; a failure that carries no code is not given one.
    #[test]
    fn an_error_without_an_operating_system_code_has_a_kind_of_its_own() {
        assert_eq!(Error::WriteStalled { after: 3 }.kind(), std::io::ErrorKind::TimedOut);
        assert_eq!(Error::UnsupportedHost.kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(Error::Configure { what: "x", code: 0 }.os_code(), None);
        assert_eq!(Error::Configure { what: "x", code: 0 }.kind(), std::io::ErrorKind::Other);
    }

    /// The code an open failed with is classified as the standard library classifies the platform's
    /// own codes, which is what tells a missing port from one in use.
    #[test]
    fn an_open_failure_is_classified_by_its_code() {
        #[cfg(windows)]
        let (missing, denied) = (2_u32, 5_u32);
        #[cfg(unix)]
        let (missing, denied) = (libc::ENOENT.unsigned_abs(), libc::EACCES.unsigned_abs());
        #[cfg(any(windows, unix))]
        {
            let open = |code| Error::Open { name: String::from("p"), code };
            assert_eq!(open(missing).kind(), std::io::ErrorKind::NotFound);
            assert_eq!(open(denied).kind(), std::io::ErrorKind::PermissionDenied);
        }
    }

    /// Listing never opens a port, so it is safe to run anywhere. What it finds depends on the
    /// machine, so this checks that the list can be read and that every port in it has a name.
    #[cfg(any(windows, target_os = "linux", target_os = "macos"))]
    #[test]
    fn the_port_list_can_be_read() {
        let ports = list().expect("the device list can be read");
        for port in &ports {
            assert!(!port.name.is_empty(), "a port with no name: {port:?}");
        }
    }

    /// **A wait has time left until its deadline, and a wait without one always has**: the rule a
    /// wait that ended or was interrupted early, and a read or a write that found nothing to move,
    /// ask before trying again, so a bounded read waits out its bound rather than reporting silence
    /// early, and a bounded write ends at its bound rather than spinning past it.
    #[cfg(any(windows, unix))]
    #[test]
    fn a_wait_has_time_left_until_its_deadline_and_always_without_one() {
        let now = Instant::now();
        assert!(time_left(None), "no deadline");
        assert!(time_left(Some(now + Duration::from_secs(3600))), "an hour from now");
        assert!(!time_left(Some(now)), "a deadline already reached");
        assert!(time_left(deadline_for(Some(Duration::MAX))), "a bound too long to be a point in time");
    }

    /// A number in a name sorts as a number, so COM3 comes before COM11, and names without one
    /// still have an order.
    #[test]
    fn names_sort_with_their_numbers_compared_as_numbers() {
        let mut names = ["COM11", "COM3", "/dev/ttyUSB10", "/dev/ttyACM0", "/dev/ttyUSB2", "COM1"];
        names.sort_by(|a, b| name_order(a, b));
        assert_eq!(names, ["/dev/ttyACM0", "/dev/ttyUSB2", "/dev/ttyUSB10", "COM1", "COM3", "COM11"]);
    }

    /// **A port can be moved to and shared with another thread on every platform**, so code that
    /// needs either builds on all of them alike rather than on all but Windows.
    #[test]
    fn a_port_can_be_sent_and_shared_between_threads() {
        fn sent_and_shared<T: Send + Sync>() {}
        sent_and_shared::<Port>();
    }

    /// **A number anywhere in a name sorts as a number**, not only one at its end: a converter's
    /// serial number sits mid-name in its macOS node, and `A2B` comes before `A10B`. A number too
    /// long for any integer type still compares, and one spelled with leading zeros sorts beside
    /// the same number without them, the whole names breaking the tie.
    #[test]
    fn a_number_inside_a_name_sorts_as_a_number_too() {
        let mut names = [
            "/dev/cu.usbserial-A10B",
            "/dev/cu.usbserial-A2B",
            "/dev/cu.usbmodem99999999999999999999999",
            "/dev/cu.usbmodem100000000000000000000000",
            "COM02",
            "COM2",
            "COM10",
        ];
        names.sort_by(|a, b| name_order(a, b));
        assert_eq!(
            names,
            [
                "/dev/cu.usbmodem99999999999999999999999",
                "/dev/cu.usbmodem100000000000000000000000",
                "/dev/cu.usbserial-A2B",
                "/dev/cu.usbserial-A10B",
                "COM02",
                "COM2",
                "COM10",
            ]
        );
    }
}
