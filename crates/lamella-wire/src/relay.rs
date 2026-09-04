//! Passing frames between two carriers, for a target a host cannot reach directly. It forwards
//! without interpreting and without reframing, so a message type it has never heard of crosses.

use crate::{Frame, Transport, TransportError};

/// Which carrier a fault came from.
///
/// The whole reason a relay reports this rather than a bare error: one side failing is a target
/// that has stopped answering and the other is a host that has gone away, and the remedies are not
/// the same one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Side {
    /// The carrier the host is on.
    Host,
    /// The carrier the target is on.
    Device,
}

/// A carrier failed, and which one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fault {
    /// The carrier that failed.
    pub side: Side,
    /// What it reported.
    pub error: TransportError,
}

/// What one pump moved.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pumped {
    /// Frames carried from the host to the target.
    pub to_device: usize,
    /// Frames carried from the target to the host.
    pub to_host: usize,
}

impl Pumped {
    /// Whether anything moved. A caller with nothing else to do can idle on `false`.
    #[must_use]
    pub fn idle(self) -> bool {
        self.to_device == 0 && self.to_host == 0
    }
}

/// Frames moved per direction in one pump before the other direction is served.
///
/// A relay with an unbounded loop in one direction is a relay a chatty side can hold: a target
/// streaming console output would keep the pump inside its own arm and the host's next command
/// would wait for the target to fall silent. Alternating in bounded rounds costs nothing and
/// removes the possibility.
const FRAMES_PER_ROUND: usize = 8;

/// Carry frames between the host's carrier and the target's, in both directions, until neither has
/// anything left or the round budget is spent.
///
/// Nothing is interpreted and nothing is renumbered: a frame goes across with the same message
/// type, sequence number and payload it arrived with.
///
/// # Errors
/// A [`Fault`] naming the carrier that failed. A fault on one side leaves the other untouched --
/// the caller decides whether that means resetting a target or releasing it, and the relay does not
/// have the standing to choose.
pub fn pump(host: &mut impl Transport, device: &mut impl Transport) -> Result<Pumped, Fault> {
    let mut moved = Pumped::default();
    for _ in 0..FRAMES_PER_ROUND {
        let mut progressed = false;

        match host.poll() {
            Ok(Some(frame)) => {
                forward(device, &frame, Side::Device)?;
                moved.to_device += 1;
                progressed = true;
            }
            Ok(None) => {}
            Err(error) => return Err(Fault { side: Side::Host, error }),
        }

        match device.poll() {
            Ok(Some(frame)) => {
                forward(host, &frame, Side::Host)?;
                moved.to_host += 1;
                progressed = true;
            }
            Ok(None) => {}
            Err(error) => return Err(Fault { side: Side::Device, error }),
        }

        if !progressed {
            break;
        }
    }
    Ok(moved)
}

/// Put one frame on a carrier exactly as it arrived.
fn forward(carrier: &mut impl Transport, frame: &Frame, side: Side) -> Result<(), Fault> {
    carrier
        .send(frame.msg_type, frame.seq, &frame.payload)
        .map_err(|error| Fault { side, error })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MemTransport, encode_frame, msg};
    use alloc::vec;
    use alloc::vec::Vec;

    /// A pair of in-memory carriers standing in for the two sides, with the wire between them
    /// readable so a test can assert on BYTES rather than on frames.
    fn frames(cases: &[(u8, u16, Vec<u8>)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (msg_type, seq, payload) in cases {
            bytes.extend_from_slice(&encode_frame(*msg_type, *seq, payload).expect("frames"));
        }
        bytes
    }

    fn table() -> Vec<(u8, u16, Vec<u8>)> {
        vec![
            (msg::HELLO, 0, Vec::new()),
            (msg::PING, 1, b"ping".to_vec()),
            (0xEE, 0x8000, b"a type from a later protocol".to_vec()),
            (0x42, 0xFFFF, b"\x4C\x57 magic in the payload".to_vec()),
            (msg::PONG, 0x1234, (0..=255u8).cycle().take(4000).collect()),
        ]
    }

    /// THE ORACLE, and it is on the far side rather than in the middle. What leaves the relay must
    /// be byte-for-byte what the codec would have produced for the frames that entered it.
    ///
    /// Asserting that a frame "arrived" would pass for a relay that renumbered every sequence, and
    /// a renumbering relay works perfectly until a host matches a reply to its request.
    #[test]
    fn a_forwarded_frame_leaves_byte_for_byte_as_it_arrived() {
        let cases = table();
        let mut host = MemTransport::new();
        let mut device = MemTransport::new();
        host.feed(&frames(&cases));

        let moved = pump(&mut host, &mut device).expect("neither carrier fails");
        assert_eq!(moved.to_device, cases.len(), "every frame crossed");
        assert_eq!(
            device.take_sent(),
            frames(&cases),
            "the relayed bytes must equal the codec's, not merely decode to the same frames"
        );
    }

    /// The same in the other direction, because a relay is two paths and testing one says nothing
    /// about the other -- they are separate code even when they look symmetric.
    #[test]
    fn the_return_path_is_byte_for_byte_too() {
        let cases = table();
        let mut host = MemTransport::new();
        let mut device = MemTransport::new();
        device.feed(&frames(&cases));

        let moved = pump(&mut host, &mut device).expect("neither carrier fails");
        assert_eq!(moved.to_host, cases.len());
        assert_eq!(host.take_sent(), frames(&cases));
    }

    /// ORDER SURVIVES THE HOP. Frames leave in the order they arrived, which is the property a
    /// relay is most able to break and least likely to be caught breaking: every frame is present
    /// and correct, and only their sequence is wrong.
    #[test]
    fn frames_leave_in_the_order_they_arrived() {
        let cases: Vec<_> = (0..20u16).map(|n| (0x50, n, vec![n as u8; 8])).collect();
        let mut host = MemTransport::new();
        let mut device = MemTransport::new();
        host.feed(&frames(&cases));

        let mut total = 0;
        for _ in 0..10 {
            total += pump(&mut host, &mut device).expect("no fault").to_device;
        }
        assert_eq!(total, cases.len());
        assert_eq!(device.take_sent(), frames(&cases));
    }

    /// Both directions at once, which is the ordinary state of a debug session: a command going
    /// down while console output comes up.
    #[test]
    fn both_directions_move_in_one_pump() {
        let down = vec![(msg::PING, 1, b"down".to_vec())];
        let up = vec![(msg::PONG, 1, b"up".to_vec())];
        let mut host = MemTransport::new();
        let mut device = MemTransport::new();
        host.feed(&frames(&down));
        device.feed(&frames(&up));

        let moved = pump(&mut host, &mut device).expect("no fault");
        assert_eq!((moved.to_device, moved.to_host), (1, 1));
        assert_eq!(device.take_sent(), frames(&down));
        assert_eq!(host.take_sent(), frames(&up));
    }

    /// A quiet relay reports that it is quiet, so a caller can idle instead of spinning. On a
    /// companion processor that is the difference between a core at rest and one at full power.
    #[test]
    fn a_pump_with_nothing_to_carry_says_so() {
        let mut host = MemTransport::new();
        let mut device = MemTransport::new();
        let moved = pump(&mut host, &mut device).expect("no fault");
        assert!(moved.idle());
    }

    /// A FAULT NAMES ITS SIDE. "The target stopped answering" and "the host hung up" want opposite
    /// responses, and a relay that reported only that something failed would leave the reader to
    /// guess which -- from a log written by the one component that knew.
    #[test]
    fn a_fault_says_which_carrier_failed() {
        struct Broken;
        impl Transport for Broken {
            fn send(&mut self, _t: u8, _s: u16, _p: &[u8]) -> Result<(), TransportError> {
                Err(TransportError::Closed)
            }
            fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
                Err(TransportError::Closed)
            }
        }

        let mut host = MemTransport::new();
        host.feed(&frames(&[(msg::PING, 1, b"x".to_vec())]));
        let fault = pump(&mut host, &mut Broken).expect_err("the device carrier is broken");
        assert_eq!(fault.side, Side::Device, "a send failure belongs to the side it was sent on");

        let mut quiet = MemTransport::new();
        let fault = pump(&mut quiet, &mut Broken).expect_err("still broken");
        assert_eq!(fault.side, Side::Device);

        let mut device = MemTransport::new();
        let fault = pump(&mut Broken, &mut device).expect_err("the host carrier is broken");
        assert_eq!(fault.side, Side::Host);
    }

    /// One side with plenty to say cannot hold the pump: a round is bounded, so the caller gets
    /// control back and the other direction is served.
    #[test]
    fn a_talkative_side_cannot_hold_the_pump() {
        let flood: Vec<_> = (0..500u16).map(|n| (0x60, n, vec![0xAB; 4])).collect();
        let mut host = MemTransport::new();
        let mut device = MemTransport::new();
        device.feed(&frames(&flood));

        let moved = pump(&mut host, &mut device).expect("no fault");
        assert!(
            moved.to_host <= FRAMES_PER_ROUND,
            "one pump carried {} frames, so a streaming target can keep the caller inside it",
            moved.to_host
        );
    }
}
