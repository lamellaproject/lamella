//! The on-device DAP server: a host binary bridging a Debug Adapter Protocol client (VS
//! Code) to a Cortex-M target over the Lamella CMSIS-DAP stack. It links `lamella-dap`
//! WITHOUT the interpreter (`default-features = false`) -- the adapter, the wire protocol,
//! and the `DebugBackend` trait -- driven here by a `DeviceBackend` over a real probe.

use lamella_aot::build;
use lamella_cmsis_dap_nrf::Nrf51Flash;
use lamella_debug_device::DeviceBackend;
use lamella_metadata::{Assembly, PortablePdb};
use lamella_probe_core::{ArmDap, ProbeError, TargetAccess, TargetAccessExt};
use std::io::{IsTerminal, Write};

/// build_debug's line-table offsets are image-relative (the code sits at image offset 8, after the
/// [SP][reset] vector table, and the image flashes at address 0), so a raw PC indexes the tables
/// directly -- no base to subtract.
/// Which probe family to open. Selected explicitly rather than by trying ids in order: on a bench
/// with several probes attached, a server that opens whichever answers first can flash a board
/// someone else is using.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProbeKind {
    CmsisDap,
    #[cfg(feature = "st")]
    StLink,
}

/// Which part is being driven: it selects the part-specific handling this binary has, which is a
/// flash algorithm for some variants and a reset sequence for others.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Nrf51,
    /// Selects how the part is STOPPED rather than how it is programmed -- stopping is the one
    /// thing the generic sequence cannot do here (see `--reset` below).
    #[cfg(feature = "sam")]
    Samd21,
    #[cfg(feature = "st")]
    Stm32F0,
    #[cfg(feature = "st")]
    Stm32F4,
    #[cfg(feature = "st")]
    Stm32F7,
    #[cfg(feature = "st")]
    Stm32H7,
}

impl Part {
    /// Where this part's flash is mapped for execution -- and therefore what `DeviceBackend` must
    /// subtract from a PC before indexing the image-relative line tables.
    fn flash_base(self) -> u32 {
        match self {
            Part::Nrf51 => 0,
            #[cfg(feature = "sam")]
            Part::Samd21 => 0,
            #[cfg(feature = "st")]
            _ => 0x0800_0000,
        }
    }

    /// The low-power debug bits an ST-LINK sets while it attaches this part with its core held in
    /// reset, for a part whose firmware can sleep where a plain attach cannot read it, or `None` for a
    /// part attached plainly.
    ///
    /// An STM32F7 is attached this way, with `DBGMCU_CR`'s low-power debug bits, which keep the clocks
    /// a debugger connection needs running while the part's firmware waits for an interrupt. A system
    /// reset does not clear them, so they stay set, and those clocks keep running in the part's Sleep,
    /// Stop and Standby modes, until its next power-on reset.
    #[cfg(feature = "st")]
    fn low_power_debug(self) -> Option<lamella_stlink::LowPowerDebug> {
        match self {
            Part::Stm32F7 => Some(lamella_stlink::LowPowerDebug {
                register: lamella_cmsis_dap_stm32::STM32F7_DBGMCU_CR,
                bits: lamella_cmsis_dap_stm32::STM32F7_DBGMCU_CR_LOW_POWER_DEBUG,
            }),
            _ => None,
        }
    }

    /// The family of the `lamella flash` route this part's deploy writes through, or `None` for a
    /// part whose deploy this server writes itself.
    #[cfg(feature = "st")]
    fn route_family(self) -> Option<lamella_flash_routes::StFamily> {
        match self {
            Part::Stm32F7 => Some(lamella_flash_routes::StFamily::F7),
            Part::Stm32H7 => Some(lamella_flash_routes::StFamily::H7),
            _ => None,
        }
    }
}

const USAGE: &str = "usage: device-dap-server [--probe cmsis|stlink] \
                     [--part nrf51|samd21|f0|f4|f7|h7] [--pid 0xNNNN] [--attach [--reset]] \
                     <program.dll|program.elf> [<Type> <Method>] [probe-serial]\n       \
                     device-dap-server --list-probes    every attached CMSIS-DAP probe, as JSON\n\n\
                     --part names a part this build can PROGRAM, and is only needed when this \
                     command\nwrites the program. To debug one that is already on the board, use \
                     --attach, which\nnames no part.";

/// Why a debug session cannot be served on the class-library tier.
///
/// **THE FLAG IS TAKEN AND REFUSED, WHICH IS NOT THE SAME AS NOT TAKING IT.** `lamella build` and
/// `lamella deploy` both offer `--class-library`, so somebody debugging the program they just
/// deployed will type it here; an unrecognized option would be read as the program name.
///
/// **AND IT DOES NOT SERVE THE FLAT IMAGE INSTEAD.** What this binary serves is a debug session,
/// whose whole content is a mapping from addresses in the flashed image back to lines of source. An
/// image built on the other tier with this tier's map would answer every `stackTrace` with a line
/// that resolves and is wrong, which is worse than refusing: a wrong line is believed.
const CLASS_LIBRARY_REFUSAL: &str = "\
device-dap-server: --class-library cannot be debugged yet.

The class-library tier links the program, which moves and drops code, so the line tables built
while lowering it no longer describe the image that gets flashed. Serving them anyway would give
you a source line for every stop, and they would be the wrong lines.

Build and deploy on that tier with `lamella deploy --class-library`; debug on the flat tier.";

/// What the command line asks for, once everything this build cannot do as asked has been refused.
struct Options {
    probe: ProbeKind,
    part: Part,
    /// Whether `--part` was given, rather than the default standing in for it.
    part_named: bool,
    attach: bool,
    reset: bool,
    /// The ST-Link model to open, by its USB product id.
    #[cfg(feature = "st")]
    stlink_pid: u16,
    program: String,
    /// `<Type> <Method>`, when the method to debug is not the entry point.
    target: Option<(String, String)>,
    serial: Option<String>,
}

impl Options {
    /// Reads the arguments that follow the program name.
    ///
    /// Anything this build cannot do as asked is an `Err` carrying a reason the user can act on: a
    /// probe or a part the build was compiled without, a product id that does not parse, or a product
    /// id with no ST-Link to apply it to. None of them falls back to a default, because the default
    /// opens a different probe, or drives a different part, than the one the user named.
    fn parse(arguments: impl IntoIterator<Item = String>) -> Result<Options, String> {
        let mut probe = ProbeKind::CmsisDap;
        let mut part = Part::Nrf51;
        let mut part_named = false;
        let mut attach = false;
        let mut reset = false;
        let mut pid = None;
        let mut positional: Vec<String> = Vec::new();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.as_str() {
                "--probe" => probe = probe_kind(arguments.next().as_deref())?,
                "--attach" => attach = true,
                "--reset" => reset = true,
                "--part" => {
                    part_named = true;
                    part = part_called(arguments.next().as_deref())?;
                }
                "--pid" => pid = Some(product_id(arguments.next().as_deref())?),
                "--class-library" => return Err(CLASS_LIBRARY_REFUSAL.to_owned()),
                unknown if unknown.starts_with("--") => {
                    return Err(format!(
                        "device-dap-server: unknown option {unknown:?}.\n{USAGE}"
                    ));
                }
                _ => positional.push(argument),
            }
        }
        #[cfg(feature = "st")]
        let opens_an_st_link = probe == ProbeKind::StLink;
        #[cfg(not(feature = "st"))]
        let opens_an_st_link = false;
        if pid.is_some() && !opens_an_st_link {
            return Err(
                "--pid names the ST-Link model to open, so it needs --probe stlink".to_owned(),
            );
        }

        let mut positional = positional.into_iter();
        let program = positional.next().ok_or_else(|| USAGE.to_owned())?;
        let rest: Vec<String> = positional.collect();
        let (target, serial): (Option<(String, String)>, Option<String>) = match rest.len() {
            0 => (None, None),
            1 => (None, Some(rest[0].clone())),
            2 => (Some((rest[0].clone(), rest[1].clone())), None),
            3 => (
                Some((rest[0].clone(), rest[1].clone())),
                Some(rest[2].clone()),
            ),
            _ => return Err(USAGE.to_owned()),
        };

        Ok(Options {
            probe,
            part,
            part_named,
            attach,
            reset,
            #[cfg(feature = "st")]
            stlink_pid: pid.unwrap_or(lamella_stlink::product_id::V2_1),
            program,
            target,
            serial,
        })
    }
}

/// `--probe`'s value.
fn probe_kind(value: Option<&str>) -> Result<ProbeKind, String> {
    match value {
        Some("cmsis") => Ok(ProbeKind::CmsisDap),
        #[cfg(feature = "st")]
        Some("stlink") => Ok(ProbeKind::StLink),
        #[cfg(not(feature = "st"))]
        Some("stlink") => Err(missing_feature("--probe stlink", "st")),
        Some(other) => Err(format!("--probe takes cmsis or stlink, not {other:?}")),
        None => Err("--probe takes cmsis or stlink".to_owned()),
    }
}

/// What a reader is told when the part they named is not one of the six.
///
/// **THE LIST READS AS THE LIST OF BOARDS THIS DEBUGS, AND IT IS NOT ONE.** `--part` chooses a
/// flash algorithm and a reset sequence -- it answers *how is this part written and stopped*, which
/// is a question this command only asks when it is the thing doing the writing. Stopping at a
/// source line is a Cortex-M facility, so a board whose part is not named here is debugged by
/// flashing it however that board is flashed and then attaching.
///
/// A short list beside a much longer board catalog invites the wrong conclusion, and the conclusion
/// costs a user their whole board: a reader with a part outside the list has no reason to try
/// `--attach` and every reason to stop.
///
/// Appended to BOTH refusals rather than written into one, because the arm a reader reaches
/// depends on whether they typed a value at all -- and the one that omits this is the one that
/// sends them away.
const PART_IS_FOR_PROGRAMMING: &str = "

Those are the parts this build can PROGRAM: --part picks a flash algorithm and a reset sequence.
It is not the list of boards that can be debugged. To debug a program already on the board, pass
--attach, which names no part -- flash the board however that board is flashed, then attach.";

/// `--part`'s value.
fn part_called(value: Option<&str>) -> Result<Part, String> {
    match value {
        Some("nrf51") => Ok(Part::Nrf51),
        #[cfg(feature = "sam")]
        Some("samd21") => Ok(Part::Samd21),
        #[cfg(not(feature = "sam"))]
        Some("samd21") => Err(missing_feature("--part samd21", "sam")),
        #[cfg(feature = "st")]
        Some("f0") => Ok(Part::Stm32F0),
        #[cfg(feature = "st")]
        Some("f4") => Ok(Part::Stm32F4),
        #[cfg(feature = "st")]
        Some("f7") => Ok(Part::Stm32F7),
        #[cfg(feature = "st")]
        Some("h7") => Ok(Part::Stm32H7),
        #[cfg(not(feature = "st"))]
        Some(part @ ("f0" | "f4" | "f7" | "h7")) => {
            Err(missing_feature(&format!("--part {part}"), "st"))
        }
        Some(other) => Err(format!("--part takes nrf51, samd21, f0, f4, f7 or h7, not {other:?}.{PART_IS_FOR_PROGRAMMING}")),
        None => Err(format!("--part takes nrf51, samd21, f0, f4, f7 or h7.{PART_IS_FOR_PROGRAMMING}")),
    }
}

/// `--pid`'s value: a USB product id in hexadecimal, with or without `0x`.
fn product_id(value: Option<&str>) -> Result<u16, String> {
    const EXPECTED: &str = "--pid takes an ST-Link's USB product id in hexadecimal, such as 0x374b";
    let text = value.ok_or_else(|| EXPECTED.to_owned())?;
    let digits = text
        .strip_prefix("0x")
        .or_else(|| text.strip_prefix("0X"))
        .unwrap_or(text);
    u16::from_str_radix(digits, 16).map_err(|_| format!("{EXPECTED}, not {text:?}"))
}

/// The refusal for an argument this build was compiled without support for: the feature it needs, and
/// the command that installs a server with every feature on.
#[cfg(any(not(feature = "st"), not(feature = "sam")))]
fn missing_feature(argument: &str, feature: &str) -> String {
    format!(
        "{argument} needs a device-dap-server built with the `{feature}` feature, and this one was \
         built without it. A default build has every feature: cargo install --locked --git \
         https://github.com/lamellaproject/lamella lamella-debug-server"
    )
}

/// One JSON string, with the two characters that would end it early and the control range escaped.
///
/// A probe reports its product name and serial as the operating system gives them, and neither is
/// this program's to trust: a quote or a backslash in either would produce a document the caller
/// cannot parse, and the caller is a debug client choosing which board to write.
fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// `--list-probes`: every attached CMSIS-DAP probe, as JSON, for a caller that has to CHOOSE one.
///
/// # WHY THIS LIVES HERE AND NOT IN A SECOND TOOL
///
/// **This is the executable that will open the probe, so the list and the open cannot disagree.**
/// A separate lister would walk the same ladder a second time and could answer differently -- a
/// probe replugged between the two, a build with different features, an environment variable set
/// in one process and not the other -- and the caller would name a board this program then fails to
/// find. One walk, one answer.
///
/// **What a caller does with it is the point.** `Selector::from_environment` refuses an ambiguous
/// bench rather than guessing, which is right and is not enough by itself: a CLI can print "name
/// one with `--probe`", but an editor that can ASK should ask, and it needs the candidates to ask
/// with. Every micro:bit is `0d28:0204` and every RPi Debug Probe is `2e8a:000c`, so the serial is
/// the only thing that distinguishes two of a kind -- and it is reported WHOLE, because these
/// serials share long suffixes (the interface chip's) and differ in the middle.
///
/// A probe the operating system reports with no serial is listed with `"serial": null` rather than
/// dropped: it is attached, it is why a bench looks ambiguous, and a caller that cannot see it
/// cannot explain its own refusal.
fn list_probes() -> std::io::Result<()> {
    use std::io::Write;

    let probes = lamella_probe::list();
    let rows: Vec<String> = probes
        .iter()
        .map(|probe| {
            let serial = probe
                .serial
                .as_deref()
                .map_or_else(|| String::from("null"), json_string);
            let product = probe
                .product
                .as_deref()
                .map_or_else(|| String::from("null"), json_string);
            format!(
                "{{\"serial\":{serial},\"product\":{product},\"vendorId\":{},\"productId\":{}}}",
                probe.vendor_id, probe.product_id
            )
        })
        .collect();
    let mut out = std::io::stdout();
    writeln!(out, "[{}]", rows.join(","))?;
    out.flush()
}

fn main() -> std::io::Result<()> {
    if std::env::args()
        .skip(1)
        .any(|argument| argument == "--list-probes")
    {
        return list_probes();
    }
    let options = match Options::parse(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(reason) => return refuse(&reason),
    };
    if options.reset && !options.part_named {
        eprintln!(
            "reset: no --part was named, so the generic sequence is what will run. A part with a \
             sequence of its own needs naming to get it."
        );
    }

    let debug = match load_program(&options) {
        Ok(debug) => debug,
        Err(reason) => return refuse(&reason),
    };
    let lamella_debug_device::program::DebugProgram {
        image,
        image_base,
        lines,
        names,
        files,
        entry,
        frames,
        locals,
        unwind,
    } = debug;

    match options.probe {
        #[cfg(feature = "st")]
        ProbeKind::StLink => {
            let stlink =
                match lamella_stlink::StLink::open(options.stlink_pid, options.serial.as_deref()) {
                    Ok(stlink) => stlink,
                    Err(error) => {
                        return refuse(&format!(
                            "could not open the ST-Link with USB product id {:#06x}: {error}",
                            options.stlink_pid
                        ));
                    }
                };
            let low_power_debug = options.part.low_power_debug();
            let attach_under_reset = move |stlink: &mut lamella_stlink::StLink| {
                stlink.attach_under_reset(low_power_debug)
            };
            serve(
                stlink,
                low_power_debug
                    .is_some()
                    .then_some(&attach_under_reset as &PartAttach<lamella_stlink::StLink>),
                options.part,
                image_base,
                options.attach,
                options.reset,
                &image,
                lines,
                names,
                files,
                entry,
                frames,
                locals,
                unwind,
            )
        }
        ProbeKind::CmsisDap => {
            let selector = options.serial.as_deref().map_or_else(
                lamella_probe::Selector::from_environment,
                lamella_probe::Selector::by_serial,
            );
            let session = match lamella_probe::open(&selector) {
                Ok(session) => session,
                Err(error) => return refuse(&format!("could not open a CMSIS-DAP probe: {error}")),
            };
            eprintln!(
                "probe: {:?} {:04x}:{:04x} serial {:?}",
                session.info.product,
                session.info.vendor_id,
                session.info.product_id,
                session.info.serial
            );
            serve(
                ArmDap::new(session.into_dap()),
                None,
                options.part,
                image_base,
                options.attach,
                options.reset,
                &image,
                lines,
                names,
                files,
                entry,
                frames,
                locals,
                unwind,
            )
        }
    }
}

/// Reads the program and composes what a session needs to map its addresses to source.
///
/// A program this server cannot debug as the command line describes it is an `Err` with the reason:
/// a file that cannot be read, an ELF that does not parse, or an ELF that would be written somewhere
/// other than where it was linked to run.
fn load_program(options: &Options) -> Result<lamella_debug_device::program::DebugProgram, String> {
    let program = &options.program;
    let bytes =
        std::fs::read(program).map_err(|error| format!("could not read {program}: {error}"))?;
    if bytes.starts_with(b"\x7fELF") {
        let elf = lamella_debug_device::program::from_elf(&bytes)
            .map_err(|error| format!("{program}: {error}"))?;
        if !options.attach && elf.image_base != options.part.flash_base() {
            return Err(format!(
                "{program} is linked to load at {:#010x} and --part puts this part's flash at {:#010x} \
                 -- either the wrong --part or an image built for another board",
                elf.image_base,
                options.part.flash_base()
            ));
        }
        if !options.attach && !options.part_named {
            return Err(format!(
                "{program} is an ELF and no --part was given, so the default would choose a flash \
                 algorithm rather than the right one -- and a matching base does not distinguish two \
                 parts that both boot from zero. Either name the part with --part, or deploy it with \
                 `lamella flash` and debug the running program with --attach."
            ));
        }
        return Ok(elf);
    }

    let (lines, names, image, file, entry) = source_lines(program, options.target.as_ref());
    let ends: Vec<u32> = names
        .iter()
        .enumerate()
        .map(|(index, _)| names.get(index + 1).map_or(u32::MAX, |&(start, _)| start))
        .collect();
    let names: Vec<(u32, u32, String)> = names
        .into_iter()
        .zip(ends)
        .map(|((start, name), end)| (start, end, name))
        .collect();
    Ok(lamella_debug_device::program::DebugProgram {
        image,
        image_base: options.part.flash_base(),
        lines: lines
            .into_iter()
            .map(|(offset, line)| lamella_debug_device::LineRow {
                offset,
                line,
                file: 0,
            })
            .collect(),
        names,
        files: vec![file],
        entry,
        frames: Vec::new(),
        locals: lamella_debug_device::program::LocalSections::default(),
        unwind: lamella_debug_device::program::UnwindTables::default(),
    })
}

/// Refuses to start, and makes sure the reason reaches whoever started this server.
///
/// The reason always goes to standard error. When standard input is not a terminal, a client started
/// this server and is waiting on the protocol, so the reason is also served as the answer to the
/// client's `launch` request, where an editor shows it to the user -- standard error is outside the
/// protocol. The process then exits with status 2.
fn refuse(reason: &str) -> std::io::Result<()> {
    eprintln!("device-dap-server: {reason}");
    if !std::io::stdin().is_terminal() {
        let mut debugger = lamella_dap::Debugger::refusing(reason);
        lamella_dap::serve(
            &mut debugger,
            &mut std::io::stdin().lock(),
            &mut std::io::stdout().lock(),
        )?;
        std::io::stdout().flush()?;
    }
    std::process::exit(2)
}

/// A part's own attach through one probe, which a session takes in place of a plain connect: see
/// [`serve`].
type PartAttach<A> = dyn Fn(&mut A) -> Result<(), ProbeError>;

/// Flashes the image, then serves DAP over stdio against the freshly flashed program.
///
/// Generic over the probe, so an ST-Link and a CMSIS-DAP probe run the identical code path. What the
/// probe adds is `attach_under_reset`, the part's own attach through that probe when the part has
/// one: the deploy and `--reset` take it in place of a plain connect.
fn serve<A: TargetAccess + 'static>(
    mut probe: A,
    attach_under_reset: Option<&PartAttach<A>>,
    part: Part,
    image_base: u32,
    attach: bool,
    reset: bool,
    image: &[u8],
    lines: Vec<lamella_debug_device::LineRow>,
    names: Vec<(u32, u32, String)>,
    files: Vec<String>,
    entry: String,
    frames: Vec<u8>,
    locals: lamella_debug_device::program::LocalSections,
    unwind: lamella_debug_device::program::UnwindTables,
) -> std::io::Result<()> {
    if attach {
        if let Err(reason) = attach_to_part(&mut probe, part, reset, attach_under_reset) {
            return refuse(&reason);
        }
    } else {
        probe = match flash(probe, part, image, attach_under_reset) {
            Ok(probe) => probe,
            Err(reason) => return refuse(&reason),
        };
    }
    let backend = DeviceBackend::new(probe, lines, image_base, names, files, entry, frames)
        .with_locals(locals)
        .with_unwind_tables(unwind);

    let mut debugger = lamella_dap::Debugger::with_backend(Box::new(backend));
    lamella_dap::serve_polled(
        &mut debugger,
        std::io::BufReader::new(std::io::stdin()),
        &mut std::io::stdout().lock(),
    )
}

/// Reaches the part for a session that debugs the program already on it, and with `reset`, stops
/// that program at its entry.
///
/// The part is attached plainly unless `reset` asks for a reset and the probe has an attach of its
/// own for the part (`attach_under_reset`), which then is the reset as well: it leaves the core halted
/// at its reset vector. A plain attach that cannot read a part with such an attach is refused naming
/// `--reset`.
///
/// # Errors
/// Why the part could not be reached, in words for the user.
fn attach_to_part<A: TargetAccess>(
    probe: &mut A,
    part: Part,
    reset: bool,
    attach_under_reset: Option<&PartAttach<A>>,
) -> Result<(), String> {
    if reset && let Some(attach) = attach_under_reset {
        attach(probe).map_err(|error| {
            format!("could not attach to the part with its core held in reset: {error}")
        })?;
        eprintln!("reset: the part is halted at its entry, attached with its core held in reset");
        return Ok(());
    }
    probe.connect().map_err(|error| match attach_under_reset {
        Some(_) => format!(
            "could not attach to the part: {error}. --reset attaches this part with its core held \
             in reset, which reaches it when a plain attach cannot, and starts its program again \
             from its reset vector"
        ),
        None => format!("could not attach to the part: {error}"),
    })?;
    probe
        .init_mem()
        .map_err(|error| format!("could not reach the part's memory: {error}"))?;
    if reset {
        #[cfg(feature = "sam")]
        let outcome = if part == Part::Samd21 {
            use lamella_cmsis_dap_sam::Samd21Debug;
            probe.samd21_park()
        } else {
            probe.reset_and_halt()
        };
        #[cfg(not(feature = "sam"))]
        let outcome = {
            let _ = part;
            probe.reset_and_halt()
        };

        match outcome {
            Ok(()) => eprintln!("reset: the part is halted at its entry"),
            Err(error) => eprintln!(
                "reset: could not reset this part and hold it ({error:?}). The session is \
                 attached to the RUNNING program instead, so a stop reported as \"entry\" is \
                 wherever it happens to be, and a breakpoint on anything that has already run \
                 will not be hit."
            ),
        }
    }
    Ok(())
}

/// Flashes a raw image to the selected part, resets the part to run it, and hands the probe back.
///
/// Every algorithm reached here is a blanket impl over [`TargetAccess`], so this is genuinely one
/// function over two probe families and five parts -- the seam doing its job rather than a
/// coincidence. An STM32F7 or H7 is written through the write `lamella flash` takes for its family;
/// for the other parts only the erase geometry and the base address are per-part.
///
/// # Errors
/// Why the image is not on the part, for the user.
fn flash<A: TargetAccess>(
    mut target: A,
    part: Part,
    image: &[u8],
    attach_under_reset: Option<&PartAttach<A>>,
) -> Result<A, String> {
    let words: Vec<u32> = image
        .chunks(4)
        .map(|c| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(w)
        })
        .collect();
    match attach_under_reset {
        Some(attach) => attach(&mut target),
        None => target.connect(),
    }
    .map_err(deploy_step("attach to the part"))?;
    target
        .read_idcode()
        .map_err(deploy_step("read the debug port's IDCODE"))?;
    target
        .init_mem()
        .map_err(deploy_step("reach the part's memory"))?;
    #[cfg(feature = "st")]
    if let Some(family) = part.route_family() {
        return leave_held(write_through_the_route(
            target,
            family,
            part.flash_base(),
            image,
        )?);
    }
    target.halt().map_err(deploy_step("halt the core"))?;
    let base = part.flash_base();

    match part {
        #[cfg(feature = "sam")]
        Part::Samd21 => {
            return Err(
                "--part samd21 selects this part's RESET sequence and there is no SAM flash \
                 algorithm in this binary. Deploy the image with `lamella flash` and debug the \
                 running program with --attach."
                    .to_owned(),
            );
        }
        Part::Nrf51 => {
            let pages = (words.len() * 4).div_ceil(0x400);
            for page in 0..pages as u32 {
                target
                    .erase_flash_page(page * 0x400)
                    .map_err(deploy_step("erase a page"))?;
            }
            target
                .write_flash(base, &words)
                .map_err(deploy_step("write the flash"))?;
            read_back(&mut target, base, image)?;
        }
        #[cfg(feature = "st")]
        Part::Stm32F0 => {
            use lamella_cmsis_dap_stm32::{STM32F0_PAGE, Stm32F0Flash};
            target
                .f0_unlock_flash()
                .map_err(deploy_step("unlock the flash"))?;
            for page in 0..(image.len() as u32).div_ceil(STM32F0_PAGE) {
                target
                    .f0_erase_page(base + page * STM32F0_PAGE)
                    .map_err(deploy_step("erase a page"))?;
            }
            target
                .f0_program(base, image)
                .map_err(deploy_step("program the flash"))?;
            target
                .f0_lock_flash()
                .map_err(deploy_step("lock the flash"))?;
            read_back(&mut target, base, image)?;
        }
        #[cfg(feature = "st")]
        Part::Stm32F4 => {
            use lamella_cmsis_dap_stm32::{
                STM32F4_FLASH_SIZE_REG, STM32F4_SECTOR_SIZES, Stm32F4Flash, sectors_covering,
                stm32_flash_size_bytes,
            };
            let table: usize = STM32F4_SECTOR_SIZES.iter().sum();
            let fitted = stm32_flash_size_bytes(&mut target, STM32F4_FLASH_SIZE_REG)
                .map_err(deploy_step("read the part's flash size"))?
                as usize;
            if image.len() > table.min(fitted) {
                return Err(format!(
                    "this image is {} B, past the {} B an F4 deploy can erase on this part: its \
                     sector table reaches {table} B, and the part reports {} KB of flash fitted",
                    image.len(),
                    table.min(fitted),
                    fitted / 1024
                ));
            }
            target
                .unlock_flash()
                .map_err(deploy_step("unlock the flash"))?;
            for sector in 0..sectors_covering(image.len(), &STM32F4_SECTOR_SIZES) {
                target
                    .erase_sector(sector)
                    .map_err(deploy_step("erase a sector"))?;
            }
            target
                .program_words(base, &words)
                .map_err(deploy_step("program the flash"))?;
            target.lock_flash().map_err(deploy_step("lock the flash"))?;
            read_back(&mut target, base, image)?;
        }
        #[cfg(feature = "st")]
        Part::Stm32F7 | Part::Stm32H7 => {
            unreachable!("an STM32F7 or H7 is written through its route before the halt")
        }
    }
    leave_held(target)
}

/// Leaves `target` reset and HELD at its reset vector, so the session about to be served can arm
/// its breakpoints before the program runs.
///
/// # A DEPLOY THAT RESETS THE PART TO RUN CANNOT BE DEBUGGED FROM ITS START
///
/// A deploy that ends by letting the part RUN has executed the program's setup before the client
/// sends its first `setBreakpoints`, so a breakpoint on any line that runs early can never be hit
/// -- and nothing says so, because resolving the line and arming the comparator both succeed. The
/// breakpoint is reported verified, at the right file and the right address, and the code it names
/// has already gone past.
///
/// So the part is left STOPPED where a plain deploy would leave it running, and the session's
/// `configurationDone` is what starts the program. A client that connects and never configures
/// leaves a board that looks dark: that is what a debugger holding a target looks like, and it is
/// the cost of being able to stop on the first line.
///
/// A part whose debug port cannot be held across a reset is let go instead rather than failing the
/// deploy: a session against a running program is still a session, and the reason says which
/// breakpoints it cannot honor.
///
/// # Errors
/// Only if the part can neither be held nor let go, which is a part that is no longer answering.
fn leave_held<A: TargetAccess>(mut target: A) -> Result<A, String> {
    if let Err(error) = target.reset_and_halt() {
        eprintln!(
            "reset: this part could not be held at its entry after the deploy ({error:?}), so it \
             is running the new image instead. A breakpoint on a line that runs before the \
             session attaches will not be hit."
        );
        target
            .reset_and_run()
            .map_err(deploy_step("reset the part to run"))?;
    }
    Ok(target)
}

/// Writes `image` from `base` through the write `lamella flash` takes for `family` -- the part
/// identified by its `DEV_ID`, the write bounded by the flash the part reports, every bank the write
/// reaches unlocked, the watchdogs the family's plan names stopped while the core is halted, and every
/// byte read back -- and hands the probe back with the part reset to run.
///
/// # Errors
/// Why the route refused or failed, for the user: a part answering another `DEV_ID`, a flash map the
/// route does not write (an STM32F7 in dual-bank mode), an image past the part's flash, or a byte that
/// did not read back as written.
#[cfg(feature = "st")]
fn write_through_the_route<A: TargetAccess>(
    target: A,
    family: lamella_flash_routes::StFamily,
    base: u32,
    image: &[u8],
) -> Result<A, String> {
    let mut backend = lamella_flash_routes::backends::StProbe::new(target, family.plan());
    let written = lamella_flash_backend::flash(
        &mut backend,
        &lamella_flash_backend::Image { bytes: image, base },
        lamella_flash_backend::VerifyPolicy::ReadBack,
        &lamella_flash_backend::Allow::Any,
    );
    let target = backend.into_target();
    written.map_err(|why| format!("could not write the image: {why}"))?;
    Ok(target)
}

/// Reads `image` back from `base` and refuses at the first byte that is not what was written.
///
/// # A WRITE NOBODY READS BACK IS A DEPLOY THAT CANNOT FAIL
///
/// Every arm here programs through a controller that can decline: a page whose erase did not take
/// programs to whatever it still held, and the nRF51 write path deliberately drops the per-word
/// `NVMC READY` poll on the hypothesis that the controller stalls the bus -- its own documentation
/// names the read-back as the control for that hypothesis, because a wrong one loses words. None of
/// those failures reports itself. The deploy then resets the part, says it wrote the image, and the
/// program that does not run looks like a compiler or a debugger problem.
///
/// So this is not an extra check: it is the only thing standing between a flash that silently did
/// nothing and a session spent looking somewhere else. It lives in one function because it was
/// written into one arm of four, and an arm without it is indistinguishable from an arm with it right
/// up to the moment it matters.
///
/// # Errors
/// The first address whose byte differs, with what it holds and what was written.
fn read_back<A: TargetAccess>(target: &mut A, base: u32, image: &[u8]) -> Result<(), String> {
    let held: Vec<u8> = target
        .read_words(base, image.len().div_ceil(4))
        .map_err(deploy_step("read the image back"))?
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .collect();
    if let Some(offset) = image
        .iter()
        .zip(&held)
        .position(|(wrote, read)| wrote != read)
    {
        return Err(format!(
            "the image did not read back as written: {:#010x} holds {:#04x} where {:#04x} was \
             written",
            base + offset as u32,
            held[offset],
            image[offset]
        ));
    }
    Ok(())
}

/// The reason a deploy gives when the probe or the part refuses `step`.
fn deploy_step(step: &'static str) -> impl FnOnce(ProbeError) -> String {
    move |error| format!("could not {step}: {error}")
}

/// Builds the flashable image and composes its native offset -> source map and per-method names:
/// `build_debug`'s per-method line tables (native -> CIL, image-relative) joined to the Portable PDB
/// beside the assembly (CIL -> source line). The `target` selects the source `file` document (its
/// declaring method, or the entry point). Lines are 0 without a PDB (instruction-level).
fn source_lines(
    program: &str,
    target: Option<&(String, String)>,
) -> (Vec<(u32, u32)>, Vec<(u32, String)>, Vec<u8>, String, String) {
    let bytes = std::fs::read(program).expect("read the program assembly");
    let assembly = Assembly::read(&bytes).expect("parse metadata");
    let method = match target {
        Some((type_name, method_name)) => {
            let (namespace, name) = type_name.rsplit_once('.').unwrap_or(("", type_name));
            let type_def = assembly.find_type(namespace, name).expect("type not found");
            type_def
                .methods()
                .find(|m| m.name() == Some(method_name.as_str()))
                .expect("method not found")
        }
        None => {
            let token = assembly.image().entry_point_token();
            assert!(
                token != 0,
                "assembly has no entry point; pass <Type> <Method> explicitly"
            );
            let rid = token & 0x00ff_ffff;
            let type_def = assembly
                .type_defs()
                .find(|type_def| type_def.methods().any(|m| m.rid() == rid))
                .expect("entry point's declaring type not found");
            type_def
                .methods()
                .find(|m| m.rid() == rid)
                .expect("entry point method not found")
        }
    };
    let entry_rid = method.rid();

    let pdb_bytes = std::fs::read(std::path::Path::new(program).with_extension("pdb")).ok();
    let pdb = pdb_bytes.as_deref().and_then(|b| PortablePdb::read(b).ok());
    let file = pdb
        .as_ref()
        .and_then(|p| p.method_document(entry_rid))
        .map(|doc| {
            let path = std::path::Path::new(&doc);
            if path.is_absolute() {
                return doc;
            }
            std::path::Path::new(program)
                .parent()
                .map(|dir| dir.join(path).to_string_lossy().into_owned())
                .unwrap_or(doc)
        })
        .unwrap_or_default();

    let (image, method_debug) = build::build_debug(&bytes, "microbit").expect("build_debug");
    let mut lines: Vec<(u32, u32)> = Vec::new();
    let mut names: Vec<(u32, String)> = Vec::new();
    for (rid, offset, line_table) in &method_debug {
        names.push((*offset, name_of(&assembly, *rid)));
        for &(native, cil) in &line_table.0 {
            let line = pdb
                .as_ref()
                .and_then(|p| p.source_location(*rid, cil))
                .map_or(0, |sp| sp.start_line);
            lines.push((native, line));
        }
    }
    lines.sort_by_key(|&(native, _)| native);
    names.sort_by_key(|&(offset, _)| offset);
    let entry = name_of(&assembly, entry_rid);
    (lines, names, image, file, entry)
}

/// The `Type.Method` name for a MethodDef `rid`, or a synthetic `rid<N>` for the entry trampoline
/// and stub gaps (which have no real method).
fn name_of(assembly: &Assembly, rid: u32) -> String {
    for type_def in assembly.type_defs() {
        if let Some(method) = type_def.methods().find(|m| m.rid() == rid) {
            let method_name = method.name().unwrap_or("?");
            return match type_def.name() {
                Some(t) if t.namespace.is_empty() => format!("{}.{method_name}", t.name),
                Some(t) => format!("{}.{}.{method_name}", t.namespace, t.name),
                None => method_name.to_string(),
            };
        }
    }
    format!("rid{rid}")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "st")]
    use std::{cell::Cell, rc::Rc};

    /// How a deploy left the part.
    ///
    /// **THE LAST ONE, NOT WHETHER EACH HAPPENED.** An ST route resets the part to run on its way
    /// out and the hold then follows it, so a pair of sticky "was reset to run" / "was held" flags
    /// reads TRUE for both and cannot say which the session actually meets. What a session sees is
    /// the ending, so that is what the fakes record.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Ending {
        Running,
        HeldAtEntry,
    }

    fn parse(arguments: &[&str]) -> Result<Options, String> {
        Options::parse(arguments.iter().map(|argument| (*argument).to_owned()))
    }

    fn refusal(arguments: &[&str]) -> String {
        match parse(arguments) {
            Ok(_) => panic!("{arguments:?} should have been refused"),
            Err(reason) => reason,
        }
    }

    /// A part this build cannot program is told how to debug the board anyway.
    ///
    /// **THE SIX NAMES READ AS THE SUPPORTED-BOARD LIST.** A reader with a part outside them has
    /// no reason to try `--attach` and every reason to conclude the debugger is not for their
    /// board -- so the refusal that turns them away is the one place the other route has to be
    /// named. Both arms are asked because a reader reaches a different one depending on whether
    /// they typed a value at all, and the arm with no value is reached by simply writing `--part`.
    #[test]
    fn a_part_this_build_cannot_program_is_still_told_how_to_debug_the_board() {
        for arguments in [
            &["--part", "l0", "program.elf"][..],
            &["--part"][..],
        ] {
            let reason = refusal(arguments);
            assert!(
                reason.contains("--attach"),
                "{arguments:?} sends the reader away without the route that works: {reason}"
            );
            assert!(
                reason.contains("PROGRAM"),
                "{arguments:?} does not say what --part is for: {reason}"
            );
        }
    }

    /// **AN UNRECOGNIZED OPTION WAS READ AS A FILE NAME.** This parser ended
    /// `_ => positional.push(argument)`, and its positionals are `<program> [<Type> <Method>]
    /// [probe-serial]` disambiguated BY COUNT -- so a misspelled flag became the program to debug,
    /// and one typed alongside a program became a PROBE SERIAL, selecting which board gets written.
    /// This binary is launched by a VS Code configuration, where nobody reads the command line.
    #[test]
    fn an_unknown_option_is_refused_rather_than_read_as_a_positional() {
        let reason = refusal(&["--reset-halt", "prog.dll"]);
        assert!(
            reason.contains("--reset-halt"),
            "it names what was wrong: {reason}"
        );
        assert!(
            reason.contains("device-dap-server"),
            "and who is refusing: {reason}"
        );
    }

    /// A near-miss on a real flag must not become a probe serial either.
    #[test]
    fn a_misspelled_flag_beside_a_program_does_not_become_a_probe_serial() {
        let reason = refusal(&["prog.dll", "--prb", "cmsis"]);
        assert!(
            reason.contains("--prb"),
            "it names the misspelling: {reason}"
        );
    }

    /// **THE FLAG IS ACCEPTED AND REFUSED BY NAME, WHICH IS NOT THE SAME AS NOT TAKING IT.** The
    /// other two verbs offer `--class-library`, so it will be typed here; the linked tier has no
    /// debug map, so it cannot be served. Saying that is the answer. Falling through to the flat
    /// tier would serve a debug session for an image the user did not ask for.
    #[test]
    fn asking_for_the_class_library_is_refused_by_name_rather_than_served_flat() {
        let reason = refusal(&["--class-library", "prog.dll"]);
        assert!(
            reason.contains("--class-library"),
            "it names what was asked for: {reason}"
        );
        assert!(
            reason.contains("line table"),
            "and what is missing: {reason}"
        );
    }

    #[test]
    fn a_product_id_that_does_not_parse_is_refused_rather_than_replaced() {
        let reason = refusal(&["--pid", "0xZZ", "program.elf"]);
        assert!(
            reason.contains("--pid") && reason.contains("\"0xZZ\""),
            "{reason}"
        );
    }

    #[test]
    fn a_product_id_with_no_st_link_to_apply_it_to_is_refused() {
        let reason = refusal(&["--probe", "cmsis", "--pid", "0x374e", "program.elf"]);
        assert!(reason.contains("--probe stlink"), "{reason}");
    }

    #[test]
    fn a_probe_kind_this_server_does_not_know_is_refused() {
        let reason = refusal(&["--probe", "jlink", "program.elf"]);
        assert!(reason.contains("\"jlink\""), "{reason}");
    }

    #[test]
    fn a_command_line_with_no_program_is_refused_with_the_usage() {
        assert_eq!(refusal(&["--attach"]), USAGE);
    }

    #[cfg(not(feature = "st"))]
    #[test]
    fn an_st_link_or_an_stm32_part_on_a_build_without_st_names_the_feature_that_builds_one() {
        for arguments in [
            &["--probe", "stlink", "program.elf"][..],
            &["--part", "f7", "--attach", "program.elf"][..],
        ] {
            let reason = refusal(arguments);
            assert!(
                reason.contains("`st` feature") && reason.contains("lamella-debug-server"),
                "{arguments:?}: {reason}"
            );
        }
    }

    #[cfg(not(feature = "sam"))]
    #[test]
    fn a_sam_part_on_a_build_without_sam_names_the_feature_that_builds_one() {
        let reason = refusal(&["--part", "samd21", "--attach", "program.elf"]);
        assert!(
            reason.contains("`sam` feature") && reason.contains("lamella-debug-server"),
            "{reason}"
        );
    }

    #[cfg(feature = "st")]
    #[test]
    fn an_st_link_session_opens_the_product_id_it_names_and_v2_1_when_it_names_none() {
        let named =
            parse(&["--probe", "stlink", "--pid", "374e", "program.elf"]).expect("accepted");
        assert!(named.probe == ProbeKind::StLink);
        assert_eq!(named.stlink_pid, 0x374e);
        let unnamed = parse(&["--probe", "stlink", "program.elf"]).expect("accepted");
        assert_eq!(unnamed.stlink_pid, lamella_stlink::product_id::V2_1);
    }

    /// A part whose firmware sleeps out of a plain attach's reach: its connect is refused, as an
    /// ST-LINK's is when CPUID reads back as a word no Cortex-M holds, until the part answers. An
    /// access a session's attach must not make panics.
    #[derive(Default)]
    struct SleepingPart {
        /// Whether a plain attach reaches the part: its core is running, or an attach under reset
        /// already set what keeps it reachable.
        awake: bool,
        /// Whether the part's own attach under reset ran.
        attached_under_reset: bool,
    }

    /// The part's own attach under reset, as a probe that has one hands it to [`attach_to_part`].
    fn attach_under_reset(part: &mut SleepingPart) -> Result<(), ProbeError> {
        part.attached_under_reset = true;
        part.awake = true;
        Ok(())
    }

    impl TargetAccess for SleepingPart {
        fn connect(&mut self) -> Result<(), ProbeError> {
            if self.awake {
                return Ok(());
            }
            Err(ProbeError::Protocol(
                "the probe reported a successful read of CPUID (0xe000ed00) that returned \
                 0x0000001a, a word no Cortex-M holds"
                    .to_owned(),
            ))
        }
        fn init_mem(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            unreachable!("a part attached under reset is not reset a second time")
        }
        fn read_idcode(&mut self) -> Result<u32, ProbeError> {
            unreachable!("an attach does not read the debug port's id")
        }
        fn read_word(&mut self, _address: u32) -> Result<u32, ProbeError> {
            unreachable!("an attach reads no memory of its own")
        }
        fn write_word(&mut self, _address: u32, _value: u32) -> Result<(), ProbeError> {
            unreachable!("an attach writes no memory of its own")
        }
        fn read_words_into(&mut self, _address: u32, _out: &mut [u32]) -> Result<(), ProbeError> {
            unreachable!("an attach reads no memory of its own")
        }
        fn write_words(&mut self, _address: u32, _words: &[u32]) -> Result<(), ProbeError> {
            unreachable!("an attach writes no memory of its own")
        }
        fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
            unreachable!("an attach reads no memory of its own")
        }
        fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
            unreachable!("an attach writes no memory of its own")
        }
        fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
            unreachable!("an attach reads no memory of its own")
        }
        fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
            unreachable!("an attach writes no memory of its own")
        }
        fn halt(&mut self) -> Result<(), ProbeError> {
            unreachable!("the session's launch halts the core, not its attach")
        }
        fn resume(&mut self) -> Result<(), ProbeError> {
            unreachable!("an attach does not run the core")
        }
        fn step(&mut self) -> Result<(), ProbeError> {
            unreachable!("an attach does not run the core")
        }
        fn is_halted(&mut self) -> Result<bool, ProbeError> {
            unreachable!("an attach does not read the run state")
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            unreachable!("an attach does not wait for a halt of its own")
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            unreachable!("an attach does not reset the part and let it run")
        }
        fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
            unreachable!("the part's own attach drives the reset line")
        }
        fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
            unreachable!("an attach reads no register")
        }
        fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
            unreachable!("an attach writes no register")
        }
        fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("the part's own attach arms the vector catch")
        }
        fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("the part's own attach disarms the vector catch")
        }
        fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
            unreachable!("an attach arms no breakpoint")
        }
        fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
            unreachable!("an attach arms no breakpoint")
        }
        fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
            unreachable!("an attach arms no breakpoint")
        }
        fn call_target(
            &mut self,
            _address: u32,
            _args: &[u32],
            _frame: &lamella_probe_core::CallFrame,
        ) -> Result<u32, ProbeError> {
            unreachable!("an attach calls nothing on the part")
        }
    }

    #[cfg(feature = "st")]
    #[test]
    fn an_f7_is_attached_under_reset_with_the_low_power_debug_bits_its_manual_names() {
        use lamella_cmsis_dap_stm32::{STM32F7_DBGMCU_CR, STM32F7_DBGMCU_CR_LOW_POWER_DEBUG};
        assert_eq!(
            Part::Stm32F7.low_power_debug(),
            Some(lamella_stlink::LowPowerDebug {
                register: STM32F7_DBGMCU_CR,
                bits: STM32F7_DBGMCU_CR_LOW_POWER_DEBUG,
            })
        );
        assert_eq!(
            Part::Stm32F4.low_power_debug(),
            None,
            "an F4 is attached plainly"
        );
    }

    #[test]
    fn with_reset_a_part_is_attached_through_its_own_attach_under_reset() {
        let mut part = SleepingPart::default();
        let attach: &PartAttach<SleepingPart> = &attach_under_reset;
        assert_eq!(
            attach_to_part(&mut part, Part::Nrf51, true, Some(attach)),
            Ok(())
        );
        assert!(part.attached_under_reset);
    }

    #[test]
    fn without_reset_a_part_a_plain_attach_cannot_read_is_refused_naming_reset() {
        let mut part = SleepingPart::default();
        let attach: &PartAttach<SleepingPart> = &attach_under_reset;
        let Err(reason) = attach_to_part(&mut part, Part::Nrf51, false, Some(attach)) else {
            panic!("a part that refuses a plain attach must not be reported attached");
        };
        assert!(
            reason.contains("0x0000001a") && reason.contains("--reset"),
            "{reason}"
        );
        assert!(
            !part.attached_under_reset,
            "without --reset the part is not reset"
        );
    }

    #[test]
    fn without_reset_a_part_that_answers_is_attached_plainly() {
        let mut part = SleepingPart {
            awake: true,
            ..SleepingPart::default()
        };
        let attach: &PartAttach<SleepingPart> = &attach_under_reset;
        assert_eq!(
            attach_to_part(&mut part, Part::Nrf51, false, Some(attach)),
            Ok(())
        );
        assert!(
            !part.attached_under_reset,
            "an attach without --reset leaves the program running where it is"
        );
    }

    /// The nRF51's non-volatile memory controller, and the flash geometry a page erase needs.
    ///
    /// Spelled out here rather than imported from `lamella-cmsis-dap-nrf`, where they are private
    /// anyway: a fixture that shares its constants with the code under test agrees with a wrong
    /// address as readily as a right one. These are the nRF51 Series Reference Manual's NVMC
    /// registers, which is also where that crate took them from.
    mod nvmc {
        /// `READY`: bit 0 set means the controller is idle.
        pub const READY: u32 = 0x4001_E400;
        /// `CONFIG`: the write/erase enable.
        pub const CONFIG: u32 = 0x4001_E504;
        /// `ERASEPAGE`: the address of the page to erase.
        pub const ERASEPAGE: u32 = 0x4001_E508;
        /// `CONFIG` = 1, writes enabled.
        pub const WEN: u32 = 1;
        /// `CONFIG` = 2, erase enabled.
        pub const EEN: u32 = 2;
        /// The part's page, and its whole main flash block.
        pub const PAGE: u32 = 1024;
        pub const FLASH: usize = 256 * 1024;
    }

    /// An nRF51 whose flash is programmed through its NVMC: a page erases to `0xFF` only while
    /// `CONFIG` is `EEN`, a cell takes a write only while `CONFIG` is `WEN`, and a write clears bits
    /// without ever setting one -- which is what flash does and what makes an unerased page visible.
    ///
    /// `drops_writes_from` is the failure this part exists to model: the nRF51 write path drops the
    /// per-word `NVMC READY` poll on the hypothesis that the controller stalls the bus, so if that
    /// hypothesis is ever wrong the words after some point are simply LOST -- accepted by the probe,
    /// never stored. Nothing about the write reports it. Only reading the image back does.
    #[derive(Default)]
    struct Nrf51Part {
        flash: Vec<u8>,
        config: u32,
        /// The word index, counted from the start of flash, from which a write is silently dropped.
        drops_writes_from: Option<usize>,
        /// How the deploy LEFT the part, which is what a session about to be served sees.
        ending: Option<Ending>,
        /// Makes the hold fail, for the part whose debug port cannot be held across a reset.
        hold_fails: bool,
    }

    impl Nrf51Part {
        /// A part holding an older firmware, so an unerased page is distinguishable from an erased
        /// one and from the image about to be written.
        fn holding_old_firmware() -> Self {
            Nrf51Part {
                flash: (0..nvmc::FLASH).map(|index| (index % 97) as u8).collect(),
                ..Nrf51Part::default()
            }
        }

        /// The offset in flash `address` addresses, for a whole word inside the block.
        fn cell(&self, address: u32) -> Option<usize> {
            let at = address as usize;
            (at + 4 <= self.flash.len()).then_some(at)
        }
    }

    impl TargetAccess for Nrf51Part {
        fn connect(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_idcode(&mut self) -> Result<u32, ProbeError> {
            Ok(0x0BB1_1477)
        }
        fn init_mem(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn halt(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            self.ending = Some(Ending::Running);
            Ok(())
        }
        fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
            if let Some(at) = self.cell(address) {
                let word = &self.flash[at..at + 4];
                return Ok(u32::from_le_bytes([word[0], word[1], word[2], word[3]]));
            }
            Ok(u32::from(address == nvmc::READY))
        }
        fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
            if let Some(at) = self.cell(address) {
                if self.config == nvmc::WEN
                    && self.drops_writes_from.is_none_or(|from| at / 4 < from)
                {
                    for (cell, byte) in self.flash[at..at + 4].iter_mut().zip(value.to_le_bytes()) {
                        *cell &= byte;
                    }
                }
                return Ok(());
            }
            match address {
                nvmc::CONFIG => self.config = value,
                nvmc::ERASEPAGE if self.config == nvmc::EEN => {
                    let start = (value & !(nvmc::PAGE - 1)) as usize;
                    let end = (start + nvmc::PAGE as usize).min(self.flash.len());
                    if start < end {
                        self.flash[start..end].fill(0xFF);
                    }
                }
                _ => {}
            }
            Ok(())
        }
        fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
            for (index, &word) in words.iter().enumerate() {
                self.write_word(address + (index * 4) as u32, word)?;
            }
            Ok(())
        }
        fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
            for (index, slot) in out.iter_mut().enumerate() {
                *slot = self.read_word(address + (index * 4) as u32)?;
            }
            Ok(())
        }

        // An access an nRF51 deploy has no reason to make panics, so this fixture states the
        // dependency surface rather than quietly absorbing a call nobody meant to write.
        fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
            unreachable!("an nRF51 deploy programs words")
        }
        fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
            unreachable!("an nRF51 deploy programs words")
        }
        fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
            unreachable!("an nRF51 deploy programs words")
        }
        fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
            unreachable!("an nRF51 deploy programs words")
        }
        fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
            unreachable!("a deploy reads no core register")
        }
        fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
            unreachable!("a deploy writes no core register")
        }
        fn resume(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy resets the part rather than resuming it")
        }
        fn step(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy does not step")
        }
        fn is_halted(&mut self) -> Result<bool, ProbeError> {
            unreachable!("a deploy halts and does not ask again")
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy halts and does not ask again")
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            if self.hold_fails {
                return Err(ProbeError::Device("reset catch"));
            }
            self.ending = Some(Ending::HeldAtEntry);
            Ok(())
        }
        fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
            unreachable!("an nRF51 deploy does not drive the reset line itself")
        }
        fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("an nRF51 deploy does not catch the reset")
        }
        fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("an nRF51 deploy does not catch the reset")
        }
        fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no breakpoint")
        }
        fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no breakpoint")
        }
        fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no breakpoint")
        }
        fn call_target(
            &mut self,
            _address: u32,
            _args: &[u32],
            _frame: &lamella_probe_core::CallFrame,
        ) -> Result<u32, ProbeError> {
            unreachable!("an nRF51 is programmed through its NVMC, not by a loader on the part")
        }
    }

    /// An nRF51 deploy reads its image back, so a write the controller did not take is refused
    /// rather than reported as a successful deploy and reset to run.
    ///
    /// This arm erased and wrote and checked nothing, while the F4 arm beside it read every byte
    /// back -- so the same silent failure was caught on one part and reported as success on another.
    #[test]
    fn an_nrf51_deploy_that_does_not_read_back_as_written_is_refused_naming_where() {
        let image = image_of(4096);
        let mut part = Nrf51Part::holding_old_firmware();
        part.drops_writes_from = Some(350);
        let Err(reason) = flash(part, Part::Nrf51, &image, None) else {
            panic!("a write the NVMC dropped left the old firmware behind, and a deploy hid that");
        };
        assert!(
            reason.contains("0x00000578"),
            "the first address that differs is named: {reason}"
        );
    }

    /// The control: the same part with nothing dropped takes the whole image and is left running it.
    #[test]
    fn an_nrf51_deploy_that_takes_puts_every_byte_on_the_part() {
        let image = image_of(4096);
        let part = flash(Nrf51Part::holding_old_firmware(), Part::Nrf51, &image, None)
            .unwrap_or_else(|reason| panic!("an image this size fits an nRF51: {reason}"));
        assert_eq!(
            &part.flash[..image.len()],
            &image[..],
            "every byte of the image is on the part"
        );
        assert_eq!(
            part.ending,
            Some(Ending::HeldAtEntry),
            "the part is left held at its entry, so the session can arm breakpoints before it runs"
        );
    }

    /// **A PROBE NAME IS NOT THIS PROGRAM'S TO TRUST.** The product string and the serial come from
    /// the operating system, and the consumer is a debug client parsing JSON to decide which board
    /// to write -- so a quote or a backslash in either must not be able to end the string early.
    #[test]
    fn a_probe_name_carrying_json_punctuation_is_escaped_rather_than_ending_the_string() {
        assert_eq!(json_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(json_string(r"a\b"), r#""a\\b""#);
        assert_eq!(json_string("a\tb"), r#""a\u0009b""#);
        assert_eq!(
            json_string("BBC micro:bit CMSIS-DAP"),
            "\"BBC micro:bit CMSIS-DAP\""
        );
    }

    /// A part that cannot be held is let go, and the deploy still succeeds.
    ///
    /// The hold is what makes a breakpoint on an early line hittable, and a part whose debug port
    /// cannot be caught across a reset cannot offer it -- a SAM D21 over an EDBG answers
    /// `Timeout("reset catch")`. But a session against a running program is still a session and is
    /// worth far more than a refused deploy, so the part is let go, the image is left on it, and
    /// the reason says which breakpoints cannot be honored.
    #[test]
    fn a_part_that_cannot_be_held_is_let_go_rather_than_failing_the_deploy() {
        let image = image_of(4096);
        let part = Nrf51Part {
            hold_fails: true,
            ..Nrf51Part::holding_old_firmware()
        };
        let part = flash(part, Part::Nrf51, &image, None).unwrap_or_else(|reason| {
            panic!("a hold that cannot be caught is not a failed deploy: {reason}")
        });
        assert_eq!(
            part.ending,
            Some(Ending::Running),
            "the part is let go, so the program it was given at least runs"
        );
        assert_eq!(
            &part.flash[..image.len()],
            &image[..],
            "and the image is still on it"
        );
    }

    /// One kilobyte, as the manuals size sectors and flash.
    #[cfg(feature = "st")]
    const KB: usize = 1024;

    /// The flash register block the STM32F4 and F7 share, and the bits of it a write uses, as
    /// `lamella-cmsis-dap-stm32` drives them.
    #[cfg(feature = "st")]
    mod f4_f7 {
        pub const FLASH_KEYR: u32 = 0x4002_3C04;
        pub const FLASH_SR: u32 = 0x4002_3C0C;
        pub const FLASH_CR: u32 = 0x4002_3C10;
        pub const FLASH_OPTCR: u32 = 0x4002_3C14;
        pub const DBGMCU_IDCODE: u32 = 0xE004_2000;
        pub const DBGMCU_APB1_FZ: u32 = 0xE004_2008;
        pub const FLASH_BASE: u32 = 0x0800_0000;
        pub const CR_PG: u32 = 1 << 0;
        pub const CR_SER: u32 = 1 << 1;
        pub const CR_SNB_SHIFT: u32 = 3;
        pub const CR_STRT: u32 = 1 << 16;
        pub const CR_LOCK: u32 = 1 << 31;
        pub const KEY1: u32 = 0x4567_0123;
        pub const KEY2: u32 = 0xCDEF_89AB;
    }

    /// An STM32F4 or F7 whose flash controller erases a sector at a time to `0xFF` and programs by
    /// clearing bits, holding what its last firmware left, with the identity, option and flash-size
    /// registers a write reads. An access a deploy has no reason to make panics.
    #[cfg(feature = "st")]
    struct FlashPart {
        /// The flash from `0x0800_0000`, as long as the part reports fitted.
        flash: Vec<u8>,
        /// The sector sizes the part's manual gives it, in order from sector 0.
        sectors: &'static [usize],
        /// `DBGMCU_IDCODE`.
        idcode: u32,
        /// `FLASH_OPTCR`.
        optcr: u32,
        /// Where the word holding the flash-size register is, and the word read there.
        size_word: (u32, u32),
        /// `FLASH_CR`, whose `LOCK` stays set until the two keys are written in order.
        control: u32,
        /// Whether the last key written was the first key.
        first_key: bool,
        /// A sector whose erase leaves its cells as they were.
        erase_does_not_take: Option<u32>,
        /// `DBGMCU_APB1_FZ`.
        watchdogs: u32,
        /// How many sector erases were started, shared with the test because a deploy takes the part.
        erases: Rc<Cell<u32>>,
        /// How the deploy LEFT the part, which is what a session about to be served sees.
        ending: Option<Ending>,
    }

    #[cfg(feature = "st")]
    impl FlashPart {
        /// An STM32F769 in single-bank mode: `DEV_ID` 0x451 and `nDBANK` set, 2048 KB fitted, and
        /// that mode's twelve sectors -- four of 32 KB, one of 128 KB, seven of 256 KB.
        fn f769() -> Self {
            const SINGLE_BANK: [usize; 12] = [
                32 * KB,
                32 * KB,
                32 * KB,
                32 * KB,
                128 * KB,
                256 * KB,
                256 * KB,
                256 * KB,
                256 * KB,
                256 * KB,
                256 * KB,
                256 * KB,
            ];
            Self::holding_old_firmware(&SINGLE_BANK, 0x1000_0451, 1 << 29, 0x1FF0_F440, 2048)
        }

        /// An STM32F4 with `fitted_kb` of flash, whose first megabyte is four sectors of 16 KB, one of
        /// 64 KB and seven of 128 KB.
        fn f4(fitted_kb: u32) -> Self {
            const FIRST_MEGABYTE: [usize; 12] = [
                16 * KB,
                16 * KB,
                16 * KB,
                16 * KB,
                64 * KB,
                128 * KB,
                128 * KB,
                128 * KB,
                128 * KB,
                128 * KB,
                128 * KB,
                128 * KB,
            ];
            Self::holding_old_firmware(&FIRST_MEGABYTE, 0x1000_0413, 0, 0x1FFF_7A20, fitted_kb)
        }

        /// A part whose every flash cell holds zero, and whose flash-size register -- the upper half
        /// of the word at `size_at` -- reads `fitted_kb`.
        fn holding_old_firmware(
            sectors: &'static [usize],
            idcode: u32,
            optcr: u32,
            size_at: u32,
            fitted_kb: u32,
        ) -> Self {
            FlashPart {
                flash: vec![0; fitted_kb as usize * KB],
                sectors,
                idcode,
                optcr,
                size_word: (size_at, fitted_kb << 16),
                control: f4_f7::CR_LOCK,
                first_key: false,
                erase_does_not_take: None,
                watchdogs: 0,
                erases: Rc::new(Cell::new(0)),
                ending: None,
            }
        }

        /// Where `address` falls in the flash array, when all `len` bytes from it do.
        fn cells(&self, address: u32, len: usize) -> Option<usize> {
            let offset = address.checked_sub(f4_f7::FLASH_BASE)? as usize;
            (offset + len <= self.flash.len()).then_some(offset)
        }

        fn erase_sector(&mut self, sector: u32) {
            self.erases.set(self.erases.get() + 1);
            if self.erase_does_not_take == Some(sector) {
                return;
            }
            let index = sector as usize;
            if index >= self.sectors.len() {
                return;
            }
            let start: usize = self.sectors[..index].iter().sum();
            let end = (start + self.sectors[index]).min(self.flash.len());
            if start < end {
                self.flash[start..end].fill(0xFF);
            }
        }
    }

    #[cfg(feature = "st")]
    impl TargetAccess for FlashPart {
        fn connect(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_idcode(&mut self) -> Result<u32, ProbeError> {
            Ok(0x5BA0_2477)
        }
        fn init_mem(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
            if let Some(at) = self.cells(address, 4) {
                let word = &self.flash[at..at + 4];
                return Ok(u32::from_le_bytes([word[0], word[1], word[2], word[3]]));
            }
            Ok(match address {
                f4_f7::FLASH_SR => 0,
                f4_f7::FLASH_CR => self.control,
                f4_f7::FLASH_OPTCR => self.optcr,
                f4_f7::DBGMCU_IDCODE => self.idcode,
                f4_f7::DBGMCU_APB1_FZ => self.watchdogs,
                address if address == self.size_word.0 => self.size_word.1,
                _ => 0,
            })
        }
        fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
            if let Some(at) = self.cells(address, 4) {
                if self.control & f4_f7::CR_PG != 0 {
                    for (cell, byte) in self.flash[at..at + 4].iter_mut().zip(value.to_le_bytes()) {
                        *cell &= byte;
                    }
                }
                return Ok(());
            }
            match address {
                f4_f7::FLASH_KEYR => {
                    if value == f4_f7::KEY2 && self.first_key {
                        self.control &= !f4_f7::CR_LOCK;
                    }
                    self.first_key = value == f4_f7::KEY1;
                }
                f4_f7::FLASH_CR if self.control & f4_f7::CR_LOCK == 0 => {
                    let erase = f4_f7::CR_SER | f4_f7::CR_STRT;
                    if value & erase == erase {
                        self.erase_sector((value >> f4_f7::CR_SNB_SHIFT) & 0x1F);
                    }
                    self.control = value & !f4_f7::CR_STRT;
                }
                f4_f7::DBGMCU_APB1_FZ => self.watchdogs = value,
                _ => {}
            }
            Ok(())
        }
        fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
            for (index, slot) in out.iter_mut().enumerate() {
                *slot = self.read_word(address + 4 * index as u32)?;
            }
            Ok(())
        }
        fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
            for (index, word) in words.iter().enumerate() {
                self.write_word(address + 4 * index as u32, *word)?;
            }
            Ok(())
        }
        fn halt(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            self.ending = Some(Ending::Running);
            Ok(())
        }
        fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
            unreachable!("a deploy reads words")
        }
        fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
            unreachable!("a deploy writes words")
        }
        fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
            unreachable!("a deploy reads words")
        }
        fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
            unreachable!("a deploy writes words")
        }
        fn resume(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy ends with a reset to run, not a resume")
        }
        fn step(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy does not step")
        }
        fn is_halted(&mut self) -> Result<bool, ProbeError> {
            unreachable!("a deploy does not read the run state")
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy does not wait for a halt")
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            self.ending = Some(Ending::HeldAtEntry);
            Ok(())
        }
        fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
            unreachable!("a deploy drives no reset line of its own")
        }
        fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
            unreachable!("a deploy reads no core register")
        }
        fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
            unreachable!("a deploy writes no core register")
        }
        fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no vector catch")
        }
        fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no vector catch")
        }
        fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no breakpoint")
        }
        fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no breakpoint")
        }
        fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no breakpoint")
        }
        fn call_target(
            &mut self,
            _address: u32,
            _args: &[u32],
            _frame: &lamella_probe_core::CallFrame,
        ) -> Result<u32, ProbeError> {
            unreachable!("these parts are programmed through their controller, not a loader")
        }
    }

    /// An image of `len` bytes with no zero byte in it, so a cell programmed without an erase first
    /// cannot match it.
    ///
    /// **NOT GATED, BECAUSE THE nRF51 DEPLOY TESTS USE IT TOO.** It was `#[cfg(feature = "st")]`
    /// while three nRF51 tests called it, so a build with `st` off did not compile at all -- and
    /// nothing noticed, because that configuration had never been built. The helper is arithmetic
    /// and has no part-specific anything in it.
    fn image_of(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8 | 1).collect()
    }

    /// Where a part's `flash` first differs from `image`, or `None` when all of it is there.
    #[cfg(feature = "st")]
    fn first_difference(flash: &[u8], image: &[u8]) -> Option<usize> {
        flash
            .iter()
            .zip(image)
            .position(|(cell, byte)| cell != byte)
    }

    #[cfg(feature = "st")]
    #[test]
    fn an_f7_deploy_past_the_first_megabyte_puts_the_whole_image_on_a_two_megabyte_part() {
        let image = image_of(1536 * KB);
        let part = flash(FlashPart::f769(), Part::Stm32F7, &image, None)
            .unwrap_or_else(|reason| panic!("a 1.5 MB image fits a 2 MB F769: {reason}"));
        assert_eq!(
            first_difference(&part.flash, &image),
            None,
            "every byte of the image is on the part, the ones past the first megabyte included"
        );
        assert_eq!(
            part.ending,
            Some(Ending::HeldAtEntry),
            "the part is left held at its entry, so the session can arm breakpoints before it runs"
        );
    }

    #[cfg(feature = "st")]
    #[test]
    fn an_f7_deploy_to_a_part_answering_another_device_id_is_refused_before_any_erase() {
        let image = image_of(64 * KB);
        let mut part = FlashPart::f769();
        part.idcode = 0x1000_0413;
        let erases = Rc::clone(&part.erases);
        let Err(reason) = flash(part, Part::Stm32F7, &image, None) else {
            panic!("a part answering DEV_ID 0x413 is no STM32F7, and writing it is not a deploy");
        };
        assert!(
            reason.contains("0x413"),
            "the id the part answered is named: {reason}"
        );
        assert_eq!(
            erases.get(),
            0,
            "nothing is erased on a part that is not the one named"
        );
    }

    #[cfg(feature = "st")]
    #[test]
    fn an_f4_deploy_past_what_its_sector_table_reaches_is_refused_before_any_erase() {
        let image = image_of(1536 * KB);
        let part = FlashPart::f4(2048);
        let erases = Rc::clone(&part.erases);
        let Err(reason) = flash(part, Part::Stm32F4, &image, None) else {
            panic!("an image past the sectors this deploy erases by would be programmed unerased");
        };
        assert_eq!(
            erases.get(),
            0,
            "refused before anything is erased: {reason}"
        );
    }

    #[cfg(feature = "st")]
    #[test]
    fn an_f4_deploy_that_does_not_read_back_as_written_is_refused_naming_where() {
        let image = image_of(300 * KB);
        let mut part = FlashPart::f4(1024);
        part.erase_does_not_take = Some(5);
        let Err(reason) = flash(part, Part::Stm32F4, &image, None) else {
            panic!(
                "a sector whose erase did not take holds the wrong bytes, and a deploy hid that"
            );
        };
        assert!(
            reason.contains("0x08020000"),
            "the first address that differs is named: {reason}"
        );
    }

    /// The STM32H7 flash register blocks, one per bank, and the bits of them a write uses, as
    /// `lamella-cmsis-dap-stm32` drives them.
    #[cfg(feature = "st")]
    mod h7 {
        /// Bank 1's register block; bank 2's is the same layout `BANK_STRIDE` higher.
        pub const REGISTERS: u32 = 0x5200_2000;
        pub const BANK_STRIDE: u32 = 0x100;
        pub const KEYR: u32 = 0x04;
        pub const CR: u32 = 0x0C;
        pub const DBGMCU_IDC: u32 = 0x5C00_1000;
        pub const FLASH_SIZE: u32 = 0x1FF1_E880;
        pub const FLASH_BASE: u32 = 0x0800_0000;
        pub const BANK: usize = 1024 * super::KB;
        pub const SECTOR: usize = 128 * super::KB;
        pub const CR_LOCK: u32 = 1 << 0;
        pub const CR_PG: u32 = 1 << 1;
        pub const CR_SER: u32 = 1 << 2;
        pub const CR_START: u32 = 1 << 7;
        pub const CR_SNB_SHIFT: u32 = 8;
        pub const KEY1: u32 = 0x4567_0123;
        pub const KEY2: u32 = 0xCDEF_89AB;
    }

    /// An STM32H7 with two banks of 1 MB, each behind a register block of its own: a bank's control
    /// register stays locked until its own two keys are written in order, a sector erases to `0xFF`
    /// only through the control register of the bank that holds it, and a cell programs by clearing
    /// bits only while that bank's `PG` is set. Its status registers always read idle with no error,
    /// and its flash holds what its last firmware left. An access a deploy has no reason to make
    /// panics.
    #[cfg(feature = "st")]
    struct H7Part {
        /// The flash from `0x0800_0000`, bank 1 then bank 2.
        flash: Vec<u8>,
        /// `DBGMCU_IDC`.
        idc: u32,
        /// Each bank's `FLASH_CR`.
        control: [u32; 2],
        /// Whether the last key written to each bank was the first key.
        first_key: [bool; 2],
        /// The first address of a sector whose erase leaves its cells as they were.
        erase_does_not_take: Option<u32>,
        /// How many sector erases were started, shared with the test because a deploy takes the part.
        erases: Rc<Cell<u32>>,
        /// How the deploy LEFT the part, which is what a session about to be served sees.
        ending: Option<Ending>,
    }

    #[cfg(feature = "st")]
    impl H7Part {
        /// An STM32H747 with 2048 KB of flash, answering `DEV_ID` 0x450, every flash cell holding zero.
        fn h747() -> Self {
            H7Part {
                flash: vec![0; 2 * h7::BANK],
                idc: 0x2003_6450,
                control: [h7::CR_LOCK; 2],
                first_key: [false; 2],
                erase_does_not_take: None,
                erases: Rc::new(Cell::new(0)),
                ending: None,
            }
        }

        /// Where `address` falls in the flash array, when all `len` bytes from it do.
        fn cells(&self, address: u32, len: usize) -> Option<usize> {
            let offset = address.checked_sub(h7::FLASH_BASE)? as usize;
            (offset + len <= self.flash.len()).then_some(offset)
        }

        /// Which bank's register block `address` is in, and where in that block.
        fn register(address: u32) -> Option<(usize, u32)> {
            let offset = address.checked_sub(h7::REGISTERS)?;
            let bank = (offset / h7::BANK_STRIDE) as usize;
            (bank < 2).then_some((bank, offset % h7::BANK_STRIDE))
        }

        fn erase_sector(&mut self, bank: usize, sector: u32) {
            self.erases.set(self.erases.get() + 1);
            let start = bank * h7::BANK + sector as usize * h7::SECTOR;
            if self.erase_does_not_take == Some(h7::FLASH_BASE + start as u32) {
                return;
            }
            self.flash[start..start + h7::SECTOR].fill(0xFF);
        }
    }

    #[cfg(feature = "st")]
    impl TargetAccess for H7Part {
        fn connect(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_idcode(&mut self) -> Result<u32, ProbeError> {
            Ok(0x5BA0_2477)
        }
        fn init_mem(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
            if let Some(at) = self.cells(address, 4) {
                let word = &self.flash[at..at + 4];
                return Ok(u32::from_le_bytes([word[0], word[1], word[2], word[3]]));
            }
            if let Some((bank, h7::CR)) = Self::register(address) {
                return Ok(self.control[bank]);
            }
            Ok(match address {
                h7::DBGMCU_IDC => self.idc,
                h7::FLASH_SIZE => (self.flash.len() / KB) as u32,
                _ => 0,
            })
        }
        fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
            if let Some(at) = self.cells(address, 4) {
                if self.control[at / h7::BANK] & h7::CR_PG != 0 {
                    for (cell, byte) in self.flash[at..at + 4].iter_mut().zip(value.to_le_bytes()) {
                        *cell &= byte;
                    }
                }
                return Ok(());
            }
            match Self::register(address) {
                Some((bank, h7::KEYR)) => {
                    if value == h7::KEY2 && self.first_key[bank] {
                        self.control[bank] &= !h7::CR_LOCK;
                    }
                    self.first_key[bank] = value == h7::KEY1;
                }
                Some((bank, h7::CR)) if self.control[bank] & h7::CR_LOCK == 0 => {
                    let erase = h7::CR_SER | h7::CR_START;
                    if value & erase == erase {
                        self.erase_sector(bank, (value >> h7::CR_SNB_SHIFT) & 0b111);
                    }
                    self.control[bank] = value & !h7::CR_START;
                }
                _ => {}
            }
            Ok(())
        }
        fn read_words_into(&mut self, address: u32, out: &mut [u32]) -> Result<(), ProbeError> {
            for (index, slot) in out.iter_mut().enumerate() {
                *slot = self.read_word(address + 4 * index as u32)?;
            }
            Ok(())
        }
        fn write_words(&mut self, address: u32, words: &[u32]) -> Result<(), ProbeError> {
            for (index, word) in words.iter().enumerate() {
                self.write_word(address + 4 * index as u32, *word)?;
            }
            Ok(())
        }
        fn halt(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            self.ending = Some(Ending::Running);
            Ok(())
        }
        fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
            unreachable!("a deploy reads words")
        }
        fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
            unreachable!("a deploy writes words")
        }
        fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
            unreachable!("a deploy reads words")
        }
        fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
            unreachable!("a deploy writes words")
        }
        fn resume(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy ends with a reset to run, not a resume")
        }
        fn step(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy does not step")
        }
        fn is_halted(&mut self) -> Result<bool, ProbeError> {
            unreachable!("a deploy does not read the run state")
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy does not wait for a halt")
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            self.ending = Some(Ending::HeldAtEntry);
            Ok(())
        }
        fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
            unreachable!("a deploy drives no reset line of its own")
        }
        fn read_core_reg(&mut self, _selector: u8) -> Result<u32, ProbeError> {
            unreachable!("a deploy reads no core register")
        }
        fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
            unreachable!("a deploy writes no core register")
        }
        fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no vector catch")
        }
        fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no vector catch")
        }
        fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no breakpoint")
        }
        fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no breakpoint")
        }
        fn set_breakpoints(&mut self, _addresses: &[u32]) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no breakpoint")
        }
        fn call_target(
            &mut self,
            _address: u32,
            _args: &[u32],
            _frame: &lamella_probe_core::CallFrame,
        ) -> Result<u32, ProbeError> {
            unreachable!("this part is programmed through its controller, not a loader")
        }
    }

    #[cfg(feature = "st")]
    #[test]
    fn an_h7_deploy_past_the_first_megabyte_puts_the_whole_image_on_both_banks() {
        let image = image_of(1536 * KB);
        let part = flash(H7Part::h747(), Part::Stm32H7, &image, None)
            .unwrap_or_else(|reason| panic!("a 1.5 MB image fits a 2 MB H7: {reason}"));
        assert_eq!(
            first_difference(&part.flash, &image),
            None,
            "every byte of the image is on the part, the ones in its second bank included"
        );
        assert_eq!(
            part.ending,
            Some(Ending::HeldAtEntry),
            "the part is left held at its entry, so the session can arm breakpoints before it runs"
        );
    }

    #[cfg(feature = "st")]
    #[test]
    fn an_h7_deploy_that_does_not_read_back_as_written_is_refused_naming_where() {
        let image = image_of(300 * KB);
        let mut part = H7Part::h747();
        part.erase_does_not_take = Some(0x0802_0000);
        let Err(reason) = flash(part, Part::Stm32H7, &image, None) else {
            panic!(
                "a sector whose erase did not take holds the wrong bytes, and a deploy hid that"
            );
        };
        assert!(
            reason.contains("0x08020000"),
            "the first address that differs is named: {reason}"
        );
    }

    #[cfg(feature = "st")]
    #[test]
    fn an_h7_deploy_to_a_part_answering_another_device_id_is_refused_before_any_erase() {
        let image = image_of(64 * KB);
        let mut part = H7Part::h747();
        part.idc = 0x1000_0413;
        let erases = Rc::clone(&part.erases);
        let Err(reason) = flash(part, Part::Stm32H7, &image, None) else {
            panic!("a part answering DEV_ID 0x413 is no STM32H7, and writing it is not a deploy");
        };
        assert!(
            reason.contains("0x413"),
            "the id the part answered is named: {reason}"
        );
        assert_eq!(
            erases.get(),
            0,
            "nothing is erased on a part that is not the one named"
        );
    }
}
