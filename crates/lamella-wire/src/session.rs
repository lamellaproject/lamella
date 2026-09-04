//! Who owns the debug session, when several carriers can reach one target at once. A pure
//! decision -- no carrier, no input and no output -- so the rule can be tested whole.

use crate::Frame;

/// What kind of carrier a claim arrives on. The ordering is the authority ordering, and it exists
/// for one reason: a person at the board with a cable is the way a board wedged by a remote host
/// gets recovered.
///
/// `#[non_exhaustive]`: a carrier reached through a broker the device dialed OUT to is a third
/// kind, neither physical nor a socket a peer opened inward, and it will want its own rung rather
/// than to be squeezed onto one of these two. Leaving room now costs a wildcard arm; not leaving
/// it costs a breaking change at exactly the moment the answer stops being obvious.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[non_exhaustive]
pub enum ChannelClass {
    /// Reached over a network -- a socket, directly or through a relay. Anyone who can route to
    /// the board can attempt this, so it is the lower authority.
    Network = 1,
    /// Reached over something somebody had to physically attach: a USB cable, a serial line, a
    /// debug probe's virtual port. Attaching it is itself evidence of being present at the board.
    Physical = 2,
}

/// A carrier instance, numbered by the serve loop that owns the carriers. The arbiter never
/// interprets it -- it hands the same number back to say which carrier to answer.
pub type ChannelId = u8;

/// What the serve loop should do about a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Granted {
    /// The previous owner, which must be told it lost the session rather than left to discover it
    /// from the next operation failing.
    pub revoke: Option<ChannelId>,
    /// A claimant that was waiting on a liveness probe and has now been overtaken. It is owed an
    /// answer too: it asked, and something happened.
    pub dropped: Option<ChannelId>,
}

/// The answer to a claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The claimant is now the owner.
    Granted(Granted),
    /// The claimant already owns the session -- a reconnect on the same carrier, or a repeated
    /// request. Reported distinctly rather than as a fresh grant, because a fresh grant would
    /// revoke the claimant's own session.
    AlreadyOwner,
    /// The incumbent is being probed for liveness. The serve loop should send it a liveness
    /// request and then call [`Arbiter::owner_answered`] or [`Arbiter::owner_silent`]. The
    /// claimant is not told anything yet: it is about to be told something true.
    Probing,
    /// A flash write or equivalent is in flight and must finish. The claimant should retry; this
    /// is a wait of milliseconds, not a refusal.
    Deferred,
    /// Refused, and by whom. The class is the useful half -- it tells a remote host that the
    /// board is in somebody's hands rather than merely busy.
    Refused {
        /// The holder's carrier class.
        holder: ChannelClass,
    },
}

/// The current owner of the session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Owner {
    channel: ChannelId,
    class: ChannelClass,
}

/// Decides which carrier holds the one debug session a target has to give.
///
/// Drive it from the serve loop: [`Arbiter::claim`] when a carrier asks, [`Arbiter::release`] when
/// one disconnects, [`Arbiter::enter_critical`] and [`Arbiter::leave_critical`] around a flash
/// write, and the two probe outcomes when a claim answers [`Decision::Probing`].
#[derive(Clone, Copy, Debug, Default)]
pub struct Arbiter {
    owner: Option<Owner>,
    /// A claimant of equal or lower class waiting on the incumbent's liveness probe. At most one:
    /// a second is refused outright and retries, which costs it a round trip and keeps this a
    /// state machine somebody can read.
    challenger: Option<Owner>,
    critical: bool,
}

impl Arbiter {
    /// An arbiter with no session open.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The carrier that currently holds the session, if any.
    #[must_use]
    pub fn owner(&self) -> Option<ChannelId> {
        self.owner.map(|owner| owner.channel)
    }

    /// The class of the carrier holding the session, if any.
    #[must_use]
    pub fn owner_class(&self) -> Option<ChannelClass> {
        self.owner.map(|owner| owner.class)
    }

    /// Whether a claim is currently waiting on a liveness probe.
    #[must_use]
    pub fn probing(&self) -> bool {
        self.challenger.is_some()
    }

    /// A carrier asks for the session.
    pub fn claim(&mut self, channel: ChannelId, class: ChannelClass) -> Decision {
        let Some(owner) = self.owner else {
            self.owner = Some(Owner { channel, class });
            return Decision::Granted(Granted { revoke: None, dropped: None });
        };
        if owner.channel == channel {
            return Decision::AlreadyOwner;
        }
        if self.critical {
            return Decision::Deferred;
        }
        if class > owner.class {
            let dropped = self.challenger.take().map(|waiting| waiting.channel);
            self.owner = Some(Owner { channel, class });
            return Decision::Granted(Granted { revoke: Some(owner.channel), dropped });
        }
        if self.challenger.is_some() {
            return Decision::Refused { holder: owner.class };
        }
        self.challenger = Some(Owner { channel, class });
        Decision::Probing
    }

    /// The incumbent answered its liveness probe, so it is alive and keeps the session.
    ///
    /// Returns the waiting claimant and the refusal owed to it, or `None` if nothing was waiting.
    pub fn owner_answered(&mut self) -> Option<(ChannelId, Decision)> {
        let waiting = self.challenger.take()?;
        let holder = self.owner.map_or(waiting.class, |owner| owner.class);
        Some((waiting.channel, Decision::Refused { holder }))
    }

    /// The incumbent did not answer within the probe window, so it has stopped being the owner in
    /// every sense but the bookkeeping. The waiting claimant takes the session.
    ///
    /// Returns the claimant and its grant, or `None` if nothing was waiting.
    pub fn owner_silent(&mut self) -> Option<(ChannelId, Decision)> {
        let waiting = self.challenger.take()?;
        let revoke = self.owner.map(|owner| owner.channel);
        self.owner = Some(waiting);
        Some((waiting.channel, Decision::Granted(Granted { revoke, dropped: None })))
    }

    /// A carrier disconnected or gave up the session.
    ///
    /// Returns a waiting claimant and its grant when releasing the owner promotes one -- the
    /// common good case, where the incumbent hangs up while a peer is mid-probe.
    pub fn release(&mut self, channel: ChannelId) -> Option<(ChannelId, Decision)> {
        if self.challenger.map(|waiting| waiting.channel) == Some(channel) {
            self.challenger = None;
            return None;
        }
        if self.owner.map(|owner| owner.channel) != Some(channel) {
            return None;
        }
        self.owner = None;
        self.critical = false;
        let waiting = self.challenger.take()?;
        self.owner = Some(waiting);
        Some((waiting.channel, Decision::Granted(Granted { revoke: None, dropped: None })))
    }

    /// Mark the start of an operation that must not be interrupted -- a flash page write, a
    /// chunked deploy. Claims are deferred until it ends.
    pub fn enter_critical(&mut self) {
        self.critical = true;
    }

    /// Mark the end of that operation.
    pub fn leave_critical(&mut self) {
        self.critical = false;
    }

    /// Whether a frame arriving on `channel` may be acted on.
    ///
    /// The session-control messages are exempt: a carrier that does not own the session still has
    /// to be able to ask for it, and has to be able to be told no.
    #[must_use]
    pub fn may_act(&self, channel: ChannelId, frame: &Frame) -> bool {
        if is_session_control(frame.msg_type) {
            return true;
        }
        self.owner.map(|owner| owner.channel) == Some(channel)
    }
}

/// Whether a message type is one a carrier may send without owning the session: the handshake, the
/// liveness pair, the refusal, and the revocation.
///
/// Keeping the handshake outside the gate is what lets a host discover WHAT a board is, and be
/// told who holds it, without holding it. Gating the handshake too would answer every question
/// with the same silence.
#[must_use]
pub fn is_session_control(msg_type: u8) -> bool {
    matches!(
        msg_type,
        crate::msg::HELLO
            | crate::msg::HELLO_ACK
            | crate::msg::HELLO_NAK
            | crate::msg::ERROR
            | crate::msg::PING
            | crate::msg::PONG
            | crate::msg::SESSION_REVOKED
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const USB: ChannelId = 1;
    const UART: ChannelId = 2;
    const TCP: ChannelId = 3;
    const TCP2: ChannelId = 4;

    fn granted(revoke: Option<ChannelId>, dropped: Option<ChannelId>) -> Decision {
        Decision::Granted(Granted { revoke, dropped })
    }

    #[test]
    fn the_first_claimant_takes_an_idle_session() {
        let mut arbiter = Arbiter::new();
        assert_eq!(arbiter.claim(TCP, ChannelClass::Network), granted(None, None));
        assert_eq!(arbiter.owner(), Some(TCP));
    }

    /// A repeat claim from the holder must not revoke the holder's own session, which is what a
    /// fresh grant would do. A host that re-sends its opening request -- and the handshake driver
    /// retries on an interval, so it does -- would otherwise tear down the session it just opened.
    #[test]
    fn the_holder_reclaiming_is_not_a_new_grant() {
        let mut arbiter = Arbiter::new();
        arbiter.claim(TCP, ChannelClass::Network);
        assert_eq!(arbiter.claim(TCP, ChannelClass::Network), Decision::AlreadyOwner);
        assert_eq!(arbiter.owner(), Some(TCP));
    }

    /// PHYSICAL PRESENCE WINS, AND WITHOUT WAITING. This is the recovery path: a board wedged by a
    /// remote host is fixed by walking over and plugging in a cable, and that must not require
    /// negotiating with the host that wedged it.
    #[test]
    fn a_cable_preempts_the_network_immediately_and_names_who_to_revoke() {
        let mut arbiter = Arbiter::new();
        arbiter.claim(TCP, ChannelClass::Network);
        assert_eq!(arbiter.claim(USB, ChannelClass::Physical), granted(Some(TCP), None));
        assert_eq!(arbiter.owner(), Some(USB));
        assert!(!arbiter.probing(), "a preemption asks nobody anything");
    }

    /// And never the other way. A remote peer does not get to evict somebody standing at the
    /// board, so it takes the ordinary route: probe, and be refused by a live holder.
    #[test]
    fn the_network_does_not_preempt_a_cable() {
        let mut arbiter = Arbiter::new();
        arbiter.claim(USB, ChannelClass::Physical);
        assert_eq!(arbiter.claim(TCP, ChannelClass::Network), Decision::Probing);
        assert_eq!(
            arbiter.owner_answered(),
            Some((TCP, Decision::Refused { holder: ChannelClass::Physical })),
            "and the refusal says a cable has it, not merely that the board is busy"
        );
        assert_eq!(arbiter.owner(), Some(USB));
    }

    /// THE WEDGED-OWNER CASE, which is the whole reason for probing. A crashed host holds a
    /// half-open socket indefinitely: nothing arrives and nothing errors. An incumbent that cannot
    /// answer has already stopped being the owner in every sense but the bookkeeping.
    #[test]
    fn a_silent_incumbent_loses_the_session_to_the_claimant_waiting_on_it() {
        let mut arbiter = Arbiter::new();
        arbiter.claim(TCP, ChannelClass::Network);
        assert_eq!(arbiter.claim(TCP2, ChannelClass::Network), Decision::Probing);
        assert_eq!(arbiter.owner_silent(), Some((TCP2, granted(Some(TCP), None))));
        assert_eq!(arbiter.owner(), Some(TCP2));
    }

    /// A FLASH WRITE IS NOT INTERRUPTIBLE, and this holds even for the carrier that outranks the
    /// holder. Preempting a chunked deploy leaves an image with a hole in it and a board that
    /// boots into it -- so the answer is "in a moment", not "no", and the claimant wins on retry.
    #[test]
    fn a_critical_section_defers_even_a_cable_and_the_claim_wins_after_it() {
        let mut arbiter = Arbiter::new();
        arbiter.claim(TCP, ChannelClass::Network);
        arbiter.enter_critical();
        assert_eq!(arbiter.claim(USB, ChannelClass::Physical), Decision::Deferred);
        assert_eq!(arbiter.owner(), Some(TCP), "and the deploy keeps the board while it finishes");

        arbiter.leave_critical();
        assert_eq!(arbiter.claim(USB, ChannelClass::Physical), granted(Some(TCP), None));
    }

    /// A claimant mid-probe is not forgotten when something outranks it: it asked, so it is owed
    /// an answer. Dropping it silently would leave it waiting for a probe result that will never
    /// be computed.
    #[test]
    fn a_preemption_also_answers_the_claimant_that_was_waiting() {
        let mut arbiter = Arbiter::new();
        arbiter.claim(TCP, ChannelClass::Network);
        assert_eq!(arbiter.claim(TCP2, ChannelClass::Network), Decision::Probing);
        assert_eq!(
            arbiter.claim(UART, ChannelClass::Physical),
            granted(Some(TCP), Some(TCP2)),
            "the evicted owner AND the overtaken claimant are both named"
        );
        assert!(!arbiter.probing());
    }

    /// One claimant waits at a time; a second is refused rather than queued, and retries.
    #[test]
    fn only_one_claimant_waits_on_a_probe() {
        let mut arbiter = Arbiter::new();
        arbiter.claim(USB, ChannelClass::Physical);
        assert_eq!(arbiter.claim(TCP, ChannelClass::Network), Decision::Probing);
        assert_eq!(
            arbiter.claim(TCP2, ChannelClass::Network),
            Decision::Refused { holder: ChannelClass::Physical }
        );
    }

    /// The ordinary good case: the holder hangs up while a peer is mid-probe, and the peer is
    /// promoted without anybody having to wait out the probe window.
    #[test]
    fn releasing_the_session_promotes_a_waiting_claimant() {
        let mut arbiter = Arbiter::new();
        arbiter.claim(TCP, ChannelClass::Network);
        arbiter.claim(TCP2, ChannelClass::Network);
        assert_eq!(arbiter.release(TCP), Some((TCP2, granted(None, None))));
        assert_eq!(arbiter.owner(), Some(TCP2));
    }

    /// A claimant that gives up while waiting takes itself out of the running, and does not
    /// inherit the session when the holder later leaves.
    #[test]
    fn a_claimant_that_disconnects_while_waiting_is_forgotten() {
        let mut arbiter = Arbiter::new();
        arbiter.claim(TCP, ChannelClass::Network);
        arbiter.claim(TCP2, ChannelClass::Network);
        assert_eq!(arbiter.release(TCP2), None);
        assert!(!arbiter.probing());
        assert_eq!(arbiter.release(TCP), None, "and nobody is promoted");
        assert_eq!(arbiter.owner(), None);
    }

    /// THE ONE WAY THIS TYPE COULD WEDGE A BOARD. A critical section belongs to the session that
    /// opened it, so an owner that vanishes mid-deploy must not leave the flag set -- every later
    /// claim would defer forever, and the board would refuse everyone politely and permanently.
    ///
    /// THE OBVIOUS VERSION OF THIS TEST CANNOT FAIL, and the first one written here was it:
    /// claim, enter, release, claim -- and the last claim finds NO owner, which is granted by the
    /// first branch of `claim` before the critical flag is ever consulted. It passed with the
    /// clearing removed. **A stale flag is invisible until an owner exists again**, so the test
    /// has to take the session twice: the second claimant is the first one to reach the check.
    #[test]
    fn an_owner_that_vanishes_mid_deploy_does_not_leave_the_board_deferring_forever() {
        let mut arbiter = Arbiter::new();
        arbiter.claim(TCP, ChannelClass::Network);
        arbiter.enter_critical();
        arbiter.release(TCP);

        assert_eq!(arbiter.claim(USB, ChannelClass::Physical), granted(None, None));

        assert_eq!(
            arbiter.claim(TCP2, ChannelClass::Network),
            Decision::Probing,
            "a critical section belonging to a session that has ended must not defer anyone"
        );
    }

    /// A carrier that does not own the session may still ask for it and still be told no -- so the
    /// handshake, the liveness pair and the refusals are outside the gate, and everything that
    /// touches target state is inside it.
    #[test]
    fn the_gate_admits_the_handshake_and_refuses_the_operations() {
        let mut arbiter = Arbiter::new();
        arbiter.claim(TCP, ChannelClass::Network);
        let frame = |msg_type| Frame { msg_type, seq: 0, payload: alloc::vec::Vec::new() };

        for control in [crate::msg::HELLO, crate::msg::PING, crate::msg::ERROR, crate::msg::SESSION_REVOKED] {
            assert!(arbiter.may_act(USB, &frame(control)), "a non-owner may still speak {control:#x}");
        }
        assert!(!arbiter.may_act(USB, &frame(0x23)), "a non-owner may not deploy");
        assert!(arbiter.may_act(TCP, &frame(0x23)), "the owner may");
    }
}
