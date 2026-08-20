//! The relay daemon's options and session policy, with the carriers kept out of it.

use std::time::Duration;

use lamella_wire::Transport;
use lamella_wire::relay::{Fault, pump};

/// How long to sleep when a pump moved nothing, so an idle relay does not spin a companion
/// processor's core at 100%.
///
/// Short enough that it is not felt between a keystroke and a board's answer, long enough that an
/// idle daemon is invisible in `top`. The relay is latency-sensitive in one direction only -- a
/// human waiting on a device's reply -- and 2 ms is already below what a serial line costs.
pub const IDLE_SLEEP: Duration = Duration::from_millis(2);

/// Where the daemon listens and what it relays to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
    /// The socket address to listen on.
    pub listen: String,
    /// The serial port the microcontroller is on, as this processor names it.
    pub device: String,
    /// The device line's bit rate.
    pub baud: u32,
}

/// Why a command line was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptionsError {
    /// `--device` was not given, and there is no sensible default for it.
    MissingDevice,
    /// A flag that takes a value was given without one.
    MissingValue(String),
    /// A `--baud` that is not a number.
    BadBaud(String),
    /// An argument this daemon does not accept.
    Unknown(String),
}

impl core::fmt::Display for OptionsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::MissingDevice => write!(
                f,
                "--device is required: name the serial port the microcontroller is on (this \
                 processor's name for it, such as /dev/ttyS0)"
            ),
            Self::MissingValue(flag) => write!(f, "{flag} needs a value"),
            Self::BadBaud(value) => write!(f, "--baud {value:?} is not a number"),
            Self::Unknown(arg) => write!(f, "unknown argument {arg:?}"),
        }
    }
}

/// The usage line, so the binary and its error paths do not spell it twice.
pub const USAGE: &str = "usage: lamella-linkd --device <serial-port> [--baud <rate>] [--listen <addr>]";

impl Options {
    /// Parse a command line, without the program name.
    ///
    /// There is no default for `--device` on purpose. Every other value here has an obviously
    /// right default, and the port does not: guessing one means a daemon that starts, listens, and
    /// relays a host onto whatever happened to be at that path.
    ///
    /// # Errors
    /// An [`OptionsError`] naming what was wrong, so the caller can print it beside [`USAGE`].
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Self, OptionsError> {
        let mut listen = format!("0.0.0.0:{}", lamella_wire::tcp::DEFAULT_PORT);
        let mut device = None;
        let mut baud = 115_200u32;

        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--device" => {
                    device = Some(args.next().ok_or_else(|| OptionsError::MissingValue(arg))?);
                }
                "--listen" => {
                    listen = args.next().ok_or_else(|| OptionsError::MissingValue(arg))?;
                }
                "--baud" => {
                    let value = args.next().ok_or_else(|| OptionsError::MissingValue(arg))?;
                    baud = value.parse().map_err(|_| OptionsError::BadBaud(value))?;
                }
                _ => return Err(OptionsError::Unknown(arg)),
            }
        }

        Ok(Self { listen, device: device.ok_or(OptionsError::MissingDevice)?, baud })
    }
}

/// Relay between one host and the device until a carrier fails, and report which one did.
///
/// `still_wanted` is consulted between pumps and is how a caller stops a session for a reason that
/// is not a fault -- a signal, or a second host waiting. Returning `false` ends the session with
/// `None`.
///
/// The two faults want opposite responses, and that is why [`Fault`] names a side: a host that
/// hung up means release the line for the next one, and a device that stopped answering means this
/// board needs attention. **The daemon reports and does neither**, because resetting a target is a
/// programming mechanism it does not have and must not grow.
pub fn relay_session(
    host: &mut impl Transport,
    device: &mut impl Transport,
    mut still_wanted: impl FnMut() -> bool,
    mut idle: impl FnMut(),
) -> Option<Fault> {
    loop {
        match pump(host, device) {
            Ok(moved) => {
                if !still_wanted() {
                    return None;
                }
                if moved.idle() {
                    idle();
                }
            }
            Err(fault) => return Some(fault),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_wire::{MemTransport, encode_frame};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn a_device_is_required_and_the_rest_default() {
        let parsed = Options::parse(args(&["--device", "/dev/ttyS0"])).expect("a device is enough");
        assert_eq!(parsed.device, "/dev/ttyS0");
        assert_eq!(parsed.baud, 115_200);
        assert_eq!(
            parsed.listen,
            format!("0.0.0.0:{}", lamella_wire::tcp::DEFAULT_PORT),
            "the default port is the protocol's, not a number this crate invented"
        );
        assert_eq!(Options::parse(args(&[])), Err(OptionsError::MissingDevice));
    }

    #[test]
    fn every_flag_is_taken_and_a_bad_one_is_refused_rather_than_ignored() {
        let parsed = Options::parse(args(&[
            "--listen", "127.0.0.1:9000", "--device", "COM7", "--baud", "921600",
        ]))
        .expect("all three parse");
        assert_eq!(parsed, Options { listen: "127.0.0.1:9000".into(), device: "COM7".into(), baud: 921_600 });

        assert_eq!(
            Options::parse(args(&["--device", "x", "--baurd", "9600"])),
            Err(OptionsError::Unknown("--baurd".into()))
        );
        assert_eq!(
            Options::parse(args(&["--device", "x", "--baud", "fast"])),
            Err(OptionsError::BadBaud("fast".into()))
        );
        assert_eq!(
            Options::parse(args(&["--device"])),
            Err(OptionsError::MissingValue("--device".into()))
        );
    }

    /// The property the whole design rests on: the daemon puts nothing of its own on either wire.
    ///
    /// Not a refusal, not a status, not an acknowledgement. What leaves the device side is exactly
    /// what arrived from the host, byte for byte, and nothing else -- so a host cannot tell a relayed
    /// session from a direct one, which is what makes the relay invisible to the protocol.
    ///
    #[test]
    fn relayed_only() {
        let inbound = encode_frame(0x30, 7, b"a payload the daemon must not touch").expect("frames");
        let mut host = MemTransport::new();
        let mut device = MemTransport::new();
        host.feed(&inbound);

        let mut rounds = 0;
        let ended = relay_session(&mut host, &mut device, || { rounds += 1; rounds < 1 }, || {});
        assert!(ended.is_none(), "a session stopped by its caller is not a fault");

        assert_eq!(
            device.take_sent(),
            inbound,
            "the device side must carry the host's frame byte for byte and NOTHING ELSE"
        );
        assert!(
            host.take_sent().is_empty(),
            "the daemon spoke to the host on its own behalf, which makes it a protocol participant"
        );
    }
}
