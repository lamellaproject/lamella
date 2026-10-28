//! `lamella run --emulator <command>`: say how the program runs, then start the emulator command a
//! device SDK composed for it, and end as the emulator ends.
//!
//! This is the run a device SDK's `dotnet run` asks for when the device it was given is an emulator,
//! not a command a person types: it is not in `lamella run --help`, and its arguments may change with
//! the SDK that passes them. The SDK composes the emulator's command, an executable and its
//! arguments, and hands both here instead of starting it itself, so that the run's first line says
//! how the program runs, as it does on every other device: `running the native image in the
//! <machine> emulator.`, on standard error with this tool's `lamella:` prefix. Everything after that line is the
//! emulator's, unless the emulator cannot be started, which this tool then says, ending with 1.
//!
//! **THE EMULATOR IS GIVEN EXACTLY THE ARGUMENTS IT WOULD HAVE HAD WITHOUT THIS TOOL.**
//! `--emulator` names its executable, and everything after the first `--` is its arguments: the
//! SDK's, then whatever `dotnet run` appends for the program. On Windows that text is handed on as
//! this tool's own command line holds it, so the emulator reads the very command line `dotnet run`
//! would have given it. On Linux and macOS this tool becomes the emulator, in the same process, with
//! the arguments as `dotnet run` split them.
//!
//! **THE RUN ENDS AS THE EMULATOR ENDS**, with its exit code unchanged. The emulator shares this
//! tool's console, so Ctrl+C reaches it as it reaches any program `dotnet run` starts, and the
//! emulator decides how it ends.

use crate::args;
use crate::ending::Ended;
use crate::own_lines::say;
use std::io::Write;

/// The option that asks for this run, and names the emulator's executable.
pub(crate) const EMULATOR: &str = "--emulator";

/// `lamella run --emulator <command> --device <id> [--board <id>] -- <arguments>`, from the words
/// before the first `--` (`options`) and the emulator's arguments after it (`arguments`).
pub(crate) fn run_emulator(options: &[String], arguments: &[String]) -> Ended {
    run_with(options, arguments, &mut std::io::stderr(), start)
}

/// [`run_emulator`], saying this tool's lines on `err` and starting the emulator with `start`.
fn run_with(
    options: &[String],
    arguments: &[String],
    err: &mut dyn Write,
    start: impl FnOnce(&str, &[String]) -> Ended,
) -> Ended {
    let spec = args::Spec {
        verb: "run",
        usage: Some(crate::program::RUN_USAGE),
        values: &[EMULATOR, "--device", "--board"],
        repeats: &[],
        flags: &[],
        positional: args::Positional::Own,
    };
    let parsed = match args::parse_or_halt(options, &spec) {
        Ok(parsed) => parsed,
        Err(args::Halt::Help) => return Ended::Program(0),
        Err(args::Halt::Bad) => return Ended::Failed,
    };
    if let Some(word) = parsed.positionals().first() {
        say(err, &format!("{word:?} is not an option of a run on an emulator: the emulator's arguments come after --."));
        return Ended::Failed;
    }
    let emulator = parsed.value(EMULATOR).unwrap_or_default();
    if emulator.is_empty() {
        say(err, &format!("{EMULATOR} names no emulator to start."));
        return Ended::Failed;
    }
    let Some(device) = parsed.value("--device").filter(|device| !device.is_empty()) else {
        say(err, "a run on an emulator names the device it runs on, with --device <device>.");
        return Ended::Failed;
    };
    say(err, &model_line(device));
    start(emulator, arguments)
}

/// The first line of a run on an emulator, which says how the program runs: as a native image, in
/// the emulator of the machine `device` names, `emulator:<machine>`.
fn model_line(device: &str) -> String {
    let machine = device.get(..EMULATOR_ID.len()).filter(|scheme| scheme.eq_ignore_ascii_case(EMULATOR_ID));
    crate::own_lines::running_in_emulator(machine.map_or(device, |scheme| &device[scheme.len()..]))
}

/// How a device list names an emulator: `emulator:` and its machine.
const EMULATOR_ID: &str = "emulator:";

/// Starts `emulator`, and ends as it ends. Its command line is this tool's own after the first
/// `--`, as the line holds it, or, should the line hold no `--` where the arguments did, the
/// arguments as they were split.
#[cfg(windows)]
fn start(emulator: &str, arguments: &[String]) -> Ended {
    use std::os::windows::ffi::OsStringExt as _;
    use std::os::windows::process::CommandExt as _;
    let mut command = std::process::Command::new(emulator);
    let line = own_command_line();
    match after_separator(&line) {
        Some(text) if !text.is_empty() => {
            command.raw_arg(std::ffi::OsString::from_wide(text));
        }
        Some(_) => {}
        None => {
            command.args(arguments);
        }
    }
    crate::ending::watch_for_ctrl_c();
    match command.status() {
        Ok(status) => Ended::Program(status.code().unwrap_or(1)),
        Err(error) => {
            say(&mut std::io::stderr(), &format!("could not start the emulator {emulator}: {error}"));
            Ended::Failed
        }
    }
}

/// Becomes `emulator`, started with `arguments`: this process is the emulator from here on, so the
/// run ends as it ends. Returns only when it cannot be started.
#[cfg(unix)]
fn start(emulator: &str, arguments: &[String]) -> Ended {
    use std::os::unix::process::CommandExt as _;
    let error = std::process::Command::new(emulator).args(arguments).exec();
    say(&mut std::io::stderr(), &format!("could not start the emulator {emulator}: {error}"));
    Ended::Failed
}

/// This process's command line, as Windows holds it.
#[cfg(windows)]
#[allow(unsafe_code)]
fn own_command_line() -> Vec<u16> {
    unsafe {
        let line = windows_sys::Win32::System::Environment::GetCommandLineW();
        let mut length = 0;
        while *line.add(length) != 0 {
            length += 1;
        }
        std::slice::from_raw_parts(line, length).to_vec()
    }
}

/// The text of a Windows command line after its first argument that is `--`, from the first
/// character past the spaces and tabs that end that argument: the rest of the line exactly as it
/// stands. `None` when no argument after the program's name is `--`.
///
/// The arguments are split by the rules the C runtime splits a command line by, which Rust's
/// standard library follows too, so the argument found here is the one this tool's own arguments
/// took for the first `--`.
#[cfg_attr(not(windows), allow(dead_code))]
fn after_separator(line: &[u16]) -> Option<&[u16]> {
    let separator = [u16::from(b'-'), u16::from(b'-')];
    let split = split_arguments(line);
    let end = split.iter().skip(1).find(|argument| argument.value == separator)?.end;
    Some(&line[end + blanks(&line[end..])..])
}

/// One argument of a command line: what it says, and where its text ends in the line.
#[cfg_attr(not(windows), allow(dead_code))]
struct Argument {
    value: Vec<u16>,
    end: usize,
}

/// How many of the units `text` starts with are spaces or tabs.
#[cfg_attr(not(windows), allow(dead_code))]
fn blanks(text: &[u16]) -> usize {
    text.iter().take_while(|&&unit| is_blank(unit)).count()
}

/// Whether `unit` is a space or a tab, the two units that end an argument outside quotes.
#[cfg_attr(not(windows), allow(dead_code))]
fn is_blank(unit: u16) -> bool {
    unit == u16::from(b' ') || unit == u16::from(b'\t')
}

/// The arguments of a Windows command line, the program's name first.
///
/// The program's name ends at the first space or tab outside quotes, and a quote only opens or
/// closes quoting there. Each argument after it ends at a space or tab outside quotes; a quote opens
/// or closes quoting unless backslashes escape it; backslashes before a quote are halved, an odd one
/// left over making the quote a character of the argument; other backslashes are kept; and inside
/// quotes two quotes are one quote character.
#[cfg_attr(not(windows), allow(dead_code))]
fn split_arguments(line: &[u16]) -> Vec<Argument> {
    const QUOTE: u16 = b'"' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    let mut split = Vec::new();
    let mut at = 0;
    let mut value = Vec::new();
    let mut quoted = false;
    while let Some(&unit) = line.get(at) {
        if unit == QUOTE {
            quoted = !quoted;
        } else if is_blank(unit) && !quoted {
            break;
        } else {
            value.push(unit);
        }
        at += 1;
    }
    split.push(Argument { value: std::mem::take(&mut value), end: at });
    at += blanks(&line[at..]);
    quoted = false;
    while let Some(&unit) = line.get(at) {
        if is_blank(unit) && !quoted {
            split.push(Argument { value: std::mem::take(&mut value), end: at });
            at += blanks(&line[at..]);
            continue;
        }
        if unit == BACKSLASH {
            let run = line[at..].iter().take_while(|&&unit| unit == BACKSLASH).count();
            at += run;
            if line.get(at) == Some(&QUOTE) {
                value.extend(std::iter::repeat_n(BACKSLASH, run / 2));
                if run % 2 == 1 {
                    value.push(QUOTE);
                    at += 1;
                }
            } else {
                value.extend(std::iter::repeat_n(BACKSLASH, run));
            }
            continue;
        }
        if unit == QUOTE && quoted {
            match line.get(at + 1) {
                Some(&QUOTE) => {
                    value.push(QUOTE);
                    at += 2;
                    continue;
                }
                Some(_) => quoted = false,
                None => {
                    at += 1;
                    break;
                }
            }
        } else if unit == QUOTE {
            quoted = true;
        } else {
            value.push(unit);
        }
        at += 1;
    }
    if !value.is_empty() || quoted {
        split.push(Argument { value, end: at });
    }
    split
}

#[cfg(test)]
mod tests {
    use super::*;

    fn units(text: &str) -> Vec<u16> {
        text.encode_utf16().collect()
    }

    fn tail(line: &str) -> Option<String> {
        let line = units(line);
        after_separator(&line).map(String::from_utf16_lossy)
    }

    fn values(line: &str) -> Vec<String> {
        split_arguments(&units(line)).iter().map(|argument| String::from_utf16_lossy(&argument.value)).collect()
    }

    fn words(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_owned()).collect()
    }

    /// **THE MODEL LINE COMES FIRST, THEN THE EMULATOR STARTS**, as `--emulator` names it, with the
    /// arguments after the `--` and nothing else; the run ends as the emulator ended.
    #[test]
    fn a_run_says_how_the_program_runs_then_starts_the_emulator_with_its_arguments() {
        let mut err = Vec::new();
        let mut started = None;
        let options = words(&["--emulator", r"C:\Tools\qemu-system-arm.exe", "--device", "emulator:mps2-an505", "--board", "arm-mps2-an505"]);
        let ended = run_with(&options, &words(&["-M", "mps2-an505"]), &mut err, |emulator, arguments| {
            started = Some((emulator.to_owned(), arguments.to_vec()));
            Ended::Program(5)
        });
        assert_eq!(ended, Ended::Program(5));
        assert_eq!(String::from_utf8(err).expect("text"), "lamella: running the native image in the mps2-an505 emulator.\n");
        assert_eq!(started, Some((r"C:\Tools\qemu-system-arm.exe".to_owned(), words(&["-M", "mps2-an505"]))));
    }

    /// **A RUN THAT NAMES NO EMULATOR OR NO DEVICE STARTS NOTHING**, and neither does one with a
    /// word before the `--` that is no option: each is refused in a sentence of this tool's, before
    /// the model line, and ends with 1.
    #[test]
    fn a_run_that_names_no_emulator_or_no_device_starts_nothing() {
        for options in [
            &["--device", "emulator:mps2-an505"][..],
            &["--emulator", "", "--device", "emulator:mps2-an505"],
            &["--emulator", "qemu-system-arm", "--board", "arm-mps2-an505"],
            &["--emulator", "qemu-system-arm", "--device", ""],
            &["--emulator", "qemu-system-arm", "--device", "emulator:mps2-an505", "stray"],
        ] {
            let mut err = Vec::new();
            let ended = run_with(&words(options), &[], &mut err, |_, _| panic!("the emulator was started for {options:?}"));
            assert_eq!(ended, Ended::Failed, "{options:?}");
            let said = String::from_utf8(err).expect("text");
            assert!(said.starts_with("lamella: ") && !said.contains("native image"), "{options:?}: {said}");
        }
    }

    /// **A RUN ON AN EMULATOR SAYS HOW THE PROGRAM RUNS, FIRST**: the native image, in the emulator
    /// of the machine the device names, never in words that say the program is on the board.
    #[test]
    fn a_runs_first_line_says_the_program_runs_on_the_emulator_as_a_native_image() {
        assert_eq!(model_line("emulator:mps2-an505"), "running the native image in the mps2-an505 emulator.");
        assert_eq!(model_line("Emulator:mps2-an505"), "running the native image in the mps2-an505 emulator.");
    }

    /// **WHAT FOLLOWS THE FIRST `--` IS HANDED ON AS THE LINE HOLDS IT**: its quotes, its spacing and
    /// a quoted path with a space in it, and the program's own arguments after the SDK's.
    #[test]
    fn the_text_after_the_first_separator_is_the_line_as_it_stands() {
        let line = r#""C:\Program Files\Lamella\lamella.exe" run --emulator "C:\Tools\qemu-system-arm.exe" --device "emulator:mps2-an505" --board "arm-mps2-an505" -- -M mps2-an505  -kernel "D:\Work\a b\App.elf" one "two words""#;
        assert_eq!(tail(line).as_deref(), Some(r#"-M mps2-an505  -kernel "D:\Work\a b\App.elf" one "two words""#));
        assert_eq!(tail("lamella run --emulator q --\t  -M  x").as_deref(), Some("-M  x"), "the blanks that end the -- are not the emulator's");
        assert_eq!(tail("lamella run --emulator q --").as_deref(), Some(""), "a -- that ends the line leaves nothing");
        assert_eq!(tail("lamella run --emulator q --device d").as_deref(), None, "no --, nothing found");
    }

    /// **THE `--` FOUND IS THE ONE THE TOOL'S OWN ARGUMENTS TOOK**: the first argument that says
    /// `--`, quoted or not, and never a `--` inside another argument, or the program's name.
    #[test]
    fn the_separator_found_is_the_first_argument_that_says_two_dashes() {
        assert_eq!(tail(r#"lamella run --emulator "a -- b" -- c"#).as_deref(), Some("c"));
        assert_eq!(tail(r#"lamella run "--" c -- d"#).as_deref(), Some("c -- d"));
        assert_eq!(tail("-- run --emulator q -- c").as_deref(), Some("c"), "the program's name is not an argument");
        assert_eq!(tail(r#"lamella run --emulator "C:\dir\\" -- c"#).as_deref(), Some("c"));
        assert_eq!(tail(r#"lamella run --emulator a\"-- -- c"#).as_deref(), Some("c"));
    }

    /// **THE LINE IS SPLIT AS THE C RUNTIME SPLITS IT**: the program's name takes no escapes;
    /// backslashes are halved before a quote, an odd one escaping it, and kept elsewhere; two quotes
    /// inside quotes are one; and a final empty pair of quotes is an argument.
    #[test]
    fn a_command_line_is_split_by_the_c_runtimes_rules() {
        assert_eq!(values(r#""C:\a b\x.exe" a"#), ["C:\\a b\\x.exe", "a"]);
        assert_eq!(values(r#"x "a b" c"#), ["x", "a b", "c"]);
        assert_eq!(values(r#"x a\\\"b c\\d e\\"f g""#), ["x", "a\\\"b", "c\\\\d", "e\\f g"]);
        assert_eq!(values(r#"x "a""b" c"#), ["x", "a\"b", "c"]);
        assert_eq!(values(r#"x a """#), ["x", "a", ""]);
        assert_eq!(values("x  a\t\tb  "), ["x", "a", "b"]);
    }
}
