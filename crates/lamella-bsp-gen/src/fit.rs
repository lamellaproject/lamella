//! Does a program fit on a board -- computed from the fact strata, with the answer's assumptions
//! carried IN the answer.

use crate::strata::{BoardTable, PartRow};
use alloc_free::*;

mod alloc_free {
    pub use std::string::String;
    pub use std::vec::Vec;
}

/// Where a budget came from, so the verdict can say which file it is describing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BudgetSource {
    /// The board's own `[memory]` record -- a soldered part the chip maps at reset.
    Board {
        /// The region id (`flash`, `qspi`).
        region: String,
    },
    /// The chip's `parts.toml` row -- internal memory, which is the whole truth when the board
    /// fits no region of its own.
    Part,
}

/// One memory budget with its provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Budget {
    /// The usable size in bytes.
    pub bytes: i64,
    /// Which file stated it.
    pub source: BudgetSource,
}

/// Whether the image fits the budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fit {
    /// It fits, with this many bytes to spare.
    Fits {
        /// Budget minus image size.
        headroom: i64,
    },
    /// It does not fit, by this many bytes.
    Exceeds {
        /// Image size minus budget.
        over: i64,
    },
    /// No budget is stated, so no comparison is possible.
    Unknown {
        /// What is missing, named so a reader can go and state it.
        missing: String,
    },
}

impl Fit {
    fn of(image: i64, budget: i64, missing: &str) -> Fit {
        if budget <= 0 {
            return Fit::Unknown { missing: String::from(missing) };
        }
        if image <= budget {
            Fit::Fits { headroom: budget - image }
        } else {
            Fit::Exceeds { over: image - budget }
        }
    }
}

/// A fit answer, with everything it assumed and everything it cannot speak to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FitVerdict {
    /// The board id the verdict is about.
    pub board: String,
    /// The part id whose row supplied the chip-truth budgets.
    pub part: String,
    /// The image size compared.
    pub image_bytes: i64,
    /// The flash budget, resolved board-first.
    pub flash: Budget,
    /// Whether the image fits flash.
    pub flash_fit: Fit,
    /// The RAM budget, always chip truth.
    pub ram: Budget,
    /// What the verdict took as given -- the profile it describes.
    pub assumptions: Vec<String>,
    /// What a hardware-free verdict STRUCTURALLY cannot answer, so "fits" is never read as
    /// "verified". Flash and RAM are sound arithmetic; timing and peripheral behaviour are not --
    /// and the sharpest evidence for that is ours, an F746 QSPI dummy-cycle count that did not
    /// reproduce across days on one board with one firmware.
    pub not_answered: Vec<String>,
}

/// Computes whether an image of `image_bytes` fits `board` (whose chip row is `part`).
///
/// Pure arithmetic over already-parsed facts: no file IO, no hardware, no network. The caller
/// supplies the parsed strata, which is what lets one implementation serve the CLI verb, the MCP
/// tool and the editor without any of them re-deriving the rule.
#[must_use]
pub fn fit(board: &BoardTable, part: &PartRow, image_bytes: i64) -> FitVerdict {
    let xip = board.xip_flash();
    let flash = if xip > 0 {
        Budget {
            bytes: xip,
            source: BudgetSource::Board {
                region: board
                    .memory
                    .iter()
                    .find(|region| region.kind == "flash" && region.controller.is_empty())
                    .map_or_else(String::new, |region| region.name.clone()),
            },
        }
    } else {
        Budget { bytes: part.flash, source: BudgetSource::Part }
    };

    let mut assumptions = Vec::new();
    match &flash.source {
        BudgetSource::Board { region } => assumptions.push(format!(
            "flash budget {} B from board '{}' region '{region}' (a fitted part, not the chip)",
            flash.bytes, board.board
        )),
        BudgetSource::Part => assumptions.push(format!(
            "flash budget {} B from part '{}' (the board declares no region of its own)",
            flash.bytes, part.part
        )),
    }
    assumptions.push(format!("RAM budget {} B from part '{}'", part.ram, part.part));

    FitVerdict {
        board: board.board.clone(),
        part: part.part.clone(),
        image_bytes,
        flash_fit: Fit::of(
            image_bytes,
            flash.bytes,
            &format!(
                "neither board '{}' nor part '{}' states a flash size",
                board.board, part.part
            ),
        ),
        flash,
        ram: Budget { bytes: part.ram, source: BudgetSource::Part },
        assumptions,
        not_answered: vec![
            String::from("RAM HEADROOM: the image size is a flash figure; what the program \
                          allocates at run time is not computed here"),
            String::from("TIMING and PERIPHERAL BEHAVIOUR: not derivable from a size, and not \
                          reproducible from facts alone"),
            String::from("WHETHER THE FITTED PARTS ARE POPULATED: a chip id cannot tell a \
                          populated board from a bare one"),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strata::MemoryRegion;

    fn region(name: &str, kind: &str, size: i64) -> MemoryRegion {
        MemoryRegion {
            name: String::from(name),
            kind: String::from(kind),
            base: -1,
            size,
            device_size: -1,
            controller: String::new(),
            optional: false,
            ..MemoryRegion::default()
        }
    }

    fn board_with(memory: Vec<MemoryRegion>) -> BoardTable {
        BoardTable { board: String::from("rpi-pico"), memory, ..BoardTable::default() }
    }

    fn xip_part() -> PartRow {
        PartRow { part: String::from("rp2040"), flash: 0, ram: 0x0004_2000, ..PartRow::default() }
    }

    #[test]
    fn an_xip_boards_flash_comes_from_the_board_not_the_zero_on_its_chip_row() {
        let board = board_with(vec![region("flash", "flash", 0x0020_0000)]);
        let verdict = fit(&board, &xip_part(), 100_000);

        assert_eq!(verdict.flash.bytes, 0x0020_0000, "2 MB, from the board");
        assert_eq!(
            verdict.flash.source,
            BudgetSource::Board { region: String::from("flash") },
            "and the verdict must SAY it came from the board"
        );
        assert_eq!(verdict.flash_fit, Fit::Fits { headroom: 0x0020_0000 - 100_000 });
        assert_eq!(verdict.ram.bytes, 0x0004_2000);
        assert_eq!(verdict.ram.source, BudgetSource::Part);
    }

    /// The other half of the same rule: a part with real internal flash on a board that fits no
    /// region of its own takes the chip row, and says so.
    #[test]
    fn a_board_with_no_region_of_its_own_falls_back_to_the_chip_row() {
        let part = PartRow {
            part: String::from("nrf52833"),
            flash: 0x0008_0000,
            ram: 0x0002_0000,
            ..PartRow::default()
        };
        let verdict = fit(&board_with(Vec::new()), &part, 100_000);
        assert_eq!(verdict.flash.bytes, 0x0008_0000);
        assert_eq!(verdict.flash.source, BudgetSource::Part);
    }

    #[test]
    fn no_stated_flash_anywhere_is_unknown_rather_than_a_refusal() {
        let verdict = fit(&board_with(Vec::new()), &xip_part(), 100_000);
        match verdict.flash_fit {
            Fit::Unknown { ref missing } => {
                assert!(missing.contains("rpi-pico"), "names the board: {missing}");
                assert!(missing.contains("rp2040"), "and the part: {missing}");
            }
            other => panic!("an unstated budget must not render as a comparison: {other:?}"),
        }
    }

    /// An image larger than the budget exceeds it BY a number -- the figure that tells someone how
    /// much to cut, which a bare "does not fit" does not.
    #[test]
    fn an_oversized_image_reports_how_far_over_it_is() {
        let board = board_with(vec![region("flash", "flash", 100_000)]);
        assert_eq!(
            fit(&board, &xip_part(), 250_000).flash_fit,
            Fit::Exceeds { over: 150_000 }
        );
    }

    #[test]
    fn a_region_needing_bring_up_is_not_counted_as_the_code_budget() {
        let mut sdram = region("sdram", "flash", 0x0080_0000);
        sdram.controller = String::from("fmc");
        let board = board_with(vec![sdram]);
        let verdict = fit(&board, &xip_part(), 100_000);
        assert_eq!(
            verdict.flash.source,
            BudgetSource::Part,
            "an 8 MB region behind a controller must not become the code budget"
        );
    }

    /// Every verdict carries its assumptions and its limits. A verdict that computed correctly and
    /// said nothing about what it assumed is the failure this module is shaped to prevent.
    #[test]
    fn a_verdict_states_what_it_assumed_and_what_it_cannot_answer() {
        let board = board_with(vec![region("flash", "flash", 0x0020_0000)]);
        let verdict = fit(&board, &xip_part(), 100_000);
        assert!(
            verdict.assumptions.iter().any(|line| line.contains("board 'rpi-pico'")),
            "the flash assumption names its source: {:?}",
            verdict.assumptions
        );
        assert!(
            verdict.not_answered.iter().any(|line| line.contains("POPULATED")),
            "a chip id cannot tell a populated board from a bare one: {:?}",
            verdict.not_answered
        );
    }
}
