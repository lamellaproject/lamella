//! A serial port on Linux and macOS, over the POSIX terminal interface directly.

use std::ffi::CString;
use std::io;
use std::time::{Duration, Instant};

use crate::{Port, PortError};

/// How long a single write may wait for the transmitter to become ready before the write is called
/// stalled.
///
/// This bounds one wait rather than the whole write, which is the distinction the error it produces
/// is about: a transmitter that never becomes ready is a dead cable or a flow control fight, while a
/// large image moving slowly is a slow line and not a fault. Bounding the total would confuse the
/// two, and the size at which it started doing so would depend on the rate.
const WRITE_READY_TIMEOUT_MS: u32 = 5_000;

/// A serial port held open.
#[derive(Debug)]
pub struct PosixPort {
    /// The open descriptor, or -1 between the close and the open inside a reopen.
    fd: libc::c_int,
    /// The name as given, so a reopen at a different rate can find the same port.
    name: String,
    /// The rate currently configured, reported for diagnostics.
    baud: u32,
}

/// The most recent operating system error, as the error variants carry it.
fn errno() -> u32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(0).unsigned_abs()
}

/// Whether the last operating system error was `code`.
fn last_error_was(code: libc::c_int) -> bool {
    io::Error::last_os_error().raw_os_error() == Some(code)
}

/// The terminal speed value for `baud` on Darwin, where the two are the same number.
///
/// Darwin's `speed_t` holds the rate itself -- its own `B115200` is defined as 115200 -- so every rate
/// the driver supports passes straight through and no table is needed.
#[cfg(any(target_os = "macos", target_os = "ios"))]
fn speed_for(baud: u32) -> Option<libc::speed_t> {
    Some(libc::speed_t::from(baud))
}

/// The terminal speed value for `baud` elsewhere, where the two are unrelated.
///
/// Linux's `speed_t` holds an opaque token rather than a rate -- its `B115200` is `0o010002` -- so a
/// rate has to be looked up, and one with no token cannot be requested through this interface at all.
/// A rate that is not in the table is refused rather than rounded to a neighbor, because a line
/// running at a rate the caller did not ask for produces framing errors that read as a broken cable.
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
fn speed_for(baud: u32) -> Option<libc::speed_t> {
    Some(match baud {
        9_600 => libc::B9600,
        19_200 => libc::B19200,
        38_400 => libc::B38400,
        57_600 => libc::B57600,
        115_200 => libc::B115200,
        230_400 => libc::B230400,
        460_800 => libc::B460800,
        921_600 => libc::B921600,
        _ => return None,
    })
}

impl PosixPort {
    /// Opens `name` at `baud`.
    ///
    /// A name with no leading `/` is taken as a device node and looked for in `/dev`, so both
    /// `ttyUSB0` and `/dev/ttyUSB0` name the same port. That is unconditional rather than a special
    /// case, so there is nothing to remember about which form a given tool wants.
    ///
    /// The port is opened without becoming the process's controlling terminal, so a hangup on the
    /// line cannot signal the program, and without waiting on carrier detect, so a device whose
    /// carrier line means something else here -- which is every device this drives -- opens
    /// immediately.
    ///
    /// # Errors
    /// [`PortError::Open`] with the platform error number when the port cannot be opened or is not a
    /// terminal, and [`PortError::Configure`] when it opens but cannot be configured.
    pub fn open(name: &str, baud: u32) -> Result<PosixPort, PortError> {
        let path = if name.starts_with('/') { name.to_string() } else { format!("/dev/{name}") };
        let Ok(c_path) = CString::new(path) else {
            return Err(PortError::Open {
                name: name.to_string(),
                code: libc::EINVAL.unsigned_abs(),
            });
        };
        let fd = unsafe {
            libc::open(c_path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_NONBLOCK)
        };
        if fd < 0 {
            return Err(PortError::Open { name: name.to_string(), code: errno() });
        }
        let port = PosixPort { fd, name: name.to_string(), baud };
        if unsafe { libc::isatty(fd) } != 1 {
            return Err(PortError::Open {
                name: name.to_string(),
                code: libc::ENOTTY.unsigned_abs(),
            });
        }
        port.configure(baud)?;
        Ok(port)
    }

    /// Installs the rate and the frame format, and confirms the line took them.
    fn configure(&self, baud: u32) -> Result<(), PortError> {
        let Some(speed) = speed_for(baud) else {
            return Err(PortError::Configure { what: "that baud rate on this platform", code: 0 });
        };
        let mut settings: libc::termios = unsafe { std::mem::zeroed() };
        settings.c_cflag = libc::CS8 | libc::CREAD | libc::CLOCAL;
        if unsafe { libc::cfsetispeed(&mut settings, speed) } != 0
            || unsafe { libc::cfsetospeed(&mut settings, speed) } != 0
        {
            return Err(PortError::Configure { what: "the baud rate", code: errno() });
        }
        if unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &settings) } != 0 {
            return Err(PortError::Configure { what: "the line settings", code: errno() });
        }
        self.confirm(speed)
    }

    /// Reads the line settings back and checks the ones this driver depends on.
    ///
    /// Applying settings is documented to succeed when only some of them were applied, so this is
    /// the step that turns that into an answer. Only the settings whose absence would produce a
    /// wrong measurement are checked; a driver is free to differ elsewhere.
    fn confirm(&self, speed: libc::speed_t) -> Result<(), PortError> {
        let mut actual: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(self.fd, &mut actual) } != 0 {
            return Err(PortError::Configure { what: "a read-back of the line settings", code: errno() });
        }
        let checks: [(bool, &'static str); 6] = [
            (actual.c_cflag & libc::CSIZE == libc::CS8, "eight data bits"),
            (actual.c_cflag & (libc::PARENB | libc::CSTOPB) == 0, "no parity and one stop bit"),
            (actual.c_cflag & libc::CRTSCTS == 0, "hardware flow control off"),
            (actual.c_cflag & libc::CLOCAL != 0, "the carrier line ignored"),
            (
                actual.c_iflag & (libc::IXON | libc::IXOFF | libc::IXANY) == 0,
                "software flow control off",
            ),
            (
                actual.c_cc[libc::VMIN] == 0 && actual.c_cc[libc::VTIME] == 0,
                "reads bounded by poll rather than by the terminal",
            ),
        ];
        for (held, what) in checks {
            if !held {
                return Err(PortError::Configure { what, code: 0 });
            }
        }
        if unsafe { libc::cfgetospeed(&actual) } != speed {
            return Err(PortError::Configure { what: "that baud rate on this line", code: 0 });
        }
        Ok(())
    }

    /// Waits up to `timeout_ms` for any of `events`, and reports what actually became ready.
    ///
    /// Zero means the bound elapsed with nothing ready, which is a normal outcome for a read.
    fn wait(&self, events: libc::c_short, timeout_ms: u32) -> Result<libc::c_short, PortError> {
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout_ms));
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let ms = libc::c_int::try_from(remaining.as_millis()).unwrap_or(libc::c_int::MAX);
            let mut watched = libc::pollfd { fd: self.fd, events, revents: 0 };
            let ready = unsafe { libc::poll(&mut watched, 1, ms) };
            if ready < 0 {
                if last_error_was(libc::EINTR) && Instant::now() < deadline {
                    continue;
                }
                if last_error_was(libc::EINTR) {
                    return Ok(0);
                }
                return Err(PortError::Read { code: errno() });
            }
            return Ok(if ready > 0 { watched.revents } else { 0 });
        }
    }
}

impl Drop for PosixPort {
    fn drop(&mut self) {
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
    }
}

impl Port for PosixPort {
    fn write(&mut self, bytes: &[u8]) -> Result<(), PortError> {
        let mut offset = 0;
        while offset < bytes.len() {
            let ready = self.wait(libc::POLLOUT, WRITE_READY_TIMEOUT_MS)?;
            if ready == 0 {
                return Err(PortError::WriteStalled { after: offset });
            }
            if ready & libc::POLLOUT == 0 {
                return Err(PortError::Write { code: libc::EIO.unsigned_abs() });
            }
            let moved = unsafe {
                libc::write(
                    self.fd,
                    bytes[offset..].as_ptr().cast::<libc::c_void>(),
                    bytes.len() - offset,
                )
            };
            if moved < 0 {
                if last_error_was(libc::EINTR) || last_error_was(libc::EAGAIN) {
                    continue;
                }
                return Err(PortError::Write { code: errno() });
            }
            if moved == 0 {
                return Err(PortError::WriteStalled { after: offset });
            }
            offset += usize::try_from(moved).unwrap_or(0);
        }
        Ok(())
    }

    fn read(&mut self, buffer: &mut [u8], timeout_ms: u32) -> Result<usize, PortError> {
        let ready = self.wait(libc::POLLIN, timeout_ms)?;
        if ready & libc::POLLIN == 0 {
            if ready & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                return Err(PortError::Read { code: libc::EIO.unsigned_abs() });
            }
            return Ok(0);
        }
        let got = unsafe {
            libc::read(self.fd, buffer.as_mut_ptr().cast::<libc::c_void>(), buffer.len())
        };
        if got < 0 {
            if last_error_was(libc::EINTR) || last_error_was(libc::EAGAIN) {
                return Ok(0);
            }
            return Err(PortError::Read { code: errno() });
        }
        Ok(usize::try_from(got).unwrap_or(0))
    }

    fn set_dtr(&mut self, on: bool) -> Result<(), PortError> {
        self.signal(libc::TIOCM_DTR, on, if on { "assert DTR" } else { "clear DTR" })
    }

    fn set_rts(&mut self, on: bool) -> Result<(), PortError> {
        self.signal(libc::TIOCM_RTS, on, if on { "assert RTS" } else { "clear RTS" })
    }

    fn reopen(&mut self, baud: u32) -> Result<(), PortError> {
        let name = self.name.clone();
        loop {
            if unsafe { libc::tcdrain(self.fd) } == 0 || !last_error_was(libc::EINTR) {
                break;
            }
        }
        unsafe { libc::close(self.fd) };
        self.fd = -1;
        let fresh = PosixPort::open(&name, baud)?;
        self.fd = fresh.fd;
        self.baud = baud;
        std::mem::forget(fresh);
        Ok(())
    }

    fn discard_buffers(&mut self) -> Result<(), PortError> {
        if unsafe { libc::tcflush(self.fd, libc::TCIOFLUSH) } != 0 {
            return Err(PortError::Signal { what: "purge", code: errno() });
        }
        Ok(())
    }

    fn describe(&self) -> String {
        format!("{} at {} baud", self.name, self.baud)
    }
}

impl PosixPort {
    /// Sets or clears one modem control line.
    ///
    /// The two directions are separate calls on this interface rather than one read-modify-write of
    /// the whole set, which is what makes a single signal transition a single operation -- the
    /// ordering a reset sequence depends on is only meaningful if each write moves one line.
    fn signal(&self, bit: libc::c_int, on: bool, what: &'static str) -> Result<(), PortError> {
        let request = if on { libc::TIOCMBIS } else { libc::TIOCMBIC };
        if unsafe { libc::ioctl(self.fd, request, &bit) } != 0 {
            return Err(PortError::Signal { what, code: errno() });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::CStr;
    use std::thread;

    use super::*;

    /// A pseudo-terminal pair: the end the test drives, and the device path a [`PosixPort`] opens.
    ///
    /// A pseudo-terminal is a real terminal as far as this file is concerned -- it answers `isatty`,
    /// it carries line settings, and `poll` reports it readable exactly when bytes are waiting -- so
    /// every setting and every timing decision below is exercised against the operating system rather
    /// than against a stand-in for it. What it cannot do is modem control lines, which is why nothing
    /// here asserts about DTR or RTS: those need a device with wires.
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
            let name = unsafe { libc::ptsname(end) };
            assert!(!name.is_null(), "ptsname: {}", io::Error::last_os_error());
            let path = unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned();
            Loopback { end, path }
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
    }

    impl Drop for Loopback {
        fn drop(&mut self) {
            unsafe { libc::close(self.end) };
        }
    }

    /// The settings are installed AND read back, which is the step that makes the first meaningful:
    /// applying them is documented to report success when only some of them took.
    #[test]
    fn a_terminal_opens_and_the_settings_are_read_back() {
        let line = Loopback::open();
        let port = PosixPort::open(&line.path, 115_200).expect("a pseudo-terminal configures");
        assert!(port.describe().contains("115200"), "the rate is reported: {}", port.describe());
        assert!(port.describe().contains(&line.path), "the port names itself: {}", port.describe());
    }

    /// **A read that is answered returns as soon as the bytes arrive, not when its bound expires.**
    /// This is the property the terminal's own timer cannot express at this resolution, and the whole
    /// reason the bound is a `poll`: a read given ten seconds and answered in fifty milliseconds must
    /// cost fifty, or every exchange in a write costs the timeout and the transfer rate becomes a
    /// function of the bound rather than of the line.
    #[test]
    fn a_read_returns_when_the_bytes_arrive_and_not_when_its_bound_expires() {
        let line = Loopback::open();
        let mut port = PosixPort::open(&line.path, 115_200).expect("configures");
        let sent: &[u8] = b"\xc0\x01\x02\xc0";
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(50));
                line.send(sent);
            });
            let mut buffer = [0u8; 64];
            let started = Instant::now();
            let got = port.read(&mut buffer, 10_000).expect("a read succeeds");
            let took = started.elapsed();
            assert_eq!(&buffer[..got], sent, "the bytes arrive intact");
            assert!(
                took < Duration::from_secs(5),
                "returned on the data rather than on the bound, but took {took:?}"
            );
        });
    }

    /// Silence is reported as zero bytes after the bound, which is the caller's timeout path, and the
    /// bound is honored rather than collapsing to an immediate answer.
    #[test]
    fn a_silent_line_reports_nothing_after_its_bound() {
        let line = Loopback::open();
        let mut port = PosixPort::open(&line.path, 115_200).expect("configures");
        let mut buffer = [0u8; 64];
        let started = Instant::now();
        let got = port.read(&mut buffer, 250).expect("silence is not an error");
        let took = started.elapsed();
        assert_eq!(got, 0, "nothing arrived");
        assert!(took >= Duration::from_millis(200), "it waited: {took:?}");
    }

    /// **A bound shorter than a tenth of a second is still a bound**, which is the case that decided
    /// the design: the terminal's own timer counts tenths of a second in one byte, so this bound would
    /// round to zero -- and zero there is not a short wait, it is the setting that means "do not wait
    /// at all". A port built that way answers instantly, every retry is spent in microseconds, and a
    /// target that was merely slow is reported as absent.
    #[test]
    fn a_bound_finer_than_the_terminal_timer_can_express_is_still_honored() {
        let line = Loopback::open();
        let mut port = PosixPort::open(&line.path, 115_200).expect("configures");
        let mut buffer = [0u8; 64];
        let started = Instant::now();
        let got = port.read(&mut buffer, 30).expect("silence is not an error");
        let took = started.elapsed();
        assert_eq!(got, 0, "nothing arrived");
        assert!(took >= Duration::from_millis(20), "30 ms was a wait, not a poll: {took:?}");
    }

    /// Every byte reaches the line unaltered -- the settings clear output processing and both flow
    /// controls, so the bytes a terminal in its default state would rewrite or swallow go out as
    /// themselves.
    #[test]
    fn a_write_delivers_every_byte_unaltered() {
        let line = Loopback::open();
        let mut port = PosixPort::open(&line.path, 115_200).expect("configures");
        let image: &[u8] = b"\x0a\x11\x13\xc0\xff";
        port.write(image).expect("the write succeeds");
        assert_eq!(line.receive(), image, "every byte arrived as itself");
    }

    /// Discarding drops what is already buffered, so a sequence that begins by clearing the line does
    /// not then read a previous conversation's tail as its own answer.
    #[test]
    fn discarding_drops_what_was_already_waiting() {
        let line = Loopback::open();
        let mut port = PosixPort::open(&line.path, 115_200).expect("configures");
        line.send(b"stale chatter from a previous boot");
        thread::sleep(Duration::from_millis(50));
        port.discard_buffers().expect("the flush succeeds");
        let mut buffer = [0u8; 64];
        assert_eq!(port.read(&mut buffer, 100).expect("no error"), 0, "the tail is gone");
    }

    /// A path that is not a terminal is refused at the open rather than configured and driven. Every
    /// setting this file installs succeeds on a regular file and means nothing there, so without this
    /// a mistyped path yields a port that opens, writes, and never answers.
    #[test]
    fn a_path_that_is_not_a_terminal_is_refused() {
        let refused = PosixPort::open("/dev/null", 115_200);
        assert!(matches!(refused, Err(PortError::Open { .. })), "got {refused:?}");
    }

    /// **A rate well above the slow ones both platforms name identically is installed and reads
    /// back**, which is the one place the two operating systems genuinely disagree: one holds an
    /// opaque token per rate and the other holds the rate itself. A port that spelled it the other
    /// platform's way would configure a line at a number that is not a rate, and the read-back is
    /// what turns that from a silent misconfiguration into a refusal.
    #[test]
    fn a_fast_rate_is_installed_and_reads_back_on_either_spelling() {
        let line = Loopback::open();
        let port = PosixPort::open(&line.path, 921_600).expect("a fast rate configures");
        assert!(port.describe().contains("921600"), "the rate is reported: {}", port.describe());
    }

    /// A rate with no terminal speed is refused rather than rounded to a neighbor, because a line
    /// running at a rate nobody asked for produces framing errors that read as a broken cable.
    #[cfg(not(any(target_os = "macos", target_os = "ios")))]
    #[test]
    fn a_rate_this_platform_cannot_express_is_refused_rather_than_rounded() {
        let line = Loopback::open();
        let refused = PosixPort::open(&line.path, 100_000);
        assert!(
            matches!(refused, Err(PortError::Configure { .. })),
            "an unrepresentable rate is refused: {refused:?}"
        );
    }
}
