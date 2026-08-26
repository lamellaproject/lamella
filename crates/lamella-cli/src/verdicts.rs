//! The verdicts that need no hardware: `boards`, `fit`, and `reconcile`.

use crate::args::{self, Spec};
use lamella_catalog::{self as catalog, BOARDS};
use lamella_bsp_gen::fit::{Budget, BudgetSource, Fit, FitVerdict, fit};
use lamella_bsp_gen::reconcile::{
    Claim, ClaimStatus, Observation, Outcome, ReconcileVerdict, reconcile,
};
use lamella_bsp_gen::strata::{Strata, parse};
use std::process::ExitCode;

/// Prints every board id the catalog carries, with the part it is built around and whether this
/// build can write it.
///
/// **THE COVERAGE COLUMN IS PRINTED PER BOARD RATHER THAN SUMMARIZED**, because a capability that
/// reaches some of a list and is described in a sentence reads as reaching all of it. A reader
/// scanning for their own board gets the answer on their own row.
const BOARDS_USAGE: &str = "\
usage: lamella boards

Every board this build knows, with the part it carries and whether `lamella flash` can write it.

A `-` in the FLASH column means nobody has stated how that board is programmed yet -- not that it
cannot be. The board id in the first column is what --board takes everywhere else.";

const FIT_USAGE: &str = "\
usage: lamella fit --board <id> --image-bytes <n>

Answers whether an image of <n> bytes fits that board, from the board facts alone -- no hardware,
no image, no compiler. `lamella build --format` prints the byte count to pass.

It answers about FLASH. A program that fits may still fail to run for want of RAM, which is a
different question this verb does not claim to answer.";

const RECONCILE_USAGE: &str = "\
usage: lamella reconcile --board <id> [--read <name>=<value>]...

Asks whether the board in front of you is the one you assumed. Give the readings you took and it
compares them against what <id> should answer.

--read repeats, once per discriminator: --read chip_id=0x4c013477 --read flash_jedec=0x1840ef

The readings come from the command line rather than from a probe on purpose. Taking them off the
wire belongs to whatever drives the board; the COMPARISON is the part that has to be right, and
keeping them apart is what lets this be exercised with nothing plugged in.";

pub fn boards_command(args: &[String]) -> ExitCode {
    let spec = Spec { verb: "boards", usage: Some(BOARDS_USAGE), values: &[], flags: &[] };
    if let Err(halt) = args::parse_or_halt(args, &spec) {
        return halt.code();
    }
    println!("{:<28} {:<14} {}", "BOARD", "PART", "FLASH");
    for (id, text) in BOARDS {
        let can = if crate::flash::can_flash(id) { "yes" } else { "-" };
        match parse(text) {
            Ok(Strata::Board(board)) => {
                let part = if board.part.is_empty() { "-" } else { &board.part };
                println!("{id:<28} {part:<14} {can}");
            }
            _ => println!("{id:<28} {:<14} {can}", "(unreadable)"),
        }
    }
    println!(
        "\nFLASH = `lamella flash` can write this board over a probe, with no firmware on it \
         first.\nA `-` means nobody has stated how that board is programmed yet, not that it \
         cannot be."
    );
    ExitCode::SUCCESS
}

pub fn fit_command(args: &[String]) -> ExitCode {
    let spec = Spec {
        verb: "fit",
        usage: Some(FIT_USAGE),
        values: &["--board", "--image-bytes"],
        flags: &[],
    };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    let (Some(board_id), Some(image_bytes)) = (
        parsed.value("--board"),
        parsed.value("--image-bytes").and_then(|value| value.parse::<i64>().ok()),
    ) else {
        eprintln!("usage: lamella fit --board <id> --image-bytes <n>");
        eprintln!(
            "\n`lamella build <file> --board <id>` measures <n> for you: it compiles the program \
             and answers this question about the image it produced."
        );
        return ExitCode::FAILURE;
    };

    let (board, part) = match catalog::resolve(board_id) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("lamella fit: {error}");
            return ExitCode::FAILURE;
        }
    };

    let verdict = fit(&board, &part, image_bytes);
    print!("{}", render(board_id, &verdict));
    exit_for(&verdict)
}

/// The exit code a fit verdict deserves.
///
/// **AN UNKNOWN BUDGET IS NOT A FAILURE EXIT.** Exiting nonzero would make "we cannot answer this"
/// indistinguishable from "it does not fit" to any script reading the code -- the same collapse
/// the verdict's own `Unknown` arm exists to prevent, reintroduced one layer up.
#[must_use]
pub fn exit_for(verdict: &FitVerdict) -> ExitCode {
    match verdict.flash_fit {
        Fit::Exceeds { .. } => ExitCode::FAILURE,
        Fit::Fits { .. } | Fit::Unknown { .. } => ExitCode::SUCCESS,
    }
}

/// `lamella reconcile`: compare readings from an attached board against what it declares.
///
/// The readings arrive on the command line rather than from a probe, which is deliberate. Taking
/// them off the wire belongs to whatever drives the board; the COMPARISON is the part that has to
/// be right, and separating them is what lets this be exercised, and answered, with nothing
/// plugged in.
pub fn reconcile_command(args: &[String]) -> ExitCode {
    let spec = Spec {
        verb: "reconcile",
        usage: Some(RECONCILE_USAGE),
        values: &["--board", "--read"],
        flags: &[],
    };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    let mut observed = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--read" {
            index += 1;
            let Some(pair) = args.get(index) else {
                eprintln!("lamella reconcile: --read wants <discriminator>=<value>");
                return ExitCode::FAILURE;
            };
            let Some((name, value)) = pair.split_once('=') else {
                eprintln!("lamella reconcile: {pair:?} is not <discriminator>=<value>");
                return ExitCode::FAILURE;
            };
            let parsed_reading = match value.strip_prefix("0x").or_else(|| value.strip_prefix("0X"))
            {
                Some(hex) => i64::from_str_radix(hex, 16),
                None => value.parse::<i64>(),
            };
            let Ok(reading) = parsed_reading else {
                eprintln!("lamella reconcile: reading {value:?} is not an integer");
                return ExitCode::FAILURE;
            };
            observed.push(Observation { discriminator: name.to_string(), reading });
        }
        index += 1;
    }

    let Some(board_id) = parsed.value("--board") else {
        eprintln!("usage: lamella reconcile --board <id> [--read <discriminator>=<value>]");
        return ExitCode::FAILURE;
    };
    let (board, part) = match catalog::resolve(board_id) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("lamella reconcile: {error}");
            return ExitCode::FAILURE;
        }
    };

    let verdict = reconcile(&board, &part, &[], &observed);
    print!("{}", render_reconcile(board_id, &verdict));
    match verdict.outcome {
        Outcome::Contradicted => ExitCode::FAILURE,
        Outcome::Unconfirmed | Outcome::Confirmed => ExitCode::SUCCESS,
    }
}

/// Renders a reconciliation for a person: the verdict, then every claim and where it stands, then
/// the profile that was compared against, then the limits.
fn render_reconcile(selected: &str, verdict: &ReconcileVerdict) -> String {
    let mut out = String::new();
    let name = if selected == verdict.board {
        selected.to_string()
    } else {
        format!("{selected} (board id {:?})", verdict.board)
    };
    out.push_str(&format!("board {name} (part {})\n\n", verdict.part));
    out.push_str(match verdict.outcome {
        Outcome::Confirmed => "CONFIRMED -- every claim below was reached at the rung it needs\n",
        Outcome::Unconfirmed => {
            "NOT CONFIRMED -- nothing disagreed, and not every claim was reached\n"
        }
        Outcome::Contradicted => "CONTRADICTED -- this is not the board the image assumed\n",
    });
    out.push_str("\nclaims:\n");
    for report in &verdict.profile {
        let what = match &report.claim {
            Claim::Part { part } => format!("part {part}"),
            Claim::Region { name, bytes } => format!("region {name:?}, {bytes} B reachable"),
            Claim::Device { name, address, part } => {
                format!("module {name:?} ({part}) at 0x{address:02X}")
            }
        };
        let stands = match &report.status {
            ClaimStatus::Confirmed { by, rung } => format!("confirmed by {by:?} ({rung})"),
            ClaimStatus::Contradicted { by, expected, read } => {
                format!("CONTRADICTED by {by:?}: expected {expected}, read {read}")
            }
            ClaimStatus::Unconfirmed { why } => format!("not reached -- {why}"),
        };
        out.push_str(&format!("  {what}\n    {stands}\n"));
    }
    if !verdict.unplaced.is_empty() {
        out.push_str("\nreadings this board cannot place:\n");
        for line in &verdict.unplaced {
            out.push_str(&format!("  - {line}\n"));
        }
    }
    out.push_str("\nassuming:\n");
    for line in &verdict.assumptions {
        out.push_str(&format!("  - {line}\n"));
    }
    out.push_str("\nthis verdict does NOT answer:\n");
    for line in &verdict.not_answered {
        out.push_str(&format!("  - {line}\n"));
    }
    out
}

/// Renders a verdict for a person: the answer, then the numbers, then what it assumed, then what it
/// cannot answer.
///
/// **THE ASSUMPTIONS AND LIMITS ARE PRINTED, NOT LOGGED.** The failure this feature exists to
/// prevent is somebody planning a product around memory nobody soldered; a verdict that computed
/// correctly and buried its own preconditions would cause exactly that.
#[must_use]
pub fn render(selected: &str, verdict: &FitVerdict) -> String {
    let mut out = String::new();
    let name = if selected == verdict.board {
        selected.to_string()
    } else {
        format!("{selected} (board id {:?})", verdict.board)
    };
    out.push_str(&format!(
        "board {name} (part {})\n  image  {} B\n",
        verdict.part, verdict.image_bytes
    ));
    out.push_str(&format!("  flash  {}\n", budget_line(&verdict.flash)));
    out.push_str(&format!("  ram    {}\n\n", budget_line(&verdict.ram)));
    out.push_str(
        match &verdict.flash_fit {
            Fit::Fits { headroom } => format!("FITS -- {headroom} B of flash to spare\n"),
            Fit::Exceeds { over } => format!("DOES NOT FIT -- {over} B over the flash budget\n"),
            Fit::Unknown { missing } => format!("CANNOT SAY -- {missing}\n"),
        }
        .as_str(),
    );
    out.push_str("\nassuming:\n");
    for line in &verdict.assumptions {
        out.push_str(&format!("  - {line}\n"));
    }
    out.push_str("\nthis verdict does NOT answer:\n");
    for line in &verdict.not_answered {
        out.push_str(&format!("  - {line}\n"));
    }
    out
}

/// `2097152 B (board region 'flash')` -- the number and the file that stated it, because a budget
/// without its provenance is what lets a bare board and a populated one read alike.
fn budget_line(budget: &Budget) -> String {
    match &budget.source {
        BudgetSource::Board { region } => {
            format!("{} B (board region {region:?})", budget.bytes)
        }
        BudgetSource::Part => format!("{} B (part row)", budget.bytes),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rendered verdict names the id the user typed even when the board file states another.
    #[test]
    fn the_rendering_leads_with_the_id_the_user_selected() {
        let (board, part) = catalog::resolve("rpi-pico").expect("the pico board and its part");
        let text = render("rpi-pico", &fit(&board, &part, 100_000));
        assert!(text.starts_with("board rpi-pico"), "got {text:?}");
        assert!(text.contains("\"pico\""), "and discloses the file's own id: {text:?}");
        assert!(text.contains("FITS"));
        assert!(text.contains("does NOT answer"), "the limits are printed, not logged");
    }

    /// An image over the budget is a failure exit; one whose budget is unknown is not.
    #[test]
    fn only_exceeding_the_budget_is_a_failure_exit() {
        let (board, part) = catalog::resolve("rpi-pico").expect("the pico board and its part");
        let fits = fit(&board, &part, 1_000);
        let exceeds = fit(&board, &part, 1_000_000_000);
        assert!(matches!(exceeds.flash_fit, Fit::Exceeds { .. }), "got {:?}", exceeds.flash_fit);
        assert_eq!(format!("{:?}", exit_for(&fits)), format!("{:?}", ExitCode::SUCCESS));
        assert_eq!(format!("{:?}", exit_for(&exceeds)), format!("{:?}", ExitCode::FAILURE));
    }
    /// **A VERB WITH NO USAGE TEXT ANSWERS `--help` BY PRINTING NOTHING AND EXITING 0**, which
    /// reads to a person as "this tool has no help" and to a script as success. Five verbs did
    /// exactly that until they were given one.
    ///
    /// Asserting the FIRST LINE rather than the presence of a string also catches the likelier
    /// drift: a usage block copied from a neighbouring verb and not renamed.
    #[test]
    fn the_usage_opens_with_the_verb_it_belongs_to() {
        assert!(
            BOARDS_USAGE.starts_with("usage: lamella boards"),
            "`boards` must open with the line a reader retypes: {}",
            BOARDS_USAGE.lines().next().unwrap_or_default()
        );
        assert!(
            FIT_USAGE.starts_with("usage: lamella fit"),
            "`fit` must open with the line a reader retypes: {}",
            FIT_USAGE.lines().next().unwrap_or_default()
        );
        assert!(
            RECONCILE_USAGE.starts_with("usage: lamella reconcile"),
            "`reconcile` must open with the line a reader retypes: {}",
            RECONCILE_USAGE.lines().next().unwrap_or_default()
        );
    }

}
