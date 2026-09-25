//! `lamella flash --board <id> --clear-bootprot`: clear a SAM D21's bootloader protection, which is
//! one field of its NVM User Row -- and `--restore-user-row <file>`, the way back.

use crate::args::Options;
use lamella_flash_routes::user_row::{
    self, SAMD21_BOOTPROT_NONE, SAMD21_USER_ROW_FIELDS, Samd21UserRow, Samd21UserRowError,
    Samd21UserRowField, UserRowReading, UserRowWritten, samd21_bootprot_bytes,
};
use lamella_flash_routes::{Programmer, programmer_for, route_for, selector_for};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The option that clears BOOTPROT.
pub const CLEAR_BOOTPROT: &str = "--clear-bootprot";

/// The option that writes a saved copy of the row back; it takes the saved file.
pub const RESTORE_USER_ROW: &str = "--restore-user-row";

/// The option that stops either step after its printout, with nothing written.
pub const DRY_RUN: &str = "--dry-run";

/// Runs `--clear-bootprot` for the board `parsed` names.
pub fn clear_bootprot_command(parsed: &Options) -> ExitCode {
    finish(run_clear(parsed))
}

/// Runs `--restore-user-row <file>` for the board `parsed` names.
pub fn restore_user_row_command(parsed: &Options, file: &str) -> ExitCode {
    finish(run_restore(parsed, Path::new(file)))
}

fn finish(outcome: Result<(), String>) -> ExitCode {
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("{}", why.trim_end());
            ExitCode::FAILURE
        }
    }
}

fn refusal(why: impl std::fmt::Display) -> String {
    format!("lamella flash: {why}")
}

/// The board a user-row step acts on, the route that reaches it, and the probe chosen for it.
struct Target<'a> {
    board: &'a str,
    programmer: Programmer,
    probe: Option<String>,
}

/// Settles which board and which probe `step` acts on. It refuses an image, `--via`, and any board
/// that is not a SAM D21 reached through a probe.
fn target<'a>(parsed: &'a Options, step: &str) -> Result<Target<'a>, String> {
    if !parsed.positionals().is_empty() {
        return Err(refusal(format!(
            "{step} changes the part's user row and writes no image, so {} was not written.\n\
             Write the image as its own command.",
            parsed.positionals().join(" ")
        )));
    }
    if parsed.value("--via").is_some() {
        return Err(refusal(format!(
            "{step} reaches the part through the board's own debugger, so --via has nothing to \
             choose."
        )));
    }
    let board = parsed.value("--board").ok_or_else(|| {
        refusal(format!(
            "{step} needs --board, which says whose user row is changed."
        ))
    })?;
    let row = programmer_for(board)?;
    let programmer = route_for(row, None).map_err(refusal)?;
    if !user_row::reaches_a_samd21(programmer) {
        return Err(refusal(format!(
            "{step} is for a SAM D21 reached through a probe, and {board} is written through {}.",
            programmer.description()
        )));
    }
    let selector = selector_for(
        programmer,
        parsed.value("--probe"),
        parsed.value("--volume"),
        parsed.value("--device"),
    )
    .map_err(refusal)?;
    let probe = crate::flash::choose_board(programmer, selector.as_deref()).map_err(refusal)?;
    Ok(Target {
        board,
        programmer,
        probe,
    })
}

fn run_clear(parsed: &Options) -> Result<(), String> {
    let target = target(parsed, CLEAR_BOOTPROT)?;
    let reading = user_row::read(target.programmer, target.probe.as_deref()).map_err(refusal)?;
    let after = reading
        .row
        .with_bootprot(SAMD21_BOOTPROT_NONE)
        .expect("7 fits the three-bit field");
    println!("{}", plan(target.board, &reading, &after));
    if after == reading.row {
        println!(
            "\nBOOTPROT is already {SAMD21_BOOTPROT_NONE}: this board protects no bootloader rows, so \
             there is nothing to write."
        );
        return Ok(());
    }
    if parsed.flag(DRY_RUN) {
        println!(
            "\n{DRY_RUN}: nothing was written. Without it, the row as read is saved to a file here \
             first,\nthen erased and written back with only BOOTPROT changed, read back and \
             compared, and the\npart is reset so that the new value is in force. \
             {RESTORE_USER_ROW} with that file is the way back."
        );
        return Ok(());
    }
    let saved = save_before_writing(&target, &reading)?;
    println!("erasing and rewriting the user row...");
    let outcome = user_row::set_bootprot(
        target.programmer,
        target.probe.as_deref(),
        &reading,
        SAMD21_BOOTPROT_NONE,
    );
    report(&target, outcome, &saved)
}

fn run_restore(parsed: &Options, file: &Path) -> Result<(), String> {
    if parsed.flag(CLEAR_BOOTPROT) {
        return Err(refusal(format!(
            "{CLEAR_BOOTPROT} and {RESTORE_USER_ROW} are two different writes; ask for one."
        )));
    }
    let target = target(parsed, RESTORE_USER_ROW)?;
    let saved_serial = serial_from_name(file).ok_or_else(|| {
        refusal(format!(
            "{} does not carry a part's serial number in its name, as every copy \
             {CLEAR_BOOTPROT}\nsaves does. A row holds calibration that belongs to one part, so a \
             copy is only written back\nto the part it names.",
            file.display()
        ))
    })?;
    let bytes =
        std::fs::read(file).map_err(|why| refusal(format!("reading {}: {why}", file.display())))?;
    let saved = Samd21UserRow::from_bytes(&bytes).ok_or_else(|| {
        refusal(format!(
            "{} holds {} bytes, and a saved user row is 256.",
            file.display(),
            bytes.len()
        ))
    })?;
    let reading = user_row::read(target.programmer, target.probe.as_deref()).map_err(refusal)?;
    if saved_serial != reading.serial {
        return Err(refusal(format!(
            "{} is named for the part with serial number {}, and this part's is {};\na copy is \
             only written back to the part it came from, so nothing was written.",
            file.display(),
            user_row::serial_text(&saved_serial),
            user_row::serial_text(&reading.serial)
        )));
    }
    println!("{}", plan(target.board, &reading, &saved));
    if reading.row == saved {
        println!(
            "\nthe row already reads as {}, so there is nothing to write.",
            file.display()
        );
        return Ok(());
    }
    if !reading.row.is_partway_to(&saved) {
        return Err(refusal(format!(
            "the row holds a bit at zero, outside BOOTPROT, that {} holds at one, so the row is \
             not\nthat copy part-way through a BOOTPROT change, and nothing was written.",
            file.display()
        )));
    }
    if parsed.flag(DRY_RUN) {
        println!(
            "\n{DRY_RUN}: nothing was written. Without it, the row as read is saved to a file here \
             first,\nthen erased and written back as\n\n\x20   {}\n\nread back and compared, and \
             the part is reset so that it is in force.",
            file.display()
        );
        return Ok(());
    }
    save_before_writing(&target, &reading)?;
    println!("erasing and writing the saved row back...");
    let outcome = user_row::restore(
        target.programmer,
        target.probe.as_deref(),
        &reading,
        &saved,
        &saved_serial,
    );
    report(&target, outcome, file)
}

/// Saves the row as read before anything is written, and says where.
fn save_before_writing(target: &Target<'_>, reading: &UserRowReading) -> Result<PathBuf, String> {
    let path = backup_path(target.board, &reading.serial, now_seconds());
    save(&path, &reading.row).map_err(refusal)?;
    println!("\nsaved the row as read to {}", path.display());
    Ok(path)
}

/// What a person is told after a write: in force; written but not yet in force; not written; or
/// erased and not written back, when `put_back` is the copy that restores it.
fn report(
    target: &Target<'_>,
    outcome: Result<UserRowWritten, Samd21UserRowError>,
    put_back: &Path,
) -> Result<(), String> {
    match outcome {
        Ok(written) => {
            let bootprot = written.row.bootprot();
            match written.reset {
                Ok(()) => println!(
                    "the user row reads back as planned, and the part was reset, so it is in \
                     force: BOOTPROT\n{bootprot}, {}.",
                    protected(bootprot)
                ),
                Err(why) => println!(
                    "the user row reads back as planned, but resetting the part failed ({why}).\n\
                     It still runs on its previous configuration until it resets: press the \
                     board's RESET\nbutton, or unplug it and plug it back in."
                ),
            }
            Ok(())
        }
        Err(Samd21UserRowError::Unchanged(why)) => {
            Err(refusal(format!("{why}; nothing was written.")))
        }
        Err(Samd21UserRowError::NotRewritten { reason, now }) => Err(refusal(not_rewritten(
            &reason,
            now.as_deref(),
            target.board,
            put_back,
        ))),
    }
}

/// The printout a person reads before anything is written: the part, every field of the row's
/// configuration now and after, and the words that change.
fn plan(board: &str, reading: &UserRowReading, after: &Samd21UserRow) -> String {
    let before = &reading.row;
    let mut text = format!(
        "the user row of {board}, read twice with the same answer (a SAM D21, DID {:#010x}),\n\
         from the part whose serial number is {}:\n\n",
        reading.part.raw,
        user_row::serial_text(&reading.serial)
    );
    text.push_str(&format!(
        "  {:<26}{:>7}{:>9}{:>9}\n",
        "field", "bits", "now", "after"
    ));
    for field in SAMD21_USER_ROW_FIELDS {
        let (now, then) = (before.field(field), after.field(field));
        text.push_str(&format!(
            "  {:<26}{:>7}{:>9}{:>9}",
            field.name,
            bits(field),
            value(field, now),
            value(field, then)
        ));
        if field.name == "BOOTPROT" {
            text.push_str(&format!(
                "   {} -> {}",
                protected(now as u8),
                protected(then as u8)
            ));
        }
        text.push('\n');
    }
    text.push('\n');
    for index in 0..2 {
        let (now, then) = (before.words[index], after.words[index]);
        if now == then {
            text.push_str(&format!("  word {index}  {now:#010x}, unchanged\n"));
        } else {
            text.push_str(&format!("  word {index}  {now:#010x} -> {then:#010x}\n"));
        }
    }
    let changed: Vec<usize> = (2..before.words.len())
        .filter(|index| before.words[*index] != after.words[*index])
        .collect();
    if changed.is_empty() {
        text.push_str(&format!(
            "  words 2 to {}, unchanged",
            before.words.len() - 1
        ));
    } else {
        for index in changed {
            text.push_str(&format!(
                "  word {index}  {:#010x} -> {:#010x}\n",
                before.words[index], after.words[index]
            ));
        }
    }
    text.trim_end_matches('\n').to_owned()
}

/// A field's bits as the datasheet's table writes them: `2:0`, or `3` for a single bit.
fn bits(field: &Samd21UserRowField) -> String {
    match field.width {
        1 => field.low_bit.to_string(),
        width => format!("{}:{}", field.low_bit + width - 1, field.low_bit),
    }
}

/// A field's value: decimal up to three bits wide, hexadecimal from four.
fn value(field: &Samd21UserRowField, value: u32) -> String {
    if field.width < 4 {
        value.to_string()
    } else {
        format!("{value:#x}")
    }
}

/// What a BOOTPROT value protects, in words.
fn protected(bootprot: u8) -> String {
    match samd21_bootprot_bytes(bootprot) {
        0 => "no rows protected".to_owned(),
        bytes => format!("{bytes} bytes protected"),
    }
}

/// What a person is told when the row was erased and not written back.
fn not_rewritten(
    reason: &str,
    now: Option<&Samd21UserRow>,
    board: &str,
    put_back: &Path,
) -> String {
    let mut text = format!(
        "the user row was erased and did not read back as intended: {reason}\n\n\
         THE PART STILL RUNS ON ITS PREVIOUS CONFIGURATION, and only until it resets. Do not \
         press its RESET\nbutton, unplug it, or run anything that resets it. Put the row back \
         first:\n\n\
         \x20   lamella flash --board {board} {RESTORE_USER_ROW} {}\n\n",
        put_back.display()
    );
    match now {
        Some(row) => text.push_str(&format!(
            "The row now reads {:#010x} {:#010x} in its first two words.",
            row.words[0], row.words[1]
        )),
        None => text.push_str("The row could not be read back."),
    }
    text
}

/// Where the row is saved before a write: the working directory, named for the board, the part's
/// serial number, and the time.
///
/// The serial number is in the name because the row holds calibration that belongs to that one
/// part, so a saved copy is only ever written back to the part it came from.
fn backup_path(board: &str, serial: &[u32; 4], seconds: u64) -> PathBuf {
    PathBuf::from(format!(
        "{board}-user-row-{}-{}.bin",
        user_row::serial_text(serial),
        utc_stamp(seconds)
    ))
}

/// The serial number a saved copy's name carries, as [`backup_path`] writes it: the 32 hexadecimal
/// digits after `-user-row-`, word 0 first. `None` for any other name.
fn serial_from_name(path: &Path) -> Option<[u32; 4]> {
    let name = path.file_name()?.to_str()?;
    let (_, rest) = name.split_once("-user-row-")?;
    let digits = rest.get(..32)?;
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit())
        || rest.as_bytes().get(32) != Some(&b'-')
    {
        return None;
    }
    let mut serial = [0u32; 4];
    for (index, word) in serial.iter_mut().enumerate() {
        *word = u32::from_str_radix(&digits[index * 8..index * 8 + 8], 16).ok()?;
    }
    Some(serial)
}

/// Saves `row` to `path`, refusing to replace a file that is already there.
fn save(path: &Path, row: &Samd21UserRow) -> Result<(), String> {
    use std::io::Write as _;

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|why| {
            format!(
                "saving the row to {} before writing anything: {why}; nothing was written",
                path.display()
            )
        })?;
    file.write_all(&row.to_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|why| {
            format!(
                "saving the row to {} before writing anything: {why}; nothing was written",
                path.display()
            )
        })
}

fn now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// `seconds` since the Unix epoch as a UTC date and time, `20260924T081500Z`.
fn utc_stamp(seconds: u64) -> String {
    let days = i64::try_from(seconds / 86_400).unwrap_or(i64::MAX / 2);
    let of_day = seconds % 86_400;
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        of_day / 3_600,
        of_day / 60 % 60,
        of_day % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zero() -> UserRowReading {
        let mut words = [u32::MAX; 64];
        words[0] = 0xd8e0_c7ff;
        words[1] = 0xffff_fc5d;
        UserRowReading {
            part: user_row::SamDeviceId::decode(0x1001_0305),
            serial: [0x0123_4567, 0x89ab_cdef, 0x0011_2233, 0x4455_6677],
            row: Samd21UserRow { words },
        }
    }

    #[test]
    fn the_plan_for_a_protected_row_shows_only_bootprot_changing() {
        let mut reading = zero();
        reading.row = reading.row.with_bootprot(2).unwrap();
        let after = reading.row.with_bootprot(SAMD21_BOOTPROT_NONE).unwrap();
        let text = plan("arduino-zero", &reading, &after);
        assert!(text.contains("DID 0x10010305"), "{text}");
        assert!(
            text.contains("BOOTPROT                      2:0        2        7   8192 bytes protected -> no rows protected"),
            "{text}"
        );
        assert!(text.contains("word 0  0xd8e0c7fa -> 0xd8e0c7ff"), "{text}");
        assert!(text.contains("word 1  0xfffffc5d, unchanged"), "{text}");
        assert!(text.contains("words 2 to 63, unchanged"), "{text}");
        for line in text
            .lines()
            .filter(|line| line.contains("0x70") || line.contains("LOCK"))
        {
            let values: Vec<&str> = line.split_whitespace().rev().take(2).collect();
            assert_eq!(values[0], values[1], "{line}");
        }
    }

    /// The whole printout for a board whose bootloader is protected, as a person reads it before
    /// anything is written. The Zero's own row with BOOTPROT at 2, which is what a board reads
    /// after a bootloader burn that sets protection.
    #[test]
    fn the_plan_for_a_protected_row_reads_in_full() {
        let mut reading = zero();
        reading.row = reading.row.with_bootprot(2).unwrap();
        let after = reading.row.with_bootprot(SAMD21_BOOTPROT_NONE).unwrap();
        assert_eq!(
            plan("arduino-zero", &reading, &after),
            "\
the user row of arduino-zero, read twice with the same answer (a SAM D21, DID 0x10010305),
from the part whose serial number is 0123456789abcdef0011223344556677:

  field                        bits      now    after
  BOOTPROT                      2:0        2        7   8192 bytes protected -> no rows protected
  reserved                        3        1        1
  EEPROM                        6:4        7        7
  reserved                        7        1        1
  BOD33 level                  13:8      0x7      0x7
  BOD33 enable                   14        1        1
  BOD33 action                16:15        1        1
  BOD12 configuration         24:17     0x70     0x70
  WDT enable                     25        0        0
  WDT always-on                  26        0        0
  WDT period                  30:27      0xb      0xb
  WDT window                  34:31      0xb      0xb
  WDT early-warning offset    38:35      0xb      0xb
  WDT window mode                39        0        0
  BOD33 hysteresis               40        0        0
  BOD12 configuration            41        0        0
  reserved                    47:42     0x3f     0x3f
  LOCK                        63:48   0xffff   0xffff

  word 0  0xd8e0c7fa -> 0xd8e0c7ff
  word 1  0xfffffc5d, unchanged
  words 2 to 63, unchanged"
        );
    }

    #[test]
    fn the_plan_for_the_zero_as_found_changes_nothing() {
        let reading = zero();
        let after = reading.row.with_bootprot(SAMD21_BOOTPROT_NONE).unwrap();
        assert_eq!(after, reading.row);
        let text = plan("arduino-zero", &reading, &after);
        assert!(
            !text
                .lines()
                .any(|line| line.trim_start().starts_with("word") && line.contains("->")),
            "{text}"
        );
        assert!(!text.contains("->  "), "{text}");
        assert_eq!(
            text.lines().filter(|line| line.starts_with("  ")).count(),
            1 + 18 + 3
        );
    }

    #[test]
    fn a_fields_bits_read_as_the_table_writes_them() {
        let [bootprot, reserved, ..] = SAMD21_USER_ROW_FIELDS else {
            unreachable!()
        };
        assert_eq!(bits(bootprot), "2:0");
        assert_eq!(bits(reserved), "3");
        assert_eq!(bits(SAMD21_USER_ROW_FIELDS.last().unwrap()), "63:48");
    }

    #[test]
    fn a_utc_stamp_counts_leap_days() {
        assert_eq!(utc_stamp(0), "19700101T000000Z");
        assert_eq!(utc_stamp(951_782_400), "20000229T000000Z");
        assert_eq!(utc_stamp(951_868_800), "20000301T000000Z");
        assert_eq!(
            utc_stamp(1_790_208_000 + 8 * 3_600 + 15 * 60 + 9),
            "20260924T081509Z"
        );
        assert_eq!(
            backup_path("arduino-zero", &zero().serial, 1_790_208_000),
            PathBuf::from(
                "arduino-zero-user-row-0123456789abcdef0011223344556677-20260924T000000Z.bin"
            )
        );
    }

    #[test]
    fn a_save_never_replaces_a_file() {
        let dir = std::env::temp_dir().join(format!("lamella-bootprot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("row.bin");
        let _ = std::fs::remove_file(&path);
        save(&path, &zero().row).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), zero().row.to_bytes());
        let why = save(&path, &zero().row).unwrap_err();
        assert!(why.contains("nothing was written"), "{why}");
        assert_eq!(
            std::fs::read(&path).unwrap(),
            zero().row.to_bytes(),
            "the first copy stands"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_row_that_was_not_written_back_says_not_to_reset_and_how_to_put_it_back() {
        let text = not_rewritten("an NVM error", None, "arduino-zero", Path::new("row.bin"));
        assert!(text.contains("Do not") && text.contains("RESET"), "{text}");
        assert!(
            text.contains("    lamella flash --board arduino-zero --restore-user-row row.bin"),
            "{text}"
        );
    }

    /// A saved copy's name carries its part's serial number, and a name that does not is refused
    /// rather than read as some serial -- one that carries a probe's serial instead among them.
    #[test]
    fn a_saved_copy_is_named_for_its_part() {
        let serial = zero().serial;
        let path = backup_path("arduino-zero", &serial, 1_790_208_000);
        assert_eq!(serial_from_name(&path), Some(serial));
        assert_eq!(
            serial_from_name(&Path::new("some/dir").join(&path)),
            Some(serial)
        );
        for other in [
            "arduino-zero-XTS10BBAM6YK0R442KSF-user-row-2026-09-23.bin",
            "arduino-zero-user-row-0123456789abcdef0011223344556677.bin",
            "arduino-zero-user-row-+123456789abcdef0011223344556677-x.bin",
            "row.bin",
        ] {
            assert_eq!(serial_from_name(Path::new(other)), None, "{other}");
        }
    }
}
