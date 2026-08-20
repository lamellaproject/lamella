//! The shared option parser for every `lamella` verb.

use std::fmt::Write as _;

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
    /// Options taking a value, as `--board <id>`.
    pub values: &'a [&'a str],
    /// Options standing alone, as `--identify`.
    pub flags: &'a [&'a str],
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

    #[test]
    fn only_positional_reports_both_ways_of_being_wrong() {
        let none = parse(&words("--target COM8"), &DEPLOY).expect("parses");
        assert!(none.only_positional("deploy", "source file").is_err());
        let two = parse(&words("a.cs b.cs"), &DEPLOY).expect("parses");
        let error = two.only_positional("deploy", "source file").expect_err("refuses two");
        assert!(error.contains("a.cs b.cs"), "it shows what it saw: {error}");
    }
}
