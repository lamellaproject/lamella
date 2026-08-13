//! The `lamella` command-line tool.

use lamella_bsp_gen::fit::{Budget, BudgetSource, Fit, FitVerdict, fit};
use lamella_bsp_gen::reconcile::{
    Claim, ClaimStatus, Observation, Outcome, ReconcileVerdict, reconcile,
};
use lamella_bsp_gen::strata::{BoardTable, PartRow, Strata, parse};
use std::process::ExitCode;

/// The board and part fact files, compiled in by `build.rs` so a `fit` verdict needs neither this
/// repository nor an install layout.
mod catalogue {
    include!(concat!(env!("OUT_DIR"), "/catalogue.rs"));
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("boards") => list_boards(),
        Some("fit") => fit_command(&args[1..]),
        Some("reconcile") => reconcile_command(&args[1..]),
        Some("--help" | "-h" | "help") | None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("lamella: unknown command {other:?}\n");
            eprint!("{USAGE}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
usage:
  lamella boards                                     every board this build knows
  lamella fit --board <id> --image-bytes <n>         does an image of <n> bytes fit?
  lamella reconcile --board <id> [--read <name>=<v>] is the attached board the one assumed?

`fit` needs no hardware: flash and RAM come from the board file and the part row.

`reconcile` compares readings taken from an attached board against what that board declares it
can be asked. It needs no hardware either: with no --read it reports what attaching the board
WOULD settle, and what nothing declared can reach. Readings are <discriminator>=<value>, and a
value may be decimal or 0x-prefixed.
";

/// Prints every board id the catalogue carries, with the part it is built around.
fn list_boards() -> ExitCode {
    for (id, text) in catalogue::BOARDS {
        match parse(text) {
            Ok(Strata::Board(board)) => {
                let part = if board.part.is_empty() { "-" } else { &board.part };
                println!("{id:<28} {part}");
            }
            _ => println!("{id:<28} (unreadable)"),
        }
    }
    ExitCode::SUCCESS
}

fn fit_command(args: &[String]) -> ExitCode {
    let mut board_id = None;
    let mut image_bytes = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--board" => {
                index += 1;
                board_id = args.get(index).cloned();
            }
            "--image-bytes" => {
                index += 1;
                image_bytes = args.get(index).and_then(|value| value.parse::<i64>().ok());
            }
            other => {
                eprintln!("lamella fit: unknown argument {other:?}");
                return ExitCode::FAILURE;
            }
        }
        index += 1;
    }
    let (Some(board_id), Some(image_bytes)) = (board_id, image_bytes) else {
        eprintln!("usage: lamella fit --board <id> --image-bytes <n>");
        return ExitCode::FAILURE;
    };

    let Some(board) = load_board(&board_id) else {
        eprintln!("lamella fit: no board {board_id:?}; try `lamella boards`");
        return ExitCode::FAILURE;
    };
    let Some(part) = load_part(&board) else {
        eprintln!(
            "lamella fit: board {board_id:?} (family {:?}, module {:?}, part {:?}) resolves to no part row in this build",
            board.family, board.module, board.part
        );
        return ExitCode::FAILURE;
    };

    let verdict = fit(&board, &part, image_bytes);
    print!("{}", render(&board_id, &verdict));
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
fn reconcile_command(args: &[String]) -> ExitCode {
    let mut board_id = None;
    let mut observed = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--board" => {
                index += 1;
                board_id = args.get(index).cloned();
            }
            "--read" => {
                index += 1;
                let Some(pair) = args.get(index) else {
                    eprintln!("lamella reconcile: --read wants <discriminator>=<value>");
                    return ExitCode::FAILURE;
                };
                let Some((name, value)) = pair.split_once('=') else {
                    eprintln!("lamella reconcile: {pair:?} is not <discriminator>=<value>");
                    return ExitCode::FAILURE;
                };
                let parsed = match value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")) {
                    Some(hex) => i64::from_str_radix(hex, 16),
                    None => value.parse::<i64>(),
                };
                let Ok(reading) = parsed else {
                    eprintln!("lamella reconcile: reading {value:?} is not an integer");
                    return ExitCode::FAILURE;
                };
                observed.push(Observation { discriminator: name.to_string(), reading });
            }
            other => {
                eprintln!("lamella reconcile: unknown argument {other:?}");
                return ExitCode::FAILURE;
            }
        }
        index += 1;
    }
    let Some(board_id) = board_id else {
        eprintln!("usage: lamella reconcile --board <id> [--read <discriminator>=<value>]");
        return ExitCode::FAILURE;
    };
    let Some(board) = load_board(&board_id) else {
        eprintln!("lamella reconcile: no board {board_id:?}; try `lamella boards`");
        return ExitCode::FAILURE;
    };
    let Some(part) = load_part(&board) else {
        eprintln!("lamella reconcile: board {board_id:?} resolves to no part row in this build");
        return ExitCode::FAILURE;
    };

    let verdict = reconcile(&board, &part, &[], &observed);
    print!("{}", render_reconcile(&board_id, &verdict));
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
        Outcome::Unconfirmed => "NOT CONFIRMED -- nothing disagreed, and not every claim was reached\n",
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

/// The board with `id`, parsed from the embedded catalogue.
fn load_board(id: &str) -> Option<BoardTable> {
    let (_, text) = catalogue::BOARDS.iter().find(|(board, _)| *board == id)?;
    match parse(text) {
        Ok(Strata::Board(board)) => Some(board),
        _ => None,
    }
}

/// The part row `board` is built around.
///
/// A board names EITHER a bare chip (`family` + `part`) or a MODULE, and the two are exclusive. A
/// module states the family and part it carries, so a module-carrying board resolves in one extra
/// hop -- and a resolver that knew only the bare-chip form reported an empty part id for it rather
/// than saying it could not follow the chain.
fn load_part(board: &BoardTable) -> Option<PartRow> {
    let (family, part) = if board.module.is_empty() {
        (board.family.clone(), board.part.clone())
    } else {
        let (_, text) = catalogue::MODULES.iter().find(|(id, _)| *id == board.module)?;
        let Ok(Strata::Module(module)) = parse(text) else {
            return None;
        };
        (module.family.clone(), module.part.clone())
    };
    let (_, text) = catalogue::PARTS.iter().find(|(id, _)| *id == family)?;
    let Ok(Strata::Parts(parts)) = parse(text) else {
        return None;
    };
    parts.rows.into_iter().find(|row| row.part == part)
}

/// Renders a verdict for a person: the answer, then the numbers, then what it assumed, then what it
/// cannot answer.
///
/// **THE ASSUMPTIONS AND LIMITS ARE PRINTED, NOT LOGGED.** The failure this feature exists to
/// prevent is somebody planning a product around memory nobody soldered; a verdict that computed
/// correctly and buried its own preconditions would cause exactly that.
fn render(selected: &str, verdict: &FitVerdict) -> String {
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
    out.push_str(match &verdict.flash_fit {
        Fit::Fits { headroom } => format!("FITS -- {headroom} B of flash to spare\n"),
        Fit::Exceeds { over } => format!("DOES NOT FIT -- {over} B over the flash budget\n"),
        Fit::Unknown { missing } => format!("CANNOT SAY -- {missing}\n"),
    }
    .as_str());
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

    /// **A CENSUS, NOT AN EXAMPLE, AND IT IS WHAT FINDS THE NEXT SHAPE.** Every board in the
    /// catalogue must resolve to a part row with a real RAM figure. Written after running the tool
    /// by hand found that module-carrying boards resolved to an EMPTY part id -- a shape a
    /// hand-picked Pico row passes straight over. A new board whose form this resolver cannot
    /// follow now fails here rather than at a user's prompt.
    ///
    /// It reports EVERY failure before asserting: a census that panics on the first one answers
    /// "is there a hole" when the question is "where are the holes".
    #[test]
    fn every_board_in_the_catalogue_resolves_to_a_part_row() {
        let mut unresolved = Vec::new();
        for (id, _) in catalogue::BOARDS {
            let Some(board) = load_board(id) else {
                unresolved.push(format!("{id}: board.toml did not parse"));
                continue;
            };
            match load_part(&board) {
                Some(part) if part.ram > 0 => {}
                Some(part) => unresolved.push(format!("{id}: part {:?} states no RAM", part.part)),
                None => unresolved.push(format!(
                    "{id}: family {:?} module {:?} part {:?} -> no row",
                    board.family, board.module, board.part
                )),
            }
        }
        assert!(unresolved.is_empty(), "boards with no resolvable part row: {unresolved:#?}");
    }

    /// The catalogue is generated from the directory, so an empty one means the build script found
    /// nothing -- which would make every other test here vacuously true.
    #[test]
    fn the_catalogue_is_not_empty() {
        assert!(catalogue::BOARDS.len() > 20, "got {} boards", catalogue::BOARDS.len());
        assert!(!catalogue::PARTS.is_empty());
        assert!(!catalogue::MODULES.is_empty(), "module-carrying boards need these");
    }

    /// A module-carrying board resolves through its module to a real part, which is the hop the
    /// bare-chip form does not have.
    #[test]
    fn a_module_carrying_board_resolves_through_its_module() {
        let board = load_board("arduino-mkr1000").expect("the mkr1000 board file");
        assert!(board.family.is_empty(), "this board names a module, not a family");
        assert!(!board.module.is_empty());
        let part = load_part(&board).expect("a module board must still resolve to a part");
        assert!(part.flash > 0 && part.ram > 0, "with real budgets: {part:?}");
    }

    /// The rendered verdict names the id the user typed even when the board file states another.
    #[test]
    fn the_rendering_leads_with_the_id_the_user_selected() {
        let board = load_board("rpi-pico").expect("the pico board file");
        let part = load_part(&board).expect("the rp2040 row");
        let text = render("rpi-pico", &fit(&board, &part, 100_000));
        assert!(text.starts_with("board rpi-pico"), "got {text:?}");
        assert!(text.contains("\"pico\""), "and discloses the file's own id: {text:?}");
        assert!(text.contains("FITS"));
        assert!(text.contains("does NOT answer"), "the limits are printed, not logged");
    }
}
