//! Which physical probe did the user mean? The selection ladder, in one place.

/// The environment variable naming the probe this shell's work should reach.
///
/// **A bench with one probe per board needs a per-shell default, because the alternative is a
/// `--serial` on every command and the one time it is forgotten is the one that matters.** A lane
/// exports its own probe's serial once; every tool that builds its selector with
/// [`Selector::from_environment`] then reaches that lane's board and no other.
pub const PROBE_SERIAL_ENV: &str = "LAMELLA_PROBE_SERIAL";

/// What the ladder needs to know about one discovered device, and nothing more.
///
/// Deliberately three fields. A probe family's own discovery type carries far more -- transport,
/// HID usage pages, a backend locator for reopening the exact interface -- and none of it takes
/// part in deciding WHICH PHYSICAL BOARD was meant. Keeping those out means this rule cannot
/// acquire a dependency on one family's idea of what a probe is, which is how it ended up
/// implemented once per family in the first place.
///
/// One physical probe yields one of these per interface, so a composite device appears several
/// times sharing a serial. [`choose`] counts DISTINCT probes, not interfaces.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// USB vendor id.
    pub vendor_id: u16,
    /// USB product id.
    pub product_id: u16,
    /// Serial number, if the OS reported one. Shared across every interface of one physical probe.
    pub serial: Option<String>,
}

/// The constraints a caller places on which probe it means.
#[derive(Debug, Clone, Default)]
pub struct Selector {
    /// Require this USB vendor id.
    pub vendor_id: Option<u16>,
    /// Require this USB product id.
    pub product_id: Option<u16>,
    /// Require this serial number -- the reliable way to pick one probe out of several alike.
    pub serial: Option<String>,
}

impl Selector {
    /// Matches the only connected probe, and REFUSES when there is more than one.
    ///
    /// **It does NOT mean "the first one discovered", which stops being a usable answer the moment
    /// a bench holds two probes of the same model.** Identical probes enumerate in an order that
    /// changes with plug order and across reboots, so "the first" names no particular board.
    #[must_use]
    pub fn any() -> Self {
        Self::default()
    }

    /// The selector a tool should build when the user named no probe: the serial in
    /// [`PROBE_SERIAL_ENV`] if the environment sets one, otherwise [`Selector::any`].
    #[must_use]
    pub fn from_environment() -> Self {
        match std::env::var(PROBE_SERIAL_ENV) {
            Ok(serial) if !serial.trim().is_empty() => Self::by_serial(serial.trim()),
            _ => Self::any(),
        }
    }

    /// The selector a tool should build from an OPTIONAL serial the user supplied: that name if
    /// there is one, otherwise [`from_environment`](Self::from_environment), otherwise
    /// [`any`](Self::any).
    ///
    /// # Why this is one function rather than three calls written out at each site
    ///
    /// The rungs are not interchangeable and every tool that reaches a probe needs all three.
    /// Written out at each call site, the two ways it degrades are both silent:
    ///
    /// - **a baked-in default OVERRIDES the environment.** A call site that defaults to one probe's
    ///   serial makes exporting the variable do nothing, so the tool goes to a device the operator
    ///   did not name -- the failure the variable exists to prevent, produced by the convenience
    ///   meant to save typing.
    /// - **[`any`](Self::any) alone SKIPS the environment.** Building `any()` when no serial was
    ///   passed refuses where two probes are attached, instead of taking the one that was named.
    ///   `any()` does NOT resolve the variable, and a comment claiming otherwise cannot fail the
    ///   way a call can.
    ///
    /// An EMPTY or whitespace-only `requested` falls through to the environment rather than
    /// matching a probe whose serial is blank: it is what an unset shell variable expands to on a
    /// command line, so treating it as a name would turn a typo into a selection.
    #[must_use]
    pub fn named_or_environment(requested: Option<&str>) -> Self {
        Self::named_or(requested, Self::from_environment())
    }

    /// The DECISION half of [`Selector::named_or_environment`], with the environment's answer
    /// supplied as a VALUE.
    ///
    /// Split out for the same reason [`choose`] is: the rung that matters is the FALLBACK, and a
    /// test can only reach it by supplying what it falls back to. Reading a process-global variable
    /// inside the decision would make the one case worth pinning -- that a blank name does not
    /// shadow a named probe -- unreachable without mutating the environment of every other test in
    /// the binary.
    #[must_use]
    pub fn named_or(requested: Option<&str>, environment: Selector) -> Self {
        match requested {
            Some(serial) if !serial.trim().is_empty() => Self::by_serial(serial.trim()),
            _ => environment,
        }
    }

    /// Matches the probe with this serial number.
    #[must_use]
    pub fn by_serial(serial: impl Into<String>) -> Self {
        Self { serial: Some(serial.into()), ..Self::default() }
    }

    /// Matches probes with this vendor and product id.
    #[must_use]
    pub fn by_vid_pid(vendor_id: u16, product_id: u16) -> Self {
        Self { vendor_id: Some(vendor_id), product_id: Some(product_id), ..Self::default() }
    }

    /// Adds a serial-number constraint (builder style).
    #[must_use]
    pub fn with_serial(mut self, serial: impl Into<String>) -> Self {
        self.serial = Some(serial.into());
        self
    }

    /// Adds a vendor/product constraint (builder style), narrowing a serial or environment selector
    /// to one probe FAMILY -- so "the sole attached probe" means the sole probe of that model
    /// rather than the sole probe of any kind on a bench that holds several.
    #[must_use]
    pub fn with_vid_pid(mut self, vendor_id: u16, product_id: u16) -> Self {
        self.vendor_id = Some(vendor_id);
        self.product_id = Some(product_id);
        self
    }

    /// Whether `candidate` satisfies every constraint this selector carries.
    ///
    /// **THE SERIAL IS COMPARED CASE-INSENSITIVELY.** The same serial reaches us in either case
    /// depending on which layer reported it -- a micro:bit's DAPLink presents a CMSIS-DAP v1 HID
    /// interface AND a WebUSB v2 bulk one, and the HID side spells it lowercase where the bulk side
    /// spells it uppercase.
    ///
    /// **A CASE-SENSITIVE FILTER HERE REFUSES A BOARD THAT IS SITTING THERE ON ITS OWN**, and it
    /// refuses it in a way the operator cannot fix: they named the probe correctly, in the case one
    /// backend reports, and the ladder answers `NotFound`. A refusal that names no remedy reads as
    /// broken hardware, which is worse than the ambiguity [`distinct`] guards against.
    #[must_use]
    pub fn matches(&self, candidate: &Candidate) -> bool {
        self.vendor_id.is_none_or(|v| v == candidate.vendor_id)
            && self.product_id.is_none_or(|p| p == candidate.product_id)
            && self.serial.as_deref().is_none_or(|s| {
                candidate
                    .serial
                    .as_deref()
                    .is_some_and(|found| found.eq_ignore_ascii_case(s))
            })
    }
}

/// What the ladder decided.
///
/// Deliberately NOT an error type. Each probe family already has one, they do not agree, and
/// merging them would drag one family's failure modes into every other. A caller maps this to its
/// own error in one line and keeps its public surface unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Exactly one physical probe matched.
    ///
    /// Carries its serial when the OS reported one. **`None` means the sole match is UNNAMED**,
    /// which a caller opening by vendor/product can still act on and a caller that must name the
    /// board cannot -- so the distinction is preserved here rather than collapsed into a refusal
    /// that would be wrong for one of them.
    Unique(Option<String>),
    /// Nothing matched the selector.
    NotFound,
    /// More than one PHYSICAL probe matched, so which board was meant is not decidable.
    ///
    /// **Refusing is the whole point: the alternative is a successful write to somebody else's
    /// target.** Carries a name per distinct probe, because the fix is to name one and the message
    /// should not make the user go and look them up.
    Ambiguous(Vec<String>),
}

/// Applies the ladder to an enumerated candidate set.
///
/// Split from every caller's discovery code because **the rung that matters -- refusing when
/// several boards match -- is the one a test can only reach by supplying the probes**, and a
/// selection rule that has never been shown to REFUSE has not been shown to do its job. A real
/// bench cannot be relied on to hold two identical probes on the day the test runs.
#[must_use]
pub fn choose(candidates: &[Candidate], selector: &Selector) -> Selection {
    let matched: Vec<&Candidate> = candidates.iter().filter(|c| selector.matches(c)).collect();
    match distinct(&matched).as_slice() {
        [] => Selection::NotFound,
        [single] => Selection::Unique(single.serial.clone()),
        several => Selection::Ambiguous(several.iter().map(|c| name_of(c)).collect()),
    }
}

/// The distinct physical probes among `candidates`, named the way a refusal names them.
///
/// Public because listing what is attached and refusing to choose between them are the same
/// question asked twice, and answering it twice is how the two drifted apart before.
#[must_use]
pub fn distinct_names(candidates: &[Candidate]) -> Vec<String> {
    let all: Vec<&Candidate> = candidates.iter().collect();
    distinct(&all).iter().map(|c| name_of(c)).collect()
}

/// The first interface of each distinct physical probe in a matched set.
///
/// A composite device presents several interfaces sharing one vid/pid/serial; counting interfaces
/// would call one attached probe ambiguous and refuse it.
fn distinct<'a>(matched: &[&'a Candidate]) -> Vec<&'a Candidate> {
    let mut seen: Vec<(u16, u16, Option<String>)> = Vec::new();
    let mut out = Vec::new();
    for candidate in matched {
        let identity = (
            candidate.vendor_id,
            candidate.product_id,
            candidate.serial.as_ref().map(|serial| serial.to_ascii_uppercase()),
        );
        if seen.contains(&identity) {
            continue;
        }
        seen.push(identity);
        out.push(*candidate);
    }
    out
}

/// How a probe is named in a refusal.
///
/// An unnamed one still has to appear, or the count in the message would not match the list under
/// it -- and it is named by vendor and product so the reader can at least tell WHICH KIND of probe
/// they have two of.
fn name_of(candidate: &Candidate) -> String {
    match candidate.serial.as_deref() {
        Some(serial) => String::from(serial),
        None => format!("{:04x}:{:04x} (no serial)", candidate.vendor_id, candidate.product_id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(vendor_id: u16, product_id: u16, serial: Option<&str>) -> Candidate {
        Candidate { vendor_id, product_id, serial: serial.map(String::from) }
    }

    fn two_alike() -> Vec<Candidate> {
        vec![probe(0x2e8a, 0x000c, Some("AAAA1111")), probe(0x2e8a, 0x000c, Some("BBBB2222"))]
    }

    /// The three rungs, and the ORDER, which is what every hand-written copy of this got wrong in
    /// one of two directions.
    ///
    /// A baked-in default is a name, so it beat the environment and sent four tools to a probe
    /// nobody had asked for. `Selector::any()` is no name at all, so it SKIPPED the environment and
    /// refused a two-probe bench that had already named one. Neither failure is visible at the call
    /// site; both are visible here.
    #[test]
    fn a_named_probe_beats_the_environment_and_no_name_falls_through_to_it() {
        let from_env = Selector::by_serial("FROM-THE-ENVIRONMENT");

        let named = Selector::named_or(Some("ON-THE-COMMAND-LINE"), from_env.clone());
        assert_eq!(
            named.serial.as_deref(),
            Some("ON-THE-COMMAND-LINE"),
            "an explicit name is the top rung -- it is the operator saying which board"
        );

        let unnamed = Selector::named_or(None, from_env.clone());
        assert_eq!(
            unnamed.serial.as_deref(),
            Some("FROM-THE-ENVIRONMENT"),
            "no name means the environment, NOT `any()` -- this is the rung `any()` skipped"
        );

        let nothing = Selector::named_or(None, Selector::any());
        assert!(nothing.serial.is_none() && nothing.vendor_id.is_none());
    }

    /// An EMPTY name is not a name. `--probe "$LAMELLA_PROBE_SERIAL"` with the variable unset
    /// expands to one empty argument, and taking that as a serial would look for a probe whose
    /// serial is the empty string -- a refusal whose message names a probe the operator never
    /// mentioned, produced by the shell rather than by anything they typed.
    #[test]
    fn a_blank_name_does_not_shadow_the_environment() {
        let from_env = Selector::by_serial("FROM-THE-ENVIRONMENT");
        for blank in ["", "   ", "\t"] {
            assert_eq!(
                Selector::named_or(Some(blank), from_env.clone()).serial.as_deref(),
                Some("FROM-THE-ENVIRONMENT"),
                "{blank:?} is not a probe name"
            );
        }
        assert_eq!(
            Selector::named_or(Some("  MC0201  "), from_env).serial.as_deref(),
            Some("MC0201")
        );
    }

    /// The ladder must SURVIVE a family narrowing, because that is how it is actually called: a
    /// tool that knows it wants an EDBG builds the selector and then adds the vendor and product
    /// ids. If the narrowing replaced the serial, "the sole attached probe of that model" would
    /// quietly become "any probe of that model", which may be several boards.
    #[test]
    fn narrowing_to_a_family_keeps_the_rung_the_ladder_chose() {
        let picked = Selector::named_or(Some("MC0201"), Selector::any()).with_vid_pid(0x03eb, 0x2175);
        assert_eq!(picked.serial.as_deref(), Some("MC0201"));
        assert_eq!((picked.vendor_id, picked.product_id), (Some(0x03eb), Some(0x2175)));

        let one_of_two = two_alike();
        let candidates: Vec<Candidate> = one_of_two.to_vec();
        let selector = Selector::named_or(Some("BBBB2222"), Selector::any())
            .with_vid_pid(0x2e8a, 0x000c);
        assert_eq!(
            choose(&candidates, &selector),
            Selection::Unique(Some("BBBB2222".to_string())),
            "and the whole point: it picks ONE of two identical probes"
        );
    }

    #[test]
    fn a_sole_probe_needs_no_serial() {
        let one = vec![probe(0x0483, 0x3748, Some("CCCC3333"))];
        assert_eq!(
            choose(&one, &Selector::any()),
            Selection::Unique(Some(String::from("CCCC3333")))
        );
    }

    #[test]
    fn several_alike_are_refused_rather_than_guessed_between() {
        let Selection::Ambiguous(names) = choose(&two_alike(), &Selector::any()) else {
            panic!("two identical probes must not resolve to one of them");
        };
        assert_eq!(names.len(), 2, "a refusal names every candidate");
        assert!(names.iter().any(|n| n == "AAAA1111"));
        assert!(names.iter().any(|n| n == "BBBB2222"));
    }

    #[test]
    fn a_serial_picks_one_out_of_several_alike() {
        assert_eq!(
            choose(&two_alike(), &Selector::by_serial("BBBB2222")),
            Selection::Unique(Some(String::from("BBBB2222")))
        );
    }

    #[test]
    fn a_serial_matching_nothing_refuses_rather_than_taking_another_board() {
        assert_eq!(choose(&two_alike(), &Selector::by_serial("DDDD4444")), Selection::NotFound);
    }

    #[test]
    fn the_sole_probe_means_the_sole_probe_of_that_family() {
        let mixed = vec![probe(0x2e8a, 0x000c, Some("AAAA1111")), probe(0x0483, 0x3748, None)];
        assert_eq!(
            choose(&mixed, &Selector::any().with_vid_pid(0x2e8a, 0x000c)),
            Selection::Unique(Some(String::from("AAAA1111")))
        );
    }

    #[test]
    fn one_composite_probe_is_not_several_probes() {
        let composite = vec![
            probe(0x0483, 0x374b, Some("EEEE5555")),
            probe(0x0483, 0x374b, Some("EEEE5555")),
            probe(0x0483, 0x374b, Some("EEEE5555")),
        ];
        assert_eq!(
            choose(&composite, &Selector::any()),
            Selection::Unique(Some(String::from("EEEE5555")))
        );
    }

    #[test]
    fn a_sole_unnamed_probe_resolves_without_a_name() {
        let one = vec![probe(0x0483, 0x3748, None)];
        assert_eq!(choose(&one, &Selector::any()), Selection::Unique(None));
    }

    #[test]
    fn two_unnamed_probes_alike_are_still_refused() {
        let two = vec![probe(0x0483, 0x3748, None), probe(0x0483, 0x3749, None)];
        let Selection::Ambiguous(names) = choose(&two, &Selector::any()) else {
            panic!("two unnamed probes must not resolve");
        };
        assert_eq!(names, ["0483:3748 (no serial)", "0483:3749 (no serial)"]);
    }

    #[test]
    fn listing_and_refusing_agree_on_what_is_distinct() {
        let mut probes = two_alike();
        probes.push(probe(0x2e8a, 0x000c, Some("AAAA1111")));
        let Selection::Ambiguous(refused) = choose(&probes, &Selector::any()) else {
            panic!("two distinct probes must not resolve");
        };
        assert_eq!(refused, distinct_names(&probes));
        assert_eq!(refused.len(), 2, "the repeated interface is one probe, not two");
    }

    #[test]
    fn no_attached_probe_is_not_found_rather_than_ambiguous() {
        assert_eq!(choose(&[], &Selector::any()), Selection::NotFound);
    }

    #[test]
    fn a_serial_named_in_either_case_selects_the_probe() {
        let attached = [probe(0x0d28, 0x0204, Some("9901000052284E45"))];

        for named in ["9901000052284E45", "9901000052284e45", "9901000052284E45".to_lowercase().as_str()] {
            assert_eq!(
                choose(&attached, &Selector::by_serial(named)),
                Selection::Unique(Some(String::from("9901000052284E45"))),
                "naming {named} must select the attached probe"
            );
        }

        assert_eq!(
            choose(&attached, &Selector::by_serial("9901000052284E46")),
            Selection::NotFound,
            "a different serial must still be refused"
        );
    }

    #[test]
    fn one_probe_reported_in_two_cases_is_one_probe() {
        let attached = [
            probe(0x0d28, 0x0204, Some("9901000052284e45")),
            probe(0x0d28, 0x0204, Some("9901000052284E45")),
        ];
        assert_eq!(
            choose(&attached, &Selector::any()),
            Selection::Unique(Some("9901000052284e45".to_owned())),
            "one physical probe, however its backends spelled the serial"
        );
        let two = [
            probe(0x0d28, 0x0204, Some("9901000052284e45")),
            probe(0x0d28, 0x0204, Some("9906000052284e45")),
        ];
        assert!(matches!(choose(&two, &Selector::any()), Selection::Ambiguous(names) if names.len() == 2));
    }
}
