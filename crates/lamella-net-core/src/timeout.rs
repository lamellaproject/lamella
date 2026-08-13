//! How long a socket operation waits before the thread parked on it is woken anyway.

extern crate alloc;

use alloc::collections::BTreeMap;

use crate::{Interest, SocketHandle};

/// Per-socket operation timeouts, in milliseconds.
///
/// Empty means every socket waits indefinitely, which is the default every socket API starts from.
#[derive(Clone, Debug, Default)]
pub struct Timeouts {
    read: BTreeMap<SocketHandle, u32>,
    write: BTreeMap<SocketHandle, u32>,
}

impl Timeouts {
    /// No timeouts on anything.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set `socket`'s timeout for one direction. **Zero CLEARS it**, restoring the indefinite
    /// default.
    ///
    /// Zero-means-infinite rather than zero-means-immediate is the shape `SO_RCVTIMEO` has
    /// everywhere, which is what a runtime forwarding that option straight through gets for free.
    ///
    /// # A binding language may still have to translate, and Python has to twice
    ///
    /// This once claimed no caller would need to translate. That is false for the second consumer,
    /// which is the thing a second consumer is for. Measured against CPython 3.14.6:
    ///
    /// ```text
    /// after settimeout(0)    -> gettimeout: 0.0  | getblocking: False
    /// after setblocking(F)   -> gettimeout: 0.0  | getblocking: False
    /// after settimeout(None) -> gettimeout: None | getblocking: True
    /// ```
    ///
    /// `settimeout(0)` and `setblocking(False)` are the same call there. So zero means NEVER WAIT,
    /// this table reads it as WAIT FOREVER, and a value forwarded unchanged blocks indefinitely on
    /// the one socket whose owner asked never to block at all. That is not a missing feature; it is
    /// the opposite answer, silently.
    ///
    /// The second form is meaner because nobody writes it deliberately. This takes MILLISECONDS as
    /// a `u32` and CPython's timeout is a float in SECONDS, so `settimeout(0.0005)` truncates to
    /// zero and therefore becomes infinite: every value under half a millisecond inverts, and the
    /// caller asking for the shortest timeout it can express gets the longest one there is.
    ///
    /// The rule is not changing. It is right for the option C# forwards, and moving it would only
    /// put the translation on the other side. What is recorded here is that the translation exists,
    /// that it belongs to the binding language, and what it costs to skip.
    ///
    /// # What a deadline structurally cannot carry
    ///
    /// This table says WHEN a wait ends and can never say WHY, and those are different types a
    /// program branches on: non-blocking mode raises `BlockingIOError` (normal, retry -- the
    /// select-loop idiom) where a timeout raises `TimeoutError` (the peer is too slow). The mode has
    /// to be remembered above the seam whatever this table does.
    ///
    /// Non-blocking is also not a deadline of zero even where it could be encoded as one: every
    /// operation would enter the reactor block point and come straight back out, a round trip per
    /// operation inside the loop that chose non-blocking to avoid parking at all. It is a
    /// control-flow decision, not a deadline value.
    pub fn set(&mut self, socket: SocketHandle, interest: Interest, millis: u32) {
        let table = match interest {
            Interest::Read => &mut self.read,
            Interest::Write => &mut self.write,
        };
        if millis == 0 {
            table.remove(&socket);
        } else {
            table.insert(socket, millis);
        }
    }

    /// The timeout set for `socket` in one direction, if any.
    #[must_use]
    pub fn get(&self, socket: SocketHandle, interest: Interest) -> Option<u32> {
        match interest {
            Interest::Read => self.read.get(&socket).copied(),
            Interest::Write => self.write.get(&socket).copied(),
        }
    }

    /// Drop everything remembered about `socket`. **A closing socket must do this.**
    ///
    /// Two reasons, and the second is the one that bites. A table nothing removes from grows for as
    /// long as the program opens sockets, which on a device is a leak with no upper bound. And a
    /// backend is free to hand the same handle out again once it is closed -- so a timeout left
    /// behind by a socket that no longer exists would silently become the timeout of an unrelated
    /// one, which is a bug that only appears under enough churn to recycle a number.
    pub fn forget(&mut self, socket: SocketHandle) {
        self.read.remove(&socket);
        self.write.remove(&socket);
    }

    /// How many sockets have a timeout in either direction, so a caller can see the table is not
    /// growing without bound.
    #[must_use]
    pub fn tracked(&self) -> usize {
        self.read.len() + self.write.len()
    }

    /// When a thread parking on `socket` for `interest` should be woken anyway, given the current
    /// monotonic reading.
    ///
    /// `None` means park indefinitely: no timeout is set, so nothing should wake the thread but the
    /// socket becoming ready.
    ///
    /// # The clockless case, which is a degradation rather than a failure
    ///
    /// A board with no monotonic source cannot compute a deadline. Passing `now: None` for a socket
    /// that HAS a timeout yields `Some(0)` -- a deadline already past -- so the park becomes a
    /// yield and the caller's loop polls instead of blocking.
    ///
    /// That is deliberate, and the alternative is worse. Reporting no deadline would park the
    /// thread indefinitely on a socket whose owner explicitly asked not to wait indefinitely: the
    /// board would hang exactly where the program had taken the trouble to say it must not. Turning
    /// the timeout into a busy poll spends power and answers; ignoring it spends the program.
    #[must_use]
    pub fn deadline(
        &self,
        socket: SocketHandle,
        interest: Interest,
        now: Option<u64>,
    ) -> Option<u64> {
        let millis = self.get(socket, interest)?;
        Some(now.map_or(0, |now| now.saturating_add(u64::from(millis))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOCKET: SocketHandle = 7;

    #[test]
    fn nothing_set_means_wait_indefinitely() {
        let timeouts = Timeouts::new();
        assert_eq!(timeouts.deadline(SOCKET, Interest::Read, Some(1_000)), None);
        assert_eq!(timeouts.get(SOCKET, Interest::Read), None);
    }

    #[test]
    fn a_deadline_is_the_timeout_from_now() {
        let mut timeouts = Timeouts::new();
        timeouts.set(SOCKET, Interest::Read, 250);
        assert_eq!(timeouts.deadline(SOCKET, Interest::Read, Some(1_000)), Some(1_250));
    }

    /// Zero clears rather than meaning "expire immediately", matching the `SO_RCVTIMEO` a C#
    /// caller forwards. A language whose own zero means "never wait" translates before it gets
    /// here, which is `set`'s documentation and not this test's subject.
    #[test]
    fn zero_clears_the_timeout_rather_than_expiring_at_once() {
        let mut timeouts = Timeouts::new();
        timeouts.set(SOCKET, Interest::Read, 250);
        timeouts.set(SOCKET, Interest::Read, 0);
        assert_eq!(timeouts.get(SOCKET, Interest::Read), None);
        assert_eq!(timeouts.deadline(SOCKET, Interest::Read, Some(1_000)), None, "park indefinitely");
    }

    /// The two directions are independent: a receive timeout must not bound a send park, which is
    /// how the C# side has always behaved and is what a caller setting one of them expects.
    #[test]
    fn the_two_directions_do_not_borrow_each_others_timeouts() {
        let mut timeouts = Timeouts::new();
        timeouts.set(SOCKET, Interest::Read, 250);
        assert_eq!(timeouts.deadline(SOCKET, Interest::Write, Some(1_000)), None);
        timeouts.set(SOCKET, Interest::Write, 90);
        assert_eq!(timeouts.deadline(SOCKET, Interest::Write, Some(1_000)), Some(1_090));
        assert_eq!(timeouts.deadline(SOCKET, Interest::Read, Some(1_000)), Some(1_250));
    }

    /// THE CLOCKLESS CASE. A socket that asked not to wait indefinitely must not be parked
    /// indefinitely just because the board cannot tell the time -- the park degrades to a yield.
    ///
    /// The distinction this pins is between the two `None`s: no timeout set parks forever, and a
    /// timeout set with no clock is already due. A single implementation that returned `None` for
    /// both would hang exactly the program that had taken the trouble to say it must not.
    #[test]
    fn a_timeout_without_a_clock_is_already_due_rather_than_indefinite() {
        let mut timeouts = Timeouts::new();
        timeouts.set(SOCKET, Interest::Read, 250);
        assert_eq!(timeouts.deadline(SOCKET, Interest::Read, None), Some(0), "already due: a yield");

        let empty = Timeouts::new();
        assert_eq!(empty.deadline(SOCKET, Interest::Read, None), None, "no timeout: park indefinitely");
    }

    /// A deadline near the top of the range saturates instead of wrapping into the past.
    #[test]
    fn a_deadline_past_the_end_of_the_clock_saturates() {
        let mut timeouts = Timeouts::new();
        timeouts.set(SOCKET, Interest::Read, u32::MAX);
        assert_eq!(timeouts.deadline(SOCKET, Interest::Read, Some(u64::MAX)), Some(u64::MAX));
    }

    /// CLOSING FORGETS, IN BOTH DIRECTIONS. A backend may hand the same handle out again, and a
    /// timeout left behind by a socket that no longer exists would become an unrelated socket's --
    /// a bug that appears only under enough churn to recycle a number.
    #[test]
    fn a_closed_socket_leaves_nothing_behind_for_the_next_one() {
        let mut timeouts = Timeouts::new();
        timeouts.set(SOCKET, Interest::Read, 250);
        timeouts.set(SOCKET, Interest::Write, 90);
        assert_eq!(timeouts.tracked(), 2);

        timeouts.forget(SOCKET);
        assert_eq!(timeouts.tracked(), 0, "both directions, not just the one most callers set");
        assert_eq!(timeouts.deadline(SOCKET, Interest::Read, Some(1_000)), None);
    }

    /// And the table does not grow with churn. On a device that is the difference between a
    /// long-running program and one that runs out of memory in a way nothing points at.
    #[test]
    fn opening_and_closing_many_sockets_leaves_the_table_empty() {
        let mut timeouts = Timeouts::new();
        for socket in 0..10_000 {
            timeouts.set(socket, Interest::Read, 100);
            timeouts.set(socket, Interest::Write, 100);
            timeouts.forget(socket);
        }
        assert_eq!(timeouts.tracked(), 0, "a table nothing removes from is an unbounded leak");
    }
}
