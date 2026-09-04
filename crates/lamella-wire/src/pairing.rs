//! Which key a target will accept, and when a replacement takes over from the one it replaces.
//! The rotation POLICY only, and no cryptography at all: a caller supplies the verifier.

/// Key length in bytes. 32, so the same material serves a MAC and a TLS pre-shared key without a
/// second size to keep track of.
pub const KEY_LEN: usize = 32;

/// A pairing key.
///
/// `Debug` is implemented by hand and prints NOTHING of the material. A derived one would put the
/// key in every log line that ever formats a structure containing it, and the deriving is exactly
/// the sort of thing added later by someone adding a field.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Key([u8; KEY_LEN]);

impl Key {
    /// A key from raw material.
    #[must_use]
    pub fn from_bytes(bytes: [u8; KEY_LEN]) -> Self {
        Self(bytes)
    }

    /// The raw material, for handing to a MAC or a TLS pre-shared-key slot.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; KEY_LEN] {
        &self.0
    }

    /// Whether two keys are equal, without a comparison whose duration depends on where they
    /// first differ.
    ///
    /// Not load-bearing at the call site inside this module -- that one compares two keys the
    /// target already holds, so there is no attacker to learn anything -- but a key type that
    /// offers only a short-circuiting comparison invites one at a call site where it would be.
    #[must_use]
    pub fn ct_eq(&self, other: &Self) -> bool {
        let mut difference = 0u8;
        for index in 0..KEY_LEN {
            difference |= self.0[index] ^ other.0[index];
        }
        difference == 0
    }
}

impl core::fmt::Debug for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("Key(<redacted>)")
    }
}

/// What happened when a replacement key was staged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Staged {
    /// Staged beside the current key. Both open the board until the new one is first used.
    Waiting,
    /// Staged, and it replaced a previous staged key that had never been used. The caller should
    /// say so: whoever holds that earlier key is now holding one that will never work, and they
    /// will not find out from anything else.
    ReplacedPending,
    /// The target had no key at all, so this one is live immediately -- there is no incumbent to
    /// prove anything against, and staging it would leave the board open to everyone meanwhile.
    ProvisionedDirectly,
    /// Refused: the staged key is the one already in use. A rotation to the same value is not a
    /// rotation, and accepting it would leave a pending slot that can never commit.
    SameAsCurrent,
}

/// The result of an authentication attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No live key matched.
    Rejected,
    /// The current key matched. Nothing changed.
    AcceptedCurrent,
    /// The STAGED key matched, so the rotation is now committed: it has become the current key and
    /// the one it replaced is gone.
    ///
    /// **This is the event worth reporting to a person.** It is the only moment at which an old
    /// key stops working, and a developer who rotated a key an hour ago wants to know that the
    /// change has taken rather than that it is still pending.
    AcceptedPendingAndCommitted,
}

/// The keys a target will currently accept.
///
/// A target with no key is UNPROVISIONED, which is a distinct state from having a key that nothing
/// matches: the first is a board that has never been paired and the second is a board somebody has
/// lost the key to, and they want different things said about them.
#[derive(Clone, Copy, Debug, Default)]
pub struct KeyStore {
    current: Option<Key>,
    pending: Option<Key>,
}

impl KeyStore {
    /// An unprovisioned store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A store holding one key and no staged replacement -- what a target loads from flash at boot.
    #[must_use]
    pub fn with_current(key: Key) -> Self {
        Self { current: Some(key), pending: None }
    }

    /// Whether any key is set. `false` means the board has never been paired.
    #[must_use]
    pub fn is_provisioned(&self) -> bool {
        self.current.is_some()
    }

    /// Whether a replacement is staged and waiting to be proved.
    #[must_use]
    pub fn rotation_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// Set the key outright, discarding any staged replacement.
    ///
    /// This is the DESTRUCTIVE path and it is deliberately a different method from
    /// [`KeyStore::stage`]. A caller reaching for it is saying it has out-of-band certainty that
    /// the new key has arrived somewhere useful -- a key the operator typed in themselves, or one
    /// baked into an image. Everything else should stage.
    pub fn set_current(&mut self, key: Key) {
        self.current = Some(key);
        self.pending = None;
    }

    /// Stage a replacement beside the current key. Both open the board until the new one is first
    /// used successfully, at which point the old one stops working.
    pub fn stage(&mut self, key: Key) -> Staged {
        let Some(current) = self.current else {
            self.current = Some(key);
            self.pending = None;
            return Staged::ProvisionedDirectly;
        };
        if current.ct_eq(&key) {
            return Staged::SameAsCurrent;
        }
        let replaced = match self.pending {
            Some(pending) => !pending.ct_eq(&key),
            None => false,
        };
        self.pending = Some(key);
        if replaced { Staged::ReplacedPending } else { Staged::Waiting }
    }

    /// Abandon a staged replacement; the current key carries on alone.
    ///
    /// Returns whether there was one, so a caller can tell "cancelled" from "there was nothing to
    /// cancel" rather than reporting success either way.
    pub fn cancel_pending(&mut self) -> bool {
        self.pending.take().is_some()
    }

    /// Try the live keys against a verifier, and commit the rotation if the staged one is what
    /// answered.
    ///
    /// `verify` is the cryptography, supplied by the caller: given a candidate key, does it
    /// produce the authenticator the peer presented? This module never sees a challenge, a MAC or
    /// a hash function, which is what keeps a protocol crate free of a cryptography dependency and
    /// keeps this rule testable without one.
    ///
    /// The current key is tried first, so the ordinary case costs one verification and the
    /// two-key window costs a second one only when the first does not answer.
    pub fn authenticate_with(&mut self, mut verify: impl FnMut(&Key) -> bool) -> Outcome {
        if let Some(current) = self.current {
            if verify(&current) {
                return Outcome::AcceptedCurrent;
            }
        }
        if let Some(pending) = self.pending {
            if !verify(&pending) {
                return Outcome::Rejected;
            }
            self.current = Some(pending);
            self.pending = None;
            return Outcome::AcceptedPendingAndCommitted;
        }
        Outcome::Rejected
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> Key {
        Key::from_bytes([seed; KEY_LEN])
    }

    /// A verifier standing in for the MAC check: the peer knows exactly this key.
    fn knows(held: Key) -> impl FnMut(&Key) -> bool {
        move |candidate: &Key| candidate.ct_eq(&held)
    }

    #[test]
    fn an_unprovisioned_board_takes_the_first_key_directly() {
        let mut store = KeyStore::new();
        assert!(!store.is_provisioned());
        assert_eq!(store.stage(key(1)), Staged::ProvisionedDirectly);
        assert!(store.is_provisioned());
        assert!(!store.rotation_pending(), "there was no incumbent for it to prove itself against");
        assert_eq!(store.authenticate_with(knows(key(1))), Outcome::AcceptedCurrent);
    }

    /// THE LOCKOUT THIS MODULE EXISTS TO PREVENT. A staged key that never reaches anybody must
    /// leave the board exactly as reachable as it was.
    #[test]
    fn a_staged_key_that_is_never_used_changes_nothing() {
        let mut store = KeyStore::with_current(key(1));
        assert_eq!(store.stage(key(2)), Staged::Waiting);
        assert_eq!(store.authenticate_with(knows(key(1))), Outcome::AcceptedCurrent);
        assert!(store.rotation_pending(), "and the rotation is still on offer");
    }

    /// THE COMMIT. Using the new key once is the proof somebody has it, and that is the moment the
    /// old one stops working -- not the moment the new one was generated.
    #[test]
    fn the_new_key_displaces_the_old_one_the_first_time_it_is_used() {
        let mut store = KeyStore::with_current(key(1));
        store.stage(key(2));
        assert_eq!(store.authenticate_with(knows(key(2))), Outcome::AcceptedPendingAndCommitted);

        assert!(!store.rotation_pending());
        assert_eq!(store.authenticate_with(knows(key(2))), Outcome::AcceptedCurrent);
        assert_eq!(
            store.authenticate_with(knows(key(1))),
            Outcome::Rejected,
            "and the key it replaced is gone the moment the replacement proved itself"
        );
    }

    /// Both keys are live during the window, in either order, which is what makes rotating over
    /// the network safe: the session doing the rotating keeps working throughout.
    #[test]
    fn both_keys_open_the_board_while_a_rotation_is_staged() {
        let mut store = KeyStore::with_current(key(1));
        store.stage(key(2));
        assert_eq!(store.authenticate_with(knows(key(1))), Outcome::AcceptedCurrent);
        assert_eq!(store.authenticate_with(knows(key(1))), Outcome::AcceptedCurrent);
        assert!(store.rotation_pending(), "using the OLD key does not commit or cancel anything");
        assert_eq!(store.authenticate_with(knows(key(2))), Outcome::AcceptedPendingAndCommitted);
    }

    /// Re-staging replaces the waiting key and SAYS SO. Whoever holds the earlier staged key is
    /// now holding one that will never work, and nothing else will ever tell them.
    #[test]
    fn re_staging_reports_that_it_invalidated_the_previous_offer() {
        let mut store = KeyStore::with_current(key(1));
        assert_eq!(store.stage(key(2)), Staged::Waiting);
        assert_eq!(store.stage(key(3)), Staged::ReplacedPending);
        assert_eq!(store.authenticate_with(knows(key(2))), Outcome::Rejected, "the first offer is dead");
        assert_eq!(store.authenticate_with(knows(key(3))), Outcome::AcceptedPendingAndCommitted);
    }

    /// STAGING TWICE WITH NOTHING IN BETWEEN. The second offer replaces the first, and the key in
    /// USE is untouched by either -- which is the invariant that matters, because it is the one
    /// that says no sequence of re-generations can lock anybody out.
    #[test]
    fn generating_twice_replaces_the_offer_and_never_touches_the_key_in_use() {
        let mut store = KeyStore::with_current(key(1));
        assert_eq!(store.stage(key(2)), Staged::Waiting);
        assert_eq!(store.stage(key(3)), Staged::ReplacedPending);

        assert_eq!(store.authenticate_with(knows(key(1))), Outcome::AcceptedCurrent);
        assert_eq!(store.authenticate_with(knows(key(2))), Outcome::Rejected, "the first offer is dead");
        assert_eq!(store.authenticate_with(knows(key(3))), Outcome::AcceptedPendingAndCommitted);
    }

    /// THE SAME, BUT WITH A LOGIN ON THE OLD KEY IN THE MIDDLE -- which is the sequence a person
    /// actually performs: generate, carry on working over the existing session, generate again.
    ///
    /// Authenticating with the CURRENT key must neither commit the offer nor cancel it, so the
    /// second generation finds exactly the state the first left and behaves identically.
    #[test]
    fn a_login_on_the_old_key_between_two_generations_changes_nothing_about_either() {
        let mut store = KeyStore::with_current(key(1));
        assert_eq!(store.stage(key(2)), Staged::Waiting);
        assert_eq!(store.authenticate_with(knows(key(1))), Outcome::AcceptedCurrent);
        assert!(store.rotation_pending(), "using the key in use neither commits nor cancels");

        assert_eq!(store.stage(key(3)), Staged::ReplacedPending);
        assert_eq!(store.authenticate_with(knows(key(1))), Outcome::AcceptedCurrent, "still in use");
        assert_eq!(store.authenticate_with(knows(key(2))), Outcome::Rejected);
        assert_eq!(store.authenticate_with(knows(key(3))), Outcome::AcceptedPendingAndCommitted);
    }

    /// RE-STAGING THE SAME OFFER IS NOT A REPLACEMENT, and reporting it as one is worse than
    /// saying nothing: a tool that retried a pairing request over a flaky link would tell the
    /// developer that the key they are holding is dead, when it is the key that is on offer.
    #[test]
    fn restaging_the_identical_offer_is_idempotent_rather_than_a_replacement() {
        let mut store = KeyStore::with_current(key(1));
        assert_eq!(store.stage(key(2)), Staged::Waiting);
        assert_eq!(
            store.stage(key(2)),
            Staged::Waiting,
            "the same offer restated is the same offer, not a new one invalidating the last"
        );
        assert_eq!(store.authenticate_with(knows(key(2))), Outcome::AcceptedPendingAndCommitted);
    }

    /// Staging the key already in use would leave a pending slot that can never commit -- it would
    /// verify as the CURRENT key every time and never reach the promotion. Refused rather than
    /// accepted into a state with no exit.
    #[test]
    fn staging_the_current_key_is_refused_rather_than_left_uncommittable() {
        let mut store = KeyStore::with_current(key(1));
        assert_eq!(store.stage(key(1)), Staged::SameAsCurrent);
        assert!(!store.rotation_pending());
    }

    #[test]
    fn cancelling_says_whether_there_was_anything_to_cancel() {
        let mut store = KeyStore::with_current(key(1));
        assert!(!store.cancel_pending(), "nothing staged");
        store.stage(key(2));
        assert!(store.cancel_pending());
        assert_eq!(store.authenticate_with(knows(key(2))), Outcome::Rejected);
        assert_eq!(store.authenticate_with(knows(key(1))), Outcome::AcceptedCurrent);
    }

    /// The destructive path is a different method precisely so it cannot be reached by accident,
    /// and it drops a staged replacement with it.
    #[test]
    fn setting_a_key_outright_discards_a_staged_one() {
        let mut store = KeyStore::with_current(key(1));
        store.stage(key(2));
        store.set_current(key(3));
        assert!(!store.rotation_pending());
        assert_eq!(store.authenticate_with(knows(key(1))), Outcome::Rejected);
        assert_eq!(store.authenticate_with(knows(key(2))), Outcome::Rejected);
        assert_eq!(store.authenticate_with(knows(key(3))), Outcome::AcceptedCurrent);
    }

    /// A wrong key is refused whether or not a rotation is in flight, and refusing costs both
    /// verifications rather than leaking which slot was consulted by returning early.
    #[test]
    fn a_key_matching_neither_slot_is_refused() {
        let mut store = KeyStore::with_current(key(1));
        store.stage(key(2));
        let mut tried = 0;
        let outcome = store.authenticate_with(|_| {
            tried += 1;
            false
        });
        assert_eq!(outcome, Outcome::Rejected);
        assert_eq!(tried, 2, "both live keys are consulted");
        assert!(store.rotation_pending(), "and a failed attempt cancels nothing");
    }

    /// A key must not print itself. A derived `Debug` would put the material into every log line
    /// that ever formats a structure containing one, and the deriving is the sort of thing added
    /// later by somebody adding an unrelated field.
    #[test]
    fn a_key_never_prints_its_material() {
        let rendered = alloc::format!("{:?}", KeyStore::with_current(key(0xAB)));
        assert!(!rendered.contains("171"), "the material must not appear as bytes: {rendered}");
        assert!(!rendered.contains("ab") && !rendered.contains("AB"), "nor as hex: {rendered}");
        assert!(rendered.contains("redacted"), "and the redaction should be visible: {rendered}");
    }

    /// The comparison does not short-circuit: it must answer the same for a key differing in the
    /// first byte and one differing only in the last.
    #[test]
    fn the_key_comparison_examines_every_byte() {
        let base = key(0);
        let mut first_differs = [0u8; KEY_LEN];
        first_differs[0] = 1;
        let mut last_differs = [0u8; KEY_LEN];
        last_differs[KEY_LEN - 1] = 1;
        assert!(!base.ct_eq(&Key::from_bytes(first_differs)));
        assert!(!base.ct_eq(&Key::from_bytes(last_differs)));
        assert!(base.ct_eq(&Key::from_bytes([0u8; KEY_LEN])));
    }
}
