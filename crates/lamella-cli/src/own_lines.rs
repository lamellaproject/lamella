//! This tool's own lines during a run, told apart from the program's.
//!
//! A run gives the program's standard output to standard output, and nothing else. This tool's own
//! lines go to standard error without a prefix. A blank line separates the introduction from the
//! program, and a caller keeping standard output in a file keeps the program's output alone.

use std::io::{self, Write};

/// How the program runs.
#[derive(Clone, Copy)]
pub(crate) enum How {
    Interpreter,
    NativeImage,
}

/// The first line of a run, naming the version, place and execution model.
pub(crate) fn first_line(on: &str, how: How) -> String {
    let how = match how {
        How::Interpreter => "C# interpreter",
        How::NativeImage => "AOT",
    };
    format!("Lamella v{}, {on} ({how})", env!("CARGO_PKG_VERSION"))
}

/// A board's human name, falling back to the identifier when the catalog has no name.
pub(crate) fn board_name(id: &str) -> String {
    lamella_catalog::load_board(id).map_or_else(|| id.to_owned(), |board| {
        if board.product.is_empty() {
            id.to_owned()
        } else {
            format!("{} {}", board.maker, board.product).trim().to_owned()
        }
    })
}

/// The first line of a run on a board running Lamella's firmware: `on` is the board the run names,
/// or else the board that answered, as its firmware names it.
pub(crate) fn running_interpreted(on: &str) -> String {
    first_line(on, How::Interpreter)
}

/// The first line of a native run through a probe, naming the board catalog's chip family.
pub(crate) fn running_through_probe(board: &str) -> String {
    let chip = lamella_catalog::load_board(board)
        .and_then(|board| lamella_catalog::family_of(&board))
        .filter(|family| !family.is_empty())
        .map_or_else(|| "unknown chip".to_owned(), |family| family.to_ascii_uppercase());
    first_line(&chip, How::NativeImage)
}

/// The first line of a run of a native image in the emulator of the machine `machine`.
pub(crate) fn running_in_emulator(machine: &str) -> String {
    first_line(&format!("{machine} emulator"), How::NativeImage)
}

/// Says `text` on standard error, without a prefix.
pub(crate) fn say(err: &mut dyn Write, text: &str) {
    for line in text.lines() {
        let _ = said_line(err, line);
    }
    let _ = err.flush();
}

/// One line, including a blank line.
fn said_line(err: &mut dyn Write, line: &str) -> io::Result<()> {
    writeln!(err, "{line}")
}

/// Separates the introduction from the program's output.
pub(crate) fn blank_line(err: &mut dyn Write) {
    let _ = writeln!(err);
    let _ = err.flush();
}

/// A run's refusal, preserving the explanation, choices and recovery instructions without a command prefix.
pub(crate) fn fail(err: &mut dyn Write, refusal: &str) {
    full_failure(err, refusal);
}

/// A direct command's full refusal, with no command prefix.
pub(crate) fn full_failure(err: &mut dyn Write, refusal: &str) {
    say(err, &capitalized(without_prefix(refusal)));
}

fn without_prefix(refusal: &str) -> &str {
    let text = refusal.trim();
    let text = text.strip_prefix("lamella: error: ").or_else(|| text.strip_prefix("lamella: ")).unwrap_or(text);
    match text.strip_prefix("lamella ").and_then(|rest| rest.split_once(": ")) {
        Some((verb, rest)) if verb.chars().all(|ch| ch.is_ascii_lowercase() || ch == '-') => rest,
        _ => text,
    }
}

/// A leading word takes a capital; paths, identifiers and percentages keep their spelling.
pub(crate) fn capitalized(text: &str) -> String {
    let word = text.split_whitespace().next().unwrap_or_default().trim_end_matches([',', '.', ':']);
    if !word.chars().all(char::is_alphabetic) {
        return text.to_owned();
    }
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A probe run names the catalog's chip family, including a board carried by a module.
    #[test]
    fn a_probe_run_names_the_catalog_chip_instead_of_the_project_board() {
        for (board, chip) in [
            ("raspberrypi-pico-2", "RP2350"),
            ("raspberrypi-pico-2-w", "RP2350"),
            ("pimoroni-pico-plus-2", "RP2350"),
            ("raspberrypi-pico", "RP2040"),
            ("arduino-mkr1000", "SAMD21"),
        ] {
            assert_eq!(running_through_probe(board), format!("Lamella v{}, {chip} (AOT)", env!("CARGO_PKG_VERSION")), "{board}");
        }
    }

    /// An unresolved chip is not presented as the project's board name.
    #[test]
    fn an_unknown_probe_chip_does_not_claim_a_board_identity() {
        assert_eq!(running_through_probe("unknown-board"), format!("Lamella v{}, unknown chip (AOT)", env!("CARGO_PKG_VERSION")));
    }

    /// The interpreter retains the name supplied by the answering firmware.
    #[test]
    fn an_interpreter_run_keeps_the_reported_board_name() {
        assert_eq!(running_interpreted("Pimoroni Pico Plus 2"), format!("Lamella v{}, Pimoroni Pico Plus 2 (C# interpreter)", env!("CARGO_PKG_VERSION")));
    }

    /// A refusal retains the filename and the action that follows it.
    #[test]
    fn a_short_refusal_keeps_the_filename_it_names() {
        let mut err = Vec::new();
        fail(&mut err, "lamella run: could not read \"C:\\Demo. App\\hello.dll\": access denied. Check permissions.");
        assert_eq!(String::from_utf8(err).unwrap(), "Could not read \"C:\\Demo. App\\hello.dll\": access denied. Check permissions.\n");
    }

    /// A native error's nested parentheses stay complete inside a run's refusal.
    #[test]
    fn a_short_refusal_keeps_the_complete_parenthesized_device_error() {
        let mut err = Vec::new();
        fail(&mut err, "2e8a:000c serial TEST could not be opened (USB error: Access is denied. (os error 5)). Another program may hold it.");
        assert_eq!(String::from_utf8(err).unwrap(), "2e8a:000c serial TEST could not be opened (USB error: Access is denied. (os error 5)). Another program may hold it.\n");
    }

    /// Recovery text keeps literal commands, paths and identifiers intact.
    #[test]
    fn a_refusal_does_not_add_punctuation_to_commands_paths_or_identifiers() {
        for ending in ["Try: lamella run --help", "C:\\Demo. App\\program.elf", "probe:TEST/board"] {
            let mut err = Vec::new();
            fail(&mut err, &format!("lamella run: cannot continue.\n{ending}"));
            assert_eq!(String::from_utf8(err).unwrap(), format!("Cannot continue.\n{ending}\n"));
        }
    }

    /// Lines carry no prefix, and blank lines stay blank.
    #[test]
    fn a_run_has_no_prefix_and_keeps_blank_lines() {
        let mut err = Vec::new();
        say(&mut err, "first\n\nthird");
        assert_eq!(String::from_utf8(err).expect("text"), "first\n\nthird\n");
    }

}
