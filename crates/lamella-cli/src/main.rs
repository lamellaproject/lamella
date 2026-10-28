//! The `lamella` command-line tool: one program for the whole toolchain.

use std::process::ExitCode;

mod args;
mod bootprot;
#[cfg(feature = "bake")]
mod attach;
#[cfg(feature = "bake")]
mod bake;
mod deploy;
mod devices;
mod emulator_run;
mod ending;
mod flash;
mod folders;
mod image_run;
mod msbuild;
mod own_lines;
mod packages;
mod ports;
mod program;
mod project;
// Test-only: the one place that says what a rendered message may and may not contain.
#[cfg(test)]
mod rendered;
mod text;
mod tiers;
mod verdicts;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("--help" | "-h" | "help") => help_command(rest),
        None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(verb) => command(verb, rest).unwrap_or_else(|| {
            eprintln!("lamella: unknown command \"{verb}\".\nTry: lamella --help");
            ExitCode::FAILURE
        }),
    }
}

/// Runs the command `verb` names with `rest`, or answers `None` for a word that names no command.
fn command(verb: &str, rest: &[String]) -> Option<ExitCode> {
    Some(match verb {
        "devices" => devices::devices_command(rest),
        "run" => program::run_command(rest).finish(),
        "board-module-run" => program::board_module_run_command(rest),
        "build" => program::build_command(rest),
        "flash" => flash::flash_command(rest),
        "deploy" => deploy::deploy_command(rest),
        "boards" => verdicts::boards_command(rest),
        "fit" => verdicts::fit_command(rest),
        "reconcile" => verdicts::reconcile_command(rest),
        "--version" | "-V" | "version" => version_command(rest),
        _ => return None,
    })
}

/// `lamella help [<command>]`: the tool's usage, or the help `lamella <command> --help` prints.
fn help_command(rest: &[String]) -> ExitCode {
    match rest {
        [] => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        [verb] => command(verb, &["--help".to_owned()]).unwrap_or_else(|| {
            eprintln!("lamella help: unknown command \"{verb}\".\nTry: lamella --help");
            ExitCode::FAILURE
        }),
        [_, extra, ..] => {
            eprintln!("lamella help: unexpected argument \"{extra}\".\nTry: lamella --help");
            ExitCode::FAILURE
        }
    }
}

/// `lamella version` and `lamella --version`: this build's version, and the versions of what it
/// reads and speaks.
fn version_command(rest: &[String]) -> ExitCode {
    let spec = args::Spec {
        verb: "version",
        usage: Some(VERSION_USAGE),
        values: &[],
        repeats: &[],
        flags: &[],
        positional: args::Positional::None,
    };
    if let Err(halt) = args::parse_or_halt(rest, &spec) {
        return halt.code();
    }
    print!("{}", lamella_flash_routes::contracts::Contracts::of(TOOL_VERSION).describe());
    ExitCode::SUCCESS
}

/// This binary's own package version.
///
/// Whatever the workspace states, reported as-is. A consumer asking for a version gets the one
/// this build carries and can pin to it; refusing to answer would tell them nothing.
const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

/// What `lamella version --help` prints.
const VERSION_USAGE: &str = "\
Usage: lamella version

Print the Lamella build version and compatibility versions.
`lamella --version` prints the same text.";

/// What `lamella --help` prints: every command's syntax, which of them puts a program where, and
/// where each command's own help is.
const USAGE: &str = "\
Usage:
  lamella devices [--identify <device>] [--all-devices] [--json]
  lamella run <file> [--device <device>] [-- <args>...]
  lamella build <file> [--board <board>] [--format <format>] [--out <path>]
  lamella deploy <file> (--device <device> | --board <board>)
  lamella deploy --erase --device <device>
  lamella flash <image> [--board <board>] [--via probe|volume]
  lamella boards
  lamella fit --board <board> --image-bytes <bytes>
  lamella reconcile --board <board> [--read <name>=<value>...]
  lamella version

Where should the program go?
  run     Build and run it on this computer or on a connected board,
          staying connected to its output.
  build   Build it and write the output to a file.
  deploy  Build it and put it on a board.
  flash   Write an already-built image file to a board.

Local `run`, `build`, `boards`, `fit` and `reconcile` do not require
attached hardware. For `reconcile`, you provide the board readings to
compare.

Run `lamella <command> --help` for that command's options, effects, and
examples. `lamella --version` prints the same build and compatibility
versions as `lamella version`.
";

/// The `Usage:` lines a refusal of `verb`'s command line shows: its lines of [`USAGE`], or `None`
/// for a word [`USAGE`] gives no syntax for.
///
/// **READ FROM THE USAGE, NOT WRITTEN BESIDE IT**, so the syntax a refusal shows is the one
/// `lamella --help` shows.
fn syntax(verb: &str) -> Option<String> {
    let lines: Vec<&str> = USAGE
        .lines()
        .map(str::trim_start)
        .filter(|line| line.strip_prefix("lamella ").and_then(|rest| rest.split(' ').next()) == Some(verb))
        .collect();
    let (first, rest) = lines.split_first()?;
    Some(rest.iter().fold(format!("Usage: {first}"), |text, line| format!("{text}\n       {line}")))
}

#[cfg(test)]
mod tests {
    /// **EVERY VERB THE DISPATCHER ACCEPTS MUST APPEAR IN THE USAGE.** A verb that works and is
    /// undocumented is invisible, which for a tool whose whole purpose is discoverability is the
    /// same as not having built it. The dispatcher and this text are edited separately, and the
    /// gap between them is silent.
    #[test]
    fn the_usage_names_every_verb_the_dispatcher_accepts() {
        for verb in
            ["devices", "run", "build", "flash", "deploy", "boards", "fit", "reconcile", "version"]
        {
            assert!(
                super::USAGE.contains(&format!("lamella {verb}")),
                "the usage does not mention `lamella {verb}`"
            );
        }
    }

    /// **TWO STRINGS STATE `run`'s OPTIONS AND BOTH HAVE TO AGREE.** This file's `USAGE` is the
    /// one-line summary and `program.rs`'s `RUN_USAGE` is the verb's own help; `--board` was
    /// removed from the verb and only the second was updated, so the summary advertised a flag the
    /// verb refuses. A reader meets the summary FIRST.
    #[test]
    fn the_summary_and_the_verbs_own_help_agree_about_run() {
        let summary = super::USAGE
            .lines()
            .find(|line| line.contains("lamella run <file>"))
            .expect("the summary names `run`");
        assert!(
            !summary.contains("--board"),
            "`run` has no --board; the summary must not offer one: {summary}"
        );
        assert!(summary.contains("--device"), "and it keeps the one it has: {summary}");
    }

    /// **ONE VERB IS DISPATCHED AND DELIBERATELY NOT DOCUMENTED**, so the test above cannot simply
    /// walk the dispatcher -- and its absence has to be asserted rather than left to the fact that
    /// nobody added it. `board-module-run` is what `run --board` was: the program on this machine
    /// against a board's generated `board` module, which is a fact table and not hardware. It is
    /// internal, it announces its own deprecation when run, and a future editor tidying the usage
    /// into agreement with the dispatcher would undo a decision rather than fix an oversight.
    #[test]
    fn the_internal_verb_stays_out_of_the_usage() {
        assert!(
            !super::USAGE.contains("board-module-run"),
            "`board-module-run` is undocumented on purpose; adding it to the usage publishes it"
        );
    }

    /// The usage has to say the thing that stops somebody buying hardware to try Lamella.
    #[test]
    fn the_usage_says_which_verbs_need_no_hardware() {
        assert!(
            super::USAGE.contains("Local `run`, `build`, `boards`, `fit` and `reconcile` do not require\nattached hardware."),
            "the usage buries the best property"
        );
    }

    /// **A REFUSAL SHOWS THE SYNTAX THE USAGE SHOWS**, every line of it for a verb with two, and none
    /// for a word the usage gives no syntax for.
    #[test]
    fn a_refusal_shows_the_usages_own_syntax() {
        assert_eq!(super::syntax("fit").as_deref(), Some("Usage: lamella fit --board <board> --image-bytes <bytes>"));
        assert_eq!(
            super::syntax("deploy").as_deref(),
            Some(
                "Usage: lamella deploy <file> (--device <device> | --board <board>)\n       \
                 lamella deploy --erase --device <device>"
            )
        );
        assert_eq!(super::syntax("boards").as_deref(), Some("Usage: lamella boards"));
        assert_eq!(super::syntax("board-module-run"), None);
        assert_eq!(super::syntax("help"), None);
    }

    /// **EVERY VERB ANSWERS `--help`, INCLUDING ONE THAT TAKES NO OPTIONS.** A verb reading no
    /// arguments would print its table for `--help` and for `--nonsense` alike and exit 0 both
    /// times -- a verb silently swallowing an option, which is the single thing `args`'s own
    /// header says this tool must never do. Taking no options is not a reason to skip the parser;
    /// it is what makes the omission invisible.
    ///
    /// Driven through the real command functions rather than through a list of specs, because a
    /// spec list would agree with itself by construction. Both spellings return before any verb
    /// does work, so nothing here touches hardware, a file, or a board.
    ///
    #[test]
    fn every_verb_answers_help_and_refuses_an_unknown_option() {
        fn code(exit: std::process::ExitCode) -> String {
            format!("{exit:?}")
        }
        let ok = code(std::process::ExitCode::SUCCESS);
        let bad = code(std::process::ExitCode::FAILURE);
        assert_ne!(ok, bad, "the two codes must be distinguishable for this test to mean anything");

        let help = vec!["--help".to_owned()];
        let junk = vec!["--surely-no-verb-takes-this".to_owned()];
        let verbs: [(&str, fn(&[String]) -> std::process::ExitCode); 9] = [
            ("version", super::version_command),
            ("boards", super::verdicts::boards_command),
            ("fit", super::verdicts::fit_command),
            ("reconcile", super::verdicts::reconcile_command),
            ("run", |args| super::program::run_command(args).finish()),
            ("build", super::program::build_command),
            ("flash", super::flash::flash_command),
            ("devices", super::devices::devices_command),
            ("deploy", super::deploy::deploy_command),
        ];
        for (verb, command) in verbs {
            assert_eq!(code(command(&help)), ok, "`lamella {verb} --help` must succeed");
            assert_eq!(
                code(command(&junk)),
                bad,
                "`lamella {verb}` must REFUSE an option it does not take, not ignore it"
            );
        }
    }
}
