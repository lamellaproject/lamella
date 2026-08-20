//! `lamella deploy`: take a program and put it on a board.

use crate::args::{self, Spec};
use lamella_wire::Capabilities;
use lamella_wire_host::{deploy_chunked_blocking, hello_blocking, open_target, send_deploy_run};
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

#[cfg(feature = "bake")]
use {crate::bake::compile_and_bake, lamella_wire_host::engine::LcscCompiler};

/// The default serial baud for a Lamella Link carrier (USB-CDC ignores it; a real UART wants it).
const BAUD: u32 = 115_200;

/// How long to wait on each wire exchange. Generous: a deploy erases and writes flash on the far
/// side, which is slow in a way a timeout should not be racing.
const TIMEOUT: Duration = Duration::from_secs(20);

/// The deploy chunk size, in bytes. The far side acknowledges each chunk, so this trades round
/// trips against the buffer a device has to hold.
const CHUNK: usize = 8 * 1024;

pub fn deploy_command(args: &[String]) -> ExitCode {
    let spec = Spec {
        verb: "deploy",
        values: &["--target", "--board", "--probe"],
        flags: &["--no-run", "--unsafe"],
    };
    let parsed = match args::parse(args, &spec) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let path = match parsed.only_positional("deploy", "source file") {
        Ok(path) => Path::new(path).to_path_buf(),
        Err(error) => {
            eprintln!("{error}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let kind = crate::artifact::classify(&path);
    if kind == crate::artifact::Kind::ChipImage {
        eprintln!(
            "lamella deploy: {} is an image for a CHIP, and this verb sends a program to firmware.\n\n\
             To write it to the chip:\n\
             \x20   lamella flash {} --board <id>",
            path.display(),
            path.display()
        );
        return ExitCode::FAILURE;
    }

    match (parsed.value("--board"), parsed.value("--target")) {
        (Some(_), Some(_)) => {
            eprintln!(
                "lamella deploy: --board and --target name different destinations, so give one.\n\n\
                 \x20   --board <id>      compile and write the CHIP, over a probe (nothing needs \
                 to be on it)\n\
                 \x20   --target <t>      compile and send it to firmware ALREADY running there"
            );
            ExitCode::FAILURE
        }
        (Some(_), None) if kind == crate::artifact::Kind::WirePayload => {
            eprintln!(
                "lamella deploy: {} is loaded by firmware already on the board, so it needs a \
                 --target rather\nthan a --board. Writing it to a bare chip would leave the board \
                 resetting into a file format.\n\n\
                 \x20   lamella deploy {} --target <t>",
                path.display(),
                path.display()
            );
            ExitCode::FAILURE
        }
        (Some(board_id), None) => crate::flash::deploy_to_chip(
            &path,
            board_id,
            parsed.value("--probe"),
            parsed.flag("--unsafe"),
        ),
        (None, Some(target)) if kind == crate::artifact::Kind::WirePayload => {
            send_payload(&path, target, parsed.flag("--no-run"))
        }
        (None, Some(target)) => to_running_firmware(&path, target, parsed.flag("--no-run")),
        (None, None) => {
            eprintln!(
                "lamella deploy: name where it goes.\n\n{USAGE}\n\
                 `lamella devices` lists what is attached and prints the --target for each one;\n\
                 `lamella boards` lists every --board this build knows."
            );
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
usage: lamella deploy <file.cs> --target <t> [--no-run]   into firmware already on the board
       lamella deploy <file.cs> --board <id> [--probe <s>] onto the bare chip, over a probe

--target is a live connection (what `lamella devices` prints); --board is a board model (what
`lamella boards` lists). The first keeps the board's firmware and takes about a second; the second
replaces everything on the chip and needs nothing there first.
";

/// Send a payload that is ALREADY built to firmware running at `target`.
///
/// **THE VERB THAT COMPILES AND THE VERB THAT SENDS ARE THE SAME VERB, AND THAT IS THE POINT.**
/// Somebody who built a `.lmli` on a build machine, or received one, has to be able to put it on a
/// board -- and before this existed they could not: `deploy` refused it toward `flash` and `flash`
/// refused it back toward `deploy`, a loop with no way out for anybody holding one.
fn send_payload(path: &Path, target: &str, no_run: bool) -> ExitCode {
    if path.extension().and_then(|extension| extension.to_str()) == Some("lpyc") {
        eprintln!(
            "lamella deploy: {} is a Python bundle, which travels by a different wire message \
             (DEPLOY_BUNDLE)\nthan a baked C# image. The host side of that message is not a \
             library call yet, so this verb\ncannot send one -- and sending it down the image path \
             would deploy successfully and leave the\nboard unable to boot what it holds.\n\n\
             `cargo run -p lamella-wire-host --example wire-py` drives it today.",
            path.display()
        );
        return ExitCode::FAILURE;
    }
    let image = match std::fs::read(path) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("lamella deploy: read {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    println!("read {} B from {} -- sending it as it stands", image.len(), path.display());
    send_image(&image, target, no_run)
}

/// Compile `path` and send it to Lamella firmware already running at `target`.
#[cfg(feature = "bake")]
fn to_running_firmware(path: &Path, target: &str, no_run: bool) -> ExitCode {
    if path.extension().and_then(|extension| extension.to_str()) != Some("cs") {
        eprintln!(
            "lamella deploy: {} is not a C# file. This verb deploys the baked C# image today; \
             `lamella build` produces the Python bundle, whose deploy path is separate.",
            path.display()
        );
        return ExitCode::FAILURE;
    }
    let source = match std::fs::read_to_string(&path) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("lamella deploy: read {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };

    let compiler = match LcscCompiler::discover() {
        Ok(compiler) => compiler,
        Err(error) => {
            eprintln!("lamella deploy: {error}");
            return ExitCode::FAILURE;
        }
    };
    let image = match compile_and_bake(&compiler, &source) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    println!("built {} B from {}", image.len(), path.display());
    send_image(&image, target, no_run)
}

/// Put `image` on the firmware running at `target`, and start it unless `no_run`.
///
/// **ONE SENDER FOR A COMPILED IMAGE AND A PREBUILT ONE**, so the wire behavior, the timeouts and
/// every message a reader sees are the same either way. The firmware cannot tell where the bytes
/// came from and neither should the output.
fn send_image(image: &[u8], target: &str, no_run: bool) -> ExitCode {
    let mut transport = match open_target(target, BAUD, TIMEOUT) {
        Ok(transport) => transport,
        Err(error) => {
            eprintln!("lamella deploy: cannot open {target}: {error:?}");
            eprintln!(
                "\nthis build can open: {}.\n\
                 `lamella devices` lists what is attached and what to write here.",
                lamella_wire_host::available_carriers().join(", ")
            );
            return ExitCode::FAILURE;
        }
    };
    if let Err(error) = hello_blocking(&mut transport, 0, host_caps(), TIMEOUT) {
        eprintln!("lamella deploy: {target} did not answer a HELLO ({error:?}).");
        eprintln!("{}", no_answer());
        return ExitCode::FAILURE;
    }
    match deploy_chunked_blocking(&mut transport, 1, image, CHUNK, TIMEOUT) {
        Ok(true) => {}
        Ok(false) => {
            eprintln!("lamella deploy: a chunk failed to verify on {target}; nothing was started.");
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("lamella deploy: deploy to {target} failed: {error:?}");
            return ExitCode::FAILURE;
        }
    }
    println!("deployed {} B to {target}", image.len());

    if no_run {
        println!("not started (--no-run). It runs at the board's next reset.");
        return ExitCode::SUCCESS;
    }
    match send_deploy_run(&mut transport, 2) {
        Ok(()) => {
            println!("started it.");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!(
                "lamella deploy: the image is on the board and the start command failed \
                 ({error:?}). Resetting the board runs it."
            );
            ExitCode::FAILURE
        }
    }
}

/// The wire route in a build that cannot bake, naming the feature rather than the verb.
///
/// **THE CHIP ROUTE STILL WORKS IN THIS BUILD, WHICH IS WHY THIS IS PER-ROUTE RATHER THAN PER-VERB.**
/// Only the wire route needs a baked image, so a default build deploys to a chip perfectly well and
/// a reader must not be told that `deploy` is unavailable when half of it is not.
#[cfg(not(feature = "bake"))]
fn to_running_firmware(_path: &Path, _target: &str, _no_run: bool) -> ExitCode {
    eprintln!(
        "lamella deploy: this build cannot deploy over a --target.\n\n\
         Sending a program to firmware already on the board bakes it into a flash image first, \
         which\nneeds the `bake` feature:\n\
         \x20   cargo build -p lamella-cli --features bake\n\n\
         The feature is off by default because the baking code is not additive -- reaching the \
         shared\nloader would stop other crates in this workspace compiling.\n\n\
         `lamella deploy <file> --board <id>` works in this build: it writes the chip over a probe."
    );
    ExitCode::FAILURE
}

/// The capabilities a deploying host offers.
fn host_caps() -> Capabilities {
    Capabilities(Capabilities::BAKED_IMAGE | Capabilities::REPL_RUN | Capabilities::PROFILE_CHIPID)
}

/// What to print when a target opens and then says nothing.
///
/// **SILENCE HAS TWO CAUSES AND THEY LEAD OPPOSITE WAYS.** An unreachable board and a board with
/// no Lamella firmware on it are the same event on the wire, and a reader told only "no response"
/// checks their cable -- which is the wrong half in the commoner case, because a board arrives
/// with no Lamella firmware on it and has to be given some once.
fn no_answer() -> String {
    "\nThe port opened, so the board is attached and the cable carries data. What did not happen \
     is an answer,\nand the usual reason is that the board is not running Lamella firmware yet -- \
     a board is not born with any.\n\n\
     A --target sends a program to firmware that is ALREADY on the board. To put a program on a \
     board\nthat has none, write the chip instead:\n\
     \x20   lamella deploy <file> --board <id>\n"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **THE NO-ANSWER TEXT IS THE POINT OF THIS FILE'S ERROR HANDLING**, so it is asserted rather
    /// than left to drift back into "no response from target".
    #[test]
    fn the_no_answer_text_names_the_likelier_cause_rather_than_the_cable() {
        let text = no_answer();
        assert!(text.contains("cable carries data"), "it rules the cable out: {text}");
        assert!(text.contains("not running Lamella firmware"), "and names the real cause");
        assert!(text.contains("--board"), "and hands over the route that works: {text}");
    }

    /// **THE TWO DESTINATIONS ARE NAMED IN THE USAGE, AND THE ROUTE EACH IMPLIES IS TOO.** This is
    /// the distinction the whole verb split rests on, and it is one sentence away from being lost.
    #[test]
    fn the_usage_separates_a_model_from_a_connection() {
        assert!(USAGE.contains("--target"), "got {USAGE}");
        assert!(USAGE.contains("--board"), "got {USAGE}");
        assert!(USAGE.contains("live connection"), "it says what a target IS");
        assert!(USAGE.contains("board model"), "and what a board IS");
    }
}
