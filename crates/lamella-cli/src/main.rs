//! The `lamella` command-line tool: one program for the whole toolchain.

use std::process::ExitCode;

mod args;
mod artifact;
#[cfg(feature = "bake")]
mod attach;
#[cfg(feature = "bake")]
mod bake;
mod bootsel;
mod catalogue;
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
        Some("build") => program::build_command(rest),
        Some("flash") => flash::flash_command(rest),
        Some("deploy") => deploy::deploy_command(rest),
        Some("boards") => verdicts::boards_command(),
        Some("fit") => verdicts::fit_command(rest),
        Some("reconcile") => verdicts::reconcile_command(rest),
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
  lamella devices [--identify]                       what is attached, and how to name it
  lamella run <file> [--board <id> | --target <t>]   run it here, or ON a board with output here
  lamella build <file> [--board <id>] [--format f]   make an artifact; nothing is written
  lamella deploy <file> --target <t> | --board <id>  compile it and put it on a board
  lamella flash <image> --board <id> [--probe <s>]   write bytes that are ALREADY an image
  lamella boards                                     every board this build knows
  lamella fit --board <id> --image-bytes <n>         does an image of <n> bytes fit?
  lamella reconcile --board <id> [--read <name>=<v>] is the attached board the one assumed?

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
        for verb in ["devices", "run", "build", "flash", "deploy", "boards", "fit", "reconcile"] {
            assert!(
                super::USAGE.contains(&format!("lamella {verb}")),
                "the usage does not mention `lamella {verb}`"
            );
        }
    }

    /// The usage has to say the thing that stops somebody buying hardware to try Lamella.
    #[test]
    fn the_usage_says_which_verbs_need_no_hardware() {
        assert!(super::USAGE.contains("NEEDS HARDWARE"), "the usage buries the best property");
    }
}
