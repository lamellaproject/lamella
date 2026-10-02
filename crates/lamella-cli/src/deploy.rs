//! `lamella deploy`: take a program and put it on a board.

use crate::args::{self, Spec};
use lamella_wire::Capabilities;
use lamella_wire_host::{
    AnyTransport, TransferAck, board_name, deploy_image_blocking, hello_blocking, open_target,
};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

/// The option that stops the program a board's firmware stores, and erases it.
pub const ERASE_FLAG: &str = "--erase";

/// The option that stays after a deploy starts the program, printing its output as it runs.
pub const FOLLOW_FLAG: &str = "--follow";

/// What a deploy over a `--target` does once the image is on the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AfterDeploy {
    /// Nothing: the image is stored and runs at the board's next reset (`--no-run`).
    Store,
    /// Start it and return (the default).
    Start,
    /// Start it, then print what it prints until it ends or this tool is stopped
    /// ([`FOLLOW_FLAG`]).
    Follow,
}

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

/// The options `deploy` takes, for the parser and for the tests that parse as it does.
const SPEC: Spec<'static> = Spec {
    verb: "deploy",
    usage: Some(USAGE),
    values: &["--target", "--board", "--probe", "--volume", "--device", "--via"],
    flags: &[
        "--no-run",
        "--unsafe",
        crate::flash::CLASS_LIBRARY_FLAG,
        crate::flash::FLAT_FLAG,
        ERASE_FLAG,
        FOLLOW_FLAG,
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
    let after = match after_deploy(&parsed) {
        Ok(after) => after,
        Err(refusal) => {
            eprintln!("{refusal}");
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
        && parsed.value("--target").is_some()
    {
        eprintln!(
            "{}",
            crate::flash::tier_flag_where_nothing_links(
                "deploy",
                flag,
                "--target sends firmware that is already running an image baked with the corlib \
                 the program\nwas compiled against, and nothing is linked on the way.",
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
        (Some(_), None) if after == AfterDeploy::Follow => {
            eprintln!(
                "lamella deploy: {FOLLOW_FLAG} prints what the program prints, which the board's \
                 firmware sends over a\nlive connection -- so it needs a --target rather than a \
                 --board. --board writes the chip and\nleaves.\n\n\
                 \x20   lamella deploy {} --target <t> {FOLLOW_FLAG}\n\n\
                 `lamella devices` prints the --target for each attached board.",
                path.display()
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
            send_payload(&path, target, after)
        }
        (None, Some(target)) => to_running_firmware(&path, target, after),
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

/// What `--no-run` and [`FOLLOW_FLAG`] ask a deploy over a `--target` to do once the image is
/// there.
///
/// # Errors
/// Both at once: one stores the program without starting it, the other starts it and watches.
fn after_deploy(parsed: &args::Options) -> Result<AfterDeploy, String> {
    match (parsed.flag("--no-run"), parsed.flag(FOLLOW_FLAG)) {
        (true, true) => Err(format!(
            "lamella deploy: --no-run and {FOLLOW_FLAG} ask for opposite things, so give one.\n\n\
             \x20   --no-run      store the program; it starts at the board's next reset\n\
             \x20   {FOLLOW_FLAG}      start it, and print what it prints until it ends"
        )),
        (true, false) => Ok(AfterDeploy::Store),
        (false, true) => Ok(AfterDeploy::Follow),
        (false, false) => Ok(AfterDeploy::Start),
    }
}

/// The `--target` an erase is for, or why the command line cannot be one.
///
/// **AN ERASE TAKES A CONNECTION AND NOTHING ELSE.** Every other option this verb has either
/// names something to send or says how to send it, and an erase sends nothing -- so an option
/// given beside it is refused rather than ignored, because an option dropped without a word reads
/// as one that was honored.
///
/// # Errors
/// A program named, an option an erase has no use for, or no `--target`.
fn erase_target(parsed: &args::Options) -> Result<&str, String> {
    if let Some(program) = parsed.positionals().first() {
        return Err(format!(
            "lamella deploy: {ERASE_FLAG} takes no program: it erases the one the board already \
             stores.\n\n\
             To replace that program with {program} instead, deploy it -- the board then runs \
             {program}:\n\
             \x20   lamella deploy {program} --target <t>"
        ));
    }
    if parsed.value("--board").is_some() {
        return Err(format!(
            "lamella deploy: {ERASE_FLAG} asks the firmware on a board to erase the program it \
             stores, so it needs a live\nconnection to that firmware -- a --target, as `lamella \
             devices` prints it -- rather than a --board.\n\n\
             \x20   lamella deploy {ERASE_FLAG} --target <t>"
        ));
    }
    let unused: Vec<&str> = ["--probe", "--volume", "--device", "--via"]
        .into_iter()
        .filter(|name| parsed.value(name).is_some())
        .chain(
            [
                "--no-run",
                "--unsafe",
                crate::flash::CLASS_LIBRARY_FLAG,
                crate::flash::FLAT_FLAG,
                FOLLOW_FLAG,
            ]
            .into_iter()
            .filter(|name| parsed.flag(name)),
        )
        .collect();
    if !unused.is_empty() {
        return Err(format!(
            "lamella deploy: {ERASE_FLAG} sends nothing, so it takes only --target, and {} would \
             have no effect.\n\n\
             \x20   lamella deploy {ERASE_FLAG} --target <t>",
            unused.join(" and ")
        ));
    }
    parsed.value("--target").ok_or_else(|| {
        format!(
            "lamella deploy: name the board to erase.\n\n\
             \x20   lamella deploy {ERASE_FLAG} --target <t>\n\n\
             `lamella devices` lists what is attached and prints the --target for each one."
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
usage: lamella deploy <file.cs|file.csproj> --target <t>      into firmware on the board
                               [--no-run | --follow]
       lamella deploy --erase --target <t>                    stop and erase its stored program
       lamella deploy <file.cs|file.csproj> --board <id> [--via probe|volume]   onto the bare chip
                               [--probe <serial>] [--volume <name>] [--device <serial>]
                               [--nostdlib]

--target is a live connection (what `lamella devices` prints); --board is a board model (what
`lamella boards` lists). The first keeps the board's firmware and takes about a second; the second
replaces everything on the chip and needs nothing there first.

--erase stops the program the board is running and erases the one it stores, so at each reset the
board starts no program and waits for a deploy. Deploying a program replaces the stored one.

--via chooses how the chip is written, on a board offering both routes. `volume` copies to the
bootloader drive and needs no probe; `probe` writes over an attached SWD probe and reads every
byte back to check it. The default is whatever the board takes without extra hardware. With more
than one probe attached, `--via probe` refuses until you name one with --probe <serial>.

--probe, --volume and --device name which probe, which drive and which USB DFU bootloader when
several are attached, each on its own route, as `lamella flash` takes them.

A .csproj builds every .cs beside it as ONE program and links the assemblies its <Reference>
elements name, each by a <HintPath>. Nothing is available to a build that its project does not
name. A single .cs file names no references and binds against the class library alone.

--target sends an image baked with the corlib the program was compiled against, so the program may
call into System.* as it does under `lamella run`. A call into another library is refused by name,
because the image carries no library but the corlib.

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
    send_image(&image, target, after)
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
fn to_running_firmware(path: &Path, target: &str, after: AfterDeploy) -> ExitCode {
    let image = match crate::bake::image_for_firmware(path, "deploy", deploy_refusal) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    println!("built {} B from {}", image.len(), path.display());
    send_image(&image, target, after)
}

/// Open `target` and complete a HELLO, or say why not and answer `None`.
///
/// **ONE OPENING FOR EVERY ROUTE OF THIS VERB THAT SPEAKS TO FIRMWARE**, so a deploy and an erase
/// cannot come to describe the same unanswering board in two different ways.
fn connect(target: &str) -> Option<(AnyTransport, lamella_wire::Negotiated)> {
    let mut transport = open(target)?;
    match hello_on(&mut transport, 0, target) {
        Ok(session) => Some((transport, session)),
        Err(refusal) => {
            eprintln!("{refusal}");
            None
        }
    }
}

/// Open `target`, or say why not and answer `None`.
fn open(target: &str) -> Option<AnyTransport> {
    match open_target(target, BAUD, TIMEOUT) {
        Ok(transport) => Some(transport),
        Err(error) => {
            eprintln!("lamella deploy: cannot open {target}: {error:?}");
            eprintln!(
                "\nthis build can open: {}.\n\
                 `lamella devices` lists what is attached and what to write here.",
                lamella_wire_host::available_carriers().join(", ")
            );
            None
        }
    }
}

/// Complete a HELLO with the firmware at the other end of `transport`, or say why not.
///
/// # Errors
/// The board refused the session, speaks another protocol version, or did not answer -- each in
/// its own words.
fn hello_on(
    transport: &mut impl lamella_wire::Transport,
    seq: u16,
    target: &str,
) -> Result<lamella_wire::Negotiated, String> {
    hello_blocking(transport, seq, host_caps(), TIMEOUT).map_err(|error| match error {
        lamella_wire::TransportError::Refused { reason, .. } => {
            format!("lamella deploy: {target} refused the connection: {}", refusal(reason))
        }
        lamella_wire::TransportError::VersionMismatch { target_min, target_max } => format!(
            "lamella deploy: cannot talk to {target}: {}",
            lamella_wire_host::version_mismatch(lamella_wire::PROTOCOL_VERSION, target_min, target_max)
        ),
        error => format!(
            "lamella deploy: {target} did not answer a HELLO ({error:?}).\n{}",
            no_answer()
        ),
    })
}

/// Put `image` on the firmware running at `target`, then do what `after` says.
///
/// **ONE SENDER FOR A COMPILED IMAGE AND A PREBUILT ONE**, so the wire behavior, the timeouts and
/// every message a reader sees are the same either way. The firmware cannot tell where the bytes
/// came from and neither should the output.
fn send_image(image: &[u8], target: &str, after: AfterDeploy) -> ExitCode {
    let Some((mut transport, session)) = connect(target) else {
        return ExitCode::FAILURE;
    };
    match deploy_image_blocking(&mut transport, 1, image, CHUNK, TIMEOUT, Capabilities(0)) {
        Ok(TransferAck::Accepted) => {}
        Ok(not_accepted) => {
            eprintln!("lamella deploy: {}.", not_accepted.describe(board_name(session.identity.product_model)));
            return ExitCode::FAILURE;
        }
        Err(lamella_wire::TransportError::Refused { reason, .. }) => {
            eprintln!("lamella deploy: {target} refused the deploy: {}", refusal(reason));
            return ExitCode::FAILURE;
        }
        Err(error) => {
            eprintln!("lamella deploy: deploy to {target} failed: {error:?}");
            return ExitCode::FAILURE;
        }
    }
    println!("deployed {} B to {target}", image.len());

    if after == AfterDeploy::Store {
        println!("not started (--no-run). It runs at the board's next reset.");
        return ExitCode::SUCCESS;
    }
    match start_deployed(&mut transport, START_ACK_PATIENCE) {
        Ok(line) => println!("{line}"),
        Err(why) => {
            eprintln!("lamella deploy: {why}");
            return ExitCode::FAILURE;
        }
    }
    if after == AfterDeploy::Follow {
        return follow(
            transport,
            target,
            &mut || open_target(target, BAUD, TIMEOUT).ok(),
            REATTACH_PATIENCE,
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        );
    }
    ExitCode::SUCCESS
}

/// How long to wait for a board to come back after it resets: to start the program it was sent,
/// or on its own.
///
/// A board's own USB leaves the bus at the reset and returns once the firmware has enumerated it
/// again; a board on a network has to rejoin the network first.
const REATTACH_PATIENCE: Duration = Duration::from_secs(20);

/// Print what the program the board just started prints, until it ends.
///
/// **IT LISTENS AND NEVER SPEAKS.** A HELLO would take the board back from the program, and any
/// other request is a claim on the board's session: the board would then send the program's output
/// to this tool alone and wait for it to be read, so a program whose watcher went away would stall
/// behind output nobody takes. Listening leaves the board as it would be with nothing attached, and
/// stopping this tool changes nothing on the board.
///
/// **THE CARRIER IS REOPENED WHEN THE BOARD RESETS.** The start this follows is a reset, and on the
/// board's own USB a reset takes the device off the bus, so the handle the image was sent through
/// is gone before the program it started has run. Each time it goes, `reopen` is tried until
/// `patience` runs out, and how long the board took to come back is said, because what the program
/// printed in that time was not seen.
fn follow<T: lamella_wire::Transport>(
    mut transport: T,
    target: &str,
    reopen: &mut dyn FnMut() -> Option<T>,
    patience: Duration,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> ExitCode {
    use lamella_wire_host::RunCollector;
    use lamella_wire_host::terminal::TerminalOutput;
    let _ = writeln!(
        out,
        "following what it prints. Stopping this tool leaves the program running on the board.\n"
    );
    let mut run = RunCollector::unprompted();
    let mut terminal = TerminalOutput::new(out, err);
    loop {
        let polled = run.poll_streaming(&mut transport, &mut |chunk| {
            let _ = terminal.show(chunk, None);
        });
        match polled {
            Ok(true) => {
                let _ = terminal.end_open_line();
                let (out, err) = terminal.into_writers();
                return report_end(&run, out, err);
            }
            Ok(false) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => {
                let _ = terminal.end_open_line();
                let (out, err) = terminal.into_writers();
                let lost = Instant::now();
                let Some(fresh) = reattach(reopen, patience) else {
                    let _ = writeln!(
                        err,
                        "\nlamella deploy: {target} did not come back within {} s of its \
                         connection dropping, so this tool stopped\nfollowing. The program is \
                         stored on the board and starts at every reset; `lamella devices` lists\n\
                         what is attached.",
                        patience.as_secs()
                    );
                    return ExitCode::FAILURE;
                };
                transport = fresh;
                let _ = writeln!(
                    err,
                    "(the board reset and its connection dropped; reattached {:.1} s later, and \
                     what the program printed\n in between is not shown)",
                    lost.elapsed().as_secs_f64()
                );
                terminal = TerminalOutput::new(out, err);
            }
        }
    }
}

/// Open the carrier again within `patience`, trying every 200 ms.
fn reattach<T>(reopen: &mut dyn FnMut() -> Option<T>, patience: Duration) -> Option<T> {
    let deadline = Instant::now() + patience;
    loop {
        if let Some(transport) = reopen() {
            return Some(transport);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The line a followed program's end gets, and the exit code it gives this tool.
///
/// **THE SAME SENTENCES `lamella run --target` ENDS WITH**, so a program reads the same whichever
/// of the two watched it. The program's exit code is reported rather than forwarded: a nonzero
/// code would be indistinguishable from the tool failing.
fn report_end(
    run: &lamella_wire_host::RunCollector,
    out: &mut dyn std::io::Write,
    err: &mut dyn std::io::Write,
) -> ExitCode {
    use lamella_wire_host::debug::reason;
    match (run.stop_reason(), run.exit_value()) {
        (Some(reason::DONE), Some(code)) => {
            let _ = writeln!(out, "\nthe program ended, exit code {code}.");
            if code == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
        }
        (Some(reason::TRAP), _) => {
            let _ = writeln!(err, "\nthe program stopped: it trapped, as reported above.");
            ExitCode::FAILURE
        }
        (Some(reason::ABORTED), _) => {
            let _ = writeln!(
                err,
                "\nthe program stopped: something else connected to the board and aborted it."
            );
            ExitCode::FAILURE
        }
        (Some(other), _) => {
            let _ = writeln!(
                err,
                "\nthe program stopped, giving reason {other}, which this build has no word for."
            );
            ExitCode::FAILURE
        }
        (None, _) => {
            let _ = writeln!(err, "\nthe program stopped without saying why.");
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
    let Some(mut transport) = open(target) else {
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
    let session = match hello_on(transport, 2, target) {
        Ok(session) => session,
        Err(refusal) => return erased.refused(refusal),
    };
    let name = board_name(session.identity.product_model);
    let board = the_board(name);
    let aborted = match abort_blocking(transport, 3, TIMEOUT) {
        Ok(aborted) => aborted,
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
                .refused(format!("lamella deploy: {board} refused the erase: {}", refusal(reason)));
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

/// "the" and the board's product name, or "the board" when its HELLO named none.
fn the_board(name: Option<&str>) -> String {
    name.map_or_else(|| "the board".to_owned(), |name| format!("the {name}"))
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

/// `text` with its first letter capitalized, for a board's name at the start of a sentence.
fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| first.to_uppercase().chain(chars).collect())
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
fn to_running_firmware(_path: &Path, _target: &str, _after: AfterDeploy) -> ExitCode {
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

    /// The options `deploy` sees for `words`.
    fn parsed(words: &[&str]) -> args::Options {
        let words: Vec<String> = words.iter().map(|word| (*word).to_owned()).collect();
        args::parse(&words, &SPEC).expect("the words parse")
    }

    fn code(exit: ExitCode) -> String {
        format!("{exit:?}")
    }

    /// **STORING AND FOLLOWING ARE OPPOSITES, REFUSED TOGETHER**, and each alone means what it says.
    #[test]
    fn storing_and_following_are_refused_together_and_each_alone_is_honored() {
        let both = after_deploy(&parsed(&["App.cs", "--target", "usb", "--no-run", FOLLOW_FLAG]))
            .expect_err("one stores without starting, the other watches what it starts");
        assert!(both.contains("give one"), "{both}");
        let store = after_deploy(&parsed(&["App.cs", "--target", "usb", "--no-run"]));
        assert_eq!(store, Ok(AfterDeploy::Store));
        let follow = after_deploy(&parsed(&["App.cs", "--target", "usb", FOLLOW_FLAG]));
        assert_eq!(follow, Ok(AfterDeploy::Follow));
        assert_eq!(after_deploy(&parsed(&["App.cs", "--target", "usb"])), Ok(AfterDeploy::Start));
    }

    /// **AN ERASE TAKES A CONNECTION AND NOTHING ELSE**, and each refusal names what to type.
    #[test]
    fn an_erase_takes_a_target_and_nothing_else() {
        assert_eq!(erase_target(&parsed(&[ERASE_FLAG, "--target", "usb"])), Ok("usb"));
        let program = erase_target(&parsed(&[ERASE_FLAG, "--target", "usb", "App.cs"]))
            .expect_err("an erase sends nothing");
        assert!(program.contains("takes no program"), "{program}");
        assert!(program.contains("lamella deploy App.cs --target <t>"), "and how to replace it: {program}");
        let board = erase_target(&parsed(&[ERASE_FLAG, "--board", "rpi-pico2"]))
            .expect_err("a board model has no firmware to ask");
        assert!(board.contains("--target"), "{board}");
        let extra = erase_target(&parsed(&[ERASE_FLAG, "--target", "usb", "--no-run", FOLLOW_FLAG]))
            .expect_err("options an erase has no use for are refused, not ignored");
        assert!(extra.contains("--no-run and --follow would have no effect"), "names both: {extra}");
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

    /// A RAM flash region as a board's flash sink presents one: an image is read where it was
    /// written.
    #[cfg(feature = "bake")]
    struct RamFlash {
        region: &'static [u8],
        erases: usize,
    }

    #[cfg(feature = "bake")]
    impl RamFlash {
        fn holding(image: &[u8]) -> Self {
            let mut region = vec![0xFF; 64 * 1024];
            region[..image.len()].copy_from_slice(image);
            Self { region: Box::leak(region.into_boxed_slice()), erases: 0 }
        }

        fn rewrite(&mut self, edit: impl FnOnce(&mut [u8])) {
            let mut region = self.region.to_vec();
            edit(&mut region);
            self.region = Box::leak(region.into_boxed_slice());
        }
    }

    #[cfg(feature = "bake")]
    impl lamella_runner::FlashSink for RamFlash {
        fn image_slice(&self) -> &'static [u8] {
            self.region
        }
        fn erase(&mut self) {
            self.erases += 1;
            self.rewrite(|region| region[..4096].fill(0xFF));
        }
        fn program(&mut self, image: &[u8]) -> bool {
            let fits = image.len() <= self.region.len();
            if fits {
                self.rewrite(|region| {
                    region.fill(0xFF);
                    region[..image.len()].copy_from_slice(image);
                });
            }
            fits
        }
        fn program_chunk(&mut self, offset: usize, chunk: &[u8], total: usize) -> bool {
            let fits = total <= self.region.len() && offset + chunk.len() <= self.region.len();
            if fits {
                self.rewrite(|region| {
                    if offset == 0 {
                        region.fill(0xFF);
                    }
                    region[offset..offset + chunk.len()].copy_from_slice(chunk);
                });
            }
            fits
        }
    }

    /// A board at the other end of the wire, in this process: every frame the host sends is served
    /// by the deploy serve a board's firmware runs, over `flash`, and its replies wait to be read.
    #[cfg(feature = "bake")]
    struct LoopbackBoard {
        replies: lamella_wire::MemTransport,
        carrier: lamella_wire::MemTransport,
        flash: RamFlash,
        load: lamella_runner::ArtifactLoad,
        /// The serve asked for the reset that starts the deployed program. Once its last replies are
        /// read, the carrier is gone, as a board's own USB is when it resets.
        reset: bool,
    }

    #[cfg(feature = "bake")]
    impl lamella_wire::Transport for LoopbackBoard {
        fn send(
            &mut self,
            msg_type: u8,
            seq: u16,
            payload: &[u8],
        ) -> Result<(), lamella_wire::TransportError> {
            let mut host = lamella_wire::MemTransport::new();
            lamella_wire::Transport::send(&mut host, msg_type, seq, payload)?;
            self.carrier.feed(&host.take_sent());
            loop {
                match lamella_runner::serve_one_deploy(&mut self.carrier, &mut self.flash, &mut self.load)? {
                    lamella_runner::Served::Nothing => break,
                    lamella_runner::Served::RunRequested => self.reset = true,
                    _ => {}
                }
            }
            self.replies.feed(&self.carrier.take_sent());
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, lamella_wire::TransportError> {
            match lamella_wire::Transport::poll(&mut self.replies)? {
                None if self.reset => Err(lamella_wire::TransportError::Closed),
                polled => Ok(polled),
            }
        }
    }

    /// **AN ERASE AGAINST THE DEPLOY SERVE A BOARD'S FIRMWARE RUNS.** A board holding a baked program
    /// is asked whether one is running, taken back, asked what it stores, told to erase and asked
    /// again, and the report is what it answered: erased, and the second time nothing to erase.
    #[cfg(feature = "bake")]
    #[test]
    fn an_erase_against_the_firmware_serve_reports_what_the_board_answered() {
        let Ok(compiler) = lamella_wire_host::engine::LcscCompiler::discover() else {
            return;
        };
        let image = crate::bake::compile_and_bake(
            &compiler,
            "class Program\n{\n    static int Main()\n    {\n        return 42;\n    }\n}\n",
        )
        .expect("a program bakes");
        let mut board = LoopbackBoard {
            replies: lamella_wire::MemTransport::new(),
            carrier: lamella_wire::MemTransport::new(),
            flash: RamFlash::holding(&image),
            load: lamella_runner::ArtifactLoad::new(),
            reset: false,
        };
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

    /// A carrier with a board's frames already queued on it. It counts what the host sends, and once
    /// its frames are read it either goes quiet or drops, as a board's reset takes its own USB.
    struct Listening {
        wire: lamella_wire::MemTransport,
        sent: std::rc::Rc<std::cell::Cell<usize>>,
        drops: bool,
    }

    impl Listening {
        fn with(frames: &[(u8, Vec<u8>)], drops: bool, sent: &std::rc::Rc<std::cell::Cell<usize>>) -> Self {
            let mut board = lamella_wire::MemTransport::new();
            for (msg_type, payload) in frames {
                lamella_wire::Transport::send(&mut board, *msg_type, 0, payload).unwrap();
            }
            let mut wire = lamella_wire::MemTransport::new();
            wire.feed(&board.take_sent());
            Self { wire, sent: std::rc::Rc::clone(sent), drops }
        }
    }

    impl lamella_wire::Transport for Listening {
        fn send(&mut self, _msg_type: u8, _seq: u16, _payload: &[u8]) -> Result<(), lamella_wire::TransportError> {
            self.sent.set(self.sent.get() + 1);
            Ok(())
        }

        fn poll(&mut self) -> Result<Option<lamella_wire::Frame>, lamella_wire::TransportError> {
            match lamella_wire::Transport::poll(&mut self.wire)? {
                Some(frame) => Ok(Some(frame)),
                None if self.drops => Err(lamella_wire::TransportError::Closed),
                None => Ok(None),
            }
        }
    }

    /// An `EVT_OUTPUT` of `text` on standard output.
    fn printed(text: &str) -> (u8, Vec<u8>) {
        let mut payload = vec![lamella_wire_host::debug::output::STDOUT, 0];
        payload.extend_from_slice(text.as_bytes());
        (lamella_wire_host::debug::EVT_OUTPUT, payload)
    }

    /// The `EVT_STOPPED` a program that returned `exit` ends with.
    fn ended(exit: i32) -> (u8, Vec<u8>) {
        let mut payload = vec![lamella_wire_host::debug::reason::DONE];
        payload.extend_from_slice(&[0; 8]);
        payload.extend_from_slice(&exit.to_le_bytes());
        payload.push(0);
        (lamella_wire_host::debug::EVT_STOPPED, payload)
    }

    /// **A FOLLOWER READS TO THE PROGRAM'S END AND NEVER SPEAKS.** Anything it sent would be a claim
    /// on the board's session, which a program's output would then wait on.
    #[test]
    fn following_reads_to_the_end_and_sends_nothing() {
        let sent = std::rc::Rc::new(std::cell::Cell::new(0));
        let carrier = Listening::with(&[printed("hello\n"), ended(0)], false, &sent);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let exit = follow(carrier, "loopback", &mut || None, Duration::from_millis(50), &mut out, &mut err);
        assert_eq!(code(exit), code(ExitCode::SUCCESS), "the program returned 0");
        assert_eq!(sent.get(), 0, "a follower sends nothing");
        let out = String::from_utf8(out).expect("UTF-8");
        assert!(out.contains("hello\n"), "the program's output: {out}");
        assert!(out.ends_with("the program ended, exit code 0.\n"), "and how it ended: {out}");
    }

    /// **THE RESET THAT STARTS THE PROGRAM TAKES THE CARRIER, AND THE FOLLOWER COMES BACK FOR IT.**
    /// The program's exit code is reported rather than turned into success.
    #[test]
    fn following_reattaches_when_the_reset_takes_the_carrier() {
        let sent = std::rc::Rc::new(std::cell::Cell::new(0));
        let first = Listening::with(&[printed("before the reset\n")], true, &sent);
        let mut later = vec![Listening::with(&[printed("after\n"), ended(3)], false, &sent)];
        let mut reopened = 0;
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let exit = follow(
            first,
            "loopback",
            &mut || {
                reopened += 1;
                later.pop()
            },
            Duration::from_millis(500),
            &mut out,
            &mut err,
        );
        assert_eq!(reopened, 1, "the follower came back once");
        assert_eq!(code(exit), code(ExitCode::FAILURE), "exit 3 is not success");
        assert_eq!(sent.get(), 0, "and sent nothing on either carrier");
        let (out, err) = (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap());
        assert!(out.contains("before the reset\n") && out.contains("after\n"), "both carriers' output: {out}");
        assert!(err.contains("reattached"), "the reattach is said: {err}");
        assert!(out.ends_with("the program ended, exit code 3.\n"), "{out}");
    }

    /// A board that does not come back is given up on once the patience runs out, with a failure.
    #[test]
    fn following_gives_up_on_a_board_that_does_not_come_back() {
        let sent = std::rc::Rc::new(std::cell::Cell::new(0));
        let started = Instant::now();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let exit = follow(
            Listening::with(&[], true, &sent),
            "loopback",
            &mut || None,
            Duration::from_millis(300),
            &mut out,
            &mut err,
        );
        assert_eq!(code(exit), code(ExitCode::FAILURE));
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("did not come back within 0 s"), "{err}");
        assert!(started.elapsed() >= Duration::from_millis(300), "it waited out its patience first");
    }

    /// **A DEPLOY THAT FOLLOWS, AGAINST THE FIRMWARE'S OWN CODE.** The board takes the image through
    /// the deploy serve, acknowledges the start and asks for its reset, and the carrier goes with
    /// it; the follower reattaches and shows what the firmware's deployed-run loop streams from the
    /// image it booted, to the program's own end.
    #[cfg(feature = "bake")]
    #[test]
    fn a_deploy_that_follows_shows_what_the_board_runs_after_its_reset() {
        let Ok(compiler) = lamella_wire_host::engine::LcscCompiler::discover() else {
            return;
        };
        let image = crate::bake::compile_and_bake(
            &compiler,
            "public static class Program\n{\n    public static int Main()\n    {\n        \
             System.Console.WriteLine(\"up\");\n        System.Console.WriteLine(\"and running\");\n        \
             return 0;\n    }\n}\n",
        )
        .expect("a program bakes");
        let mut board = LoopbackBoard {
            replies: lamella_wire::MemTransport::new(),
            carrier: lamella_wire::MemTransport::new(),
            flash: RamFlash::holding(&[]),
            load: lamella_runner::ArtifactLoad::new(),
            reset: false,
        };
        hello_on(&mut board, 0, "loopback").expect("the board answers a HELLO");
        let sent = deploy_image_blocking(&mut board, 1, &image, CHUNK, TIMEOUT, Capabilities(0));
        assert_eq!(sent, Ok(TransferAck::Accepted), "the board took the image");
        assert_eq!(start_deployed(&mut board, START_ACK_PATIENCE), Ok("started it."));
        assert!(board.reset, "and asked for the reset that starts it");

        let stored: &'static [u8] = board.flash.region;
        let mut boots = 0;
        let mut reopen = || {
            boots += 1;
            let (module, entry) = lamella_cil_runtime::Module::from_baked(stored).ok()?;
            let mut carrier = lamella_wire::MemTransport::new();
            lamella_runner::run_deployed(&mut carrier, &module, entry?).ok()?;
            let mut fresh = LoopbackBoard {
                replies: lamella_wire::MemTransport::new(),
                carrier: lamella_wire::MemTransport::new(),
                flash: RamFlash::holding(&[]),
                load: lamella_runner::ArtifactLoad::new(),
                reset: false,
            };
            fresh.replies.feed(&carrier.take_sent());
            Some(fresh)
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let exit = follow(board, "loopback", &mut reopen, Duration::from_secs(2), &mut out, &mut err);
        let (out, err) = (String::from_utf8(out).unwrap(), String::from_utf8(err).unwrap());
        assert_eq!(boots, 1, "the follower reattached once: {err}");
        assert_eq!(code(exit), code(ExitCode::SUCCESS), "{out}{err}");
        assert!(out.contains("up\nand running\n"), "what the stored program printed: {out}");
        assert!(out.ends_with("the program ended, exit code 0.\n"), "{out}");
    }
}
