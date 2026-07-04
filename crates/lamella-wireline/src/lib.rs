//! The HOST side of the wireline debug + REPL channel:

pub use lamella_runner::{
    RunResult, debug, deploy, repl, run_program, send_image, send_program, serve_one,
    try_recv_result,
};

pub mod engine;

#[cfg(feature = "debug-backend")]
pub mod debug_backend;

#[cfg(feature = "serial")]
use lamella_wire::{Frame, FrameReader, Transport, TransportError, encode_frame};
#[cfg(feature = "serial")]
use serialport::SerialPort;
#[cfg(feature = "serial")]
use std::io::{Read, Write};
#[cfg(feature = "serial")]
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

/// Host driver, blocking: HELLO the target and return the negotiated session (the chosen
/// version + the capability INTERSECTION). `host_caps` is what this host offers -- check
/// the result's caps to pick the PE ([`send_program`]) vs baked ([`send_image`]) path.
///
/// # Errors
/// [`TransportError::Closed`] on timeout or a version `NAK`; otherwise a carrier error.
#[cfg(feature = "serial")]
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
#[cfg(feature = "serial")]
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
#[cfg(feature = "serial")]
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
#[cfg(feature = "serial")]
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

#[cfg(feature = "serial")]
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
