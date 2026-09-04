//! The HOST side of the Lamella Link debug + REPL channel:

pub use lamella_runner::{
    ArtifactLoad, RunCollector, RunResult, baked_image_checksum, debug, deploy, exec, load, repl,
    run_program, send_image, send_program, serve_one, stop_exit,
};

pub mod engine;
pub mod firmware;
pub mod identity;

#[cfg(feature = "debug-backend")]
pub mod debug_backend;

use lamella_wire::{Transport, TransportError};
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
use lamella_wire::{Frame, FrameReader, encode_frame};
#[cfg(feature = "serial")]
use serialport::SerialPort;
#[cfg(any(feature = "serial", feature = "tcp"))]
use std::io::{Read, Write};
#[cfg(feature = "tcp")]
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

/// A [`Transport`] over a serial carrier (USB-CDC or UART). Frames are byte-framed via lamella-wire's
/// [`encode_frame`] / [`FrameReader`]; `poll` is non-blocking (a short read timeout).
#[cfg(feature = "serial")]
pub struct SerialTransport {
    port: Box<dyn SerialPort>,
    reader: FrameReader,
    baud: u32,
}

/// Show everything that arrives on the carrier when `LAMELLA_WIRE_TRACE` is set.
///
/// # Why this is worth a function
///
/// **A target's firmware writes its diagnostics to the same line the protocol runs on**, and the frame
/// reader necessarily DISCARDS anything outside a frame -- resynchronizing is normal operation on a line
/// that also carries boot chatter. So a board that aborts, prints exactly why, and then fails to recover
/// looks to every host tool like a board that said nothing at all: **the explanation was transmitted and
/// thrown away one layer below the code that needed it.**
///
/// That is not a defect in the frame reader; it is the cost of sharing the line. The remedy is to make
/// the discarded bytes visible on request, which is all this does.
///
/// Non-printable bytes become `.` rather than being escaped, because the thing being hunted is a
/// sentence -- a hex dump of a frame with a sentence buried in it is harder to read than the sentence
/// with the frame bytes flattened.
#[cfg(feature = "serial")]
fn trace_received(bytes: &[u8]) {
    if std::env::var_os("LAMELLA_WIRE_TRACE").is_none() {
        return;
    }
    let rendered: String = bytes
        .iter()
        .map(|&byte| match byte {
            b'\r' | b'\n' | 0x20..=0x7E => char::from(byte),
            _ => '.',
        })
        .collect();
    eprint!("{rendered}");
}

/// Why a `serial:<id>` target could not be turned into a port name.
///
/// Two states with different remedies, so they are two variants rather than one failure: "plug the
/// board in / check the serial" against "say which of these you meant". It lives HERE, in the host
/// crate, and deliberately not in [`TransportError`] -- that type is shared with `no_std` firmware,
/// where this lane has measured a payload-carrying enum variant at ~1,950 bytes of flash for a
/// condition no device can ever be in.
#[cfg(feature = "serial")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// No attached USB-backed port reports a serial containing the requested text.
    NoSuchSerial(String),
    /// More than one does. Carries every match, because the useful message names the alternatives.
    Ambiguous(String, Vec<String>),
}

#[cfg(feature = "serial")]
impl core::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoSuchSerial(wanted) => write!(
                f,
                "no attached port carries a USB serial containing {wanted:?} -- run `wire-boards` to list them"
            ),
            Self::Ambiguous(wanted, ports) => write!(
                f,
                "{:?} matches {} attached boards ({}) -- give more of the serial",
                wanted,
                ports.len(),
                ports.join(", ")
            ),
        }
    }
}

/// Turn a target string into a port name: `serial:<id>` resolves through the attached boards' USB
/// serial numbers, anything else is already a port name and is returned unchanged.
///
/// # Why a serial is the addressable form and a port name is not
///
/// **A COM number is an assignment, not an identity.** The OS hands it out, it changes when a board
/// moves to a different hub port, and on a shared bench two people can be handed each other's. Every
/// other carrier in this tree already refuses to work that way -- `st-enumerate` lists probes by
/// descriptor serial, `wire-list` lists native-USB boards by serial, and `stlink-flash` REQUIRES one
/// because a flash tool that picks its own probe can erase somebody else's board. The serial carrier
/// was the last one that could only be named as `COM64`, and it is the one most boards use: every
/// micro:bit, every ST board reached through its on-board debugger's virtual COM port.
///
/// The mapping was never missing from the machine, only from us -- a USB-CDC port's parent device is
/// the board or probe itself, and its `iSerialNumber` is exactly what `st-enumerate` prints.
///
/// **Ambiguity is REFUSED, not resolved by picking the first.** A substring match is a convenience
/// for a human reading a serial off a label; silently taking one of several matches is how a tool
/// writes to the wrong board, and probe serials commonly share a prefix.
///
/// # Errors
/// [`ResolveError`] if nothing matches or if more than one does.
#[cfg(feature = "serial")]
pub fn resolve_target(target: &str) -> Result<String, ResolveError> {
    let Some(wanted) = target.strip_prefix("serial:") else {
        return Ok(target.to_string());
    };
    let needle = wanted.to_ascii_lowercase();
    let matches: Vec<String> = serialport::available_ports()
        .into_iter()
        .flatten()
        .filter(|port| match &port.port_type {
            serialport::SerialPortType::UsbPort(info) => info
                .serial_number
                .as_deref()
                .is_some_and(|actual| actual.to_ascii_lowercase().contains(&needle)),
            _ => false,
        })
        .map(|port| port.port_name)
        .collect();
    match matches.len() {
        0 => Err(ResolveError::NoSuchSerial(wanted.to_string())),
        1 => Ok(matches.into_iter().next().expect("length checked")),
        _ => Err(ResolveError::Ambiguous(wanted.to_string(), matches)),
    }
}

#[cfg(feature = "serial")]
impl SerialTransport {
    /// Open the serial carrier at `path` -- either a port name (`"COM5"` / `"/dev/ttyACM0"`) or
    /// `serial:<id>`, which selects the board by its USB serial number (see [`resolve_target`]).
    /// The baud is moot for native USB-CDC but honored for a real UART.
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the port cannot be opened, or if a `serial:` target names no
    /// board or more than one. Call [`resolve_target`] first when the caller wants to tell a user
    /// WHICH of those happened -- this signature is shared with the carrier-neutral driver and
    /// cannot widen without reaching the `no_std` side.
    pub fn open(target: &str, baud: u32) -> Result<Self, TransportError> {
        let resolved = resolve_target(target).map_err(|_| TransportError::Carrier)?;
        let path = resolved.as_str();
        let resets_on_dtr = serialport::available_ports()
            .into_iter()
            .flatten()
            .find(|p| p.port_name.eq_ignore_ascii_case(path))
            .map(|p| match p.port_type {
                serialport::SerialPortType::UsbPort(ref i) => {
                    i.vid == 0x303a || (i.vid == 0x2341 && i.pid == 0x003D)
                }
                _ => false,
            })
            .unwrap_or(false);
        let mut port = serialport::new(path, baud)
            .timeout(Duration::from_millis(50))
            .dtr_on_open(!resets_on_dtr)
            .open()
            .map_err(|_| TransportError::Carrier)?;
        if resets_on_dtr {
            port.write_data_terminal_ready(false).ok();
            port.write_request_to_send(false).ok();
        } else {
            port.write_data_terminal_ready(true).map_err(|_| TransportError::Carrier)?;
            port.write_request_to_send(true).ok();
        }
        std::thread::sleep(Duration::from_millis(300));
        Ok(Self::from_port(port, baud))
    }

    /// Wrap a port the caller already has.
    ///
    /// Exists so the frame-delivery order above can be measured against a port that records what
    /// it was asked to do. That order is a latency property, invisible to any test that reads what
    /// arrived without also counting how many times the carrier was consulted to get it.
    pub fn from_port(port: Box<dyn SerialPort>, baud: u32) -> Self {
        Self { port, reader: FrameReader::new(), baud }
    }
}

#[cfg(feature = "serial")]
impl Transport for SerialTransport {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        let frame = encode_frame(msg_type, seq, payload).ok_or(TransportError::PayloadTooLarge)?;
        let wire_ms = 100 + (frame.len() as u64 * 10 * 1000) / u64::from(self.baud.max(1));
        self.port.set_timeout(Duration::from_millis(wire_ms)).ok();
        let written = self
            .port
            .write_all(&frame)
            .and_then(|()| self.port.flush());
        self.port.set_timeout(Duration::from_millis(50)).ok();
        written.map_err(|_| TransportError::Carrier)?;
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
        if let Some(frame) = self.reader.next_frame() {
            return Ok(Some(frame));
        }
        let mut buf = [0u8; 512];
        match self.port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => {
                trace_received(&buf[..n]);
                self.reader.push(&buf[..n]);
            }
            Err(ref error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return Err(TransportError::Carrier),
        }
        Ok(self.reader.next_frame())
    }
}

/// A [`Transport`] over the native USB (driverless WinUSB) Lamella Link carrier -- the device's vendor
/// interface bulk pipes carry the same [`encode_frame`] framing a UART carrier uses. `send` writes an
/// encoded frame (the OS splits it across the bulk max packet); `poll` reads a bulk packet with a short
/// timeout and feeds a [`FrameReader`], so the self-synchronizing framing reassembles across packets.
#[cfg(feature = "usb")]
pub struct UsbTransport {
    device: lamella_usbbulk::Device,
    reader: FrameReader,
}

#[cfg(feature = "usb")]
impl UsbTransport {
    /// Open the Lamella Link vendor interface -- its VID/PID + WinUSB interface GUID from
    /// [`lamella_wire::usb`].
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if no matching device is present or it cannot be opened.
    pub fn open() -> Result<Self, TransportError> {
        Self::open_ids(lamella_wire::usb::VID, lamella_wire::usb::PID)
    }

    /// Open a specific `vid`/`pid` on the Lamella Link interface GUID (a board built with its own id pair).
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if no matching device is present or it cannot be opened.
    pub fn open_ids(vid: u16, pid: u16) -> Result<Self, TransportError> {
        Self::open_matching(vid, pid, None)
    }

    /// Open the Lamella Link board matching `vid`/`pid` AND -- when several boards share the id
    /// pair -- a case-insensitive substring of its USB serial number (an RP2350 reports its
    /// 16-hex-digit chip id; what a board reports is its firmware's choice). `None` opens the
    /// first match.
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if no matching device is present or it cannot be opened.
    pub fn open_matching(vid: u16, pid: u16, serial: Option<&str>) -> Result<Self, TransportError> {
        let device = lamella_usbbulk::Device::open_interface(
            lamella_wire::usb::WINUSB_INTERFACE_GUID,
            vid,
            pid,
            serial,
        )
        .map_err(|_| TransportError::Carrier)?;
        Ok(Self { device, reader: FrameReader::new() })
    }

    /// List the attached Lamella Link boards (any VID/PID under the Lamella Link interface GUID), with
    /// product + serial strings where the OS reports them -- the picker's data source.
    ///
    /// # A board carries the identity it was flashed with, and the list has to hold both
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the platform cannot enumerate by interface GUID.
    pub fn list() -> Result<Vec<lamella_usbbulk::DeviceInfo>, TransportError> {
        match lamella_usbbulk::enumerate_interface(lamella_wire::usb::WINUSB_INTERFACE_GUID) {
            Ok(boards) => Ok(boards),
            Err(lamella_usbbulk::Error::Unsupported) => Ok(lamella_usbbulk::enumerate()
                .map_err(|_| TransportError::Carrier)?
                .into_iter()
                .filter(|board| {
                    lamella_wire::usb::identify(board.vendor_id, board.product_id).is_some()
                })
                .collect()),
            Err(_) => Err(TransportError::Carrier),
        }
    }
}

/// A serial port the OS reports, with its USB identity when it is a USB device -- the serial half of the
/// board picker (an RPi Debug Probe / FTDI / EDBG UART, etc.). Cross-platform via `serialport::available_ports`.
#[cfg(feature = "serial")]
#[derive(Debug, Clone)]
pub struct SerialPortDesc {
    /// The OS port name (`COM8`, `/dev/ttyACM0`, `/dev/cu.usbmodemXXXX`, ...).
    pub port: String,
    /// USB vendor id, when the port is a USB device.
    pub vid: Option<u16>,
    /// USB product id, when the port is a USB device.
    pub pid: Option<u16>,
    /// USB serial-number string, if the OS reported one.
    pub serial_number: Option<String>,
    /// USB product string, if the OS reported one.
    pub product: Option<String>,
}

/// List the OS serial ports (cross-platform: Windows / macOS / Linux), with USB identity where the port is a
/// USB device -- the serial half of the board picker. A bare (non-USB) UART comes back with `vid`/`pid` `None`.
#[cfg(feature = "serial")]
#[must_use]
pub fn list_serial() -> Vec<SerialPortDesc> {
    serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .map(|port| {
            let (vid, pid, serial_number, product) = match port.port_type {
                serialport::SerialPortType::UsbPort(info) => {
                    (Some(info.vid), Some(info.pid), info.serial_number, info.product)
                }
                _ => (None, None, None, None),
            };
            SerialPortDesc { port: port.port_name, vid, pid, serial_number, product }
        })
        .collect()
}

/// Parse an example's `usb` target argument into (vid, pid, serial-substring):
/// - `usb` -- the Lamella Link VID/PID, first attached board;
/// - `usb:<vid>:<pid>` -- hex id pair (each exactly 4 hex digits), e.g. `usb:39e9:0001`;
/// - `usb:<vid>:<pid>:<serial>` -- id pair plus a serial-substring pick;
/// - `usb:<serial>` -- the Lamella Link VID/PID, board picked by a case-insensitive serial
/// substring (chip-id serials are hex, so a single token is ALWAYS a serial -- overriding
/// the ids requires the full pair form).
#[cfg(feature = "usb")]
pub fn parse_usb_target(target: &str) -> (u16, u16, Option<String>) {
    fn hex4(s: &str) -> Option<u16> {
        (s.len() == 4).then(|| u16::from_str_radix(s, 16).ok()).flatten()
    }
    let mut vid = lamella_wire::usb::VID;
    let mut pid = lamella_wire::usb::PID;
    let mut serial = None;
    let mut parts = target.splitn(4, ':').skip(1).filter(|s| !s.is_empty()).peekable();
    if let Some(first) = parts.next() {
        match (hex4(first), parts.peek().and_then(|s| hex4(s))) {
            (Some(v), Some(p)) => {
                vid = v;
                pid = p;
                parts.next();
                serial = parts.next().map(str::to_string);
            }
            _ => serial = Some(first.to_string()),
        }
    }
    (vid, pid, serial)
}

/// The target string that names `board` -- **what to hand [`open_target`] to reach this exact
/// device**, and the answer a listing should print.
///
/// **THE GRAMMAR IS NOT PART OF THIS PROMISE.** The contract is only that what comes out of here
/// goes back into [`open_target`] and reaches the same board; the spelling stays private to this
/// crate, so it can gain a field (a hub, a port) without moving every caller. A public
/// `format(vid, pid, serial)` would have promised the shape instead of the property.
///
/// It exists so the string has ONE writer. Spelled at each call site instead, a format with
/// several writers and one private reader cannot be round-trip tested by any of them -- and the
/// drift that ends with a listing showing a target nothing can open is silent until somebody
/// pastes one in.
#[cfg(feature = "usb")]
#[must_use]
pub fn usb_target_of(board: &lamella_usbbulk::DeviceInfo) -> String {
    format_usb_target(board.vendor_id, board.product_id, board.serial_number.as_deref())
}

/// The private spelling of a usb target, and the inverse of [`parse_usb_target`]: the full id-pair
/// form, so it round-trips whatever the ids are.
///
/// Not public: see [`usb_target_of`]. The four-digit padding is load-bearing -- a `{:x}` printer
/// turns `(0x39e9, 0x0001)` into `usb:39e9:1`, which parses back as the DEFAULT pid with the SERIAL
/// `39e9`, because a single token is always read as a serial. That is not an error anywhere; it is
/// a target that opens a different board.
#[cfg(feature = "usb")]
fn format_usb_target(vid: u16, pid: u16, serial: Option<&str>) -> String {
    match serial {
        Some(serial) => format!("usb:{vid:04x}:{pid:04x}:{serial}"),
        None => format!("usb:{vid:04x}:{pid:04x}"),
    }
}

/// Which firmware era `board` was flashed in, or `None` if it is not a Lamella Link.
///
/// A thin forward to [`lamella_wire::usb::identify`] so a listing asks the same question the scan's
/// own filter asks. The definition stays there; this is where a board is the subject rather than a
/// pair of numbers.
#[cfg(feature = "usb")]
#[must_use]
pub fn firmware_era(board: &lamella_usbbulk::DeviceInfo) -> Option<lamella_wire::usb::LinkIdentity> {
    lamella_wire::usb::identify(board.vendor_id, board.product_id)
}

/// The one line a listing should print beside a board of `era`, or `None` when there is nothing
/// to say about it.
///
/// # What this says, and the thing it deliberately does not say
///
/// A board answering the previous vendor id is **invisible to a scan that matches only the current
/// pair** -- not listed as old, not listed at all -- and a board nobody lists is a board nobody
/// thinks to reprogram. That is what this note is for, and it is the whole of what the vendor id
/// establishes.
///
/// **It is NOT a statement that the firmware is otherwise current.** The vendor id changed at one
/// moment; the message numbering changed at another. A board on the previous vendor id was
/// certainly flashed before both, but a board on the current one may still predate the numbering --
/// so `None` here means "nothing to say about this board's era", never "this board is up to date".
/// A listing that reads it as reassurance would give exactly the false comfort the missing-board
/// case is dangerous for.
#[must_use]
pub fn era_note(era: lamella_wire::usb::LinkIdentity) -> Option<&'static str> {
    match era {
        lamella_wire::usb::LinkIdentity::Current => None,
        lamella_wire::usb::LinkIdentity::Legacy => Some(
            "legacy vendor id: this board was flashed before the Lamella Link had one of its own, \
             so a scan matching only the current pair does not list it at all. Reflash it to move \
             it to the current pair.",
        ),
    }
}

#[cfg(feature = "usb")]
impl Transport for UsbTransport {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        let frame = encode_frame(msg_type, seq, payload).ok_or(TransportError::PayloadTooLarge)?;
        self.device.write_packet(&frame).map_err(|_| TransportError::Carrier)
    }

    fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
        if let Some(frame) = self.reader.next_frame() {
            return Ok(Some(frame));
        }
        let mut buf = [0u8; 512];
        match self.device.read_packet(&mut buf, Duration::from_millis(50)) {
            Ok(count) => self.reader.push(&buf[..count]),
            Err(lamella_usbbulk::Error::Timeout) => {}
            Err(_) => return Err(TransportError::Carrier),
        }
        Ok(self.reader.next_frame())
    }
}

/// A [`Transport`] over TCP -- the carrier for a board that is reached across a network rather
/// than down a cable.
///
/// Two arrangements use it and it does not distinguish them, because from here they are the same
/// socket: a board with its own network interface serving Lamella Link itself, and a board reached
/// through a relay daemon running on a companion processor beside it.
///
/// # What differs from the cable carriers
///
/// **Nagle's algorithm is turned OFF.** It exists to coalesce small writes into fuller segments,
/// and this protocol's traffic is exactly what it coalesces: short request frames whose replies
/// the sender is waiting on. Left on, it adds a round trip's delay to interactive stepping in
/// order to save bandwidth nobody here is short of.
///
/// **A write that does not finish ENDS the session.** A frame is length-prefixed, so half of one
/// on the wire is not a short frame -- it is a frame header promising bytes that will never come,
/// and the far side consumes whatever follows as its payload. Rather than continue on a stream
/// whose next frame boundary is a guess, the transport reports [`TransportError::Carrier`] and
/// stays ended. The remedy is a new connection, which is a new session, which is the honest thing
/// to make the caller do -- see [`TcpTransport::connect`].
///
/// **Ending the session does not stop the reading**, though, because the two directions of a
/// socket fail independently. A peer that replies and then stops reading breaks the next write
/// while its reply is still on its way in, and a transport that took a write failure as a reason
/// to stop reading would discard an answer it already had. What survives that is up to the
/// platform's socket layer; what this transport does with whatever survives is deliver it, and
/// report the failure once there is nothing left.
#[cfg(feature = "tcp")]
pub struct TcpTransport {
    stream: TcpStream,
    reader: FrameReader,
    /// Why this session ended, once it has. Frames in hand -- and frames still arriving -- are
    /// delivered afterward; see [`TcpTransport::poll`]. First ending wins.
    ended: Option<TransportError>,
    /// Whether the RECEIVE side has reached its end. Tracked apart from `ended` because the two
    /// directions of a socket fail independently: a peer that half-closes after replying breaks
    /// the next write while leaving its reply perfectly readable, and one flag cannot hold both
    /// facts without the write failure suppressing the read.
    read_ended: bool,
    write_timeout: Duration,
}

/// How long a poll waits for bytes before reporting that none arrived. Matches the serial and USB
/// carriers, so a driver's polling loop behaves the same whichever one is under it.
#[cfg(feature = "tcp")]
const TCP_POLL_TIMEOUT: Duration = Duration::from_millis(50);

/// Read size per poll. The frame reader reassembles across reads, so this bounds how much one poll
/// moves rather than how large a frame may be.
#[cfg(feature = "tcp")]
const TCP_READ_CHUNK: usize = 4096;

/// Parse a `tcp:` target into a `host:port` address string: `tcp:<host>`, `tcp:<host>:<port>`, or
/// a bare `<host>:<port>`. A target with no port gets [`lamella_wire::tcp::DEFAULT_PORT`].
///
/// An IPv6 literal is written in brackets (`tcp:[::1]:14825`), which is the notation every other
/// tool uses and the only one that can be told from a bare address whose colons are its own.
#[cfg(feature = "tcp")]
#[must_use]
pub fn parse_tcp_target(target: &str) -> String {
    let body = target.strip_prefix("tcp:").unwrap_or(target);
    let has_port = match body.rfind(']') {
        Some(close) => body[close + 1..].starts_with(':'),
        None => body.matches(':').count() == 1,
    };
    if has_port {
        body.to_string()
    } else {
        format!("{body}:{}", lamella_wire::tcp::DEFAULT_PORT)
    }
}

#[cfg(feature = "tcp")]
impl TcpTransport {
    /// Connect to a target and start a session on it.
    ///
    /// `target` is `tcp:<host>[:<port>]` or a bare `<host>:<port>` (see [`parse_tcp_target`]).
    /// A name resolving to several addresses is tried in the resolver's order, so a board reachable
    /// over both address families connects over whichever answers.
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the target does not resolve, or if no address accepted a
    /// connection within `timeout`.
    pub fn connect(target: &str, timeout: Duration) -> Result<Self, TransportError> {
        let address = parse_tcp_target(target);
        let candidates: Vec<SocketAddr> =
            address.to_socket_addrs().map_err(|_| TransportError::Carrier)?.collect();
        for candidate in candidates {
            if let Ok(stream) = TcpStream::connect_timeout(&candidate, timeout) {
                return Self::from_stream(stream);
            }
        }
        Err(TransportError::Carrier)
    }

    /// Start a session on a socket the caller already has.
    ///
    /// This is how the host LISTENS: a board that sits behind a router which will not forward a
    /// port to it dials out instead, and the host accepts the connection and hands the stream
    /// here. Nothing above the transport can tell which way the connection was established.
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the socket options cannot be set, which on a live connection
    /// means the connection is not live.
    pub fn from_stream(stream: TcpStream) -> Result<Self, TransportError> {
        stream.set_nodelay(true).map_err(|_| TransportError::Carrier)?;
        stream.set_read_timeout(Some(TCP_POLL_TIMEOUT)).map_err(|_| TransportError::Carrier)?;
        Ok(Self {
            stream,
            reader: FrameReader::new(),
            ended: None,
            read_ended: false,
            write_timeout: Duration::from_secs(10),
        })
    }

    /// Record why the session ended, keeping the FIRST reason. A write failure followed by the
    /// peer's clean close must report the failure: it is the one with a remedy attached.
    fn end(&mut self, why: TransportError) {
        if self.ended.is_none() {
            self.ended = Some(why);
        }
    }

    /// Set how long a single frame's write may take before the session is ended. The default is
    /// ten seconds, which is generous for the largest frame this wire carries on any link fast
    /// enough to be worth debugging over.
    pub fn set_write_timeout(&mut self, timeout: Duration) {
        self.write_timeout = timeout;
    }

    /// The address of the peer, for a tool that wants to report what it is talking to.
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the socket cannot report it.
    pub fn peer(&self) -> Result<SocketAddr, TransportError> {
        self.stream.peer_addr().map_err(|_| TransportError::Carrier)
    }

    /// Whether this session has ended. A closed session still yields frames that arrived before
    /// it closed.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.ended.is_some()
    }

    /// Write every byte or end the session, tracking progress so a stall is never mistaken for a
    /// completed write.
    fn write_whole_frame(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        let deadline = Instant::now() + self.write_timeout;
        let mut at = 0;
        while at < frame.len() {
            match self.stream.write(&frame[at..]) {
                Ok(0) => break,
                Ok(count) => at += count,
                Err(ref error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::Interrupted
                    ) => {}
                Err(_) => break,
            }
            if at < frame.len() && Instant::now() >= deadline {
                break;
            }
        }
        if at < frame.len() {
            self.end(TransportError::Carrier);
            return Err(TransportError::Carrier);
        }
        self.stream.flush().map_err(|_| {
            self.end(TransportError::Carrier);
            TransportError::Carrier
        })
    }
}

#[cfg(feature = "tcp")]
impl Transport for TcpTransport {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        if let Some(ended) = self.ended {
            return Err(ended);
        }
        let frame = encode_frame(msg_type, seq, payload).ok_or(TransportError::PayloadTooLarge)?;
        self.write_whole_frame(&frame)
    }

    fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
        if let Some(frame) = self.reader.next_frame() {
            return Ok(Some(frame));
        }
        if !self.read_ended {
            let mut buf = [0u8; TCP_READ_CHUNK];
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.read_ended = true;
                    self.end(TransportError::Closed);
                }
                Ok(count) => self.reader.push(&buf[..count]),
                Err(ref error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => {
                    self.read_ended = true;
                    self.end(TransportError::Carrier);
                }
            }
        }
        match (self.reader.next_frame(), self.ended) {
            (Some(frame), _) => Ok(Some(frame)),
            (None, Some(ended)) => Err(ended),
            (None, None) => Ok(None),
        }
    }
}

/// Whichever carrier a target string named.
///
/// The tools are generic over [`Transport`], so they take this without knowing which arm they got.
/// It exists so that the mapping from a target string to a carrier lives in ONE place: before it
/// did, each tool carried its own copy of the dispatch, which is why a carrier could be added and
/// reach none of them. A new carrier now reaches every tool by being added here.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
#[non_exhaustive]
pub enum AnyTransport {
    /// A serial port, by name or by USB serial number.
    #[cfg(feature = "serial")]
    Serial(SerialTransport),
    /// The native-USB vendor interface.
    #[cfg(feature = "usb")]
    Usb(UsbTransport),
    /// A network socket, direct to a board or through a relay.
    #[cfg(feature = "tcp")]
    Tcp(TcpTransport),
}

#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
impl Transport for AnyTransport {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        match self {
            #[cfg(feature = "serial")]
            Self::Serial(carrier) => carrier.send(msg_type, seq, payload),
            #[cfg(feature = "usb")]
            Self::Usb(carrier) => carrier.send(msg_type, seq, payload),
            #[cfg(feature = "tcp")]
            Self::Tcp(carrier) => carrier.send(msg_type, seq, payload),
        }
    }

    fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
        match self {
            #[cfg(feature = "serial")]
            Self::Serial(carrier) => carrier.poll(),
            #[cfg(feature = "usb")]
            Self::Usb(carrier) => carrier.poll(),
            #[cfg(feature = "tcp")]
            Self::Tcp(carrier) => carrier.poll(),
        }
    }
}

/// Which carrier a target string names, decided WITHOUT opening anything.
///
/// Separated from opening so a tool can report an unbuildable target before it starts, and so the
/// syntax can be tested without hardware -- the rule is prefix-only and there is nothing about it
/// that needs a board to check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetKind {
    /// `tcp:<host>[:<port>]`, or a bare `host:port`.
    Tcp,
    /// `usb`, `usb:<serial>`, or `usb:<vid>:<pid>[:<serial>]`.
    Usb,
    /// A port name, or `serial:<usb-serial-substring>`.
    Serial,
}

/// Classify a target string.
///
/// A bare `host:port` reads as TCP, and that is the one rule a reader should check against their
/// own habits: a Windows port name (`COM8`) carries no colon and an appended one is not a thing
/// anybody writes, while `192.168.1.50:14825` is exactly what somebody types. A `/dev/...` path can
/// contain a colon on no platform this runs on. Where the guess would be wrong, `serial:` says so
/// explicitly and always wins.
#[must_use]
pub fn classify_target(target: &str) -> TargetKind {
    if target.starts_with("tcp:") {
        return TargetKind::Tcp;
    }
    if target == "usb" || target.starts_with("usb:") {
        return TargetKind::Usb;
    }
    if target.starts_with("serial:") {
        return TargetKind::Serial;
    }
    let looks_like_address = target.starts_with('[')
        || matches!(target.split_once(':'),
            Some((host, port)) if !host.is_empty()
                && !port.is_empty()
                && !port.contains(':')
                && port.bytes().all(|b| b.is_ascii_digit()));
    if looks_like_address { TargetKind::Tcp } else { TargetKind::Serial }
}

/// Open whichever carrier `target` names.
///
/// One place knows the target syntax, so every tool built on this gains a carrier at the same
/// moment rather than one tool at a time. `baud` applies only to a serial target and is ignored by
/// the others; `timeout` only to a network one.
///
/// # Errors
/// [`TransportError::Carrier`] if the carrier will not open, or if the target names a carrier this
/// build was compiled without -- a build missing a feature and a board that is unplugged are both
/// "cannot reach it from here", and the message a tool prints should say which.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn open_target(
    target: &str,
    baud: u32,
    timeout: Duration,
) -> Result<AnyTransport, TransportError> {
    match classify_target(target) {
        TargetKind::Tcp => {
            #[cfg(feature = "tcp")]
            {
                return TcpTransport::connect(target, timeout).map(AnyTransport::Tcp);
            }
            #[cfg(not(feature = "tcp"))]
            Err(TransportError::Carrier)
        }
        TargetKind::Usb => {
            #[cfg(feature = "usb")]
            {
                let (vid, pid, serial) = parse_usb_target(target);
                return UsbTransport::open_matching(vid, pid, serial.as_deref()).map(AnyTransport::Usb);
            }
            #[cfg(not(feature = "usb"))]
            Err(TransportError::Carrier)
        }
        TargetKind::Serial => {
            #[cfg(feature = "serial")]
            {
                return SerialTransport::open(target, baud).map(AnyTransport::Serial);
            }
            #[cfg(not(feature = "serial"))]
            {
                let _ = baud;
                Err(TransportError::Carrier)
            }
        }
    }
}

/// The carriers this build can actually open, for a tool's usage line and for the message it prints
/// when a target names one that was compiled out.
///
/// A tool that prints the syntax it accepts without checking what it was built with sends the
/// reader to debug a cable for a carrier the binary does not contain.
#[must_use]
pub fn available_carriers() -> &'static [&'static str] {
    &[
        #[cfg(feature = "serial")]
        "serial",
        #[cfg(feature = "usb")]
        "usb",
        #[cfg(feature = "tcp")]
        "tcp",
    ]
}

/// What a [`TransportError::VersionMismatch`] MEANS, as a sentence somebody can act on, naming
/// which end is behind.
///
/// `host` is the version the reporting build speaks -- [`lamella_wire::PROTOCOL_VERSION`] for a
/// tool talking to a board directly. It is a parameter rather than read from the constant so that
/// the direction logic is a pure function of its inputs: the "board is behind" branch cannot be
/// reached at all while `PROTOCOL_VERSION` is 1, and a rule nothing can exercise is a rule nothing
/// checks.
///
/// # Why the sentence lives here and not at each tool
///
/// Three tools report this and each had its own wording for the failure it replaced -- *no answer*,
/// *no HELLO_ACK*, *did not answer a HELLO* -- every one of which says the board is silent. **The
/// board is not silent: it answered, promptly and correctly, that it cannot speak to this build.**
/// A sentence written three times is a sentence corrected once, so this is the one copy.
///
/// It states the DIRECTION because that is what decides the remedy, and a reader given two version
/// numbers has to work it out: a target behind the host wants reflashing, a target ahead of it wants
/// newer tools, and those send a person to opposite conclusions.
#[must_use]
pub fn version_mismatch(host: u16, target_min: u16, target_max: u16) -> String {
    if target_max == 0 {
        return format!(
            concat!(
                "it speaks a Lamella Link version this build cannot read, and its reply did not ",
                "decode. This tool speaks version {host}.",
            ),
            host = host,
        );
    }
    let range = if target_min == target_max {
        format!("version {target_min}")
    } else {
        format!("versions {target_min} to {target_max}")
    };
    let remedy = if target_max < host {
        "the BOARD is behind this tool -- reflash its serve firmware from this tree"
    } else {
        "this TOOL is behind the board -- update the tools, the board is newer"
    };
    format!(
        concat!(
            "it is running Lamella firmware and it answered, but the two of you share no protocol ",
            "version: it speaks {range} and this tool speaks {host}. {remedy}. Nothing is wrong ",
            "with the cable or the port -- the handshake completed and the answer was a refusal.",
        ),
        range = range,
        host = host,
        remedy = remedy,
    )
}

/// Host driver, blocking: HELLO the target and return the negotiated session (the chosen
/// version + the capability INTERSECTION). `host_caps` is what this host offers -- check
/// the result's caps to pick the PE ([`send_program`]) vs baked ([`send_image`]) path.
///
/// # Errors
/// [`TransportError::Closed`] on timeout or a version `NAK`; otherwise a carrier error.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn hello_blocking(
    transport: &mut impl Transport,
    seq: u16,
    host_caps: lamella_wire::Capabilities,
    timeout: Duration,
) -> Result<lamella_wire::Negotiated, TransportError> {
    use lamella_wire::{Hello, HelloAck, HelloNak, PROTOCOL_VERSION, ProtocolRange, host_finish, msg};
    let hello = Hello {
        range: ProtocolRange { min: PROTOCOL_VERSION, max: PROTOCOL_VERSION },
        caps: host_caps,
    };
    let encoded = hello.encode();
    let deadline = Instant::now() + timeout;
    let mut next_send = Instant::now();
    while Instant::now() < deadline {
        if Instant::now() >= next_send {
            transport.send(msg::HELLO, seq, &encoded)?;
            next_send = Instant::now() + Duration::from_millis(250);
        }
        while let Some(frame) = transport.poll()? {
            match frame.msg_type {
                msg::HELLO_ACK if frame.seq == seq => {
                    if let Some(ack) = HelloAck::decode(&frame.payload) {
                        return Ok(host_finish(&ack, host_caps));
                    }
                }
                msg::HELLO_NAK if frame.seq == seq => {
                    let range = HelloNak::decode(&frame.payload)
                        .map_or(ProtocolRange { min: 0, max: 0 }, |nak| nak.target_range);
                    return Err(TransportError::VersionMismatch {
                        target_min: range.min,
                        target_max: range.max,
                    });
                }
                msg::ERROR if frame.seq == seq => {
                    return Err(TransportError::Refused {
                        reason: frame.payload.first().copied().unwrap_or(0),
                        msg_type: lamella_wire::error::refused_message_type(&frame.payload)
                            .unwrap_or(0),
                    });
                }
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(TransportError::Closed)
}

/// Host driver, blocking convenience for a real (concurrent) target: send the program, then poll for
/// its result until `timeout`. The target runs the runner loop concurrently.
///
/// # Errors
/// [`TransportError::Closed`] on timeout; otherwise a carrier [`TransportError`].
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn eval_blocking(
    transport: &mut impl Transport,
    seq: u16,
    program: &[u8],
    timeout: Duration,
) -> Result<RunResult, TransportError> {
    send_program(transport, seq, program)?;
    await_result(transport, seq, timeout)
}

/// [`eval_blocking`]'s twin for a PE-less target: send a BAKED image, wait for its result.
///
/// # Errors
/// [`TransportError::Closed`] on timeout; otherwise a carrier [`TransportError`].
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn eval_image_blocking(
    transport: &mut impl Transport,
    seq: u16,
    image: &[u8],
    timeout: Duration,
) -> Result<RunResult, TransportError> {
    send_image(transport, seq, image)?;
    await_result(transport, seq, timeout)
}

/// What a target said about an artifact transfer: it took the bytes, or it refused a chunk.
///
/// # Why this is not a `bool`
///
/// A `bool` makes a refused transfer easy to walk past. `driver(..)?` propagates only the carrier
/// faults, so a `false` returned from a target that answered promptly and precisely slides through
/// the one operator a caller reaches for first -- and the caller carries on as though the artifact
/// were there. Matching is required to learn anything here, and that is the point.
///
/// **A refusal stays out of the error channel** because it is a NEGOTIATED outcome and not a fault:
/// the carrier worked, the target answered, and the answer was no. Putting it in `Err` would push
/// every caller into matching on error KINDS to tell a negotiation from a broken cable.
///
/// [`TransferAck::Rejected`] carries WHICH chunk, which is the question a refusal actually raises.
/// A bare `false` says it failed and nothing else, so a caller that wants to report or retry has to
/// have tracked the index itself -- state every caller would have to keep in order to use the
/// answer at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferAck {
    /// The target took every chunk it was offered.
    Accepted,
    /// The target refused a chunk. `chunk` is that chunk's index in the plan, counting from zero.
    Rejected {
        /// The index of the refused chunk in the plan that produced it.
        chunk: usize,
    },
}

/// What running a loaded artifact did: it ran, or the target refused the transfer and nothing
/// started.
///
/// The second outcome exists because a LOAD is chunked and each chunk is acknowledged, so a target
/// can decline the bytes -- an arena that cannot hold them is the ordinary case on a small part.
/// Reporting that as a carrier fault would send a reader to look at a cable for a board that
/// answered immediately and exactly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunOutcome {
    /// The artifact crossed, started, and finished. The result is the program's own.
    Ran(RunResult),
    /// The target refused a chunk of the transfer, so nothing started. `chunk` is its index.
    Rejected {
        /// The index of the refused chunk in the plan that produced it.
        chunk: usize,
    },
}

/// Whether one transfer status means the chunk was ACCEPTED.
///
/// `written, cannot read back` is an acceptance and not a failure, and reading it the other way
/// would abort a legitimate deploy: it is what a target says when it is holding back a partial flash
/// write unit, in preference to reporting a read-back match over bytes that are not in flash yet.
/// The two refusals are a failed write and a rejected range.
///
/// ONE definition, because several drivers ask it. The status carries more than an acceptance --
/// a CRC over the memory as assembled or the flash as read back -- and a caller that wants that
/// reads the reply itself.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
fn transfer_accepted(payload: &[u8]) -> bool {
    use lamella_wire::msg::xfer;
    matches!(payload.first().copied(), Some(xfer::MATCHED) | Some(xfer::WRITTEN_NOT_READ_BACK))
}

/// Host driver, blocking: persist `image` to the target's flash (it boots on reset), or
/// -- with `image` empty -- clear the deployed image (un-deploy). Returns whether the
/// target reported the flash write / clear succeeded.
///
/// A non-empty image goes through [`deploy_chunked_blocking`] rather than a second code path: the
/// deploy op is chunked without exception now, and a single-frame image is its degenerate one-chunk
/// case. That is what keeps the artifact kind on every frame, so an interrupted transfer cannot be
/// misread as a partial artifact of another kind.
///
/// # Errors
/// [`TransportError::Closed`] on timeout; otherwise a carrier [`TransportError`].
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn deploy_blocking(
    transport: &mut impl Transport,
    seq: u16,
    image: &[u8],
    timeout: Duration,
) -> Result<bool, TransportError> {
    use lamella_wire::Frame;
    if !image.is_empty() {
        return deploy_chunked_blocking(transport, seq, image, CHUNK_DATA_CAP, timeout);
    }
    transport.send(deploy::DEPLOY_CLEAR, seq, &[])?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(Frame { msg_type, seq: reply_seq, payload }) = transport.poll()? {
            if msg_type == deploy::XFER_RESULT && reply_seq == seq {
                return Ok(transfer_accepted(&payload));
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(TransportError::Closed)
}

/// The largest image slice one `DEPLOY_IMAGE` frame can carry: the frame's `u16` LEN cap, less the
/// 8-byte `(offset, total)` header this payload puts ahead of the bytes, rounded DOWN to 512 bytes
/// -- the LARGEST write granularity any supported target requires, so a chunk this size starts
/// on a write unit whichever board receives it.
///
/// **512 is a CEILING over the supported targets, not the write unit of any particular one.** A
/// target's write unit is a property of its flash controller and nothing on the wire carries it, so
/// rounding to the largest one any supported target requires is what makes a single number safe for
/// all of them. It is not a claim that a given board wants 512.
///
/// **This is a bound on what the wire can carry, not on throughput.** A frame's `LEN` is a `u16`
/// ([`lamella_wire::MAX_PAYLOAD`]), and a chunk spends 8 of those bytes on
/// `(offset, total)` before any image byte.
///
/// Without the refusal below it would be a bound on SILENT CORRUPTION: an `encode_frame` answering
/// an over-long payload by dropping the tail and CRCing what was left leaves the frame well formed,
/// so the target cannot tell it received a short chunk. An over-long payload is refused instead
/// ([`lamella_wire::TransportError::PayloadTooLarge`]), so exceeding this cap ABORTS a deploy rather
/// than completing one that leaves a hole in the image. **The cap is no less necessary for that; it
/// is what keeps a caller's `chunk-bytes` from reaching the refusal at all.**
///
/// Not gated behind the carrier features the deploy driver needs, so the arithmetic stays testable
/// without one.
pub const CHUNK_DATA_CAP: usize = ((u16::MAX as usize - 8) / 512) * 512;

/// Deploy a baked image to flash in CHUNKS, so an image larger than one 64 KB wire frame can cross
/// the wire (the frame LEN is a `u16`, so a single [`deploy_blocking`] silently truncates a
/// corlib-baked image). Sends `DEPLOY_IMAGE(offset, total, bytes)` frames in ascending order,
/// waiting for each `XFER_RESULT` ack before the next; returns whether every chunk was accepted.
/// `chunk_len` must be a multiple of the TARGET's flash write unit, so that each chunk starts on
/// one. Its UPPER bound is enforced here rather than required of the caller (see
/// [`CHUNK_DATA_CAP`]); the alignment is NOT, and cannot be -- see below.
///
/// # The write unit is a per-board fact this call cannot check
///
/// It is not one number: across the supported targets the accepted offset must be a multiple of
/// anything from 4 bytes to 512. **Nothing in the handshake or the manifest carries which**, so a
/// caller choosing `chunk_len` is choosing against a board property it has no way to read. Passing
/// [`CHUNK_DATA_CAP`]'s 512-byte granularity is safe on every supported target; anything smaller is
/// safe only where the target's own unit divides it.
///
/// **An unaligned chunk is not uniformly refused**, so the symptom of a wrong `chunk_len` depends on
/// which target received it -- which is the reading that gets blamed on the board.
///
/// [`lamella_wire::Negotiated::max_chunk_data`] answers the OTHER half -- how much the target's
/// carrier can absorb -- and a caller must satisfy both. On a small ring the two can be jointly
/// unsatisfiable: a 256-byte receive ring cannot carry a 512-byte-aligned chunk at all, and that is
/// a fact about the board rather than something a caller can pick its way around.
///
/// # Errors
/// Propagates a [`TransportError`], or reports the wire closed if a chunk goes unacked past `timeout`.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn deploy_chunked_blocking(
    transport: &mut impl Transport,
    seq: u16,
    image: &[u8],
    chunk_len: usize,
    timeout: Duration,
) -> Result<bool, TransportError> {
    use lamella_wire::Frame;
    let chunk_len = chunk_len.clamp(1, CHUNK_DATA_CAP);
    let total = image.len() as u32;
    let mut offset = 0usize;
    while offset < image.len() {
        let end = (offset + chunk_len).min(image.len());
        let mut payload = Vec::with_capacity(8 + (end - offset));
        payload.extend_from_slice(&(offset as u32).to_le_bytes());
        payload.extend_from_slice(&total.to_le_bytes());
        payload.extend_from_slice(&image[offset..end]);
        transport.send(deploy::DEPLOY_IMAGE, seq, &payload)?;

        let deadline = Instant::now() + timeout;
        let mut acked = None;
        'wait: while Instant::now() < deadline {
            while let Some(Frame { msg_type, seq: reply_seq, payload }) = transport.poll()? {
                if msg_type == deploy::XFER_RESULT && reply_seq == seq {
                    acked = Some(transfer_accepted(&payload));
                    break 'wait;
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        match acked {
            Some(true) => {}
            Some(false) => return Ok(false),
            None => return Err(TransportError::Closed),
        }
        offset = end;
    }
    Ok(true)
}

/// The largest bundle slice one frame can carry: the frame's `u16` LEN cap, less the 8-byte
/// `(offset, total)` header every chunk carries, rounded DOWN to a 4-byte word.
///
/// # Why this is not published
///
/// A published constant is the one form that cannot stay flexible, because it invites arithmetic
/// at a call site: `((u16::MAX - 8) / 4) * 4` publishes an 8-byte header and a `u16` length field
/// AS A NUMBER, and every caller who computes with it is silently wrong the day either changes.
/// It is also unnecessary -- [`BundleChunks::new`] clamps, so passing the bundle's own length is
/// correct without a caller ever seeing the cap.
///
/// **The alignment is the target's, not a preference.** A bundle is written to flash with word
/// stores, so `deploy_bundle_chunk` REFUSES an `offset % 4 != 0` outright. That makes an unaligned
/// `chunk_len` fail on the SECOND frame rather than the first -- chunk zero is always aligned -- which
/// is the reading that sends someone to look at the bundle instead of at the caller's chunk size.
///
/// NOTE: this is a smaller word than [`CHUNK_DATA_CAP`]'s 512, and deliberately a separate
/// constant. A baked image must start each chunk on a 512-byte flash PAGE because the image path
/// erases per page; the bundle path erases once up front, so word alignment is all its stores
/// require. Reusing the image cap here would work, and would quietly demand 128x more alignment
/// than the target asks for.
pub(crate) const BUNDLE_CHUNK_DATA_CAP: usize = ((u16::MAX as usize - 8) / 4) * 4;

/// Host driver: put ONE planned chunk of a bundle on the wire as a LOAD -- into the target's RAM,
/// without persisting it. The ack is the caller's to collect with [`try_recv_bundle_ack`].
///
/// The mirror of [`send_bundle_chunk`], which puts the same planned chunk in the persistent region
/// instead. Both take a frame from [`BundleChunks`]: the two halves of the transfer differ only in
/// where the bytes land, so they share one plan, one chunk shape and one reply.
///
/// **A LOAD STARTS NOTHING.** It places an artifact and stops there; starting it is a separate op,
/// which is what makes the same transfer usable for running now and for inspecting before running.
///
/// A bundle is an artifact the target loads through a front end rather than a baked image, so this
/// is the path a Python bundle takes and a baked image never does.
///
/// **GATE ON [`lamella_wire::Capabilities::BUNDLE`], from the `HELLO_ACK`**, which
/// [`hello_blocking`] already returns -- the bit exists precisely so a host can decide without a
/// round trip. This follows the house contract that the CALLER picks the path from the negotiated
/// caps, the same way it picks `send_program` vs `send_image`. Calling it against a target without
/// the bit is not unsafe -- the target refuses by name and this reports
/// [`TransportError::Refused`] promptly rather than timing out -- but it spends a round trip to
/// learn what the session already knew.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn send_run_bundle(
    transport: &mut impl Transport,
    seq: u16,
    chunk: &[u8],
) -> Result<(), TransportError> {
    transport.send(load::LOAD_BUNDLE, seq, chunk)
}

/// Host driver, blocking: load `bundle` into the target's RAM chunk by chunk, then start it and wait
/// for the result.
///
/// **A CONVENIENCE OVER [`BundleChunks`], [`send_run_bundle`] AND [`try_recv_bundle_ack`], NOT THE
/// CONTRACT.** A host with an event loop, a cancel button or an agent behind it drives those three
/// and owns its own waiting; nothing here needs a thread.
///
/// # Choosing `timeout`
///
/// It bounds each WAIT, not the call: one wait per chunk acknowledged, then one for the program to
/// finish. So it is governed by two things a caller knows and this function does not -- how long a
/// round trip takes on the carrier in use, and how long the PROGRAM runs.
///
/// The transfer term is arithmetic: a bundle divided by the frame size, times a round trip. Over a
/// serial carrier at 115200 baud a full frame is about 5.7 seconds of line time, so a per-wait
/// bound under that will time out a transfer that is proceeding normally. The program term has no
/// formula at all -- a blink never returns.
///
/// Worked example: a 40 KB bundle over a 115200 carrier crosses in one frame, so a wait of 30
/// seconds covers the transfer with room, and the choice is then entirely about the program. There
/// is deliberately no default constant here: a value that suits a test suite would silently cut off
/// a program somebody meant to watch.
///
/// # Errors
/// [`TransportError::Refused`] if the target does not implement the op, [`TransportError::Closed`]
/// on timeout; otherwise a carrier [`TransportError`].
///
/// A target REFUSING a chunk is not among them: it is [`RunOutcome::Rejected`], because the carrier
/// worked and the target answered.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn run_bundle_blocking(
    transport: &mut impl Transport,
    seq: u16,
    bundle: &[u8],
    timeout: Duration,
) -> Result<RunOutcome, TransportError> {
    let mut chunks = BundleChunks::new(bundle, BUNDLE_CHUNK_DATA_CAP);
    let mut index = 0usize;
    while let Some(chunk) = chunks.next() {
        send_run_bundle(transport, seq, &chunk)?;
        if let TransferAck::Rejected { chunk } = await_transfer_ack(transport, seq, index, timeout)? {
            return Ok(RunOutcome::Rejected { chunk });
        }
        index += 1;
    }
    transport.send(exec::EXEC, seq, &[exec::exec_source::LOADED, 0])?;
    Ok(RunOutcome::Ran(await_result(transport, seq, timeout)?))
}

/// Wait for one chunk's transfer ack.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
fn await_transfer_ack(
    transport: &mut impl Transport,
    seq: u16,
    chunk: usize,
    timeout: Duration,
) -> Result<TransferAck, TransportError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(ack) = try_recv_bundle_ack(transport, seq, chunk)? {
            return Ok(ack);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(TransportError::Closed)
}

/// **DEPLOYING a BUNDLE: the protocol, stated once.** A bundle persists to the target's deploy
/// region so it boots on reset -- the bundle counterpart of `deploy_chunked_blocking`.
///
/// **GATE ON [`lamella_wire::Capabilities::BUNDLE`]**, as for [`run_bundle_blocking`].
///
/// **Payload is ALWAYS `[offset u32][total u32][bytes]`**, so a bundle that fits one frame is the
/// degenerate one-chunk case rather than a second code path -- the protocol pays eight bytes on a
/// small bundle to keep the artifact kind on EVERY frame, so an interrupted transfer cannot be
/// misread as a partial artifact of another kind.
///
/// The chunk plan for a bundle deploy: **pure arithmetic, no I/O, no waiting.**
///
/// Every rule that decides what goes on the wire lives here and nowhere else: the clamp, the word
/// rounding, and the single frame an EMPTY bundle is still due. A host that drives its own event loop
/// iterates this and sends each chunk when it likes; [`deploy_bundle_blocking`] is one caller of it,
/// not the definition of the protocol.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub struct BundleChunks<'a> {
    bundle: &'a [u8],
    chunk_len: usize,
    offset: usize,
    done: bool,
}

#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
impl<'a> BundleChunks<'a> {
    /// Plan `bundle` into frames of at most `chunk_len` payload bytes.
    ///
    /// `chunk_len` is clamped to [`BUNDLE_CHUNK_DATA_CAP`] and rounded DOWN to a 4-byte word, and
    /// floored at one word: the target refuses an unaligned offset, an over-long payload aborts the
    /// deploy rather than short-writing it, and a zero would never advance. Rounding down rather
    /// than refusing keeps the obvious "send it in one frame" call -- passing the bundle's own
    /// length -- working on an odd-sized bundle.
    #[must_use]
    pub fn new(bundle: &'a [u8], chunk_len: usize) -> Self {
        Self {
            bundle,
            chunk_len: (chunk_len.min(BUNDLE_CHUNK_DATA_CAP) / 4 * 4).max(4),
            offset: 0,
            done: false,
        }
    }

    /// The next frame payload -- `[offset u32][total u32][bytes]` -- or `None` when the bundle is
    /// fully planned.
    ///
    /// **An EMPTY bundle yields exactly ONE frame** (`offset = 0, total = 0`): the target erases
    /// the header and commits a zero length, which is how it records that nothing is deployed.
    /// Yielding nothing would report a clear that never happened. Discarding a transfer is a
    /// different thing and has its own op.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<Vec<u8>> {
        if self.done {
            return None;
        }
        let end = (self.offset + self.chunk_len).min(self.bundle.len());
        let total = self.bundle.len() as u32;
        let mut payload = Vec::with_capacity(8 + (end - self.offset));
        payload.extend_from_slice(&(self.offset as u32).to_le_bytes());
        payload.extend_from_slice(&total.to_le_bytes());
        payload.extend_from_slice(&self.bundle[self.offset..end]);
        self.offset = end;
        self.done = self.offset >= self.bundle.len();
        Some(payload)
    }
}

/// Host driver: put one planned chunk on the wire. The ack is the caller's to collect.
///
/// # Errors
/// Propagates a [`TransportError`] from the carrier.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn send_bundle_chunk(
    transport: &mut impl Transport,
    seq: u16,
    chunk: &[u8],
) -> Result<(), TransportError> {
    transport.send(deploy::DEPLOY_BUNDLE, seq, chunk)
}

/// Host driver: poll for one chunk's `XFER_RESULT` (non-blocking; `Ok(None)` if it is not in yet).
///
/// `chunk` is the index of the chunk this poll is FOR, and it is an argument because only the
/// caller knows it: the reply carries the request's sequence number, not a position in a plan.
/// Passing it is what lets a [`TransferAck::Rejected`] answer the question it raises without the
/// caller having tracked the index alongside.
///
/// `Ok(None)` means ONE thing -- nothing has arrived -- so a target that does not implement the
/// op comes back as [`TransportError::Refused`] rather than as a caller polling a healthy link to
/// its deadline and blaming the cable.
///
/// # Errors
/// [`TransportError::Refused`] when the target answered `ERROR` for this sequence; otherwise a
/// carrier [`TransportError`].
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn try_recv_bundle_ack(
    transport: &mut impl Transport,
    seq: u16,
    chunk: usize,
) -> Result<Option<TransferAck>, TransportError> {
    use lamella_wire::{Frame, msg};
    while let Some(Frame { msg_type, seq: reply_seq, payload }) = transport.poll()? {
        if reply_seq != seq {
            continue;
        }
        if msg_type == deploy::XFER_RESULT {
            return Ok(Some(if transfer_accepted(&payload) {
                TransferAck::Accepted
            } else {
                TransferAck::Rejected { chunk }
            }));
        }
        if msg_type == msg::ERROR {
            return Err(TransportError::Refused {
                reason: payload.first().copied().unwrap_or(0),
                msg_type: payload.get(1).copied().unwrap_or(0),
            });
        }
    }
    Ok(None)
}

/// Host driver, blocking: send every chunk and wait for each ack. Returns whether every chunk was
/// accepted.
///
/// **A CONVENIENCE OVER [`BundleChunks`], [`send_bundle_chunk`] AND [`try_recv_bundle_ack`], NOT THE
/// CONTRACT.** The protocol is those three; this is the one waiting policy that suits a CLI. A host
/// with an event loop drives the same three and never blocks.
///
/// # Errors
/// [`TransportError::Refused`] if the target does not implement the op, [`TransportError::Closed`]
/// if a chunk goes unacked past `timeout`; otherwise a carrier [`TransportError`].
///
/// A target REJECTING a chunk is not among them: it is [`TransferAck::Rejected`], and it carries
/// which chunk.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn deploy_bundle_blocking(
    transport: &mut impl Transport,
    seq: u16,
    bundle: &[u8],
    chunk_len: usize,
    timeout: Duration,
) -> Result<TransferAck, TransportError> {
    let mut chunks = BundleChunks::new(bundle, chunk_len);
    let mut index = 0usize;
    while let Some(chunk) = chunks.next() {
        send_bundle_chunk(transport, seq, &chunk)?;
        if let rejected @ TransferAck::Rejected { .. } =
            await_transfer_ack(transport, seq, index, timeout)?
        {
            return Ok(rejected);
        }
        index += 1;
    }
    Ok(TransferAck::Accepted)
}

/// Host driver, blocking: query the deployed artifact's content checksum -- `Some(checksum)` when
/// the target holds one it has VERIFIED, `None` otherwise. Compare it to a freshly-baked image's
/// [`lamella_runner::baked_image_checksum`] to SKIP re-deploying an image the target already holds
/// (content-addressed deploy -- the phone-style "already installed, just run it").
///
/// The reply distinguishes five states -- nothing there, verified, present but unverifiable, a
/// different runtime's artifact, a different partition layout -- and this reduces four of them to
/// `None`. That reduction is right for the question this function asks, because every one of the
/// four is a case where the caller should deploy; a caller that wants to say WHY reads the reply.
///
/// # Errors
/// [`TransportError::Closed`] on timeout; otherwise a carrier [`TransportError`].
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn deployed_status_blocking(
    transport: &mut impl Transport,
    seq: u16,
    timeout: Duration,
) -> Result<Option<u64>, TransportError> {
    use lamella_wire::Frame;
    transport.send(deploy::DEPLOY_STATUS, seq, &[])?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(Frame { msg_type, seq: reply_seq, payload }) = transport.poll()? {
            if msg_type == deploy::DEPLOY_STATUS_RESULT && reply_seq == seq {
                let verified = payload.first().copied() == Some(deploy::deploy_state::VERIFIED);
                if verified && payload.len() >= 10 {
                    let sum = u64::from_le_bytes(payload[2..10].try_into().unwrap());
                    return Ok(Some(sum));
                }
                return Ok(None);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(TransportError::Closed)
}

/// Host driver: pull the target's full resident-profile manifest: the identity, the resident
/// library's capability-symbol bitmap, the profile name, and the complete listing of runtime seams
/// the target registers. Ask only when the `HELLO_ACK` identity's hash misses the host's manifest
/// cache -- a KNOWN board costs no extra round trip at all.
///
/// The reply is CHUNKED, `offset, total, bytes` like every other transfer, which is what makes the
/// manifest's own promise of an unconstrained profile description true rather than aspirational: one
/// frame's length field caps at 65,535 bytes, and the manifest grows with the seam count and with
/// each resident runtime a board carries. Chunks are reassembled here; a chunk arriving out of order
/// re-requests from the point already held rather than restarting, which is what the request's
/// offset is for.
///
/// # Errors
/// [`TransportError::Closed`] on timeout or an undecodable manifest; otherwise a carrier error.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn profile_manifest_blocking(
    transport: &mut impl Transport,
    seq: u16,
    timeout: Duration,
) -> Result<lamella_wire::ProfileManifest, TransportError> {
    use lamella_runner::profile;
    use lamella_wire::{Frame, ProfileManifest};
    let mut held: Vec<u8> = Vec::new();
    transport.send(profile::PROFILE_GET, seq, &0u32.to_le_bytes())?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(Frame { msg_type, seq: reply_seq, payload }) = transport.poll()? {
            if msg_type != profile::PROFILE_MANIFEST || reply_seq != seq || payload.len() < 8 {
                continue;
            }
            let offset = u32::from_le_bytes(payload[0..4].try_into().unwrap_or_default()) as usize;
            let total = u32::from_le_bytes(payload[4..8].try_into().unwrap_or_default()) as usize;
            if offset != held.len() {
                transport.send(profile::PROFILE_GET, seq, &(held.len() as u32).to_le_bytes())?;
                continue;
            }
            held.extend_from_slice(&payload[8..]);
            if held.len() >= total {
                return ProfileManifest::decode(&held).ok_or(TransportError::Closed);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(TransportError::Closed)
}

/// Host driver: tell the target to start its DEPLOYED artifact now, so deploy-then-run needs no
/// debug probe.
///
/// The target acknowledges before anything the start implies, so a host can tell an accepted start
/// from a target that simply stopped answering. Do not follow it with a `HELLO` -- that is a
/// separate question and asking it is not free.
///
/// # Errors
/// A carrier [`TransportError`] if the command could not be sent.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn send_deploy_run(transport: &mut impl Transport, seq: u16) -> Result<(), TransportError> {
    transport.send(exec::EXEC, seq, &[exec::exec_source::DEPLOYED, 0])
}

#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
fn await_result(
    transport: &mut impl Transport,
    seq: u16,
    timeout: Duration,
) -> Result<RunResult, TransportError> {
    let mut run = RunCollector::new(seq);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if run.poll(transport)? {
            return run.finish().ok_or(TransportError::Closed);
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(TransportError::Closed)
}

#[cfg(all(test, feature = "usb"))]
mod usb_target_tests {
    use super::parse_usb_target;

    #[test]
    fn bare_usb_is_the_link_ids_first_board() {
        assert_eq!(parse_usb_target("usb"), (0x39e9, 0x0001, None));
    }

    #[test]
    fn id_pair_and_optional_serial() {
        assert_eq!(parse_usb_target("usb:39e9:0001"), (0x39e9, 0x0001, None));
        assert_eq!(
            parse_usb_target("usb:39e9:0001:E463"),
            (0x39e9, 0x0001, Some("E463".to_string()))
        );
    }

    #[test]
    fn lone_token_is_always_a_serial() {
        assert_eq!(parse_usb_target("usb:BOARD-0001"), (0x39e9, 0x0001, Some("BOARD-0001".into())));
        assert_eq!(parse_usb_target("usb:E46341"), (0x39e9, 0x0001, Some("E46341".into())));
        assert_eq!(parse_usb_target("usb:7B53"), (0x39e9, 0x0001, Some("7B53".into())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_wire::MemTransport;

    /// A TARGET THAT REFUSES THE VERSION IS REPORTED AS A VERSION REFUSAL, NOT AS A CLOSED LINK.
    ///
    /// This is the defect the variant was added for. `hello_blocking` decoded the `HELLO_NAK` and
    /// threw it away, returning `Closed` -- so the protocol's ONE deliberate incompatibility signal
    /// arrived at the user as *the link is closed*, which is the reading that sends somebody to
    /// check a cable that is fine.
    #[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
    #[test]
    fn a_version_refusal_is_not_reported_as_a_closed_link() {
        use lamella_wire::{HelloNak, ProtocolRange, msg};

        let mut transport = MemTransport::new();
        let nak = HelloNak { target_range: ProtocolRange { min: 7, max: 9 } };
        let frame = lamella_wire::encode_frame(msg::HELLO_NAK, 0, &nak.encode()).expect("frames");
        transport.feed(&frame);

        let error = hello_blocking(
            &mut transport,
            0,
            lamella_wire::Capabilities(0),
            Duration::from_millis(200),
        )
        .expect_err("a NAK is not a session");

        match error {
            TransportError::VersionMismatch { target_min, target_max } => {
                assert_eq!((target_min, target_max), (7, 9), "the target's own range survives");
            }
            other => panic!("a version refusal must not arrive as {other:?}"),
        }
    }

    /// AND THE SENTENCE NAMES THE DIRECTION, because that is what decides where a person goes next.
    ///
    /// Two version numbers on their own leave the reader to work out which end is behind, and the
    /// two answers send them to opposite places.
    ///
    #[test]
    fn the_version_sentence_says_which_end_has_to_move() {
        let behind = version_mismatch(3, 1, 1);
        assert!(behind.contains("BOARD is behind"), "got {behind}");
        assert!(behind.contains("reflash"), "and says what to do: {behind}");
        assert!(behind.contains("version 1"), "a single version does not read as a range: {behind}");

        let ahead = version_mismatch(1, 4, 6);
        assert!(ahead.contains("TOOL is behind"), "got {ahead}");
        assert!(ahead.contains("update the tools"), "and says what to do: {ahead}");
        assert!(ahead.contains("versions 4 to 6"), "a real range reads as a range: {ahead}");

        for sentence in [&behind, &ahead] {
            assert!(sentence.contains("cable"), "must rule the cable out: {sentence}");
        }

        for sentence in [&behind, &ahead] {
            assert!(!sentence.contains("  "), "a doubled space reached the sentence: {sentence}");
        }
    }

    /// A NAK THAT DID NOT DECODE STILL PRODUCES A SENTENCE, AND IT DOES NOT INVENT A DIRECTION.
    ///
    /// `0` is not a protocol version, so it cannot be mistaken for a real range -- but a renderer
    /// comparing it numerically would confidently report the board as behind, which is a direction
    /// invented out of a decode failure.
    #[test]
    fn an_undecodable_refusal_does_not_claim_to_know_which_end_is_behind() {
        let sentence = version_mismatch(lamella_wire::PROTOCOL_VERSION, 0, 0);
        assert!(!sentence.contains("BOARD is behind"), "got {sentence}");
        assert!(!sentence.contains("TOOL is behind"), "got {sentence}");
        assert!(sentence.contains("did not decode"), "and says what happened: {sentence}");
        assert!(!sentence.contains("  "), "a doubled space reached the sentence: {sentence}");
    }

    #[cfg(feature = "usb")]
    fn listed_board(vendor_id: u16, product_id: u16) -> lamella_usbbulk::DeviceInfo {
        lamella_usbbulk::DeviceInfo {
            vendor_id,
            product_id,
            serial_number: Some(String::from("BENCHSERIAL0001")),
            product: Some(String::from("Lamella Link (test)")),
            interface_name: None,
        }
    }

    #[cfg(feature = "usb")]
    #[test]
    fn a_current_board_is_recognized_and_has_nothing_said_about_it() {
        use lamella_wire::usb::{LinkIdentity, PID, VID};
        let era = firmware_era(&listed_board(VID, PID)).expect("a Link");
        assert_eq!(era, LinkIdentity::Current);
        assert_eq!(era_note(era), None);
    }

    #[cfg(feature = "usb")]
    #[test]
    fn a_legacy_board_is_recognized_and_the_note_says_why_it_would_be_missing() {
        use lamella_wire::usb::{LEGACY_VID, LinkIdentity, PID};
        let era = firmware_era(&listed_board(LEGACY_VID, PID)).expect("a Link");
        assert_eq!(era, LinkIdentity::Legacy);
        let note = era_note(era).expect("a legacy board has something said about it");
        assert!(note.contains("does not list it at all"), "got {note}");
        assert!(note.contains("Reflash"), "and says what to do: {note}");
    }

    #[cfg(feature = "usb")]
    #[test]
    fn something_that_is_not_a_link_is_not_given_an_era_at_all() {
        use lamella_wire::usb::{LEGACY_VID, PID, VID};
        assert_eq!(firmware_era(&listed_board(0x2e8a, 0x000c)), None, "a debug probe is not a Link");
        assert_eq!(firmware_era(&listed_board(VID, PID + 1)), None, "the product id matters too");
        assert_eq!(firmware_era(&listed_board(LEGACY_VID, PID + 1)), None, "on both pairs");
    }

    /// One accepted transfer ack, framed: `status | crc32`. Built here rather than spelled at each
    /// site so a test cannot come to disagree with the ladder about what acceptance looks like.
    #[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
    fn xfer_ack(seq: u16) -> Vec<u8> {
        let mut payload = vec![lamella_wire::msg::xfer::MATCHED];
        payload.extend_from_slice(&0u32.to_le_bytes());
        encode_frame(deploy::XFER_RESULT, seq, &payload).expect("a 5-byte ack frames")
    }

    fn corlib() -> Option<Vec<u8>> {
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../lamella-load/tests/fixtures/corlib.dll")).ok()
    }

    fn hello() -> Option<Vec<u8>> {
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hello.exe")).ok()
    }

    #[test]
    fn run_program_executes_and_captures_output() {
        let (Some(corlib), Some(program)) = (corlib(), hello()) else { return };
        let result = run_program(&corlib, &program);
        assert_eq!(result.exit, 7);
        assert_eq!(result.stdout, "hi\n");
    }

    #[test]
    fn tier0_repl_round_trips_over_the_wire() {
        let (Some(corlib), Some(program)) = (corlib(), hello()) else { return };

        let mut driver = MemTransport::new();
        let mut runner = MemTransport::new();

        send_program(&mut driver, 1, &program).unwrap();
        runner.feed(&driver.take_sent());

        let mut arena = ArtifactLoad::new();
        while serve_one(&mut runner, &corlib, &mut arena).unwrap() {}
        driver.feed(&runner.take_sent());

        let mut run = RunCollector::new(1);
        assert!(run.poll(&mut driver).unwrap(), "the execution ended");
        let result = run.finish().expect("a result arrived");
        assert_eq!(result.exit, 7);
        assert_eq!(result.stdout, "hi\n");
    }

    /// THE HAZARD [`CHUNK_DATA_CAP`] EXISTS FOR.
    ///
    /// A `DEPLOY_IMAGE` payload carries 8 bytes of `(offset, total)` ahead of the image bytes, and
    /// those 8 bytes are what make 65536 -- round, and a legal multiple of the 512-byte flash page
    /// the doc asks for -- the value that goes over the wire's `u16` length field.
    ///
    /// **The alternative is silent corruption.** An `encode_frame` that dropped the tail and CRCed
    /// what was left would put something on the wire indistinguishable from a good frame: no short
    /// read to notice, no CRC failure to resynchronize on.
    ///
    /// So `encode_frame` REFUSES an over-long payload, and the mistake surfaces as a
    /// `TransportError::PayloadTooLarge` at the sender instead of a hole in the image at the
    /// receiver. **The cap is still exactly as necessary** -- it is what keeps a caller's
    /// `chunk-bytes` from reaching that refusal at all -- but it now bounds an ABORTED deploy rather
    /// than a silently corrupt one, which is the difference between a deploy that stops and a board
    /// that boots a broken image.
    #[test]
    fn an_oversized_chunk_is_refused_rather_than_silently_truncated() {
        let over = vec![0xA5u8; 8 + 65536];
        assert_eq!(
            lamella_wire::encode_frame(0x20, 1, &over),
            None,
            "an over-long chunk must be refused; truncating it produced a frame the target \
             could not tell from a good one"
        );

        let capped = vec![0xA5u8; 8 + CHUNK_DATA_CAP];
        let bytes = lamella_wire::encode_frame(0x20, 1, &capped).expect("a capped chunk frames");
        let mut reader = lamella_wire::FrameReader::new();
        reader.push(&bytes);
        let frame = reader.next_frame().expect("a capped chunk is a well-formed frame");
        assert_eq!(frame.payload.len(), capped.len(), "a capped chunk must cross whole");
        assert_eq!(CHUNK_DATA_CAP % 512, 0, "a chunk must still start on a 512-byte flash page");
    }

    /// The same defect through the REAL deploy loop, which is what the clamp is actually protecting.
    /// `wire-flash` takes `chunk-bytes` straight off the command line, so 65536 is a value a person
    /// types. Un-clamped, the first chunk loses 9 bytes, the loop advances `offset` by the full 65536
    /// regardless, and the image arrives WITH A HOLE while the call still returns `Ok(true)` -- so the
    /// assertion that matters is coverage, not length.
    #[test]
    #[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
    fn every_byte_crosses_even_when_the_caller_asks_for_an_oversized_chunk() {
        let image: Vec<u8> = (0..(2 * 65536 + 777)).map(|i| (i % 251) as u8).collect();
        let mut transport = MemTransport::new();
        for _ in 0..8 {
            transport.feed(&xfer_ack(3));
        }

        let ok = deploy_chunked_blocking(&mut transport, 3, &image, 65536, Duration::from_secs(5))
            .expect("the in-memory carrier never errors");
        assert!(ok, "every chunk acked");

        let mut got = vec![0u8; image.len()];
        let mut covered = vec![false; image.len()];
        let mut reader = FrameReader::new();
        reader.push(&transport.take_sent());
        while let Some(frame) = reader.next_frame() {
            if frame.msg_type != deploy::DEPLOY_IMAGE {
                continue;
            }
            let offset = u32::from_le_bytes(frame.payload[0..4].try_into().unwrap()) as usize;
            for (i, byte) in frame.payload[8..].iter().enumerate() {
                got[offset + i] = *byte;
                covered[offset + i] = true;
            }
        }
        assert!(covered.iter().all(|c| *c), "a byte of the image never crossed, and the deploy said OK");
        assert_eq!(got, image, "the image reassembled wrong");
    }

    /// The bundle deploy's own version of the coverage test, plus the constraint that is NOT the
    /// image path's: **every offset must be 4-byte aligned**, because `deploy_bundle_chunk` refuses
    /// `offset % 4 != 0` outright. The caller here asks for 1,023 -- not a multiple of 4 -- which is
    /// exactly the value a person types. Un-rounded, chunk zero lands at offset 0 and is ACCEPTED,
    /// and every chunk after it is refused; the deploy then returns `Ok(false)` naming a bundle that
    /// is fine, so the assertion that matters is alignment and coverage together.
    #[test]
    #[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
    fn every_bundle_chunk_starts_on_a_word_and_the_whole_bundle_crosses() {
        let bundle: Vec<u8> = (0..10_003).map(|i| (i % 251) as u8).collect();
        let mut transport = MemTransport::new();
        for _ in 0..32 {
            transport.feed(&xfer_ack(7));
        }

        let ack = deploy_bundle_blocking(&mut transport, 7, &bundle, 1023, Duration::from_secs(5))
            .expect("the in-memory carrier never errors");
        assert_eq!(ack, TransferAck::Accepted, "every chunk acked");

        let mut got = vec![0u8; bundle.len()];
        let mut covered = vec![false; bundle.len()];
        let mut frames = 0usize;
        let mut reader = FrameReader::new();
        reader.push(&transport.take_sent());
        while let Some(frame) = reader.next_frame() {
            assert_eq!(
                frame.msg_type,
                deploy::DEPLOY_BUNDLE,
                "a bundle must never travel under another op"
            );
            let offset = u32::from_le_bytes(frame.payload[0..4].try_into().unwrap()) as usize;
            let total = u32::from_le_bytes(frame.payload[4..8].try_into().unwrap()) as usize;
            assert_eq!(offset % 4, 0, "offset {offset} is not word aligned; the target refuses it");
            assert_eq!(total, bundle.len(), "the total must be the whole bundle on EVERY frame");
            for (i, byte) in frame.payload[8..].iter().enumerate() {
                got[offset + i] = *byte;
                covered[offset + i] = true;
            }
            frames += 1;
        }
        assert!(frames > 1, "a 10 KB bundle at 1,020 bytes a chunk must actually chunk");
        assert!(covered.iter().all(|c| *c), "a byte never crossed and the deploy said OK");
        assert_eq!(got, bundle, "the bundle reassembled wrong");
    }

    /// An empty bundle is the CLEAR, and a clear that sends nothing is a clear that did not happen.
    /// `while offset < len` -- the image path's loop shape -- sends zero frames here and returns
    /// `Ok(true)`, reporting a region wiped that still holds the old program.
    #[test]
    #[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
    fn an_empty_bundle_still_sends_one_frame_because_that_is_the_clear() {
        let mut transport = MemTransport::new();
        transport.feed(&xfer_ack(9));

        let ack = deploy_bundle_blocking(&mut transport, 9, &[], 4096, Duration::from_secs(5))
            .expect("the in-memory carrier never errors");
        assert_eq!(ack, TransferAck::Accepted, "the clear was acked");

        let mut reader = FrameReader::new();
        reader.push(&transport.take_sent());
        let frame = reader.next_frame().expect("a clear must put ONE frame on the wire");
        assert_eq!(frame.msg_type, deploy::DEPLOY_BUNDLE);
        assert_eq!(frame.payload, vec![0u8; 8], "offset 0, total 0, and no bytes");
        assert!(reader.next_frame().is_none(), "a clear is exactly one frame");
    }

    /// A target that does not implement the op answers `ERROR`. Without an arm for it the loop polls
    /// to its deadline and reports `Closed` -- "the link is closed" for a board that answered
    /// immediately, which is the one reading that sends someone to check the cable.
    #[test]
    #[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
    fn a_target_that_refuses_the_bundle_op_is_reported_as_refused_not_as_a_closed_link() {
        let mut transport = MemTransport::new();
        transport.feed(&encode_frame(lamella_wire::msg::ERROR, 5, &[]).expect("an ERROR frames"));

        let error = deploy_bundle_blocking(&mut transport, 5, &[1, 2, 3, 4], 4096, Duration::from_secs(5))
            .expect_err("a refusal is an error, not an Ok(false)");
        assert!(
            matches!(error, TransportError::Refused { .. }),
            "expected Refused, got {error:?} -- a refusal reported as a timeout is the defect"
        );
    }

    /// Both halves take the SAME chunk shape. Where they differ is where the bytes land, and that is
    /// deliberate, so a header helpfully added here would be decoded as the first eight bytes of the
    /// program.
    ///
    /// A LOAD places an artifact and STARTS NOTHING, so the two halves have to both appear on the
    /// wire and in that order. This asserts what CROSSED rather than what came back: the completion
    /// event and its decode belong to the target-side runner, and a test that fed one here would be
    /// asserting that lane's shape rather than this driver's.
    #[test]
    #[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
    fn a_bundle_run_loads_in_chunks_and_then_starts_what_it_loaded() {
        let bundle: Vec<u8> = (0..64).map(|i| (i % 251) as u8).collect();
        let mut transport = MemTransport::new();
        transport.feed(&xfer_ack(6));

        let _ = run_bundle_blocking(&mut transport, 6, &bundle, Duration::from_millis(20));

        let mut reader = FrameReader::new();
        reader.push(&transport.take_sent());
        let load = reader.next_frame().expect("the load chunk");
        assert_eq!(load.msg_type, load::LOAD_BUNDLE);
        assert_eq!(&load.payload[0..4], &0u32.to_le_bytes(), "offset 0");
        assert_eq!(&load.payload[4..8], &(bundle.len() as u32).to_le_bytes(), "the whole length");
        assert_eq!(&load.payload[8..], &bundle[..], "then the bundle");

        let start = reader.next_frame().expect("the start");
        assert_eq!(start.msg_type, exec::EXEC);
        assert_eq!(start.payload, vec![exec::exec_source::LOADED, 0], "from RAM, running");
    }
}
