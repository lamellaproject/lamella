//! The shared option parser for every `lamella` verb, and the one shape a command line it cannot
//! use is refused in.
//!
//! A refusal of a command line is short and goes to standard error: what is wrong, the verb's
//! syntax as `lamella --help` gives it, and the help to read.
//!
//! ```text
//! lamella fit: --image-bytes "nope" is not a non-negative integer.
//! Usage: lamella fit --board <board> --image-bytes <bytes>
//! Try: lamella fit --help
//! ```
//!
//! An unknown option is refused naming that option and no other, and a word a verb does not take
//! is refused rather than ignored.

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
    /// The options among `values` the verb takes more than once, as `--read <name>=<value>...`. Any
    /// other given twice is refused, since one of its values would be dropped without a word.
    pub repeats: &'a [&'a str],
    /// Options standing alone, as `--identify`.
    pub flags: &'a [&'a str],
    /// The words the verb takes besides its options.
    pub positional: Positional<'a>,
}

/// The words a verb takes besides its options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Positional<'a> {
    /// None: a word is refused.
    None,
    /// One, of the kind named, as "source file": a second is refused. The verb asks for the word
    /// with [`Options::only_positional`] where it needs one.
    One(&'a str),
    /// Whatever the verb reads itself: a grammar of the SDK's, which says what it refuses.
    Own,
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

    /// Every positional word, in the order given.
    #[must_use]
    pub fn positionals(&self) -> &[String] {
        &self.positional
    }

    /// The single positional word this verb wants, or the refusal of a command line without it.
    ///
    /// # Errors
    /// When no word, or more than one, was given.
    pub fn only_positional(&self, verb: &str, what: &str) -> Result<&str, String> {
        match self.positional.as_slice() {
            [one] => Ok(one),
            [] => Err(refusal(verb, &format!("give a {what}"))),
            [_, extra, ..] => Err(refusal(verb, &format!("unexpected argument \"{extra}\""))),
        }
    }
}

/// The refusal of a command line `verb` cannot use: `wrong`, the verb's syntax as `lamella --help`
/// gives it, and the help to read.
#[must_use]
pub fn refusal(verb: &str, wrong: &str) -> String {
    refusal_trying(verb, wrong, &format!("lamella {verb} --help"))
}

/// [`refusal`], with `next` as the command to try: `lamella boards` for a board this build does not
/// know, since the boards it does know are listed there.
#[must_use]
pub fn refusal_trying(verb: &str, wrong: &str, next: &str) -> String {
    let middle = crate::syntax(verb).map(|syntax| format!("{syntax}\n")).unwrap_or_default();
    format!("lamella {verb}: {}\n{middle}Try: {next}", sentence(wrong))
}

/// `id`, when it names a board this build knows, or else the refusal of `verb`'s command line that
/// gave it to `--board`.
///
/// # Errors
/// A board id that is not in this build's catalog.
pub fn known_board<'a>(verb: &str, id: &'a str) -> Result<&'a str, String> {
    match lamella_catalog::load_board(id) {
        Some(_) => Ok(id),
        None => Err(refusal_trying(
            verb,
            &format!("--board \"{id}\" is not a board this build knows"),
            "lamella boards",
        )),
    }
}

/// The refusal of an option `verb` does not take: that option, and no other.
fn unknown_option(verb: &str, name: &str) -> String {
    format!("lamella {verb}: unknown option \"{name}\".\nTry: lamella {verb} --help")
}

/// The refusal of a word `spec`'s verb does not take.
fn unexpected(spec: &Spec, word: &str) -> String {
    let wrong = format!("unexpected argument \"{word}\"");
    if spec.values.is_empty() && spec.flags.is_empty() {
        return format!(
            "lamella {}: {}\nThis command takes no arguments.\nTry: lamella {} --help",
            spec.verb,
            sentence(&wrong),
            spec.verb
        );
    }
    refusal(spec.verb, &wrong)
}

/// `text` as a sentence: ended by a full stop, unless it ends in one already.
fn sentence(text: &str) -> String {
    if text.ends_with(['.', '?', '!']) { text.to_owned() } else { format!("{text}.") }
}

/// Parse `args` against `spec`.
///
/// # Errors
/// An option the verb does not declare, a value option given no value, and a word the verb does not
/// take, each as the refusal to print.
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
                    return Err(refusal(spec.verb, &format!("{name} wants a value")));
                };
                if !spec.repeats.contains(&name.as_str()) && parsed.named.iter().any(|(given, _)| *given == name) {
                    return Err(refusal(spec.verb, &format!("{name} is given more than once; give it once")));
                }
                parsed.named.push((name, value.clone()));
            } else {
                return Err(unknown_option(spec.verb, &name));
            }
        } else {
            let taken = match spec.positional {
                Positional::None => false,
                Positional::One(_) => parsed.positional.is_empty(),
                Positional::Own => true,
            };
            if !taken {
                return Err(unexpected(spec, argument));
            }
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
        values: &["--device", "--board"],
        repeats: &[],
        flags: &["--no-run"],
        positional: Positional::One("source file"),
    };

    #[test]
    fn reads_values_flags_and_positionals() {
        let parsed = parse(&words("Program.cs --device COM8 --no-run"), &DEPLOY).expect("parses");
        assert_eq!(parsed.value("--device"), Some("COM8"));
        assert!(parsed.flag("--no-run"));
        assert!(!parsed.flag("--never-given"));
        assert_eq!(parsed.only_positional("deploy", "source file"), Ok("Program.cs"));
        assert_eq!(parsed.value("--board"), None);
    }

    /// **AN UNKNOWN OPTION MUST NOT PARSE, AND THE REFUSAL NAMES IT AND NO OTHER.** This is the
    /// near-miss case the parser exists for: `--port` is what a reader coming from another tool
    /// types, and accepting it silently would report an unreachable board. The way out is the
    /// verb's help, which says what each option does.
    #[test]
    fn an_unknown_option_is_refused_naming_it_alone() {
        let error = parse(&words("Program.cs --port COM8"), &DEPLOY).expect_err("refuses");
        assert_eq!(error, "lamella deploy: unknown option \"--port\".\nTry: lamella deploy --help");
    }

    #[test]
    fn a_value_option_at_the_end_is_refused_rather_than_dropped() {
        let error = parse(&words("Program.cs --device"), &DEPLOY).expect_err("refuses");
        assert!(error.starts_with("lamella deploy: --device wants a value.\nUsage: lamella deploy "), "got {error}");
        assert!(error.ends_with("\nTry: lamella deploy --help"), "got {error}");
    }

    /// **A WORD A VERB DOES NOT TAKE IS REFUSED**: any word where the verb takes none, and a second
    /// where it takes one. A verb that reads its own words is handed every one.
    #[test]
    fn a_word_the_verb_does_not_take_is_refused() {
        let error = parse(&words("Program.cs Other.cs"), &DEPLOY).expect_err("refuses a second word");
        assert!(error.starts_with("lamella deploy: unexpected argument \"Other.cs\".\nUsage: "), "got {error}");
        let none = Spec { verb: "boards", usage: None, values: &[], repeats: &[], flags: &[], positional: Positional::None };
        assert_eq!(
            parse(&words("extra"), &none).expect_err("refuses any word"),
            "lamella boards: unexpected argument \"extra\".\nThis command takes no arguments.\nTry: lamella boards --help"
        );
        let own = Spec { verb: "run", usage: None, values: &[], repeats: &[], flags: &[], positional: Positional::Own };
        assert_eq!(parse(&words("a b c"), &own).expect("taken").positionals(), words("a b c"));
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
        usage: Some("Usage:\n  lamella flash <image> --board <board>\n"),
        values: &["--board"],
        repeats: &[],
        flags: &[],
        positional: Positional::One("prebuilt image"),
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
        assert!(text.contains("--device"), "it names the real options: {text}");
        assert!(text.contains("lamella --help"), "and where the prose lives: {text}");
    }

    #[test]
    fn a_documented_verb_answers_with_its_own_prose() {
        assert!(help_text(&DOCUMENTED).contains("<image>"), "got {}", help_text(&DOCUMENTED));
    }

    /// A missing word is refused by what to give, with the verb's syntax.
    #[test]
    fn only_positional_refuses_a_line_without_its_word() {
        let none = parse(&words("--device COM8"), &DEPLOY).expect("parses");
        let error = none.only_positional("deploy", "source file").expect_err("refuses none");
        assert!(error.starts_with("lamella deploy: give a source file.\nUsage: lamella deploy "), "got {error}");
    }
}
