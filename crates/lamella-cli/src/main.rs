//! The `lamella` command-line tool: one program for the whole toolchain.

use std::process::ExitCode;

mod args;
#[cfg(feature = "bake")]
mod attach;
#[cfg(feature = "bake")]
mod bake;
mod deploy;
mod devices;
mod flash;
mod program;
mod verdicts;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("devices") => devices::devices_command(rest),
        Some("run") => program::run_command(rest),
        Some("board-module-run") => program::board_module_run_command(rest),
        Some("build") => program::build_command(rest),
        Some("flash") => flash::flash_command(rest),
        Some("deploy") => deploy::deploy_command(rest),
        Some("boards") => verdicts::boards_command(rest),
        Some("fit") => verdicts::fit_command(rest),
        Some("reconcile") => verdicts::reconcile_command(rest),
        Some("--version" | "-V" | "version") => {
            if let Some(extra) = rest.first() {
                eprintln!("lamella version: takes no arguments, and got {extra:?}");
                return ExitCode::FAILURE;
            }
            print!("{}", lamella_flash_routes::contracts::Contracts::of(TOOL_VERSION).describe());
            ExitCode::SUCCESS
        }
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

/// This binary's own package version.
///
/// Whatever the workspace states, reported as-is. A consumer asking for a version gets the one
/// this build carries and can pin to it; refusing to answer would tell them nothing.
const TOOL_VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
usage:
  lamella devices [--identify] [--all-devices]       what is attached, and how to name it
  lamella run <file> [--target <t>]                  run it here, or ON a board with output here
  lamella build <file> [--board <id>] [--format f]   make an artifact; nothing is written
  lamella deploy <file> --target <t> | --board <id>  compile it and put it on a board
  lamella flash <image> --board <id> [--probe <s>]   write bytes that are ALREADY an image
  lamella boards                                     every board this build knows
  lamella fit --board <id> --image-bytes <n>         does an image of <n> bytes fit?
  lamella reconcile --board <id> [--read <name>=<v>] is the attached board the one assumed?
  lamella version | --version                        this build, and the contracts it speaks

THREE VERBS PUT A PROGRAM SOMEWHERE, AND THEY DIFFER IN WHAT THEY TAKE AND WHERE IT LANDS.

  run <file>            SOURCE -> here, or a board.   ATTACHES: its output appears here, and it waits.
  build <file>          SOURCE -> a file on disk.     No board, no hardware, nothing written to one.
  deploy <file>         SOURCE -> a board.            Puts it there and RETURNS YOU TO YOUR SHELL.
  flash <image>         AN IMAGE -> a board's chip.   Writes bytes somebody already built.

`run` and `deploy` differ only in whether the tool stays. `run --target` prints what the program
prints, as it prints it, and waits for it to end -- which for a program written to loop forever
means until you stop the tool. `deploy` starts it and leaves, which is what a production push wants.

`build` is how you get an image WITHOUT deploying it -- `--format hex` or `bin` writes the
bare-metal image for `--board`, which is exactly what `flash` then takes. So `build` produces what
`flash` consumes, and `deploy` is the two of them in one step.

`deploy` takes ONE of two destinations, and the option chooses the route. `--target <t>` is a live
connection (what `devices` prints): the board's firmware stays put, only the program crosses, and a
cycle is about a second. `--board <id>` is a board model (what `boards` lists): it writes the chip
over a probe and needs nothing on the board first, which is where a new board begins.

NOTHING HERE NEEDS HARDWARE EXCEPT `devices`, `deploy` AND `flash`. `run` executes on this machine, and
against a named board it serves that board's own generated `board` module -- so a program written
for hardware that has not arrived yet runs today. `build` and `fit` answer from the board file and
the part row; an attached board contributes nothing to the arithmetic.

`build <file> --board <id>` also answers `fit` about what it just produced, which is the way to
obtain the <n> that `fit` asks for.

`reconcile` compares readings taken from an attached board against what that board declares it
can be asked. With no --read it reports what attaching the board WOULD settle, and what nothing
declared can reach. Readings are <discriminator>=<value>, decimal or 0x-prefixed.

Files are read by extension: .cs is C#, .py is Python.
";

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
        assert!(summary.contains("--target"), "and it keeps the one it has: {summary}");
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
        assert!(super::USAGE.contains("NEEDS HARDWARE"), "the usage buries the best property");
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
        let verbs: [(&str, fn(&[String]) -> std::process::ExitCode); 8] = [
            ("boards", super::verdicts::boards_command),
            ("fit", super::verdicts::fit_command),
            ("reconcile", super::verdicts::reconcile_command),
            ("run", super::program::run_command),
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
