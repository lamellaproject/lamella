//! The HOST side of the wireline debug + REPL channel:

pub use lamella_runner::{RunResult, repl, run_program, send_program, serve_one, try_recv_result};

pub mod engine;
#[cfg(feature = "repl-host")]
pub mod compile;

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
}

#[cfg(feature = "serial")]
impl SerialTransport {
    /// Open the serial port at `path` (e.g. `"COM5"` / `"/dev/ttyACM0"`) at `baud`. The baud is moot
    /// for native USB-CDC but honored for a real UART.
    ///
    /// # Errors
    /// [`TransportError::Carrier`] if the port cannot be opened.
    pub fn open(path: &str, baud: u32) -> Result<Self, TransportError> {
        let port = serialport::new(path, baud)
            .timeout(Duration::from_millis(50))
            .open()
            .map_err(|_| TransportError::Carrier)?;
        Ok(Self { port, reader: FrameReader::new() })
    }
}

#[cfg(feature = "serial")]
impl Transport for SerialTransport {
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        let frame = encode_frame(msg_type, seq, payload);
        self.port.write_all(&frame).map_err(|_| TransportError::Carrier)?;
        self.port.flush().map_err(|_| TransportError::Carrier)?;
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
