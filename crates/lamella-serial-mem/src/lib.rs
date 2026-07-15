//! An in-memory loopback [`SerialBackend`]: a table of open ports, each with a receive queue
//! into which its OWN writes loop back (hardware UART loopback mode). It pins the seam's
//! SEMANTICS -- open/config, byte round-trip, `BytesToRead`, discard -- in unit tests that touch
//! no host COM port, and it is the serial story for embedders with no device at all: a browser
//! tab, a simulator, a test harness. `no_std` + `alloc`, like the interpreter core it plugs into.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use lamella_cil_runtime::serial::{
    SerialBackend, SerialConfig, SerialError, SerialHandle, SerialResult,
};

/// One open loopback port: the configuration it opened with and the bytes written-but-not-yet-read
/// (a write appends here; a read drains it -- TX loops straight to this port's own RX).
#[derive(Debug)]
struct Port {
    /// The frozen line configuration (kept for inspection; loopback does not interpret framing).
    config: SerialConfig,
    /// The receive queue: bytes this port has written to itself, awaiting a read.
    rx: VecDeque<u8>,
}

/// A deterministic in-memory serial backend. `Default` starts with no open ports.
#[derive(Debug, Default)]
pub struct MemSerial {
    /// The open-port table; a closed handle leaves a `None` hole (handles are never reused, so a
    /// stale handle reads back as [`SerialError::Io`] rather than aliasing a later port).
    ports: Vec<Option<Port>>,
}

impl MemSerial {
    /// An empty loopback backend (no ports open).
    #[must_use]
    pub fn new() -> MemSerial {
        MemSerial::default()
    }

    /// The open port for `handle`, or [`SerialError::Io`] if it is closed or was never opened.
    fn port(&mut self, handle: SerialHandle) -> SerialResult<&mut Port> {
        self.ports
            .get_mut(handle as usize)
            .and_then(|slot| slot.as_mut())
            .ok_or(SerialError::Io)
    }

    /// The configuration a port opened with -- test-only inspection of the frozen line settings.
    #[must_use]
    pub fn config_of(&self, handle: SerialHandle) -> Option<SerialConfig> {
        self.ports
            .get(handle as usize)
            .and_then(|slot| slot.as_ref())
            .map(|port| port.config)
    }
}

impl SerialBackend for MemSerial {
    fn open(&mut self, _port_name: &str, config: &SerialConfig) -> SerialResult<SerialHandle> {
        let handle = self.ports.len() as SerialHandle;
        self.ports.push(Some(Port {
            config: *config,
            rx: VecDeque::new(),
        }));
        Ok(handle)
    }

    fn read(
        &mut self,
        handle: SerialHandle,
        buf: &mut [u8],
        _timeout_ms: i32,
    ) -> SerialResult<usize> {
        let port = self.port(handle)?;
        let mut n = 0;
        while n < buf.len() {
            match port.rx.pop_front() {
                Some(byte) => {
                    buf[n] = byte;
                    n += 1;
                }
                None => break,
            }
        }
        Ok(n)
    }

    fn write(&mut self, handle: SerialHandle, buf: &[u8], _timeout_ms: i32) -> SerialResult<usize> {
        let port = self.port(handle)?;
        port.rx.extend(buf.iter().copied());
        Ok(buf.len())
    }

    fn bytes_to_read(&mut self, handle: SerialHandle) -> SerialResult<usize> {
        Ok(self.port(handle)?.rx.len())
    }

    fn bytes_to_write(&mut self, handle: SerialHandle) -> SerialResult<usize> {
        self.port(handle)?;
        Ok(0)
    }

    fn flush(&mut self, handle: SerialHandle) -> SerialResult<()> {
        self.port(handle)?;
        Ok(())
    }

    fn discard_in(&mut self, handle: SerialHandle) -> SerialResult<()> {
        self.port(handle)?.rx.clear();
        Ok(())
    }

    fn discard_out(&mut self, handle: SerialHandle) -> SerialResult<()> {
        self.port(handle)?;
        Ok(())
    }

    fn close(&mut self, handle: SerialHandle) {
        if let Some(slot) = self.ports.get_mut(handle as usize) {
            *slot = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_cil_runtime::serial::{Handshake, Parity, StopBits};

    fn config() -> SerialConfig {
        SerialConfig {
            baud_rate: 115_200,
            parity: Parity::None,
            data_bits: 8,
            stop_bits: StopBits::One,
            handshake: Handshake::None,
        }
    }

    #[test]
    fn loopback_round_trips_bytes() {
        let mut serial = MemSerial::new();
        let handle = serial.open("COM-LOOP", &config()).unwrap();
        assert_eq!(serial.bytes_to_read(handle).unwrap(), 0);

        let written = serial.write(handle, b"hi!", -1).unwrap();
        assert_eq!(written, 3);
        assert_eq!(serial.bytes_to_read(handle).unwrap(), 3);
        assert_eq!(serial.bytes_to_write(handle).unwrap(), 0);

        let mut buf = [0u8; 8];
        let read = serial.read(handle, &mut buf, -1).unwrap();
        assert_eq!(read, 3);
        assert_eq!(&buf[..3], b"hi!");
        assert_eq!(serial.bytes_to_read(handle).unwrap(), 0);
    }

    #[test]
    fn read_is_non_blocking_when_empty() {
        let mut serial = MemSerial::new();
        let handle = serial.open("COM1", &config()).unwrap();
        let mut buf = [0u8; 4];
        assert_eq!(serial.read(handle, &mut buf, 1000).unwrap(), 0);
    }

    #[test]
    fn partial_read_drains_in_order() {
        let mut serial = MemSerial::new();
        let handle = serial.open("COM1", &config()).unwrap();
        serial.write(handle, b"ABCDE", 0).unwrap();
        let mut two = [0u8; 2];
        assert_eq!(serial.read(handle, &mut two, 0).unwrap(), 2);
        assert_eq!(&two, b"AB");
        assert_eq!(serial.bytes_to_read(handle).unwrap(), 3);
        let mut rest = [0u8; 8];
        assert_eq!(serial.read(handle, &mut rest, 0).unwrap(), 3);
        assert_eq!(&rest[..3], b"CDE");
    }

    #[test]
    fn discard_in_clears_the_receive_queue() {
        let mut serial = MemSerial::new();
        let handle = serial.open("COM1", &config()).unwrap();
        serial.write(handle, b"junk", 0).unwrap();
        serial.discard_in(handle).unwrap();
        assert_eq!(serial.bytes_to_read(handle).unwrap(), 0);
    }

    #[test]
    fn config_is_preserved_and_a_closed_handle_is_io() {
        let mut serial = MemSerial::new();
        let handle = serial.open("COM7", &config()).unwrap();
        assert_eq!(serial.config_of(handle).unwrap().baud_rate, 115_200);
        serial.close(handle);
        assert_eq!(serial.bytes_to_read(handle), Err(SerialError::Io));
        assert!(serial.config_of(handle).is_none());
    }
}
