//! The verdicts that need no hardware: `boards`, `fit`, and `reconcile`.

use crate::args::{self, Positional, Spec};
use lamella_catalog::{self as catalog, BOARDS};
use lamella_bsp_gen::fit::{Budget, BudgetSource, Fit, FitVerdict, fit, fit_external};
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
Usage:
  lamella boards

Every board this build knows, with the part it carries and whether `lamella flash` can write it.

A `-` in the FLASH column means nobody has stated how that board is programmed yet -- not that it
cannot be. The board ID in the first column is what --board takes everywhere else.";

const FIT_USAGE: &str = "\
Usage:
  lamella fit --board <board> --image-bytes <bytes> [--window-bytes <bytes>]

Answers whether an image of that many bytes fits the board, from the board facts alone -- no
hardware, no image, no compiler. `lamella build --format` prints the byte count to pass.

--window-bytes is the size of the half of an image the board keeps in its external flash, on a
board that has one. That half is judged against the external flash, and --image-bytes, then the
half the part boots from, against the part's own flash.

It answers about flash. A program that fits may still fail to run for want of RAM, which is a
different question this verb does not claim to answer.";

const RECONCILE_USAGE: &str = "\
Usage:
  lamella reconcile --board <board> [--read <name>=<value>...]

Asks whether the board in front of you is the one you assumed. Give the readings you took and it
compares them against what <board> should answer.

--read repeats, once per discriminator: --read chip_id=0x4c013477 --read flash_jedec=0x1840ef

The readings come from the command line rather than from a probe on purpose. Taking them off the
wire belongs to whatever drives the board; the comparison is the part that has to be right, and
keeping them apart is what lets this be exercised with nothing plugged in.";

/// The column headings of `lamella boards`, named once because they are measured as well as
/// printed: a column has to be at least as wide as its own title.
const BOARD_HEADING: &str = "BOARD";
const PART_HEADING: &str = "PART";

/// The `lamella boards` table, rendered rather than printed, so its columns can be checked.
///
/// The heading and one line per board, newline-separated, with no trailing newline.
fn boards_table() -> String {
    let rows: Vec<(&str, String, &str)> = BOARDS
        .iter()
        .map(|(id, text)| {
            let can = if crate::flash::can_flash(id) { "yes" } else { "-" };
            match parse(text) {
                Ok(Strata::Board(board)) => {
                    let resolved = catalog::load_part(&board).map(|row| row.part);
                    let part = resolved
                        .as_deref()
                        .filter(|p| !p.is_empty())
                        .or(Some(board.part.as_str()).filter(|p| !p.is_empty()))
                        .unwrap_or("-")
                        .to_owned();
                    (*id, part, can)
                }
                _ => (*id, String::from("(unreadable)"), can),
            }
        })
        .collect();
    let board_width = rows.iter().map(|(id, ..)| id.len()).chain([BOARD_HEADING.len()]).max();
    let part_width = rows.iter().map(|(_, part, _)| part.len()).chain([PART_HEADING.len()]).max();
    let board_width = board_width.unwrap_or(BOARD_HEADING.len());
    let part_width = part_width.unwrap_or(PART_HEADING.len());
    let mut table = format!("{BOARD_HEADING:<board_width$} {PART_HEADING:<part_width$} FLASH");
    for (id, part, can) in &rows {
        table.push('\n');
        table.push_str(&format!("{id:<board_width$} {part:<part_width$} {can}"));
    }
    table
}

/// `lamella boards`'s option that writes the boards as the MSBuild file `Lamella.Sdk` checks a
/// project's board against before restore, as `lamella devices --msbuild` writes the devices.
const MSBUILD: &str = "--msbuild";

/// Every board this build knows, by its id, as the MSBuild file of `LamellaBoard` items that
/// `Lamella.Sdk` imports: the boards a lamella1.0 project may name, which are the ones every verb
/// here takes for `--board`.
///
/// Each says what this build does for it, as the SDK's device list needs to know: `NativeImage`,
/// whether it writes a native image for the board, as `dotnet publish` does with `PublishAot`; and
/// `ProbeFamilies`, the families of debug probe a run writes such an image through and follows the
/// program through, separated by semicolons, and empty where it runs none that way. A board whose
/// product has a name, as the board names itself over Lamella Link, carries it as `Name`.
fn boards_msbuild() -> String {
    let mut text = String::from(
        "<Project>\n  <!-- Written by the lamella boards command: every board the lamella tool knows, by its \
         id. Lamella.Sdk checks the board a lamella1.0 project names against them. -->\n  <ItemGroup>\n",
    );
    for (id, board) in BOARDS.iter().filter_map(|(id, _)| catalog::load_board(id).map(|board| (id, board))) {
        let name = u16::try_from(board.product_model).ok().and_then(lamella_wire_host::board_name);
        text.push_str(&format!(
            "    <LamellaBoard Include=\"{}\"{} NativeImage=\"{}\" ProbeFamilies=\"{}\" />\n",
            crate::devices::attribute(id),
            name.map(|name| format!(" Name=\"{}\"", crate::devices::attribute(name))).unwrap_or_default(),
            crate::flash::links_native_image(id),
            crate::image_run::probe_families(id).join(";"),
        ));
    }
    text.push_str("  </ItemGroup>\n</Project>\n");
    text
}

pub fn boards_command(args: &[String]) -> ExitCode {
    if let [option, file] = args
        && option == MSBUILD
    {
        return match std::fs::write(file, boards_msbuild()) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("lamella boards: error: write {file}: {error}");
                ExitCode::FAILURE
            }
        };
    }
    let spec = Spec {
        verb: "boards",
        usage: Some(BOARDS_USAGE),
        values: &[],
        repeats: &[],
        flags: &[],
        positional: Positional::None,
    };
    if let Err(halt) = args::parse_or_halt(args, &spec) {
        return halt.code();
    }
    println!("{}", boards_table());
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
        values: &["--board", "--image-bytes", "--window-bytes"],
        repeats: &[],
        flags: &[],
        positional: Positional::None,
    };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    let (board_id, image_bytes) = match (parsed.value("--board"), parsed.value("--image-bytes")) {
        (Some(board), Some(bytes)) => (board, bytes),
        (None, None) => return refused("fit", "give --board and --image-bytes"),
        (None, Some(_)) => return refused("fit", "give --board"),
        (Some(_), None) => return refused("fit", "give --image-bytes"),
    };
    let image_bytes = match size("--image-bytes", image_bytes) {
        Ok(bytes) => bytes,
        Err(refusal) => return refused_by(&refusal),
    };
    let window = match parsed.value("--window-bytes").map(|bytes| size("--window-bytes", bytes)) {
        None => None,
        Some(Ok(bytes)) => Some(bytes),
        Some(Err(refusal)) => return refused_by(&refusal),
    };
    if let Err(refusal) = args::known_board("fit", board_id) {
        return refused_by(&refusal);
    }
    let (board, part) = match catalog::resolve(board_id) {
        Ok(resolved) => resolved,
        Err(error) => {
            eprintln!("lamella fit: {error}");
            return ExitCode::FAILURE;
        }
    };
    let external = match window.map(|bytes| fit_external(&board, bytes)) {
        None => None,
        Some(Some(judged)) => Some(judged),
        Some(None) => {
            eprintln!(
                "lamella fit: {board_id} states no external flash, so --window-bytes has nothing to be \
                 judged against."
            );
            return ExitCode::FAILURE;
        }
    };

    let verdict = fit(&board, &part, image_bytes);
    print!("{}", render(board_id, &verdict));
    let Some((budget, external_fit)) = external else {
        return exit_for(&verdict);
    };
    let region = match &budget.source {
        BudgetSource::Board { region } => region.as_str(),
        BudgetSource::Part => "",
    };
    let window_bytes = window.unwrap_or(0);
    match &external_fit {
        Fit::Fits { headroom } => println!(
            "\nexternal flash: FITS -- {window_bytes} B of {} B in board region '{region}', {headroom} B to spare",
            budget.bytes
        ),
        Fit::Exceeds { over } => println!(
            "\nexternal flash: DOES NOT FIT -- {window_bytes} B against {} B in board region '{region}', \
             {over} B over",
            budget.bytes
        ),
        Fit::Unknown { missing } => println!("\nexternal flash: UNKNOWN -- {missing}"),
    }
    if matches!(external_fit, Fit::Exceeds { .. }) {
        return ExitCode::FAILURE;
    }
    exit_for(&verdict)
}

/// `value`, given to `fit`'s `option`, as a size in bytes, or the refusal of the command line.
fn size(option: &str, value: &str) -> Result<i64, String> {
    value
        .parse::<i64>()
        .ok()
        .filter(|bytes| *bytes >= 0)
        .ok_or_else(|| args::refusal("fit", &format!("{option} \"{value}\" is not a non-negative integer")))
}

/// Says `refusal` and fails.
fn refused_by(refusal: &str) -> ExitCode {
    eprintln!("{refusal}");
    ExitCode::FAILURE
}

/// Refuses `verb`'s command line for what is `wrong` with it.
fn refused(verb: &str, wrong: &str) -> ExitCode {
    refused_by(&args::refusal(verb, wrong))
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
        repeats: &["--read"],
        flags: &[],
        positional: Positional::None,
    };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    let mut observed = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--read"
            && let Some(pair) = args.get(index + 1)
        {
            index += 1;
            let Some((name, value)) = pair.split_once('=') else {
                return refused("reconcile", &format!("--read \"{pair}\" is not <name>=<value>"));
            };
            let parsed_reading = match value.strip_prefix("0x").or_else(|| value.strip_prefix("0X"))
            {
                Some(hex) => i64::from_str_radix(hex, 16),
                None => value.parse::<i64>(),
            };
            let Ok(reading) = parsed_reading else {
                return refused("reconcile", &format!("--read \"{pair}\" has a value that is not an integer"));
            };
            observed.push(Observation { discriminator: name.to_string(), reading });
        }
        index += 1;
    }

    let Some(board_id) = parsed.value("--board") else {
        return refused("reconcile", "give --board");
    };
    if let Err(refusal) = args::known_board("reconcile", board_id) {
        return refused_by(&refusal);
    }
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

    /// **THE SDK's LIST OF BOARDS IS THE BOARD CHECK's LIST.** The MSBuild file names each board
    /// once, and exactly the boards `--board` takes everywhere else, so the SDK never refuses a
    /// board this build takes, nor takes one it would refuse.
    #[test]
    fn the_msbuild_boards_file_names_exactly_the_boards_the_board_check_takes() {
        let text = boards_msbuild();
        let named: Vec<&str> = text
            .lines()
            .filter_map(|line| line.trim().strip_prefix("<LamellaBoard Include=\""))
            .filter_map(|rest| rest.split('"').next())
            .collect();
        let taken: Vec<&str> = BOARDS
            .iter()
            .map(|(id, _)| *id)
            .filter(|id| crate::project::BoardCheck::for_board("build", id).is_ok())
            .collect();
        assert_eq!(named, taken, "the file names the boards the board check takes, in the catalog's order");
        assert!(named.contains(&"rpi-pico2") && named.contains(&"bbc-micro-bit-v2"), "{named:?}");
        let mut unique = named.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), named.len(), "each board is named once");
        assert!(text.starts_with("<Project>") && text.ends_with("</Project>\n"), "{text}");
    }

    /// Each board says what a native run of it takes, as the run decides it: whether this build
    /// writes a native image for it, and the probe families a run by the image's manifest goes
    /// through. A Pico 2 is written as a native image through a CMSIS-DAP probe.
    #[test]
    fn the_msbuild_boards_file_says_how_each_board_runs_a_native_image() {
        let text = boards_msbuild();
        let line_of = |board: &str| {
            let start = format!("<LamellaBoard Include=\"{board}\" ");
            text.lines().map(str::trim).find(|line| line.starts_with(&start)).unwrap_or_default().to_owned()
        };
        assert_eq!(
            line_of("rpi-pico2"),
            "<LamellaBoard Include=\"rpi-pico2\" Name=\"Raspberry Pi Pico 2\" NativeImage=\"true\" ProbeFamilies=\"cmsis-dap\" />"
        );
        assert_eq!(
            line_of("st-stm32f769i-disco"),
            "<LamellaBoard Include=\"st-stm32f769i-disco\" Name=\"STM32F769I Discovery\" NativeImage=\"false\" \
             ProbeFamilies=\"st-link\" />"
        );
        assert_eq!(
            line_of("bbc-micro-bit-v2"),
            "<LamellaBoard Include=\"bbc-micro-bit-v2\" Name=\"BBC micro:bit v2\" NativeImage=\"true\" ProbeFamilies=\"\" />"
        );
    }

    /// The four RP2350 boards with a CMSIS-DAP route are written as every board is: the Pico LiPo
    /// 2 XL W's line is the Pico LiPo 2's shape, and no board carries an attribute of its own.
    #[test]
    fn the_msbuild_boards_file_writes_the_four_rp2350_boards_as_it_writes_every_board() {
        let text = boards_msbuild();
        let line_of = |board: &str| {
            let start = format!("<LamellaBoard Include=\"{board}\" ");
            text.lines().map(str::trim).find(|line| line.starts_with(&start)).unwrap_or_default().to_owned()
        };
        assert_eq!(
            line_of("pimoroni-pico-lipo-2-xl-w"),
            "<LamellaBoard Include=\"pimoroni-pico-lipo-2-xl-w\" Name=\"Pimoroni Pico LiPo 2 XL W\" \
             NativeImage=\"false\" ProbeFamilies=\"cmsis-dap\" />"
        );
        assert_eq!(
            line_of("pimoroni-pico-lipo-2"),
            "<LamellaBoard Include=\"pimoroni-pico-lipo-2\" Name=\"Pimoroni Pico LiPo 2\" NativeImage=\"false\" \
             ProbeFamilies=\"cmsis-dap\" />"
        );
        assert!(!text.contains("Advertised="), "no board is marked: {text}");
    }

    /// `lamella boards` lists every board of the catalog, the Pico LiPo 2s and the Feather RP2350s
    /// among them, each written by `lamella flash`.
    #[test]
    fn the_boards_table_lists_every_board_of_the_catalog() {
        let table = boards_table();
        let listed: Vec<&str> =
            table.lines().skip(1).filter_map(|line| line.split_whitespace().next()).collect();
        for board in [
            "pimoroni-pico-lipo-2",
            "pimoroni-pico-lipo-2-xl-w",
            "adafruit-feather-rp2350-hstx",
            "adafruit-feather-rp2350-hstx-psram",
        ] {
            let line = table
                .lines()
                .find(|line| line.split_whitespace().next() == Some(board))
                .unwrap_or_else(|| panic!("{board} is a row: {table}"));
            assert!(line.ends_with(" yes"), "{board} is written by `lamella flash`: {line}");
        }
        assert_eq!(listed.len(), BOARDS.len(), "every board of the catalog is a row");
    }

    /// Every row of `lamella boards` puts PART and FLASH in the same column as the heading does.
    ///
    /// **A FIXED COLUMN WIDTH IS A CLAIM ABOUT DATA IT CANNOT SEE.** A padding width pads and
    /// never truncates, so one id longer than the width pushes PART and FLASH right on that row
    /// alone. This asks the table this verb prints rather than the format string that built it, so
    /// a board name longer than any here today fails this the day it is added rather than quietly
    /// bending the table.
    #[test]
    fn every_row_of_the_boards_table_lines_its_columns_up_with_the_heading() {
        fn column_starts(line: &str) -> Vec<usize> {
            let mut starts = Vec::new();
            let mut previous_was_space = true;
            for (index, character) in line.char_indices() {
                if !character.is_whitespace() && previous_was_space {
                    starts.push(index);
                }
                previous_was_space = character.is_whitespace();
            }
            starts
        }

        let table = boards_table();
        let mut lines = table.lines();
        let heading = lines.next().expect("the table has a heading");
        let expected = column_starts(heading);
        assert_eq!(expected.len(), 3, "the heading names three columns: {heading}");

        let mut rows = 0;
        for line in lines {
            rows += 1;
            assert_eq!(column_starts(line), expected, "a row is out of column: {line}");
        }
        assert!(rows > 20, "the table listed {rows} boards, which is too few to have rendered");
    }

    /// The rendered verdict names the id the user typed, and discloses the file's own id when
    /// the two differ. They agree on every board in the tree, so the second half is exercised
    /// with a selection the file does not state rather than with a board that drifts.
    #[test]
    fn the_rendering_leads_with_the_id_the_user_selected() {
        let (board, part) = catalog::resolve("rpi-pico").expect("the pico board and its part");
        let verdict = fit(&board, &part, 100_000);
        let text = render("rpi-pico", &verdict);
        assert!(text.starts_with("board rpi-pico"), "got {text:?}");
        assert!(!text.contains("board id"), "the two ids agree, so only one is printed: {text:?}");
        assert!(text.contains("FITS"));
        assert!(text.contains("does NOT answer"), "the limits are printed, not logged");

        let other = render("a-name-the-file-does-not-state", &verdict);
        assert!(other.starts_with("board a-name-the-file-does-not-state"), "got {other:?}");
        assert!(other.contains("\"rpi-pico\""), "and discloses the file's own id: {other:?}");
    }

    /// An image over the budget is a failure exit; one whose budget is unknown is not.
    /// A negative `--image-bytes` is refused as not a size. The rule called one a fit with more
    /// room than the board has, and overflowed at the most negative.
    #[test]
    fn a_negative_size_is_refused_rather_than_fitted() {
        for size in ["-5", "-9223372036854775808"] {
            let args: Vec<String> =
                ["--board", "rpi-pico", "--image-bytes", size].iter().map(|word| (*word).to_owned()).collect();
            assert_eq!(
                format!("{:?}", fit_command(&args)),
                format!("{:?}", ExitCode::FAILURE),
                "--image-bytes {size} must be refused"
            );
        }
    }

    #[test]
    fn only_exceeding_the_budget_is_a_failure_exit() {
        let (board, part) = catalog::resolve("rpi-pico").expect("the pico board and its part");
        let fits = fit(&board, &part, 1_000);
        let exceeds = fit(&board, &part, 1_000_000_000);
        assert!(matches!(exceeds.flash_fit, Fit::Exceeds { .. }), "got {:?}", exceeds.flash_fit);
        assert_eq!(format!("{:?}", exit_for(&fits)), format!("{:?}", ExitCode::SUCCESS));
        assert_eq!(format!("{:?}", exit_for(&exceeds)), format!("{:?}", ExitCode::FAILURE));
    }

    /// EVERY board in the listing resolves to a part, including the ones that name a MODULE.
    ///
    /// A board names either a bare chip (`family` + `part`) or a module, and the two are exclusive.
    /// This verb read `board.part` off the file, so both module boards printed `-` in the PART
    /// column as though the part were unknown -- while `lamella flash` routed one of them happily.
    /// It was not caught by any test because a dash is a plausible rendering: two of sixty rows
    /// looked like boards nobody had filled in yet.
    ///
    /// The assertion is over ALL boards rather than the two, because the failure is a class: a
    /// module board added tomorrow with no `csp/*/module.toml` behind it fails here rather than
    /// printing a dash for the rest of its life.
    #[test]
    fn every_board_in_the_listing_resolves_to_a_part() {
        let mut unresolved: Vec<&str> = Vec::new();
        let mut modules = 0;
        for (id, text) in BOARDS {
            let Ok(Strata::Board(board)) = parse(text) else {
                unresolved.push(id);
                continue;
            };
            if !board.module.is_empty() {
                modules += 1;
            }
            match catalog::load_part(&board) {
                Some(row) if !row.part.is_empty() => {}
                _ => unresolved.push(id),
            }
        }
        assert!(unresolved.is_empty(), "these boards resolve to no part: {unresolved:?}");
        assert!(modules > 0, "no board names a module, so this proves nothing about that hop");
    }

    /// **A VERB WITH NO USAGE TEXT ANSWERS `--help` BY PRINTING NOTHING AND EXITING 0**, which
    /// reads to a person as "this tool has no help" and to a script as success. Five verbs did
    /// exactly that until they were given one.
    ///
    /// Asserting the FIRST LINE rather than the presence of a string also catches the likelier
    /// drift: a usage block copied from a neighboring verb and not renamed.
    #[test]
    fn the_usage_opens_with_the_verb_it_belongs_to() {
        assert!(
            BOARDS_USAGE.starts_with("Usage:\n  lamella boards"),
            "`boards` must open with the line a reader retypes: {}",
            BOARDS_USAGE.lines().nth(1).unwrap_or_default()
        );
        assert!(
            FIT_USAGE.starts_with("Usage:\n  lamella fit "),
            "`fit` must open with the line a reader retypes: {}",
            FIT_USAGE.lines().nth(1).unwrap_or_default()
        );
        assert!(
            RECONCILE_USAGE.starts_with("Usage:\n  lamella reconcile "),
            "`reconcile` must open with the line a reader retypes: {}",
            RECONCILE_USAGE.lines().nth(1).unwrap_or_default()
        );
    }

}
