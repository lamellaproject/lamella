//! The HOST side of the wireline debug + REPL channel:

pub use lamella_runner::{
    RunResult, baked_image_checksum, debug, deploy, repl, run_program, send_image, send_program,
    serve_one, try_recv_result,
};

pub mod engine;

#[cfg(feature = "debug-backend")]
pub mod debug_backend;

#[cfg(any(feature = "serial", feature = "usb"))]
use lamella_wire::{Frame, FrameReader, Transport, TransportError, encode_frame};
#[cfg(feature = "serial")]
use serialport::SerialPort;
#[cfg(feature = "serial")]
use std::io::{Read, Write};
#[cfg(any(feature = "serial", feature = "usb"))]
use std::time::{Duration, Instant};

/// A [`Transport`] over a serial carrier (USB-CDC or UART). Frames are byte-framed via lamella-wire's
/// [`encode_frame`] / [`FrameReader`]; `poll` is non-blocking (a short read timeout).
#[cfg(feature = "serial")]
pub struct SerialTransport {
    port: Box<dyn SerialPort>,
    reader: FrameReader,
    baud: u32,
}

#[cfg(feature = "serial")]
impl SerialTransport {
    /// Open the serial port at `path` (e.g. `"COM5"` / `"/dev/ttyACM0"`) at `baud`. The baud is moot
    /// for native USB-CDC but honored for a real UART.
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the port cannot be opened.
    pub fn open(path: &str, baud: u32) -> Result<Self, TransportError> {
        let mut port = serialport::new(path, baud)
            .timeout(Duration::from_millis(50))
            .open()
            .map_err(|_| TransportError::Carrier)?;
        port.write_data_terminal_ready(true).map_err(|_| TransportError::Carrier)?;
        port.write_request_to_send(true).ok();
        std::thread::sleep(Duration::from_millis(300));
        Ok(Self { port, reader: FrameReader::new(), baud })
    }
}

#[cfg(feature = "serial")]
impl Transport for SerialTransport {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        let frame = encode_frame(msg_type, seq, payload);
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
        let mut buf = [0u8; 512];
        match self.port.read(&mut buf) {
            Ok(0) => {}
            Ok(n) => self.reader.push(&buf[..n]),
            Err(ref error) if error.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => return Err(TransportError::Carrier),
        }
        Ok(self.reader.next_frame())
    }
}

/// A [`Transport`] over the native USB (driverless WinUSB) wireline carrier -- the device's vendor
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
    /// Open the Lamella wireline vendor interface -- its VID/PID + WinUSB interface GUID from
    /// [`lamella_wire::usb`].
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if no matching device is present or it cannot be opened.
    pub fn open() -> Result<Self, TransportError> {
        Self::open_ids(lamella_wire::usb::VID, lamella_wire::usb::PID)
    }

    /// Open a specific `vid`/`pid` on the wireline interface GUID (a board built with its own id pair).
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if no matching device is present or it cannot be opened.
    pub fn open_ids(vid: u16, pid: u16) -> Result<Self, TransportError> {
        Self::open_matching(vid, pid, None)
    }

    /// Open the wireline board matching `vid`/`pid` AND -- when several boards share the id
    /// pair -- a case-insensitive substring of its USB serial number (the F427 reports
    /// `F427-0001`; an RP2350 reports its 16-hex-digit chip id). `None` opens the first match.
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

    /// List the attached wireline boards (any VID/PID under the wireline interface GUID), with
    /// product + serial strings where the OS reports them -- the picker's data source.
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the platform cannot enumerate by interface GUID.
    pub fn list() -> Result<Vec<lamella_usbbulk::DeviceInfo>, TransportError> {
        lamella_usbbulk::enumerate_interface(lamella_wire::usb::WINUSB_INTERFACE_GUID)
            .map_err(|_| TransportError::Carrier)
    }
}

/// Parse an example's `usb` target argument into (vid, pid, serial-substring):
/// - `usb` -- the Lamella wireline VID/PID, first attached board;
/// - `usb:<vid>:<pid>` -- hex id pair (each exactly 4 hex digits), e.g. `usb:1209:0001`;
/// - `usb:<vid>:<pid>:<serial>` -- id pair plus a serial-substring pick;
/// - `usb:<serial>` -- the wireline VID/PID, board picked by a case-insensitive serial
///   substring (chip-id serials are hex, so a single token is ALWAYS a serial -- overriding
///   the ids requires the full pair form).
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
        let frame = encode_frame(msg_type, seq, payload);
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

/// Host driver, blocking: HELLO the target and return the negotiated session (the chosen
/// version + the capability INTERSECTION). `host_caps` is what this host offers -- check
/// the result's caps to pick the PE ([`send_program`]) vs baked ([`send_image`]) path.
///
/// # Errors
/// [`TransportError::Closed`] on timeout or a version `NAK`; otherwise a carrier error.
#[cfg(any(feature = "serial", feature = "usb"))]
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
#[cfg(any(feature = "serial", feature = "usb"))]
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
#[cfg(any(feature = "serial", feature = "usb"))]
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
#[cfg(any(feature = "serial", feature = "usb"))]
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

/// Deploy a baked image to flash in CHUNKS, so an image larger than one 64 KB wire frame can cross
/// the wire (the frame LEN is a `u16`, so a single [`deploy_blocking`] silently truncates a
/// corlib-baked image). Sends `DEPLOY_CHUNK(offset, total, bytes)` frames in ascending order,
/// waiting for each `DEPLOY_RESULT` ack before the next; returns whether every chunk verified.
/// `chunk_len` must be a multiple of the target's flash page (512 B) so each chunk starts on a page
/// boundary, and leave frame-header room under 64 KB. A single-chunk deploy (image `<= chunk_len`)
/// is equivalent to a `DEPLOY_IMAGE`, without the truncation risk.
///
/// # Errors
/// Propagates a [`TransportError`], or reports the wire closed if a chunk goes unacked past `timeout`.
pub fn deploy_chunked_blocking(
    transport: &mut impl Transport,
    seq: u16,
    image: &[u8],
    chunk_len: usize,
    timeout: Duration,
) -> Result<bool, TransportError> {
    use lamella_wire::Frame;
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
pub fn send_deploy_run(transport: &mut impl Transport, seq: u16) -> Result<(), TransportError> {
    transport.send(deploy::DEPLOY_RUN, seq, &[])
}

#[cfg(any(feature = "serial", feature = "usb"))]
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
    fn bare_usb_is_the_wireline_ids_first_board() {
        assert_eq!(parse_usb_target("usb"), (0x1209, 0x0001, None));
    }

    #[test]
    fn id_pair_and_optional_serial() {
        assert_eq!(parse_usb_target("usb:1209:0001"), (0x1209, 0x0001, None));
        assert_eq!(
            parse_usb_target("usb:1209:0001:E463"),
            (0x1209, 0x0001, Some("E463".to_string()))
        );
    }

    #[test]
    fn lone_token_is_always_a_serial() {
        assert_eq!(parse_usb_target("usb:F427-0001"), (0x1209, 0x0001, Some("F427-0001".into())));
        assert_eq!(parse_usb_target("usb:E46341"), (0x1209, 0x0001, Some("E46341".into())));
        assert_eq!(parse_usb_target("usb:7B53"), (0x1209, 0x0001, Some("7B53".into())));
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
}
