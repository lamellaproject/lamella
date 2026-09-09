//! The on-device DAP server: a host binary bridging a Debug Adapter Protocol client (VS
//! Code) to a Cortex-M target over the Lamella CMSIS-DAP stack. It links `lamella-dap`
//! WITHOUT the interpreter (`default-features = false`) -- the adapter, the wire protocol,
//! and the `DebugBackend` trait -- driven here by a `DeviceBackend` over a real probe.

use lamella_aot::build;
use lamella_probe_core::{ArmDap, TargetAccess};
use lamella_cmsis_dap_nrf::Nrf51Flash;
use lamella_debug_device::DeviceBackend;
use lamella_metadata::{Assembly, PortablePdb};

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
}

const USAGE: &str = "usage: device-dap-server [--probe cmsis|stlink] \
                     [--part nrf51|samd21|f0|f4|f7|h7] [--pid 0xNNNN] [--attach [--reset]] \
                     <program.dll|program.elf> [<Type> <Method>] [probe-serial]";

fn main() -> std::io::Result<()> {
    let mut probe = ProbeKind::CmsisDap;
    let mut part = Part::Nrf51;
    let mut part_named = false;
    let mut attach = false;
    let mut reset = false;
    #[cfg(feature = "st")]
    let mut stlink_pid = lamella_stlink::product_id::V2_1;
    let mut positional: Vec<String> = Vec::new();
    let mut raw = std::env::args().skip(1);
    while let Some(arg) = raw.next() {
        match arg.as_str() {
            "--probe" => {
                probe = match raw.next().as_deref() {
                    Some("cmsis") => ProbeKind::CmsisDap,
                    #[cfg(feature = "st")]
                    Some("stlink") => ProbeKind::StLink,
                    #[cfg(not(feature = "st"))]
                    Some("stlink") => panic!("--probe stlink needs this crate built with --features st"),
                    other => panic!("--probe takes cmsis or stlink, not {other:?}"),
                };
            }
            "--attach" => attach = true,
            "--reset" => reset = true,
            "--part" => {
                part_named = true;
                part = match raw.next().as_deref() {
                    Some("nrf51") => Part::Nrf51,
                    #[cfg(feature = "sam")]
                    Some("samd21") => Part::Samd21,
                    #[cfg(feature = "st")]
                    Some("f0") => Part::Stm32F0,
                    #[cfg(feature = "st")]
                    Some("f4") => Part::Stm32F4,
                    #[cfg(feature = "st")]
                    Some("f7") => Part::Stm32F7,
                    #[cfg(feature = "st")]
                    Some("h7") => Part::Stm32H7,
                    other => panic!("--part does not accept {other:?} in this build"),
                };
            }
            "--pid" => {
                let value = raw.next();
                #[cfg(feature = "st")]
                {
                    stlink_pid = value
                        .as_deref()
                        .and_then(|t| u16::from_str_radix(t.trim_start_matches("0x"), 16).ok())
                        .unwrap_or(stlink_pid);
                }
                #[cfg(not(feature = "st"))]
                let _ = value;
            }
            _ => positional.push(arg),
        }
    }
    let mut args = positional.into_iter();
    let program = args.next().expect(USAGE);
    if reset && !part_named {
        eprintln!(
            "reset: no --part was named, so the generic sequence is what will run. A part with a \
             sequence of its own needs naming to get it."
        );
    }

    let rest: Vec<String> = args.collect();
    let (target, serial): (Option<(String, String)>, Option<String>) = match rest.len() {
        0 => (None, None),
        1 => (None, Some(rest[0].clone())),
        2 => (Some((rest[0].clone(), rest[1].clone())), None),
        3 => (Some((rest[0].clone(), rest[1].clone())), Some(rest[2].clone())),
        _ => panic!("{USAGE}"),
    };

    let bytes = std::fs::read(&program).expect("read the program");
    let debug = if bytes.starts_with(b"\x7fELF") {
        let elf = lamella_debug_device::program::from_elf(&bytes)
            .unwrap_or_else(|error| panic!("{program}: {error}"));
        assert!(
            attach || elf.image_base == part.flash_base(),
            "{program} is linked to load at {:#010x} and --part puts this part's flash at {:#010x} \
             -- either the wrong --part or an image built for another board",
            elf.image_base,
            part.flash_base()
        );
        assert!(
            attach || part_named,
            "{program} is an ELF and no --part was given, so the default would choose a flash \
             algorithm rather than the right one -- and a matching base does not distinguish two \
             parts that both boot from zero.

             Either name the part with --part, or deploy it with `lamella flash` and debug the \
             running program with --attach."
        );
        elf
    } else {
        let (lines, names, image, file, entry) = source_lines(&program, target.as_ref());
        let ends: Vec<u32> = names
            .iter()
            .enumerate()
            .map(|(index, _)| {
                names.get(index + 1).map_or(u32::MAX, |&(start, _)| start)
            })
            .collect();
        let names: Vec<(u32, u32, String)> = names
            .into_iter()
            .zip(ends)
            .map(|((start, name), end)| (start, end, name))
            .collect();
        lamella_debug_device::program::DebugProgram {
            image,
            image_base: part.flash_base(),
            lines: lines
                .into_iter()
                .map(|(offset, line)| lamella_debug_device::LineRow { offset, line, file: 0 })
                .collect(),
            names,
            files: vec![file],
            entry,
            frames: Vec::new(),
            locals: lamella_debug_device::program::LocalSections::default(),
        }
    };
    let lamella_debug_device::program::DebugProgram {
        image, image_base, lines, names, files, entry, frames, locals,
    } = debug;

    match probe {
        #[cfg(feature = "st")]
        ProbeKind::StLink => {
            let stlink = lamella_stlink::StLink::open(
                stlink_pid,
                serial.as_deref(),
            )
            .expect("open the ST-Link");
            serve(stlink, part, image_base, attach, reset, &image, lines, names, files, entry, frames, locals)
        }
        ProbeKind::CmsisDap => {
            let selector = serial
                .as_deref()
                .map_or_else(lamella_probe::Selector::from_environment, lamella_probe::Selector::by_serial);
            let session = lamella_probe::open(&selector).expect("open a CMSIS-DAP probe");
            eprintln!(
                "probe: {:?} {:04x}:{:04x} serial {:?}",
                session.info.product,
                session.info.vendor_id,
                session.info.product_id,
                session.info.serial
            );
            serve(ArmDap::new(session.into_dap()), part, image_base, attach, reset, &image, lines, names, files, entry, frames, locals)
        }
    }
}

/// Flashes the image, then serves DAP over stdio against the freshly flashed program.
///
/// Generic over the probe, so an ST-Link and a CMSIS-DAP probe run the identical code path.
fn serve<A: TargetAccess + 'static>(
    mut probe: A,
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
) -> std::io::Result<()> {
    if attach {
        probe.connect().expect("connect SWD");
        probe.init_mem().expect("init MEM-AP");
        if reset {
            #[cfg(feature = "sam")]
            let outcome = if part == Part::Samd21 {
                use lamella_cmsis_dap_sam::Samd21Debug;
                probe.samd21_park()
            } else {
                probe.reset_and_halt()
            };
            #[cfg(not(feature = "sam"))]
            let outcome = probe.reset_and_halt();

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
    } else {
        flash(&mut probe, part, image);
    }
    let backend = DeviceBackend::new(probe, lines, image_base, names, files, entry, frames)
        .with_locals(locals);

    let mut debugger = lamella_dap::Debugger::with_backend(Box::new(backend));
    lamella_dap::serve_polled(
        &mut debugger,
        std::io::BufReader::new(std::io::stdin()),
        &mut std::io::stdout().lock(),
    )
}

/// Flashes a raw image to the selected part and resets the target to run it.
///
/// Every algorithm reached here is a blanket impl over [`TargetAccess`], so this is genuinely one
/// function over two probe families and five parts -- the seam doing its job rather than a
/// coincidence. Only the erase geometry and the base address are per-part.
fn flash<A: TargetAccess>(target: &mut A, part: Part, image: &[u8]) {
    let words: Vec<u32> = image
        .chunks(4)
        .map(|c| {
            let mut w = [0u8; 4];
            w[..c.len()].copy_from_slice(c);
            u32::from_le_bytes(w)
        })
        .collect();
    target.connect().expect("connect SWD");
    target.read_idcode().expect("read IDCODE");
    target.init_mem().expect("init MEM-AP");
    target.halt().expect("halt");
    let base = part.flash_base();

    match part {
        #[cfg(feature = "sam")]
        Part::Samd21 => panic!(
            "--part samd21 selects this part's RESET sequence and there is no SAM flash algorithm \
             in this binary. Deploy the image with `lamella flash` and debug the running program \
             with --attach."
        ),
        Part::Nrf51 => {
            let pages = (words.len() * 4).div_ceil(0x400);
            for page in 0..pages as u32 {
                target.erase_flash_page(page * 0x400).expect("erase page");
            }
            target.write_flash(base, &words).expect("write flash");
        }
        #[cfg(feature = "st")]
        Part::Stm32F0 => {
            use lamella_cmsis_dap_stm32::{STM32F0_PAGE, Stm32F0Flash};
            target.f0_unlock_flash().expect("unlock flash");
            for page in 0..(image.len() as u32).div_ceil(STM32F0_PAGE) {
                target.f0_erase_page(base + page * STM32F0_PAGE).expect("erase page");
            }
            target.f0_program(base, image).expect("program");
            target.f0_lock_flash().expect("lock flash");
        }
        #[cfg(feature = "st")]
        Part::Stm32F4 | Part::Stm32F7 => {
            use lamella_cmsis_dap_stm32::{
                STM32F4_SECTOR_SIZES, STM32F7_SECTOR_SIZES, Stm32F4Flash, sectors_covering,
            };
            let sizes: &[usize] = if part == Part::Stm32F7 {
                &STM32F7_SECTOR_SIZES
            } else {
                &STM32F4_SECTOR_SIZES
            };
            target.unlock_flash().expect("unlock flash");
            for sector in 0..sectors_covering(image.len(), sizes) {
                target.erase_sector(sector).expect("erase sector");
            }
            target.program_words(base, &words).expect("program");
            target.lock_flash().expect("lock flash");
        }
        #[cfg(feature = "st")]
        Part::Stm32H7 => {
            use lamella_cmsis_dap_stm32::{STM32H7_SECTOR, Stm32H7Flash};
            target.h7_unlock_flash(base).expect("unlock flash");
            for sector in 0..(image.len() as u32).div_ceil(STM32H7_SECTOR) {
                target.h7_erase_sector(base + sector * STM32H7_SECTOR).expect("erase sector");
            }
            target.h7_program(base, image).expect("program");
            target.h7_lock_flash(base).expect("lock flash");
        }
    }
    target.reset_and_run().expect("reset and run");
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
