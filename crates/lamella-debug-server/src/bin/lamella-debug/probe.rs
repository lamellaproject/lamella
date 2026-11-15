//! The native tier: a program compiled ahead of time and written to a Cortex-M board through its
//! debug probe, then debugged there over SWD by a `DeviceBackend`.

use lamella_aot::build;
use lamella_cmsis_dap_nrf::Nrf51Flash;
use lamella_dap::DebugBackend;
use lamella_debug_device::DeviceBackend;
use lamella_metadata::{Assembly, PortablePdb};
use lamella_probe_core::{ArmDap, ProbeError, TargetAccess, TargetAccessExt};

/// build_debug's line-table offsets are image-relative (the code sits at image offset 8, after the
/// [SP][reset] vector table, and the image flashes at address 0), so a raw PC indexes the tables
/// directly -- no base to subtract.
/// Which probe family to open. Selected explicitly rather than by trying ids in order: on a bench
/// with several probes attached, a server that opens whichever answers first can flash a board
/// someone else is using.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProbeKind {
    CmsisDap,
    #[cfg(feature = "st")]
    StLink,
}

/// Which part is being driven: it selects the part-specific handling this binary has, which is a
/// flash algorithm for some variants and a reset sequence for others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Nrf51,
    /// Written by the route `lamella flash` takes for a micro:bit v2, and then held at its program's
    /// entry by a reset of its own: see [`hold_nrf52833`].
    Nrf52833,
    /// Selects how the part is STOPPED rather than how it is programmed -- stopping is the one
    /// thing the generic sequence cannot do here (see `--reset` below).
    #[cfg(feature = "sam")]
    Samd21,
    /// Written by the route `lamella flash` takes for it, through the chip's own boot ROM, and then
    /// held at its program's entry: see [`hold_rp2350`].
    Rp2350,
    #[cfg(feature = "st")]
    Stm32F0,
    #[cfg(feature = "st")]
    Stm32F4,
    #[cfg(feature = "st")]
    Stm32F7,
    #[cfg(feature = "st")]
    Stm32H7,
    /// Written, like an F7 or an H7, through the route `lamella flash` takes for its family.
    #[cfg(feature = "st")]
    Stm32U5,
}

impl Part {
    /// Where this part's flash is mapped for execution -- and therefore what `DeviceBackend` must
    /// subtract from a PC before indexing the image-relative line tables.
    fn flash_base(self) -> u32 {
        match self {
            Part::Nrf51 => 0,
            Part::Nrf52833 => 0,
            #[cfg(feature = "sam")]
            Part::Samd21 => 0,
            Part::Rp2350 => lamella_cmsis_dap_rp2350::XIP_BASE,
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
            Part::Stm32U5 => Some(lamella_flash_routes::StFamily::U5),
            _ => None,
        }
    }
}

/// The board a launch names, reduced to what this tier does with it.
struct NamedBoard {
    /// The id as it was given, which every `lamella` verb resolves the same way.
    id: String,
    /// The part this build writes for the board, or `None` for a board whose part it writes none of.
    part: Option<Part>,
    /// The debug-port IDCODE the board's part answers, where its part row states one.
    idcode: Option<u32>,
}

impl NamedBoard {
    /// Why the debugger cannot start the program on the board: it cannot write it.
    fn cannot_write(&self) -> String {
        format!("this debugger cannot write {}", self.id)
    }
}

/// A probe attached to this computer, as a launch's device id is matched against it.
pub struct AttachedProbe {
    /// Its USB serial number, which a device id names it by.
    pub serial: Option<String>,
    /// Whether it is an ST-LINK rather than a CMSIS-DAP probe.
    pub st_link: bool,
    /// Its USB product id, which names an ST-LINK's model.
    pub product_id: u16,
}

/// Every probe attached to this computer: the CMSIS-DAP probes, and the ST-LINKs the CMSIS-DAP
/// enumeration passes over, as `lamella devices` lists them.
pub fn attached_probes() -> Vec<AttachedProbe> {
    let mut passed_over = Vec::new();
    let cmsis_dap: Vec<AttachedProbe> = lamella_probe::list_reporting(&mut passed_over)
        .into_iter()
        .map(|probe| AttachedProbe {
            serial: probe.serial,
            st_link: false,
            product_id: probe.product_id,
        })
        .collect();
    probes_among(cmsis_dap, passed_over)
}

/// The probes a launch is matched against: `cmsis_dap`, the CMSIS-DAP probes, and the ST-LINKs among
/// `passed_over`, the devices the CMSIS-DAP enumeration passed over.
fn probes_among(
    cmsis_dap: Vec<AttachedProbe>,
    passed_over: Vec<lamella_probe::PassedOver>,
) -> Vec<AttachedProbe> {
    let mut attached = cmsis_dap;
    attached.extend(
        passed_over
            .into_iter()
            .filter(|device| lamella_stlink::is_st_link(device.vendor_id, device.product_id))
            .map(|device| AttachedProbe {
                serial: device.serial,
                st_link: true,
                product_id: device.product_id,
            }),
    );
    attached
}

/// What a launch asks of this tier, once everything this build cannot do as asked has been refused.
pub struct ProbePlan {
    part: Part,
    /// Whether the board chose the part, rather than the default standing in for it.
    part_named: bool,
    /// The board the launch named, whose part a session checks before it writes or attaches.
    board: Option<NamedBoard>,
    attach: bool,
    reset: bool,
    program: String,
    /// The program's Portable PDB, for a .NET assembly: the file the launch found for it, the
    /// manifest's or the one beside the program. An ELF carries its own debug information.
    symbols: Option<std::path::PathBuf>,
    /// The serial number of the probe the launch's device id names.
    serial: String,
    /// Whether a session that writes the program stops at the first line of its `Main`: the
    /// launch's `stopAtEntry`, for a session that debugs.
    stop_at_entry: bool,
}

impl ProbePlan {
    /// The plan for a launch that debugs `program` on the board behind the probe with the serial
    /// number `serial`. `board` chooses the part the program is written to; `attach` debugs the
    /// program already on the board, writing nothing. Nothing is opened: the probe is found when
    /// the session starts ([`backend`]), after the program has been read.
    ///
    /// # Errors
    /// Why the launch cannot be served as asked: a board the catalog does not know, or a board this
    /// build cannot write when the launch would write it.
    pub fn new(
        serial: &str,
        board: Option<&str>,
        program: &str,
        attach: bool,
    ) -> Result<ProbePlan, String> {
        let board = board.map(board_called).transpose()?;
        let (part, part_named) = match &board {
            Some(named) => match named.part {
                Some(part) => (part, true),
                None if attach => (Part::Nrf51, false),
                None => return Err(named.cannot_write()),
            },
            None => (Part::Nrf51, false),
        };
        Ok(ProbePlan {
            part,
            part_named,
            board,
            attach,
            reset: false,
            program: program.to_owned(),
            symbols: None,
            serial: serial.to_owned(),
            stop_at_entry: false,
        })
    }

    /// This plan, with `symbols` as the program's Portable PDB: the file the launch found for it.
    #[must_use]
    pub fn reading_symbols(self, symbols: Option<std::path::PathBuf>) -> ProbePlan {
        ProbePlan { symbols, ..self }
    }

    /// This plan, stopping at the first line of the program's `Main` once it is written, when
    /// `stop_at_entry` says so.
    #[must_use]
    pub fn stopping_at_entry(self, stop_at_entry: bool) -> ProbePlan {
        ProbePlan {
            stop_at_entry,
            ..self
        }
    }
}

/// The probe the serial number `serial` names among `attached`: which kind it is, and its USB
/// product id, which names an ST-LINK's model.
///
/// # Errors
/// No attached probe has that serial number, or the one that has it is a kind this build cannot
/// drive.
fn probe_named(serial: &str, attached: &[AttachedProbe]) -> Result<(ProbeKind, u16), String> {
    let found = attached.iter().find(|probe| {
        probe
            .serial
            .as_deref()
            .is_some_and(|attached| attached.eq_ignore_ascii_case(serial))
    });
    let Some(found) = found else {
        return Err(format!(
            "no attached debug probe has the serial number {serial}. The lamella devices command lists the \
             probes this computer can see, by the ids a launch's device takes."
        ));
    };
    if found.st_link {
        #[cfg(feature = "st")]
        return Ok((ProbeKind::StLink, found.product_id));
        #[cfg(not(feature = "st"))]
        return Err(missing_feature("a probe that is an ST-LINK", "st"));
    }
    Ok((ProbeKind::CmsisDap, found.product_id))
}

/// The board `id` names, resolved through the catalog every `lamella` verb resolves a board through.
///
/// **THE SAME ID THE BUILD TOOK.** A class-library program is built for a board, and the ELF that
/// build writes is what this tier debugs; naming the board once, in the same vocabulary, means the
/// image and the part it is written to cannot be chosen apart.
fn board_called(id: &str) -> Result<NamedBoard, String> {
    let (board, row) = lamella_catalog::resolve(id).map_err(|error| format!("board {id}: {error}"))?;
    Ok(NamedBoard {
        id: id.to_owned(),
        part: part_of_family(&board.family)?,
        idcode: row
            .dp_idcode
            .and_then(|idcode| u32::try_from(idcode.value).ok()),
    })
}

/// The part this build writes for a catalog family, or `None` for a family it writes none of.
///
/// Keyed by the FAMILY because both things a part chooses here, the flash algorithm and the reset
/// sequence, are family facts: an STM32F746 and an STM32F769 are written by one route, and two
/// boards carrying one chip are written alike.
fn part_of_family(family: &str) -> Result<Option<Part>, String> {
    match family {
        "nrf51" => Ok(Some(Part::Nrf51)),
        "nrf52833" => Ok(Some(Part::Nrf52833)),
        "rp2350" => Ok(Some(Part::Rp2350)),
        #[cfg(feature = "sam")]
        "samd21" => Ok(Some(Part::Samd21)),
        #[cfg(not(feature = "sam"))]
        "samd21" => Err(missing_feature("a board with a SAM D21", "sam")),
        #[cfg(feature = "st")]
        "stm32f091" => Ok(Some(Part::Stm32F0)),
        #[cfg(feature = "st")]
        "stm32f42x" => Ok(Some(Part::Stm32F4)),
        #[cfg(feature = "st")]
        "stm32f7" | "stm32f769" => Ok(Some(Part::Stm32F7)),
        #[cfg(feature = "st")]
        "stm32h7" => Ok(Some(Part::Stm32H7)),
        #[cfg(feature = "st")]
        "stm32u5a5" => Ok(Some(Part::Stm32U5)),
        #[cfg(not(feature = "st"))]
        "stm32f091" | "stm32f42x" | "stm32f7" | "stm32f769" | "stm32h7" | "stm32u5a5" => {
            Err(missing_feature("a board with an STM32", "st"))
        }
        _ => Ok(None),
    }
}

/// Refuses a part whose debug port does not answer the IDCODE the named board's part states.
///
/// **A MISMATCH PROVES THE PART IS NOT THE BOARD'S, AND A MATCH DOES NOT PROVE IT IS.** An IDCODE
/// names a debug-port design, which parts from different vendors share. It still separates the two
/// micro:bits, whose parts answer differently, so an image built for one is not written to the
/// other, and a session is not served against the wrong part's program.
fn check_board(found: u32, board: Option<&NamedBoard>) -> Result<(), String> {
    let Some(NamedBoard {
        id,
        idcode: Some(expected),
        ..
    }) = board
    else {
        return Ok(());
    };
    if found == *expected {
        return Ok(());
    }
    Err(format!(
        "the board on this probe is not {id}: its chip identifies itself as {found:#010x}, and \
         {id}'s chip as {expected:#010x}. Nothing was written. Check which board this probe is \
         on, or name the board it is."
    ))
}

/// The refusal for an argument this build was compiled without support for: the feature it needs, and
/// the command that installs a server with every feature on.
#[cfg(any(not(feature = "st"), not(feature = "sam")))]
fn missing_feature(argument: &str, feature: &str) -> String {
    format!(
        "{argument} needs a lamella-debug built with the \"{feature}\" feature, and this one was \
         built without it. A default build has every feature: cargo install --locked --git \
         https://github.com/lamellaproject/lamella lamella-debug-server"
    )
}

/// The backend for `plan`: the program read and composed for debugging, the probe opened, and the
/// program written to the part or, for an attach, the part reached.
///
/// The program is read before any probe is opened, so a launch refused for its program touches no
/// board.
///
/// # Errors
/// Why the session cannot start, in words for the user.
pub fn backend(plan: &ProbePlan) -> Result<Box<dyn DebugBackend>, String> {
    let (debug, image_entry, window) = load_program(plan)?;
    let (probe, product_id) = probe_named(&plan.serial, &attached_probes())?;
    if plan.part == Part::Rp2350 && !plan.attach {
        write_as_lamella_flash_does(plan, &debug.image)?;
    }
    match probe {
        #[cfg(feature = "st")]
        ProbeKind::StLink => {
            let stlink = lamella_stlink::StLink::open(product_id, Some(&plan.serial)).map_err(
                |error| {
                    format!("could not open the ST-LINK with USB product id {product_id:#06x}: {error}")
                },
            )?;
            let low_power_debug = plan.part.low_power_debug();
            let attach_under_reset = move |stlink: &mut lamella_stlink::StLink| {
                stlink.attach_under_reset(low_power_debug)
            };
            prepare(
                stlink,
                low_power_debug
                    .is_some()
                    .then_some(&attach_under_reset as &PartAttach<lamella_stlink::StLink>),
                plan,
                debug,
                image_entry,
                window.as_ref(),
            )
        }
        ProbeKind::CmsisDap => {
            let _ = product_id;
            let selector = lamella_probe::Selector::by_serial(&plan.serial);
            let session = lamella_probe::open(&selector)
                .map_err(|error| format!("could not open a CMSIS-DAP probe: {error}"))?;
            eprintln!(
                "probe: {} {:04x}:{:04x}, serial {}",
                session.info.product.as_deref().unwrap_or("a probe that gives no product name"),
                session.info.vendor_id,
                session.info.product_id,
                session.info.serial.as_deref().unwrap_or("none")
            );
            prepare(
                ArmDap::new(session.into_dap()),
                None,
                plan,
                debug,
                image_entry,
                window.as_ref(),
            )
        }
    }
}

/// Writes the program to the board as `lamella flash <program> --board <board> --probe <serial>`
/// writes it: through the route the board's row names for a probe, from the image that command reads
/// out of the file, with every byte read back. The route opens the probe itself and closes it again.
///
/// # Errors
/// The route's refusal or failure, in its own words, and a file whose image is not the one this
/// session maps to source, which is refused before anything is opened.
fn write_as_lamella_flash_does(plan: &ProbePlan, image: &[u8]) -> Result<(), String> {
    use lamella_flash_routes::placement::{BootloaderChoice, placement_for};

    let Some(board) = plan.board.as_ref() else {
        return Err(format!(
            "the build named no board for {}, so nothing was written",
            plan.program
        ));
    };
    let row = lamella_flash_routes::programmer_for(&board.id)?;
    let route = lamella_flash_routes::route_for_request(
        row,
        Some("probe"),
        Some(&plan.serial),
        None,
        None,
    )?;
    let placement = placement_for(row, route, BootloaderChoice::Keep)?;
    let prepared = lamella_flash_routes::prepare_image(
        std::path::Path::new(&plan.program),
        row,
        route,
        &placement,
    )?;
    if prepared.window.is_some() || prepared.bytes != image {
        return Err(format!(
            "{}: the image the lamella flash command writes from this file is not the one this session maps to \
             its source, so nothing was written",
            plan.program
        ));
    }
    let selector = lamella_flash_routes::selector_for(route, Some(&plan.serial), None, None)?;
    eprintln!("deploy: {}, to {}", prepared.read, board.id);
    let mut say = |line: &str| eprintln!("{line}");
    lamella_flash_routes::write(
        route,
        &placement,
        &prepared.bytes,
        selector.as_deref(),
        &mut say,
    )?;
    Ok(())
}

/// The address an ELF's image starts running at, from its header's `e_entry` with the bit that
/// selects the Thumb instruction set cleared, or `None` for a file that is not a 32-bit
/// little-endian ELF.
fn elf_entry(bytes: &[u8]) -> Option<u32> {
    if bytes.get(..6) != Some(&b"\x7fELF\x01\x01"[..]) {
        return None;
    }
    let word: [u8; 4] = bytes.get(24..28)?.try_into().ok()?;
    Some(u32::from_le_bytes(word) & !1)
}

/// The half of a two-region image that belongs in the board's external flash, with the board fact
/// that says how that flash is reached.
struct WindowHalf {
    /// The board's external flash: the window the chip maps it at, its wiring and the parts allowed.
    flash: &'static lamella_flash_routes::external::ExternalFlash,
    /// The bytes that belong there, from the window's base.
    half: lamella_flash_routes::WindowHalf,
}

/// What a session writes and maps: the program composed for debugging, the address its image starts
/// running at where the file states one, and its half for the board's external flash, where it has one.
type Loaded = (
    lamella_debug_device::program::DebugProgram,
    Option<u32>,
    Option<WindowHalf>,
);

/// Reads the program and composes what a session needs to map its addresses to source, and, for an
/// ELF, the address its image starts running at and its half for the board's external flash.
///
/// A program this tier cannot debug as the launch describes it is an `Err` with the reason:
/// a file that cannot be read, an ELF that does not parse, an ELF that would be written somewhere
/// other than where it was linked to run, an ELF whose halves `lamella flash` would refuse to write,
/// or an assembly that names nothing to start or does not build on the tier this server debugs.
fn load_program(plan: &ProbePlan) -> Result<Loaded, String> {
    let program = &plan.program;
    let bytes =
        std::fs::read(program).map_err(|error| format!("could not read {program}: {error}"))?;
    check_record(plan, &bytes)?;
    if bytes.starts_with(b"\x7fELF") {
        let (elf, window) = match read_the_set(plan)? {
            Some((base, image, window)) => (
                lamella_debug_device::program::from_elf_with_image(&bytes, base, image),
                Some(window),
            ),
            None => (lamella_debug_device::program::from_elf(&bytes), None),
        };
        let elf = elf.map_err(|error| format!("{program}: {error}"))?;
        refuse_the_write(
            plan,
            program,
            Producer::Elf {
                image_base: elf.image_base,
            },
        )?;
        return Ok((elf, elf_entry(&bytes), window));
    }

    refuse_the_write(plan, program, Producer::Assembly)?;
    let SourceLines {
        lines,
        names,
        image,
        file,
        entry,
    } = source_lines(program, &bytes, plan.symbols.as_deref())?;
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
    let program = lamella_debug_device::program::DebugProgram {
        image,
        image_base: plan.part.flash_base(),
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
        console: None,
    };
    Ok((program, None, None))
}

/// The halves `lamella flash` writes from the program's file for a board with an external flash, when
/// the file has a half for it: where the half the part boots from goes, its bytes, and the external
/// flash's half. `None` for a board with no external flash, or a file with nothing for one.
///
/// **READ AS `lamella flash` READS IT, BY THE SAME FUNCTION**, so the halves a session writes and maps
/// are the ones that command writes from the same file, refused where it refuses them: a segment
/// across the window's edge, a half that does not start where its region does, or one too big for
/// the flash it belongs in. Read for an attach too, which writes nothing: the tables of a file split
/// in two are composed from its halves, never from one image across the gap between them.
///
/// # Errors
/// The route's refusal, in its own words.
fn read_the_set(plan: &ProbePlan) -> Result<Option<(u32, Vec<u8>, WindowHalf)>, String> {
    use lamella_flash_routes::placement::{BootloaderChoice, placement_for};

    let Some(board) = plan.board.as_ref() else {
        return Ok(None);
    };
    let Some(flash) = lamella_flash_routes::external::for_board(&board.id) else {
        return Ok(None);
    };
    let row = lamella_flash_routes::programmer_for(&board.id)?;
    let route = lamella_flash_routes::route_for_request(row, None, Some(&plan.serial), None, None)?;
    let placement = placement_for(row, route, BootloaderChoice::Keep)?;
    let prepared = lamella_flash_routes::prepare_image(
        std::path::Path::new(&plan.program),
        row,
        route,
        &placement,
    )?;
    Ok(prepared
        .window
        .map(|half| (placement.base(), prepared.bytes, WindowHalf { flash, half })))
}

/// Refuses a program its record says is another image, or one built for another board than the
/// build names. The record is the sidecar beside the image, `<image>.manifest.json`, which the tool
/// that linked the image wrote and `lamella flash` checks before it writes one; `bytes` are the image
/// as this session read it.
///
/// **THE SAME CHECKS `lamella flash` MAKES, BEFORE ANY PROBE IS LOOKED FOR**, for a write and for an
/// attach alike: the session maps the program on the board with this file's tables, so a file that is
/// not the image its record describes would step through the wrong source even where nothing is
/// written. A record that cannot be read is refused, because a claim was made that cannot be checked;
/// an image with no record beside it is not checked, as `lamella flash` does not check one.
fn check_record(plan: &ProbePlan, bytes: &[u8]) -> Result<(), String> {
    use lamella_flash_routes::manifest;
    let image = std::path::Path::new(&plan.program);
    let record = match manifest::read(image) {
        Ok(Some(record)) => record,
        Ok(None) => return Ok(()),
        Err(why) => {
            return Err(format!(
                "the build's record of the board this program is for cannot be read: {} Without it the \
                 board cannot be checked, so nothing was written. Build the project again.",
                sentences(&why)
            ));
        }
    };
    if let Some(board) = plan.board.as_ref()
        && record.board != board.id
    {
        return Err(format!(
            "{} was built for {}, and this session debugs {}. Nothing was written. Build the project \
             for the board you debug on.",
            plan.program, record.board, board.id
        ));
    }
    let format = lamella_flash_routes::artifact::classify_format(image);
    manifest::check_identity(&record, bytes, format.as_deref()).map_err(|why| {
        format!(
            "{} is not the program its build recorded: {} Nothing was written. Build the project \
             again.",
            plan.program,
            sentences(&why)
        )
    })
}

/// `text` as the sentences of one paragraph, ending in a full stop: each run of spaces and line
/// breaks one space, so a reason written for a terminal, in lines and aligned columns, reads as part
/// of the refusal the editor shows.
fn sentences(text: &str) -> String {
    let mut paragraph = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if !paragraph.ends_with('.') {
        paragraph.push('.');
    }
    paragraph
}

/// What kind of program a write would put on the part, as far as the write cares.
#[derive(Clone, Copy, Debug)]
enum Producer {
    /// A linked ELF, which says where it loads.
    Elf {
        /// The address the ELF is linked to load at.
        image_base: u32,
    },
    /// A .NET assembly, whose image this tier builds.
    Assembly,
}

/// Refuses a write the plan cannot make to the part it names: one with no part named, whatever the
/// program, and one whose program was made for another part than the named one. An attach writes
/// nothing, so it is never refused here.
///
/// **ONE RULE FOR BOTH KINDS OF PROGRAM, AND THE PART COMES FIRST.** The part selects the flash
/// algorithm as well as the base, so with none named the default part's algorithm would be driven
/// against whatever part the probe reaches -- a write to whatever that part maps at those addresses,
/// read back as a success.
fn refuse_the_write(plan: &ProbePlan, program: &str, producer: Producer) -> Result<(), String> {
    if plan.attach {
        return Ok(());
    }
    if !plan.part_named {
        return Err(format!(
            "the build named no board for {program}, so this debugger does not know how to write it, \
             and nothing was written. Build the project for the board you debug on."
        ));
    }
    match producer {
        Producer::Elf { image_base } if image_base != plan.part.flash_base() => Err(format!(
            "{program} is linked to load at {image_base:#010x}, and the board's part has its flash at \
             {:#010x}: the image was built for another board",
            plan.part.flash_base()
        )),
        Producer::Assembly if plan.part != Part::Nrf51 => {
            let board = plan
                .board
                .as_ref()
                .map_or("the board the build names", |board| board.id.as_str());
            Err(format!(
                "{program} is an assembly, and an assembly is debugged directly only on a micro:bit v1. \
                 The build names {board}, so nothing was written. A program for {board} is debugged \
                 from the ELF the project's build for a debug probe writes."
            ))
        }
        _ => Ok(()),
    }
}

/// A part's own attach through one probe, which a session takes in place of a plain connect: see
/// [`prepare`].
type PartAttach<A> = dyn Fn(&mut A) -> Result<(), ProbeError>;

/// Writes the image and composes the backend over the freshly written program, or, for an attach,
/// reaches the part and composes it over the program already there. A written program is held at its
/// first instruction, and for a launch that stops at its entry, run on to the first line of its
/// `Main`.
///
/// Generic over the probe, so an ST-Link and a CMSIS-DAP probe run the identical code path. What the
/// probe adds is `attach_under_reset`, the part's own attach through that probe when the part has
/// one: the deploy and a reset take it in place of a plain connect. `image_entry` is where an ELF's
/// image starts running, which an RP2350 is held at; `window` is the program's half for the board's
/// external flash, written with the rest.
fn prepare<A: TargetAccess + 'static>(
    mut probe: A,
    attach_under_reset: Option<&PartAttach<A>>,
    plan: &ProbePlan,
    debug: lamella_debug_device::program::DebugProgram,
    image_entry: Option<u32>,
    window: Option<&WindowHalf>,
) -> Result<Box<dyn DebugBackend>, String> {
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
        console,
    } = debug;
    let board = plan.board.as_ref();
    if plan.attach {
        attach_to_part(&mut probe, plan.part, board, plan.reset, attach_under_reset)?;
    } else {
        probe = match plan.part {
            Part::Rp2350 => hold_rp2350(probe, board, image_entry)?,
            part => flash(probe, part, board, &image, window, attach_under_reset)?,
        };
        if let Some(ring) = console {
            lamella_debug_device::clear_console_ring(&mut probe, ring).map_err(|error| {
                format!("could not clear the program output the last run left in the board's memory: {error}")
            })?;
        }
        if plan.stop_at_entry {
            let main = program_main(&names, &entry, image_base).map(|main| {
                lamella_debug_device::program::past_the_prologue(&locals, &lines, image_base, main)
            });
            stop_at_main(&mut probe, main)?;
        }
    }
    let mut backend = DeviceBackend::new(probe, lines, image_base, names, files, entry, frames)
        .with_locals(locals)
        .with_unwind_tables(unwind);
    if let Some(ring) = console {
        backend = backend.with_console(ring);
    }
    Ok(Box::new(backend))
}

/// Reaches the part for a session that debugs the program already on it, and with `reset`, stops
/// that program at its entry.
///
/// The part is attached plainly unless `reset` asks for a reset and the probe has an attach of its
/// own for the part (`attach_under_reset`), which then is the reset as well: it leaves the core halted
/// at its reset vector. A plain attach that cannot read the part is refused with the probe's reason.
///
/// # Errors
/// Why the part could not be reached, in words for the user.
fn attach_to_part<A: TargetAccess>(
    probe: &mut A,
    part: Part,
    board: Option<&NamedBoard>,
    reset: bool,
    attach_under_reset: Option<&PartAttach<A>>,
) -> Result<(), String> {
    let check = |probe: &mut A| match board {
        Some(_) => {
            let found = probe
                .read_idcode()
                .map_err(|error| format!("could not reach the board's debug port: {error}"))?;
            check_board(found, board)
        }
        None => Ok(()),
    };
    if reset && let Some(attach) = attach_under_reset {
        attach(probe).map_err(|error| {
            format!("could not attach to the part with its core held in reset: {error}")
        })?;
        check(probe)?;
        eprintln!("reset: the part is halted at its entry, attached with its core held in reset");
        return Ok(());
    }
    probe
        .connect()
        .map_err(|error| format!("could not attach to the part: {error}"))?;
    check(probe)?;
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
                "reset: could not reset this part and hold it ({error}). The session is \
                 attached to the RUNNING program instead, so a stop reported as \"entry\" is \
                 wherever it happens to be, and a breakpoint on anything that has already run \
                 will not be hit."
            ),
        }
    }
    Ok(())
}

/// Flashes a raw image to the selected part, leaves the part held at its program's first instruction
/// (or running it, where it cannot be held), and hands the probe back.
///
/// Every algorithm reached here is a blanket impl over [`TargetAccess`], so this is genuinely one
/// function over two probe families and every part it writes -- the seam doing its job rather than a
/// coincidence. An STM32F7, H7 or U5 is written through the write `lamella flash` takes for its family,
/// with `window`, the image's half for the board's external flash, where it has one. So is a micro:bit
/// v2's nRF52833, which is then held by a reset of its own ([`hold_nrf52833`]). For the other parts
/// only the erase geometry and the base address are per-part.
///
/// # Errors
/// Why the image is not on the part, for the user.
fn flash<A: TargetAccess>(
    mut target: A,
    part: Part,
    board: Option<&NamedBoard>,
    image: &[u8],
    window: Option<&WindowHalf>,
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
    let idcode = target
        .read_idcode()
        .map_err(deploy_step("reach the board's debug port"))?;
    check_board(idcode, board)?;
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
            window,
        )?);
    }
    if window.is_some() {
        return Err(
            "this image has a half for the board's external flash, and this debugger writes that \
             flash only through the board's own ST-LINK, so nothing was written"
                .to_owned(),
        );
    }
    if part == Part::Nrf52833 {
        return hold_nrf52833(write_through_the_micro_bit_v2_route(target, image)?, image);
    }
    target.halt().map_err(deploy_step("halt the core"))?;
    let base = part.flash_base();

    match part {
        #[cfg(feature = "sam")]
        Part::Samd21 => {
            return Err(board.map_or_else(
                || String::from("this debugger cannot write a SAM D21"),
                NamedBoard::cannot_write,
            ));
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
                let on = board.map_or("this board", |board| board.id.as_str());
                return Err(format!(
                    "this debugger cannot write a program of {} bytes to {on}: it writes at most {} \
                     bytes of its flash",
                    image.len(),
                    table.min(fitted)
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
        Part::Stm32F7 | Part::Stm32H7 | Part::Stm32U5 => {
            unreachable!("an STM32F7, H7 or U5 is written through its route before the halt")
        }
        Part::Nrf52833 => {
            unreachable!("an nRF52833 is written through its route before the halt")
        }
        Part::Rp2350 => {
            unreachable!("an RP2350 is written before the session opens its probe, then held")
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
        crate::launch::say(format!(
            "reset: this part could not be held at its entry after the deploy ({error}), so it \
             is running the new image instead. A breakpoint on a line that runs before the \
             session attaches will not be hit."
        ));
        target
            .reset_and_run()
            .map_err(deploy_step("reset the part to run"))?;
    }
    Ok(target)
}

/// `AIRCR`, the Application Interrupt and Reset Control Register, which a debugger asks for a system
/// reset through (Armv7-M Architecture Reference Manual, DDI 0403E.d, B3.2.6).
const AIRCR: u32 = 0xE000_ED0C;
/// The write to [`AIRCR`] that asks for the reset: `VECTKEY` 0x05FA in bits 31:16, without which the
/// register ignores the write, and `SYSRESETREQ`, bit 2.
const AIRCR_SYSRESETREQ: u32 = 0x05FA_0004;

/// How long an nRF52833's reset is given to stop its core at the reset vector. The catch halts the
/// core as it leaves the reset, so this bound covers the probe's round trips, not the part.
const HOLD_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

/// The pause before each look at whether that reset has stopped the core.
const HOLD_LOOK: std::time::Duration = std::time::Duration::from_millis(10);

/// Holds a freshly written nRF52833 at its program's first instruction by one reset of its own, or
/// leaves it running its new image where that reset does not stop it.
///
/// # NOT THE HOLD EVERY OTHER PART TAKES
///
/// [`leave_held`] begins with a pulse on the reset line, and once a write has erased this part's
/// UICR, no pin carries the reset: its `PSELRESET` registers then read `CONNECT` = 1, "Disconnected"
/// (nRF52833 Product Specification, 4452_021 v1.7, 4.5.1.62). The reset a debugger has here is the
/// soft one `SYSRESETREQ` asks for. It resets the CPU, the peripherals and the GPIO but not the debug
/// logic (5.3.6.8), so the reset vector catch armed before it survives it and halts the core at its
/// reset vector (Armv7-M Architecture Reference Manual, DDI 0403E.d, C1.6.5, `DEMCR`).
///
/// So the hold is that one reset:
/// - held means the core halted with its PC at the image's reset vector, read back rather than
///   assumed;
/// - a core halted elsewhere is left where it stopped, and the session is told where;
/// - a reset that does not stop the core is not followed by another: the part runs its new image,
///   and the session is told which breakpoints that costs.
///
/// Each step is said on standard error as it starts.
///
/// # Errors
/// A step the probe or the part fails: nothing more is asked of the probe after it, and the session
/// does not start.
fn hold_nrf52833<A: TargetAccess>(mut target: A, image: &[u8]) -> Result<A, String> {
    eprintln!("reset: arming the reset vector catch");
    target
        .arm_reset_catch()
        .map_err(hold_step("arm the reset vector catch"))?;
    eprintln!("reset: resetting the part");
    match target.write_word(AIRCR, AIRCR_SYSRESETREQ) {
        Ok(()) | Err(ProbeError::Ack(_)) => {}
        Err(error) => return Err(hold_step("reset the part")(error)),
    }
    eprintln!("reset: looking for the core stopped at its reset vector");
    let started = std::time::Instant::now();
    let stopped = loop {
        std::thread::sleep(HOLD_LOOK);
        if target
            .is_halted()
            .map_err(hold_step("look at the core after its reset"))?
        {
            break true;
        }
        if started.elapsed() >= HOLD_WAIT {
            break false;
        }
    };
    eprintln!("reset: disarming the reset vector catch");
    target
        .disarm_reset_catch()
        .map_err(hold_step("disarm the reset vector catch"))?;
    if !stopped {
        crate::launch::say(format!(
            "reset: this part could not be held at its entry after the deploy (its reset did not stop \
             the core within {} ms), so it is running the new image instead. A breakpoint on a line \
             that runs before the session attaches will not be hit.",
            HOLD_WAIT.as_millis()
        ));
        return Ok(target);
    }
    let pc = target
        .read_core_reg(15)
        .map_err(hold_step("read where the core stopped"))?;
    match reset_vector(image) {
        Some(entry) if pc == entry => eprintln!("reset: the part is held at its entry, {pc:#010x}"),
        Some(entry) => crate::launch::say(format!(
            "reset: the part stopped at {pc:#010x} after its reset, not at its image's entry \
             {entry:#010x}, so the session starts where it stopped."
        )),
        None => crate::launch::say(format!(
            "reset: the part stopped at {pc:#010x} after its reset, and the image is too short to hold \
             the vector table that names its entry, so the session starts where it stopped."
        )),
    }
    Ok(target)
}

/// Where an image's vector table starts its core: the table's second word, the reset vector, with
/// the bit that selects the Thumb instruction set cleared. `None` for an image too short to hold one.
fn reset_vector(image: &[u8]) -> Option<u32> {
    let word: [u8; 4] = image.get(4..8)?.try_into().ok()?;
    Some(u32::from_le_bytes(word) & !1)
}

/// The reason a session's start gives when the probe or the part fails `step` of the hold.
fn hold_step(step: &'static str) -> impl FnOnce(ProbeError) -> String {
    move |error| {
        format!("could not {step} after the deploy, so nothing more was asked of the probe: {error}")
    }
}

/// How long a part is given to reach a stop it is run to: its boot ROM and the program's own start
/// before the image's entry, or the program's start before `Main`.
const RUN_TO_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

/// Where a run to an address ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunTo {
    /// The core stopped at the address.
    Arrived,
    /// The core stopped somewhere else first, at this address: a debug event of the program's own.
    StoppedAt(u32),
    /// The core did not stop in the time given, and was halted where it was, at this address.
    HaltedAt(u32),
}

/// Runs a halted core until it reaches `address`, with one breakpoint there, and leaves it halted
/// with no breakpoint armed, wherever it stopped.
///
/// # Errors
/// The probe's, for a part that stopped answering.
fn run_to<A: TargetAccess>(
    target: &mut A,
    address: u32,
    wait: std::time::Duration,
) -> Result<RunTo, ProbeError> {
    target.set_breakpoints(&[address])?;
    target.resume()?;
    let deadline = std::time::Instant::now() + wait;
    let stopped = loop {
        match target.is_halted() {
            Ok(true) => break true,
            Err(error) if target.stopped_answering().is_some() => return Err(error),
            _ => {}
        }
        if std::time::Instant::now() >= deadline {
            break false;
        }
    };
    if !stopped {
        target.halt()?;
    }
    let pc = target.read_core_reg(15)?;
    target.set_breakpoints(&[])?;
    Ok(match (stopped, pc == address) {
        (true, true) => RunTo::Arrived,
        (true, false) => RunTo::StoppedAt(pc),
        (false, _) => RunTo::HaltedAt(pc),
    })
}

/// Holds an RP2350, which the write `lamella flash` makes has just written and started, at its image's
/// entry: the first instruction of the program's own, so the session can arm its breakpoints before
/// anything of the program runs.
///
/// The part is reset with its core caught at the boot ROM's reset vector, then run to `entry` with one
/// breakpoint there: the boot ROM finds the image and starts it, and the core stops on its entry. A
/// part that cannot be caught is left running its program, and a program that does not reach its
/// entry is held where it is; either way the session is served, and what it cannot honor is said.
///
/// # Errors
/// Only a part that no longer answers.
fn hold_rp2350<A: TargetAccess>(
    mut target: A,
    board: Option<&NamedBoard>,
    entry: Option<u32>,
) -> Result<A, String> {
    target.connect().map_err(deploy_step("reach the part"))?;
    let idcode = target
        .read_idcode()
        .map_err(deploy_step("reach the board's debug port"))?;
    check_board(idcode, board)?;
    target
        .init_mem()
        .map_err(deploy_step("reach the part's memory"))?;
    if let Err(error) = lamella_cmsis_dap_rp2350::reset_halt(&mut target) {
        if target.stopped_answering().is_some() {
            return Err(deploy_step("hold the part at its entry")(error));
        }
        crate::launch::say(format!(
            "reset: this part could not be held at its entry after the deploy ({error}), so it is \
             running the new image instead. A breakpoint on a line that runs before the session \
             attaches will not be hit."
        ));
        return Ok(target);
    }
    let Some(entry) = entry else {
        crate::launch::say(format!(
            "reset: the image names no entry, so the part is held at its boot ROM's reset vector, \
             before the program runs"
        ));
        return Ok(target);
    };
    match run_to(&mut target, entry, RUN_TO_WAIT).map_err(deploy_step("run the part to its entry"))? {
        RunTo::Arrived => {}
        RunTo::StoppedAt(pc) => crate::launch::say(format!(
            "reset: the part stopped at {pc:#010x} before its program's entry at {entry:#010x}, and \
             is held there"
        )),
        RunTo::HaltedAt(pc) => crate::launch::say(format!(
            "reset: the program did not reach its entry at {entry:#010x} within {} seconds, so the \
             part is held where it was, at {pc:#010x}",
            RUN_TO_WAIT.as_secs()
        )),
    }
    Ok(target)
}

/// The address of the program's `Main`, the method the language starts it in: the entry the build
/// names when that is a `Main`, or else the one function its debug information names `Main` or
/// `<Main>$`, which is what C# names the method a program of top-level statements runs. `None`
/// where there is none, or more than one, so a session never stops in a `Main` it guessed.
fn program_main(names: &[(u32, u32, String)], entry: &str, image_base: u32) -> Option<u32> {
    let is_main = |name: &str| {
        let last = name.rsplit(['.', ':']).next().unwrap_or(name);
        last == "Main" || last == "<Main>$"
    };
    let address = |(start, _, _): &(u32, u32, String)| image_base.wrapping_add(*start);
    if is_main(entry)
        && let Some(named) = names.iter().find(|(_, _, name)| name == entry)
    {
        return Some(address(named));
    }
    let mut mains = names.iter().filter(|(_, _, name)| is_main(name));
    let first = mains.next()?;
    mains.next().is_none().then(|| address(first))
}

/// Runs a part held at its program's first instruction on to `main`, the first line of its `Main`,
/// for a launch whose `stopAtEntry` asks to stop there. Where it cannot, the part is left where it
/// is and the reason is said.
///
/// # Errors
/// Only a part that no longer answers.
fn stop_at_main<A: TargetAccess>(target: &mut A, main: Option<u32>) -> Result<(), String> {
    match target.is_halted() {
        Ok(true) => {}
        Err(error) if target.stopped_answering().is_some() => {
            return Err(deploy_step("look at the core before running it to Main")(error));
        }
        _ => {
            crate::launch::say(format!(
                "stopAtEntry: the part could not be held before its program ran, so the session \
                 stops wherever the program is"
            ));
            return Ok(());
        }
    }
    let Some(main) = main else {
        crate::launch::say(format!(
            "stopAtEntry: the program's debug information names no single Main, so the session \
             stops at the program's entry"
        ));
        return Ok(());
    };
    match run_to(target, main, RUN_TO_WAIT).map_err(deploy_step("run the program to its Main"))? {
        RunTo::Arrived => {}
        RunTo::StoppedAt(pc) => crate::launch::say(format!(
            "stopAtEntry: the program stopped at {pc:#010x} before it reached Main, and the session \
             stops there"
        )),
        RunTo::HaltedAt(pc) => crate::launch::say(format!(
            "stopAtEntry: the program did not reach Main within {} seconds, so the session stops \
             where it was, at {pc:#010x}",
            RUN_TO_WAIT.as_secs()
        )),
    }
    Ok(())
}

/// Writes `image` from `base` through the write `lamella flash` takes for `family` -- the part
/// identified by its `DEV_ID`, the write bounded by the flash the part reports, every bank the write
/// reaches unlocked, the watchdogs the family's plan names stopped while the core is halted, and every
/// byte read back -- and hands the probe back with the part reset to run.
///
/// With `window`, the image's half for the board's external flash, the two are written as the one
/// matched set `lamella flash` writes (`lamella_flash_routes::write_set`), by the same steps over this
/// session's attach: the external flash identified before anything is erased, the part's half erased
/// first, the window's half written whole, then the part's half with its vector last, and both read
/// back.
///
/// # Errors
/// Why the route refused or failed, for the user: a part answering another `DEV_ID`, a flash map the
/// route does not write (an STM32F7 in dual-bank mode), an image past the part's flash, an external
/// flash that is not a part the board allows, or a byte that did not read back as written.
#[cfg(feature = "st")]
fn write_through_the_route<A: TargetAccess>(
    target: A,
    family: lamella_flash_routes::StFamily,
    base: u32,
    image: &[u8],
    window: Option<&WindowHalf>,
) -> Result<A, String> {
    let boot = lamella_flash_backend::Image { bytes: image, base };
    let mut backend = lamella_flash_routes::backends::StProbe::new(target, family.plan())
        .saying(|line: &str| eprintln!("deploy: {line}"));
    let written = match window {
        None => lamella_flash_backend::flash(
            &mut backend,
            &boot,
            lamella_flash_backend::VerifyPolicy::ReadBack,
            &lamella_flash_backend::Allow::Any,
        )
        .map(drop),
        Some(window) => {
            backend = backend.with_window(window.flash);
            lamella_flash_backend::flash_set(
                &mut backend,
                &boot,
                &lamella_flash_backend::Image {
                    bytes: &window.half.bytes,
                    base: window.half.base,
                },
                lamella_flash_backend::VerifyPolicy::ReadBack,
                &lamella_flash_backend::Allow::Any,
            )
            .map(drop)
        }
    };
    let target = backend.into_target();
    written.map_err(|why| format!("could not write the image: {why}"))?;
    Ok(target)
}

/// Writes `image` at flash zero through the write `lamella flash` takes for a micro:bit v2: the part
/// identified by the IDCODE its debug port answers, the whole flash erased, the image programmed
/// through the part's flash controller, and every byte read back. Then it hands the probe back with
/// the part reset to run.
///
/// # Errors
/// Why the route refused or failed, for the user: a part answering another IDCODE, or a byte that
/// did not read back as written.
fn write_through_the_micro_bit_v2_route<A: TargetAccess>(target: A, image: &[u8]) -> Result<A, String> {
    eprintln!("deploy: writing the image through the micro:bit v2's route");
    let mut backend = lamella_flash_routes::backends::MicrobitDaplink::new(
        target,
        lamella_cmsis_dap_nrf::NRF52_IDCODE,
        lamella_flash_routes::NRF52_SETTLES,
    );
    let written = lamella_flash_backend::flash(
        &mut backend,
        &lamella_flash_backend::Image { bytes: image, base: 0 },
        lamella_flash_backend::VerifyPolicy::ReadBack,
        &lamella_flash_backend::Allow::Any,
    );
    let target = backend.into_target();
    written.map_err(|why| format!("could not write the image: {why}"))?;
    eprintln!("deploy: written, and every byte read back");
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
/// `symbols` (CIL -> source line), the file the launch found for the program. The entry point's
/// method selects the source `file` document. Lines are 0 without a PDB (instruction-level).
///
/// A program that cannot be served is an `Err` with the reason, which the session's `launch` shows
/// the user. Every one of these is something a person can do: debug a different project, or change
/// the program.
fn source_lines(
    program: &str,
    bytes: &[u8],
    symbols: Option<&std::path::Path>,
) -> Result<SourceLines, String> {
    let assembly = Assembly::read(bytes).map_err(|_| {
        format!("{program} is neither a linked ELF nor a .NET assembly this debugger can read.")
    })?;
    refuse_libraries(program, &assembly)?;
    let token = assembly.image().entry_point_token();
    if token == 0 {
        return Err(format!(
            "{program} has no entry point -- it is a class library, or it declares no static Main -- \
             so nothing says where the program starts. Debug the project of a program, whose Main is \
             where it starts."
        ));
    }
    let rid = token & 0x00ff_ffff;
    let method = assembly
        .type_defs()
        .find_map(|type_def| type_def.methods().find(|m| m.rid() == rid))
        .ok_or_else(|| {
            format!(
                "{program} names method {rid} as its entry point, and no type in it declares that \
                 method, so its metadata does not hold together."
            )
        })?;
    let entry_rid = method.rid();

    let pdb_bytes = symbols
        .map(|symbols| {
            std::fs::read(symbols)
                .map_err(|error| format!("could not read the symbols {}: {error}", symbols.display()))
        })
        .transpose()?;
    let pdb = pdb_bytes
        .as_deref()
        .zip(symbols)
        .map(|(bytes, symbols)| {
            PortablePdb::read(bytes).map_err(|_| crate::launch::not_a_portable_pdb(symbols))
        })
        .transpose()?;
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

    let (image, method_debug) =
        build::build_debug(bytes, "microbit", lamella_aot::BuildKind::Debug)
            .map_err(|error| flat_refusal(program, &error))?;
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
    Ok(SourceLines {
        lines,
        names,
        image,
        file,
        entry,
    })
}

/// What [`source_lines`] composes from a .NET assembly: the image to flash, and what maps its
/// addresses back to source.
struct SourceLines {
    /// Native offset to source line, image-relative, in offset order.
    lines: Vec<(u32, u32)>,
    /// Each method's start offset and `Type.Method` name, in offset order.
    names: Vec<(u32, String)>,
    /// The image that gets flashed, the same bytes the line tables were built from.
    image: Vec<u8>,
    /// The source document every row of `lines` comes from.
    file: String,
    /// The entry method's `Type.Method` name.
    entry: String,
}

/// Refuses an assembly compiled against any library beyond its core library.
///
/// **THE FLAT TIER LINKS NOTHING, SO SUCH A PROGRAM IS NOT ONE IT CAN BUILD, EVEN WHERE EVERY METHOD
/// IT REACHES LOWERS.** A method lowered here that names a library's type would run without any of
/// that library's code on the board. The class-library tier links the libraries a program is compiled
/// against, and it builds from source, so the refusal names the source.
///
/// A core library is an assembly the program's `System.Object` resolves to: `mscorlib` for a
/// program `lcsc` compiled, and `System.Runtime` for one `dotnet build` compiled. There can be more
/// than one: a program compiled against the class library's corlib names `System.Object` through
/// both `mscorlib` and `corlib`.
fn refuse_libraries(program: &str, assembly: &Assembly) -> Result<(), String> {
    let core: Vec<u32> = assembly
        .type_refs()
        .filter(|type_ref| {
            type_ref
                .name()
                .is_some_and(|name| (name.namespace, name.name) == ("System", "Object"))
        })
        .map(|type_ref| type_ref.resolution_scope())
        .filter(|scope| scope.table() == lamella_metadata::tables::table::ASSEMBLY_REF)
        .map(|scope| scope.row())
        .collect();
    let libraries: Vec<&str> = (1..)
        .map_while(|index| {
            assembly
                .assembly_ref(index)
                .map(|reference| (index, reference))
        })
        .filter(|(index, _)| !core.contains(index))
        .map(|(_, reference)| reference.name().unwrap_or("an assembly with no name"))
        .collect();
    let named = match libraries.as_slice() {
        [] => return Ok(()),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    };
    Err(format!(
        "{program} is compiled against {named}. An assembly debugged directly is built with no \
         library linked in, so that code would not be on the board.\n\n\
         Debug the project instead: its build for a debug probe writes an ELF that links what \
         the program is compiled against, and this debugger reads the ELF."
    ))
}

/// Why a program did not build on the tier this server builds an assembly on, and where it does
/// build.
///
/// The backend's own sentence comes first, rendered as the prose it was written as. What follows is
/// the part only a debugger can say: the tier that would build the program is debugged from a file
/// `lamella build` writes, not from the assembly, so the way out is a different program argument.
fn flat_refusal(program: &str, error: &build::BuildError) -> String {
    format!(
        "the ahead-of-time build of {program} failed: {error}\n\n\
         An assembly debugged directly is built on its own, with nothing linked in, so floating \
         point, allocation, and anything that reaches the class library do not build that way.\n\n\
         A program that needs those is debugged from the ELF the project's build for a debug \
         probe writes."
    )
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
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

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

    /// A bench of one Raspberry Pi Debug Probe and one ST-LINK/V2-1, as a launch is matched against.
    fn bench() -> Vec<AttachedProbe> {
        vec![
            AttachedProbe {
                serial: Some("E660C0D1A2B3C4D5".to_owned()),
                st_link: false,
                product_id: 0x000c,
            },
            AttachedProbe {
                serial: Some("066DFF485451707867".to_owned()),
                st_link: true,
                product_id: 0x374b,
            },
        ]
    }

    /// The Debug Probe's serial number on [`bench`].
    const DEBUG_PROBE: &str = "E660C0D1A2B3C4D5";

    fn plan(serial: &str, board: Option<&str>, attach: bool) -> Result<ProbePlan, String> {
        ProbePlan::new(serial, board, "program.elf", attach)
    }

    fn refusal(serial: &str, board: Option<&str>, attach: bool) -> String {
        match plan(serial, board, attach) {
            Ok(_) => panic!("{serial} {board:?} attach={attach} should have been refused"),
            Err(reason) => reason,
        }
    }

    /// A launch's board chooses the part that board carries, and the IDCODE that part answers.
    #[test]
    fn a_board_chooses_its_part_and_the_idcode_that_part_answers() {
        let plan = plan(DEBUG_PROBE, Some("bbc-micro-bit-v1"), false).expect("planned");
        assert!(plan.part == Part::Nrf51, "the v1 carries an nRF51");
        assert!(plan.part_named, "and the part counts as named");
        let board = plan.board.expect("the board is kept");
        assert_eq!(board.id, "bbc-micro-bit-v1");
        assert_eq!(board.idcode, Some(0x0BB1_1477), "its part row's IDCODE");
    }

    /// The NUCLEO-U5A5ZJ-Q's family chooses the U5 part, which is written through the route `lamella
    /// flash` takes for an STM32U5 rather than attached to only.
    #[cfg(feature = "st")]
    #[test]
    fn the_nucleo_u5a5_chooses_the_u5_part_written_through_its_route() {
        let plan = ProbePlan::new("066DFF485451707867", Some("st-nucleo-u5a5zj-q"), "program.elf", false)
            .expect("a board this build writes");
        assert!(plan.part == Part::Stm32U5 && plan.part_named);
        assert_eq!(plan.part.route_family(), Some(lamella_flash_routes::StFamily::U5));
        assert_eq!(plan.part.flash_base(), 0x0800_0000);
        assert!(plan.part.low_power_debug().is_none(), "the U5's route attaches plainly");
    }

    /// The micro:bit v2's family chooses the nRF52833 part, a part this debugger writes, where the board
    /// was refused as one it cannot write and reached only by an attach. Its flash is at zero, as the
    /// v1's is.
    #[test]
    fn the_micro_bit_v2_chooses_the_nrf52833_part_written_through_its_route() {
        let plan = plan(DEBUG_PROBE, Some("bbc-micro-bit-v2"), false).expect("a board this build writes");
        assert!(plan.part == Part::Nrf52833 && plan.part_named);
        assert_eq!(plan.part.flash_base(), 0);
    }

    /// **AN RP2350 BOARD IS WRITTEN, NOT ONLY ATTACHED TO.** Its family chooses the RP2350 part,
    /// whose flash the core executes at `0x10000000`, so an ELF linked there is written and one linked
    /// elsewhere is refused as another board's.
    #[test]
    fn an_rp2350_board_chooses_the_rp2350_part_and_its_flash_at_0x10000000() {
        let plan = plan(DEBUG_PROBE, Some("raspberrypi-pico-2-w"), false)
            .unwrap_or_else(|reason| panic!("an RP2350 board is written: {reason}"));
        assert!(plan.part_named, "the board's part counts as named");
        assert!(plan.part == Part::Rp2350, "the Pico 2 W carries an RP2350");
        assert_eq!(plan.part.flash_base(), 0x1000_0000);
        let here = Producer::Elf {
            image_base: 0x1000_0000,
        };
        assert!(refuse_the_write(&plan, "Tick.elf", here).is_ok());
        let elsewhere = Producer::Elf {
            image_base: 0x0800_0000,
        };
        assert!(
            refuse_the_write(&plan, "Tick.elf", elsewhere)
                .is_err_and(|reason| reason.contains("built for another board")),
            "an ELF linked for another part's flash is refused"
        );
    }

    /// An ELF's entry is read from its header with the Thumb bit cleared; anything else has none.
    #[test]
    fn an_elfs_entry_is_its_headers_with_the_thumb_bit_cleared() {
        let mut header = b"\x7fELF\x01\x01\x01".to_vec();
        header.resize(28, 0);
        header[24..28].copy_from_slice(&0x1000_0101_u32.to_le_bytes());
        assert_eq!(elf_entry(&header), Some(0x1000_0100));
        let mut wide = header.clone();
        wide[4] = 2;
        assert_eq!(elf_entry(&wide), None, "a 64-bit ELF is not read as a 32-bit one");
        assert_eq!(elf_entry(b"MZ\x90\x00"), None, "an assembly has no ELF header");
    }

    /// A linked ELF32 executable for an Arm part with one loadable segment per `(physical address,
    /// bytes)`, and no sections: what `lamella flash` reads an image's regions from.
    fn linked_elf(segments: &[(u32, &[u8])]) -> Vec<u8> {
        const HEADER: usize = 52;
        const PROGRAM_HEADER: usize = 32;
        let word = |value: usize| u32::try_from(value).expect("a 32-bit field");
        let half = |value: usize| u16::try_from(value).expect("a 16-bit field");
        let mut elf = vec![0u8; HEADER + PROGRAM_HEADER * segments.len()];
        elf[..7].copy_from_slice(&[0x7F, b'E', b'L', b'F', 1, 1, 1]);
        elf[16..18].copy_from_slice(&2u16.to_le_bytes());
        elf[18..20].copy_from_slice(&40u16.to_le_bytes());
        elf[20..24].copy_from_slice(&1u32.to_le_bytes());
        elf[28..32].copy_from_slice(&word(HEADER).to_le_bytes());
        elf[40..42].copy_from_slice(&half(HEADER).to_le_bytes());
        elf[42..44].copy_from_slice(&half(PROGRAM_HEADER).to_le_bytes());
        elf[44..46].copy_from_slice(&half(segments.len()).to_le_bytes());
        for (index, (address, bytes)) in segments.iter().enumerate() {
            let at = HEADER + PROGRAM_HEADER * index;
            let (offset, length) = (word(elf.len()), word(bytes.len()));
            for (field, value) in
                [(0, 1), (4, offset), (8, *address), (12, *address), (16, length), (20, length)]
            {
                elf[at + field..at + field + 4].copy_from_slice(&value.to_le_bytes());
            }
            elf.extend_from_slice(bytes);
        }
        elf
    }

    /// A file with a half for the board's quad-SPI flash is read as `lamella flash` reads it: the
    /// half the part boots from, where that half goes, and the window's half, rather than one image
    /// across the gap of over two gigabytes between 0x08000000 and 0x90000000. A file with nothing
    /// for the window, and a board with no external flash, are read as one image, as before.
    #[cfg(feature = "st")]
    #[test]
    fn a_file_with_a_half_for_the_boards_external_flash_is_read_in_its_two_halves() {
        let dir = std::env::temp_dir().join(format!("lamella-debug-window-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        let boot = [0x00, 0x00, 0x08, 0x20, 0x09, 0x00, 0x00, 0x08];
        let window = [0x52, 0x48, 0x55, 0x58, 0x01, 0x00, 0x00, 0x00, 0x00, 0xBF, 0x00, 0xBF];
        let read = |name: &str, board: &str, segments: &[(u32, &[u8])]| {
            let path = dir.join(name);
            std::fs::write(&path, linked_elf(segments)).expect("write the program");
            let plan = ProbePlan::new(
                "066DFF485451707867",
                Some(board),
                &path.to_string_lossy(),
                false,
            )
            .expect("planned");
            read_the_set(&plan)
        };

        let (base, image, half) = read(
            "two.elf",
            "st-stm32f769i-disco",
            &[(0x0800_0000, &boot), (0x9000_0000, &window)],
        )
        .expect("the halves are read")
        .expect("a half for the window");
        assert_eq!(base, 0x0800_0000, "the part's half goes where the part boots from");
        assert_eq!(image, boot, "and is the part's half alone");
        assert_eq!(half.half.base, 0x9000_0000, "the window's half goes at the window");
        assert_eq!(half.half.bytes, window, "and is the window's bytes");
        assert_eq!(half.flash.window, 0x9000_0000, "the board's external flash is the F769's");

        let one = read("one.elf", "st-stm32f769i-disco", &[(0x0800_0000, &boot)]);
        assert!(matches!(one, Ok(None)), "a file with nothing for the window is one image");
        let elsewhere = read(
            "nucleo.elf",
            "st-nucleo-f429zi",
            &[(0x0800_0000, &boot), (0x9000_0000, &window)],
        );
        assert!(matches!(elsewhere, Ok(None)), "a board with no external flash reads no window");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Main` is the build's entry when that is a `Main`, or else the one function named `Main` or
    /// `<Main>$`; with none, or with more than one, there is no `Main` to stop in.
    #[test]
    fn main_is_the_one_function_named_for_it() {
        let named = |list: &[(u32, &str)]| -> Vec<(u32, u32, String)> {
            list.iter()
                .map(|&(start, name)| (start, start + 8, name.to_owned()))
                .collect()
        };
        let ours = named(&[(0x100, "?"), (0x108, "Tick.Main")]);
        assert_eq!(program_main(&ours, "?", 0x1000_0000), Some(0x1000_0108));
        let theirs = named(&[(0x6_446c, "Reset"), (0x7_846c, "<Main>$"), (0xa_b790, "GetMainMethodArguments")]);
        assert_eq!(program_main(&theirs, "Reset", 0x1000_0000), Some(0x1007_846c));
        let two = named(&[(0x10, "A.Main"), (0x40, "B.Main")]);
        assert_eq!(program_main(&two, "?", 0), None, "two `Main`s are not guessed between");
        assert_eq!(
            program_main(&two, "B.Main", 0),
            Some(0x40),
            "the build's own entry decides between them"
        );
        let wrapped = named(&[(0x10, "Program.<Main>"), (0x40, "Program.Main"), (0x80, "Program.<Main>d__0.MoveNext")]);
        assert_eq!(
            program_main(&wrapped, "?", 0),
            Some(0x40),
            "an async `Main`'s wrapper and state machine are not `Main`"
        );
        assert_eq!(program_main(&named(&[(0x10, "Reset")]), "Reset", 0), None);
    }

    /// A core that runs its program as a list of addresses from its reset, for the runs a deploy
    /// makes once its part is held: a resumed core stops at the first address on its way that a
    /// breakpoint is armed at, and spins at the last address forever when none is. Only what a hold
    /// and a run make is answered; anything else panics.
    struct ScriptedCore {
        /// The addresses the program executes in order from its reset; the last is a loop.
        path: Vec<u32>,
        /// Where on `path` the core is.
        at: usize,
        halted: bool,
        armed: Vec<u32>,
        /// Where the program itself halts the core, as a `BKPT` instruction does.
        bkpt: Vec<u32>,
        /// `DEMCR` as last written: its vector catch decides whether a reset stops the core.
        demcr: u32,
        /// The core's PC, shared with the test, since the core moves into whatever is built over it.
        pc: Rc<Cell<u32>>,
        /// What each write of the breakpoints left armed, shared the same way.
        arming: Rc<RefCell<Vec<Vec<u32>>>>,
        /// Where the program's console ring is, whose magic word is the one word of RAM a session
        /// may write.
        ring: Option<u32>,
        /// Each word written there, with the core's PC when it was: shared the same way.
        ring_writes: Rc<RefCell<Vec<(u32, u32)>>>,
        /// Whether a reset that should hold the core at its first instruction fails instead, as a
        /// part's that cannot be held after a write does.
        cannot_be_held: bool,
        /// The words of a probe that gave up on its link: from then on a look at the core fails, as
        /// a silent link's does.
        gave_up: Option<String>,
    }

    impl ScriptedCore {
        /// A core running `path`'s loop, as a part a write has just started is.
        fn running(path: &[u32]) -> Self {
            let mut core = ScriptedCore {
                path: path.to_vec(),
                at: path.len() - 1,
                halted: false,
                armed: Vec::new(),
                bkpt: Vec::new(),
                demcr: 0,
                pc: Rc::new(Cell::new(0)),
                arming: Rc::new(RefCell::new(Vec::new())),
                ring: None,
                ring_writes: Rc::new(RefCell::new(Vec::new())),
                cannot_be_held: false,
                gave_up: None,
            };
            core.moved();
            core
        }

        fn moved(&mut self) {
            self.pc.set(self.path[self.at]);
        }

        /// Runs from where the core is to the next address on its path that a breakpoint is armed at
        /// or that halts the core itself, or else to its loop.
        fn run(&mut self, from: usize) {
            let stops = |address: &u32| self.armed.contains(address) || self.bkpt.contains(address);
            match (from..self.path.len()).find(|&index| stops(&self.path[index])) {
                Some(index) => {
                    self.at = index;
                    self.halted = true;
                }
                None => {
                    self.at = self.path.len() - 1;
                    self.halted = false;
                }
            }
            self.moved();
        }
    }

    /// The Debug Exception and Monitor Control Register and the Application Interrupt and Reset
    /// Control Register, which a hold through the part's own reset writes, as it does `DHCSR`.
    const DEMCR: u32 = 0xE000_EDFC;
    const AIRCR: u32 = 0xE000_ED0C;

    impl TargetAccess for ScriptedCore {
        fn connect(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_idcode(&mut self) -> Result<u32, ProbeError> {
            Ok(0x4C01_3477)
        }
        fn init_mem(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn write_word(&mut self, address: u32, value: u32) -> Result<(), ProbeError> {
            match address {
                DHCSR => {}
                DEMCR => self.demcr = value,
                AIRCR if value & 0b100 != 0 => {
                    self.at = 0;
                    self.moved();
                    if self.demcr & 1 != 0 {
                        self.halted = true;
                    } else {
                        self.run(0);
                    }
                }
                ring if Some(ring) == self.ring => self.ring_writes.borrow_mut().push((value, self.pc.get())),
                _ => unreachable!("a hold writes no other word, {address:#010x}"),
            }
            Ok(())
        }
        fn read_word(&mut self, address: u32) -> Result<u32, ProbeError> {
            unreachable!("a hold and a run read no memory, {address:#010x}")
        }
        fn read_words_into(&mut self, _address: u32, _out: &mut [u32]) -> Result<(), ProbeError> {
            unreachable!("a hold and a run read no memory")
        }
        fn write_words(&mut self, _address: u32, _words: &[u32]) -> Result<(), ProbeError> {
            unreachable!("a hold and a run write no memory")
        }
        fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
            unreachable!("a hold and a run read no memory")
        }
        fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
            unreachable!("a hold and a run write no memory")
        }
        fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
            unreachable!("a hold and a run read no memory")
        }
        fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
            unreachable!("a hold and a run write no memory")
        }
        fn halt(&mut self) -> Result<(), ProbeError> {
            self.halted = true;
            Ok(())
        }
        fn resume(&mut self) -> Result<(), ProbeError> {
            assert!(self.halted, "only a halted core is resumed");
            self.run(self.at + 1);
            Ok(())
        }
        fn step(&mut self) -> Result<(), ProbeError> {
            unreachable!("a hold and a run do not step")
        }
        fn is_halted(&mut self) -> Result<bool, ProbeError> {
            if self.gave_up.is_some() {
                return Err(ProbeError::Transport("probe transport error: USB read timed out".into()));
            }
            Ok(self.halted)
        }
        fn stopped_answering(&self) -> Option<String> {
            self.gave_up.clone()
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            unreachable!("a run polls rather than waits")
        }
        fn read_core_reg(&mut self, selector: u8) -> Result<u32, ProbeError> {
            assert_eq!(selector, 15, "a run reads the PC alone");
            assert!(self.halted, "a core register is read only while the core is halted");
            Ok(self.path[self.at])
        }
        fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
            unreachable!("a hold and a run write no core register")
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            if self.cannot_be_held {
                return Err(ProbeError::Timeout("the core to halt"));
            }
            self.at = 0;
            self.halted = true;
            self.moved();
            Ok(())
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            self.at = 0;
            self.run(0);
            Ok(())
        }
        fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
            unreachable!("a hold does not drive the reset line")
        }
        fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("the part's own reset arms its catch through DEMCR")
        }
        fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
            unreachable!("the part's own reset disarms its catch through DEMCR")
        }
        fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
            unreachable!("a run arms its one breakpoint as the whole set")
        }
        fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
            unreachable!("a run clears the whole set")
        }
        fn set_breakpoints(&mut self, addresses: &[u32]) -> Result<(), ProbeError> {
            self.armed = addresses.to_vec();
            self.arming.borrow_mut().push(addresses.to_vec());
            Ok(())
        }
        fn call_target(
            &mut self,
            _address: u32,
            _args: &[u32],
            _frame: &lamella_probe_core::CallFrame,
        ) -> Result<u32, ProbeError> {
            unreachable!("a hold calls nothing on the part")
        }
    }

    /// The program the tests below run on an RP2350: two addresses in its boot ROM, then the image's
    /// entry, `Main` and a loop in `Main`, where an AOT C# image puts them.
    const BOOT_ROM: u32 = 0x0000_7642;
    const IMAGE_ENTRY: u32 = 0x1000_0100;
    const MAIN: u32 = 0x1000_0108;
    const LOOP: u32 = 0x1000_0150;
    const RP2350_PATH: [u32; 5] = [BOOT_ROM, 0x0000_1a2c, IMAGE_ENTRY, MAIN, LOOP];

    /// **THE RESET VECTOR'S HOLD**, which an nRF51 or STM32 write leaves: the part reset and held at
    /// its first instruction. A launch that stops at its entry runs on from there to `Main`, and the
    /// breakpoint it took is gone again.
    #[test]
    fn a_part_held_at_its_reset_vector_runs_on_to_main_for_stop_at_entry() {
        let core = ScriptedCore::running(&[0x0000_00d4, 0x0000_0100, MAIN, LOOP]);
        let (pc, arming) = (Rc::clone(&core.pc), Rc::clone(&core.arming));
        let mut core = leave_held(core).expect("held");
        assert_eq!(pc.get(), 0x0000_00d4, "held at its reset vector");
        stop_at_main(&mut core, Some(MAIN)).expect("run to Main");
        assert_eq!(pc.get(), MAIN, "held at the first line of Main");
        assert!(core.halted);
        assert_eq!(arming.borrow().last(), Some(&Vec::new()), "no breakpoint is left armed");
    }

    /// **THE RP2350's HOLD**: written and started by `lamella flash`'s write, then reset with its core
    /// caught in the boot ROM and run to its image's entry -- and from there, for `stopAtEntry`, on to
    /// `Main` by the same path every part takes.
    #[test]
    fn an_rp2350_is_held_at_its_image_entry_and_runs_on_to_main_for_stop_at_entry() {
        let core = ScriptedCore::running(&RP2350_PATH);
        let (pc, arming) = (Rc::clone(&core.pc), Rc::clone(&core.arming));
        let mut core = hold_rp2350(core, None, Some(IMAGE_ENTRY)).expect("held");
        assert_eq!(pc.get(), IMAGE_ENTRY, "past the boot ROM, on the image's first instruction");
        assert!(core.halted);
        assert_eq!(arming.borrow().as_slice(), &[vec![IMAGE_ENTRY], Vec::new()]);
        stop_at_main(&mut core, Some(MAIN)).expect("run to Main");
        assert_eq!(pc.get(), MAIN);
        assert_eq!(arming.borrow().last(), Some(&Vec::new()), "no breakpoint is left armed");
    }

    /// **A PART THAT COULD NOT BE HELD** runs its program already, so there is no first instruction
    /// to run on from: it is left running, nothing is armed, and the reason is said.
    #[test]
    fn a_part_that_could_not_be_held_is_left_running_rather_than_run_to_main() {
        let mut core = ScriptedCore::running(&[0x0000_00d4, MAIN, LOOP]);
        stop_at_main(&mut core, Some(MAIN)).expect("not an error");
        assert!(!core.halted, "still running");
        assert!(core.arming.borrow().is_empty(), "nothing was armed");
        assert_eq!(
            crate::launch::take_said(),
            [String::from(
                "stopAtEntry: the part could not be held before its program ran, so the session stops wherever \
                 the program is"
            )],
            "said for the session's Debug Console"
        );
    }

    /// **A PROBE THAT STOPPED ANSWERING IS NOT A PART LEFT RUNNING.** A look at the core that fails
    /// because the probe gave up on its link ends the start in the probe's words, and nothing is said
    /// of a part that could not be held.
    #[test]
    fn a_probe_that_stopped_answering_ends_the_start_rather_than_reading_as_a_part_left_running() {
        let words = lamella_probe_core::probe_stopped_answering("USB read timed out");
        let mut core = ScriptedCore::running(&[0x0000_00d4, MAIN, LOOP]);
        core.halted = true;
        core.gave_up = Some(words.clone());
        let reason = stop_at_main(&mut core, Some(MAIN)).expect_err("the start ends");
        assert!(reason.ends_with(&words), "in the probe's words: {reason}");
        assert!(crate::launch::take_said().is_empty(), "nothing is said of a part left running");
        assert!(core.arming.borrow().is_empty(), "nothing was armed");
    }

    /// **THE RUN TO AN ADDRESS ENDS AT ONCE AT A PROBE THAT STOPPED ANSWERING**, rather than looking
    /// again until its time is up: a look that fails while a part starts is retried, but a probe that
    /// gave up on its link answers nothing more.
    #[test]
    fn a_run_to_an_address_ends_at_once_at_a_probe_that_stopped_answering() {
        let words = lamella_probe_core::probe_stopped_answering("USB read timed out");
        let mut core = ScriptedCore::running(&[0x0000_00d4, MAIN, LOOP]);
        core.halted = true;
        core.at = 0;
        core.gave_up = Some(words.clone());
        let started = std::time::Instant::now();
        let error = run_to(&mut core, MAIN, RUN_TO_WAIT).expect_err("the run ends");
        assert!(started.elapsed() < RUN_TO_WAIT, "at once, not when its time is up");
        assert!(matches!(error, ProbeError::Transport(_)), "{error}");
    }

    /// **WHERE THE SESSION STOPS IN PLACE OF `Main` IS SAID ON ITS DEBUG CONSOLE**, for each reason it
    /// could not stop there: no single `Main`, a stop of the program's own before it, and no arrival
    /// in time.
    #[test]
    fn where_the_session_stops_in_place_of_main_is_said_for_its_debug_console() {
        let mut core = ScriptedCore::running(&[0x0000_00d4, MAIN, LOOP]);
        core.halted = true;
        core.at = 0;
        stop_at_main(&mut core, None).expect("not an error");
        assert_eq!(
            crate::launch::take_said(),
            [String::from(
                "stopAtEntry: the program's debug information names no single Main, so the session stops at \
                 the program's entry"
            )]
        );

        let mut core = ScriptedCore::running(&[0x0000_00d4, 0x0000_0102, MAIN, LOOP]);
        core.bkpt = vec![0x0000_0102];
        core.halted = true;
        core.at = 0;
        stop_at_main(&mut core, Some(MAIN)).expect("not an error");
        assert_eq!(
            crate::launch::take_said(),
            [String::from(
                "stopAtEntry: the program stopped at 0x00000102 before it reached Main, and the session stops \
                 there"
            )]
        );

        let mut core = ScriptedCore::running(&[0x0000_00d4, LOOP]);
        core.halted = true;
        core.at = 0;
        stop_at_main(&mut core, Some(MAIN)).expect("not an error");
        assert_eq!(
            crate::launch::take_said(),
            [format!(
                "stopAtEntry: the program did not reach Main within {} seconds, so the session stops where it \
                 was, at {LOOP:#010x}",
                RUN_TO_WAIT.as_secs()
            )]
        );
    }

    /// **A PART THAT COULD NOT BE HELD AFTER THE WRITE SAYS SO ON THE SESSION'S DEBUG CONSOLE**, with
    /// the breakpoints that consequence costs, and runs the new image instead.
    #[test]
    fn a_part_not_held_after_the_write_says_so_for_the_sessions_debug_console() {
        let mut core = ScriptedCore::running(&[0x0000_00d4, MAIN, LOOP]);
        core.cannot_be_held = true;
        let core = leave_held(core).expect("let go rather than failing the deploy");
        assert!(!core.halted, "running the new image");
        let said = crate::launch::take_said();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].starts_with("reset: this part could not be held at its entry after the deploy (")
                && said[0].ends_with(
                    "), so it is running the new image instead. A breakpoint on a line that runs before the \
                     session attaches will not be hit."
                ),
            "{said:?}"
        );
    }

    /// An RP2350 image that names no entry is held at the boot ROM's reset vector, and the session's
    /// Debug Console says so.
    #[test]
    fn an_rp2350_image_with_no_entry_is_held_at_its_boot_rom_and_says_so() {
        let core = ScriptedCore::running(&RP2350_PATH);
        let _core = hold_rp2350(core, None, None).expect("held");
        assert_eq!(
            crate::launch::take_said(),
            [String::from(
                "reset: the image names no entry, so the part is held at its boot ROM's reset vector, before \
                 the program runs"
            )]
        );
    }

    /// A program that never reaches the address it is run to is halted where it is when the time is
    /// up, with nothing left armed; one that stops somewhere else first says where.
    #[test]
    fn a_run_that_does_not_arrive_says_where_the_core_stopped() {
        let mut core = ScriptedCore::running(&[0x0000_00d4, LOOP]);
        core.halted = true;
        core.at = 0;
        let wait = std::time::Duration::from_millis(5);
        assert_eq!(run_to(&mut core, MAIN, wait), Ok(RunTo::HaltedAt(LOOP)));
        assert!(core.halted);
        assert_eq!(core.arming.borrow().last(), Some(&Vec::new()));

        let mut core = ScriptedCore::running(&[0x0000_00d4, 0x0000_0102, MAIN, LOOP]);
        core.bkpt = vec![0x0000_0102];
        core.halted = true;
        core.at = 0;
        assert_eq!(run_to(&mut core, MAIN, wait), Ok(RunTo::StoppedAt(0x0000_0102)));
        assert!(core.halted);
        assert_eq!(core.arming.borrow().last(), Some(&Vec::new()));
    }

    /// **THE LAUNCH, END TO END AT THE DESK**: an RP2350 launch that stops at its entry is held at the
    /// first line of `Main` before the session's backend is built over it; one that does not stop at
    /// its entry is held at the image's entry, before anything of the program runs.
    #[test]
    fn an_rp2350_launch_is_held_before_its_program_and_at_main_for_stop_at_entry() {
        let program = || lamella_debug_device::program::DebugProgram {
            image: Vec::new(),
            image_base: 0x1000_0000,
            lines: Vec::new(),
            names: vec![(0x108, 0x158, String::from("Tick.Main"))],
            files: Vec::new(),
            entry: String::from("?"),
            frames: Vec::new(),
            locals: lamella_debug_device::program::LocalSections::default(),
            unwind: lamella_debug_device::program::UnwindTables::default(),
            console: None,
        };
        for (stop_at_entry, held_at) in [(true, MAIN), (false, IMAGE_ENTRY)] {
            let plan = plan(DEBUG_PROBE, Some("raspberrypi-pico-2-w"), false)
                .expect("planned")
                .stopping_at_entry(stop_at_entry);
            let core = ScriptedCore::running(&RP2350_PATH);
            let pc = Rc::clone(&core.pc);
            let prepared = prepare(core, None, &plan, program(), Some(IMAGE_ENTRY), None);
            assert!(prepared.is_ok(), "stopAtEntry {stop_at_entry}: the session is served");
            assert_eq!(pc.get(), held_at, "stopAtEntry {stop_at_entry}");
        }
    }

    /// **A WRITE CLEARS THE CONSOLE RING THE LAST RUN LEFT, BEFORE THE NEW PROGRAM RUNS.** Its magic
    /// word is cleared while the part is held at the image's entry -- for a launch that stops at its
    /// entry, before the run to `Main` -- so no look at the ring reads the last run's output or its
    /// end as this one's. An attach takes the program already running, and leaves its ring alone.
    #[test]
    fn a_write_clears_the_last_runs_console_ring_before_the_new_program_runs_and_an_attach_does_not() {
        const RING: u32 = 0x2004_1930;
        let program = || lamella_debug_device::program::DebugProgram {
            image: Vec::new(),
            image_base: 0x1000_0000,
            lines: Vec::new(),
            names: vec![(0x108, 0x158, String::from("Tick.Main"))],
            files: Vec::new(),
            entry: String::from("?"),
            frames: Vec::new(),
            locals: lamella_debug_device::program::LocalSections::default(),
            unwind: lamella_debug_device::program::UnwindTables::default(),
            console: Some(lamella_debug_device::program::DataObject { address: RING, size: 16 + 16384 }),
        };
        for (stop_at_entry, attach) in [(true, false), (false, false), (false, true)] {
            let plan = plan(DEBUG_PROBE, Some("raspberrypi-pico-2-w"), attach)
                .expect("planned")
                .stopping_at_entry(stop_at_entry);
            let mut core = ScriptedCore::running(&RP2350_PATH);
            core.ring = Some(RING);
            let writes = Rc::clone(&core.ring_writes);
            let prepared = prepare(core, None, &plan, program(), Some(IMAGE_ENTRY), None);
            assert!(prepared.is_ok(), "stopAtEntry {stop_at_entry}, attach {attach}: the session is served");
            let expected = if attach { Vec::new() } else { vec![(0, IMAGE_ENTRY)] };
            assert_eq!(*writes.borrow(), expected, "stopAtEntry {stop_at_entry}, attach {attach}");
        }
    }

    /// **A BOARD THIS BUILD CANNOT WRITE IS STILL DEBUGGED.** This debugger has no write for the
    /// Raspberry Pi Pico's RP2040, so a write is refused in those words and no others, and an attach
    /// is served.
    #[test]
    fn a_board_this_build_cannot_write_is_refused_for_a_write_and_served_for_an_attach() {
        let reason = refusal(DEBUG_PROBE, Some("raspberrypi-pico"), false);
        assert_eq!(reason, "this debugger cannot write raspberrypi-pico");

        let plan = plan(DEBUG_PROBE, Some("raspberrypi-pico"), true).expect("an attach writes nothing");
        assert!(!plan.part_named, "no part is claimed for it");
        let board = plan.board.expect("the board is kept for the attach's check");
        assert_eq!(board.idcode, Some(0x0BC1_2477));
    }

    /// **SERVED MEANS A SESSION, NOT A PLAN.** The attach planned above, against a part answering the
    /// Pico's IDCODE: this tier reaches the part, and the backend it prepares takes the program running
    /// there, which a session needs before it can arm a breakpoint.
    #[test]
    fn an_attach_this_tier_plans_is_taken_by_the_backend_it_prepares() {
        let plan = plan(DEBUG_PROBE, Some("raspberrypi-pico"), true).expect("an attach writes nothing");
        let the_picos_part = Nrf51Part {
            answers: Some(0x0BC1_2477),
            ..Nrf51Part::default()
        };
        let program = lamella_debug_device::program::DebugProgram {
            image: Vec::new(),
            image_base: 0,
            lines: Vec::new(),
            names: Vec::new(),
            files: Vec::new(),
            entry: String::from("Program.Main"),
            frames: Vec::new(),
            locals: lamella_debug_device::program::LocalSections::default(),
            unwind: lamella_debug_device::program::UnwindTables::default(),
            console: None,
        };
        let mut backend = prepare(the_picos_part, None, &plan, program, None, None)
            .unwrap_or_else(|reason| panic!("the part the Pico carries is reached: {reason}"));
        assert_eq!(backend.attach(), Ok(()), "the running program is taken, not refused");
    }

    /// An id the catalog does not know is refused in the catalog's own words, which name the
    /// listing, rather than standing in for a default part.
    #[test]
    fn an_unknown_board_is_refused_naming_the_listing() {
        let reason = refusal(DEBUG_PROBE, Some("bbc-micro-bit-v9"), false);
        assert!(
            reason.contains("bbc-micro-bit-v9") && reason.contains("lamella boards"),
            "{reason}"
        );
    }

    /// **A WRITE NAMES ITS PART, FOR AN ASSEMBLY AS FOR AN ELF, BEFORE ANYTHING IS COMPARED WITH IT.**
    /// With no part named, the default part's flash algorithm would be driven against whatever part
    /// the probe reaches; and an ELF linked high is not told it was built for another board when the
    /// build named none. An attach writes nothing, so it is never refused here.
    #[test]
    fn a_write_names_its_part_for_an_assembly_as_for_an_elf_before_its_base_is_compared() {
        let unnamed = plan(DEBUG_PROBE, None, false).expect("planned");
        for producer in [
            Producer::Elf { image_base: 0x1000_0000 },
            Producer::Elf { image_base: 0 },
            Producer::Assembly,
        ] {
            let reason = refuse_the_write(&unnamed, "App", producer).expect_err("no part is named");
            assert!(
                reason.contains("the build named no board") && !reason.contains("linked to load at"),
                "{producer:?}: {reason}"
            );
            assert!(
                reason.ends_with("Build the project for the board you debug on.")
                    && !reason.contains("attach")
                    && !reason.contains('`'),
                "{producer:?}: {reason}"
            );
        }
        let attach = plan(DEBUG_PROBE, None, true).expect("planned");
        for producer in [Producer::Elf { image_base: 0x1000_0000 }, Producer::Assembly] {
            assert!(refuse_the_write(&attach, "App", producer).is_ok(), "{producer:?}");
        }
        let v1 = plan(DEBUG_PROBE, Some("bbc-micro-bit-v1"), false).expect("planned");
        assert!(refuse_the_write(&v1, "App", Producer::Assembly).is_ok());
        assert!(refuse_the_write(&v1, "App", Producer::Elf { image_base: 0 }).is_ok());
        let reason = refuse_the_write(&v1, "App", Producer::Elf { image_base: 0x1000_0000 })
            .expect_err("linked for another part");
        assert!(reason.contains("linked to load at 0x10000000"), "{reason}");
    }

    /// **AN ASSEMBLY IS WRITTEN ONLY TO THE PART ITS IMAGE IS BUILT FOR.** The flat tier builds the
    /// micro:bit v1's image, so an assembly for a board on another part is refused, naming the board,
    /// rather than written there by that part's flash algorithm and read back as a success.
    #[cfg(feature = "st")]
    #[test]
    fn an_assembly_is_written_only_to_the_part_the_flat_tier_builds_for() {
        let f4 = plan(DEBUG_PROBE, Some("st-nucleo-f429zi"), false).expect("planned");
        let reason = refuse_the_write(&f4, "App.dll", Producer::Assembly).expect_err("another part");
        assert!(
            reason.contains("st-nucleo-f429zi") && reason.contains("nothing was written"),
            "{reason}"
        );
        let attach = plan(DEBUG_PROBE, Some("st-nucleo-f429zi"), true).expect("planned");
        assert!(refuse_the_write(&attach, "App.dll", Producer::Assembly).is_ok(), "an attach writes nothing");
    }

    /// **EVERY FAMILY THE MAP NAMES IS ONE A BOARD CARRIES.** The map is keyed by the catalog's
    /// family strings, so a family renamed there would make a launch refuse to write a board this
    /// build can write, and nothing else would notice.
    #[test]
    fn every_family_the_part_map_names_is_carried_by_a_catalog_board() {
        let families: std::collections::BTreeSet<String> = lamella_catalog::BOARDS
            .iter()
            .filter_map(|(id, _)| lamella_catalog::load_board(id))
            .map(|board| board.family)
            .collect();
        #[allow(unused_mut)]
        let mut mapped = vec!["nrf51", "nrf52833"];
        #[cfg(feature = "sam")]
        mapped.push("samd21");
        #[cfg(feature = "st")]
        mapped.extend(["stm32f091", "stm32f42x", "stm32f7", "stm32f769", "stm32h7", "stm32u5a5"]);
        for family in mapped {
            assert!(
                families.contains(family),
                "no catalog board carries {family:?}"
            );
            assert!(
                matches!(part_of_family(family), Ok(Some(_))),
                "{family:?} maps to a part"
            );
        }
    }

    /// The IDCODE check refuses only a mismatch, and says nothing was written.
    #[test]
    fn the_board_check_refuses_a_part_answering_another_idcode() {
        let v1 = NamedBoard {
            id: "bbc-micro-bit-v1".to_owned(),
            part: Some(Part::Nrf51),
            idcode: Some(0x0BB1_1477),
        };
        assert_eq!(check_board(0x0BB1_1477, Some(&v1)), Ok(()));
        let reason = check_board(0x2BA0_1477, Some(&v1)).expect_err("a v2 answered");
        assert!(
            reason.contains("0x2ba01477")
                && reason.contains("0x0bb11477")
                && reason.contains("Nothing was written"),
            "{reason}"
        );
        assert!(!reason.contains("IDCODE") && !reason.contains("debug port"), "{reason}");
        assert_eq!(check_board(0x2BA0_1477, None), Ok(()), "no board, no check");
        let unstated = NamedBoard { idcode: None, ..v1 };
        assert_eq!(
            check_board(0x2BA0_1477, Some(&unstated)),
            Ok(()),
            "a part row that states no IDCODE checks nothing"
        );
    }

    /// **A PROBE IS CHOSEN BY THE SERIAL NUMBER ITS DEVICE ID CARRIES, OR NOT AT ALL.** A launch
    /// whose probe is not attached is refused naming the listing, never served by whichever probe
    /// answers first: on a bench of several, that probe is on somebody else's board.
    #[test]
    fn a_probe_that_is_not_attached_is_refused_naming_the_listing() {
        let reason = probe_named("NOT-ON-THIS-BENCH", &bench()).expect_err("not attached");
        assert!(
            reason.contains("NOT-ON-THIS-BENCH") && reason.contains("lamella devices"),
            "{reason}"
        );
    }

    /// The serial number is matched as the operating system reports it, whatever its case, as
    /// `lamella devices` and the probe selector match it.
    #[test]
    fn a_probe_is_matched_by_its_serial_number_in_either_case() {
        assert!(probe_named(&DEBUG_PROBE.to_lowercase(), &bench()).is_ok());
    }

    /// **WHICH KIND OF PROBE IS THE ENUMERATION'S ANSWER, NOT A FIELD.** An ST-LINK is opened as one,
    /// with the product id that names its model, and a CMSIS-DAP probe as a CMSIS-DAP probe.
    #[cfg(feature = "st")]
    #[test]
    fn an_st_link_is_opened_as_one_with_the_product_id_of_its_model() {
        let (kind, product_id) = probe_named("066DFF485451707867", &bench()).expect("attached");
        assert!(kind == ProbeKind::StLink);
        assert_eq!(product_id, 0x374b);
        let (kind, _) = probe_named(DEBUG_PROBE, &bench()).expect("attached");
        assert!(kind == ProbeKind::CmsisDap);
    }

    /// The symbols the launch found are the ones read, wherever they are: a build may keep its PDB in
    /// another folder than its program. Without one the program is debugged by instruction, and one
    /// the launch found that cannot be read, or that is not a Portable PDB, is a refusal naming it.
    #[test]
    fn the_symbols_the_launch_found_are_read_where_they_are() {
        let dir = std::env::temp_dir().join(format!("lamella-debug-symbols-{}", std::process::id()));
        let elsewhere = dir.join("symbols");
        std::fs::create_dir_all(&elsewhere).expect("a scratch folder");
        let compiled = lamella_assemble::compile_source(
            "class Program\n{\n    static int Main()\n    {\n        int a = 40;\n        return a + 2;\n    }\n}\n",
            "Program.cs",
            "Program.dll",
            "Program",
            &[],
            true,
        );
        assert!(compiled.diagnostics.is_empty(), "{:?}", compiled.diagnostics);
        let bytes = compiled.image.expect("an assembly");
        let program = dir.join("Program.dll");
        std::fs::write(&program, &bytes).expect("write the program");
        let pdb = elsewhere.join("Program.pdb");
        std::fs::write(&pdb, compiled.pdb.expect("a PDB")).expect("write the PDB");
        let program = program.to_string_lossy();

        let found = source_lines(&program, &bytes, Some(&pdb)).expect("the flat tier builds it");
        assert!(found.lines.iter().any(|&(_, line)| line != 0), "source lines from the PDB elsewhere");
        let none = source_lines(&program, &bytes, None).expect("built");
        assert!(none.lines.iter().all(|&(_, line)| line == 0), "and by instruction without one");
        let gone = source_lines(&program, &bytes, Some(&dir.join("Gone.pdb")))
            .map(|_| ())
            .expect_err("a PDB the launch found that cannot be read");
        assert!(gone.contains("Gone.pdb"), "{gone}");
        let not_a_pdb = elsewhere.join("NotAPdb.pdb");
        std::fs::write(&not_a_pdb, b"these bytes are not a portable pdb").expect("write the file");
        let unread = source_lines(&program, &bytes, Some(&not_a_pdb))
            .map(|_| ())
            .expect_err("a file the launch found that is not a Portable PDB");
        assert!(unread.contains("NotAPdb.pdb") && unread.contains("Portable PDB"), "{unread}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A device the CMSIS-DAP enumeration passed over, with `vendor_id:product_id` and `serial`.
    fn passed_over(vendor_id: u16, product_id: u16, serial: &str) -> lamella_probe::PassedOver {
        lamella_probe::PassedOver {
            vendor_id,
            product_id,
            serial: Some(serial.to_owned()),
            product: None,
            interface_name: None,
            port_id: None,
        }
    }

    /// **AN ATTACHED ST-LINK IS LISTED AS ONE IN EVERY BUILD.** A build with `st` opens it, and a
    /// build without refuses it naming the feature that drives one -- which it can do only for a
    /// probe it listed: otherwise a launch naming an attached ST-LINK is told that no attached probe
    /// has its serial number. Driven through the listing `attached_probes` makes, so the
    /// ST-LINK here is one the enumeration passed over, as a real one is.
    #[test]
    fn an_attached_st_link_is_listed_as_one_whatever_this_build_drives() {
        let attached = probes_among(
            Vec::new(),
            vec![
                passed_over(0x0483, 0x374b, "066DFF485451707867"),
                passed_over(0x0483, 0x5740, "NOT-A-PROBE"),
            ],
        );
        assert_eq!(attached.len(), 1, "the ST-LINK and nothing else");
        let found = probe_named("066DFF485451707867", &attached);
        #[cfg(feature = "st")]
        assert!(matches!(found, Ok((ProbeKind::StLink, 0x374b))), "{:?}", found.err());
        #[cfg(not(feature = "st"))]
        {
            let reason = found.map(|_| ()).expect_err("an ST-LINK needs `st`");
            assert!(
                reason.contains("\"st\" feature") && reason.contains("lamella-debug-server"),
                "the refusal names the feature that drives one: {reason}"
            );
        }
    }

    #[cfg(not(feature = "st"))]
    #[test]
    fn an_st_link_or_an_stm32_board_on_a_build_without_st_names_the_feature_that_builds_one() {
        for reason in [
            probe_named("066DFF485451707867", &bench()).expect_err("an ST-LINK needs `st`"),
            part_of_family("stm32f7").expect_err("an STM32 needs `st`"),
        ] {
            assert!(
                reason.contains("\"st\" feature") && reason.contains("lamella-debug-server"),
                "{reason}"
            );
        }
    }

    #[cfg(not(feature = "sam"))]
    #[test]
    fn a_sam_board_on_a_build_without_sam_names_the_feature_that_builds_one() {
        let reason = part_of_family("samd21").expect_err("a SAM D21 needs `sam`");
        assert!(
            reason.contains("\"sam\" feature") && reason.contains("lamella-debug-server"),
            "{reason}"
        );
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
            attach_to_part(&mut part, Part::Nrf51, None, true, Some(attach)),
            Ok(())
        );
        assert!(part.attached_under_reset);
    }

    /// Refused with the probe's reason and no other advice: `lamella-debug` has no `--reset` option,
    /// and no launch field asks for a reset.
    #[test]
    fn without_reset_a_part_a_plain_attach_cannot_read_is_refused_with_its_reason() {
        let mut part = SleepingPart::default();
        let attach: &PartAttach<SleepingPart> = &attach_under_reset;
        let Err(reason) = attach_to_part(&mut part, Part::Nrf51, None, false, Some(attach)) else {
            panic!("a part that refuses a plain attach must not be reported attached");
        };
        assert!(
            reason.contains("0x0000001a") && !reason.contains("--reset"),
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
            attach_to_part(&mut part, Part::Nrf51, None, false, Some(attach)),
            Ok(())
        );
        assert!(
            !part.attached_under_reset,
            "an attach without --reset leaves the program running where it is"
        );
    }

    /// The nRF51's non-volatile memory controller, which the nRF52833 keeps, and the flash geometry
    /// a page erase needs.
    ///
    /// Spelled out here rather than imported from `lamella-cmsis-dap-nrf`, where they are private
    /// anyway: a fixture that shares its constants with the code under test agrees with a wrong
    /// address as readily as a right one. These are the nRF51 Series Reference Manual's NVMC
    /// registers, which is also where that crate took them from. The nRF52833 Product Specification
    /// (4452_021 v1.7, 4.3.10) gives the same base and offsets.
    mod nvmc {
        /// `READY`: bit 0 set means the controller is idle.
        pub const READY: u32 = 0x4001_E400;
        /// `CONFIG`: the write/erase enable.
        pub const CONFIG: u32 = 0x4001_E504;
        /// `ERASEPAGE`: the address of the page to erase.
        pub const ERASEPAGE: u32 = 0x4001_E508;
        /// `ERASEALL`: 1 erases the whole main flash block.
        pub const ERASEALL: u32 = 0x4001_E50C;
        /// `CONFIG` = 1, writes enabled.
        pub const WEN: u32 = 1;
        /// `CONFIG` = 2, erase enabled.
        pub const EEN: u32 = 2;
        /// The part's page, and its whole main flash block.
        pub const PAGE: u32 = 1024;
        pub const FLASH: usize = 256 * 1024;
        /// The nRF52833's whole main flash block.
        pub const NRF52833_FLASH: usize = 512 * 1024;
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
        /// The IDCODE its debug port answers, where it is not an nRF51's: a probe on another board.
        answers: Option<u32>,
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
            Ok(self.answers.unwrap_or(0x0BB1_1477))
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
        let Err(reason) = flash(part, Part::Nrf51, None, &image, None, None) else {
            panic!("a write the NVMC dropped left the old firmware behind, and a deploy hid that");
        };
        assert!(
            reason.contains("0x00000578"),
            "the first address that differs is named: {reason}"
        );
    }

    /// **THE MICRO:BIT MIX-UP IS REFUSED AT THE DEBUG PORT, BEFORE THE ERASE.** An image built for
    /// the v1, and a probe on a board whose part answers the v2's IDCODE: the write stops there,
    /// and the same image to the part the board does carry is written as ever.
    #[test]
    fn a_write_to_a_part_the_named_board_does_not_carry_is_refused() {
        let board = board_called("bbc-micro-bit-v1").expect("the v1 is in the catalog");
        let image = image_of(4096);
        let not_the_board = Nrf51Part {
            answers: Some(0x2BA0_1477),
            ..Nrf51Part::holding_old_firmware()
        };
        let Err(reason) = flash(not_the_board, Part::Nrf51, Some(&board), &image, None, None) else {
            panic!("a part answering the v2's IDCODE was written as the v1's");
        };
        assert!(
            reason.contains("is not bbc-micro-bit-v1:") && reason.contains("Nothing was written"),
            "{reason}"
        );

        let part = flash(
            Nrf51Part::holding_old_firmware(),
            Part::Nrf51,
            Some(&board),
            &image,
            None,
            None,
        )
        .unwrap_or_else(|reason| panic!("the v1's own part is written: {reason}"));
        assert_eq!(&part.flash[..image.len()], &image[..]);
    }

    /// The control: the same part with nothing dropped takes the whole image and is left running it.
    #[test]
    fn an_nrf51_deploy_that_takes_puts_every_byte_on_the_part() {
        let image = image_of(4096);
        let part = flash(
            Nrf51Part::holding_old_firmware(),
            Part::Nrf51,
            None,
            &image,
            None,
            None,
        )
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
        let part = flash(part, Part::Nrf51, None, &image, None, None).unwrap_or_else(|reason| {
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

    /// What a micro:bit v2's part was asked to do, of the things a deploy and its hold can do to it,
    /// in the order asked.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum V2 {
        /// `ERASEALL`: the whole main flash block.
        EraseAll,
        /// `ERASEPAGE`: one page.
        ErasePage,
        /// A reset to run.
        ResetToRun,
        /// The reset vector catch armed.
        Arm,
        /// A soft reset, asked for through `AIRCR`.
        SoftReset,
        /// A look at whether the core is halted.
        Look,
        /// The PC read.
        ReadPc,
        /// The reset vector catch disarmed.
        Disarm,
    }

    /// A micro:bit v2's nRF52833, as the route `lamella flash` takes for it writes it and as a reset of
    /// its own holds it.
    ///
    /// Its flash controller is the nRF51's ([`nvmc`]): a cell takes a write only while `CONFIG` is
    /// `WEN`, a write clears bits without setting one, and `ERASEALL` erases the whole block while
    /// `CONFIG` is `EEN`. Its core is modeled as far as a reset reaches it: a soft reset through `AIRCR`
    /// restarts it at its vector table's reset vector and, with the catch armed, halts it there; a reset
    /// to run restarts it running.
    ///
    /// **THE RESET LINE AND THE HOLD EVERY OTHER PART TAKES ARE UNREACHABLE.** Neither is this part's,
    /// so a deploy that reached either would be holding it the way it is not held.
    struct Nrf52833Part {
        flash: Vec<u8>,
        config: u32,
        /// The word index, counted from the start of flash, from which a write is silently dropped.
        drops_writes_from: Option<usize>,
        /// The IDCODE its debug port answers, where it is not an nRF52's: a probe on another board.
        answers: Option<u32>,
        halted: bool,
        pc: u32,
        catch_armed: bool,
        /// Makes the catch miss: the soft reset restarts the core, and nothing halts it.
        catch_misses: bool,
        /// Where the soft reset leaves the core halted, where that is not its reset vector: a core
        /// stopped by something other than the catch.
        halts_at: Option<u32>,
        /// Makes the probe stop answering at the first look after a soft reset.
        goes_silent: bool,
        /// What the part was asked to do, shared so a test can still read it after a refused deploy
        /// has consumed the part.
        asked: Rc<RefCell<Vec<V2>>>,
    }

    impl Nrf52833Part {
        /// A part holding an older firmware, so an unerased cell is distinguishable from an erased one
        /// and from the image about to be written.
        fn holding_old_firmware() -> Self {
            Nrf52833Part {
                flash: (0..nvmc::NRF52833_FLASH).map(|index| (index % 97) as u8).collect(),
                config: 0,
                drops_writes_from: None,
                answers: None,
                halted: false,
                pc: 0,
                catch_armed: false,
                catch_misses: false,
                halts_at: None,
                goes_silent: false,
                asked: Rc::new(RefCell::new(Vec::new())),
            }
        }

        /// The offset in flash `address` addresses, for a whole word inside the block.
        fn cell(&self, address: u32) -> Option<usize> {
            let at = address as usize;
            (at + 4 <= self.flash.len()).then_some(at)
        }

        /// Where a reset starts the core: the vector table's reset vector, its Thumb bit cleared.
        fn reset_vector(&self) -> u32 {
            u32::from_le_bytes([self.flash[4], self.flash[5], self.flash[6], self.flash[7]]) & !1
        }

        fn ask(&self, what: V2) {
            self.asked.borrow_mut().push(what);
        }
    }

    impl TargetAccess for Nrf52833Part {
        fn connect(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn read_idcode(&mut self) -> Result<u32, ProbeError> {
            Ok(self.answers.unwrap_or(0x2BA0_1477))
        }
        fn init_mem(&mut self) -> Result<(), ProbeError> {
            Ok(())
        }
        fn halt(&mut self) -> Result<(), ProbeError> {
            self.halted = true;
            Ok(())
        }
        fn resume(&mut self) -> Result<(), ProbeError> {
            self.halted = false;
            Ok(())
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            self.ask(V2::ResetToRun);
            self.pc = self.reset_vector();
            self.halted = false;
            Ok(())
        }
        fn is_halted(&mut self) -> Result<bool, ProbeError> {
            self.ask(V2::Look);
            if self.goes_silent && self.asked.borrow().contains(&V2::SoftReset) {
                return Err(ProbeError::Transport("the probe went silent".to_owned()));
            }
            Ok(self.halted)
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
                nvmc::ERASEPAGE if self.config == nvmc::EEN => self.ask(V2::ErasePage),
                nvmc::ERASEALL if self.config == nvmc::EEN && value == 1 => {
                    self.ask(V2::EraseAll);
                    self.flash.fill(0xFF);
                }
                0xE000_ED0C if value == 0x05FA_0004 => {
                    self.ask(V2::SoftReset);
                    self.pc = self.halts_at.unwrap_or_else(|| self.reset_vector());
                    self.halted =
                        self.halts_at.is_some() || (self.catch_armed && !self.catch_misses);
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
        fn read_core_reg(&mut self, selector: u8) -> Result<u32, ProbeError> {
            assert_eq!(selector, 15, "the hold reads the PC and no other register");
            assert!(self.halted, "a core register is read from a halted core");
            self.ask(V2::ReadPc);
            Ok(self.pc)
        }
        fn arm_reset_catch(&mut self) -> Result<(), ProbeError> {
            self.ask(V2::Arm);
            self.catch_armed = true;
            self.halted = false;
            Ok(())
        }
        fn disarm_reset_catch(&mut self) -> Result<(), ProbeError> {
            self.ask(V2::Disarm);
            self.catch_armed = false;
            Ok(())
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            unreachable!(
                "the hold every other part takes, which pulses the reset line and then resets the part \
                 up to eight times more, is not an nRF52833's"
            )
        }
        fn set_reset(&mut self, _assert: bool) -> Result<u8, ProbeError> {
            unreachable!("an nRF52833 deploy never drives the reset line")
        }
        fn set_breakpoints(&mut self, addresses: &[u32]) -> Result<(), ProbeError> {
            assert!(addresses.is_empty(), "a deploy arms no breakpoint");
            Ok(())
        }

        // An access a v2 deploy has no reason to make panics, so this fixture states the dependency
        // surface rather than quietly absorbing a call nobody meant to write.
        fn read_byte(&mut self, _address: u32) -> Result<u8, ProbeError> {
            unreachable!("a v2 deploy programs words")
        }
        fn write_byte(&mut self, _address: u32, _value: u8) -> Result<(), ProbeError> {
            unreachable!("a v2 deploy programs words")
        }
        fn read_halfword(&mut self, _address: u32) -> Result<u16, ProbeError> {
            unreachable!("a v2 deploy programs words")
        }
        fn write_halfword(&mut self, _address: u32, _value: u16) -> Result<(), ProbeError> {
            unreachable!("a v2 deploy programs words")
        }
        fn write_core_reg(&mut self, _selector: u8, _value: u32) -> Result<(), ProbeError> {
            unreachable!("a deploy writes no core register")
        }
        fn step(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy does not step")
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            unreachable!("the hold looks at the core within its own bound")
        }
        fn set_breakpoint(&mut self, _address: u32) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no breakpoint")
        }
        fn clear_breakpoint(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy arms no breakpoint")
        }
        fn call_target(
            &mut self,
            _address: u32,
            _args: &[u32],
            _frame: &lamella_probe_core::CallFrame,
        ) -> Result<u32, ProbeError> {
            unreachable!("an nRF52833 is programmed through its NVMC, not by a loader on the part")
        }
    }

    /// An image for the v2 whose vector table starts its core at the handler at `0x100`.
    fn v2_image() -> Vec<u8> {
        let mut image = image_of(4096);
        image[..4].copy_from_slice(&0x2002_0000u32.to_le_bytes());
        image[4..8].copy_from_slice(&0x0000_0101u32.to_le_bytes());
        image
    }

    /// **THE MICRO:BIT v2 IS WRITTEN BY THE ROUTE `lamella flash` TAKES FOR IT AND HELD BY ONE RESET
    /// OF ITS OWN.** The route erases the whole flash, not the image's pages, and resets the part to
    /// run. The hold then resets it once under the reset vector catch, and the part is held where its
    /// vector table starts it, as its PC reads back.
    #[test]
    fn a_micro_bit_v2_deploy_goes_through_its_route_and_is_held_by_one_reset_of_its_own() {
        let board = board_called("bbc-micro-bit-v2").expect("the v2 is in the catalog");
        let image = v2_image();
        let part = Nrf52833Part::holding_old_firmware();
        let asked = Rc::clone(&part.asked);
        let part = flash(part, Part::Nrf52833, Some(&board), &image, None, None)
            .unwrap_or_else(|reason| panic!("an image this size fits an nRF52833: {reason}"));
        assert_eq!(
            &part.flash[..image.len()],
            &image[..],
            "every byte of the image is on the part"
        );
        assert!(
            part.flash[image.len()..].iter().all(|&cell| cell == 0xFF),
            "and the rest of the flash is erased: the route erases all of it, not the image's pages"
        );
        assert!(
            part.halted && part.pc == 0x100,
            "held at the image's entry, not at {:#x}",
            part.pc
        );
        assert!(!part.catch_armed, "with the catch disarmed, so a later reset boots freely");
        let asked: Vec<V2> = asked.borrow().iter().copied().filter(|step| *step != V2::Look).collect();
        assert_eq!(
            asked,
            [V2::EraseAll, V2::ResetToRun, V2::Arm, V2::SoftReset, V2::Disarm, V2::ReadPc],
            "the route's erase and its reset to run, then ONE reset under the catch"
        );
        assert!(
            crate::launch::take_said().is_empty(),
            "a part held at its entry has nothing to say"
        );
    }

    /// **ONE RESET, AT MOST, AND NEVER A SECOND.** A reset that does not stop the core leaves the part
    /// running its new image: the catch is disarmed, nothing resets the part again, and the session's
    /// Debug Console says which breakpoints the start costs.
    #[test]
    fn a_micro_bit_v2_whose_reset_does_not_stop_the_core_runs_after_that_one_reset() {
        let board = board_called("bbc-micro-bit-v2").expect("the v2 is in the catalog");
        let part = Nrf52833Part {
            catch_misses: true,
            ..Nrf52833Part::holding_old_firmware()
        };
        let asked = Rc::clone(&part.asked);
        let part = flash(part, Part::Nrf52833, Some(&board), &v2_image(), None, None)
            .unwrap_or_else(|reason| {
                panic!("a reset that does not stop the core is not a failed deploy: {reason}")
            });
        assert!(!part.halted, "running the new image");
        assert!(!part.catch_armed, "with the catch disarmed");
        let asked = asked.borrow();
        let resets: Vec<V2> = asked
            .iter()
            .copied()
            .filter(|step| matches!(step, V2::ResetToRun | V2::SoftReset))
            .collect();
        assert_eq!(
            resets,
            [V2::ResetToRun, V2::SoftReset],
            "the route's reset to run, the hold's one reset, and no other: {asked:?}"
        );
        assert_eq!(
            asked.last(),
            Some(&V2::Disarm),
            "nothing is asked of the part after the disarm: {asked:?}"
        );
        let said = crate::launch::take_said();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].starts_with("reset: this part could not be held at its entry after the deploy (")
                && said[0].ends_with(
                    "), so it is running the new image instead. A breakpoint on a line that runs before the \
                     session attaches will not be hit."
                ),
            "{said:?}"
        );
    }

    /// **A PROBE THAT STOPS ANSWERING DURING THE HOLD IS ASKED NOTHING MORE.** The look after the reset
    /// fails, and the session's start fails with it, naming the step: no disarm, no second reset and no
    /// reset to run come after it.
    #[test]
    fn a_probe_that_stops_answering_during_the_v2s_hold_is_asked_nothing_more() {
        let part = Nrf52833Part {
            goes_silent: true,
            ..Nrf52833Part::holding_old_firmware()
        };
        let asked = Rc::clone(&part.asked);
        let Err(reason) = flash(part, Part::Nrf52833, None, &v2_image(), None, None) else {
            panic!("a probe that went silent during the hold was taken for a held part");
        };
        assert!(reason.contains("the probe stopped answering"), "{reason}");
        let asked = asked.borrow();
        assert_eq!(
            asked.last(),
            Some(&V2::Look),
            "the failed look is the last thing asked: {asked:?}"
        );
        assert_eq!(
            asked.iter().filter(|step| **step == V2::SoftReset).count(),
            1,
            "{asked:?}"
        );
    }

    /// A core the reset leaves halted somewhere other than its image's entry is left where it stopped,
    /// and the session's Debug Console says where: held means the PC read back, not the halt alone.
    #[test]
    fn a_v2_core_halted_somewhere_other_than_its_entry_is_left_there_and_says_where() {
        let part = Nrf52833Part {
            halts_at: Some(0x0000_0400),
            ..Nrf52833Part::holding_old_firmware()
        };
        let part = flash(part, Part::Nrf52833, None, &v2_image(), None, None)
            .unwrap_or_else(|reason| panic!("held where it stopped: {reason}"));
        assert!(part.halted && part.pc == 0x400, "left where it stopped");
        let said = crate::launch::take_said();
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(
            said[0].contains("0x00000400") && said[0].contains("0x00000100"),
            "{said:?}"
        );
    }

    /// **THE MICRO:BIT MIX-UP THE OTHER WAY IS REFUSED AT THE DEBUG PORT, BEFORE THE ERASE.** An image
    /// for the v2 and a probe on a part answering the v1's IDCODE: nothing is erased, and nothing is
    /// reset.
    #[test]
    fn a_v2_image_to_a_part_answering_the_v1s_idcode_is_refused_before_any_erase() {
        let board = board_called("bbc-micro-bit-v2").expect("the v2 is in the catalog");
        let part = Nrf52833Part {
            answers: Some(0x0BB1_1477),
            ..Nrf52833Part::holding_old_firmware()
        };
        let asked = Rc::clone(&part.asked);
        let Err(reason) = flash(part, Part::Nrf52833, Some(&board), &v2_image(), None, None) else {
            panic!("a part answering the v1's IDCODE was written as the v2's");
        };
        assert!(
            reason.contains("is not bbc-micro-bit-v2:") && reason.contains("Nothing was written"),
            "{reason}"
        );
        assert!(
            asked.borrow().is_empty(),
            "nothing is erased or reset: {:?}",
            asked.borrow()
        );
    }

    /// The route reads a v2's image back, so a write the controller did not take is refused naming
    /// where, and the part is neither reset to run nor held.
    #[test]
    fn a_micro_bit_v2_deploy_that_does_not_read_back_as_written_is_refused_naming_where() {
        let part = Nrf52833Part {
            drops_writes_from: Some(350),
            ..Nrf52833Part::holding_old_firmware()
        };
        let asked = Rc::clone(&part.asked);
        let Err(reason) = flash(part, Part::Nrf52833, None, &v2_image(), None, None) else {
            panic!("a write the controller dropped was reported as a deploy");
        };
        assert!(
            reason.contains("0x00000578"),
            "the first address that differs is named: {reason}"
        );
        assert_eq!(*asked.borrow(), [V2::EraseAll], "nothing is reset");
    }

    /// One kilobyte, as the manuals size sectors and flash.
    #[cfg(feature = "st")]
    const KB: usize = 1024;

    /// The Debug Halting Control and Status Register, which a route's write reads and writes as it
    /// hands the part back, and a hold through the part's own reset writes.
    const DHCSR: u32 = 0xE000_EDF0;

    /// Halting debug as a fake part's DHCSR reports it: whether halting debug is on, and whether
    /// the core is halted. `FlashPart` and `H7Part` carry one, because an F7 or H7 deploy writes
    /// through the route `lamella flash` takes, and the route's write ends by handing the part back
    /// through `lamella_probe_core::hand_back`, which reads the halt state and writes DHCSR.
    #[cfg(feature = "st")]
    #[derive(Default)]
    struct Dhcsr {
        /// `C_DEBUGEN`: halting debug is on.
        debugen: bool,
        /// `S_HALT`: the core is halted.
        halted: bool,
    }

    #[cfg(feature = "st")]
    impl Dhcsr {
        /// A write to DHCSR. One without the key in its top half is ignored, as the architecture
        /// ignores it; with halting debug on, `C_HALT` halts the core and its absence lets it run.
        fn write(&mut self, value: u32) {
            if value >> 16 != 0xA05F {
                return;
            }
            self.debugen = value & 1 != 0;
            if self.debugen {
                self.halted = value & 2 != 0;
            }
        }

        /// `TargetAccess::halt`, and the hold ending a deploy: halting debug on, the core halted.
        fn halt(&mut self) {
            self.debugen = true;
            self.halted = true;
        }

        /// `TargetAccess::reset_and_run`: a system reset, then a resume leaving halting debug on.
        fn reset_and_run(&mut self) {
            self.debugen = true;
            self.halted = false;
        }
    }

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
        /// Halting debug, which the route's hand-back reads and turns off.
        dhcsr: Dhcsr,
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
                dhcsr: Dhcsr::default(),
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
            if address == DHCSR {
                self.dhcsr.write(value);
                return Ok(());
            }
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
            self.dhcsr.halt();
            Ok(())
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            self.dhcsr.reset_and_run();
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
            Ok(self.dhcsr.halted)
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy does not wait for a halt")
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            self.dhcsr.halt();
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
        fn set_breakpoints(&mut self, addresses: &[u32]) -> Result<(), ProbeError> {
            assert!(addresses.is_empty(), "a deploy arms no breakpoint: {addresses:?}");
            Ok(())
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
        let part = flash(FlashPart::f769(), Part::Stm32F7, None, &image, None, None)
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
        let Err(reason) = flash(part, Part::Stm32F7, None, &image, None, None) else {
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
        let Err(reason) = flash(part, Part::Stm32F4, None, &image, None, None) else {
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
        let Err(reason) = flash(part, Part::Stm32F4, None, &image, None, None) else {
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
        /// Halting debug, which the route's hand-back reads and turns off.
        dhcsr: Dhcsr,
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
                dhcsr: Dhcsr::default(),
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
            if address == DHCSR {
                self.dhcsr.write(value);
                return Ok(());
            }
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
            self.dhcsr.halt();
            Ok(())
        }
        fn reset_and_run(&mut self) -> Result<(), ProbeError> {
            self.dhcsr.reset_and_run();
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
            Ok(self.dhcsr.halted)
        }
        fn wait_halted(&mut self) -> Result<(), ProbeError> {
            unreachable!("a deploy does not wait for a halt")
        }
        fn reset_and_halt(&mut self) -> Result<(), ProbeError> {
            self.dhcsr.halt();
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
        fn set_breakpoints(&mut self, addresses: &[u32]) -> Result<(), ProbeError> {
            assert!(addresses.is_empty(), "a deploy arms no breakpoint: {addresses:?}");
            Ok(())
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
        let part = flash(H7Part::h747(), Part::Stm32H7, None, &image, None, None)
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
        let Err(reason) = flash(part, Part::Stm32H7, None, &image, None, None) else {
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
        let Err(reason) = flash(part, Part::Stm32H7, None, &image, None, None) else {
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
