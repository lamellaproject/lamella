//! Comparing what a host holds against what a target reports, and saying what differs.

use lamella_wire::{Surface, surface};

/// How a host's surface record relates to the one a target reported.
///
/// Ordered coarse to fine, and the order is the point: a different runtime is not a version
/// question, and an incompatible seam level is not a build question. Reporting either as "your
/// library is out of date" would send a reader to rebuild something that was never the problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceMatch {
    /// The same content. Nothing to report.
    Same,
    /// Different runtimes entirely -- a Python target and a C# host, say.
    DifferentTier {
        /// The tier the host holds.
        host: u8,
        /// The tier the target reported.
        target: u8,
    },
    /// The same runtime at seam levels that are not compatible.
    AbiDiffers {
        /// The host's ABI level.
        host: u16,
        /// The target's ABI level.
        target: u16,
    },
    /// Different contract levels, so the two are ORDERABLE and one of them is behind.
    ContractDiffers {
        /// The host's contract version.
        host: [u16; 4],
        /// The target's contract version.
        target: [u16; 4],
    },
    /// The same contract level built differently -- the ordinary case when anything differs at all.
    ///
    /// **A version states a COMPATIBILITY LEVEL rather than counting builds**, so it is stable for
    /// long stretches by design. A host reading it as a build counter would call every
    /// capability-symbol difference a match, which is why the content hash is what decides and the
    /// version only explains.
    BuildDiffers {
        /// The host's file version, which orders builds within one contract level.
        host: [u16; 4],
        /// The target's file version.
        target: [u16; 4],
    },
}

impl SurfaceMatch {
    /// Whether this difference should stop a host from proceeding.
    ///
    /// **A DIFFERENT BUILD IS NOT A REFUSAL.** Two builds of one contract level are compatible by
    /// the definition of a contract level; saying otherwise would refuse every board that had been
    /// reflashed since the host was built, which is most of them. What stops a deploy is a symbol
    /// the board cannot resolve, and [`surface_refusal`] is what answers that.
    #[must_use]
    pub fn is_blocking(self) -> bool {
        matches!(
            self,
            SurfaceMatch::DifferentTier { .. }
                | SurfaceMatch::AbiDiffers { .. }
                | SurfaceMatch::ContractDiffers { .. }
        )
    }

    /// The difference as a sentence, or `None` when there is none.
    #[must_use]
    pub fn describe(self) -> Option<String> {
        let version = |quad: [u16; 4]| {
            format!("{}.{}.{}.{}", quad[0], quad[1], quad[2], quad[3])
        };
        match self {
            SurfaceMatch::Same => None,
            SurfaceMatch::DifferentTier { host, target } => Some(format!(
                "this host speaks runtime tier {host} and the board is running tier {target}. \
                 These are different runtimes, not different versions of one -- the board needs \
                 firmware for the runtime you are targeting."
            )),
            SurfaceMatch::AbiDiffers { host, target } => Some(format!(
                "this host was built against ABI {host} and the board reports ABI {target}. An ABI \
                 level moves only when an existing seam changes meaning, so these two cannot be \
                 used together: rebuild whichever of the two is older."
            )),
            SurfaceMatch::ContractDiffers { host, target } => {
                let (older, which) = if host < target { (version(host), "this host") } else { (version(target), "the board") };
                Some(format!(
                    "this host holds library contract {} and the board reports {} -- {which} is the \
                     older at {older}. Rebuild it against the other, or reflash the board.",
                    version(host),
                    version(target)
                ))
            }
            SurfaceMatch::BuildDiffers { host, target } => Some(format!(
                "the board's library is the same contract level as this host's and a different \
                 build of it ({} here, {} there). That is ordinarily fine -- a contract level is \
                 what compatibility is defined against -- and it is reported because it explains \
                 why the two content hashes differ.",
                version(host),
                version(target)
            )),
        }
    }
}

/// Compare the surface a host holds against the one a target reported.
///
/// # The order of the checks is the design
///
/// Coarse to fine: **tier, then ABI, then the content hash, then the versions.** A different runtime
/// is not a version question and an incompatible seam is not a build question, so each is answered
/// before the finer one can mislabel it.
///
/// The hash decides whether anything differs at all -- it is the CONTENT fingerprint, folded with
/// the resident library's -- and the versions are consulted only to explain a difference the hash
/// already found. Doing it the other way round would call every capability-symbol difference a match
/// whenever the versions happened to agree.
#[must_use]
pub fn compare(host: &Surface, target: &Surface) -> SurfaceMatch {
    if host.tier != target.tier {
        return SurfaceMatch::DifferentTier { host: host.tier, target: target.tier };
    }
    if host.abi != target.abi {
        return SurfaceMatch::AbiDiffers { host: host.abi, target: target.abi };
    }
    if host.hash == target.hash {
        return SurfaceMatch::Same;
    }
    if host.lib_version != target.lib_version {
        return SurfaceMatch::ContractDiffers {
            host: host.lib_version,
            target: target.lib_version,
        };
    }
    SurfaceMatch::BuildDiffers {
        host: host.lib_file_version,
        target: target.lib_file_version,
    }
}

/// Why a board cannot resolve a program built against `program`, or `None` when it can.
///
/// # The refusal names the symbols, and says the check is conservative
///
/// [`lamella_wire::surface::accepts`] compares whole-library bitmaps, so **a program built against a
/// large surface that only touches a small part of it is refused on a board carrying the small
/// part, even though it would have run.** A message reading only *your program does not fit* would
/// therefore be a wrong answer for a program that would have worked -- and it is the one message a
/// reader has no way to check for themselves.
///
/// So the refusal says which symbols are absent and admits what the check cannot see. A reader who
/// knows their program never touches the named symbol then knows to look at the check rather than
/// at their code.
#[must_use]
pub fn surface_refusal(program: u64, board: u64) -> Option<String> {
    let absent = surface::missing(program, board);
    if absent == 0 {
        return None;
    }
    let mut names: Vec<String> = Vec::new();
    let mut unnamed = absent;
    for (bit, name) in surface::NAMED {
        if absent & bit != 0 {
            names.push((*name).to_string());
            unnamed &= !bit;
        }
    }
    for index in 0..u64::BITS {
        if unnamed & (1u64 << index) != 0 {
            names.push(format!("unknown surface bit {index}"));
        }
    }
    Some(format!(
        "the board's library does not carry {}, which the program's library was built against.\n\n\
         This check compares whole libraries rather than the symbols your program actually uses, \
         so it refuses a program that would have run if it never touches the ones named above. If \
         that is the case here, the board needs a library built with them rather than your program \
         needing a change.",
        names.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface_of(tier: u8, abi: u16, hash: u64, lib: [u16; 4], file: [u16; 4]) -> Surface {
        Surface { tier, abi, hash, lib_version: lib, lib_file_version: file, ..Surface::default() }
    }

    #[test]
    fn an_equal_hash_is_a_match_and_says_nothing() {
        let a = surface_of(1, 2, 0xABCD, [1, 0, 0, 0], [1, 0, 0, 7]);
        let b = surface_of(1, 2, 0xABCD, [1, 0, 0, 0], [1, 0, 0, 9]);
        assert_eq!(compare(&a, &b), SurfaceMatch::Same);
        assert_eq!(compare(&a, &b).describe(), None);
        assert!(!compare(&a, &b).is_blocking());
    }

    #[test]
    fn a_different_tier_is_not_reported_as_a_version_difference() {
        let host = surface_of(1, 2, 1, [1, 0, 0, 0], [1, 0, 0, 0]);
        let target = surface_of(2, 2, 9, [9, 0, 0, 0], [9, 0, 0, 0]);
        assert_eq!(compare(&host, &target), SurfaceMatch::DifferentTier { host: 1, target: 2 });
        let said = compare(&host, &target).describe().expect("a difference");
        assert!(said.contains("different runtimes"), "got {said}");
        assert!(compare(&host, &target).is_blocking());
    }

    #[test]
    fn an_abi_difference_is_not_reported_as_a_version_difference() {
        let host = surface_of(1, 2, 1, [1, 0, 0, 0], [1, 0, 0, 0]);
        let target = surface_of(1, 3, 9, [9, 0, 0, 0], [9, 0, 0, 0]);
        assert_eq!(compare(&host, &target), SurfaceMatch::AbiDiffers { host: 2, target: 3 });
        assert!(compare(&host, &target).is_blocking());
    }

    #[test]
    fn differing_contracts_are_ordered_and_the_older_side_is_named() {
        let host = surface_of(1, 2, 1, [1, 0, 0, 0], [1, 0, 0, 0]);
        let target = surface_of(1, 2, 2, [2, 0, 0, 0], [2, 0, 0, 0]);
        assert!(matches!(compare(&host, &target), SurfaceMatch::ContractDiffers { .. }));
        let said = compare(&host, &target).describe().expect("a difference");
        assert!(said.contains("this host is the older"), "the older side is named: {said}");
        let said_back = compare(&target, &host).describe().expect("a difference");
        assert!(said_back.contains("the board is the older"), "got {said_back}");
    }

    #[test]
    fn the_same_contract_built_differently_is_reported_and_does_not_block() {
        let host = surface_of(1, 2, 1, [1, 0, 0, 0], [1, 0, 0, 4]);
        let target = surface_of(1, 2, 2, [1, 0, 0, 0], [1, 0, 0, 5]);
        assert!(matches!(compare(&host, &target), SurfaceMatch::BuildDiffers { .. }));
        assert!(!compare(&host, &target).is_blocking());
        let said = compare(&host, &target).describe().expect("it is still reported");
        assert!(said.contains("ordinarily fine"), "got {said}");
    }

    #[test]
    fn a_board_that_carries_everything_refuses_nothing() {
        assert_eq!(surface_refusal(surface::FLOAT | surface::GC, surface::FLOAT | surface::GC), None);
        assert_eq!(surface_refusal(surface::FLOAT, surface::FLOAT | surface::GC), None);
    }

    #[test]
    fn a_refusal_names_the_missing_symbols_and_admits_the_check_is_coarse() {
        let said = surface_refusal(surface::FLOAT | surface::DECIMAL, surface::FLOAT)
            .expect("DECIMAL is absent");
        assert!(said.contains("LAMELLA_SURFACE_DECIMAL"), "the symbol is named: {said}");
        assert!(!said.contains("LAMELLA_SURFACE_FLOAT"), "and one it HAS is not: {said}");
        assert!(said.contains("whole libraries"), "and the check admits its own coarseness: {said}");
    }

    #[test]
    fn a_symbol_this_host_does_not_know_is_still_reported() {
        let future = 1u64 << 40;
        let said = surface_refusal(future, 0).expect("absent");
        assert!(said.contains("unknown surface bit 40"), "got {said}");
    }
}
