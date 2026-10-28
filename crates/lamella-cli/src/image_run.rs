//! `lamella run --manifest <path>`: write an image built for a board, follow the program's console
//! over the debug probe, and end with the program's own exit code.
//!
//! This is the run a device SDK's `dotnet run` asks for, not a command a person types: it is not in
//! `lamella run --help`, and its arguments may change with the SDK that passes them. The manifest is
//! the record a toolchain writes beside an image it linked, `<image>.manifest.json`, and the image is
//! the file it is named for. It says which board the image was built for and what its bytes hash
//! to, so a run needs nothing more from its caller than the probe: `--probe <serial>`, or
//! `LAMELLA_PROBE_SERIAL`, or the only probe attached.
//!
//! **THE PROGRAM IS FOLLOWED IN THE BOARD'S MEMORY AND NEVER HALTED.** The image keeps a console
//! ring in RAM, at the address its ELF gives the symbol `__lamella_console_log`, or
//! `__rhu_console_log` in an image Lamella RHU built: every byte the board's console carries, and,
//! once the program has ended, how it ended. The run reads it through the probe while the program
//! runs.
//!
//! **STANDARD OUTPUT IS THE BOARD'S CONSOLE, AND NOTHING OF THIS TOOL'S.** The ring holds the
//! program's output and its kernel's own lines as they were written, interleaved, and all of it goes
//! to standard output in that order. Everything this tool says -- what it read, the probe it used,
//! how the write went, why it stopped -- goes to standard error, every line prefixed `lamella:`.
//!
//! **A RUN ENDS AS THE PROGRAM ENDED**: with its own exit code, or with 1 and a sentence when the
//! program was aborted, when the time limit `--timeout` sets ran out, or when this tool failed.
//! Ctrl+C stops following at once and ends as the operating system ends any program stopped by
//! Ctrl+C. Neither stops the program: the board is let go of, never halted or reset.
//!
//! **EVERY REFUSAL COMES BEFORE ANYTHING IS OPENED.** The manifest, the image's identity, the
//! board's route and the console's address are settled from files alone, so a run that could not
//! be followed writes nothing.

use crate::args;
use crate::devices::ProbeFound;
use crate::ending::Ended;
use crate::own_lines::say;
use lamella_elf::symbols::DataObject;
use lamella_flash_routes::manifest::{self, Manifest};
use lamella_flash_routes::placement::{BootloaderChoice, Placement, placement_for};
use lamella_flash_routes::{Programmer, Programming, prepare_image, programmer_for};
use lamella_probe_console::{END_MAGICS, LAMELLA_RING, NoRing, ProgramEnd, RHU_RING, RingReader, Seen, TargetMemory};
use lamella_probe_core::selection::serial_named;
use std::borrow::Cow;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the follow loop waits after a look that found nothing, before it looks again.
const IDLE: Duration = Duration::from_millis(20);

/// How many looks a console ring that is not set up yet is waited through: about ten seconds of
/// them, for a board that is still starting.
const PATIENCE: u32 = 500;

/// How long after the write the console ring is first looked at.
///
/// The write ends by resetting the board, and until the new start has set the ring up again it
/// still holds the last run's words -- how that run ended, or its output if it never did. The
/// board's kernel does that within 50 ms of starting, after whatever boot ROM runs before it, so
/// half a second is ten times the bound with room for the ROM, and nothing the last run left is
/// read as this one's.
const SETTLE: Duration = Duration::from_millis(500);

/// The option that asks for this run, and names the image's manifest.
pub(crate) const MANIFEST: &str = "--manifest";

/// The file name a manifest ends with, after its image's own.
const MANIFEST_SUFFIX: &str = ".manifest.json";

/// `lamella run --manifest <path> [--probe <serial>] [--timeout <seconds>]`, from the words before
/// any `--` (`options`) and the words after it (`after`).
pub(crate) fn run_manifest(options: &[String], after: &[String]) -> Ended {
    let spec = args::Spec {
        verb: "run",
        usage: Some(crate::program::RUN_USAGE),
        values: &[MANIFEST, "--probe", "--timeout"],
        repeats: &[],
        flags: &[],
        positional: args::Positional::Own,
    };
    let parsed = match args::parse_or_halt(options, &spec) {
        Ok(parsed) => parsed,
        Err(args::Halt::Help) => return Ended::Program(0),
        Err(args::Halt::Bad) => return Ended::Failed,
    };
    let mut err = std::io::stderr();
    let given: Vec<&String> = parsed.positionals().iter().chain(after).collect();
    if !given.is_empty() {
        say(&mut err, &arguments_refusal(&given));
        return Ended::Failed;
    }
    let manifest = parsed.value(MANIFEST).unwrap_or_default();
    let image = match image_of(Path::new(manifest)) {
        Ok(image) => image,
        Err(refusal) => {
            say(&mut err, &refusal);
            return Ended::Failed;
        }
    };
    run_image(&image, parsed.value("--probe"), parsed.value("--timeout"))
}

/// The refusal for a program given arguments, which a program on a board cannot take yet.
pub(crate) fn arguments_refusal(given: &[&String]) -> String {
    let listed: Vec<String> = given.iter().map(|argument| format!("{argument:?}")).collect();
    format!(
        "a program on a board cannot take arguments yet, so nothing was written (given: {}).",
        listed.join(", ")
    )
}

/// The image a manifest describes: the file its name is the manifest's without `.manifest.json`.
///
/// # Errors
/// A path whose file name is not an image's name followed by `.manifest.json`.
fn image_of(manifest: &Path) -> Result<PathBuf, String> {
    let name = manifest.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    match name.strip_suffix(MANIFEST_SUFFIX) {
        Some(image) if !image.is_empty() => Ok(manifest.with_file_name(image)),
        _ => Err(format!(
            "{} is not an image's manifest: a manifest is named for its image, with {MANIFEST_SUFFIX} \
             after the\nimage's own name, and the image is found beside it.",
            manifest.display()
        )),
    }
}

/// The run of `image`, whose manifest is beside it, through the probe `probe` names.
fn run_image(image: &Path, probe: Option<&str>, time_limit: Option<&str>) -> Ended {
    let mut err = std::io::stderr();
    let plan = match plan(image, time_limit) {
        Ok(plan) => plan,
        Err(refusal) => {
            say(&mut err, &refusal);
            return Ended::Failed;
        }
    };
    let environment = std::env::var("LAMELLA_PROBE_SERIAL").ok();
    let chosen = match choose_probe(
        probe,
        environment.as_deref(),
        plan.board,
        plan.route,
        &crate::devices::attached_probes(),
    ) {
        Ok(chosen) => chosen,
        Err(refusal) => {
            say(&mut err, &refusal);
            return Ended::Failed;
        }
    };
    run_planned(
        &plan,
        &chosen,
        &mut FlashWrite,
        &mut BoardConsole,
        &mut std::io::stdout(),
        &mut err,
        &SystemClock(Instant::now()),
        &mut crate::ending::watch_for_ctrl_c,
        &crate::ending::ctrl_c_pressed,
    )
}

/// How this build reads a running board's memory, by the probe its route writes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// A CMSIS-DAP probe wired to an RP2350, through core 0's memory access port.
    Rp2350,
    /// The ST-LINK on the board, by its USB product id, through the probe's own memory commands.
    StLink(u16),
}

impl Reach {
    /// How the memory of `board`, written through `route`, is read while its program runs.
    ///
    /// # Errors
    /// A route this build cannot follow a running program through.
    fn of(route: Programmer, board: &str) -> Result<Reach, String> {
        match route {
            Programmer::Rp2350Probe { .. } => Ok(Reach::Rp2350),
            Programmer::StlinkOnboard { probe_id, .. } => Ok(Reach::StLink(probe_id)),
            other => Err(format!(
                "{board} is written through {}, and this build follows a running program only \
                 through a debug probe\non an RP2350 or the ST-LINK on an ST board, so nothing was \
                 written.",
                other.description()
            )),
        }
    }

    /// The family of the debug probe this reach goes through, as `lamella devices` names a probe's
    /// family.
    fn family(self) -> &'static str {
        match self {
            Reach::Rp2350 => crate::devices::CMSIS_DAP,
            Reach::StLink(_) => crate::devices::ST_LINK,
        }
    }
}

/// The families of debug probe a run by an image's manifest writes `board` through and follows its
/// program through, by the route [`plan`] takes: none for a board this build cannot run that way.
pub(crate) fn probe_families(board: &str) -> Vec<&'static str> {
    programmer_for(board)
        .ok()
        .and_then(|row| probe_route(row).ok())
        .and_then(|route| Reach::of(route, board).ok())
        .map(|reach| vec![reach.family()])
        .unwrap_or_default()
}

/// A run of an image, settled from files alone: what is written where, and what is followed.
pub(crate) struct ImagePlan {
    /// The image, as the caller named it.
    image: PathBuf,
    /// What the manifest attests, in the words a person reads before a write.
    attestation: String,
    /// The board the image was built for.
    board: &'static str,
    /// The route that writes the board, which is always one over a debug probe.
    route: Programmer,
    /// How the board's memory is read while the program runs.
    reach: Reach,
    /// Where the route puts the image.
    placement: Placement,
    /// The bytes the route takes.
    bytes: Vec<u8>,
    /// The image's half for the board's external flash, written with `bytes` as one set.
    window: Option<lamella_flash_routes::WindowHalf>,
    /// What the file held: `998096 B of ELF`.
    read: String,
    /// Where the image keeps its console ring, and how many bytes it gives it.
    ring: DataObject,
    /// Whether the image was built with its console off, so that nothing the program prints is
    /// kept.
    console_off: bool,
    /// The time limit the caller set, if it set one.
    limit: Option<Duration>,
}

/// Settles the run of `image`, reading nothing but files.
///
/// `time_limit` is `--timeout`'s value.
///
/// # Errors
/// A bad time limit, a missing or unreadable manifest, an image its manifest does not describe, a
/// board with no route this build can follow a program through, and an image with no console ring
/// to follow; each worded for a reader.
fn plan(image: &Path, time_limit: Option<&str>) -> Result<ImagePlan, String> {
    let limit = time_limit.map(parse_time_limit).transpose()?;
    let manifest = match manifest::read(image) {
        Ok(Some(manifest)) => manifest,
        Ok(None) => {
            return Err(format!(
                "there is no manifest at {}, and a run takes the board the image was built for \
                 from it.",
                manifest::path_for(image).display()
            ));
        }
        Err(why) => {
            return Err(format!(
                "{why}\n\nA manifest that is present and cannot be read is a claim about the image \
                 that nobody can\ncheck, so nothing was written."
            ));
        }
    };
    if manifest.model == manifest::INTERPRETER {
        return Err(format!(
            "{} is an image Lamella's firmware runs (its manifest's model is {}), and a run by its manifest \
             writes a native image over a probe.\ndotnet run --device <device> runs the program on a board running \
             the firmware.",
            image.display(),
            manifest::INTERPRETER
        ));
    }
    let shipped = std::fs::read(image).map_err(|error| {
        format!(
            "read {}: {error}\nThe image a manifest describes is the file beside it, named as the \
             manifest is without {MANIFEST_SUFFIX}.",
            image.display()
        )
    })?;
    let extension = lamella_flash_routes::artifact::classify_format(image);
    manifest::check_identity(&manifest, &shipped, extension.as_deref())?;
    let row = programmer_for(&manifest.board).map_err(|error| unprefixed(&error))?;
    let route = probe_route(row)?;
    let reach = Reach::of(route, row.board)?;
    let ring = ring_of(&manifest, image, &shipped)?;
    let placement = placement_for(row, route, BootloaderChoice::Keep)?;
    if let Some(base) = manifest.base
        && base != placement.base()
    {
        return Err(format!(
            "the manifest says {} belongs at {base:#010x}, and {} is written from {:#010x}.\nIt was \
             built for another layout, so nothing was written.",
            image.display(),
            row.board,
            placement.base()
        ));
    }
    let prepared = prepare_image(image, row, route, &placement)?;
    Ok(ImagePlan {
        image: image.to_path_buf(),
        attestation: manifest::attestation(&manifest),
        board: row.board,
        route,
        reach,
        placement,
        bytes: prepared.bytes,
        window: prepared.window,
        read: prepared.read,
        ring,
        console_off: manifest.console_channel.as_deref() == Some("off"),
        limit,
    })
}

/// Where the image keeps its console ring: the address and size its ELF gives the ring's symbol.
///
/// The ELF is the image itself when the image is one, and otherwise the file the manifest names
/// beside it.
///
/// # Errors
/// A manifest that names no ELF for a flat image, an ELF that cannot be read, and one that names
/// no console ring or one too small to hold its header.
fn ring_of(manifest: &Manifest, image: &Path, shipped: &[u8]) -> Result<DataObject, String> {
    let (elf, bytes): (PathBuf, Cow<'_, [u8]>) = if manifest.format == "elf" {
        (image.to_path_buf(), Cow::Borrowed(shipped))
    } else {
        let Some(name) = manifest.elf.as_deref() else {
            return Err(format!(
                "the manifest names no ELF beside {}, and a run reads the address of the program's \
                 console from\nthe image's ELF, so nothing was written.",
                image.display()
            ));
        };
        let path = image.with_file_name(name);
        let bytes = std::fs::read(&path).map_err(|error| {
            format!("read {}, the ELF the manifest names beside the image: {error}", path.display())
        })?;
        (path, Cow::Owned(bytes))
    };
    match lamella_probe_console::ring_in(&bytes) {
        Ok((_, ring)) => Ok(ring),
        Err(NoRing::Neither) => Err(format!(
            "{} names neither {LAMELLA_RING} nor {RHU_RING}, the console ring a run reads the \
             program's\noutput and its end from, so nothing was written.",
            elf.display()
        )),
        Err(NoRing::Both) => Err(format!(
            "{} names both {LAMELLA_RING} and {RHU_RING}, so which console ring its program \
             writes\ncannot be told from it, and nothing was written.",
            elf.display()
        )),
        Err(NoRing::TooSmall { symbol, size }) => Err(format!(
            "{symbol} in {} is {size} bytes, too few for a console ring's header, so nothing was \
             written.",
            elf.display()
        )),
    }
}

/// A refusal from the flash routes, without the verb they put in front of it.
fn unprefixed(error: &str) -> String {
    error.strip_prefix("lamella flash: ").unwrap_or(error).trim_end().to_owned()
}

/// The longest time limit taken, in seconds: a year.
const LONGEST_LIMIT: f64 = 366.0 * 24.0 * 60.0 * 60.0;

/// The time limit `--timeout` gives: a number of seconds, whole or with a fraction.
///
/// # Errors
/// Anything that is not a positive number of seconds up to a year.
fn parse_time_limit(text: &str) -> Result<Duration, String> {
    let refusal = || {
        format!("--timeout takes a number of seconds above 0, such as 30 or 2.5, and got {text:?}")
    };
    let seconds: f64 = text.trim().parse().map_err(|_| refusal())?;
    if !(seconds > 0.0 && seconds <= LONGEST_LIMIT) {
        return Err(refusal());
    }
    Duration::try_from_secs_f64(seconds).map_err(|_| refusal())
}

/// The route that writes `row`'s board over a debug probe: its own, when that is a probe, or the
/// probe route it offers beside a bootloader drive.
///
/// A run reads the program's console and its end through the probe, so a route that needs none is
/// no use to it, however well it writes.
///
/// # Errors
/// A board with no route over a probe in this build.
fn probe_route(row: &Programming) -> Result<Programmer, String> {
    if row.programmer.writes_over_a_probe() {
        return Ok(row.programmer);
    }
    if let Some(alternate) = row.alternate.filter(|route| route.writes_over_a_probe()) {
        return Ok(alternate);
    }
    Err(format!(
        "{} is written by {}, and this build has no route to it over a debug probe.\nA run follows \
         the program through a probe, so nothing was written.",
        row.board,
        row.programmer.description()
    ))
}

/// The probe a run goes through, and the serial number it is named by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Chosen {
    /// The probe's USB serial number, as the probe reports it.
    serial: String,
    /// The probe, as the listing found it.
    probe: ProbeFound,
}

/// The probe a run was told to use, or the only one it could mean.
///
/// `named` is `--probe`'s value and `environment` is `LAMELLA_PROBE_SERIAL`'s, in that order, each a
/// probe's USB serial number or its id as a device list shows it, `probe:<serial>`. A
/// named serial must be one of `attached`, since a caller such as `dotnet run --device` passes on
/// whatever it was given without checking it. With none named, the one probe attached is taken --
/// on a route through a debugger soldered to the board, the one of that kind -- and with several,
/// nothing is guessed.
///
/// # Errors
/// An empty name, a serial no attached probe has, a probe of the wrong kind for the board's route,
/// no probe attached, and several with none named.
fn choose_probe(
    named: Option<&str>,
    environment: Option<&str>,
    board: &str,
    route: Programmer,
    attached: &[ProbeFound],
) -> Result<Chosen, String> {
    let wanted = match named.map(serial_named) {
        Some("") => {
            return Err("--probe was given no serial number. A probe is named by its USB serial \
                        number, as `lamella devices`\nlists it; a caller that passes an empty one \
                        chose no device."
                .to_owned());
        }
        Some(serial) => Some(serial),
        None => environment.map(serial_named).filter(|serial| !serial.is_empty()),
    };
    let Some(serial) = wanted else {
        let candidates: Vec<&ProbeFound> = attached.iter().filter(|probe| fits(route, probe)).collect();
        return match candidates.as_slice() {
            [] => Err(format!(
                "no debug probe that can write {board} is attached. A run writes the image and reads \
                 the program's\nconsole through one; {board} is written through {}.",
                route.description()
            )),
            [only] => match &only.serial {
                Some(serial) => Ok(Chosen { serial: serial.clone(), probe: (*only).clone() }),
                None => Err(format!(
                    "the one probe attached, {}, reports no USB serial number, so nothing can name \
                     it, and a write\nto a board nobody named is never made.",
                    only.description
                )),
            },
            several => Err(format!(
                "{} probes that can write {board} are attached and none was named:\n{}\n\nName one \
                 with --probe <serial>, or set LAMELLA_PROBE_SERIAL. A write to the wrong board \
                 succeeds\nand reports nothing, so it is never guessed at.",
                several.len(),
                listing(several)
            )),
        };
    };
    let probe = named_probe(serial, board, route, attached)?;
    Ok(Chosen { serial: probe.serial.clone().unwrap_or_default(), probe: probe.clone() })
}

/// The attached probe whose USB serial number is `serial`, in either case, which must be one that
/// can write `board` through `route`; or why not. A run takes a probe by it, and so does a write
/// that names one (`crate::flash`).
///
/// # Errors
/// A serial no attached probe has, naming it beside what is attached; and a probe of another kind
/// than the one `route` writes through.
pub(crate) fn named_probe<'a>(
    serial: &str,
    board: &str,
    route: Programmer,
    attached: &'a [ProbeFound],
) -> Result<&'a ProbeFound, String> {
    let found = attached
        .iter()
        .find(|probe| probe.serial.as_deref().is_some_and(|own| own.eq_ignore_ascii_case(serial)));
    let Some(probe) = found else {
        let all: Vec<&ProbeFound> = attached.iter().collect();
        let attached_now = if all.is_empty() {
            "No debug probe is attached.".to_owned()
        } else {
            format!("Attached:\n{}", listing(&all))
        };
        return Err(format!(
            "no attached probe has the serial number {serial}.\n{attached_now}\n\n`lamella devices` \
             lists what is attached."
        ));
    };
    if !fits(route, probe) {
        return Err(format!(
            "{serial} is {}, and {board} is written through {}.",
            probe.description,
            route.description()
        ));
    }
    Ok(probe)
}

/// Whether `probe` is of the kind `route` writes through.
fn fits(route: Programmer, probe: &ProbeFound) -> bool {
    route.usb_identity().is_none_or(|identity| probe.usb == identity)
}

/// Probes as a list a person reads: each serial number, then what the probe is.
fn listing(probes: &[&ProbeFound]) -> String {
    probes
        .iter()
        .map(|probe| {
            let serial = probe.serial.as_deref().unwrap_or("(no serial number)");
            format!("    {serial}  {}", probe.description)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Reading what a program on a board prints, and learning that it ended.
pub(crate) trait Console {
    /// What arrived since the last look; empty when nothing did. It waits a moment at most.
    ///
    /// # Errors
    /// A console that can no longer be read.
    fn poll(&mut self) -> Result<Vec<Seen>, String>;
}

/// Writing the image.
pub(crate) trait Writer {
    /// Writes `plan`'s image through the probe `serial` names, saying each step to `say`.
    ///
    /// # Errors
    /// Anything that stopped the write, worded for a reader.
    fn write(&mut self, plan: &ImagePlan, serial: &str, say: &mut dyn FnMut(&str))
    -> Result<(), String>;
}

/// Attaching to the program's console, once the image is written and the program runs.
pub(crate) trait Attach {
    /// The reader that follows the program, on a probe session of its own: one probe takes one
    /// session at a time, and the write's has closed.
    ///
    /// # Errors
    /// A console that cannot be reached.
    fn attach(&mut self, plan: &ImagePlan, probe: &Chosen) -> Result<Box<dyn Console>, String>;
}

/// Time, as the follow loop keeps it: the system's, or a test's.
pub(crate) trait Clock {
    /// Time since some fixed moment.
    fn now(&self) -> Duration;
    /// Waits a moment before the next look.
    fn pause(&self);
}

/// The write `lamella flash` makes, through the same function.
struct FlashWrite;

impl Writer for FlashWrite {
    fn write(
        &mut self,
        plan: &ImagePlan,
        serial: &str,
        say: &mut dyn FnMut(&str),
    ) -> Result<(), String> {
        let window = plan.window.as_ref().map(|half| (plan.board, half));
        crate::flash::write_image(plan.route, plan.board, &plan.placement, &plan.bytes, window, Some(serial), say)
    }
}

/// The console ring in the board's memory, read through the probe the image was written through.
struct BoardConsole;

impl Attach for BoardConsole {
    fn attach(&mut self, plan: &ImagePlan, probe: &Chosen) -> Result<Box<dyn Console>, String> {
        let memory = live::open(plan.reach, &probe.serial)?;
        Ok(Box::new(RingConsole { memory, reader: RingReader::new(plan.ring, END_MAGICS, PATIENCE) }))
    }
}

/// A program's console ring, followed through the probe session that holds its memory.
struct RingConsole {
    /// The board's memory, through the probe.
    memory: Box<dyn TargetMemory>,
    /// What has been followed of the ring so far.
    reader: RingReader,
}

impl Console for RingConsole {
    fn poll(&mut self) -> Result<Vec<Seen>, String> {
        self.reader.poll(self.memory.as_mut())
    }
}

/// The system's clock.
struct SystemClock(Instant);

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }

    fn pause(&self) {
        std::thread::sleep(IDLE);
    }
}

/// A planned run, with the probe chosen: write, attach, follow.
///
/// `watch` arranges for Ctrl+C to be noticed rather than obeyed, once the program runs, and `stop`
/// says whether it has been pressed.
#[allow(clippy::too_many_arguments)]
fn run_planned(
    plan: &ImagePlan,
    chosen: &Chosen,
    writer: &mut dyn Writer,
    attach: &mut dyn Attach,
    out: &mut dyn Write,
    err: &mut dyn Write,
    clock: &dyn Clock,
    watch: &mut dyn FnMut() -> bool,
    stop: &dyn Fn() -> bool,
) -> Ended {
    say(
        err,
        &crate::own_lines::running_through_probe(&plan.board, &chosen.serial),
    );
    say(err, &plan.attestation);
    say(err, &format!("read {} from {}", plan.read, plan.image.display()));
    say(err, &format!("probe {}: {}", chosen.serial, chosen.probe.description));
    if plan.console_off {
        say(
            err,
            "the image was built with its console off, so nothing the program prints is kept; \
             this run says how\nit ends.",
        );
    }
    if let Err(why) = writer.write(plan, &chosen.serial, &mut |line| say(&mut *err, line)) {
        say(err, &why);
        return Ended::Failed;
    }
    let first_look = clock.now() + SETTLE;
    let mut console = match attach.attach(plan, chosen) {
        Ok(console) => console,
        Err(why) => {
            say(err, &why);
            return Ended::Failed;
        }
    };
    let _ = watch();
    follow(console.as_mut(), out, err, plan.limit, clock, stop, first_look)
}

/// Follows a program to its end: the board's console to `out` as it arrives, and the program's end
/// as this run's. The console is not looked at before `first_look`.
fn follow(
    console: &mut dyn Console,
    out: &mut dyn Write,
    err: &mut dyn Write,
    limit: Option<Duration>,
    clock: &dyn Clock,
    stop: &dyn Fn() -> bool,
    first_look: Duration,
) -> Ended {
    let started = clock.now();
    loop {
        if stop() {
            say(err, "stopped by Ctrl+C. The program is still running on the board.");
            return Ended::Interrupted;
        }
        if let Some(limit) = limit
            && clock.now().saturating_sub(started) >= limit
        {
            say(
                err,
                &format!(
                    "the time limit of {} s ran out. The program is still running on the board.",
                    limit.as_secs_f64()
                ),
            );
            return Ended::TimedOut;
        }
        if clock.now() < first_look {
            clock.pause();
            continue;
        }
        let seen = match console.poll() {
            Ok(seen) => seen,
            Err(why) => {
                say(err, &format!("lost the program's console: {why}"));
                return Ended::Failed;
            }
        };
        if seen.is_empty() {
            clock.pause();
            continue;
        }
        for one in seen {
            match one {
                Seen::Out(bytes) => {
                    if out.write_all(&bytes).and_then(|()| out.flush()).is_err() {
                        say(err, "standard output was closed, so the program can no longer be followed.");
                        return Ended::Failed;
                    }
                }
                Seen::Lost(count) => say(
                    err,
                    &format!(
                        "{count} bytes of the board's console were overwritten on the board before \
                         they could be read."
                    ),
                ),
                Seen::Restarted => {
                    say(err, "the board started again; following the program from its new start.");
                }
                Seen::Ended(ProgramEnd::Returned(code)) => return Ended::Program(code),
                Seen::Ended(ProgramEnd::Aborted(code)) => {
                    say(err, &format!("the program was aborted, and the board gives code {code} for it."));
                    return Ended::Uncaught;
                }
            }
        }
    }
}

/// A board's memory, read through a probe while its program runs.
mod live {
    use super::Reach;
    use lamella_probe_console::TargetMemory;
    use lamella_probe_core::TargetAccess;

    /// Opens the memory of the board `reach` reaches, through the probe `serial` names, without
    /// halting it or resetting it.
    ///
    /// # Errors
    /// A probe that cannot be opened, and a board its debug port does not answer for.
    pub(super) fn open(reach: Reach, serial: &str) -> Result<Box<dyn TargetMemory>, String> {
        match reach {
            Reach::Rp2350 => {
                let session = lamella_probe::open(&lamella_probe::Selector::by_serial(serial))
                    .map_err(|why| format!("opening the probe {serial} to follow the program: {why}"))?;
                let mut target = lamella_probe_core::ArmDap::new(session.into_dap());
                lamella_cmsis_dap_rp2350::connect(&mut target).map_err(|why| {
                    format!("reaching the RP2350 through the probe {serial} to follow the program: {why}")
                })?;
                Ok(Box::new(Live(target)))
            }
            Reach::StLink(product_id) => {
                let mut stlink = lamella_stlink::StLink::open(product_id, Some(serial))
                    .map_err(|why| format!("opening the ST-LINK {serial} to follow the program: {why}"))?;
                stlink.enter_swd().map_err(|why| {
                    format!("entering SWD through the ST-LINK {serial} to follow the program: {why}")
                })?;
                Ok(Box::new(Live(stlink)))
            }
        }
    }

    /// Memory read through a probe that speaks the target-access seam, in whole words.
    struct Live<A>(A);

    impl<A: TargetAccess> TargetMemory for Live<A> {
        fn read(&mut self, address: u32, into: &mut [u8]) -> Result<(), String> {
            let (first, words, skip) = words_covering(address, into.len()).ok_or_else(|| {
                format!("{} bytes from {address:#010x} run past the end of memory", into.len())
            })?;
            if words == 0 {
                return Ok(());
            }
            let mut read = vec![0u32; words];
            self.0
                .read_words_into(first, &mut read)
                .map_err(|why| format!("reading the board's memory at {first:#010x}: {why}"))?;
            let bytes: Vec<u8> = read.iter().flat_map(|word| word.to_le_bytes()).collect();
            into.copy_from_slice(&bytes[skip..skip + into.len()]);
            Ok(())
        }
    }

    /// The whole words that hold `length` bytes from `address`: the first word's address, how many
    /// words, and where in them the bytes start. `None` for bytes past the end of memory.
    pub(super) fn words_covering(address: u32, length: usize) -> Option<(u32, usize, usize)> {
        let first = address & !3;
        let skip = (address - first) as usize;
        if length == 0 {
            return Some((first, 0, skip));
        }
        let end = u64::from(address).checked_add(u64::try_from(length).ok()?)?;
        if end > 1 << 32 {
            return None;
        }
        let words = usize::try_from((end - u64::from(first)).div_ceil(4)).ok()?;
        Some((first, words, skip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    /// A scratch directory of the test's own.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lamella-image-run-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// Lowercase hex of `bytes`' SHA-256, as a manifest spells it.
    fn digest(bytes: &[u8]) -> String {
        lamella_pe::sha256::sha256(bytes).iter().map(|byte| format!("{byte:02x}")).collect()
    }

    /// An RP2350 image the flash routes take: its boot block's marker inside the first 4 KB, where
    /// the bootrom looks for it.
    fn pico2_image() -> Vec<u8> {
        let mut image = vec![0u8; 512];
        image[64..68].copy_from_slice(&lamella_flash_routes::PICOBIN_BLOCK_START.to_le_bytes());
        image
    }

    /// Where the test ELF puts the ring, and how big it says the ring is: a 16 KB buffer and its
    /// header.
    const RING_AT: u32 = 0x2004_0000;
    const RING_BYTES: u32 = 16 + 16 * 1024;

    /// A little-endian ELF32 file whose symbol table names one data object, `name`, at `address`
    /// with `size` bytes: the null section, then the symbol table, its strings and the section
    /// names.
    fn elf_naming(name: &str, address: u32, size: u32) -> Vec<u8> {
        elf_naming_each(&[(name, address, size)])
    }

    /// As [`elf_naming`], with each `(name, address, size)` of `objects` named in turn.
    fn elf_naming_each(objects: &[(&str, u32, u32)]) -> Vec<u8> {
        let mut strings = vec![0u8];
        let mut symbols = vec![0u8; 16];
        for (name, address, size) in objects {
            let named_at = strings.len() as u32;
            strings.extend_from_slice(name.as_bytes());
            strings.push(0);
            for word in [named_at, *address, *size] {
                symbols.extend_from_slice(&word.to_le_bytes());
            }
            symbols.extend_from_slice(&[0x11, 0, 1, 0]);
        }
        let names: &[u8] = b"\0.symtab\0.strtab\0.shstrtab\0";
        let mut elf = vec![0u8; 52];
        elf[..4].copy_from_slice(&[0x7F, b'E', b'L', b'F']);
        elf[4..7].copy_from_slice(&[1, 1, 1]);
        elf[18..20].copy_from_slice(&40u16.to_le_bytes());
        let symbols_at = elf.len();
        elf.extend_from_slice(&symbols);
        let strings_at = elf.len();
        elf.extend_from_slice(&strings);
        let names_at = elf.len();
        elf.extend_from_slice(names);
        let headers_at = elf.len() as u32;
        elf[32..36].copy_from_slice(&headers_at.to_le_bytes());
        elf[46..48].copy_from_slice(&40u16.to_le_bytes());
        elf[48..50].copy_from_slice(&4u16.to_le_bytes());
        elf[50..52].copy_from_slice(&3u16.to_le_bytes());
        let sections = [
            (0u32, 0u32, 0usize, 0usize, 0u32),
            (1, 2, symbols_at, symbols.len(), 2),
            (9, 3, strings_at, strings.len(), 0),
            (17, 3, names_at, names.len(), 0),
        ];
        for (name, kind, offset, size, link) in sections {
            for word in [name, kind, 0, 0, offset as u32, size as u32, link, 0, 0, 0] {
                elf.extend_from_slice(&word.to_le_bytes());
            }
        }
        elf
    }

    /// `image` written as `name` in `dir` with its manifest beside it, for `board` at `base`, plus
    /// `extra` fields, and an ELF named `app.elf` holding the ring.
    fn image_with_manifest(dir: &Path, name: &str, image: &[u8], board: &str, base: &str, extra: &str) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, image).expect("write the image");
        std::fs::write(dir.join("app.elf"), elf_naming(RHU_RING, RING_AT, RING_BYTES)).expect("write the ELF");
        std::fs::write(
            manifest::path_for(&path),
            format!(
                "{{\"schema\":1,\"kind\":\"flash-image\",\"model\":\"native\",\"board\":\"{board}\",\"format\":\"bin\",\
                 \"base\":\"{base}\",\"sha256\":\"{}\",\"bytes\":{},\"producer\":\"a test\"{extra}}}",
                digest(image),
                image.len()
            ),
        )
        .expect("write the manifest");
        path
    }

    /// A Pico 2 image with a manifest naming its ELF.
    fn pico2_with_manifest(dir: &Path, extra: &str) -> PathBuf {
        image_with_manifest(dir, "app.bin", &pico2_image(), "rpi-pico2", "0x10000000", &format!(",\"elf\":\"app.elf\"{extra}"))
    }

    /// **AN IMAGE LAMELLA's FIRMWARE RUNS IS NOT WRITTEN OVER A PROBE**: its record's model is the
    /// interpreter's, and the refusal says how such an image is run, before anything is read beside it.
    #[test]
    fn an_interpreters_image_is_refused_naming_dotnet_run() {
        let dir = scratch("lmli");
        let image = dir.join("app.lmli");
        let bytes = b"a baked image".to_vec();
        std::fs::write(&image, &bytes).expect("write the image");
        manifest::write(&image, &manifest::for_lmli("rpi-pico2", &bytes, "a test")).expect("write the manifest");
        let refusal = plan(&image, None).err().expect("refused");
        assert!(refusal.contains("an image Lamella's firmware runs") && refusal.contains("dotnet run --device <device>"), "{refusal}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **WITHOUT ITS MANIFEST AN IMAGE IS NOT RUN**, and the refusal names the manifest it looked
    /// for; a manifest with no image beside it is refused naming where the image is looked for.
    #[test]
    fn a_missing_manifest_or_image_is_refused_naming_the_file() {
        let dir = scratch("no-manifest");
        let image = dir.join("app.bin");
        std::fs::write(&image, b"bytes").expect("write the image");
        let refusal = plan(&image, None).err().expect("refused");
        assert!(refusal.contains("no manifest at") && refusal.contains("app.bin.manifest.json"), "{refusal}");

        let lone = image_with_manifest(&dir, "lone.bin", &pico2_image(), "rpi-pico2", "0x10000000", "");
        std::fs::remove_file(&lone).expect("remove the image");
        let refusal = plan(&lone, None).err().expect("refused");
        assert!(refusal.contains("lone.bin") && refusal.contains("named as the manifest is"), "{refusal}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A MANIFEST IS NAMED FOR ITS IMAGE**, and the image is the file that name leaves when
    /// `.manifest.json` comes off it; any other name is refused, saying how a manifest is named.
    #[test]
    fn the_image_is_the_manifests_name_without_its_suffix() {
        assert_eq!(image_of(Path::new("out/app.elf.manifest.json")), Ok(PathBuf::from("out/app.elf")));
        assert_eq!(image_of(Path::new("app.bin.manifest.json")), Ok(PathBuf::from("app.bin")));
        for bad in ["out/app.elf", "out/.manifest.json", "out/app.manifest", "out/"] {
            let refusal = image_of(Path::new(bad)).expect_err(bad);
            assert!(refusal.contains("is not an image's manifest"), "{bad}: {refusal}");
        }
    }

    /// **A PROGRAM ON A BOARD TAKES NO ARGUMENTS YET**, and the refusal is one sentence that says
    /// so and names what it was given, quoted, so a space inside one argument stays visible.
    #[test]
    fn a_programs_arguments_are_refused_in_one_sentence() {
        let given = ["app1".to_owned(), "app two".to_owned()];
        let refusal = arguments_refusal(&given.iter().collect::<Vec<_>>());
        assert_eq!(
            refusal,
            "a program on a board cannot take arguments yet, so nothing was written (given: \"app1\", \
             \"app two\")."
        );
        assert_eq!(refusal.lines().count(), 1);
    }

    /// **AN IMAGE ITS MANIFEST DOES NOT DESCRIBE IS NOT RUN**, by the check `lamella flash` makes.
    #[test]
    fn an_image_its_manifest_does_not_describe_is_refused() {
        let dir = scratch("stale-manifest");
        let image = pico2_with_manifest(&dir, "");
        std::fs::write(&image, [1u8; 512]).expect("rewrite the image");
        let refusal = plan(&image, None).err().expect("refused");
        assert!(refusal.contains("digest does not match"), "{refusal}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A PICO IS WRITTEN OVER ITS PROBE ROUTE AND FOLLOWED AT ITS RING**: a run reads the console
    /// through the probe, so the bootloader drive is no use to it, and the ring's address and size
    /// come from the ELF the manifest names.
    #[test]
    fn a_pico_image_is_planned_over_the_probe_route_with_its_ring() {
        let dir = scratch("pico2");
        let image = pico2_with_manifest(&dir, ",\"console_channel\":\"memory\",\"log_channel\":\"memory\"");
        let planned = plan(&image, Some("2.5")).expect("planned");
        assert!(planned.route.writes_over_a_probe(), "{:?}", planned.route);
        assert_eq!(planned.reach, Reach::Rp2350);
        assert_eq!(planned.bytes, pico2_image(), "the flat bytes, unwrapped");
        assert_eq!(planned.ring, DataObject { address: RING_AT, size: RING_BYTES });
        assert!(!planned.console_off);
        assert_eq!(planned.limit, Some(Duration::from_millis(2500)));
        assert_eq!(planned.board, "rpi-pico2");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **WITHOUT A CONSOLE RING TO READ, NOTHING IS WRITTEN**: a flat image's manifest must name its
    /// ELF, the ELF must be there, and it must name a ring big enough for its header.
    #[test]
    fn a_run_without_a_ring_to_follow_writes_nothing() {
        let dir = scratch("no-ring");
        let unnamed = image_with_manifest(&dir, "app.bin", &pico2_image(), "rpi-pico2", "0x10000000", "");
        let refusal = plan(&unnamed, None).err().expect("refused");
        assert!(refusal.contains("names no ELF"), "{refusal}");

        let missing = pico2_with_manifest(&dir, "");
        std::fs::remove_file(dir.join("app.elf")).expect("remove the ELF");
        let refusal = plan(&missing, None).err().expect("refused");
        assert!(refusal.contains("the ELF the manifest names"), "{refusal}");

        std::fs::write(dir.join("app.elf"), elf_naming("other_symbol", RING_AT, RING_BYTES)).expect("an ELF");
        let refusal = plan(&missing, None).err().expect("refused");
        assert!(refusal.contains("names neither __lamella_console_log nor __rhu_console_log"), "{refusal}");

        std::fs::write(dir.join("app.elf"), elf_naming(RHU_RING, RING_AT, 16)).expect("an ELF");
        let refusal = plan(&missing, None).err().expect("refused");
        assert!(refusal.contains("too few for a console ring's header"), "{refusal}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A LAMELLA IMAGE IS FOLLOWED AT ITS OWN RING**: its ELF names `__lamella_console_log`, and
    /// the ring is taken from there as an RHU image's is taken from `__rhu_console_log`.
    #[test]
    fn a_lamella_image_is_followed_at_its_own_ring() {
        let dir = scratch("lamella-ring");
        let image = pico2_with_manifest(&dir, "");
        std::fs::write(dir.join("app.elf"), elf_naming("__lamella_console_log", RING_AT, RING_BYTES))
            .expect("an ELF");
        let planned = plan(&image, None).expect("planned");
        assert_eq!(planned.ring, DataObject { address: RING_AT, size: RING_BYTES });
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **AN ELF THAT NAMES BOTH RINGS IS REFUSED**: the file cannot say which one the program
    /// writes, and following the other would wait on a ring nothing writes.
    #[test]
    fn an_elf_naming_both_rings_is_refused() {
        let dir = scratch("both-rings");
        let image = pico2_with_manifest(&dir, "");
        let both = [("__lamella_console_log", RING_AT, RING_BYTES), ("__rhu_console_log", 0x2003_0000, RING_BYTES)];
        std::fs::write(dir.join("app.elf"), elf_naming_each(&both)).expect("an ELF");
        let refusal = plan(&image, None).err().expect("refused");
        assert!(refusal.contains("__lamella_console_log") && refusal.contains("__rhu_console_log"), "{refusal}");
        assert!(refusal.contains("nothing was written"), "{refusal}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A FLAT IMAGE IS WRITTEN ONLY WHERE ITS MANIFEST SAYS IT BELONGS**: a `base` the board's
    /// route does not write from is an image built for another layout.
    #[test]
    fn a_flat_image_whose_manifest_names_another_address_is_refused() {
        let dir = scratch("base");
        let image =
            image_with_manifest(&dir, "app.bin", &pico2_image(), "rpi-pico2", "0x10004000", ",\"elf\":\"app.elf\"");
        let refusal = plan(&image, None).err().expect("refused");
        assert!(refusal.contains("0x10004000") && refusal.contains("0x10000000"), "{refusal}");
        assert!(refusal.contains("nothing was written"), "{refusal}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A BOARD WITH NO ROUTE OVER A PROBE IS REFUSED**, and the refusal says why a run needs one;
    /// a board whose own route is a bootloader is written over the probe route beside it.
    #[test]
    fn a_board_written_only_by_a_bootloader_is_refused() {
        let row = Programming {
            board: "a-volume-only-board",
            aot_target: None,
            programmer: Programmer::Uf2Volume { family: 0, base: 0 },
            alternate: None,
        };
        let refusal = probe_route(&row).expect_err("refused");
        assert!(refusal.contains("a-volume-only-board"), "{refusal}");
        assert!(refusal.contains("through a probe"), "{refusal}");
        let dfu_first = lamella_flash_routes::PROGRAMMING
            .iter()
            .find(|row| row.board == "arduino-giga-r1-wifi")
            .expect("the GIGA's row");
        assert!(probe_route(dfu_first).expect("its probe route").writes_over_a_probe());
    }

    /// **A BOARD THIS BUILD CANNOT FOLLOW A PROGRAM ON IS REFUSED BEFORE ANYTHING IS WRITTEN**; an
    /// RP2350 over its probe and an ST board over its own ST-LINK are followed.
    #[test]
    fn only_a_route_the_reader_can_follow_is_planned() {
        assert_eq!(Reach::of(pico_route(), "rpi-pico2"), Ok(Reach::Rp2350));
        assert_eq!(
            Reach::of(f769_route(), "st-stm32f769i-disco"),
            Ok(Reach::StLink(lamella_stlink::product_id::V2_1))
        );
        let refusal = Reach::of(Programmer::MicrobitV2Daplink, "bbc-micro-bit-v2").expect_err("refused");
        assert!(refusal.contains("bbc-micro-bit-v2") && refusal.contains("nothing was written"), "{refusal}");
    }

    /// The time limit is a positive number of seconds, whole or with a fraction, and nothing else.
    #[test]
    fn the_time_limit_is_a_positive_number_of_seconds() {
        assert_eq!(parse_time_limit("30"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_time_limit(" 0.25 "), Ok(Duration::from_millis(250)));
        for bad in ["0", "-1", "abc", "inf", "NaN", "1e12", ""] {
            let refusal = parse_time_limit(bad).expect_err(bad);
            assert!(refusal.contains("--timeout takes a number of seconds"), "{bad}: {refusal}");
        }
    }

    /// The four Pico 2 boards' probe, a second one, and an F769's ST-LINK, as the listing finds them.
    fn bench() -> Vec<ProbeFound> {
        vec![
            ProbeFound {
                serial: Some("E6610000000000B2".to_owned()),
                description: "Debugprobe on Pico (CMSIS-DAP)".to_owned(),
                usb: (0x2e8a, 0x000c),
                ports: vec!["COM12".to_owned()],
            },
            ProbeFound {
                serial: Some("E6610000000000C3".to_owned()),
                description: "Debugprobe on Pico (CMSIS-DAP)".to_owned(),
                usb: (0x2e8a, 0x000c),
                ports: Vec::new(),
            },
            ProbeFound {
                serial: Some("066D000000000000000000C1".to_owned()),
                description: "ST-LINK/V2-1".to_owned(),
                usb: (lamella_stlink::ST_VENDOR_ID, 0x374b),
                ports: vec!["COM7".to_owned()],
            },
        ]
    }

    fn pico_route() -> Programmer {
        Programmer::Rp2350Probe { base: lamella_flash_routes::RP2_XIP_BASE }
    }

    fn f769_route() -> Programmer {
        let row = lamella_flash_routes::PROGRAMMING
            .iter()
            .find(|row| row.board == "st-stm32f769i-disco")
            .expect("the F769's row");
        row.programmer
    }

    /// **A NAMED PROBE MUST BE ATTACHED**, because a caller such as `dotnet run --device` passes on
    /// whatever id it was given; the refusal lists what is attached.
    #[test]
    fn a_named_probe_must_be_attached_and_the_refusal_lists_what_is() {
        let chosen = choose_probe(Some("e6610000000000b2"), None, "rpi-pico2", pico_route(), &bench())
            .expect("named, in any case");
        assert_eq!(chosen.serial, "E6610000000000B2", "the probe's own spelling");
        let refusal = choose_probe(Some("NOT-A-PROBE"), None, "rpi-pico2", pico_route(), &bench())
            .expect_err("refused");
        assert!(refusal.contains("NOT-A-PROBE") && refusal.contains("E6610000000000C3"), "{refusal}");
        let none = choose_probe(Some("X"), None, "rpi-pico2", pico_route(), &[]).expect_err("refused");
        assert!(none.contains("No debug probe is attached"), "{none}");
    }

    /// **AN EMPTY NAME IS NO NAME**: `dotnet run` passes an empty `$(Device)` when no device was
    /// chosen, and the run refuses rather than taking whatever is attached.
    #[test]
    fn an_empty_probe_name_is_refused_rather_than_taken_as_none() {
        let refusal =
            choose_probe(Some("  "), None, "rpi-pico2", pico_route(), &bench()[..1]).expect_err("refused");
        assert!(refusal.contains("no serial number"), "{refusal}");
    }

    /// **A PROBE'S ID FROM THE DEVICE LIST NAMES IT**, on the command line and in the environment
    /// alike, since a person pastes the id `dotnet run --list-devices` shows; the id with no serial
    /// after it is no name.
    #[test]
    fn a_probe_is_named_by_the_id_the_device_list_gives_it() {
        let named = choose_probe(Some("probe:E6610000000000B2"), None, "rpi-pico2", pico_route(), &bench())
            .expect("named by its id");
        assert_eq!(named.serial, "E6610000000000B2");
        let from_environment =
            choose_probe(None, Some("probe:e6610000000000c3"), "rpi-pico2", pico_route(), &bench()).expect("chosen");
        assert_eq!(from_environment.serial, "E6610000000000C3", "the probe's own spelling");
        let empty = choose_probe(Some("probe:"), None, "rpi-pico2", pico_route(), &bench()[..1]).expect_err("refused");
        assert!(empty.contains("no serial number"), "{empty}");
        let unknown = choose_probe(Some("probe:NOT-A-PROBE"), None, "rpi-pico2", pico_route(), &bench())
            .expect_err("refused");
        assert!(unknown.contains("serial number NOT-A-PROBE."), "the serial it looked for: {unknown}");
    }

    /// The environment names a probe when the command line does not, and the command line wins.
    #[test]
    fn the_environment_names_a_probe_when_the_command_line_does_not() {
        let from_environment =
            choose_probe(None, Some("E6610000000000C3"), "rpi-pico2", pico_route(), &bench()).expect("chosen");
        assert_eq!(from_environment.serial, "E6610000000000C3");
        let named =
            choose_probe(Some("E6610000000000B2"), Some("E6610000000000C3"), "rpi-pico2", pico_route(), &bench())
                .expect("chosen");
        assert_eq!(named.serial, "E6610000000000B2");
    }

    /// **WITH NONE NAMED, ONE PROBE IS TAKEN AND SEVERAL ARE NEVER GUESSED BETWEEN** -- and on a
    /// board whose debugger is soldered to it, only that kind of probe counts.
    #[test]
    fn with_none_named_one_probe_is_taken_and_several_are_refused() {
        let several = choose_probe(None, None, "rpi-pico2", pico_route(), &bench()).expect_err("refused");
        assert!(several.contains("3 probes") && several.contains("LAMELLA_PROBE_SERIAL"), "{several}");
        let st_link = choose_probe(None, None, "st-stm32f769i-disco", f769_route(), &bench()).expect("the ST-LINK");
        assert_eq!(st_link.serial, "066D000000000000000000C1");
        assert_eq!(st_link.probe.ports, ["COM7"], "and the serial port it carries");
        let wrong = choose_probe(Some("E6610000000000B2"), None, "st-stm32f769i-disco", f769_route(), &bench())
            .expect_err("refused");
        assert!(wrong.contains("Debugprobe") && wrong.contains("ST-LINK"), "{wrong}");
    }

    /// A clock a test moves: each pause moves it on by `step`. The time is shared, so a fake can read
    /// it.
    struct TestClock {
        at: Rc<Cell<Duration>>,
        step: Duration,
    }

    impl Clock for TestClock {
        fn now(&self) -> Duration {
            self.at.get()
        }

        fn pause(&self) {
            self.at.set(self.at.get() + self.step);
        }
    }

    fn clock() -> TestClock {
        TestClock { at: Rc::default(), step: Duration::from_millis(20) }
    }

    /// A console that notes the time of its first look, then answers as `inner` does.
    struct Timed {
        now: Rc<Cell<Duration>>,
        first: Rc<Cell<Option<Duration>>>,
        inner: Scripted,
    }

    impl Console for Timed {
        fn poll(&mut self) -> Result<Vec<Seen>, String> {
            if self.first.get().is_none() {
                self.first.set(Some(self.now.get()));
            }
            self.inner.poll()
        }
    }

    /// A console that hands back what it was scripted to, then nothing.
    struct Scripted(VecDeque<Result<Vec<Seen>, String>>);

    impl Console for Scripted {
        fn poll(&mut self) -> Result<Vec<Seen>, String> {
            self.0.pop_front().unwrap_or_else(|| Ok(Vec::new()))
        }
    }

    fn scripted(polls: Vec<Result<Vec<Seen>, String>>) -> Scripted {
        Scripted(polls.into())
    }

    /// Follows `console` to its end, and what each stream received.
    fn followed_to_end(
        console: &mut dyn Console,
        limit: Option<Duration>,
        stop: &dyn Fn() -> bool,
    ) -> (Ended, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ended = follow(console, &mut out, &mut err, limit, &clock(), stop, Duration::ZERO);
        (ended, String::from_utf8(out).expect("text"), String::from_utf8(err).expect("text"))
    }

    /// **THE BOARD'S CONSOLE IS STANDARD OUTPUT AND THE PROGRAM'S CODE COMES BACK UNCHANGED.**
    /// Standard output holds what the board wrote and nothing of this tool's; standard error holds
    /// this tool's lines, each prefixed.
    #[test]
    fn the_boards_console_is_standard_output_and_the_code_comes_back() {
        let mut console = scripted(vec![
            Ok(vec![Seen::Out(b"hello ".to_vec())]),
            Ok(Vec::new()),
            Ok(vec![Seen::Out(b"board\n".to_vec()), Seen::Lost(12)]),
            Ok(vec![Seen::Out(b"bye\n".to_vec()), Seen::Ended(ProgramEnd::Returned(70_000))]),
        ]);
        let (ended, out, err) = followed_to_end(&mut console, None, &|| false);
        assert_eq!(ended, Ended::Program(70_000), "a code past a byte, whole");
        assert_eq!(out, "hello board\nbye\n");
        assert_eq!(
            err,
            "lamella: 12 bytes of the board's console were overwritten on the board before they \
             could be read.\n"
        );
    }

    /// **AN ABORTED PROGRAM IS 1, AND SAID WITH THE CODE THE BOARD GIVES.**
    #[test]
    fn an_aborted_program_ends_the_run_with_one_and_says_so() {
        let mut console = scripted(vec![Ok(vec![Seen::Ended(ProgramEnd::Aborted(134))])]);
        let (ended, out, err) = followed_to_end(&mut console, None, &|| false);
        assert_eq!((ended, out.as_str()), (Ended::Uncaught, ""));
        assert_eq!(err, "lamella: the program was aborted, and the board gives code 134 for it.\n");
    }

    /// **A TIME LIMIT EXISTS ONLY WHEN THE CALLER GAVE ONE**, and a run it stops is 1, with the
    /// limit named and the program left running.
    #[test]
    fn a_time_limit_stops_following_only_when_one_was_given() {
        let mut quiet = scripted(Vec::new());
        let (ended, _, err) = followed_to_end(&mut quiet, Some(Duration::from_millis(100)), &|| false);
        assert_eq!(ended, Ended::TimedOut);
        assert_eq!(err, "lamella: the time limit of 0.1 s ran out. The program is still running on the board.\n");

        let mut polls: Vec<Result<Vec<Seen>, String>> = (0..1000).map(|_| Ok(Vec::new())).collect();
        polls.push(Ok(vec![Seen::Ended(ProgramEnd::Returned(0))]));
        let (ended, _, _) = followed_to_end(&mut scripted(polls), None, &|| false);
        assert_eq!(ended, Ended::Program(0));
    }

    /// **CTRL+C STOPS FOLLOWING AT ONCE AND LEAVES THE PROGRAM RUNNING**, as the operating system
    /// ends a program stopped by Ctrl+C.
    #[test]
    fn ctrl_c_stops_following_and_leaves_the_program_running() {
        let looks = Cell::new(0);
        let stop = || {
            looks.set(looks.get() + 1);
            looks.get() > 3
        };
        let mut console = scripted(vec![Ok(vec![Seen::Out(b"tick\n".to_vec())])]);
        let (ended, out, err) = followed_to_end(&mut console, None, &stop);
        assert_eq!(ended, Ended::Interrupted);
        assert_eq!(out, "tick\n");
        assert_eq!(err, "lamella: stopped by Ctrl+C. The program is still running on the board.\n");
    }

    /// A console that cannot be read any longer is this tool's failure, and said.
    #[test]
    fn a_console_lost_mid_run_is_this_tools_failure() {
        let mut console = scripted(vec![Err("the probe went away".to_owned())]);
        let (ended, _, err) = followed_to_end(&mut console, None, &|| false);
        assert_eq!(ended, Ended::Failed);
        assert_eq!(err, "lamella: lost the program's console: the probe went away\n");
    }

    /// The steps a whole run took, in order, as the fakes saw them.
    type Steps = Rc<RefCell<Vec<String>>>;

    struct FakeWriter {
        steps: Steps,
        fails: bool,
    }

    impl Writer for FakeWriter {
        fn write(&mut self, plan: &ImagePlan, serial: &str, say: &mut dyn FnMut(&str)) -> Result<(), String> {
            self.steps.borrow_mut().push(format!("write {} B through {serial}", plan.bytes.len()));
            say("writing over a test's probe...");
            if self.fails { Err("the part did not answer".to_owned()) } else { Ok(()) }
        }
    }

    struct FakeAttach {
        steps: Steps,
        console: Option<Box<dyn Console>>,
    }

    impl Attach for FakeAttach {
        fn attach(&mut self, _plan: &ImagePlan, probe: &Chosen) -> Result<Box<dyn Console>, String> {
            self.steps.borrow_mut().push(format!("follow through {}", probe.serial));
            Ok(self.console.take().expect("one console"))
        }
    }

    /// The plan of a Pico 2 image, with `extra` manifest fields.
    fn planned_pico(dir: &Path, extra: &str) -> ImagePlan {
        plan(&pico2_with_manifest(dir, extra), None).expect("planned")
    }

    /// **A WHOLE RUN, IN ITS ORDER**: the write goes through the chosen probe, the console is
    /// followed after it, Ctrl+C is watched only once the program runs, every line of this tool's
    /// is on standard error, and the program's code is the run's.
    #[test]
    fn a_whole_run_writes_then_follows_and_ends_with_the_programs_code() {
        let dir = scratch("whole-run");
        let planned = planned_pico(&dir, "");
        let chosen = choose_probe(Some("E6610000000000B2"), None, "rpi-pico2", planned.route, &bench())
            .expect("chosen");
        let steps: Steps = Rc::default();
        let console = scripted(vec![Ok(vec![Seen::Out(b"42\n".to_vec()), Seen::Ended(ProgramEnd::Returned(7))])]);
        let mut writer = FakeWriter { steps: Rc::clone(&steps), fails: false };
        let mut attach = FakeAttach { steps: Rc::clone(&steps), console: Some(Box::new(console)) };
        let watched_steps = Rc::clone(&steps);
        let mut watch = move || {
            watched_steps.borrow_mut().push("watch Ctrl+C".to_owned());
            true
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ended = run_planned(&planned, &chosen, &mut writer, &mut attach, &mut out, &mut err, &clock(), &mut watch, &|| false);

        assert_eq!(ended, Ended::Program(7));
        assert_eq!(String::from_utf8(out).expect("text"), "42\n", "standard output is the board's alone");
        assert_eq!(
            steps.borrow().as_slice(),
            ["write 512 B through E6610000000000B2", "follow through E6610000000000B2", "watch Ctrl+C"]
        );
        let err = String::from_utf8(err).expect("text");
        assert!(err.lines().all(|line| line.starts_with("lamella:")), "every line of ours is marked:\n{err}");
        assert_eq!(
            err.lines().next(),
            Some("lamella: running the native image for rpi-pico2 through probe E6610000000000B2."),
            "the run says how the program runs first:\n{err}"
        );
        assert!(err.contains("lamella: probe E6610000000000B2: Debugprobe on Pico (CMSIS-DAP)"), "{err}");
        assert!(err.contains("lamella: writing over a test's probe..."), "the write's own lines: {err}");
        assert!(!err.contains("console off"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A WRITE THAT FAILS ENDS THE RUN WITH 1 AND ITS SENTENCE, AND NOTHING IS FOLLOWED.**
    #[test]
    fn a_failed_write_ends_the_run_and_follows_nothing() {
        let dir = scratch("failed-write");
        let planned = planned_pico(&dir, "");
        let chosen = choose_probe(None, Some("E6610000000000B2"), "rpi-pico2", planned.route, &bench())
            .expect("chosen");
        let steps: Steps = Rc::default();
        let mut writer = FakeWriter { steps: Rc::clone(&steps), fails: true };
        let mut attach = FakeAttach { steps: Rc::clone(&steps), console: Some(Box::new(scripted(Vec::new()))) };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ended = run_planned(&planned, &chosen, &mut writer, &mut attach, &mut out, &mut err, &clock(), &mut || true, &|| false);
        assert_eq!(ended, Ended::Failed);
        assert!(out.is_empty());
        assert!(!steps.borrow().iter().any(|step| step.starts_with("follow")), "{:?}", steps.borrow());
        assert!(String::from_utf8(err).expect("text").ends_with("lamella: the part did not answer\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **THE RING IS FIRST LOOKED AT WELL AFTER THE WRITE'S RESET**: until the new start sets it up
    /// again it holds the last run's words, so the first look waits out `SETTLE` from the write.
    #[test]
    fn the_ring_is_first_looked_at_a_settle_after_the_write() {
        let dir = scratch("settle");
        let planned = planned_pico(&dir, "");
        let chosen = choose_probe(Some("E6610000000000B2"), None, "rpi-pico2", planned.route, &bench())
            .expect("chosen");
        let clock = clock();
        let first = Rc::new(Cell::new(None));
        let console = Timed {
            now: Rc::clone(&clock.at),
            first: Rc::clone(&first),
            inner: scripted(vec![Ok(vec![Seen::Ended(ProgramEnd::Returned(0))])]),
        };
        let steps: Steps = Rc::default();
        let mut writer = FakeWriter { steps: Rc::clone(&steps), fails: false };
        let mut attach = FakeAttach { steps: Rc::clone(&steps), console: Some(Box::new(console)) };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ended = run_planned(&planned, &chosen, &mut writer, &mut attach, &mut out, &mut err, &clock, &mut || true, &|| false);
        assert_eq!(ended, Ended::Program(0));
        assert!(first.get().expect("looked at") >= SETTLE, "first looked at {:?}", first.get());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **AN IMAGE BUILT WITH ITS CONSOLE OFF IS RUN, AND SAID BEFORE THE WRITE**: its output is
    /// kept nowhere, and how it ends still comes back.
    #[test]
    fn an_image_built_with_its_console_off_is_run_and_said() {
        let dir = scratch("console-off");
        let planned = planned_pico(&dir, ",\"console_channel\":\"off\"");
        assert!(planned.console_off);
        let chosen = choose_probe(Some("E6610000000000B2"), None, "rpi-pico2", planned.route, &bench())
            .expect("chosen");
        let steps: Steps = Rc::default();
        let console = scripted(vec![Ok(vec![Seen::Ended(ProgramEnd::Returned(0))])]);
        let mut writer = FakeWriter { steps: Rc::clone(&steps), fails: false };
        let mut attach = FakeAttach { steps: Rc::clone(&steps), console: Some(Box::new(console)) };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let ended = run_planned(&planned, &chosen, &mut writer, &mut attach, &mut out, &mut err, &clock(), &mut || true, &|| false);
        assert_eq!(ended, Ended::Program(0));
        let err = String::from_utf8(err).expect("text");
        let said = err.find("console off").expect("said");
        assert!(said < err.find("writing over").expect("the write"), "before the write:\n{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A board's RAM as one block at `at`, as a test arranges it before the run follows it.
    struct Block {
        at: u32,
        bytes: Vec<u8>,
    }

    impl TargetMemory for Block {
        fn read(&mut self, address: u32, into: &mut [u8]) -> Result<(), String> {
            let start = (address - self.at) as usize;
            into.copy_from_slice(&self.bytes[start..start + into.len()]);
            Ok(())
        }
    }

    /// **AN ABORTED PROGRAM, READ FROM ITS RING, ENDS THE RUN WITH ONE AND SAYS SO**: the ring's
    /// text on standard output, and its code in the sentence on standard error.
    #[test]
    fn an_aborted_program_arrives_through_its_end_magic() {
        let text = b"Unhandled exception.\n";
        let mut bytes = vec![0u8; 16 + 32];
        for (index, word) in [END_MAGICS.aborted, 32, text.len() as u32, 134].into_iter().enumerate() {
            bytes[index * 4..index * 4 + 4].copy_from_slice(&word.to_le_bytes());
        }
        bytes[16..16 + text.len()].copy_from_slice(text);
        let ring = DataObject { address: 0x2000_8000, size: bytes.len() as u32 };
        let memory = Box::new(Block { at: 0x2000_8000, bytes });
        let mut console = RingConsole { memory, reader: RingReader::new(ring, END_MAGICS, 3) };
        let (ended, out, err) = followed_to_end(&mut console, None, &|| false);
        assert_eq!((ended, out.as_str()), (Ended::Uncaught, "Unhandled exception.\n"));
        assert!(err.contains("code 134"), "{err}");
    }

    /// **A READ IN WHOLE WORDS COVERS THE BYTES ASKED FOR**, from an address on any byte, and
    /// nothing past the end of memory.
    #[test]
    fn the_words_read_cover_the_bytes_asked_for() {
        assert_eq!(live::words_covering(0x2000_0001, 6), Some((0x2000_0000, 2, 1)));
        assert_eq!(live::words_covering(0x2000_0004, 4), Some((0x2000_0004, 1, 0)));
        assert_eq!(live::words_covering(0x2000_0003, 2), Some((0x2000_0000, 2, 3)));
        assert_eq!(live::words_covering(0x2000_0002, 0), Some((0x2000_0000, 0, 2)));
        assert_eq!(live::words_covering(0xFFFF_FFFC, 4), Some((0xFFFF_FFFC, 1, 0)));
        assert_eq!(live::words_covering(0xFFFF_FFFE, 4), None);
    }
}
