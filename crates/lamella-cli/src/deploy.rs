//! `lamella deploy`: take a program and put it on a board.

use crate::args::{self, Spec};
use lamella_wire::Capabilities;
use lamella_wire_host::{deploy_chunked_blocking, hello_blocking, open_target};
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

/// The deploy chunk size, in bytes.
///
/// # It is bounded by the SMALLEST device receive ring, not by round-trip economics
///
/// A serve firmware drains its UART into a fixed ring from an interrupt, and a full ring DROPS.
/// So a frame larger than that ring can never assemble no matter how patient either side is: the
/// reader waits for bytes that were discarded while it was being told about them. The principle is
/// the same one a serve firmware applies when it sizes that ring: once the whole frame FITS, the
/// reader has unlimited time, because no more bytes are coming.
///
const CHUNK: usize = 256;

/// What a refusal reason MEANS, as a sentence somebody can act on.
///
/// A refusal is the target answering, not failing, and each reason has a different remedy: one says
/// stop asking, the other says wait for a colleague to unplug. Rendering the struct printed
/// `reason: 2, msg_type: 2` for the second, which reads as a protocol fault and sends the reader to
/// the cable -- the one direction that cannot help.
fn refusal(reason: u8) -> String {
    match reason {
        lamella_wire::error::SESSION_HELD => String::from(
            concat!(
                "another carrier already holds the debug session -- typically somebody at the ",
                "board with a cable, or a host that did not let go. The request was well formed ",
                "and this target implements it, so the answer changes when the other carrier ",
                "releases it; a reset clears a session whose host is gone.",
            ),
        ),
        lamella_wire::error::UNKNOWN_MESSAGE_TYPE => String::from(
            concat!(
                "this target does not implement that message. Stop asking rather than retrying ",
                "-- the answer will not change without different firmware.",
            ),
        ),
        other => unnamed_reason(other),
    }
}

/// A reason byte this build has no sentence for, named rather than swallowed.
fn unnamed_reason(reason: u8) -> String {
    format!("refusal reason {reason}, which this build has no description for")
}

pub fn deploy_command(args: &[String]) -> ExitCode {
    let spec = Spec {
        verb: "deploy",
        usage: Some(USAGE),
        values: &["--target", "--board", "--probe", "--volume", "--device", "--via"],
        flags: &["--no-run", "--unsafe", crate::flash::CLASS_LIBRARY_FLAG],
    };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    let path = match parsed.only_positional("deploy", "source file") {
        Ok(path) => Path::new(path).to_path_buf(),
        Err(error) => {
            eprintln!("{error}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let kind = lamella_flash_routes::artifact::classify(&path);
    if kind == lamella_flash_routes::artifact::Kind::ChipImage {
        eprintln!(
            "lamella deploy: {} is an image for a CHIP, and this verb sends a program to firmware.\n\n\
             To write it to the chip:\n\
             \x20   lamella flash {} --board <id>",
            path.display(),
            path.display()
        );
        return ExitCode::FAILURE;
    }

    let tier = crate::flash::Tier::from_options(&parsed);
    if tier == crate::flash::Tier::ClassLibrary && parsed.value("--target").is_some() {
        eprintln!(
            "{}",
            crate::flash::tier_flag_where_nothing_links(
                "deploy",
                "--target hands the program to firmware that is already running, which resolves \
                 what it needs\non the board itself.",
                "Use --board <id> to compile and write the chip, which is the route that links.",
                "Nothing was sent.",
            )
        );
        return ExitCode::FAILURE;
    }
    let libraries: Vec<crate::flash::Library> = Vec::new();

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
        (Some(_), None) if kind == lamella_flash_routes::artifact::Kind::WirePayload => {
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
            parsed.value("--volume"),
            parsed.value("--device"),
            parsed.value("--via"),
            parsed.flag("--unsafe"),
            tier,
            &libraries,
        ),
        (None, Some(target)) if kind == lamella_flash_routes::artifact::Kind::WirePayload => {
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

#[cfg(test)]
mod source_refusal_tests {
    use super::{Uncompilable, deploy_refusal, uncompilable_source};
    use std::path::Path;

    /// Three routes across two verbs compile C# ahead of time, and the rule lived in two of them:
    /// `deploy --board` read a `.py`, handed it to the C# compiler and printed thirty Roslyn
    /// diagnostics naming a language the author never used. One predicate, every caller.
    #[test]
    fn every_route_that_compiles_ahead_of_time_agrees_on_what_it_cannot_take() {
        assert_eq!(uncompilable_source(Path::new("Blink.cs")), None, "C# is what they compile");
        assert_eq!(uncompilable_source(Path::new("app.py")), Some(Uncompilable::Python));
        assert_eq!(uncompilable_source(Path::new("notes.txt")), Some(Uncompilable::Other));
    }

    /// The predicate is shared and the SENTENCE is not. An earlier unification shared the finished
    /// message, which put this verb's wording under `lamella run:` -- where it read "`--board`
    /// builds one ahead of time", false of a verb whose `--board` runs on the host and is the one
    /// mode a Python program has. This holds deploy's half to deploy's meaning.
    #[test]
    fn the_deploy_refusal_speaks_for_deploys_own_routes_only() {
        let python = deploy_refusal(Path::new("app.py"), &Uncompilable::Python);
        assert!(python.starts_with("lamella deploy: "), "its own verb: {python}");
        assert!(python.contains("BUNDLE") && python.contains("lamella build"), "and the real route");
        assert!(
            !python.contains("deploy path is separate"),
            "it must not point at a route that also refuses Python: {python}"
        );
        let other = deploy_refusal(Path::new("notes.txt"), &Uncompilable::Other);
        assert!(other.contains("not a C# file"), "and says so plainly: {other}");
    }
}

const USAGE: &str = "\
usage: lamella deploy <file.cs|file.csproj> --target <t> [--no-run]    into firmware on the board
       lamella deploy <file.cs|file.csproj> --board <id> [--via probe|volume]   onto the bare chip
                               [--probe <serial>] [--volume <name>] [--device <serial>]
                               [--class-library]

--target is a live connection (what `lamella devices` prints); --board is a board model (what
`lamella boards` lists). The first keeps the board's firmware and takes about a second; the second
replaces everything on the chip and needs nothing there first.

--via chooses how the chip is written, on a board offering both routes. `volume` copies to the
bootloader drive and needs no probe; `probe` writes over an attached SWD probe and reads every
byte back to check it. The default is whatever the board takes without extra hardware. With more
than one probe attached, `--via probe` refuses until you name one with --probe <serial>.

--probe, --volume and --device name which probe, which drive and which USB DFU bootloader when
several are attached, each on its own route, as `lamella flash` takes them.

A .csproj builds every .cs beside it as ONE program and links the assemblies its <Reference>
elements name, each by a <HintPath>. Nothing is available to a build that its project does not
name. A single .cs file names no references and binds against the class library alone.

--class-library links the program with the class library and the runtime support archive, so it
may allocate, use floating point and call into System.*. Without it the flat tier is used, which
is linker-free and resolves no call outside the program. Every build says which tier produced it.
The class-library tier covers fewer boards; asking for it where there is no plan names the ones
there are.
";

/// Send a payload that is ALREADY built to firmware running at `target`.
///
/// **THE VERB THAT COMPILES AND THE VERB THAT SENDS ARE THE SAME VERB, AND THAT IS THE POINT.**
/// Somebody who built a `.lmli` on a build machine, or received one, has to be able to put it on a
/// board.
fn send_payload(path: &Path, target: &str, no_run: bool) -> ExitCode {
    if path.extension().and_then(|extension| extension.to_str()) == Some("lpyc") {
        eprintln!(
            "lamella deploy: {} is a Python bundle, which travels by a different wire message \
             (DEPLOY_BUNDLE)\nthan a baked C# image. The host side of that message is not a \
             library call yet, so this verb\ncannot send one -- and sending it down the image path \
             would deploy successfully and leave the\nboard unable to boot what it holds.",
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

/// What a source file is, when it is not something the ahead-of-time paths can compile.
///
/// **A LANGUAGE, NOT A SENTENCE.** Which sources can be compiled ahead of time is one fact and is
/// shared. What to tell the reader is not: `--board` names a board MODEL to `deploy` and a module
/// on THIS machine to `run`, so a consequence written for one verb is wrong in the other.
#[derive(Debug, PartialEq, Eq)]
pub enum Uncompilable {
    /// A language this project supports, on a path that cannot carry it.
    Python,
    /// Anything else -- no language claims the extension.
    Other,
}

/// Whether `path` names a source the ahead-of-time paths cannot compile.
///
/// **ONE RULE, EVERY CALLER THAT COMPILES A SOURCE AHEAD OF TIME.** A route that answered this
/// question for itself could disagree with the others by omission, and the answer is a property of
/// the compiler rather than of any one verb.
#[must_use]
pub fn uncompilable_source(path: &Path) -> Option<Uncompilable> {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("cs") => None,
        Some("py") => Some(Uncompilable::Python),
        _ => Some(Uncompilable::Other),
    }
}

/// `deploy`'s wording for [`uncompilable_source`]. Both of ITS routes put a compiled image on a
/// board, so neither takes Python and saying "use the other one" would be a circle.
pub fn deploy_refusal(path: &Path, what: &Uncompilable) -> String {
    match what {
        Uncompilable::Python => format!(
            "lamella deploy: {} is a Python program, and this verb compiles C#.\n\n\
             Neither route of this verb takes one: `--target` sends a baked C# image and \
             `--board`\nbuilds one ahead of time. A Python program reaches a board as a BUNDLE, \
             which `lamella build`\nproduces; this verb does not send bundles.",
            path.display()
        ),
        Uncompilable::Other => format!(
            "lamella deploy: {} is not a C# file. This verb deploys the baked C# image today; \
             `lamella build` produces the Python bundle, whose deploy path is separate.",
            path.display()
        ),
    }
}

/// Compile `path` and send it to Lamella firmware already running at `target`.
#[cfg(feature = "bake")]
fn to_running_firmware(path: &Path, target: &str, no_run: bool) -> ExitCode {
    if let Some(what) = uncompilable_source(path) {
        eprintln!("{}", deploy_refusal(path, &what));
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
        if let lamella_wire::TransportError::Refused { reason, .. } = error {
            eprintln!("lamella deploy: {target} refused the connection: {}", refusal(reason));
            return ExitCode::FAILURE;
        }
        if let lamella_wire::TransportError::VersionMismatch { target_min, target_max } = error {
            eprintln!(
                "lamella deploy: cannot talk to {target}: {}",
                lamella_wire_host::version_mismatch(lamella_wire::PROTOCOL_VERSION, target_min, target_max)
            );
            return ExitCode::FAILURE;
        }
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
    match start_deployed(&mut transport, START_ACK_PATIENCE) {
        Ok(line) => {
            println!("{line}");
            ExitCode::SUCCESS
        }
        Err(why) => {
            eprintln!("lamella deploy: {why}");
            ExitCode::FAILURE
        }
    }
}

/// How long to wait for the board to acknowledge a start. It answers before the reset the start
/// implies, so the answer is prompt.
const START_ACK_PATIENCE: Duration = Duration::from_secs(2);

/// Asks the board to start the image just deployed over `transport`, answering the line to print when
/// it started, and why it did not when it did not.
///
/// **THE BOARD's ACKNOWLEDGEMENT IS READ, AND ONLY IT SAYS "STARTED".** A board answers a start with
/// its reason when it will not make one, so a start sent and never read reported a refusal as a start.
fn start_deployed(transport: &mut impl lamella_wire::Transport, patience: Duration) -> Result<&'static str, String> {
    use lamella_wire_host::{StartFailure, exec};
    match lamella_wire_host::start_execution(transport, 2, exec::exec_source::DEPLOYED, 0, patience) {
        Ok(()) => Ok("started it."),
        Err(StartFailure::NoAnswer) => Err(
            "the image is on the board, and the board did not acknowledge the start, so whether it is \
             running is not known. Resetting the board runs it."
                .to_owned(),
        ),
        Err(failure @ (StartFailure::Refused(_) | StartFailure::Transport(lamella_wire::TransportError::Refused { .. }))) => {
            Err(format!("the image is on the board, and {failure}."))
        }
        Err(failure @ StartFailure::Transport(_)) => {
            Err(format!("the image is on the board, and {failure}. Resetting the board runs it."))
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

    /// A target that answers the first frame it is sent with the frames `answer` builds.
    struct Answering {
        wire: lamella_wire::MemTransport,
        answer: Option<Vec<u8>>,
    }

    impl Answering {
        fn with(answer: impl FnOnce(&mut lamella_wire::MemTransport)) -> Self {
            let mut peer = lamella_wire::MemTransport::new();
            answer(&mut peer);
            Answering { wire: lamella_wire::MemTransport::new(), answer: Some(peer.take_sent()) }
        }
    }

    impl lamella_wire::Transport for Answering {
        fn send(&mut self, _msg_type: u8, _seq: u16, _payload: &[u8]) -> Result<(), lamella_wire::TransportError> {
            if let Some(answer) = self.answer.take() {
                self.wire.feed(&answer);
            }
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, lamella_wire::TransportError> {
            lamella_wire::Transport::poll(&mut self.wire)
        }
    }

    /// A board answering a start with the acknowledgement `code`.
    fn acknowledging(code: u8) -> Answering {
        Answering::with(|peer| {
            lamella_wire::Transport::send(peer, lamella_wire_host::exec::EXEC_ACK, 2, &[code]).unwrap();
        })
    }

    /// A start is reported as made only when the board acknowledges it. A start the board refuses is
    /// reported with the board's reason, a board that does not answer has not been shown to start, and
    /// either way the deploy itself is said to have succeeded.
    #[test]
    fn a_start_is_reported_as_made_only_when_the_board_acknowledges_it() {
        let quick = Duration::from_millis(50);
        let started = start_deployed(&mut acknowledging(lamella_wire_host::exec::ack::STARTED), quick);
        assert_eq!(started, Ok("started it."));

        let refused = start_deployed(&mut acknowledging(lamella_wire_host::exec::ack::NOTHING_TO_RUN), quick)
            .expect_err("a refused start did not start");
        assert!(refused.contains("NOTHING_TO_RUN"), "names the board's reason: {refused}");
        assert!(refused.contains("the image is on the board"), "and that the deploy succeeded: {refused}");

        let silent = start_deployed(&mut Answering::with(|_| {}), quick).expect_err("silence is not a start");
        assert!(silent.contains("did not acknowledge"), "{silent}");
        assert!(silent.contains("the image is on the board"), "{silent}");
    }
}
