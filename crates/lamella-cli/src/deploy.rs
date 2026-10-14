//! `lamella deploy`: take a program and put it on a board.

use crate::args::{self, Spec};
use lamella_wire::Capabilities;
use lamella_wire_host::on_device::{self, capitalized, describe_refusal, host_caps, the_board};
use lamella_wire_host::{AnyTransport, TransferAck, board_name, hello_blocking, open_target};
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// The option that stops the program a board's firmware stores, and erases it.
pub const ERASE_FLAG: &str = "--erase";

/// What a deploy over a `--device` does once the image is on the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterDeploy {
    /// Nothing: the image is stored and runs at the board's next reset (`--no-run`).
    Store,
    /// Start it and return (the default).
    Start,
}

/// The default serial baud for a Lamella Link carrier (USB-CDC ignores it; a real UART wants it).
const BAUD: u32 = 115_200;

/// How long to wait on each wire exchange. Generous: a deploy erases and writes flash on the far
/// side, which is slow in a way a timeout should not be racing.
const TIMEOUT: Duration = Duration::from_secs(20);

/// The options `deploy` takes, for the parser and for the tests that parse as it does.
const SPEC: Spec<'static> = Spec {
    verb: "deploy",
    usage: Some(USAGE),
    values: &["--device", "--board", "--probe", "--volume", "--dfu", "--via"],
    flags: &[
        "--no-run",
        "--unsafe",
        crate::flash::CLASS_LIBRARY_FLAG,
        crate::flash::FLAT_FLAG,
        ERASE_FLAG,
    ],
};

pub fn deploy_command(args: &[String]) -> ExitCode {
    let parsed = match args::parse_or_halt(args, &SPEC) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    if parsed.flag(ERASE_FLAG) {
        return match erase_target(&parsed) {
            Ok(target) => erase_stored(target),
            Err(refusal) => {
                eprintln!("{refusal}");
                ExitCode::FAILURE
            }
        };
    }
    let after = if parsed.flag("--no-run") { AfterDeploy::Store } else { AfterDeploy::Start };
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

    let tier = match crate::flash::Tier::from_options(&parsed, "deploy") {
        Ok(tier) => tier,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(flag) = crate::flash::Tier::typed(&parsed)
        && parsed.value("--device").is_some()
    {
        eprintln!(
            "{}",
            crate::flash::tier_flag_where_nothing_links(
                "deploy",
                flag,
                "--device sends firmware that is already running an image baked with the corlib \
                 the program\nwas compiled against, and nothing is linked on the way.",
                "Use --board <id> to compile and write the chip, which is the route that links.",
                "Nothing was sent.",
            )
        );
        return ExitCode::FAILURE;
    }
    let libraries: Vec<crate::flash::Library> = Vec::new();

    match (parsed.value("--board"), parsed.value("--device")) {
        (Some(_), Some(_)) => {
            eprintln!(
                "lamella deploy: --board and --device name different destinations, so give one.\n\n\
                 \x20   --board <id>      compile and write the CHIP, over a probe (nothing needs \
                 to be on it)\n\
                 \x20   --device <d>      compile and send it to firmware ALREADY running there"
            );
            ExitCode::FAILURE
        }
        (Some(_), None) if kind == lamella_flash_routes::artifact::Kind::WirePayload => {
            eprintln!(
                "lamella deploy: {} is loaded by firmware already on the board, so it needs a \
                 --device rather\nthan a --board. Writing it to a bare chip would leave the board \
                 resetting into a file format.\n\n\
                 \x20   lamella deploy {} --device <d>",
                path.display(),
                path.display()
            );
            ExitCode::FAILURE
        }
        (Some(board_id), None) => {
            if let Err(error) = crate::project::board_for_build("deploy", &path, Some(board_id)) {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
            crate::flash::deploy_to_chip(
                &path,
                board_id,
                parsed.value("--probe"),
                parsed.value("--volume"),
                parsed.value("--dfu"),
                parsed.value("--via"),
                parsed.flag("--unsafe"),
                tier,
                &libraries,
            )
        }
        (None, Some(target)) if kind == lamella_flash_routes::artifact::Kind::WirePayload => {
            send_payload(&path, target, after)
        }
        (None, Some(target)) => to_running_firmware(&path, target, after),
        (None, None) => {
            eprintln!(
                "lamella deploy: name where it goes.\n\n{USAGE}\n\
                 `lamella devices` lists what is attached and prints the --device for each one;\n\
                 `lamella boards` lists every --board this build knows."
            );
            ExitCode::FAILURE
        }
    }
}

/// The `--device` an erase is for, or why the command line cannot be one.
///
/// **AN ERASE TAKES A CONNECTION AND NOTHING ELSE.** Every other option this verb has either
/// names something to send or says how to send it, and an erase sends nothing -- so an option
/// given beside it is refused rather than ignored, because an option dropped without a word reads
/// as one that was honored.
///
/// # Errors
/// A program named, an option an erase has no use for, or no `--device`.
fn erase_target(parsed: &args::Options) -> Result<&str, String> {
    if let Some(program) = parsed.positionals().first() {
        return Err(format!(
            "lamella deploy: {ERASE_FLAG} takes no program: it erases the one the board already \
             stores.\n\n\
             To replace that program with {program} instead, deploy it -- the board then runs \
             {program}:\n\
             \x20   lamella deploy {program} --device <d>"
        ));
    }
    if parsed.value("--board").is_some() {
        return Err(format!(
            "lamella deploy: {ERASE_FLAG} asks the firmware on a board to erase the program it \
             stores, so it needs a live\nconnection to that firmware -- a --device, as `lamella \
             devices` prints it -- rather than a --board.\n\n\
             \x20   lamella deploy {ERASE_FLAG} --device <d>"
        ));
    }
    let unused: Vec<&str> = ["--probe", "--volume", "--dfu", "--via"]
        .into_iter()
        .filter(|name| parsed.value(name).is_some())
        .chain(
            [
                "--no-run",
                "--unsafe",
                crate::flash::CLASS_LIBRARY_FLAG,
                crate::flash::FLAT_FLAG,
            ]
            .into_iter()
            .filter(|name| parsed.flag(name)),
        )
        .collect();
    if !unused.is_empty() {
        return Err(format!(
            "lamella deploy: {ERASE_FLAG} sends nothing, so it takes only --device, and {} would \
             have no effect.\n\n\
             \x20   lamella deploy {ERASE_FLAG} --device <d>",
            unused.join(" and ")
        ));
    }
    parsed.value("--device").ok_or_else(|| {
        format!(
            "lamella deploy: name the board to erase.\n\n\
             \x20   lamella deploy {ERASE_FLAG} --device <d>\n\n\
             `lamella devices` lists what is attached and prints the --device for each one."
        )
    })
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
usage: lamella deploy <file.cs|file.csproj> --device <d>      into firmware on the board
                               [--no-run]
       lamella deploy --erase --device <d>                    stop and erase its stored program
       lamella deploy <file.cs|file.csproj> --board <id> [--via probe|volume]   onto the bare chip
                               [--probe <serial>] [--volume <name>] [--dfu <serial>]
                               [--nostdlib]

--device is a live connection (what `lamella devices` prints); --board is a board model (what
`lamella boards` lists). The first keeps the board's firmware and writes only the program; the
second replaces everything on the chip and needs nothing there first.

Over a --device, the program is written to the board's flash, where it stays, and the board then
starts it from a reset, as it starts it at every power-up. A board that already stores exactly
this program is sent nothing. The write is reported a quarter of the image at a time, and each
part is checked against what the board reads back from its flash. Over a Pico 2 W's own USB a
326 KB program is written in about 2 seconds, or 3 when a larger one has to be erased first; a
serial line takes 256 bytes at a time, and far longer.

--no-run stores the program without starting it. To start it and watch what it prints, use
`lamella run <file> --device <d>`, which deploys it the same way.

--erase stops the program the board is running and erases the one it stores, so at each reset the
board starts no program and waits for a deploy. Deploying a program replaces the stored one.

--via chooses how the chip is written, on a board offering both routes. `volume` copies to the
bootloader drive and needs no probe; `probe` writes over an attached SWD probe and reads every
byte back to check it. The default is whatever the board takes without extra hardware. With more
than one probe attached, `--via probe` refuses until you name one with --probe <serial>.

--probe, --volume and --dfu name which probe, which drive and which USB DFU bootloader when
several are attached, each on its own route, as `lamella flash` takes them.

A .csproj builds every .cs beside it as ONE program and links the assemblies its <Reference>
elements name, each by a <HintPath>. Nothing is available to a build that its project does not
name. A single .cs file names no references and binds against the class library alone.

--device sends an image baked with the corlib and the libraries the program was compiled against
-- the class library's own, System.Device.Gpio among them, and the ones a project's <Reference>
elements name -- keeping only what Main reaches. A library the program reaches through another is
carried only when the program was compiled against it too, and otherwise the deploy is refused,
naming it.

--board links the program with the class library and the runtime support archive, so it may
allocate, use floating point and call into System.*. That tier's collector reclaims an object
without finalizing it, so a finalizer (a class's ~destructor) never runs there. It covers fewer
boards than the flat tier, and a board it has no plan for is refused, naming the ones there are.
--nostdlib builds the flat tier instead, which is linker-free and resolves no call outside the
program. Every build says which tier produced it.

A class-library image is compiled as a debug build is, with the debug information set aside, so
`lamella build <file> --board <id> --format elf` for the same program describes exactly the image
deployed, and a debugger can attach to the board with it. The flat tier is compiled without debug
information.
";

/// Send a payload that is ALREADY built to firmware running at `target`.
///
/// **THE VERB THAT COMPILES AND THE VERB THAT SENDS ARE THE SAME VERB, AND THAT IS THE POINT.**
/// Somebody who built a `.lmli` on a build machine, or received one, has to be able to put it on a
/// board.
fn send_payload(path: &Path, target: &str, after: AfterDeploy) -> ExitCode {
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
    send_image(&image, target, after, None)
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
             Neither route of this verb takes one: `--device` sends a baked C# image and \
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
fn to_running_firmware(path: &Path, target: &str, after: AfterDeploy) -> ExitCode {
    let board = match crate::project::BoardCheck::for_file("deploy", path) {
        Ok(board) => board,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(refusal) = crate::bake::refuse_uncompilable(path, deploy_refusal) {
        eprintln!("{refusal}");
        return ExitCode::FAILURE;
    }
    let Some((mut transport, session)) = connect("deploy", target, host_caps(), board.as_ref()) else {
        return ExitCode::FAILURE;
    };
    let answered = lamella_wire_host::boards::board_answering(session.identity.product_model);
    let image = match crate::bake::image_for_firmware(path, "deploy", deploy_refusal, answered.as_deref()) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    println!("built {} B from {}", image.len(), path.display());
    deliver(&mut transport, &session, target, &image, after)
}

/// Open `target` and complete a HELLO offering `caps`, or say why not and answer `None`.
///
/// **ONE OPENING FOR EVERY ROUTE THAT SPEAKS TO FIRMWARE** -- a deploy, an erase, and `lamella run
/// --device` -- so none of them can come to describe the same unanswering board in words of its
/// own. `verb` is the command each message speaks for.
///
/// A project that names its board brings `board`, and a board that answers as another one is
/// refused here, before anything but the HELLO has been sent.
pub(crate) fn connect(
    verb: &str,
    target: &str,
    caps: Capabilities,
    board: Option<&crate::project::BoardCheck>,
) -> Option<(AnyTransport, lamella_wire::Negotiated)> {
    let mut transport = open(verb, target)?;
    match hello_on(&mut transport, 0, verb, target, caps) {
        Ok(session) => match board.map(|board| board.against(verb, session.identity.product_model)) {
            Some(Err(refusal)) => {
                eprintln!("{refusal}");
                None
            }
            _ => Some((transport, session)),
        },
        Err(refusal) => {
            eprintln!("{refusal}");
            None
        }
    }
}

/// Open `target`, or say why not and answer `None`.
fn open(verb: &str, target: &str) -> Option<AnyTransport> {
    match open_target(target, BAUD, TIMEOUT) {
        Ok(transport) => Some(transport),
        Err(error) => {
            eprintln!("lamella {verb}: cannot open {target}: {error:?}");
            eprintln!(
                "\nthis build can open: {}.\n\
                 `lamella devices` lists what is attached and what to write here.",
                lamella_wire_host::available_carriers().join(", ")
            );
            None
        }
    }
}

/// Complete a HELLO offering `caps` with the firmware at the other end of `transport`, or say why
/// not, in `verb`'s words.
///
/// # Errors
/// The board refused the session, speaks another protocol version, or did not answer -- each in
/// its own words.
pub(crate) fn hello_on(
    transport: &mut impl lamella_wire::Transport,
    seq: u16,
    verb: &str,
    target: &str,
    caps: Capabilities,
) -> Result<lamella_wire::Negotiated, String> {
    hello_blocking(transport, seq, caps, TIMEOUT).map_err(|error| match error {
        lamella_wire::TransportError::Refused { reason, .. } => {
            format!("lamella {verb}: {target} refused the connection: {}", describe_refusal(reason))
        }
        lamella_wire::TransportError::VersionMismatch { target_min, target_max } => format!(
            "lamella {verb}: cannot talk to {target}: {}",
            lamella_wire_host::version_mismatch(lamella_wire::PROTOCOL_VERSION, target_min, target_max)
        ),
        error => format!(
            "lamella {verb}: {target} did not answer a HELLO ({error:?}).\n{}",
            no_answer()
        ),
    })
}

/// Put `image` on the firmware running at `target`, then do what `after` says.
///
/// **ONE SENDER FOR A COMPILED IMAGE AND A PREBUILT ONE**, so the wire behavior, the timeouts and
/// every message a reader sees are the same either way. The firmware cannot tell where the bytes
/// came from and neither should the output. `board` is the project's own board, when it names one.
fn send_image(
    image: &[u8],
    target: &str,
    after: AfterDeploy,
    board: Option<&crate::project::BoardCheck>,
) -> ExitCode {
    let Some((mut transport, session)) = connect("deploy", target, host_caps(), board) else {
        return ExitCode::FAILURE;
    };
    deliver(&mut transport, &session, target, image, after)
}

/// Store `image` on the board at the other end of `transport`, whose HELLO `session` is, then do
/// what `after` says.
fn deliver(
    transport: &mut AnyTransport,
    session: &lamella_wire::Negotiated,
    target: &str,
    image: &[u8],
    after: AfterDeploy,
) -> ExitCode {
    if let Err(refusal) = on_device::store_image(transport, session, target, image, &mut std::io::stdout()) {
        eprintln!("lamella deploy: {refusal}");
        return ExitCode::FAILURE;
    }
    if after == AfterDeploy::Store {
        println!("not started (--no-run). It runs at the board's next reset.");
        return ExitCode::SUCCESS;
    }
    match on_device::start_deployed(transport, on_device::START_PATIENCE) {
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

/// How long to wait for a board to say whether a program is running. A running program answers
/// between two of its own steps, so the answer is prompt.
const STATUS_PATIENCE: Duration = Duration::from_secs(2);

/// Whether the board at the other end of `transport` is running a program: `Some(true)` when it
/// is, `Some(false)` when it is idle, `None` when it does not say.
///
/// **ASKED BEFORE THE HELLO**, because a HELLO takes the board back from the program it is running,
/// and then nothing would be running to report. A running program answers this between steps and
/// carries on.
fn running_now(
    transport: &mut impl lamella_wire::Transport,
    seq: u16,
    patience: Duration,
) -> Option<bool> {
    use lamella_wire_host::exec;
    transport.send(exec::EXEC_STATUS, seq, &[]).ok()?;
    let deadline = Instant::now() + patience;
    while Instant::now() < deadline {
        while let Ok(Some(frame)) = transport.poll() {
            if frame.seq != seq {
                continue;
            }
            if frame.msg_type == exec::EXEC_ACK {
                return match frame.payload.first().copied() {
                    Some(exec::ack::RUNNING) => Some(true),
                    Some(exec::ack::IDLE) => Some(false),
                    _ => None,
                };
            }
            if frame.msg_type == lamella_wire::msg::ERROR {
                return None;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

/// Stop the program the firmware at `target` is running, erase the one it stores, and say what the
/// board reported at each step.
fn erase_stored(target: &str) -> ExitCode {
    let Some(mut transport) = open("deploy", target) else {
        return ExitCode::FAILURE;
    };
    let erased = erase_on(&mut transport, target);
    for line in &erased.lines {
        println!("{line}");
    }
    match erased.refusal {
        None => ExitCode::SUCCESS,
        Some(refusal) => {
            eprintln!("{refusal}");
            ExitCode::FAILURE
        }
    }
}

/// What an erase found, as it is printed: the lines that report what happened, and the refusal
/// that ended it early or the answer that contradicts it, if either.
#[derive(Debug, Default)]
struct Erased {
    lines: Vec<String>,
    refusal: Option<String>,
}

impl Erased {
    /// The erase ends here, with `refusal`.
    fn refused(mut self, refusal: String) -> Self {
        self.refusal = Some(refusal);
        self
    }
}

/// [`erase_stored`] over a carrier that is already open.
///
/// **EVERY LINE IS THE BOARD'S OWN ANSWER.** Whether a program was running, whether one was stored
/// and whether one is stored now are each asked rather than assumed, so a board that took the erase
/// and still holds a program is reported as exactly that rather than as done.
fn erase_on(transport: &mut impl lamella_wire::Transport, target: &str) -> Erased {
    use lamella_wire_host::{Aborted, abort_blocking, clear_deployed_blocking, deploy_status_blocking};
    let mut erased = Erased::default();
    let was_running = running_now(transport, 1, STATUS_PATIENCE);
    let session = match hello_on(transport, 2, "deploy", target, host_caps()) {
        Ok(session) => session,
        Err(refusal) => return erased.refused(refusal),
    };
    let name = board_name(session.identity.product_model);
    let board = the_board(name);
    let aborted = match abort_blocking(transport, 3, TIMEOUT) {
        Ok(aborted) => aborted,
        Err(lamella_wire::TransportError::Refused { .. }) => Aborted::NothingWasRunning,
        Err(error) => {
            return erased.refused(format!(
                "lamella deploy: {board} answered the connection and then did not answer an ABORT \
                 within {} s ({error:?}):\nit is neither waiting for a host nor running a program \
                 this tool can stop. Nothing was erased.",
                TIMEOUT.as_secs()
            ));
        }
    };
    if was_running == Some(true) || aborted == Aborted::StoppedAProgram {
        erased.lines.push(format!("stopped the program {board} was running."));
    }
    let stored = deploy_status_blocking(transport, 4, TIMEOUT).ok();
    let count = match clear_deployed_blocking(transport, 5, TIMEOUT) {
        Ok(cleared) if cleared.ack == TransferAck::Accepted => cleared.erased,
        Ok(_) => {
            return erased.refused(format!(
                "lamella deploy: {board} did not finish erasing its stored program, so some of it \
                 may still be in its flash\nand it may start again at its next reset."
            ));
        }
        Err(lamella_wire::TransportError::Refused { reason, .. }) => {
            return erased
                .refused(format!("lamella deploy: {board} refused the erase: {}", describe_refusal(reason)));
        }
        Err(error) => {
            return erased.refused(format!("lamella deploy: the erase on {board} failed: {error:?}"));
        }
    };
    let now = deploy_status_blocking(transport, 6, TIMEOUT).ok();
    match erase_report(&board, stored, now, count) {
        Ok(line) => erased.lines.push(line),
        Err(contradiction) => return erased.refused(format!("lamella deploy: {contradiction}")),
    }
    erased
}

/// What an erase reports, from what the board said it stored before the erase and after it, and
/// how many bytes of flash it says it erased.
///
/// **A SIZE IS STATED ONLY WHERE THE BOARD STATES ONE.** A board that does not report it may have
/// erased only enough to invalidate the program, and then the rest of its bytes are still in flash
/// -- which matters to anybody who stored a key or a password in it -- so the line says that rather
/// than implying more.
///
/// # Errors
/// The board accepted the erase and still reports a stored program.
fn erase_report(
    board: &str,
    stored: Option<lamella_wire_host::DeployStatus>,
    now: Option<lamella_wire_host::DeployStatus>,
    erased: Option<u32>,
) -> Result<String, String> {
    let after = "at each reset it now starts no program and waits for a deploy.";
    let unsaid = "It does not say how much of its flash it erased, so parts of the old program may \
                  remain readable there.";
    let stored = stored.map(|status| status.checksum.is_some());
    match (stored, now.map(|status| status.checksum.is_some()), erased) {
        (_, Some(true), _) => Err(format!(
            "{board} accepted the erase and still reports a stored program, so it may start it \
             again at its next reset."
        )),
        (Some(true), Some(false), Some(bytes)) => Ok(format!(
            "erased the stored program ({} of flash). {} reports nothing deployed, so {after}",
            size(bytes),
            capitalized(board)
        )),
        (Some(true), Some(false), None) => Ok(format!(
            "erased the stored program. {} reports nothing deployed, so {after}\n{unsaid}",
            capitalized(board)
        )),
        (Some(false), Some(false), Some(bytes)) => Ok(format!(
            "{} held no stored program, and erased {} of flash an earlier one had left; {after}",
            capitalized(board),
            size(bytes)
        )),
        (Some(false), Some(false), None) => Ok(format!(
            "{} held no stored program, so there was nothing to erase; {after}",
            capitalized(board)
        )),
        (_, _, Some(bytes)) => Ok(format!(
            "{} accepted the erase and erased {} of flash. It does not report what it stores, so \
             this tool\ncannot confirm that nothing is stored now.",
            capitalized(board),
            size(bytes)
        )),
        (_, _, None) => Ok(format!(
            "{} accepted the erase. It does not report what it stores, so this tool cannot confirm \
             that\nnothing is stored now.",
            capitalized(board)
        )),
    }
}

/// `bytes` as a reader reads a flash size: whole kilobytes where it is one, and bytes otherwise.
fn size(bytes: u32) -> String {
    if bytes % 1024 == 0 {
        format!("{} KB", bytes / 1024)
    } else {
        format!("{bytes} B")
    }
}

/// The wire route in a build that cannot bake, naming the feature rather than the verb.
///
/// **THE CHIP ROUTE STILL WORKS IN THIS BUILD, WHICH IS WHY THIS IS PER-ROUTE RATHER THAN PER-VERB.**
/// Only the wire route needs a baked image, so a default build deploys to a chip perfectly well and
/// a reader must not be told that `deploy` is unavailable when half of it is not.
#[cfg(not(feature = "bake"))]
fn to_running_firmware(_path: &Path, _target: &str, _after: AfterDeploy) -> ExitCode {
    eprintln!(
        "lamella deploy: this build cannot deploy over a --device.\n\n\
         Sending a program to firmware already on the board bakes it into a flash image first, \
         which\nneeds the `bake` feature:\n\
         \x20   cargo build -p lamella-cli --features bake\n\n\
         The feature is off by default because the baking code is not additive -- reaching the \
         shared\nloader would stop other crates in this workspace compiling.\n\n\
         `lamella deploy <file> --board <id>` works in this build: it writes the chip over a probe."
    );
    ExitCode::FAILURE
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
     A --device sends a program to firmware that is ALREADY on the board. To put a program on a \
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
        assert!(USAGE.contains("--device"), "got {USAGE}");
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

    /// The options `deploy` sees for `words`.
    fn parsed(words: &[&str]) -> args::Options {
        let words: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        args::parse(&words, &SPEC).expect("the words parse")
    }

    /// **`--follow` IS NOT AN OPTION OF THIS VERB**: running a program and watching what it prints
    /// is what `lamella run --device` is for.
    #[test]
    fn the_follow_option_is_gone() {
        let words: Vec<String> = ["App.cs", "--device", "usb", "--follow"].iter().map(|word| (*word).to_owned()).collect();
        let refusal = args::parse(&words, &SPEC).expect_err("--follow is not an option of deploy");
        assert!(refusal.contains("unknown option"), "{refusal}");
        assert!(!USAGE.contains("--follow"), "and the usage does not offer it");
    }

    /// **AN ERASE TAKES A CONNECTION AND NOTHING ELSE**, and each refusal names what to type.
    #[test]
    fn an_erase_takes_a_target_and_nothing_else() {
        assert_eq!(erase_target(&parsed(&[ERASE_FLAG, "--device", "usb"])), Ok("usb"));
        let program = erase_target(&parsed(&[ERASE_FLAG, "--device", "usb", "App.cs"]))
            .expect_err("an erase sends nothing");
        assert!(program.contains("takes no program"), "{program}");
        assert!(program.contains("lamella deploy App.cs --device <d>"), "and how to replace it: {program}");
        let board = erase_target(&parsed(&[ERASE_FLAG, "--board", "rpi-pico2"]))
            .expect_err("a board model has no firmware to ask");
        assert!(board.contains("--device"), "{board}");
        let extra = erase_target(&parsed(&[ERASE_FLAG, "--device", "usb", "--no-run", "--unsafe"]))
            .expect_err("options an erase has no use for are refused, not ignored");
        assert!(extra.contains("--no-run and --unsafe would have no effect"), "names both: {extra}");
        let nowhere = erase_target(&parsed(&[ERASE_FLAG])).expect_err("no board was named");
        assert!(nowhere.contains("lamella devices"), "{nowhere}");
    }

    /// **WHAT THE BOARD STORED BEFORE AND AFTER DECIDES THE LINE**, and a board still reporting a
    /// program is not reported as erased. No size is claimed: the board reports none.
    #[test]
    fn an_erase_reports_what_the_board_said_before_and_after() {
        use lamella_wire_host::DeployStatus;
        let held = Some(DeployStatus { checksum: Some(7), window: Some(4096) });
        let empty = Some(DeployStatus { checksum: None, window: Some(4096) });
        let erased = erase_report("the Pico 2 W", held, empty, Some(331_776))
            .expect("a program was there and is gone");
        assert!(
            erased.starts_with("erased the stored program (324 KB of flash). The Pico 2 W reports nothing deployed"),
            "{erased}"
        );
        assert!(erased.contains("starts no program and waits for a deploy"), "{erased}");
        let unreported = erase_report("the Pico 2 W", held, empty, None).expect("a board that does not say how much");
        assert!(unreported.starts_with("erased the stored program. The Pico 2 W"), "{unreported}");
        assert!(unreported.contains("may remain readable"), "it says what an unreported erase may leave: {unreported}");
        let nothing = erase_report("the Pico 2 W", empty, empty, None).expect("nothing was there");
        assert!(nothing.starts_with("The Pico 2 W held no stored program, so there was nothing"), "{nothing}");
        let remnant = erase_report("the Pico 2 W", empty, empty, Some(8192)).expect("a remnant was erased");
        assert!(remnant.contains("erased 8 KB of flash an earlier one had left"), "{remnant}");
        let still = erase_report("the Pico 2 W", held, held, Some(4096)).expect_err("still stored is not erased");
        assert!(still.contains("still reports a stored program"), "{still}");
        let unknown = erase_report("the board", None, None, None).expect("a board that does not say");
        assert!(unknown.contains("cannot confirm"), "{unknown}");
        assert_eq!(size(331_776), "324 KB");
        assert_eq!(size(1_000), "1000 B");
    }

    /// A board answers whether a program is running, and a board that does not say is not taken to
    /// have said no.
    #[test]
    fn a_board_says_whether_a_program_is_running() {
        use lamella_wire_host::exec;
        let quick = Duration::from_millis(50);
        let answering = |code: u8| {
            Answering::with(move |peer| {
                lamella_wire::Transport::send(peer, exec::EXEC_ACK, 9, &[code]).unwrap();
            })
        };
        assert_eq!(running_now(&mut answering(exec::ack::RUNNING), 9, quick), Some(true));
        assert_eq!(running_now(&mut answering(exec::ack::IDLE), 9, quick), Some(false));
        let mut refusing = Answering::with(|peer| {
            let payload = lamella_wire::error::unknown_message_type(exec::EXEC_STATUS);
            lamella_wire::Transport::send(peer, lamella_wire::msg::ERROR, 9, &payload).unwrap();
        });
        assert_eq!(running_now(&mut refusing, 9, quick), None, "a firmware that predates the question");
        assert_eq!(running_now(&mut Answering::with(|_| {}), 9, quick), None, "and one that is silent");
    }

    /// **AN ERASE AGAINST THE DEPLOY SERVE A BOARD'S FIRMWARE RUNS.** A board holding a baked program
    /// is asked whether one is running, taken back, asked what it stores, told to erase and asked
    /// again, and the report is what it answered: erased, and the second time nothing to erase.
    #[cfg(feature = "bake")]
    #[test]
    fn an_erase_against_the_firmware_serve_reports_what_the_board_answered() {
        let Ok(set) = lamella_wire_host::boards::ReferenceSet::discover(None) else {
            return;
        };
        let image = crate::bake::compile_and_bake(
            &set,
            "class Program\n{\n    static int Main()\n    {\n        return 42;\n    }\n}\n",
            "run",
        )
        .expect("a program bakes");
        let mut board =
            lamella_wire_host::loopback::LoopbackBoard::on(lamella_wire_host::loopback::RamFlash::holding(&image));
        assert!(
            lamella_cil_runtime::verified_image_checksum(board.flash.region).is_some(),
            "the board holds a verified program to begin with"
        );

        let first = erase_on(&mut board, "loopback");
        assert_eq!(first.refusal, None, "{first:?}");
        assert_eq!(first.lines.len(), 1, "nothing was running, so one line: {first:?}");
        assert!(first.lines[0].starts_with("erased the stored program."), "{first:?}");
        assert_eq!(board.flash.erases, 1, "the board was told to erase, once");
        assert!(
            lamella_cil_runtime::verified_image_checksum(board.flash.region).is_none(),
            "and holds no program now"
        );

        let second = erase_on(&mut board, "loopback");
        assert_eq!(second.refusal, None, "{second:?}");
        assert!(second.lines[0].contains("held no stored program"), "{second:?}");
    }
}
