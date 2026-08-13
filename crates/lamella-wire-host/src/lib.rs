//! The HOST side of the Lamella Link debug + REPL channel:

pub use lamella_runner::{
    RunResult, baked_image_checksum, debug, deploy, repl, run_program, send_image, send_program,
    serve_one, try_recv_result,
};

pub mod engine;

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
/// writes to the wrong board, and this bench has four probes whose serials share a prefix.
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
    /// # Errors
    /// [`TransportError::Carrier`] if the platform cannot enumerate by interface GUID.
    pub fn list() -> Result<Vec<lamella_usbbulk::DeviceInfo>, TransportError> {
        match lamella_usbbulk::enumerate_interface(lamella_wire::usb::WINUSB_INTERFACE_GUID) {
            Ok(boards) => Ok(boards),
            Err(lamella_usbbulk::Error::Unsupported) => Ok(lamella_usbbulk::enumerate()
                .map_err(|_| TransportError::Carrier)?
                .into_iter()
                .filter(|board| board.vendor_id == lamella_wire::usb::VID)
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
    use lamella_wire::{Hello, HelloAck, PROTOCOL_VERSION, ProtocolRange, host_finish, msg};
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
                msg::NAK if frame.seq == seq => return Err(TransportError::Closed),
                msg::ERROR if frame.seq == seq => {
                    return Err(TransportError::Refused {
                        reason: frame.payload.first().copied().unwrap_or(0),
                        msg_type: frame.payload.get(1).copied().unwrap_or(0),
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

/// Host driver, blocking: persist `image` to the target's flash (it boots on reset), or
/// -- with `image` empty -- clear the deployed image (un-deploy). Returns whether the
/// target reported the flash write / clear succeeded.
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
    let msg_type = if image.is_empty() { deploy::DEPLOY_CLEAR } else { deploy::DEPLOY_IMAGE };
    transport.send(msg_type, seq, image)?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(Frame { msg_type, seq: reply_seq, payload }) = transport.poll()? {
            if msg_type == deploy::DEPLOY_RESULT && reply_seq == seq {
                return Ok(payload.first().copied().unwrap_or(0) == 1);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(TransportError::Closed)
}

/// The largest image slice one `DEPLOY_CHUNK` frame can carry: the frame's `u16` LEN cap, less the
/// 8-byte `(offset, total)` header this payload puts ahead of the bytes, rounded DOWN to the 512-byte
/// flash page a chunk must start on.
///
/// **This is a bound on what the wire can carry, not on throughput.** A frame's `LEN` is a `u16`
/// ([`lamella_wire::MAX_PAYLOAD`]), and a `DEPLOY_CHUNK` spends 8 of those bytes on
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
/// corlib-baked image). Sends `DEPLOY_CHUNK(offset, total, bytes)` frames in ascending order,
/// waiting for each `DEPLOY_RESULT` ack before the next; returns whether every chunk verified.
/// `chunk_len` must be a multiple of the target's flash page (512 B) so each chunk starts on a page
/// boundary; its UPPER bound is enforced here rather than required of the caller (see
/// [`CHUNK_DATA_CAP`]). A single-chunk deploy (image `<= chunk_len`) is equivalent to a
/// `DEPLOY_IMAGE`, without the truncation risk.
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
        transport.send(deploy::DEPLOY_CHUNK, seq, &payload)?;

        let deadline = Instant::now() + timeout;
        let mut acked = None;
        'wait: while Instant::now() < deadline {
            while let Some(Frame { msg_type, seq: reply_seq, payload }) = transport.poll()? {
                if msg_type == deploy::DEPLOY_RESULT && reply_seq == seq {
                    acked = Some(payload.first().copied().unwrap_or(0) == 1);
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

/// Host driver, blocking: query the deployed image's content checksum -- `Some(checksum)` if the
/// target holds a valid image, `None` if none. Compare it to a freshly-baked image's
/// [`lamella_runner::baked_image_checksum`] to SKIP re-deploying an image the target already holds
/// (content-addressed deploy -- the phone-style "already installed, just run it").
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
                if payload.first().copied().unwrap_or(0) == 1 && payload.len() >= 9 {
                    let sum = u64::from_le_bytes(payload[1..9].try_into().unwrap());
                    return Ok(Some(sum));
                }
                return Ok(None);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(TransportError::Closed)
}

/// Host driver: pull the target's full resident-profile manifest ([`profile::GET_PROFILE`] ->
/// [`profile::PROFILE_MANIFEST`]): the identity + the complete intrinsic-id listing of the
/// resident surface. Ask only when the `HELLO_ACK`'s [`lamella_wire::ProfileIdentity`] hash
/// misses the host's manifest cache -- a KNOWN board costs no extra round-trip at all.
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
    transport.send(profile::GET_PROFILE, seq, &[])?;
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        while let Some(Frame { msg_type, seq: reply_seq, payload }) = transport.poll()? {
            if msg_type == profile::PROFILE_MANIFEST && reply_seq == seq {
                return ProfileManifest::decode(&payload).ok_or(TransportError::Closed);
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(TransportError::Closed)
}

/// Host driver: tell the target to boot its deployed image NOW ([`deploy::DEPLOY_RUN`]) -- a clean
/// self-reset into the boot-run path, so deploy->run needs no debug probe. Fire-and-forget: the
/// target resets and does not reply (do NOT then `HELLO`, which would interrupt the running app).
///
/// # Errors
/// A carrier [`TransportError`] if the command could not be sent.
#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
pub fn send_deploy_run(transport: &mut impl Transport, seq: u16) -> Result<(), TransportError> {
    transport.send(deploy::DEPLOY_RUN, seq, &[])
}

#[cfg(any(feature = "serial", feature = "usb", feature = "tcp"))]
fn await_result(
    transport: &mut impl Transport,
    seq: u16,
    timeout: Duration,
) -> Result<RunResult, TransportError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(result) = try_recv_result(transport, seq)? {
            return Ok(result);
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

        assert!(serve_one(&mut runner, &corlib).unwrap(), "the runner handled a RUN_PROGRAM");
        driver.feed(&runner.take_sent());

        let result = try_recv_result(&mut driver, 1).unwrap().expect("a result arrived");
        assert_eq!(result.exit, 7);
        assert_eq!(result.stdout, "hi\n");
    }

    /// THE HAZARD [`CHUNK_DATA_CAP`] EXISTS FOR.
    ///
    /// A `DEPLOY_CHUNK` payload carries 8 bytes of `(offset, total)` ahead of the image bytes, and
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
            "an over-long DEPLOY_CHUNK must be refused; truncating it produced a frame the target \
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
            transport.feed(&encode_frame(deploy::DEPLOY_RESULT, 3, &[1]).expect("a 1-byte ack frames"));
        }

        let ok = deploy_chunked_blocking(&mut transport, 3, &image, 65536, Duration::from_secs(5))
            .expect("the in-memory carrier never errors");
        assert!(ok, "every chunk acked");

        let mut got = vec![0u8; image.len()];
        let mut covered = vec![false; image.len()];
        let mut reader = FrameReader::new();
        reader.push(&transport.take_sent());
        while let Some(frame) = reader.next_frame() {
            if frame.msg_type != deploy::DEPLOY_CHUNK {
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
}
