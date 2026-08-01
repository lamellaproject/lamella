//! The native driver for [`lamella_esp_serial`]: a real serial port, and the loop that performs what
//! the protocol asks for.

#![allow(unsafe_code)]

use std::time::Duration;

use lamella_esp_serial::{Action, Session, Step};

#[cfg(windows)]
pub mod windows;

#[cfg(windows)]
pub use windows::WindowsPort;

/// What went wrong with the port itself, as distinct from anything the protocol reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError {
    /// The port could not be opened.
    Open {
        /// The port name as given.
        name: String,
        /// The operating system's error code.
        code: u32,
    },
    /// The port opened but could not be configured.
    Configure {
        /// Which setting.
        what: &'static str,
        /// The operating system's error code.
        code: u32,
    },
    /// A control signal could not be changed.
    Signal {
        /// Which signal, and in which direction.
        what: &'static str,
        /// The operating system's error code.
        code: u32,
    },
    /// A write failed.
    Write {
        /// The operating system's error code.
        code: u32,
    },
    /// **A bounded write moved nothing**, which is a transmitter that never became ready. Distinct
    /// from [`PortError::Write`] because the remedy differs: this is flow control or a dead cable, not
    /// a bad handle.
    WriteStalled {
        /// How many bytes had gone out before it stalled.
        after: usize,
    },
    /// A read failed.
    Read {
        /// The operating system's error code.
        code: u32,
    },
}

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortError::Open { name, code } => write!(f, "cannot open {name} (error {code})"),
            PortError::Configure { what, code } => write!(f, "cannot set {what} (error {code})"),
            PortError::Signal { what, code } => write!(f, "cannot {what} (error {code})"),
            PortError::Write { code } => write!(f, "write failed (error {code})"),
            PortError::WriteStalled { after } => {
                write!(f, "write stalled after {after} bytes -- the transmitter never became ready")
            }
            PortError::Read { code } => write!(f, "read failed (error {code})"),
        }
    }
}

impl std::error::Error for PortError {}

/// A serial port, as this driver needs it.
///
/// The two signal setters are separate because no operating system can change both together -- the
/// part's own manual says so, and the protocol crate's action enum keeps them separate for the same
/// reason. A combined method here would let a caller believe otherwise.
pub trait Port {
    /// Writes every byte, or reports why not.
    ///
    /// # Errors
    /// [`PortError`] describing the transport failure.
    fn write(&mut self, bytes: &[u8]) -> Result<(), PortError>;

    /// Reads whatever has arrived within `timeout_ms`, which may be nothing.
    ///
    /// **Returning zero is a normal outcome, not an error.** A target that is still booting, or that
    /// never entered download mode, answers nothing -- and the protocol session has a distinct call
    /// for exactly that, because what silence MEANS depends on which command is outstanding.
    ///
    /// # Errors
    /// [`PortError`] describing the transport failure.
    fn read(&mut self, buffer: &mut [u8], timeout_ms: u32) -> Result<usize, PortError>;

    /// Sets data-terminal-ready.
    ///
    /// # Errors
    /// [`PortError`] describing the transport failure.
    fn set_dtr(&mut self, on: bool) -> Result<(), PortError>;

    /// Sets request-to-send.
    ///
    /// # Errors
    /// [`PortError`] describing the transport failure.
    fn set_rts(&mut self, on: bool) -> Result<(), PortError>;

    /// Closes and reopens at `baud`.
    ///
    /// # Errors
    /// [`PortError`] describing the transport failure.
    fn reopen(&mut self, baud: u32) -> Result<(), PortError>;

    /// Discards anything buffered in either direction.
    ///
    /// # Errors
    /// [`PortError`] describing the transport failure.
    fn discard_buffers(&mut self) -> Result<(), PortError>;

    /// How to name this port in a report.
    fn describe(&self) -> String;
}

/// How a driven session ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The write completed and the target's own read-back of flash matched.
    Verified {
        /// The digest the target reported.
        device_digest: Vec<u8>,
        /// Which encoding it arrived in.
        encoding: lamella_esp_serial::session::DigestEncoding,
    },
    /// The protocol session stopped, with its reason.
    Refused(lamella_esp_serial::Error),
    /// The port itself failed.
    Transport(PortError),
}

/// How much of the line to take in one read. One block's worth, so a response that arrives in one
/// burst is taken in one call rather than in fragments the frame reader then has to rejoin.
const READ_CHUNK: usize = 4096;

/// Somewhere for the driver to narrate to, so a caller chooses whether the detail is shown.
///
/// A closure rather than a flag: the interesting case is not "verbose or not" but "show me the signal
/// transitions in order", which is the evidence a reset-sequence question is settled with.
pub type Trace<'a> = &'a mut dyn FnMut(&str);

/// Drives `session` to completion against `port`.
///
/// # Errors
/// Never -- every failure is a variant of [`Outcome`]. The signature returns the outcome directly
/// because a flash that was refused by the target and a flash whose cable fell out are both results
/// a caller reports rather than exceptions it propagates.
pub fn drive(session: &mut Session, port: &mut dyn Port, trace: Trace<'_>) -> Outcome {
    let mut buffer = [0u8; READ_CHUNK];
    loop {
        match session.poll() {
            Step::Do(action) => {
                if let Err(problem) = perform(&action, port, trace) {
                    return Outcome::Transport(problem);
                }
            }
            Step::Await { timeout_ms } => match port.read(&mut buffer, timeout_ms) {
                Ok(0) => {
                    trace(&format!("  <- nothing within {timeout_ms} ms"));
                    session.timeout();
                }
                Ok(got) => {
                    trace(&format!("  <- {got} bytes"));
                    session.feed(&buffer[..got]);
                }
                Err(problem) => return Outcome::Transport(problem),
            },
            Step::Done { device_digest, encoding } => {
                return Outcome::Verified { device_digest, encoding }
            }
            Step::Failed(why) => return Outcome::Refused(why),
        }
    }
}

/// Performs one emitted action.
fn perform(action: &Action, port: &mut dyn Port, trace: Trace<'_>) -> Result<(), PortError> {
    match action {
        Action::Write(bytes) => {
            trace(&format!("  -> {} bytes", bytes.len()));
            port.write(bytes)
        }
        Action::SetDtr(on) => {
            trace(&format!("  DTR {}", if *on { "assert" } else { "clear" }));
            port.set_dtr(*on)
        }
        Action::SetRts(on) => {
            trace(&format!("  RTS {}", if *on { "assert" } else { "clear" }));
            port.set_rts(*on)
        }
        Action::Delay(ms) => {
            trace(&format!("  wait {ms} ms"));
            std::thread::sleep(Duration::from_millis(u64::from(*ms)));
            Ok(())
        }
        Action::Reopen { baud } => {
            trace(&format!("  reopen at {baud} baud"));
            port.reopen(*baud)
        }
    }
}

#[cfg(test)]
mod tests {
    use lamella_esp_serial::{
        session::{Dialect, DigestEncoding},
        Command, Connector, FlashParams,
    };

    use super::*;

    /// A port that answers every command successfully, so the LOOP can be tested without silicon.
    ///
    /// It is not a simulated ESP32 and does not pretend to be: it decodes the opcode a request
    /// carries and replies with a success for that opcode. That is enough to exercise every arm of
    /// the loop -- writes, reads, signals, delays -- which is what this crate owns.
    #[derive(Default)]
    struct Obliging {
        /// Frames waiting to be read back.
        pending: Vec<u8>,
        /// The signal transitions performed, in order.
        signals: Vec<(char, bool)>,
        /// The digest to answer the verification command with.
        digest: Vec<u8>,
        /// How many reads returned nothing.
        empty_reads: usize,
    }

    impl Port for Obliging {
        fn write(&mut self, bytes: &[u8]) -> Result<(), PortError> {
            let body = lamella_esp_serial::unescape(&bytes[1..bytes.len() - 1]);
            let command = body[1];
            let payload: &[u8] =
                if command == Command::SpiFlashMd5 as u8 { &self.digest } else { &[] };
            let mut packet = vec![0x01, command];
            packet.extend_from_slice(
                &(u16::try_from(payload.len() + 4).expect("small")).to_le_bytes(),
            );
            packet.extend_from_slice(&0u32.to_le_bytes());
            packet.extend_from_slice(payload);
            packet.extend_from_slice(&[0u8; 4]);
            self.pending.extend_from_slice(&lamella_esp_serial::frame(&packet));
            Ok(())
        }

        fn read(&mut self, buffer: &mut [u8], _timeout_ms: u32) -> Result<usize, PortError> {
            if self.pending.is_empty() {
                self.empty_reads += 1;
                return Ok(0);
            }
            let take = self.pending.len().min(buffer.len());
            buffer[..take].copy_from_slice(&self.pending[..take]);
            self.pending.drain(..take);
            Ok(take)
        }

        fn set_dtr(&mut self, on: bool) -> Result<(), PortError> {
            self.signals.push(('D', on));
            Ok(())
        }

        fn set_rts(&mut self, on: bool) -> Result<(), PortError> {
            self.signals.push(('R', on));
            Ok(())
        }

        fn reopen(&mut self, _baud: u32) -> Result<(), PortError> {
            Ok(())
        }

        fn discard_buffers(&mut self) -> Result<(), PortError> {
            Ok(())
        }

        fn describe(&self) -> String {
            String::from("an obliging test port")
        }
    }

    fn session_for(image: &[u8]) -> Session {
        Session::write_flash(
            Connector::UartBridge,
            Dialect::ESP32C6,
            FlashParams::serial_nor(8 * 1024 * 1024),
            0x10_0000,
            image.to_vec(),
        )
    }

    /// The loop drives a session to a verified finish, and the digest comparison happens for real.
    #[test]
    fn the_loop_drives_a_write_to_a_verified_finish() {
        let image = vec![0xAB; 5000];
        let mut port =
            Obliging { digest: lamella_esp_serial::md5_hex(&image), ..Obliging::default() };
        let mut session = session_for(&image);
        let outcome = drive(&mut session, &mut port, &mut |_| {});
        assert_eq!(
            outcome,
            Outcome::Verified {
                device_digest: lamella_esp_serial::md5_hex(&image),
                encoding: DigestEncoding::AsciiHex,
            }
        );
    }

    /// **The trace records every signal transition in order, including the ones that change nothing.**
    /// That ordering is the evidence a reset-sequence question is settled with, so a trace that
    /// deduplicated it would destroy the measurement it exists to produce.
    #[test]
    fn the_trace_keeps_repeated_signal_writes_rather_than_collapsing_them() {
        let image = vec![0x01; 16];
        let mut port =
            Obliging { digest: lamella_esp_serial::md5_hex(&image), ..Obliging::default() };
        let mut session = session_for(&image);
        let mut lines: Vec<String> = Vec::new();
        drive(&mut session, &mut port, &mut |line| lines.push(line.to_string()));

        let signal_lines: Vec<&String> =
            lines.iter().filter(|l| l.contains("DTR") || l.contains("RTS")).collect();
        assert!(!signal_lines.is_empty(), "signal transitions are traced");
        assert_eq!(
            port.signals.len(),
            signal_lines.len(),
            "every signal write reaching the port is also traced -- no collapsing"
        );
    }

    /// **Silence reaches the session as a timeout, not as an empty feed.** A driver that fed an empty
    /// slice instead would leave a sync attempt uncounted, and a board that never enters download mode
    /// would spin forever rather than being reported.
    #[test]
    fn a_silent_port_is_reported_rather_than_spun_on() {
        /// A port that accepts writes and never answers.
        struct Mute {
            reads: usize,
        }
        impl Port for Mute {
            fn write(&mut self, _bytes: &[u8]) -> Result<(), PortError> {
                Ok(())
            }
            fn read(&mut self, _buffer: &mut [u8], _timeout_ms: u32) -> Result<usize, PortError> {
                self.reads += 1;
                assert!(self.reads < 1000, "the loop must not read forever");
                Ok(0)
            }
            fn set_dtr(&mut self, _on: bool) -> Result<(), PortError> {
                Ok(())
            }
            fn set_rts(&mut self, _on: bool) -> Result<(), PortError> {
                Ok(())
            }
            fn reopen(&mut self, _baud: u32) -> Result<(), PortError> {
                Ok(())
            }
            fn discard_buffers(&mut self) -> Result<(), PortError> {
                Ok(())
            }
            fn describe(&self) -> String {
                String::from("a mute test port")
            }
        }
        let mut port = Mute { reads: 0 };
        let mut session = session_for(&[0u8; 16]);
        let outcome = drive(&mut session, &mut port, &mut |_| {});
        assert_eq!(outcome, Outcome::Refused(lamella_esp_serial::Error::NoResponse));
        assert!(port.reads > 1, "it retried before giving up: {} reads", port.reads);
    }

    /// A transport failure is reported as a transport failure, not as a protocol refusal -- the two
    /// send a user to entirely different places.
    #[test]
    fn a_port_failure_is_not_reported_as_a_protocol_refusal() {
        /// A port whose first write fails.
        struct Broken;
        impl Port for Broken {
            fn write(&mut self, _bytes: &[u8]) -> Result<(), PortError> {
                Err(PortError::WriteStalled { after: 0 })
            }
            fn read(&mut self, _buffer: &mut [u8], _timeout_ms: u32) -> Result<usize, PortError> {
                Ok(0)
            }
            fn set_dtr(&mut self, _on: bool) -> Result<(), PortError> {
                Ok(())
            }
            fn set_rts(&mut self, _on: bool) -> Result<(), PortError> {
                Ok(())
            }
            fn reopen(&mut self, _baud: u32) -> Result<(), PortError> {
                Ok(())
            }
            fn discard_buffers(&mut self) -> Result<(), PortError> {
                Ok(())
            }
            fn describe(&self) -> String {
                String::from("a broken test port")
            }
        }
        let mut session = session_for(&[0u8; 16]);
        let outcome = drive(&mut session, &mut Broken, &mut |_| {});
        assert_eq!(outcome, Outcome::Transport(PortError::WriteStalled { after: 0 }));
    }
}
