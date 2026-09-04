//! What THIS BUILD is, and which agreed contracts it speaks.

/// The versions a build states about itself.
///
/// Every field is read from the crate that OWNS the fact rather than restated, which is what keeps
/// this a report rather than a second opinion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Contracts {
    /// The front end's own package version.
    ///
    /// **PASSED IN RATHER THAN READ HERE**, because `CARGO_PKG_VERSION` evaluates to the version of
    /// whichever crate compiles it -- reading it in this module would report this library's version
    /// to every caller and be wrong for all of them.
    ///
    /// NOTE: a caller whose package states no version reports `0.0.0`. That is an ANSWER rather
    /// than a failure -- the field says what the front end declares, and declaring nothing is a
    /// thing a front end may do.
    pub tool: &'static str,
    /// The Lamella Link protocol version this build speaks, from the crate that defines it.
    pub wire_protocol: u16,
    /// The image-sidecar record version this build reads, from the module that parses it.
    ///
    /// **A HIGHER ONE IS REFUSED RATHER THAN READ HOPEFULLY**, so a producer can tell from this
    /// number alone whether its records will be accepted -- which is the question a release note
    /// is trying to answer when it names a minimum tools version.
    pub sidecar_schema: u64,
    /// Boards this build carries a catalog row for.
    pub boards: usize,
    /// Boards of those that `flash` can write, which is a strict subset and moves independently.
    pub flashable: usize,
}

impl Contracts {
    /// The contracts this build speaks, for a front end that names its own version.
    #[must_use]
    pub fn of(tool: &'static str) -> Contracts {
        Contracts {
            tool,
            wire_protocol: lamella_wire::PROTOCOL_VERSION,
            sidecar_schema: crate::manifest::SCHEMA,
            boards: lamella_catalog::BOARDS.len(),
            flashable: lamella_catalog::BOARDS
                .iter()
                .filter(|(id, _)| crate::can_flash(id))
                .count(),
        }
    }

    /// The report a person reads.
    ///
    /// **IT SAYS WHAT EACH NUMBER DECIDES, because a bare list of versions is not actionable.** A
    /// reader comparing two machines wants to know which line explains why one of them refused an
    /// image, and a number with no consequence beside it cannot tell them.
    #[must_use]
    pub fn describe(&self) -> String {
        format!(
            "lamella {tool}\n\
             \n\
             \x20 link protocol   {wire}   the Lamella Link version this build speaks\n\
             \x20 sidecar schema  {schema}   the `<image>.manifest.json` record it reads; a higher\n\
             \x20                     one is refused rather than read as if the extra were absent\n\
             \x20 boards          {boards}  in this build's catalog\n\
             \x20 flashable       {flashable}  of those can be written by `flash` over a probe\n\
             \n\
             The two protocol numbers are what decide interoperation, and they move independently\n\
             of the version above and of each other. A tool version pins a build; these pin what it\n\
             will accept.\n",
            tool = self.tool,
            wire = self.wire_protocol,
            schema = self.sidecar_schema,
            boards = self.boards,
            flashable = self.flashable,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every number comes from the crate that owns it, so none of them can drift from its source.
    ///
    /// **THE POINT IS THE DIRECTION.** A report is exactly the place a value gets typed in rather
    /// than referenced -- it is prose-shaped, it is read by people, and a stale number in it looks
    /// like every other number. Comparing each field against its owner is what makes that
    /// impossible rather than unlikely.
    #[test]
    fn every_number_is_the_owning_crates_and_not_a_copy() {
        let it = Contracts::of("test");
        assert_eq!(it.wire_protocol, lamella_wire::PROTOCOL_VERSION);
        assert_eq!(it.sidecar_schema, crate::manifest::SCHEMA);
        assert_eq!(it.boards, lamella_catalog::BOARDS.len());
        assert!(it.flashable > 0, "no board is flashable, so the census is not wired up");
        assert!(it.flashable < it.boards, "{} of {}", it.flashable, it.boards);
    }

    /// The report names what each number decides, not just its value.
    #[test]
    fn the_report_says_what_each_number_is_for() {
        let text = Contracts::of("9.9.9").describe();
        assert!(text.starts_with("lamella 9.9.9\n"), "{text}");
        assert!(text.contains("link protocol"), "{text}");
        assert!(text.contains("sidecar schema"), "{text}");
        assert!(text.contains("refused rather than read"), "{text}");
        assert!(text.contains("move independently"), "{text}");
    }
}
