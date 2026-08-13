//! Is the attached board the board this image was built for -- compared against board truth, with
//! the profile it assumed carried IN the answer.

use crate::strata::{BoardTable, DeviceTable, Discriminator, PartRow, SOURCING_VALIDATION};
use alloc_free::*;

mod alloc_free {
    pub use std::string::String;
    pub use std::vec::Vec;
}

/// One reading taken from an attached board: which declared discriminator was run, and what came
/// back.
///
/// Producing these is the wire half of the check and belongs to whatever drives the board. This
/// module consumes them, which is the seam: the comparison has no opinion about how a number was
/// obtained, and gains no hardware dependency by being complete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    /// The `[[discriminators]] name` this reading came from.
    pub discriminator: String,
    /// What the board answered.
    pub reading: i64,
}

/// A module somebody says is attached: what they call it, where they say it sits, and the part
/// table that describes what should be there.
///
/// SUPPLIED PER INVOCATION RATHER THAN READ FROM A BOARD FILE, and that is the whole point. A
/// module on a cable is not a property of the board -- the same board has different residents on
/// different days, on different desks, in different orders -- so a board file cannot state one and
/// a table of them would be wrong for every unit that had something else plugged in. It arrives
/// the way an image size arrives at a fit verdict: as an argument.
#[derive(Debug, Clone, Copy)]
pub struct Attached<'a> {
    /// What the caller calls this module. Also the key an observation names.
    pub name: &'a str,
    /// The 7-bit address it is claimed to answer at.
    pub address: i64,
    /// The part table describing what should be there.
    pub part: &'a DeviceTable,
}

/// One thing the image took as given, which reconciliation either reaches or reports unreached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// The board is built around this part.
    Part {
        /// The part id the board file names.
        part: String,
    },
    /// This region is fitted, at the accessible size the board declares.
    Region {
        /// The region id.
        name: String,
        /// The accessible size claimed, in bytes.
        bytes: i64,
    },
    /// A module of this part type is attached at this address.
    Device {
        /// What the caller calls it.
        name: String,
        /// Where it is claimed to sit.
        address: i64,
        /// The part id that should be there.
        part: String,
    },
}

impl Claim {
    /// The `confirms` target a discriminator must name to reach this claim.
    #[must_use]
    pub fn target(&self) -> String {
        match self {
            Claim::Part { .. } => String::from("part"),
            Claim::Region { name, .. } => format!("memory:{name}"),
            Claim::Device { name, .. } => format!("device:{name}"),
        }
    }

    /// The weakest rung that can establish this claim.
    ///
    /// A part answers its own identity register, and that is what identity registers are for. A
    /// region's ACCESSIBLE size is not a property of the fitted device, so no read that only
    /// identifies the device reaches it.
    #[must_use]
    pub fn requires(&self) -> &'static str {
        match self {
            Claim::Part { .. } => "identified",
            Claim::Region { .. } => "exercised",
            Claim::Device { .. } => "identified",
        }
    }
}

/// Where one claim stands after the readings are applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClaimStatus {
    /// A discriminator that reaches this claim was run and answered what the board declares.
    Confirmed {
        /// The discriminator that settled it.
        by: String,
        /// The rung it was settled at.
        rung: String,
    },
    /// A discriminator that reaches this claim answered something else. The attached board is not
    /// the board the image assumed.
    Contradicted {
        /// The discriminator that disagreed.
        by: String,
        /// What the board file declares.
        expected: i64,
        /// What came back.
        read: i64,
    },
    /// Nothing reached the claim, and why not -- an absent discriminator, an unrun one, or one
    /// whose rung is below what the claim needs.
    Unconfirmed {
        /// Named so a reader can go and close it.
        why: String,
    },
}

/// One claim and where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimReport {
    /// What was assumed.
    pub claim: Claim,
    /// Whether the board bore it out.
    pub status: ClaimStatus,
}

/// The answer, ranked by its weakest claim.
///
/// The order is deliberate: a contradiction outranks an absence, because they call for opposite
/// responses. A contradicted claim means the wrong board is attached and work should stop; an
/// unconfirmed one means nobody has stated a way to check, and work may proceed knowing that.
/// Collapsing them would make "we did not look" read like "we looked and it was fine".
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A reading disagreed with board truth.
    Contradicted,
    /// Nothing disagreed, and at least one claim was not reached.
    Unconfirmed,
    /// Every claim was reached, at the rung it requires.
    Confirmed,
}

/// A reconciliation answer, with everything it assumed and everything it cannot speak to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileVerdict {
    /// The board id the verdict is about.
    pub board: String,
    /// The part id the board is built around.
    pub part: String,
    /// Every claim in the profile, in a fixed order, each with where it stands. This IS the
    /// profile being named: a verdict that reported a rank and not the claims behind it would
    /// leave a reader unable to tell which assumption went unchecked.
    pub profile: Vec<ClaimReport>,
    /// The verdict, which is the weakest status in `profile`.
    pub outcome: Outcome,
    /// What the comparison took as given, in the terms the board file states them.
    pub assumptions: Vec<String>,
    /// Readings naming a discriminator this board does not declare. Reported rather than dropped:
    /// a check that silently ignores a reading it cannot place reports on a population nobody
    /// stated.
    pub unplaced: Vec<String>,
    /// What reconciliation structurally cannot answer, so `Confirmed` is never read as
    /// `everything is fine`.
    pub not_answered: Vec<String>,
}

impl ReconcileVerdict {
    /// The claims nothing reached, in profile order.
    #[must_use]
    pub fn unconfirmed(&self) -> Vec<&ClaimReport> {
        self.profile
            .iter()
            .filter(|report| matches!(report.status, ClaimStatus::Unconfirmed { .. }))
            .collect()
    }
}

/// The rank of a validation rung within the one ladder, so nothing here builds a second ordering.
fn rank(rung: &str) -> Option<usize> {
    SOURCING_VALIDATION.iter().position(|known| *known == rung)
}

/// Whether `rung` is at least `needed` on that ladder. An unknown rung satisfies nothing, which is
/// the safe direction: the parser refuses one, and a value that reached here anyway must not be
/// read as strength.
fn reaches(rung: &str, needed: &str) -> bool {
    match (rank(rung), rank(needed)) {
        (Some(have), Some(want)) => have >= want,
        _ => false,
    }
}

/// The claims an image built for `board` takes as given.
///
/// Every declared region is a claim, including the ones [`crate::fit`] deliberately excludes from
/// the code budget. That is not an inconsistency between the two verdicts: a region behind a
/// controller cannot hold a program laid down by a linker script, which is why fit refuses to
/// count it -- and it is exactly the memory a program allocates into at run time, which is why
/// reconciliation must not skip it. The motivating failure is about a region fit declines to
/// count.
#[must_use]
pub fn profile_of(board: &BoardTable, part: &PartRow) -> Vec<Claim> {
    let mut claims = vec![Claim::Part { part: part.part.clone() }];
    for region in &board.memory {
        claims.push(Claim::Region { name: region.name.clone(), bytes: region.size });
    }
    claims
}

/// Settles an attached-module claim against its part table's identity and the reading taken.
///
/// THE DISCRIMINATOR IS THE PART'S OWN `[identity]` AND NEEDS NO AUTHORING -- a BME280 answers the
/// same value at the same register on every carrier that ever held one, so a board restating it
/// would be a second copy of a chip fact. The reading is keyed by the caller's name for the module.
///
/// A PART WITH NO IDENTITY REGISTER CANNOT BE SETTLED HERE AT ALL, and that is the case this arm
/// exists to report rather than paper over. Some parts carry no readable id -- a sensor whose whole
/// interface is a measurement request and a fetch has nothing to answer -- so presence and type are
/// not confirmable by reading, and only exercising the part can establish them.
fn settle_device(attached: &Attached<'_>, observed: &[Observation]) -> ClaimStatus {
    let Some(identity) = attached.part.identity.as_ref() else {
        return ClaimStatus::Unconfirmed {
            why: format!(
                "part '{}' states no identity at all, so nothing describes what a reading should be",
                attached.part.part
            ),
        };
    };
    if !identity.absent.is_empty() {
        return ClaimStatus::Unconfirmed {
            why: format!(
                "part '{}' HAS no identity register, so presence and type cannot be confirmed by reading -- only by exercising it: {}",
                attached.part.part, identity.absent
            ),
        };
    }
    let Some(observation) = observed.iter().find(|o| o.discriminator == attached.name) else {
        return ClaimStatus::Unconfirmed {
            why: format!(
                "no reading was supplied for '{}' -- its identity register 0x{:X} was not read",
                attached.name, identity.reg.value
            ),
        };
    };
    if identity.values.iter().any(|v| v.value == observation.reading) {
        ClaimStatus::Confirmed {
            by: format!("{}'s identity register 0x{:X}", attached.part.part, identity.reg.value),
            rung: String::from("identified"),
        }
    } else {
        ClaimStatus::Contradicted {
            by: format!("{}'s identity register 0x{:X}", attached.part.part, identity.reg.value),
            expected: identity.values.first().map_or(-1, |v| v.value),
            read: observation.reading,
        }
    }
}

/// Settles one claim against the board's declared discriminators and the readings taken.
fn settle(claim: &Claim, board: &BoardTable, observed: &[Observation]) -> ClaimStatus {
    let target = claim.target();
    let needed = claim.requires();
    let reaching: Vec<&Discriminator> =
        board.discriminators.iter().filter(|d| d.confirms == target).collect();

    if reaching.is_empty() {
        return ClaimStatus::Unconfirmed {
            why: format!("board '{}' declares no discriminator that confirms '{target}'", board.board),
        };
    }

    let mut ran = 0;
    for discriminator in &reaching {
        let Some(observation) = observed.iter().find(|o| o.discriminator == discriminator.name)
        else {
            continue;
        };
        ran += 1;
        if observation.reading != discriminator.expect {
            return ClaimStatus::Contradicted {
                by: discriminator.name.clone(),
                expected: discriminator.expect,
                read: observation.reading,
            };
        }
        if reaches(&discriminator.validation, needed) {
            return ClaimStatus::Confirmed {
                by: discriminator.name.clone(),
                rung: discriminator.validation.clone(),
            };
        }
    }

    let best = reaching
        .iter()
        .filter_map(|d| rank(&d.validation).map(|r| (r, d.validation.clone())))
        .max_by_key(|(r, _)| *r);
    match best {
        Some((_, rung)) if !reaches(&rung, needed) => ClaimStatus::Unconfirmed {
            why: format!(
                "the strongest discriminator for '{target}' is declared '{rung}', and this claim needs '{needed}' -- an identity read reports the fitted device, not what a program can reach of it"
            ),
        },
        _ if ran == 0 => ClaimStatus::Unconfirmed {
            why: format!(
                "a discriminator for '{target}' is declared but was not run -- no reading was supplied for it"
            ),
        },
        _ => ClaimStatus::Unconfirmed {
            why: format!("nothing supplied settled '{target}'"),
        },
    }
}

/// Compares what `board` declares against what an attached board answered.
///
/// Pure: no file IO, no hardware, no network. `observed` empty is the hardware-free case and is
/// answered rather than refused -- the verdict then names every claim it could not reach, which is
/// the honest answer to `what would attaching this board tell me`.
#[must_use]
pub fn reconcile(
    board: &BoardTable,
    part: &PartRow,
    attached: &[Attached<'_>],
    observed: &[Observation],
) -> ReconcileVerdict {
    let mut profile: Vec<ClaimReport> = profile_of(board, part)
        .into_iter()
        .map(|claim| {
            let status = settle(&claim, board, observed);
            ClaimReport { claim, status }
        })
        .collect();
    for module in attached {
        profile.push(ClaimReport {
            claim: Claim::Device {
                name: String::from(module.name),
                address: module.address,
                part: module.part.part.clone(),
            },
            status: settle_device(module, observed),
        });
    }

    let outcome = if profile.iter().any(|r| matches!(r.status, ClaimStatus::Contradicted { .. })) {
        Outcome::Contradicted
    } else if profile.iter().any(|r| matches!(r.status, ClaimStatus::Unconfirmed { .. })) {
        Outcome::Unconfirmed
    } else {
        Outcome::Confirmed
    };

    let mut assumptions = vec![format!(
        "the image was built for board '{}' around part '{}'",
        board.board, part.part
    )];
    for region in &board.memory {
        let fitted = if region.device_size >= 0 && region.device_size != region.size {
            format!(
                " (a {}-byte device, of which this board wires {} reachable)",
                region.device_size, region.size
            )
        } else {
            String::new()
        };
        assumptions.push(format!(
            "region '{}' is fitted and {} bytes of it are reachable{fitted}",
            region.name, region.size
        ));
    }

    let unplaced = observed
        .iter()
        .filter(|o| {
            !board.discriminators.iter().any(|d| d.name == o.discriminator)
                && !attached.iter().any(|m| m.name == o.discriminator)
        })
        .map(|o| {
            format!(
                "reading {} named discriminator '{}', which board '{}' does not declare",
                o.reading, o.discriminator, board.board
            )
        })
        .collect();

    ReconcileVerdict {
        board: board.board.clone(),
        part: part.part.clone(),
        profile,
        outcome,
        assumptions,
        unplaced,
        not_answered: vec![
            String::from(
                "ANY CLAIM WITH NO DISCRIMINATOR: this compares declared reads against readings, \
                 and cannot invent a read nobody stated. An unreached claim is reported, never \
                 assumed",
            ),
            String::from(
                "WHETHER A CONFIRMED REGION KEEPS ANSWERING: a read establishes that memory \
                 answered once, not that it holds across temperature, retention or marginal \
                 timing",
            ),
            String::from(
                "WHETHER THE IMAGE IS CORRECT: this compares a board against the profile an image \
                 assumed, and says nothing about the program",
            ),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strata::MemoryRegion;

    fn region(name: &str, size: i64, device_size: i64) -> MemoryRegion {
        MemoryRegion {
            name: String::from(name),
            kind: String::from("ram"),
            base: -1,
            size,
            device_size,
            ..MemoryRegion::default()
        }
    }

    fn discriminator(name: &str, confirms: &str, validation: &str, expect: i64) -> Discriminator {
        Discriminator {
            name: String::from(name),
            confirms: String::from(confirms),
            reads: String::from("a read"),
            validation: String::from(validation),
            expect,
            source: String::from("a document"),
        }
    }

    fn part() -> PartRow {
        PartRow { part: String::from("stm32f746ng"), ..PartRow::default() }
    }

    /// A board with an 8 MB region and whatever discriminators a case needs.
    fn board(discriminators: Vec<Discriminator>) -> BoardTable {
        BoardTable {
            board: String::from("stm32f746g-disco"),
            memory: vec![region("sdram", 0x0080_0000, 0x0100_0000)],
            discriminators,
            ..BoardTable::default()
        }
    }

    fn saw(name: &str, reading: i64) -> Observation {
        Observation { discriminator: String::from(name), reading }
    }

    /// THE CASE THE CHECK EXISTS FOR, AND THE ONE AN IDENTITY-ONLY IMPLEMENTATION PASSES. The
    /// chip answers its identity correctly, because a bare board and a populated one carry the
    /// same part. The region claim must not be confirmed by that.
    #[test]
    fn a_correct_chip_identity_does_not_confirm_that_anything_was_soldered_on() {
        let board = board(vec![discriminator("part-id", "part", "identified", 0x0451)]);
        let verdict = reconcile(&board, &part(), &[], &[saw("part-id", 0x0451)]);

        assert_eq!(verdict.outcome, Outcome::Unconfirmed, "{:#?}", verdict.profile);
        let unreached = verdict.unconfirmed();
        assert_eq!(unreached.len(), 1, "exactly the region claim: {unreached:#?}");
        assert_eq!(
            unreached[0].claim,
            Claim::Region { name: String::from("sdram"), bytes: 0x0080_0000 }
        );
    }

    /// The rung barrier, independent of the target barrier. Even a discriminator that DOES name
    /// the region cannot confirm its accessible size at `identified`, because an identity read
    /// reports the device and this board reaches half of it.
    #[test]
    fn an_identified_rung_cannot_confirm_a_regions_accessible_size() {
        let board = board(vec![discriminator("sdram-id", "memory:sdram", "identified", 7)]);
        let verdict = reconcile(&board, &part(), &[], &[saw("sdram-id", 7)]);

        assert_eq!(verdict.outcome, Outcome::Unconfirmed);
        let why = match &verdict.profile[1].status {
            ClaimStatus::Unconfirmed { why } => why.clone(),
            other => panic!("the region claim must not be confirmed at identified: {other:?}"),
        };
        assert!(why.contains("exercised"), "and it names the rung it needed: {why}");
    }

    /// The strongest verdict is reachable, and only this way: a discriminator that names the
    /// region AND is declared at `exercised`, with a reading that matches.
    #[test]
    fn a_probing_discriminator_at_the_exercised_rung_confirms_the_region() {
        let board = board(vec![
            discriminator("part-id", "part", "identified", 0x0451),
            discriminator("sdram-sweep", "memory:sdram", "exercised", 0x0080_0000),
        ]);
        let verdict = reconcile(
            &board,
            &part(),
            &[],
            &[saw("part-id", 0x0451), saw("sdram-sweep", 0x0080_0000)],
        );

        assert_eq!(verdict.outcome, Outcome::Confirmed, "{:#?}", verdict.profile);
        assert!(verdict.unconfirmed().is_empty());
    }

    /// A bare board answering zero where eight megabytes were claimed is CONTRADICTED, not
    /// unconfirmed. This is the difference between `stop` and `proceed knowing less`.
    #[test]
    fn a_bare_board_answering_the_wrong_size_contradicts_rather_than_going_unconfirmed() {
        let board = board(vec![discriminator("sdram-sweep", "memory:sdram", "exercised", 0x0080_0000)]);
        let verdict = reconcile(&board, &part(), &[], &[saw("sdram-sweep", 0)]);

        assert_eq!(verdict.outcome, Outcome::Contradicted);
        assert_eq!(
            verdict.profile[1].status,
            ClaimStatus::Contradicted {
                by: String::from("sdram-sweep"),
                expected: 0x0080_0000,
                read: 0
            }
        );
    }

    /// The hardware-free case is an ordinary input. With no readings at all the verdict still
    /// answers, naming every claim and distinguishing `declared but not run` from `nothing
    /// declared`.
    #[test]
    fn with_no_readings_at_all_it_still_answers_and_names_every_claim() {
        let board = board(vec![discriminator("sdram-sweep", "memory:sdram", "exercised", 0x0080_0000)]);
        let verdict = reconcile(&board, &part(), &[], &[]);

        assert_eq!(verdict.outcome, Outcome::Unconfirmed);
        assert_eq!(verdict.unconfirmed().len(), 2, "both claims: {:#?}", verdict.profile);
        let part_why = match &verdict.profile[0].status {
            ClaimStatus::Unconfirmed { why } => why.clone(),
            other => panic!("{other:?}"),
        };
        let region_why = match &verdict.profile[1].status {
            ClaimStatus::Unconfirmed { why } => why.clone(),
            other => panic!("{other:?}"),
        };
        assert!(part_why.contains("declares no discriminator"), "{part_why}");
        assert!(region_why.contains("was not run"), "{region_why}");
    }

    /// A verdict names the profile it compared against, including the size split that makes an
    /// identity read insufficient in the first place.
    #[test]
    fn a_verdict_states_the_profile_it_assumed_and_what_it_cannot_answer() {
        let verdict = reconcile(&board(Vec::new()), &part(), &[], &[]);
        assert!(
            verdict.assumptions.iter().any(|line| line.contains("stm32f746ng")),
            "{:?}",
            verdict.assumptions
        );
        assert!(
            verdict
                .assumptions
                .iter()
                .any(|line| line.contains("8388608 bytes") && line.contains("16777216-byte device")),
            "the reachable and fitted sizes are both named: {:?}",
            verdict.assumptions
        );
        assert!(
            verdict.not_answered.iter().any(|line| line.contains("NO DISCRIMINATOR")),
            "{:?}",
            verdict.not_answered
        );
    }

    /// A part table with an identity, and one that states it has none -- the two shapes an
    /// attached module can have.
    fn part_table(id: &str, identity: Option<(i64, i64)>, absent: &str) -> crate::strata::DeviceTable {
        use crate::Int;
        use crate::strata::{DeviceIdentity, DeviceTable};
        crate::strata::DeviceTable {
            part: String::from(id),
            identity: Some(DeviceIdentity {
                reg: identity.map_or_else(Int::default, |(reg, _)| Int { value: reg, hex: true }),
                width: if identity.is_some() { 8 } else { 0 },
                values: identity
                    .map(|(_, v)| vec![Int { value: v, hex: true }])
                    .unwrap_or_default(),
                absent: String::from(absent),
            }),
            ..DeviceTable::default()
        }
    }

    /// AN ATTACHED MODULE IS CONFIRMED BY ITS PART'S OWN IDENTITY, with nothing authored on the
    /// board. A chip answers the same value at the same register on every carrier that ever held
    /// one, so a board restating it would be a second copy of a chip fact.
    #[test]
    fn an_attached_module_is_confirmed_by_its_parts_own_identity() {
        let table = part_table("lsm6dsox", Some((0x0F, 0x6C)), "");
        let module = Attached { name: "movement", address: 0x6A, part: &table };
        let verdict =
            reconcile(&board(Vec::new()), &part(), &[module], &[saw("movement", 0x6C)]);

        let report = verdict.profile.last().expect("the module claim");
        assert_eq!(
            report.claim,
            Claim::Device {
                name: String::from("movement"),
                address: 0x6A,
                part: String::from("lsm6dsox")
            }
        );
        assert!(
            matches!(report.status, ClaimStatus::Confirmed { ref rung, .. } if rung == "identified"),
            "{:?}",
            report.status
        );
    }

    /// A reading that is not in the accepted set CONTRADICTS rather than merely failing to
    /// confirm: something answered that address, and it was not this part.
    #[test]
    fn a_module_answering_another_parts_identity_contradicts() {
        let table = part_table("lsm6dsox", Some((0x0F, 0x6C)), "");
        let module = Attached { name: "movement", address: 0x6A, part: &table };
        let verdict =
            reconcile(&board(Vec::new()), &part(), &[module], &[saw("movement", 0xD3)]);

        assert_eq!(verdict.outcome, Outcome::Contradicted);
        assert!(matches!(
            verdict.profile.last().expect("the module claim").status,
            ClaimStatus::Contradicted { expected: 0x6C, read: 0xD3, .. }
        ));
    }

    /// THE CASE THE PARTS SCHEMA HAD TO GROW A SPELLING FOR. A part with no identity register has
    /// nothing to answer, so its presence and type cannot be confirmed by reading at all -- and the
    /// verdict says which part and why, rather than reporting an ordinary missing reading.
    #[test]
    fn a_part_with_no_identity_register_cannot_be_confirmed_by_reading() {
        let table = part_table("hs3003", None, "the datasheet describes no identity register");
        let module = Attached { name: "thermo", address: 0x44, part: &table };
        let verdict =
            reconcile(&board(Vec::new()), &part(), &[module], &[saw("thermo", 0x60)]);

        let why = match &verdict.profile.last().expect("the module claim").status {
            ClaimStatus::Unconfirmed { why } => why.clone(),
            other => panic!("a part with no identity must not be confirmed by a read: {other:?}"),
        };
        assert!(why.contains("hs3003"), "names the part: {why}");
        assert!(why.contains("exercising"), "and names the only way left: {why}");
    }

    /// A reading nobody can place is reported. Dropping it would let a supplier run a
    /// discriminator this board never declared and read the silence as agreement.
    #[test]
    fn a_reading_naming_an_undeclared_discriminator_is_reported_not_dropped() {
        let verdict = reconcile(&board(Vec::new()), &part(), &[], &[saw("psram-jedec", 0x9D5D)]);
        assert_eq!(verdict.unplaced.len(), 1, "{:?}", verdict.unplaced);
        assert!(verdict.unplaced[0].contains("psram-jedec"));
    }
}
