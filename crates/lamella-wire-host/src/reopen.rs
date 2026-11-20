//! Reaching a board that leaves its USB under a request, the one way every host reaches one: the
//! `lamella` command and the MCP server.
//!
//! A board whose firmware runs Python takes back the program it runs by resetting, since the Python
//! runtime cannot stop a run from inside it, so its own USB leaves the bus under the HELLO and the
//! read fails. The board comes back serving: it is opened again, once, and asked again. And a board a
//! command just reset (a deploy that starts its program does) is off its USB for a moment, so the
//! first open of a board on its own USB waits for it.

use std::time::{Duration, Instant};

use lamella_wire::{Capabilities, Negotiated, Transport, TransportError};

/// How long the first open of a board on its own USB waits for it: a command just before may have
/// reset it, and it is off the bus until it enumerates again.
pub const COMING_BACK: Duration = Duration::from_secs(3);

/// How long a board that reset under a HELLO is given to come back on its USB.
pub const BACK_ON_THE_BUS: Duration = Duration::from_secs(10);

/// Whether a board at `target` leaves the bus when it resets: one reached over its own USB.
#[must_use]
pub fn reopens_after_a_reset(target: &str) -> bool {
    crate::classify_target(target) == crate::TargetKind::Usb
}

/// `target` opened by `opening`, after `first` and then every quarter second until it opens or
/// `limit` has passed, when the last refusal is the answer.
///
/// # Errors
/// `opening`'s last refusal.
pub fn opened_within<T>(
    target: &str,
    opening: &dyn Fn(&str) -> Result<T, String>,
    first: Duration,
    limit: Duration,
) -> Result<T, String> {
    std::thread::sleep(first);
    let until = Instant::now() + limit;
    loop {
        match opening(target) {
            Ok(opened) => return Ok(opened),
            Err(refusal) if Instant::now() >= until => return Err(refusal),
            Err(_) => std::thread::sleep(Duration::from_millis(250)),
        }
    }
}

/// `target` opened by `opening`: at once, or, for a board on its own USB that is not there, within
/// [`COMING_BACK`].
///
/// # Errors
/// `opening`'s last refusal.
pub fn opened<T>(target: &str, opening: &dyn Fn(&str) -> Result<T, String>) -> Result<T, String> {
    match opening(target) {
        Ok(transport) => Ok(transport),
        Err(_) if reopens_after_a_reset(target) => opened_within(target, opening, Duration::ZERO, COMING_BACK),
        Err(refusal) => Err(refusal),
    }
}

/// A HELLO offering `caps` at `seq` to the board `target` names, over `transport`, which `opening`
/// opened, waiting `timeout` for the answer; answers the carrier the answer came over, and the
/// answer.
///
/// A board on its own USB whose carrier fails under the HELLO is opened again, once, and asked again:
/// a board that runs Python resets to take back its program. The handle it left is let go first, so
/// a system that lends a device to one handle at a time lends it to the new one.
///
/// # Errors
/// The board did not come back within [`BACK_ON_THE_BUS`]: `opening`'s last refusal.
pub fn hello_reopening<T: Transport>(
    transport: T,
    target: &str,
    seq: u16,
    caps: Capabilities,
    timeout: Duration,
    opening: &dyn Fn(&str) -> Result<T, String>,
) -> Result<(T, Result<Negotiated, TransportError>), String> {
    let mut transport = transport;
    match crate::hello_blocking(&mut transport, seq, caps, timeout) {
        Err(TransportError::Carrier) if reopens_after_a_reset(target) => {
            drop(transport);
            let mut again = opened_within(target, opening, Duration::from_millis(500), BACK_ON_THE_BUS)?;
            let hello = crate::hello_blocking(&mut again, seq, caps, timeout);
            Ok((again, hello))
        }
        hello => Ok((transport, hello)),
    }
}

/// The board `target` names, [`opened`] by `opening`, and its answer to a HELLO offering `caps` at
/// `seq` within `timeout` ([`hello_reopening`]): the carrier the answer came over, and the answer.
///
/// # Errors
/// The board could not be opened: `opening`'s last refusal.
pub fn opened_and_asked<T: Transport>(
    target: &str,
    seq: u16,
    caps: Capabilities,
    timeout: Duration,
    opening: &dyn Fn(&str) -> Result<T, String>,
) -> Result<(T, Result<Negotiated, TransportError>), String> {
    hello_reopening(opened(target, opening)?, target, seq, caps, timeout, opening)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A BOARD COMING BACK ON THE BUS IS WAITED FOR, AND ONE THAT IS NOT IS REFUSED IN TIME**: the open
    /// is tried until it works or the limit passes, and the last refusal is the answer.
    #[test]
    fn a_board_coming_back_on_the_bus_is_waited_for_within_a_limit() {
        let tries = std::cell::Cell::new(0);
        let opening = |_: &str| {
            tries.set(tries.get() + 1);
            if tries.get() < 3 { Err(format!("not yet ({})", tries.get())) } else { Ok(7_u32) }
        };
        let limit = Duration::from_secs(2);
        assert_eq!(opened_within("usb", &opening, Duration::ZERO, limit), Ok(7));
        assert_eq!(tries.get(), 3);
        let never = |_: &str| -> Result<u32, String> { Err("gone".to_owned()) };
        let began = Instant::now();
        assert_eq!(opened_within("usb", &never, Duration::ZERO, Duration::from_millis(600)), Err("gone".to_owned()));
        assert!(began.elapsed() < Duration::from_secs(2), "it waited {:?}", began.elapsed());
    }

    /// **ONLY A BOARD ON ITS OWN USB IS WAITED FOR OR OPENED AGAIN**: a serial port or a network board
    /// that is not there is refused at once, since neither leaves its carrier when the board resets.
    #[test]
    fn only_a_board_on_its_own_usb_is_waited_for() {
        assert!(reopens_after_a_reset("usb:39e9:0001:D4C3"));
        assert!(!reopens_after_a_reset("COM8"));
        let gone = |_: &str| -> Result<u32, String> { Err("gone".to_owned()) };
        let began = Instant::now();
        assert_eq!(opened("COM8", &gone), Err("gone".to_owned()));
        assert!(began.elapsed() < Duration::from_millis(500), "it waited {:?}", began.elapsed());
    }
}
