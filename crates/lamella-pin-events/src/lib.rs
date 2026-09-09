//! The pin-change event queue: what a board's interrupt handler hands the runtime.

#![no_std]
#![forbid(unsafe_code)]

/// One pin-change event: the board's opaque token, and the pad level the handler read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinEvent {
    /// The board's own token, carried whole and never decoded here.
    pub token: u32,
    /// The pad as the interrupt handler read it: `true` = high.
    ///
    /// An OBSERVATION rather than a direction. Turning it into `Rising` or `Falling` is an
    /// inference about what happened before the read, and it belongs where it can be described as
    /// one -- see this module's note.
    pub level: bool,
}

/// The most events one drain takes before letting the program run again.
///
/// A drain that ran until the queue was empty would be unbounded in the one case that matters: a
/// line bouncing faster than managed code retires an event refills the queue behind the drain, and
/// the program that registered the handler never runs again. Bounding the batch makes a saturated
/// line degrade into a program that runs SLOWLY rather than one that stops, and the events not taken
/// this time are still queued -- they are the next batch and not a loss.
///
/// It belongs here rather than with the storage because it is a rule about ordering and fairness,
/// which is what this crate owns; a host that kept its own bound would be the second copy of it.
pub const MAX_EVENTS_PER_DRAIN: usize = 16;

/// A fixed-capacity queue of pin events, filled from interrupt context and drained by whoever runs
/// managed code.
///
/// `N` is the capacity in events. There is no allocation and no growth: a queue that could grow
/// would have to allocate in an interrupt handler, which is the one place it must not.
#[derive(Debug)]
pub struct PinEventQueue<const N: usize> {
    events: [PinEvent; N],
    /// Where the next push lands.
    head: usize,
    /// How many events are held, `0..=N`.
    len: usize,
    /// Whether any event has been DROPPED since the last drain. See [`PinEventQueue::overflowed`].
    overflowed: bool,
}

impl<const N: usize> Default for PinEventQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> PinEventQueue<N> {
    /// An empty queue.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            events: [PinEvent { token: 0, level: false }; N],
            head: 0,
            len: 0,
            overflowed: false,
        }
    }

    /// How many events are waiting.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether an event has been DROPPED since the last [`PinEventQueue::drain_overflowed`].
    ///
    /// # Why loss is reported rather than merely permitted
    ///
    /// A bounded queue must drop something when it is full, and .NET does not promise that every
    /// edge is delivered -- so dropping is legal. **Dropping SILENTLY is a different claim.** A
    /// handler that sees three events where five happened cannot tell that from five events where
    /// three happened, and nothing else in the system can tell it either. This flag is what makes
    /// the difference reportable.
    #[must_use]
    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Records `event`, dropping the OLDEST if the queue is full. Answers whether anything was
    /// dropped.
    ///
    /// # Why the oldest, given both choices lose something
    ///
    /// Dropping the NEWEST loses the edge that just happened, which for a button is the whole
    /// point -- a press that arrives during a burst is exactly the one a person is waiting to see.
    /// Dropping the oldest keeps the most recent state and costs the earliest transition. **Neither
    /// is free and this one is at least recoverable**: a handler that reads the pad on receipt sees
    /// where the pin actually is, which the newest event describes and the oldest does not.
    ///
    /// It does mean a caller may see a Falling whose Rising it never received. That is why
    /// [`PinEventQueue::overflowed`] exists: the sequence is incomplete and the caller is told so,
    /// rather than being handed a coherent story about something that did not happen.
    pub fn push(&mut self, event: PinEvent) -> bool {
        if N == 0 {
            self.overflowed = true;
            return true;
        }
        let dropped = self.len == N;
        if dropped {
            self.overflowed = true;
        }
        self.events[self.head] = event;
        self.head = (self.head + 1) % N;
        if !dropped {
            self.len += 1;
        }
        dropped
    }

    /// Takes the oldest event, or `None`.
    pub fn pop(&mut self) -> Option<PinEvent> {
        if self.len == 0 {
            return None;
        }
        let tail = (self.head + N - self.len) % N;
        self.len -= 1;
        Some(self.events[tail])
    }

    /// Clears the overflow flag and answers what it was.
    ///
    /// Separate from [`PinEventQueue::pop`] so a caller reports loss ONCE per drain rather than
    /// once per event: a burst that overflowed is one gap in the sequence, not one gap per event
    /// that survived it.
    pub fn drain_overflowed(&mut self) -> bool {
        let overflowed = self.overflowed;
        self.overflowed = false;
        overflowed
    }
}

#[cfg(test)]
mod tests {
    use super::{PinEvent, PinEventQueue};

    fn event(token: u32, level: bool) -> PinEvent {
        PinEvent { token, level }
    }

    #[test]
    fn events_come_back_oldest_first_and_carry_both_fields() {
        let mut queue = PinEventQueue::<4>::new();
        assert!(queue.is_empty());
        assert!(!queue.push(event(7, true)));
        assert!(!queue.push(event(9, false)));
        assert_eq!(queue.len(), 2);

        assert_eq!(queue.pop(), Some(event(7, true)));
        assert_eq!(queue.pop(), Some(event(9, false)));
        assert_eq!(queue.pop(), None);
        assert!(queue.is_empty());
    }

    #[test]
    fn a_full_queue_drops_the_oldest_and_says_that_it_did() {
        let mut queue = PinEventQueue::<2>::new();
        assert!(!queue.push(event(1, true)));
        assert!(!queue.push(event(2, false)));
        assert!(!queue.overflowed(), "two into a two-deep queue fits exactly");

        assert!(queue.push(event(3, true)), "the push reports the drop to its caller");
        assert!(queue.overflowed());
        assert_eq!(queue.len(), 2, "capacity is fixed, not grown");
        assert_eq!(queue.pop(), Some(event(2, false)), "the OLDEST went, not the newest");
        assert_eq!(queue.pop(), Some(event(3, true)));
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn overflow_is_reported_once_per_drain_and_not_once_per_event() {
        let mut queue = PinEventQueue::<1>::new();
        queue.push(event(1, true));
        queue.push(event(2, false));
        queue.push(event(3, true));

        assert!(queue.drain_overflowed(), "the gap is reported");
        assert!(!queue.drain_overflowed(), "and not a second time");
        assert!(!queue.overflowed());
    }

    #[test]
    fn the_ring_keeps_working_after_it_wraps() {
        let mut queue = PinEventQueue::<3>::new();
        for round in 0..2 {
            for i in 0..3u32 {
                assert!(!queue.push(event(round * 10 + i, i % 2 == 0)));
            }
            for i in 0..3u32 {
                assert_eq!(queue.pop(), Some(event(round * 10 + i, i % 2 == 0)));
            }
            assert!(queue.is_empty());
            assert!(!queue.overflowed(), "a queue drained in step never overflowed");
        }
    }

    #[test]
    fn one_interrupt_entry_may_notify_several_tokens() {
        let mut queue = PinEventQueue::<8>::new();
        for token in 0..5u32 {
            assert!(!queue.push(event(token, true)));
        }
        assert_eq!(queue.len(), 5);
        for token in 0..5u32 {
            assert_eq!(queue.pop().map(|e| e.token), Some(token));
        }
    }

    #[test]
    fn a_zero_capacity_queue_refuses_loudly_rather_than_swallowing() {
        let mut queue = PinEventQueue::<0>::new();
        assert!(queue.push(event(1, true)), "the push reports that nothing was kept");
        assert!(queue.overflowed());
        assert_eq!(queue.pop(), None);
    }
}
