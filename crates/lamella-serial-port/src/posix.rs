//! A serial port on Linux and macOS, over the POSIX terminal interface.

use std::ffi::CString;
use std::io;
use std::time::{Duration, Instant};

use crate::{Buffer, DataBits, Error, Parity, Settings, StopBits, deadline_for, time_left};

/// The XON and XOFF characters, DC1 and DC3: the START and STOP characters the terminal interface
/// acts on when software flow control is on.
const XON: libc::cc_t = 0x11;
const XOFF: libc::cc_t = 0x13;

/// The most recent operating system error number, as the error variants carry it.
fn errno() -> u32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0).unsigned_abs()
}

/// Whether the last operating system error was `code`.
fn last_error_was(code: libc::c_int) -> bool {
    io::Error::last_os_error().raw_os_error() == Some(code)
}

/// The terminal speed for `baud` on Darwin, where a speed is the rate itself.
///
/// Darwin's `speed_t` holds the rate (its own `B115200` is defined as 115200), so every rate passes
/// straight through and the driver decides whether it can run it -- except zero, which is `B0`,
/// and with it "the modem control lines shall no longer be asserted. Normally, this shall
/// disconnect the line" (POSIX tcsetattr). Zero is a hangup rather than a rate, so it is refused,
/// as the Linux table below refuses it by not naming it.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn speed_for(baud: u32) -> Option<libc::speed_t> {
    (baud != 0).then(|| libc::speed_t::from(baud))
}

/// The terminal speed for `baud` elsewhere, where a speed is a token for a rate.
///
/// Linux's `speed_t` is an opaque token (its `B115200` is `0o010002`), so a rate has to be one the
/// interface names, and one it does not is refused rather than rounded to a neighbor: a line running
/// at a rate the caller did not ask for produces framing errors that read as a broken cable.
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn speed_for(baud: u32) -> Option<libc::speed_t> {
    Some(match baud {
        50 => libc::B50,
        75 => libc::B75,
        110 => libc::B110,
        134 => libc::B134,
        150 => libc::B150,
        200 => libc::B200,
        300 => libc::B300,
        600 => libc::B600,
        1_200 => libc::B1200,
        1_800 => libc::B1800,
        2_400 => libc::B2400,
        4_800 => libc::B4800,
        9_600 => libc::B9600,
        19_200 => libc::B19200,
        38_400 => libc::B38400,
        57_600 => libc::B57600,
        115_200 => libc::B115200,
        230_400 => libc::B230400,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        460_800 => libc::B460800,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        500_000 => libc::B500000,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        576_000 => libc::B576000,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        921_600 => libc::B921600,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        1_000_000 => libc::B1000000,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        1_152_000 => libc::B1152000,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        1_500_000 => libc::B1500000,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        2_000_000 => libc::B2000000,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        2_500_000 => libc::B2500000,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        3_000_000 => libc::B3000000,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        3_500_000 => libc::B3500000,
        #[cfg(any(target_os = "linux", target_os = "android"))]
        4_000_000 => libc::B4000000,
        _ => return None,
    })
}

/// The control-mode bits for mark or space parity, where the terminal interface can express them.
///
/// Mark and space parity are not POSIX. Linux expresses them as `CMSPAR` with `PARENB`, `PARODD`
/// choosing mark; Darwin has no such bit, and a line asked for either is refused there.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn stick_parity(mark: bool) -> Option<libc::tcflag_t> {
    Some(libc::PARENB | libc::CMSPAR | if mark { libc::PARODD } else { 0 })
}

/// The control-mode bits for mark or space parity, which this operating system cannot express.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn stick_parity(_mark: bool) -> Option<libc::tcflag_t> {
    None
}

/// Every control-mode bit that carries parity, so the read-back compares exactly what was asked.
fn parity_bits() -> libc::tcflag_t {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        libc::PARENB | libc::PARODD | libc::CMSPAR
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        libc::PARENB | libc::PARODD
    }
}

/// The terminal settings for `settings`, built from zero, or the first setting this operating
/// system cannot express.
fn terminal_settings(settings: &Settings) -> Result<libc::termios, Error> {
    let speed = speed_for(settings.baud).ok_or(Error::Configure {
        what: "that baud rate, which this operating system's terminal interface does not name",
        code: 0,
    })?;
    let mut terminal: libc::termios = unsafe { std::mem::zeroed() };
    terminal.c_cflag = libc::CREAD | libc::CLOCAL;
    terminal.c_cflag |= match settings.data_bits {
        DataBits::Five => libc::CS5,
        DataBits::Six => libc::CS6,
        DataBits::Seven => libc::CS7,
        DataBits::Eight => libc::CS8,
    };
    terminal.c_cflag |= match settings.parity {
        Parity::None => 0,
        Parity::Odd => libc::PARENB | libc::PARODD,
        Parity::Even => libc::PARENB,
        Parity::Mark | Parity::Space => stick_parity(settings.parity == Parity::Mark).ok_or(
            Error::Configure { what: "mark or space parity, which this operating system's terminal interface cannot express", code: 0 },
        )?,
    };
    terminal.c_cflag |= match settings.stop_bits {
        StopBits::One => 0,
        StopBits::Two => libc::CSTOPB,
        StopBits::OnePointFive => {
            return Err(Error::Configure {
                what: "1.5 stop bits, which the terminal interface cannot express",
                code: 0,
            });
        }
    };
    if settings.flow_control.hardware() {
        terminal.c_cflag |= libc::CRTSCTS;
    }
    if settings.flow_control.software() {
        terminal.c_iflag |= libc::IXON | libc::IXOFF;
        terminal.c_cc[libc::VSTART] = XON;
        terminal.c_cc[libc::VSTOP] = XOFF;
    }
    if unsafe { libc::cfsetispeed(&mut terminal, speed) } != 0
        || unsafe { libc::cfsetospeed(&mut terminal, speed) } != 0
    {
        return Err(Error::Configure { what: "the baud rate", code: errno() });
    }
    Ok(terminal)
}

/// The path a port name opens: the name itself when it is a path, and a node in `/dev` when it has
/// no leading `/`, so `ttyUSB0` and `/dev/ttyUSB0` name the same port.
fn device_path(name: &str) -> String {
    if name.starts_with('/') { name.to_string() } else { format!("/dev/{name}") }
}

/// Whether opening `given` opens the port the listing names `listed`.
pub(crate) fn opens_listed(given: &str, listed: &str) -> bool {
    let given = device_path(given);
    let given = std::fs::canonicalize(&given).map_or(given, |path| path.to_string_lossy().into_owned());
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    let given = match given.strip_prefix("/dev/tty.") {
        Some(name) => format!("/dev/cu.{name}"),
        None => given,
    };
    given == device_path(listed)
}

/// A serial port held open.
#[derive(Debug)]
pub(crate) struct Port {
    /// The open descriptor, or -1 once closed.
    fd: libc::c_int,
}

impl Port {
    /// Opens `name` with `settings`.
    ///
    /// A name with no leading `/` is a device node in `/dev`, so `ttyUSB0` and `/dev/ttyUSB0` name
    /// the same port. The port is opened without becoming the process's controlling terminal, so a
    /// hangup on the line cannot signal the program, and without waiting on carrier detect, so a
    /// device that never raises it opens at once.
    pub(crate) fn open(name: &str, settings: &Settings) -> Result<Port, Error> {
        let Ok(c_path) = CString::new(device_path(name)) else {
            return Err(Error::Open { name: name.to_string(), code: libc::EINVAL.unsigned_abs() });
        };
        let wanted = terminal_settings(settings)?;
        let fd = unsafe {
            libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC)
        };
        if fd < 0 {
            return Err(Error::Open { name: name.to_string(), code: errno() });
        }
        let port = Port { fd };
        if unsafe { libc::isatty(fd) } != 1 {
            return Err(Error::Open { name: name.to_string(), code: libc::ENOTTY.unsigned_abs() });
        }
        let locked = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;
        if !locked && last_error_was(libc::EWOULDBLOCK) {
            port.close_unsent();
            return Err(Error::Open { name: name.to_string(), code: libc::EBUSY.unsigned_abs() });
        }
        if unsafe { libc::ioctl(fd, libc::TIOCEXCL as _) } != 0 {
            return Err(Error::Configure { what: "exclusive access", code: errno() });
        }
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &wanted) } != 0 {
            return Err(Error::Configure { what: "the line settings", code: errno() });
        }
        port.confirm(&wanted)?;
        Ok(port)
    }

    /// Reads the line settings back and checks the ones a caller depends on.
    ///
    /// Applying settings is documented to succeed when only some of them were applied, so this is
    /// the step that turns that into an answer. A driver is free to differ in fields not checked here.
    fn confirm(&self, wanted: &libc::termios) -> Result<(), Error> {
        let mut actual: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(self.fd, &mut actual) } != 0 {
            return Err(Error::Configure { what: "a read-back of the line settings", code: errno() });
        }
        let same = |field: libc::tcflag_t, mask: libc::tcflag_t| field & mask;
        let checks: [(bool, &'static str); 8] = [
            (
                same(actual.c_cflag, libc::CSIZE) == same(wanted.c_cflag, libc::CSIZE),
                "the data bits",
            ),
            (
                same(actual.c_cflag, parity_bits()) == same(wanted.c_cflag, parity_bits()),
                "the parity",
            ),
            (
                same(actual.c_cflag, libc::CSTOPB) == same(wanted.c_cflag, libc::CSTOPB),
                "the stop bits",
            ),
            (
                same(actual.c_cflag, libc::CRTSCTS) == same(wanted.c_cflag, libc::CRTSCTS),
                "the hardware flow control",
            ),
            (actual.c_cflag & libc::CLOCAL != 0, "the carrier line ignored"),
            (
                same(actual.c_iflag, libc::IXON | libc::IXOFF | libc::IXANY)
                    == same(wanted.c_iflag, libc::IXON | libc::IXOFF | libc::IXANY),
                "the software flow control",
            ),
            (
                actual.c_cc[libc::VMIN] == 0 && actual.c_cc[libc::VTIME] == 0,
                "reads bounded by poll rather than by the terminal",
            ),
            (
                unsafe { libc::cfgetospeed(&actual) } == unsafe { libc::cfgetospeed(wanted) },
                "that baud rate on this line",
            ),
        ];
        for (held, what) in checks {
            if !held {
                return Err(Error::Configure { what, code: 0 });
            }
        }
        Ok(())
    }

    /// Waits up to `timeout` (without a bound for `None`) for any of `events`, and reports which
    /// became ready: zero when the bound elapsed with nothing.
    fn wait(&self, events: libc::c_short, timeout: Option<Duration>) -> Result<libc::c_short, Error> {
        self.wait_in_polls_of(events, timeout, libc::c_int::MAX)
    }

    /// [`Port::wait`], in polls of at most `longest` milliseconds each.
    ///
    /// `poll` takes its bound as an `int` of milliseconds, so a bound longer than `c_int::MAX` of
    /// them (about 24.9 days) is waited out in several polls. The tests pass a short `longest` to
    /// see that happen.
    fn wait_in_polls_of(
        &self,
        events: libc::c_short,
        timeout: Option<Duration>,
        longest: libc::c_int,
    ) -> Result<libc::c_short, Error> {
        if self.fd < 0 {
            return Err(Error::Read { code: libc::EBADF.unsigned_abs() });
        }
        let deadline = deadline_for(timeout);
        loop {
            let ms = match deadline {
                None => -1,
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    let ms = remaining.as_nanos().div_ceil(1_000_000);
                    libc::c_int::try_from(ms).map_or(longest, |ms| ms.min(longest))
                }
            };
            let mut watched = libc::pollfd { fd: self.fd, events, revents: 0 };
            let ready = unsafe { libc::poll(&mut watched, 1, ms) };
            if ready < 0 {
                if last_error_was(libc::EINTR) {
                    if time_left(deadline) {
                        continue;
                    }
                    return Ok(0);
                }
                return Err(Error::Read { code: errno() });
            }
            if ready == 0 && time_left(deadline) {
                continue;
            }
            return Ok(if ready > 0 { watched.revents } else { 0 });
        }
    }

    pub(crate) fn read(&mut self, buffer: &mut [u8], timeout: Option<Duration>) -> Result<usize, Error> {
        let deadline = deadline_for(timeout);
        loop {
            let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let ready = self.wait(libc::POLLIN, remaining)?;
            if ready & libc::POLLIN == 0 {
                if ready & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(Error::Read { code: libc::EIO.unsigned_abs() });
                }
                return Ok(0);
            }
            let got = unsafe {
                libc::read(self.fd, buffer.as_mut_ptr().cast::<libc::c_void>(), buffer.len())
            };
            if got < 0 {
                if last_error_was(libc::EINTR) || last_error_was(libc::EAGAIN) {
                    if time_left(deadline) {
                        continue;
                    }
                    return Ok(0);
                }
                return Err(Error::Read { code: errno() });
            }
            if got == 0 {
                return Err(Error::Read { code: libc::EIO.unsigned_abs() });
            }
            return Ok(usize::try_from(got).unwrap_or(0));
        }
    }

    pub(crate) fn write(&mut self, bytes: &[u8], timeout: Option<Duration>) -> Result<usize, Error> {
        let deadline = deadline_for(timeout);
        let mut sent = 0;
        while sent < bytes.len() {
            let remaining = deadline.map(|deadline| deadline.saturating_duration_since(Instant::now()));
            let ready = self.wait(libc::POLLOUT, remaining).map_err(|error| match error {
                Error::Read { code } => Error::Write { code },
                other => other,
            })?;
            if ready == 0 {
                break;
            }
            if ready & libc::POLLOUT == 0 {
                return Err(Error::Write { code: libc::EIO.unsigned_abs() });
            }
            let moved = unsafe {
                libc::write(self.fd, bytes[sent..].as_ptr().cast::<libc::c_void>(), bytes.len() - sent)
            };
            if moved < 0 {
                if last_error_was(libc::EINTR) || last_error_was(libc::EAGAIN) {
                    if time_left(deadline) {
                        continue;
                    }
                    break;
                }
                return Err(Error::Write { code: errno() });
            }
            sent += usize::try_from(moved).unwrap_or(0);
            if !time_left(deadline) {
                break;
            }
        }
        if sent == 0 {
            return Err(Error::WriteStalled { after: 0 });
        }
        Ok(sent)
    }

    /// Sets or clears one modem control line, in one call.
    fn signal(&mut self, bit: libc::c_int, on: bool, what: &'static str) -> Result<(), Error> {
        let request = if on { libc::TIOCMBIS } else { libc::TIOCMBIC };
        if unsafe { libc::ioctl(self.fd, request, &bit) } != 0 {
            return Err(Error::Signal { what, code: errno() });
        }
        Ok(())
    }

    pub(crate) fn set_dtr(&mut self, on: bool) -> Result<(), Error> {
        self.signal(libc::TIOCM_DTR, on, if on { "assert DTR" } else { "clear DTR" })
    }

    pub(crate) fn set_rts(&mut self, on: bool) -> Result<(), Error> {
        self.signal(libc::TIOCM_RTS, on, if on { "assert RTS" } else { "clear RTS" })
    }

    pub(crate) fn discard(&mut self, which: Buffer) -> Result<(), Error> {
        let queue = match which {
            Buffer::Input => libc::TCIFLUSH,
            Buffer::Output => libc::TCOFLUSH,
            Buffer::Both => libc::TCIOFLUSH,
        };
        if unsafe { libc::tcflush(self.fd, queue) } != 0 {
            return Err(Error::Buffer { what: "discard the buffers", code: errno() });
        }
        Ok(())
    }

    /// One queue-count request: `FIONREAD` for the input queue, `TIOCOUTQ` for the output queue.
    fn queued(&mut self, request: libc::c_ulong, what: &'static str) -> Result<usize, Error> {
        let mut count: libc::c_int = 0;
        if unsafe { libc::ioctl(self.fd, request as _, &mut count) } != 0 {
            return Err(Error::Buffer { what, code: errno() });
        }
        Ok(usize::try_from(count).unwrap_or(0))
    }

    pub(crate) fn bytes_to_read(&mut self) -> Result<usize, Error> {
        self.queued(libc::FIONREAD as libc::c_ulong, "count the input buffer")
    }

    pub(crate) fn bytes_to_write(&mut self) -> Result<usize, Error> {
        self.queued(libc::TIOCOUTQ as libc::c_ulong, "count the output buffer")
    }

    pub(crate) fn drain(&mut self) -> Result<(), Error> {
        loop {
            if unsafe { libc::tcdrain(self.fd) } == 0 {
                return Ok(());
            }
            if !last_error_was(libc::EINTR) {
                return Err(Error::Buffer { what: "drain the output", code: errno() });
            }
        }
    }

    /// Closes the descriptor without sending what is waiting, for an open that gives the port up to
    /// another open of it: the output waiting is the other's, and a dropped port drains it on Darwin
    /// and, when the drain runs out of time, discards it.
    fn close_unsent(mut self) {
        unsafe { libc::close(self.fd) };
        self.fd = -1;
    }

    /// Sends what is waiting, then closes, leaving nothing open, and reports whether the output was
    /// sent. The port is closed either way.
    pub(crate) fn close_after_sending(&mut self) -> Result<(), Error> {
        if self.fd < 0 {
            return Ok(());
        }
        let sent = self.drain();
        unsafe { libc::close(self.fd) };
        self.fd = -1;
        sent
    }
}

/// How long a port being dropped waits for written bytes to leave before it closes.
#[cfg(any(target_os = "macos", target_os = "ios"))]
const DRAIN_BEFORE_CLOSE: Duration = Duration::from_secs(2);

/// How long a drain that outlasted its bound is given to end once the output is discarded.
#[cfg(any(target_os = "macos", target_os = "ios"))]
const DRAIN_AFTER_DISCARD: Duration = Duration::from_millis(500);

#[cfg(any(target_os = "macos", target_os = "ios"))]
impl Port {
    /// Waits, at most `bound`, for every written byte to be sent, and discards what is left when
    /// the bound runs out. Reports whether the wait ended, which it does unless the system leaves
    /// a drain blocked even after the discard.
    fn drain_within(&self, bound: Duration) -> bool {
        let duplicate = unsafe { libc::fcntl(self.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return false;
        }
        let (finished, ended) = std::sync::mpsc::channel();
        let drainer = std::thread::Builder::new().name(String::from("serial port drain")).spawn(move || {
            while unsafe { libc::tcdrain(duplicate) } != 0 && last_error_was(libc::EINTR) {}
            unsafe { libc::close(duplicate) };
            let _ = finished.send(());
        });
        if drainer.is_err() {
            unsafe { libc::close(duplicate) };
            return false;
        }
        if ended.recv_timeout(bound).is_ok() {
            return true;
        }
        unsafe { libc::tcflush(self.fd, libc::TCOFLUSH) };
        ended.recv_timeout(DRAIN_AFTER_DISCARD).is_ok()
    }
}

impl Drop for Port {
    fn drop(&mut self) {
        if self.fd >= 0 {
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            let _ = self.drain_within(DRAIN_BEFORE_CLOSE);
            unsafe { libc::close(self.fd) };
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::thread;

    use super::*;
    use crate::FlowControl;

    /// A pseudo-terminal pair: the end the test drives, and the device path a [`Port`] opens.
    ///
    /// A pseudo-terminal is a real terminal as far as this file is concerned -- it answers `isatty`,
    /// carries line settings, and `poll` reports it readable exactly when bytes are waiting -- so the
    /// settings and the timing below are exercised against the operating system rather than a
    /// stand-in. What it cannot do is modem control lines, so nothing here asserts about DTR or RTS.
    struct Loopback {
        /// The controlling end, which the test writes to and reads from.
        end: libc::c_int,
        /// The device path of the other end.
        path: String,
    }

    impl Loopback {
        fn open() -> Loopback {
            let end = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
            assert!(end >= 0, "posix_openpt: {}", io::Error::last_os_error());
            assert_eq!(unsafe { libc::grantpt(end) }, 0, "grantpt: {}", io::Error::last_os_error());
            assert_eq!(unsafe { libc::unlockpt(end) }, 0, "unlockpt: {}", io::Error::last_os_error());
            static NAMING: std::sync::Mutex<()> = std::sync::Mutex::new(());
            let path = {
                let _held = NAMING.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                let name = unsafe { libc::ptsname(end) };
                assert!(!name.is_null(), "ptsname: {}", io::Error::last_os_error());
                unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned()
            };
            Loopback { end, path }
        }

        /// Opens the port's end as another program might: taking no lock, and leaving the terminal
        /// as it finds it.
        fn open_without_a_lock(&self) -> io::Result<OwnedFd> {
            let path = CString::new(self.path.clone()).expect("a device path has no NUL");
            let fd = unsafe {
                libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK | libc::O_CLOEXEC)
            };
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(unsafe { OwnedFd::from_raw_fd(fd) })
        }

        /// Puts bytes on the line for the port to find.
        fn send(&self, bytes: &[u8]) {
            let moved =
                unsafe { libc::write(self.end, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
            assert_eq!(usize::try_from(moved).unwrap_or(0), bytes.len(), "the whole slice went out");
        }

        /// Takes whatever the port has written.
        fn receive(&self) -> Vec<u8> {
            let mut buffer = [0u8; 256];
            let got = unsafe {
                libc::read(self.end, buffer.as_mut_ptr().cast::<libc::c_void>(), buffer.len())
            };
            assert!(got >= 0, "read: {}", io::Error::last_os_error());
            buffer[..usize::try_from(got).unwrap_or(0)].to_vec()
        }

        /// Takes what the port writes until `count` bytes have arrived or `bound` has passed.
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        fn receive_until(&self, count: usize, bound: Duration) -> Vec<u8> {
            let until = Instant::now() + bound;
            let mut taken = Vec::new();
            let mut buffer = [0u8; 256];
            while taken.len() < count && Instant::now() < until {
                let mut watched = libc::pollfd { fd: self.end, events: libc::POLLIN, revents: 0 };
                if unsafe { libc::poll(&mut watched, 1, 50) } <= 0 {
                    continue;
                }
                let got = unsafe {
                    libc::read(self.end, buffer.as_mut_ptr().cast::<libc::c_void>(), buffer.len())
                };
                if got <= 0 {
                    break;
                }
                taken.extend_from_slice(&buffer[..usize::try_from(got).unwrap_or(0)]);
            }
            taken
        }
    }

    impl Drop for Loopback {
        fn drop(&mut self) {
            unsafe { libc::close(self.end) };
        }
    }

    /// The settings are installed and read back, which is what makes the install meaningful.
    #[test]
    fn a_terminal_opens_with_its_settings_read_back() {
        let line = Loopback::open();
        Port::open(&line.path, &Settings::new(115_200)).expect("a pseudo-terminal configures");
    }

    /// **A read that is answered returns as soon as the bytes arrive, not when its bound expires.**
    #[test]
    fn a_read_returns_when_the_bytes_arrive_and_not_when_its_bound_expires() {
        let line = Loopback::open();
        let mut port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        let sent: &[u8] = b"\xc0\x01\x02\xc0";
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(50));
                line.send(sent);
            });
            let mut buffer = [0u8; 64];
            let started = Instant::now();
            let got = port.read(&mut buffer, Some(Duration::from_secs(10))).expect("a read succeeds");
            let took = started.elapsed();
            assert_eq!(&buffer[..got], sent, "the bytes arrive intact");
            assert!(took < Duration::from_secs(5), "returned on the data, but took {took:?}");
        });
    }

    /// A read with no bound waits for the first byte however long it takes, and returns it.
    #[test]
    fn a_read_without_a_bound_waits_for_the_first_byte() {
        let line = Loopback::open();
        let mut port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(300));
                line.send(b"late");
            });
            let mut buffer = [0u8; 64];
            let got = port.read(&mut buffer, None).expect("a read succeeds");
            assert_eq!(&buffer[..got], b"late");
        });
    }

    /// **A bound too long to be a point in time waits as no bound does**, rather than panicking:
    /// `Duration::MAX` is a caller's "forever", and the Windows half takes it too.
    #[test]
    fn a_bound_too_long_to_be_a_point_in_time_waits_as_no_bound_does() {
        let line = Loopback::open();
        let mut port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(100));
                line.send(b"late");
            });
            let mut buffer = [0u8; 64];
            let got = port.read(&mut buffer, Some(Duration::MAX)).expect("a read succeeds");
            assert_eq!(&buffer[..got], b"late");
        });
        assert_eq!(port.write(b"x", Some(Duration::MAX)).expect("a write succeeds"), 1);
        assert_eq!(line.receive(), b"x", "the byte arrived");
    }

    /// **A bound longer than one `poll` can carry is waited out in several**, rather than ending
    /// with the first: here every poll is held to 20 ms, as one past `c_int::MAX` milliseconds is
    /// held to that, and a silent line's 250 ms bound still runs its length.
    #[test]
    fn a_bound_longer_than_one_poll_is_waited_out_in_several() {
        let line = Loopback::open();
        let port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        let started = Instant::now();
        let ready = port
            .wait_in_polls_of(libc::POLLIN, Some(Duration::from_millis(250)), 20)
            .expect("a wait on a silent line succeeds");
        let took = started.elapsed();
        assert_eq!(ready, 0, "nothing arrived");
        assert!(took >= Duration::from_millis(200), "the bound ran its length, but took {took:?}");
    }

    /// **A line that goes away is an error, not silence**: closing the controlling end hangs the
    /// terminal up (Linux pty.c, pty_close; xnu tty_dev.c, ptcclose), and the read after it fails
    /// at once, with a bound or without one.
    #[test]
    fn a_read_after_the_line_hangs_up_fails_at_once() {
        let line = Loopback::open();
        let mut port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        drop(line);
        let mut buffer = [0u8; 64];
        for timeout in [Some(Duration::from_secs(5)), None] {
            let started = Instant::now();
            let read = port.read(&mut buffer, timeout);
            assert!(matches!(read, Err(Error::Read { .. })), "{timeout:?}: got {read:?}");
            let took = started.elapsed();
            assert!(took < Duration::from_secs(2), "{timeout:?}: took {took:?}");
        }
    }

    /// Silence is zero bytes after the bound, and the bound is honored rather than collapsing.
    #[test]
    fn a_silent_line_reports_nothing_after_its_bound() {
        let line = Loopback::open();
        let mut port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        let mut buffer = [0u8; 64];
        let started = Instant::now();
        let got = port.read(&mut buffer, Some(Duration::from_millis(250))).expect("silence is no error");
        assert_eq!(got, 0, "nothing arrived");
        assert!(started.elapsed() >= Duration::from_millis(200), "it waited");
    }

    /// **A bound shorter than a tenth of a second is still a bound**, which the terminal's own timer
    /// would have turned into "do not wait at all".
    #[test]
    fn a_bound_finer_than_the_terminal_timer_can_express_is_still_honored() {
        let line = Loopback::open();
        let mut port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        let mut buffer = [0u8; 64];
        let started = Instant::now();
        let got = port.read(&mut buffer, Some(Duration::from_millis(30))).expect("silence is no error");
        assert_eq!(got, 0, "nothing arrived");
        assert!(started.elapsed() >= Duration::from_millis(20), "30 ms was a wait, not a poll");
    }

    /// Every byte reaches the line unaltered: output processing and both flow controls are off, so
    /// the bytes a terminal in its default state would rewrite or swallow go out as themselves.
    #[test]
    fn a_write_delivers_every_byte_unaltered() {
        let line = Loopback::open();
        let mut port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        let image: &[u8] = b"\x0a\x11\x13\xc0\xff";
        let sent = port.write(image, Some(Duration::from_secs(1))).expect("the write succeeds");
        assert_eq!(sent, image.len());
        assert_eq!(line.receive(), image, "every byte arrived as itself");
    }

    /// What has arrived is counted before it is read, and discarding drops it, so a sequence that
    /// begins by clearing the line does not read a previous conversation's tail as its answer.
    #[test]
    fn waiting_bytes_are_counted_and_discarding_drops_them() {
        let line = Loopback::open();
        let mut port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        line.send(b"stale chatter");
        thread::sleep(Duration::from_millis(50));
        assert_eq!(port.bytes_to_read().expect("counts"), 13);
        port.discard(Buffer::Input).expect("the flush succeeds");
        assert_eq!(port.bytes_to_read().expect("counts"), 0);
        let mut buffer = [0u8; 64];
        assert_eq!(port.read(&mut buffer, Some(Duration::from_millis(100))).expect("no error"), 0);
        port.write(b"x", Some(Duration::from_secs(1))).expect("writes");
        assert_eq!(line.receive(), b"x", "the byte arrived");
        port.drain().expect("the output drains");
    }

    /// **While one `Port` holds a terminal, another `Port::open` of it is refused**, as a Windows COM
    /// port's is, whoever makes it, and once the first closes, the terminal opens again. Another
    /// program's open, which takes no lock, is refused by the terminal itself unless the program is
    /// privileged (ioctl_tty TIOCEXCL).
    #[test]
    fn a_second_open_is_refused_while_the_first_holds_the_port() {
        let line = Loopback::open();
        let first = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        let Err(error) = Port::open(&line.path, &Settings::new(115_200)) else {
            panic!("a second open succeeded")
        };
        assert!(matches!(error, Error::Open { .. }), "got {error:?}");
        assert_eq!(error.kind(), io::ErrorKind::ResourceBusy, "got {error:?}");
        #[cfg(target_os = "linux")]
        {
            let other = line.open_without_a_lock();
            if unsafe { libc::geteuid() } == 0 {
                assert!(other.is_ok(), "a privileged process is let through: {other:?}");
            } else {
                let error = other.expect_err("an open without a lock succeeded");
                assert_eq!(error.raw_os_error(), Some(libc::EBUSY), "got {error:?}");
            }
        }
        drop(first);
        Port::open(&line.path, &Settings::new(115_200)).expect("opens again once released");
    }

    /// **An open that comes in before the port has been made exclusive is refused by its lock**, so
    /// two opens at the same moment are not both given the port. TIOCEXCL refuses only the opens
    /// after it, and the first opener here is caught between its open and its TIOCEXCL: the
    /// terminal is open and locked, and not yet exclusive.
    #[test]
    fn an_open_that_comes_in_before_the_port_is_exclusive_is_refused_by_the_lock() {
        let line = Loopback::open();
        let first = line.open_without_a_lock().expect("the terminal opens");
        let locked = unsafe { libc::flock(first.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_eq!(locked, 0, "flock: {}", io::Error::last_os_error());
        let Err(error) = Port::open(&line.path, &Settings::new(115_200)) else {
            panic!("the second open was given the port")
        };
        assert!(matches!(error, Error::Open { .. }), "got {error:?}");
        assert_eq!(error.kind(), io::ErrorKind::ResourceBusy, "got {error:?}");
        drop(first);
        Port::open(&line.path, &Settings::new(115_200)).expect("opens once the first lets go");
    }

    /// **An open refused by the lock leaves the holder's output alone**: a port dropped on Darwin
    /// drains what waits to be sent, and discards it when the drain runs out of time, and what
    /// waits on a port another open holds is the holder's.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn an_open_refused_by_the_lock_leaves_the_holders_output_alone() {
        let line = Loopback::open();
        let mut first = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        first.write(b"held", Some(Duration::from_secs(1))).expect("writes");
        let started = Instant::now();
        let refused = Port::open(&line.path, &Settings::new(115_200));
        let took = started.elapsed();
        assert!(matches!(refused, Err(Error::Open { .. })), "got {refused:?}");
        assert!(took < Duration::from_secs(1), "the refusal waited on the holder's output: {took:?}");
        assert_eq!(line.receive_until(4, Duration::from_secs(2)), b"held", "the holder's output is there");
    }

    /// **A port whose reopen failed is closed, and every later read and write on it fails**, with a
    /// bound or without one, rather than waiting on a descriptor that is not there.
    #[test]
    fn a_port_whose_reopen_failed_fails_every_later_read_and_write() {
        let line = Loopback::open();
        let mut port = crate::Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        let refused = port.reopen(&Settings { stop_bits: StopBits::OnePointFive, ..Settings::new(9_600) });
        assert!(matches!(refused, Err(Error::Configure { .. })), "got {refused:?}");
        let mut buffer = [0u8; 8];
        for timeout in [Some(Duration::from_millis(100)), None] {
            let read = port.read(&mut buffer, timeout);
            assert!(matches!(read, Err(Error::Read { .. })), "{timeout:?}: got {read:?}");
            let written = port.write(b"x", timeout);
            assert!(matches!(written, Err(Error::Write { .. })), "{timeout:?}: got {written:?}");
        }
    }

    /// **A reopen whose waiting output cannot be sent fails, and leaves the port closed**, rather
    /// than dropping the output unannounced and opening whatever the name names by then. Once the
    /// line hangs up, the drain before the close fails: a hung-up terminal's only operations are a
    /// read, a write, a poll and a request, and every request but one fails with EIO (Linux
    /// tty_io.c, hung_up_tty_fops and hung_up_tty_ioctl).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_reopen_whose_waiting_output_cannot_be_sent_fails_and_leaves_the_port_closed() {
        let line = Loopback::open();
        let mut port = crate::Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        drop(line);
        let refused = port.reopen(&Settings::new(115_200));
        assert!(matches!(refused, Err(Error::Buffer { what: "drain the output", .. })), "got {refused:?}");
        let mut buffer = [0u8; 8];
        let read = port.read(&mut buffer, Some(Duration::from_millis(100)));
        assert!(matches!(read, Err(Error::Read { .. })), "the port is closed: got {read:?}");
    }

    /// Reopening keeps the name and changes the rate.
    #[test]
    fn a_reopen_takes_the_new_settings() {
        let line = Loopback::open();
        let mut port = crate::Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        port.reopen(&Settings::new(9_600)).expect("reopens");
        assert_eq!(port.settings().baud, 9_600);
        assert_eq!(port.name(), line.path);
    }

    /// **A port dropped right after a write sends it before it closes**: everything written reaches
    /// the other end, though the drop came at once.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn a_port_dropped_right_after_a_write_sends_the_output_first() {
        let line = Loopback::open();
        let mut port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        let sent: Vec<u8> = (0..3000u32).map(|at| (at % 251) as u8).collect();
        thread::scope(|scope| {
            let reader = scope.spawn(|| line.receive_until(sent.len(), Duration::from_secs(5)));
            let mut at = 0;
            while at < sent.len() {
                at += port.write(&sent[at..], Some(Duration::from_secs(5))).expect("writes");
            }
            drop(port);
            assert_eq!(reader.join().expect("the reader"), sent, "every byte arrived");
        });
    }

    /// **A drain that outlasts its bound is ended by discarding the output**, so a dropped port
    /// closes in bounded time though nothing takes what it wrote. A Darwin pseudo-terminal holds
    /// output until its reader takes it, so without the discard this drain would never end.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn a_drain_nothing_answers_is_ended_by_discarding_the_output() {
        let line = Loopback::open();
        let mut port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        port.write(b"never read", Some(Duration::from_secs(1))).expect("writes");
        let started = Instant::now();
        assert!(port.drain_within(Duration::from_millis(200)), "the discard ended the drain");
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(200), "it waited its bound first: {took:?}");
        assert!(took < Duration::from_secs(2), "and was ended at once after it: {took:?}");
        assert_eq!(port.bytes_to_write().expect("counts"), 0, "what was left was discarded");
    }

    /// **The port is closed across an exec**, so a program the caller starts while the port is open
    /// does not keep the terminal after the caller lets go.
    #[test]
    fn the_port_is_not_inherited_by_a_program_the_caller_starts() {
        let line = Loopback::open();
        let port = Port::open(&line.path, &Settings::new(115_200)).expect("configures");
        let flags = unsafe { libc::fcntl(port.fd, libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0, "descriptor flags {flags:#x}");
    }

    /// **A listed port is found by every name that opens it**: the name in `/dev` given without it,
    /// a link to the node (the way udev's `/dev/serial/by-id` names a device), and on Darwin the
    /// dial-in node beside the callout one the listing gives. Another port's name is not it.
    #[test]
    fn a_listed_port_is_found_by_every_name_that_opens_it() {
        let listed = crate::PortInfo { name: String::from("/dev/ttyACM0"), usb: None };
        assert!(listed.is_opened_by("/dev/ttyACM0"));
        assert!(listed.is_opened_by("ttyACM0"));
        assert!(!listed.is_opened_by("ttyACM1"));
        let dir = std::env::temp_dir().join(format!("lamella-serial-port-links-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("by-id")).expect("a directory of the test's own");
        std::fs::write(dir.join("ttyX0"), b"").expect("a node");
        let link = dir.join("by-id").join("usb-Board_0123456789ABCDEF-if00");
        std::os::unix::fs::symlink("../ttyX0", &link).expect("a link");
        let node = dir.join("ttyX0").canonicalize().expect("the node resolves");
        let listed = crate::PortInfo { name: node.to_string_lossy().into_owned(), usb: None };
        let found = listed.is_opened_by(&link.to_string_lossy());
        let other = listed.is_opened_by(&dir.join("ttyX1").to_string_lossy());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(found, "a link to the node opens it");
        assert!(!other, "a different node does not");
        #[cfg(any(target_os = "macos", target_os = "ios"))]
        {
            let callout = crate::PortInfo { name: String::from("/dev/cu.usbmodem1101"), usb: None };
            assert!(callout.is_opened_by("/dev/tty.usbmodem1101"), "the dial-in node opens it");
            assert!(callout.is_opened_by("cu.usbmodem1101"));
            assert!(!callout.is_opened_by("/dev/tty.usbmodem1102"));
        }
    }

    /// A path that is not a terminal is refused at the open rather than configured and driven.
    #[test]
    fn a_path_that_is_not_a_terminal_is_refused() {
        let refused = Port::open("/dev/null", &Settings::new(115_200));
        assert!(matches!(refused, Err(Error::Open { .. })), "got {refused:?}");
    }

    /// A fast rate is installed and reads back, on either platform's spelling of a speed.
    #[test]
    fn a_fast_rate_is_installed_and_reads_back() {
        let line = Loopback::open();
        Port::open(&line.path, &Settings::new(921_600)).expect("a fast rate configures");
    }

    /// Both flow controls are installed and read back when asked for, and only then.
    #[test]
    fn flow_control_is_installed_and_reads_back() {
        let line = Loopback::open();
        for flow_control in [FlowControl::XonXoff, FlowControl::RtsCts, FlowControl::RtsCtsAndXonXoff] {
            Port::open(&line.path, &Settings { flow_control, ..Settings::new(115_200) })
                .unwrap_or_else(|error| panic!("{flow_control:?}: {error}"));
        }
    }

    /// 1.5 stop bits have no spelling in the terminal interface, so they are refused, before the port
    /// is opened at all.
    #[test]
    fn one_and_a_half_stop_bits_are_refused() {
        let line = Loopback::open();
        let refused =
            Port::open(&line.path, &Settings { stop_bits: StopBits::OnePointFive, ..Settings::new(9_600) });
        assert!(matches!(refused, Err(Error::Configure { code: 0, .. })), "got {refused:?}");
    }

    /// A rate the terminal interface does not name is refused rather than rounded to a neighbor.
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    #[test]
    fn a_rate_this_platform_cannot_express_is_refused_rather_than_rounded() {
        let line = Loopback::open();
        let refused = Port::open(&line.path, &Settings::new(100_000));
        assert!(matches!(refused, Err(Error::Configure { .. })), "got {refused:?}");
    }

    /// **A rate of zero is refused on both systems**: it is `B0`, the hangup, so an open given it
    /// would drop the modem control lines rather than run the line.
    #[test]
    fn a_rate_of_zero_is_refused_as_the_hangup_it_would_be() {
        let line = Loopback::open();
        let refused = Port::open(&line.path, &Settings::new(0));
        assert!(matches!(refused, Err(Error::Configure { .. })), "got {refused:?}");
    }

    /// **A line that does not take a setting is refused rather than driven.** A Linux
    /// pseudo-terminal forces eight data bits and no parity, so asking it for seven bits and even
    /// parity is a setting that reported success and is not there -- exactly what the read-back is
    /// for.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_line_that_drops_a_setting_is_refused() {
        let line = Loopback::open();
        let refused = Port::open(
            &line.path,
            &Settings { data_bits: DataBits::Seven, parity: Parity::Even, ..Settings::new(9_600) },
        );
        assert!(matches!(refused, Err(Error::Configure { code: 0, .. })), "got {refused:?}");
    }

    /// Mark and space parity have no spelling on Darwin, so they are refused there before the open.
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    #[test]
    fn mark_and_space_parity_are_refused_where_they_cannot_be_expressed() {
        let line = Loopback::open();
        for parity in [Parity::Mark, Parity::Space] {
            let refused = Port::open(&line.path, &Settings { parity, ..Settings::new(9_600) });
            assert!(matches!(refused, Err(Error::Configure { code: 0, .. })), "{parity:?}: {refused:?}");
        }
    }

    /// The terminal settings name exactly what was asked, and mark and space parity carry the stick
    /// bit where the interface has one.
    #[test]
    fn the_terminal_settings_carry_each_setting() {
        let two = terminal_settings(&Settings { stop_bits: StopBits::Two, ..Settings::new(9_600) })
            .expect("expressible");
        assert_ne!(two.c_cflag & libc::CSTOPB, 0);
        assert_eq!(two.c_cflag & libc::CSIZE, libc::CS8);
        let odd = terminal_settings(&Settings { parity: Parity::Odd, ..Settings::new(9_600) })
            .expect("expressible");
        assert_eq!(odd.c_cflag & (libc::PARENB | libc::PARODD), libc::PARENB | libc::PARODD);
        let software =
            terminal_settings(&Settings { flow_control: FlowControl::XonXoff, ..Settings::new(9_600) })
                .expect("expressible");
        assert_eq!(software.c_iflag & (libc::IXON | libc::IXOFF), libc::IXON | libc::IXOFF);
        assert_eq!((software.c_cc[libc::VSTART], software.c_cc[libc::VSTOP]), (0x11, 0x13));
        #[cfg(target_os = "linux")]
        {
            let mark = terminal_settings(&Settings { parity: Parity::Mark, ..Settings::new(9_600) })
                .expect("expressible on Linux");
            assert_eq!(mark.c_cflag & parity_bits(), libc::PARENB | libc::CMSPAR | libc::PARODD);
            let space = terminal_settings(&Settings { parity: Parity::Space, ..Settings::new(9_600) })
                .expect("expressible on Linux");
            assert_eq!(space.c_cflag & parity_bits(), libc::PARENB | libc::CMSPAR);
        }
    }
}
