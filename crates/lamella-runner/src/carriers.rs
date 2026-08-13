//! Serving Lamella Link on several carriers at once, with one session between them.

use lamella_wire::session::{Arbiter, ChannelClass, ChannelId, Decision, Granted};
use lamella_wire::{Frame, Transport, TransportError, error, msg};

use crate::{bundle, deploy};

/// The sequence number a target-originated frame carries when it answers no request: the
/// revocation and the liveness probe. Unsolicited frames ride at zero everywhere in this protocol.
const UNSOLICITED_SEQ: u16 = 0;

/// The largest number of carriers one set can hold, because [`ChannelId`] numbers them.
const MAX_CARRIERS: usize = ChannelId::MAX as usize + 1;

/// The two waits a set has to bound, in milliseconds.
///
/// Both are firmware policy rather than protocol: they trade how long a claimant waits against how
/// readily the board gives up on a peer that may simply be slow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Windows {
    /// How long an incumbent has to show any sign of life before its session is taken.
    ///
    /// It has to cover a round trip on the slowest carrier the board serves, because the cost of
    /// getting this wrong is taking the board away from a host that was working.
    pub probe_ms: u64,
    /// How long after a flash-writing frame a transfer is still believed to be in flight.
    ///
    /// It has to cover the longest gap WITHIN a transfer rather than the transfer itself, since
    /// each write refreshes it. The module-firmware erase is what sizes it: that one step takes
    /// seconds, and a board interrupted during it has a module with no firmware.
    pub write_ms: u64,
}

impl Default for Windows {
    /// A second to answer a probe, five to keep believing in a transfer.
    fn default() -> Self {
        Self { probe_ms: 1_000, write_ms: 5_000 }
    }
}

/// Whether a message type programs flash, and therefore holds a transfer open.
///
/// The module-firmware types are here for the same reason the image ones are, and more sharply: the
/// erase alone takes seconds, which is the longest a board spends unable to survive losing its
/// session.
///
/// A reply, a query and a reset are all absent on purpose. Only a write leaves a half-finished
/// state behind it.
fn writes_flash(msg_type: u8) -> bool {
    matches!(
        msg_type,
        deploy::DEPLOY_IMAGE
            | deploy::DEPLOY_CLEAR
            | deploy::DEPLOY_CHUNK
            | deploy::WINC_FW_START
            | deploy::WINC_FW_CHUNK
            | deploy::WINC_FW_END
            | bundle::DEPLOY_BUNDLE
    )
}

/// One carrier a set serves on: a transport, and what kind of reach it represents.
///
/// The class is the carrier's, not the peer's, and it is what makes somebody at the bench with a
/// cable outrank a remote host. A firmware states it once, where it knows what the transport is
/// physically attached to, because nothing further down can tell.
pub struct Carrier<'t> {
    transport: &'t mut dyn Transport,
    class: ChannelClass,
}

impl<'t> Carrier<'t> {
    /// A carrier reached over a network -- a socket, directly or through a relay.
    pub fn network(transport: &'t mut dyn Transport) -> Self {
        Self { transport, class: ChannelClass::Network }
    }

    /// A carrier somebody had to physically attach: a USB cable, a serial line, a debug probe's
    /// virtual port.
    pub fn physical(transport: &'t mut dyn Transport) -> Self {
        Self { transport, class: ChannelClass::Physical }
    }
}

/// Several carriers serving one Lamella Link session, as a single [`Transport`].
///
/// Build it with [`CarrierSet::new`] and hand it to a serve function exactly as a single transport
/// would be handed over. A carrier's [`ChannelId`] is its index in the slice.
pub struct CarrierSet<'a, 't> {
    carriers: &'a mut [Carrier<'t>],
    arbiter: Arbiter,
    /// `None` on a board with no monotonic source -- see [`CarrierSet::unclocked`].
    now_ms: Option<fn() -> u64>,
    windows: Windows,
    /// When the liveness probe in flight gives up on the incumbent.
    probe_deadline: Option<u64>,
    /// The claim waiting on that probe, kept whole so it can be answered either way rather than
    /// having to be sent again by a host that was told nothing.
    held_claim: Option<(usize, Frame)>,
    /// When a flash transfer stops being believed in. `Some` exactly while the arbiter is in its
    /// critical section, so the two cannot drift apart.
    write_deadline: Option<u64>,
    /// The carrier whose request is being answered. Replies follow the request rather than the
    /// session, so a refusal reaches the carrier that earned it.
    reply_to: Option<usize>,
    /// Where the next poll starts, so a carrier with a lot to say cannot starve the others.
    next: usize,
}

impl<'a, 't> CarrierSet<'a, 't> {
    /// A set over `carriers`, bounded by `windows`.
    ///
    /// `None` for an empty slice, which can never serve anything, and for more carriers than a
    /// [`ChannelId`] can number. Both are answered at construction rather than by a set that
    /// quietly serves some of what it was given.
    pub fn new(
        carriers: &'a mut [Carrier<'t>],
        now_ms: fn() -> u64,
        windows: Windows,
    ) -> Option<Self> {
        if carriers.is_empty() || carriers.len() > MAX_CARRIERS {
            return None;
        }
        Some(Self {
            carriers,
            arbiter: Arbiter::new(),
            now_ms: Some(now_ms),
            windows,
            probe_deadline: None,
            held_claim: None,
            write_deadline: None,
            reply_to: None,
            next: 0,
        })
    }

    /// A set on a board with NO monotonic source, which gives up two rules and says which.
    ///
    /// Ranking still works: a physical carrier preempts a network one immediately and without
    /// probing, because presence at the board is the answer and no clock is consulted to reach it.
    /// So does telling the loser, answering a claimant, and following a request with its reply.
    ///
    /// # What is given up, and why each is given up rather than faked
    ///
    /// **The liveness probe.** Deciding an incumbent has stopped answering means bounding how long
    /// to wait, and there is nothing here to bound it with. A probe that can never be given up on
    /// would hold the board for a claimant that is never told anything -- worse than not probing.
    /// So a claim against a live incumbent of EQUAL OR LOWER rank is REFUSED at once, naming the
    /// holder. The consequence is stated rather than hidden: **a wedged host keeps the board until
    /// its carrier is released or the board is reset**, which on a cable is unplugging it -- the
    /// recovery path such a board already had.
    ///
    /// **The deploy critical section.** Its whole safety comes from being bounded; unbounded, it
    /// is the one way arbitration can wedge a board permanently, because a host that dies
    /// mid-transfer sends no end-of-transfer message and a clockless set has no other way to stop
    /// believing in it. It is not opened here at all.
    ///
    /// **That second one costs nothing on the boards this exists for, and the reason is worth
    /// following:** a clockless board's carriers are a cable and a USB port, both
    /// [`ChannelClass::Physical`]. Equal rank means a second claimant is refused rather than
    /// granted, so nothing can interrupt a transfer in the first place and the section it would
    /// have opened protects a window that cannot occur. A board with a NETWORK carrier is a board
    /// with a network stack, and those need a clock of their own -- so the mixed-rank clockless
    /// case, the one where this would matter, does not arise.
    ///
    /// `None` on an empty slice or more carriers than a [`ChannelId`] can number, as [`new`].
    ///
    /// [`new`]: CarrierSet::new
    pub fn unclocked(carriers: &'a mut [Carrier<'t>]) -> Option<Self> {
        if carriers.is_empty() || carriers.len() > MAX_CARRIERS {
            return None;
        }
        Some(Self {
            carriers,
            arbiter: Arbiter::new(),
            now_ms: None,
            windows: Windows::default(),
            probe_deadline: None,
            held_claim: None,
            write_deadline: None,
            reply_to: None,
            next: 0,
        })
    }

    /// Whether this set has a monotonic source, and therefore probes an incumbent rather than
    /// refusing its challenger outright.
    #[must_use]
    pub fn is_clocked(&self) -> bool {
        self.now_ms.is_some()
    }

    /// The carrier currently holding the session, if any.
    #[must_use]
    pub fn owner(&self) -> Option<ChannelId> {
        self.arbiter.owner()
    }

    /// The class of the carrier holding the session, if any.
    #[must_use]
    pub fn owner_class(&self) -> Option<ChannelClass> {
        self.arbiter.owner_class()
    }

    /// Whether an incumbent is being probed for liveness right now.
    #[must_use]
    pub fn probing(&self) -> bool {
        self.arbiter.probing()
    }

    /// Whether a claim is being HELD across polls, waiting on a liveness probe.
    ///
    /// The one thing this set keeps alive between requests that a caller may need to know about: the
    /// claimant's own frame, held whole so the probe can be answered either way. **A firmware on a
    /// rewinding bump arena must not roll the frontier back past it** -- the frame was allocated
    /// during the request that is ending, so a rewind hands its bytes to the next allocation and the
    /// claim is answered from memory something else now owns. Skipping one rewind costs that
    /// iteration's reclamation; not skipping it corrupts the answer.
    ///
    /// A board on a reclaiming heap can ignore this: its rewind is a no-op and the frame dies with
    /// its own `Drop`.
    #[must_use]
    pub fn holds_claim(&self) -> bool {
        self.held_claim.is_some()
    }

    /// Whether a flash transfer is believed to be in flight, so claims are being deferred.
    #[must_use]
    pub fn writing(&self) -> bool {
        self.write_deadline.is_some()
    }

    /// How many carriers this set serves.
    #[must_use]
    pub fn len(&self) -> usize {
        self.carriers.len()
    }

    /// Never true -- a set with no carriers has no representation, which [`CarrierSet::new`]
    /// enforces. Present because a type with a `len` and no `is_empty` is one every reader asks
    /// about.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        false
    }

    /// Send on one carrier, releasing it if it has failed.
    ///
    /// A carrier that cannot be written to is gone, and leaving it holding the session would wedge
    /// the board on a peer that is not there. The error is still returned to whatever asked for the
    /// send, because a reply that did not arrive is not a delivered reply.
    fn send_on(
        &mut self,
        index: usize,
        msg_type: u8,
        seq: u16,
        payload: &[u8],
    ) -> Result<(), TransportError> {
        let Some(carrier) = self.carriers.get_mut(index) else {
            return Ok(());
        };
        match carrier.transport.send(msg_type, seq, payload) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.release(index);
                Err(error)
            }
        }
    }

    /// Send on one carrier ONLY if it can take the frame now, releasing it if it has failed.
    ///
    /// This is the path for every frame the SET ORIGINATES -- a liveness probe, a revocation, a
    /// refusal owed to a claim whose host may long since have gone. None of them answers a request,
    /// so none of them is worth blocking the whole loop for: the carrier they are aimed at is
    /// precisely the one whose host might not be there.
    ///
    /// `Ok(false)` is information rather than failure. Nothing was written, the carrier is still
    /// attached, and what the caller has learned is that nobody is reading it.
    fn try_send_on(
        &mut self,
        index: usize,
        msg_type: u8,
        seq: u16,
        payload: &[u8],
    ) -> Result<bool, TransportError> {
        let Some(carrier) = self.carriers.get_mut(index) else {
            return Ok(true);
        };
        match carrier.transport.try_send(msg_type, seq, payload) {
            Ok(sent) => Ok(sent),
            Err(error) => {
                self.release(index);
                Err(error)
            }
        }
    }

    /// Give up on a carrier: it loses the session if it held it, and stops being a claim in flight.
    fn release(&mut self, index: usize) {
        if self.reply_to == Some(index) {
            self.reply_to = None;
        }
        if self.held_claim.as_ref().is_some_and(|(claimant, _)| *claimant == index) {
            self.held_claim = None;
            self.probe_deadline = None;
        }
        if self.arbiter.owner() == Some(index as ChannelId) {
            self.write_deadline = None;
        }
        if let Some((winner, decision)) = self.arbiter.release(index as ChannelId) {
            let _ = self.settle(winner as usize, decision, UNSOLICITED_SEQ);
        }
    }

    /// Act on an arbiter [`Decision`], answering everybody it names.
    ///
    /// `Ok(true)` when the claimant may go on to be served, so a caller holding its frame can hand
    /// that frame up.
    fn settle(
        &mut self,
        claimant: usize,
        decision: Decision,
        seq: u16,
    ) -> Result<bool, TransportError> {
        match decision {
            Decision::Granted(Granted { revoke, dropped }) => {
                let holder = self.carriers[claimant].class as u8;
                if let Some(revoked) = revoke {
                    let _ = self.try_send_on(
                        revoked as usize,
                        msg::SESSION_REVOKED,
                        UNSOLICITED_SEQ,
                        &[holder],
                    );
                }
                if let Some(overtaken) = dropped {
                    let overtaken_seq = self.take_held_seq(overtaken as usize);
                    let _ = self.try_send_on(
                        overtaken as usize,
                        msg::ERROR,
                        overtaken_seq,
                        &error::session_held(holder),
                    );
                }
                Ok(true)
            }
            Decision::AlreadyOwner => Ok(true),
            Decision::Probing if self.now_ms.is_none() => {
                let _ = self.arbiter.release(claimant as ChannelId);
                let holder = self.arbiter.owner_class().map_or(0, |class| class as u8);
                self.send_on(claimant, msg::ERROR, seq, &error::session_held(holder))?;
                Ok(false)
            }
            Decision::Probing => {
                if let Some(owner) = self.arbiter.owner() {
                    match self.try_send_on(owner as usize, msg::PING, UNSOLICITED_SEQ, &[]) {
                        Err(_) => return Ok(false),
                        Ok(false) => {
                            if let Some((winner, decision)) = self.arbiter.owner_silent() {
                                let _ = self.settle(winner as usize, decision, UNSOLICITED_SEQ);
                            }
                            return Ok(false);
                        }
                        Ok(true) => {}
                    }
                }
                self.probe_deadline =
                    self.now().map(|now| now.saturating_add(self.windows.probe_ms));
                Ok(false)
            }
            Decision::Deferred => Ok(false),
            Decision::Refused { holder } => {
                self.send_on(claimant, msg::ERROR, seq, &error::session_held(holder as u8))?;
                Ok(false)
            }
        }
    }

    /// The monotonic reading, or `None` on a clockless set.
    fn now(&self) -> Option<u64> {
        self.now_ms.map(|clock| clock())
    }

    /// Take the held claim's sequence number when it belongs to `index`, clearing the claim.
    fn take_held_seq(&mut self, index: usize) -> u16 {
        match self.held_claim.as_ref() {
            Some((claimant, frame)) if *claimant == index => {
                let seq = frame.seq;
                self.held_claim = None;
                self.probe_deadline = None;
                seq
            }
            _ => UNSOLICITED_SEQ,
        }
    }

    /// Give up on an incumbent whose liveness probe has run out of time.
    fn expire_probe(&mut self) {
        let Some(deadline) = self.probe_deadline else {
            return;
        };
        let Some(now) = self.now() else {
            return;
        };
        if now < deadline {
            return;
        }
        self.probe_deadline = None;
        if let Some((winner, decision)) = self.arbiter.owner_silent() {
            let _ = self.settle(winner as usize, decision, UNSOLICITED_SEQ);
        }
    }

    /// Stop believing in a transfer nothing has refreshed.
    fn expire_write(&mut self) {
        let Some(deadline) = self.write_deadline else {
            return;
        };
        let Some(now) = self.now() else {
            return;
        };
        if now < deadline {
            return;
        }
        self.end_write();
    }

    /// Close the critical section a transfer opened.
    fn end_write(&mut self) {
        if self.write_deadline.take().is_some() {
            self.arbiter.leave_critical();
        }
    }

    /// Hand back a held claim whose carrier now owns the session.
    ///
    /// This is the single place a claim wins, whether it got there by outlasting the probe or by
    /// the incumbent hanging up mid-probe. The host is answered on the request it actually sent
    /// instead of having to send it again, which is what keeps the probe invisible to it.
    fn take_won_claim(&mut self) -> Option<Frame> {
        let (claimant, _) = self.held_claim.as_ref()?;
        if self.arbiter.owner() != Some(*claimant as ChannelId) {
            return None;
        }
        let (claimant, frame) = self.held_claim.take()?;
        self.probe_deadline = None;
        self.reply_to = Some(claimant);
        Some(frame)
    }

    /// Decide what happens to a frame that arrived on `index`, and hand it up if it may be served.
    fn admit(&mut self, index: usize, frame: Frame) -> Result<Option<Frame>, TransportError> {
        if self.probing() && self.arbiter.owner() == Some(index as ChannelId) {
            if let Some((waiting, decision)) = self.arbiter.owner_answered() {
                let seq = self.take_held_seq(waiting as usize);
                let _ = self.settle(waiting as usize, decision, seq);
            }
            self.probe_deadline = None;
        }

        if frame.msg_type == msg::PONG {
            return Ok(None);
        }

        if frame.msg_type == msg::HELLO || self.arbiter.owner().is_none() {
            let class = self.carriers[index].class;
            let seq = frame.seq;
            let decision = self.arbiter.claim(index as ChannelId, class);
            if matches!(decision, Decision::Probing) {
                self.held_claim = Some((index, frame));
                let _ = self.settle(index, decision, seq);
                return Ok(None);
            }
            if !self.settle(index, decision, seq)? {
                return Ok(None);
            }
        }

        if !self.arbiter.may_act(index as ChannelId, &frame) {
            let holder = self.arbiter.owner_class().map_or(0, |class| class as u8);
            self.send_on(index, msg::ERROR, frame.seq, &error::session_held(holder))?;
            return Ok(None);
        }

        self.reply_to = Some(index);
        if writes_flash(frame.msg_type) {
            if let Some(now) = self.now() {
                self.arbiter.enter_critical();
                self.write_deadline = Some(now.saturating_add(self.windows.write_ms));
            }
        } else if self.arbiter.owner() == Some(index as ChannelId) {
            self.end_write();
        }
        Ok(Some(frame))
    }
}

impl Transport for CarrierSet<'_, '_> {
    /// Answer on the carrier whose request is being served, or on the session's owner when the
    /// frame answers nothing.
    ///
    /// A frame with NEITHER goes to every carrier -- see below, it is the boot path and it matters.
    fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
        if let Some(index) = self.reply_to.or_else(|| self.arbiter.owner().map(usize::from)) {
            return self.send_on(index, msg_type, seq, payload);
        }
        let mut delivered = false;
        let mut failure = None;
        for index in 0..self.carriers.len() {
            match self.try_send_on(index, msg_type, seq, payload) {
                Ok(true) => delivered = true,
                Ok(false) => {}
                Err(error) => failure = Some(error),
            }
        }
        if delivered {
            Ok(())
        } else {
            Err(failure.unwrap_or(TransportError::Closed))
        }
    }

    /// The same routing as [`Transport::send`], for a caller above this set that has an unsolicited
    /// frame of its own and does not want to block on a carrier nobody is reading.
    fn try_send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<bool, TransportError> {
        let target = self.reply_to.or_else(|| self.arbiter.owner().map(usize::from));
        let Some(index) = target else {
            return Ok(true);
        };
        self.try_send_on(index, msg_type, seq, payload)
    }

    /// The next frame any carrier has that may be served, having arbitrated everything underneath.
    ///
    /// Frames consumed by arbitration -- a claim, a refusal, a probe answer -- return `None`: they
    /// were answered here and there is nothing above this for them to mean. A caller polls again.
    ///
    /// Never fails. A carrier's error is that carrier's, and is handled by releasing it.
    fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
        self.expire_write();
        self.expire_probe();
        if let Some(frame) = self.take_won_claim() {
            return Ok(Some(frame));
        }
        let count = self.carriers.len();
        for step in 0..count {
            let index = (self.next + step) % count;
            let polled = self.carriers[index].transport.poll();
            match polled {
                Err(_) => {
                    self.release(index);
                }
                Ok(None) => {}
                Ok(Some(frame)) => {
                    self.next = (index + 1) % count;
                    return self.admit(index, frame);
                }
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use lamella_wire::{FrameReader, encode_frame};

    thread_local! {
        static CLOCK: Cell<u64> = const { Cell::new(0) };
    }

    fn now_ms() -> u64 {
        CLOCK.with(Cell::get)
    }

    fn set_now(millis: u64) {
        CLOCK.with(|clock| clock.set(millis));
    }

    fn windows() -> Windows {
        Windows { probe_ms: 500, write_ms: 5_000 }
    }

    #[derive(Default)]
    struct LineState {
        reader: FrameReader,
        out: Vec<Frame>,
        failing: bool,
        /// Attached and healthy, but nothing is draining it -- a native-USB carrier whose host has
        /// closed its handle while the device stays configured.
        unread: bool,
    }

    /// One carrier, as a handle the test keeps a clone of. The set holds one clone and the test the
    /// other, so what the target sent on a given line can be read WHILE the set is still borrowing
    /// it -- which is the only way to watch arbitration happen rather than only its result.
    #[derive(Clone, Default)]
    struct Line(Rc<RefCell<LineState>>);

    impl Line {
        /// The host on this line sends the target a frame.
        fn host_sends(&self, msg_type: u8, seq: u16, payload: &[u8]) {
            let bytes = encode_frame(msg_type, seq, payload).expect("encodable");
            self.0.borrow_mut().reader.push(&bytes);
        }

        /// Everything the target has sent on this line since the last look.
        fn target_sent(&self) -> Vec<Frame> {
            core::mem::take(&mut self.0.borrow_mut().out)
        }

        fn sent_types(&self) -> Vec<u8> {
            self.target_sent().iter().map(|frame| frame.msg_type).collect()
        }

        /// The line breaks: every later operation on it fails.
        fn break_line(&self) {
            self.0.borrow_mut().failing = true;
        }

        /// The line goes unread while staying attached and healthy: a host that walked away
        /// without closing, which is not the same as a broken line.
        fn stop_reading(&self) {
            self.0.borrow_mut().unread = true;
        }
    }

    impl Transport for Line {
        fn send(&mut self, msg_type: u8, seq: u16, payload: &[u8]) -> Result<(), TransportError> {
            let mut state = self.0.borrow_mut();
            if state.failing {
                return Err(TransportError::Closed);
            }
            state.out.push(Frame { msg_type, seq, payload: payload.to_vec() });
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<Frame>, TransportError> {
            let mut state = self.0.borrow_mut();
            if state.failing {
                return Err(TransportError::Closed);
            }
            Ok(state.reader.next_frame())
        }

        fn try_send(
            &mut self,
            msg_type: u8,
            seq: u16,
            payload: &[u8],
        ) -> Result<bool, TransportError> {
            let mut state = self.0.borrow_mut();
            if state.failing {
                return Err(TransportError::Closed);
            }
            if state.unread {
                return Ok(false);
            }
            state.out.push(Frame { msg_type, seq, payload: payload.to_vec() });
            Ok(true)
        }
    }

    /// A chunk header the way a real deploy sends one. The set never reads it; it is here so the
    /// frame under test is the frame that actually programs flash.
    fn chunk_payload() -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes());
        payload.extend_from_slice(&64u32.to_le_bytes());
        payload.extend_from_slice(&[0xAA; 4]);
        payload
    }

    #[test]
    fn a_set_with_no_carriers_has_no_representation() {
        let mut none: [Carrier; 0] = [];
        assert!(
            CarrierSet::new(&mut none, now_ms, windows()).is_none(),
            "an empty set can never serve anything and says so at construction"
        );
    }

    #[test]
    fn an_unowned_board_answers_whoever_speaks_first_without_a_hello() {
        set_now(0);
        let uart = Line::default();
        let tcp = Line::default();
        let (mut uart_t, mut tcp_t) = (uart.clone(), tcp.clone());
        let mut carriers = [Carrier::physical(&mut uart_t), Carrier::network(&mut tcp_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        tcp.host_sends(deploy::DEPLOY_STATUS, 5, &[]);
        let served = set.poll().expect("a set never fails as a whole");

        assert_eq!(
            served.map(|frame| frame.msg_type),
            Some(deploy::DEPLOY_STATUS),
            "a board nothing owns serves the first carrier to speak, as it did before arbitration"
        );
        assert_eq!(set.owner(), Some(1), "and that carrier now holds the session");
        assert!(tcp.target_sent().is_empty(), "nothing was refused");
    }

    #[test]
    fn a_cable_takes_the_board_from_the_network_and_the_network_host_is_told() {
        set_now(0);
        let uart = Line::default();
        let tcp = Line::default();
        let (mut uart_t, mut tcp_t) = (uart.clone(), tcp.clone());
        let mut carriers = [Carrier::physical(&mut uart_t), Carrier::network(&mut tcp_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        tcp.host_sends(msg::HELLO, 1, &[]);
        assert!(set.poll().unwrap().is_some(), "the network host is served the session");
        assert_eq!(set.owner(), Some(1));

        uart.host_sends(msg::HELLO, 2, &[]);
        let served = set.poll().expect("a set never fails as a whole");

        assert_eq!(
            served.map(|frame| frame.msg_type),
            Some(msg::HELLO),
            "presence at the board wins immediately and without a probe"
        );
        assert_eq!(set.owner(), Some(0));
        let revoked = tcp.target_sent();
        assert_eq!(revoked.len(), 1, "the loser is told exactly once");
        assert_eq!(revoked[0].msg_type, msg::SESSION_REVOKED);
        assert_eq!(
            revoked[0].payload,
            vec![ChannelClass::Physical as u8],
            "and is told a CABLE took it, which is a thing to wait out rather than a fault"
        );
    }

    #[test]
    fn a_peer_of_equal_rank_probes_the_incumbent_and_is_refused_on_its_own_sequence() {
        set_now(0);
        let first = Line::default();
        let second = Line::default();
        let (mut first_t, mut second_t) = (first.clone(), second.clone());
        let mut carriers = [Carrier::network(&mut first_t), Carrier::network(&mut second_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        first.host_sends(msg::HELLO, 1, &[]);
        set.poll().unwrap();
        let _ = first.target_sent();

        second.host_sends(msg::HELLO, 77, &[]);
        assert!(set.poll().unwrap().is_none(), "the claim is held, not served");
        assert!(set.probing(), "the incumbent is being asked whether it is still there");
        assert_eq!(first.sent_types(), vec![msg::PING], "and asked with a liveness probe");
        assert!(
            second.target_sent().is_empty(),
            "the claimant is told nothing it would have to undo"
        );

        first.host_sends(msg::PONG, 0, &[]);
        assert!(
            set.poll().unwrap().is_none(),
            "a probe answer is the set's own mail, not the serve loop's"
        );

        assert!(!set.probing());
        assert_eq!(set.owner(), Some(0), "a host that is working is a host that is still there");
        let refusal = second.target_sent();
        assert_eq!(refusal.len(), 1, "the claim is answered rather than dropped");
        assert_eq!(refusal[0].msg_type, msg::ERROR);
        assert_eq!(refusal[0].payload, error::session_held(ChannelClass::Network as u8));
        assert_eq!(
            refusal[0].seq, 77,
            "on the sequence it is still waiting on, so the host matches it to its own request"
        );
    }

    #[test]
    fn a_wedged_incumbent_loses_the_board_and_the_winners_own_claim_is_what_gets_answered() {
        set_now(0);
        let wedged = Line::default();
        let peer = Line::default();
        let (mut wedged_t, mut peer_t) = (wedged.clone(), peer.clone());
        let mut carriers = [Carrier::network(&mut wedged_t), Carrier::network(&mut peer_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        wedged.host_sends(msg::HELLO, 1, &[]);
        set.poll().unwrap();
        let _ = wedged.target_sent();

        peer.host_sends(msg::HELLO, 91, &[]);
        assert!(set.poll().unwrap().is_none());
        assert!(set.probing());

        set_now(501);
        let served = set.poll().expect("a set never fails as a whole");

        assert_eq!(set.owner(), Some(1), "the board is not held forever by a host that is gone");
        let promoted = served.expect("the held claim is what comes out, not silence");
        assert_eq!(promoted.msg_type, msg::HELLO);
        assert_eq!(
            promoted.seq, 91,
            "the host is answered on the request it sent rather than having to send it again"
        );
        assert_eq!(
            wedged.sent_types(),
            vec![msg::PING, msg::SESSION_REVOKED],
            "it was asked before it was dropped -- an incumbent is never taken unprobed"
        );
    }

    #[test]
    fn a_carrier_that_fails_does_not_fail_the_set() {
        set_now(0);
        let uart = Line::default();
        let tcp = Line::default();
        let (mut uart_t, mut tcp_t) = (uart.clone(), tcp.clone());
        let mut carriers = [Carrier::network(&mut tcp_t), Carrier::physical(&mut uart_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        tcp.break_line();
        uart.host_sends(msg::HELLO, 3, &[]);
        let served = set.poll().expect("a broken carrier is not the set breaking");

        assert_eq!(
            served.map(|frame| frame.msg_type),
            Some(msg::HELLO),
            "a board reachable three ways must not die when one of them does"
        );
        assert_eq!(set.owner(), Some(1));
    }

    #[test]
    fn a_carrier_that_fails_while_holding_the_session_gives_it_up() {
        set_now(0);
        let uart = Line::default();
        let tcp = Line::default();
        let (mut uart_t, mut tcp_t) = (uart.clone(), tcp.clone());
        let mut carriers = [Carrier::physical(&mut uart_t), Carrier::network(&mut tcp_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        tcp.host_sends(msg::HELLO, 1, &[]);
        set.poll().unwrap();
        assert_eq!(set.owner(), Some(1));

        tcp.break_line();
        set.poll().unwrap();

        assert_eq!(set.owner(), None, "a carrier that cannot be read is not holding anything");
        uart.host_sends(msg::HELLO, 2, &[]);
        set.poll().unwrap();
        assert_eq!(set.owner(), Some(0), "so the next claimant wins outright rather than probing");
        assert!(!set.probing());
    }

    #[test]
    fn a_claim_between_the_chunks_of_a_deploy_is_deferred_rather_than_granted() {
        set_now(0);
        let uart = Line::default();
        let tcp = Line::default();
        let (mut uart_t, mut tcp_t) = (uart.clone(), tcp.clone());
        let mut carriers = [Carrier::physical(&mut uart_t), Carrier::network(&mut tcp_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        tcp.host_sends(msg::HELLO, 1, &[]);
        set.poll().unwrap();
        let _ = tcp.target_sent();

        tcp.host_sends(deploy::DEPLOY_CHUNK, 2, &chunk_payload());
        let writing = set.poll().unwrap().expect("the owner's chunk is served");
        assert_eq!(writing.msg_type, deploy::DEPLOY_CHUNK);
        set.send(deploy::DEPLOY_RESULT, 2, &[1]).unwrap();
        let _ = tcp.target_sent();
        assert!(set.writing(), "the transfer outlives its own reply, which is the whole point");

        set_now(50);
        uart.host_sends(msg::HELLO, 3, &[]);
        assert!(set.poll().unwrap().is_none(), "a claim mid-transfer is not served");
        assert_eq!(set.owner(), Some(1), "even a cable waits out a deploy");
        assert!(
            tcp.target_sent().is_empty(),
            "nothing revoked the writer, which is what leaves an image with a hole in it"
        );

        tcp.host_sends(deploy::DEPLOY_STATUS, 4, &[]);
        set.poll().unwrap();
        assert!(!set.writing(), "the section ends when the owner stops writing");

        uart.host_sends(msg::HELLO, 5, &[]);
        assert!(set.poll().unwrap().is_some(), "the retry is served");
        assert_eq!(set.owner(), Some(0), "and wins, a deploy later");
        assert_eq!(tcp.sent_types(), vec![msg::SESSION_REVOKED]);
    }

    #[test]
    fn a_transfer_nothing_refreshes_stops_being_believed_in() {
        set_now(0);
        let uart = Line::default();
        let tcp = Line::default();
        let (mut uart_t, mut tcp_t) = (uart.clone(), tcp.clone());
        let mut carriers = [Carrier::physical(&mut uart_t), Carrier::network(&mut tcp_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        tcp.host_sends(msg::HELLO, 1, &[]);
        set.poll().unwrap();
        tcp.host_sends(deploy::DEPLOY_CHUNK, 2, &chunk_payload());
        set.poll().unwrap();
        let _ = tcp.target_sent();
        assert!(set.writing());

        set_now(5_001);
        uart.host_sends(msg::HELLO, 3, &[]);
        assert!(set.poll().unwrap().is_some(), "the claim is granted once the transfer lapses");

        assert!(!set.writing());
        assert_eq!(
            set.owner(),
            Some(0),
            "a section nothing closes would defer every future claim forever"
        );
    }

    #[test]
    fn a_reply_follows_the_request_rather_than_the_session() {
        set_now(0);
        let uart = Line::default();
        let tcp = Line::default();
        let (mut uart_t, mut tcp_t) = (uart.clone(), tcp.clone());
        let mut carriers = [Carrier::physical(&mut uart_t), Carrier::network(&mut tcp_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        uart.host_sends(msg::HELLO, 1, &[]);
        set.poll().unwrap();
        let _ = uart.target_sent();

        tcp.host_sends(msg::PING, 42, &[]);
        let served = set.poll().unwrap().expect("session control is exempt from the gate");
        assert_eq!(served.msg_type, msg::PING);

        set.send(msg::PONG, 42, &[]).unwrap();

        assert_eq!(
            tcp.sent_types(),
            vec![msg::PONG],
            "the answer went where the question came from"
        );
        assert!(uart.target_sent().is_empty(), "and not to the session's owner");
    }

    #[test]
    fn a_non_owner_asking_for_real_work_is_refused_and_told_who_holds_the_board() {
        set_now(0);
        let uart = Line::default();
        let tcp = Line::default();
        let (mut uart_t, mut tcp_t) = (uart.clone(), tcp.clone());
        let mut carriers = [Carrier::physical(&mut uart_t), Carrier::network(&mut tcp_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        uart.host_sends(msg::HELLO, 1, &[]);
        set.poll().unwrap();
        let _ = uart.target_sent();

        tcp.host_sends(deploy::DEPLOY_CLEAR, 8, &[]);
        assert!(set.poll().unwrap().is_none(), "another carrier's work is not done");

        let refusal = tcp.target_sent();
        assert_eq!(refusal.len(), 1);
        assert_eq!(refusal[0].msg_type, msg::ERROR);
        assert_eq!(refusal[0].seq, 8);
        assert_eq!(
            error::session_holder(&refusal[0].payload),
            Some(ChannelClass::Physical as u8),
            "the useful half is not NO but that somebody is at the bench with a cable"
        );
    }

    #[test]
    fn a_carrier_with_a_lot_to_say_does_not_starve_the_other() {
        set_now(0);
        let busy = Line::default();
        let quiet = Line::default();
        let (mut busy_t, mut quiet_t) = (busy.clone(), quiet.clone());
        let mut carriers = [Carrier::network(&mut busy_t), Carrier::network(&mut quiet_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        for seq in 1..=3 {
            busy.host_sends(msg::PING, seq, &[]);
        }
        quiet.host_sends(msg::PING, 99, &[]);

        let mut seen = Vec::new();
        for _ in 0..4 {
            if let Some(frame) = set.poll().unwrap() {
                seen.push(frame.seq);
            }
        }

        assert_eq!(
            seen,
            vec![1, 99, 2, 3],
            "the quiet carrier is reached on the very next poll, not after the queue drains"
        );
    }

    #[test]
    fn a_boot_report_nobody_asked_for_reaches_every_carrier() {
        set_now(0);
        let uart = Line::default();
        let usb = Line::default();
        let (mut uart_t, mut usb_t) = (uart.clone(), usb.clone());
        let mut carriers = [Carrier::physical(&mut uart_t), Carrier::physical(&mut usb_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        assert_eq!(set.owner(), None);
        set.send(crate::repl::RUN_RESULT, 0, &[7]).unwrap();

        for (name, line) in [("uart", &uart), ("usb", &usb)] {
            let sent = line.target_sent();
            assert_eq!(sent.len(), 1, "the {name} carrier was told");
            assert_eq!(sent[0].msg_type, crate::repl::RUN_RESULT);
            assert_eq!(sent[0].payload, vec![7]);
        }
    }

    #[test]
    fn a_probe_a_carrier_will_not_take_is_answered_at_once_instead_of_waited_out() {
        set_now(0);
        let incumbent = Line::default();
        let claimant = Line::default();
        let (mut incumbent_t, mut claimant_t) = (incumbent.clone(), claimant.clone());
        let mut carriers =
            [Carrier::physical(&mut incumbent_t), Carrier::physical(&mut claimant_t)];
        let mut set = CarrierSet::new(&mut carriers, now_ms, windows()).expect("two carriers");

        incumbent.host_sends(msg::HELLO, 1, &[]);
        set.poll().unwrap();
        let _ = incumbent.target_sent();

        incumbent.stop_reading();

        claimant.host_sends(msg::HELLO, 42, &[]);
        assert!(set.poll().unwrap().is_none(), "the claim is held for one poll, as ever");

        let served = set
            .poll()
            .unwrap()
            .expect("the claim is answered without waiting out a window it cannot need");

        assert_eq!(served.msg_type, msg::HELLO);
        assert_eq!(served.seq, 42, "on its own request, not a resend");
        assert_eq!(set.owner(), Some(1));
        assert!(!set.probing());
        assert!(
            incumbent.target_sent().is_empty(),
            "and nothing was written to the carrier that could not take it"
        );
    }

    #[test]
    fn a_clockless_set_refuses_an_equal_rank_challenger_rather_than_probing_forever() {
        let first = Line::default();
        let second = Line::default();
        let (mut first_t, mut second_t) = (first.clone(), second.clone());
        let mut carriers = [Carrier::physical(&mut first_t), Carrier::physical(&mut second_t)];
        let mut set = CarrierSet::unclocked(&mut carriers).expect("two carriers");
        assert!(!set.is_clocked());

        first.host_sends(msg::HELLO, 1, &[]);
        set.poll().unwrap();
        let _ = first.target_sent();

        second.host_sends(msg::HELLO, 55, &[]);
        assert!(set.poll().unwrap().is_none(), "the claim is not served");

        assert!(
            !set.probing(),
            "and NOTHING is left waiting on a probe this board could never give up on"
        );
        assert!(first.target_sent().is_empty(), "the incumbent is not probed at all");
        let refusal = second.target_sent();
        assert_eq!(
            refusal.len(),
            1,
            "the claimant is still ANSWERED -- that is the part which must not degrade"
        );
        assert_eq!(refusal[0].msg_type, msg::ERROR);
        assert_eq!(refusal[0].seq, 55, "on its own sequence");
        assert_eq!(
            error::session_holder(&refusal[0].payload),
            Some(ChannelClass::Physical as u8)
        );
    }

    #[test]
    fn a_clockless_set_still_ranks_a_cable_over_the_network() {
        let uart = Line::default();
        let tcp = Line::default();
        let (mut uart_t, mut tcp_t) = (uart.clone(), tcp.clone());
        let mut carriers = [Carrier::physical(&mut uart_t), Carrier::network(&mut tcp_t)];
        let mut set = CarrierSet::unclocked(&mut carriers).expect("two carriers");

        tcp.host_sends(msg::HELLO, 1, &[]);
        set.poll().unwrap();
        let _ = tcp.target_sent();

        uart.host_sends(msg::HELLO, 2, &[]);
        assert!(set.poll().unwrap().is_some(), "presence at the board still wins");

        assert_eq!(set.owner(), Some(0));
        assert_eq!(
            tcp.target_sent().first().map(|frame| frame.msg_type),
            Some(msg::SESSION_REVOKED),
            "ranking consults no clock, so it is not one of the things a clockless board gives up"
        );
    }

    #[test]
    fn a_clockless_set_opens_no_deploy_section_and_a_higher_rank_claim_is_granted_through_one() {
        let uart = Line::default();
        let tcp = Line::default();
        let (mut uart_t, mut tcp_t) = (uart.clone(), tcp.clone());
        let mut carriers = [Carrier::physical(&mut uart_t), Carrier::network(&mut tcp_t)];
        let mut set = CarrierSet::unclocked(&mut carriers).expect("two carriers");

        tcp.host_sends(msg::HELLO, 1, &[]);
        set.poll().unwrap();
        tcp.host_sends(deploy::DEPLOY_CHUNK, 2, &chunk_payload());
        set.poll().unwrap();
        let _ = tcp.target_sent();

        assert!(!set.writing(), "no section is opened, because nothing here could ever close one");

        uart.host_sends(msg::HELLO, 3, &[]);
        assert!(set.poll().unwrap().is_some(), "a higher rank is granted through the transfer");
        assert_eq!(set.owner(), Some(0));
    }
}
