//! The shared option parser for every `lamella` verb.

use std::fmt::Write as _;
use std::process::ExitCode;

/// One verb's command line, after parsing.
#[derive(Debug)]
pub struct Options {
    positional: Vec<String>,
    named: Vec<(String, String)>,
    flags: Vec<String>,
}

/// What a verb accepts: options that take a value, and options that stand alone.
pub struct Spec<'a> {
    /// The verb's name, for the error text ("lamella deploy: ...").
    pub verb: &'a str,
    /// The verb's own usage text, shown for `--help` and beside its errors.
    ///
    /// `None` where a verb has no prose of its own; `--help` then falls back to the accepted-options
    /// line, which is derived and so cannot go stale.
    pub usage: Option<&'a str>,
    /// Options taking a value, as `--board <id>`.
    pub values: &'a [&'a str],
    /// Options standing alone, as `--identify`.
    pub flags: &'a [&'a str],
}

/// Why a verb should stop before doing any work, and what it should exit with.
///
/// **THE EXIT CODE IS DECIDED IN ONE PLACE.** Asking for help SUCCEEDS and mistyping an option
/// FAILS, and a tool that returned the same code for both could not be scripted around. Every verb
/// maps this to its exit code the same way, so the distinction cannot hold at one verb and be
/// missing at another.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Halt {
    /// `--help` was asked for; the text has been printed to stdout.
    Help,
    /// The command line was wrong; the reason has been printed to stderr.
    Bad,
}

impl Halt {
    /// What the verb should return from `main`.
    #[must_use]
    pub fn code(self) -> ExitCode {
        match self {
            Self::Help => ExitCode::SUCCESS,
            Self::Bad => ExitCode::FAILURE,
        }
    }
}

impl Options {
    /// The value given for `name`, if any.
    #[must_use]
    pub fn value(&self, name: &str) -> Option<&str> {
        self.named
            .iter()
            .find(|(option, _)| option == name)
            .map(|(_, value)| value.as_str())
    }

    /// Whether the standalone option `name` was given.
    #[must_use]
    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|option| option == name)
    }

    /// The single positional word this verb wants, or an error naming what was found instead.
    ///
    /// # Errors
    /// When no word, or more than one, was given.
    pub fn only_positional(&self, verb: &str, what: &str) -> Result<&str, String> {
        match self.positional.as_slice() {
            [one] => Ok(one),
            [] => Err(format!("lamella {verb}: give a {what}")),
            many => Err(format!(
                "lamella {verb}: wanted one {what}, got {}: {}",
                many.len(),
                many.join(" ")
            )),
        }
    }
}

/// Parse `args` against `spec`.
///
/// # Errors
/// An option the verb does not declare, or a value option given no value. Both messages list every
/// option the verb accepts, because the reader is looking at this text precisely because they did
/// not know.
pub fn parse(args: &[String], spec: &Spec) -> Result<Options, String> {
    let mut parsed =
        Options { positional: Vec::new(), named: Vec::new(), flags: Vec::new() };
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        if let Some(name) = argument.strip_prefix("--") {
            let name = format!("--{name}");
            if spec.flags.contains(&name.as_str()) {
                parsed.flags.push(name);
            } else if spec.values.contains(&name.as_str()) {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(format!(
                        "lamella {}: {name} wants a value.\n{}",
                        spec.verb,
                        accepted(spec)
                    ));
                };
                parsed.named.push((name, value.clone()));
            } else {
                return Err(format!(
                    "lamella {}: unknown option {name:?}.\n{}",
                    spec.verb,
                    accepted(spec)
                ));
            }
        } else {
            parsed.positional.push(argument.to_owned());
        }
        index += 1;
    }
    Ok(parsed)
}

/// Parse `args`, or print the right thing and say how the verb should exit.
///
/// **`--help` IS ANSWERED HERE, FOR EVERY VERB AT ONCE.** It is the first thing anybody types at a
/// subcommand, and answering it in the shared parser is what makes every verb answer it the same
/// way: the verbs' call sites are otherwise identical, so a case added to one of them would be a
/// case missing from the rest.
///
/// `-h` is taken as well, and it has to be: this parser recognizes only `--` options, so a bare
/// `-h` would otherwise be read as a POSITIONAL and `lamella flash -h` would go looking for a file
/// named `-h`.
///
/// # Errors
/// [`Halt::Help`] when help was asked for -- printed to stdout, and a SUCCESS for the shell.
/// [`Halt::Bad`] when the command line was wrong -- printed to stderr, and a FAILURE.
pub fn parse_or_halt(args: &[String], spec: &Spec) -> Result<Options, Halt> {
    if args.iter().any(|argument| argument == "--help" || argument == "-h") {
        println!("{}", help_text(spec));
        return Err(Halt::Help);
    }
    parse(args, spec).map_err(|error| {
        eprintln!("{error}");
        Halt::Bad
    })
}

/// What `--help` prints for a verb: its own prose where it has some, and the derived
/// accepted-options line where it does not.
#[must_use]
pub fn help_text(spec: &Spec) -> String {
    match spec.usage {
        Some(usage) => usage.trim_end().to_owned(),
        None => format!(
            "{}\n\nSee `lamella --help` for what this verb is for.",
            accepted(spec)
        ),
    }
}

/// The "this verb accepts" line, listing value options with a placeholder so the shape is visible.
fn accepted(spec: &Spec) -> String {
    if spec.values.is_empty() && spec.flags.is_empty() {
        return format!("lamella {} takes no options.", spec.verb);
    }
    let mut text = format!("lamella {} accepts:", spec.verb);
    for option in spec.values {
        let _ = write!(text, " {option} <value>");
    }
    for option in spec.flags {
        let _ = write!(text, " {option}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        text.split_whitespace().map(str::to_owned).collect()
    }

    const DEPLOY: Spec = Spec {
        verb: "deploy",
        usage: None,
        values: &["--target", "--board"],
        flags: &["--no-run"],
    };

    #[test]
    fn reads_values_flags_and_positionals() {
        let parsed = parse(&words("Program.cs --target COM8 --no-run"), &DEPLOY).expect("parses");
        assert_eq!(parsed.value("--target"), Some("COM8"));
        assert!(parsed.flag("--no-run"));
        assert!(!parsed.flag("--never-given"));
        assert_eq!(parsed.only_positional("deploy", "source file"), Ok("Program.cs"));
        assert_eq!(parsed.value("--board"), None);
    }

    /// **AN UNKNOWN OPTION MUST NOT PARSE, AND THE MESSAGE MUST CARRY THE WAY OUT.** This is the
    /// near-miss case the parser exists for: `--port` is what a reader coming from another tool
    /// types, and accepting it silently would report an unreachable board.
    #[test]
    fn an_unknown_option_is_refused_and_the_message_lists_the_real_ones() {
        let error = parse(&words("Program.cs --port COM8"), &DEPLOY).expect_err("refuses");
        assert!(error.contains("--port"), "it names what was wrong: {error}");
        assert!(error.contains("--target"), "and what to write instead: {error}");
    }

    #[test]
    fn a_value_option_at_the_end_is_refused_rather_than_dropped() {
        let error = parse(&words("Program.cs --target"), &DEPLOY).expect_err("refuses");
        assert!(error.contains("wants a value"), "got {error}");
    }

    /// A flag must not swallow the word after it -- `--no-run Program.cs` names a file, not a value.
    #[test]
    fn a_flag_does_not_consume_the_next_word() {
        let parsed = parse(&words("--no-run Program.cs"), &DEPLOY).expect("parses");
        assert!(parsed.flag("--no-run"));
        assert_eq!(parsed.only_positional("deploy", "source file"), Ok("Program.cs"));
    }

    const DOCUMENTED: Spec = Spec {
        verb: "flash",
        usage: Some("usage: lamella flash <image> --board <id>\n"),
        values: &["--board"],
        flags: &[],
    };

    /// **BOTH SPELLINGS ARE ANSWERED, AND `-h` NEEDS ITS OWN CASE.** This parser does not recognize
    /// `-h` as an option, so without one it reads as a POSITIONAL and `lamella flash -h` goes
    /// looking for a file named `-h`.
    #[test]
    fn help_is_answered_rather_than_refused_in_both_spellings() {
        for spelling in ["--help", "-h"] {
            let halt = parse_or_halt(&words(spelling), &DOCUMENTED)
                .err()
                .unwrap_or_else(|| panic!("`lamella flash {spelling}` parsed instead of helping"));
            assert_eq!(halt, Halt::Help, "`lamella flash {spelling}` must answer with the help");
        }
    }

    /// Asking for help SUCCEEDED; getting the command line wrong FAILED. A tool that returns one
    /// code for both cannot be scripted around, and this is the assertion that keeps them apart.
    #[test]
    fn help_exits_successfully_and_a_bad_option_does_not() {
        let help = parse_or_halt(&words("--help"), &DOCUMENTED).expect_err("halts");
        let bad = parse_or_halt(&words("--nonsense"), &DOCUMENTED).expect_err("halts");
        assert_eq!(help, Halt::Help);
        assert_eq!(bad, Halt::Bad);
        assert_eq!(format!("{:?}", help.code()), format!("{:?}", std::process::ExitCode::SUCCESS));
        assert_eq!(format!("{:?}", bad.code()), format!("{:?}", std::process::ExitCode::FAILURE));
    }

    /// Help wins over a complaint about the line it appears on. Somebody who mistyped an option and
    /// then added `--help` wants the help; being told about the typo instead is the loop the parser
    /// exists to avoid.
    #[test]
    fn help_beats_an_unknown_option_on_the_same_line() {
        let halt = parse_or_halt(&words("--nonsense --help"), &DOCUMENTED).expect_err("halts");
        assert_eq!(halt, Halt::Help);
    }

    /// A verb with no prose of its own still answers `--help` with something derived -- the option
    /// list -- rather than with nothing. An empty answer would read as "this verb takes no help".
    #[test]
    fn a_verb_without_prose_still_answers_with_its_options() {
        let text = help_text(&DEPLOY);
        assert!(text.contains("--target"), "it names the real options: {text}");
        assert!(text.contains("lamella --help"), "and where the prose lives: {text}");
    }

    #[test]
    fn a_documented_verb_answers_with_its_own_prose() {
        assert!(help_text(&DOCUMENTED).contains("<image>"), "got {}", help_text(&DOCUMENTED));
    }

    #[test]
    fn only_positional_reports_both_ways_of_being_wrong() {
        let none = parse(&words("--target COM8"), &DEPLOY).expect("parses");
        assert!(none.only_positional("deploy", "source file").is_err());
        let two = parse(&words("a.cs b.cs"), &DEPLOY).expect("parses");
        let error = two.only_positional("deploy", "source file").expect_err("refuses two");
        assert!(error.contains("a.cs b.cs"), "it shows what it saw: {error}");
    }
}
