//! Writing bytes to a chip: the `flash` verb, and the mechanism `deploy --board` writes through.

use crate::args::{self, Spec};
use crate::catalogue;
use std::path::Path;
use std::process::ExitCode;

/// How a board takes an image, and what the ahead-of-time backend calls its chip.
///
/// **HOW A BOARD IS PROGRAMMED IS NOT WHAT ITS `[[carriers]]` DECLARES, AND THE TWO READ ALIKE.**
/// A carrier records the path the Lamella Link console takes through a RUNNING board; this table
/// records how firmware reaches a BLANK one. A bridge usually offers both, which is why one is so
/// easily mistaken for the other.
///
/// The table lives here in ONE place with a census over it, so a board missing from it fails
/// loudly rather than quietly.
struct Programming {
    /// The board id, as `lamella boards` lists it.
    board: &'static str,
    /// The chip name `lamella_aot::build::build_cortex_m` knows this board by.
    aot_target: &'static str,
    /// How the image reaches the board.
    programmer: Programmer,
}

/// The ways an image reaches a board. One variant per mechanism, not per board.
#[derive(Clone, Copy)]
enum Programmer {
    /// The micro:bit v1's on-board DAPLink probe, over SWD.
    MicrobitV1Daplink,
    /// The micro:bit v2's on-board DAPLink probe, over SWD. A separate variant from the v1's
    /// because the part differs: the write path checks the debug port's IDCODE BEFORE it erases,
    /// so pointing a v2 image at a v1 board stops at a message rather than erasing the board and
    /// then writing an image its core cannot run.
    MicrobitV2Daplink,
    /// A UF2 bootloader volume: the image is COPIED to a drive the halted chip presents. Needs no
    /// probe at all, which is why it is the shortest path from nothing to a running board -- and
    /// why it is the one a person with a brand-new board can follow.
    Uf2Volume {
        /// The chip family the bootloader checks the image against, so one built for another part
        /// is refused instead of run.
        family: u32,
        /// Where the image belongs in the chip's address space.
        base: u32,
    },
}

impl Programmer {
    /// What this mechanism is, for a person reading the output.
    fn description(self) -> &'static str {
        match self {
            Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => {
                "the board's on-board DAPLink probe, over SWD"
            }
            Programmer::Uf2Volume { .. } => "the board's bootloader volume, by copying the image",
        }
    }

    /// The address this mechanism writes an image from.
    ///
    /// **ONE PLACE STATES IT, so the address a `build --format` file declares is the address a
    /// write actually uses.** A file that said one thing while the writer did another would be
    /// wrong in the way nothing catches: it would flash correctly here and be rejected, or
    /// misplaced, by somebody else's programmer.
    fn flash_base(self) -> u32 {
        match self {
            Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => 0,
            Programmer::Uf2Volume { base, .. } => base,
        }
    }

    /// The format an image must be written in to reach this mechanism, where the mechanism decides
    /// it. A probe takes raw bytes; a bootloader volume takes a file, and which file matters.
    fn required_format(self) -> Option<crate::artifact::Format> {
        match self {
            Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => None,
            Programmer::Uf2Volume { family, .. } => {
                Some(crate::artifact::Format::Uf2 { family })
            }
        }
    }

    /// The USB vendor and product this mechanism's probes present, taken from the crate that owns
    /// the fact rather than restated here.
    fn usb_identity(self) -> Option<(u16, u16)> {
        match self {
            Programmer::MicrobitV1Daplink | Programmer::MicrobitV2Daplink => {
                Some(lamella_cmsis_dap_nrf::MICROBIT_DAPLINK)
            }
            Programmer::Uf2Volume { .. } => None,
        }
    }
}

/// The RP2350 in its Arm secure profile, as its bootloader checks it. `bin2uf2` states the same
/// number and this is the second place it appears; when a third wants it, it wants a home in
/// `lamella-wire` beside the other wire-visible identifiers rather than a third copy.
const RP2350_UF2_FAMILY: u32 = 0xe48b_ff59;

/// The RP2040's family id, for the same bootloader on the older part.
const RP2040_UF2_FAMILY: u32 = 0xe48b_ff56;

/// Where an RP2350 or RP2040 image belongs: the base of execute-in-place flash.
const RP2_XIP_BASE: u32 = 0x1000_0000;

/// Settle WHICH board to write, adding an interactive rung STRICTLY BELOW the refusal.
///
/// **THE ORDER MATTERS MORE THAN THE PROMPT DOES.** The existing ladder is explicit serial, then
/// `LAMELLA_PROBE_SERIAL`, then the sole attached board of that family, then a refusal naming every
/// candidate -- and no rung guesses. This asks the user only where that ladder was going to REFUSE,
/// so a named board is still written without a question, and an ambiguous bench is resolved by a
/// person rather than by enumeration order.
///
/// **IT ASKS ONLY A TERMINAL.** With no human on the other end -- a script, a build, an agent
/// driving the tool -- there is nobody to answer, and a prompt that times out or reads end-of-file
/// would have to fall back to something. Falling back means guessing, and the thing being guessed
/// at is which board gets erased. So without a terminal it refuses exactly as before.
fn choose_board(
    programmer: Programmer,
    requested: Option<&str>,
) -> Result<Option<String>, String> {
    if requested.is_some() {
        return Ok(requested.map(str::to_owned));
    }
    let Some((vid, pid)) = programmer.usb_identity() else {
        return Ok(None);
    };
    match lamella_probe::resolve_serial(vid, pid, None) {
        Ok(_) => Ok(None),
        Err(lamella_probe::ProbeError::Ambiguous(candidates)) => ask(&candidates).map(Some),
        Err(_) => Ok(None),
    }
}

/// Ask which of `candidates` to write.
///
/// # Errors
/// When there is no terminal to ask, or the answer is not one of the candidates. Both refuse
/// rather than defaulting, because the default would be a board somebody else is using.
fn ask(candidates: &[String]) -> Result<String, String> {
    use std::io::{IsTerminal, Write};

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        return Err(format!(
            "more than one board of this family is attached and none was named:\n{}\n\n\
             Name one with --probe <serial>, or set LAMELLA_PROBE_SERIAL. This is not being \
             guessed at:\na write to the wrong board succeeds and reports nothing.",
            list_of(candidates)
        ));
    }
    println!("\nmore than one board of this family is attached:");
    println!("{}", list_of(candidates));
    println!("\nwhich one should be written? (a number, or the serial; anything else cancels)");
    print!("> ");
    let _ = std::io::stdout().flush();

    let mut answer = String::new();
    if std::io::stdin().read_line(&mut answer).is_err() {
        return Err("could not read an answer; name the board with --probe <serial>".to_owned());
    }
    let answer = answer.trim();
    if let Ok(index) = answer.parse::<usize>()
        && index >= 1
        && index <= candidates.len()
    {
        return Ok(candidates[index - 1].clone());
    }
    if let Some(chosen) = candidates.iter().find(|candidate| *candidate == answer) {
        return Ok(chosen.clone());
    }
    Err(format!(
        "{answer:?} is not one of the boards listed, so nothing was written. \
         Name one with --probe <serial>."
    ))
}

/// The candidates as a numbered list.
fn list_of(candidates: &[String]) -> String {
    candidates
        .iter()
        .enumerate()
        .map(|(index, serial)| format!("  {}. {serial}", index + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every board this build can write, and how.
const PROGRAMMING: &[Programming] = &[
    Programming {
        board: "micro-bit-v1",
        aot_target: "microbit",
        programmer: Programmer::MicrobitV1Daplink,
    },
    Programming {
        board: "micro-bit-v2",
        aot_target: "nrf52833",
        programmer: Programmer::MicrobitV2Daplink,
    },
    Programming {
        board: "rpi-pico2",
        aot_target: "rp2350",
        programmer: Programmer::Uf2Volume { family: RP2350_UF2_FAMILY, base: RP2_XIP_BASE },
    },
    Programming {
        board: "rpi-pico2-w",
        aot_target: "rp2350",
        programmer: Programmer::Uf2Volume { family: RP2350_UF2_FAMILY, base: RP2_XIP_BASE },
    },
    Programming {
        board: "rpi-pico",
        aot_target: "rp2040",
        programmer: Programmer::Uf2Volume { family: RP2040_UF2_FAMILY, base: RP2_XIP_BASE },
    },
    Programming {
        board: "rpi-pico-w",
        aot_target: "rp2040",
        programmer: Programmer::Uf2Volume { family: RP2040_UF2_FAMILY, base: RP2_XIP_BASE },
    },
];

/// Whether `lamella flash` can write `board`, for the `boards` listing's coverage column.
#[must_use]
pub fn can_flash(board: &str) -> bool {
    PROGRAMMING.iter().any(|row| row.board == board)
}

/// `lamella flash <artifact> --board <id>`: write bytes that are already an image.
///
/// **IT TAKES AN IMAGE, NEVER SOURCE.** Compiling a program and putting it on a board is `deploy`;
/// this writes bytes somebody else's toolchain may have produced. Keeping the two apart is the
/// whole reason the verb exists separately -- a tool with two words for one job teaches nobody
/// anything, and "flash a `.cs` file" is not a sentence about the hardware.
pub fn flash_command(args: &[String]) -> ExitCode {
    let spec = Spec { verb: "flash", values: &["--board", "--probe"], flags: &[] };
    let parsed = match args::parse(args, &spec) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let path = match parsed.only_positional("flash", "prebuilt image") {
        Ok(path) => Path::new(path).to_path_buf(),
        Err(error) => {
            eprintln!("{error}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let Some(board_id) = parsed.value("--board") else {
        eprintln!("lamella flash: --board is required -- it says which chip is written.\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    let row = match programmer_for(board_id) {
        Ok(row) => row,
        Err(error) => {
            eprint!("{error}");
            return ExitCode::FAILURE;
        }
    };

    match crate::artifact::classify(&path) {
        crate::artifact::Kind::ChipImage => {}
        crate::artifact::Kind::Source => {
            eprintln!(
                "lamella flash: {} is source, and this verb writes bytes that are already an \
                 image.\n\n\
                 To compile it and put it on the board:\n\
                 \x20   lamella deploy {} --board {board_id}\n\n\
                 `flash` is for an image somebody has already built -- a published firmware, a \
                 release\nartifact, or the output of `lamella build --format`.",
                path.display(),
                path.display()
            );
            return ExitCode::FAILURE;
        }
        crate::artifact::Kind::WirePayload => {
            eprintln!(
                "lamella flash: {} is loaded BY firmware rather than written to a chip -- it needs \
                 a board that\nis already running Lamella, not a probe.\n\n\
                 \x20   lamella deploy {} --target <t>\n\n\
                 `lamella devices` prints the target for each attached board.",
                path.display(),
                path.display()
            );
            return ExitCode::FAILURE;
        }
    }
    let (bytes, described) = match row.programmer.required_format() {
        Some(required) => {
            let named = crate::artifact::classify_format(&path);
            let raw = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    eprintln!("lamella flash: read {}: {error}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            match named {
                Some(extension) if extension == required.extension() => {
                    let count = raw.len();
                    (raw, format!("{count} B of {}", required.description()))
                }
                Some("bin") => {
                    let wrapped = required.render(&raw, row.programmer.flash_base());
                    let described = format!(
                        "{} B of raw binary, wrapped as {} at {:#010x}",
                        raw.len(),
                        required.description(),
                        row.programmer.flash_base()
                    );
                    (wrapped, described)
                }
                _ => {
                    eprintln!(
                        "lamella flash: {board_id} takes an image COPIED to its bootloader volume, \
                         as a {} (or a\n.bin, which is wrapped into one). {} is neither. \
                         `lamella build <file> --board {board_id} --format {}`\nproduces it.",
                        required.extension(),
                        path.display(),
                        required.extension()
                    );
                    return ExitCode::FAILURE;
                }
            }
        }
        None => {
            let artifact = match crate::artifact::read(&path) {
                Ok(artifact) => artifact,
                Err(error) => {
                    eprintln!("lamella flash: {error}");
                    return ExitCode::FAILURE;
                }
            };
            if let Err(error) = check_base(&artifact, row.programmer) {
                eprintln!("lamella flash: {error}");
                return ExitCode::FAILURE;
            }
            let described = format!("{} B of {}", artifact.bytes.len(), artifact.format);
            (artifact.bytes, described)
        }
    };
    println!("read {described} from {}", path.display());
    write_image(row, &bytes, parsed.value("--probe"))
}

/// The mechanism for `board_id`, or the message explaining why there is none.
///
/// # Errors
/// An unknown board id, or one no mechanism covers. The two read differently on purpose.
fn programmer_for(board_id: &str) -> Result<&'static Programming, String> {
    catalogue::resolve(board_id).map_err(|error| format!("lamella flash: {error}\n"))?;
    PROGRAMMING
        .iter()
        .find(|row| row.board == board_id)
        .ok_or_else(|| cannot_write(board_id))
}

/// Write `image` to the board `row` describes, settling which physical board first.
///
/// Shared by `flash` and by `deploy --board`, so the probe ladder, the interactive rung and the
/// reporting are identical whether the bytes were compiled a moment ago or read off disk. The
/// board cannot tell the difference and neither should the output.
fn write_image(row: &Programming, image: &[u8], probe: Option<&str>) -> ExitCode {
    let probe = match choose_board(row.programmer, probe) {
        Ok(chosen) => chosen,
        Err(error) => {
            eprintln!("lamella: {error}");
            return ExitCode::FAILURE;
        }
    };
    println!("writing over {}...", row.programmer.description());
    match write(row.programmer, image, probe.as_deref()) {
        Ok(report) => {
            println!(
                "wrote and verified {} B ({} words); the board is running it.",
                report.bytes, report.words
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("lamella: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Compile `path` and write the result to `board_id`'s chip -- the route `deploy --board` takes.
///
/// **THE MECHANISM LIVES HERE AND THE VERB LIVES IN `deploy`**, because this file is about how
/// bytes reach a chip and that one is about taking a program to a board. Splitting them that way
/// is what lets `deploy` choose between this route and the wire without either route knowing the
/// other exists.
pub fn deploy_to_chip(
    path: &Path,
    board_id: &str,
    probe: Option<&str>,
    unsafe_code: bool,
) -> ExitCode {
    let row = match programmer_for(board_id) {
        Ok(row) => row,
        Err(error) => {
            eprint!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let source = match std::fs::read_to_string(path) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("lamella deploy: read {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let image = match build_image(path, &source, row.aot_target, unsafe_code) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "built {} B for {board_id} ({}), ahead of time -- no firmware needed on the board",
        image.len(),
        row.aot_target
    );
    write_image(row, &image, probe)
}

/// The bare-metal image for `board_id`, and the address it belongs at -- what `build --format`
/// writes to a file and `deploy --board` writes to a chip.
///
/// **ONE FUNCTION, SO THE FILE AND THE CHIP GET THE SAME BYTES.** A `build` that produced anything
/// other than what `deploy` writes would make the two verbs disagree exactly where somebody is
/// least able to check -- an image handed to a colleague, or archived for a release.
///
/// # Errors
/// An unwritable board, or any compile or lowering failure, already worded for a reader.
pub fn image_for_board(
    path: &Path,
    source: &str,
    board_id: &str,
    unsafe_code: bool,
) -> Result<(Vec<u8>, u32), String> {
    let row = programmer_for(board_id)?;
    let image = build_image(path, source, row.aot_target, unsafe_code)?;
    Ok((image, row.programmer.flash_base()))
}

/// The chip family a UF2 for `board_id` must declare, when its mechanism uses one.
///
/// **THE FAMILY BELONGS TO THE CHIP AND SO IT COMES FROM THE BOARD.** A `--format uf2` on the
/// command line cannot supply it, and a UF2 carrying the wrong one is refused by the bootloader --
/// which is the behavior worth preserving, so it is filled in from here rather than defaulted.
#[must_use]
pub fn uf2_family_for_board(board_id: &str) -> Option<u32> {
    let row = PROGRAMMING.iter().find(|row| row.board == board_id)?;
    match row.programmer {
        Programmer::Uf2Volume { family, .. } => Some(family),
        _ => None,
    }
}

/// Compile `source` and lower it ahead of time to a bare-metal image for `aot_target`.
fn build_image(
    path: &Path,
    source: &str,
    aot_target: &str,
    unsafe_code: bool,
) -> Result<Vec<u8>, String> {
    let assembly = crate::program::compile_csharp_assembly(path, source, unsafe_code)?;
    if !has_static_main(&assembly) {
        return Err(format!(
            "lamella deploy: {} declares no static Main.\n\n\
             A flashed image IS the program: the chip resets straight into it, so it needs one \
             entry point.\nAdd `static void Main()` (or `static int Main()`) to a class in this \
             file. A sample written as a\nlibrary -- a `Run()` that a harness calls -- has to gain \
             a Main before it can be deployed on its own.",
            path.display()
        ));
    }
    lamella_aot::build::build_cortex_m(&assembly, aot_target).map_err(|error| {
        format!(
            "lamella deploy: the ahead-of-time build failed: {error:?}\n\n\
             This is the flat, linker-free path, and its limits are worth knowing before you read \
             that as a bug:\nit resolves no calls outside the program, so floating point, \
             allocation, and anything reaching the\nclass library are unavailable. A program that \
             writes device registers and loops is the shape it covers."
        )
    })
}

/// Check that a prebuilt image belongs where this mechanism writes.
///
/// **A FILE THAT STATES AN ADDRESS IS BELIEVED ABOUT ITS OWN ADDRESS, NOT ABOUT OURS.** Intel HEX
/// carries a base, and every mechanism here writes from a fixed one; a file built for a different
/// part -- an STM32 image at `0x0800_0000`, say -- is well-formed, parses cleanly, and would be
/// written to the wrong place on a Nordic part where flash begins at zero. That is a silent bad
/// flash, so the disagreement is a refusal rather than a warning.
///
/// # Errors
/// When the artifact states a base this mechanism does not write to.
fn check_base(artifact: &crate::artifact::Artifact, programmer: Programmer) -> Result<(), String> {
    let expected = programmer.flash_base();
    if artifact.base == expected {
        return Ok(());
    }
    Err(format!(
        "this image states it belongs at {:#010x}, and this board is written from {expected:#010x}.\n\
         It was almost certainly built for a different part -- writing it here would put the right \
         bytes\nin the wrong place, which a board reports as nothing at all.",
        artifact.base
    ))
}

/// Whether `assembly` declares a static method named `Main`.
///
/// A guard on the ENTRY CONTRACT rather than a second copy of the backend's entry search: what is
/// being asked here is whether the file a person named can be a program at all, and the answer is
/// worth having before an image exists.
fn has_static_main(assembly: &[u8]) -> bool {
    let Ok(parsed) = lamella_metadata::Assembly::read(assembly) else {
        return false;
    };
    parsed.type_defs().any(|type_def| {
        type_def.methods().any(|method| method.is_static() && method.name() == Some("Main"))
    })
}

/// What a write reported.
struct Written {
    bytes: usize,
    words: usize,
}

/// Write `image` to the board through `programmer`.
fn write(
    programmer: Programmer,
    image: &[u8],
    probe: Option<&str>,
) -> Result<Written, String> {
    let report = match programmer {
        Programmer::MicrobitV1Daplink => lamella_cmsis_dap_nrf::flash_microbit(image, probe),
        Programmer::MicrobitV2Daplink => lamella_cmsis_dap_nrf::flash_microbit_v2(image, probe),
        Programmer::Uf2Volume { .. } => return copy_to_volume(image, probe),
    };
    report
        .map(|report| Written { bytes: report.bytes, words: report.words })
        .map_err(describe)
}

/// Copy `image` to a bootloader volume, and settle WHICH volume first.
///
/// **THE VOLUME CANNOT BE CHOSEN BY LABEL AND THIS IS NOT A DETAIL.** Two RP2350s in BOOTSEL mount
/// as two drives both labelled `RP2350`, with byte-identical `INFO_UF2.TXT` files -- measured. So
/// nothing readable from a volume distinguishes them, and a copy aimed at "the RP2350 drive" is a
/// coin flip whose wrong outcome is somebody else's board taking your program. With more than one
/// mounted, this REFUSES and names them, exactly as the probe ladder does.
fn copy_to_volume(image: &[u8], requested: Option<&str>) -> Result<Written, String> {
    let mounted: Vec<crate::bootsel::Waiting> = crate::bootsel::waiting()
        .into_iter()
        .filter(|found| found.via == crate::bootsel::Via::Bootloader)
        .collect();
    let volume = match requested {
        Some(named) => named.to_owned(),
        None => match mounted.as_slice() {
            [] => {
                return Err(
                    "no board is in its bootloader. Hold BOOTSEL while plugging the board in \
                     (or press RESET\nwith BOOTSEL held), and it will appear as a drive."
                        .to_owned(),
                );
            }
            [only] => only.volume.clone(),
            several => {
                let list: Vec<&str> =
                    several.iter().map(|found| found.volume.as_str()).collect();
                return Err(format!(
                    "{} boards are in their bootloader and nothing on a volume tells them apart: \
                     {}\n\nName one with --probe <volume>. Their labels and their INFO_UF2.TXT \
                     files are identical, so\nthis will not guess -- the wrong choice puts your \
                     program on somebody else's board.",
                    several.len(),
                    list.join(", ")
                ));
            }
        },
    };
    let destination = std::path::Path::new(&volume).join("lamella.uf2");
    write_through(&destination, image)
        .map_err(|error| format!("copying to {}: {error}", destination.display()))?;
    Ok(Written { bytes: image.len(), words: image.len() / UF2_BLOCK })
}

/// Write `bytes` to `path` and make sure they have actually reached the device.
///
/// **`fs::write` IS NOT ENOUGH HERE AND THE FAILURE IS SILENT.** It writes and closes, which hands
/// the data to the operating system; on a removable volume the operating system is entitled to
/// hold it in cache. A bootloader volume is not a disk -- it is a device watching for blocks, and
/// it acts the moment they arrive. Without a flush the copy reports success, the file stays in the
/// directory listing, and the board stays in its bootloader, because nothing has been delivered:
/// every layer reports success and nothing happens.
///
/// `sync_all` is the difference: it flushes the file's buffers through to the device before the
/// call returns, so a success here means the board has the bytes.
fn write_through(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::File::create(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

/// A UF2 block, for reporting how many crossed.
const UF2_BLOCK: usize = 512;

/// A write failure in terms that name the next move.
fn describe(error: lamella_cmsis_dap_nrf::FlashError) -> String {
    match error {
        lamella_cmsis_dap_nrf::FlashError::ProbeOpen(text) => format!(
            "{text}\n\nName the board with --probe <serial>, or set LAMELLA_PROBE_SERIAL. \
             `lamella devices`\nlists what is attached. A bench with more than one board of a \
             family is refused rather than\nguessed at, because a write to the wrong board \
             succeeds and says nothing."
        ),
        other => format!("{other:?}"),
    }
}

/// What to print for a board this build cannot write.
///
/// **IT NAMES WHAT IS MISSING RATHER THAN REPORTING A CAPABILITY GAP.** The reader's question is
/// "can I use my board", and the honest answer distinguishes a board nobody has taught this tool
/// about from one that cannot work -- they are completely different waits.
fn cannot_write(board: &str) -> String {
    let mut text = format!("lamella flash: this build cannot write {board}.\n\n");
    text.push_str("it can write:\n");
    for row in PROGRAMMING {
        text.push_str(&format!("  {:<16} {}\n", row.board, row.programmer.description()));
    }
    text.push_str(
        "\nthat list is short because how a board is PROGRAMMED is not yet stated in any board \
         file --\nthe board files declare how a running board is TALKED TO, which is a different \
         fact. Every\nmechanism here has to be added by hand until it is.\n\n\
         `lamella build <file> --board ",
    );
    text.push_str(board);
    text.push_str("` still builds and measures an image for it.\n");
    text
}

const USAGE: &str = "\
usage: lamella flash <file.cs> --board <id> [--probe <serial>]

Builds the program ahead of time and writes it to the board over its debug probe. The board needs
nothing on it first -- the image IS the program.

--probe names WHICH board when more than one of a family is attached. Without it, LAMELLA_PROBE_SERIAL
is used, then the sole attached board of that family, and otherwise the write is REFUSED with every
candidate named. A write to the wrong board succeeds and reports nothing, so it is never guessed at.
";

#[cfg(test)]
mod tests {
    use super::*;

    /// **A CENSUS OVER THE TABLE, BECAUSE THE TABLE IS HAND-MAINTAINED AND IN THE WRONG PLACE.**
    /// Every board it names must exist in the catalogue and must name a chip the backend knows. A
    /// typo in either column would otherwise surface as "cannot write that board" or as a build
    /// failure at a user's prompt, neither of which points at this file.
    #[test]
    fn every_programmable_board_resolves_and_names_a_target_the_backend_knows() {
        for row in PROGRAMMING {
            assert!(
                catalogue::load_board(row.board).is_some(),
                "{}: not a board in the catalogue -- `lamella boards` does not list it",
                row.board
            );
            assert!(
                lamella_aot::build::CORTEX_M_TARGETS.contains(&row.aot_target),
                "{}: the backend does not know a chip called {:?}; it knows {:?}",
                row.board,
                row.aot_target,
                lamella_aot::build::CORTEX_M_TARGETS
            );
        }
        assert!(!PROGRAMMING.is_empty(), "an empty table would pass every assertion above");
    }

    /// A self-contained program of the shape the flat path covers: device registers written
    /// through raw pointers, then a loop that never returns. The addresses are the micro:bit v1's;
    /// what is under test is the PIPELINE, which lowers the same way whatever the constants are.
    const BLINK: &str = "\
class Program
{
    unsafe static int Main()
    {
        *(int*)0x50000518 = 0xFFF0;
        *(int*)0x50000508 = 0xE000;
        *(int*)0x5000050C = 0x1FF0;
        while (true)
        {
        }
    }
}
";

    /// **THE WHOLE COMPILE-AND-LOWER PATH, FOR EVERY BOARD IN THE TABLE, WITH NO HARDWARE.** This
    /// is the half of `flash` that can be gated, and it is the half that breaks silently: a board
    /// whose chip name the backend stopped accepting, or a lowering that stopped covering this
    /// shape, would otherwise surface as a failure at somebody's bench with a board in front of
    /// them.
    ///
    /// **IT WEIGHS THE ARTIFACT RATHER THAN THE EXIT.** A boot image that came back empty, or
    /// without the `[initial SP][reset]` header the chip resets into, is a successful build of
    /// something that cannot run -- and every exit code involved is zero.
    /// Where each part's vector table sits in its own image.
    ///
    /// **NOT EVERY IMAGE BEGINS WITH ITS VECTOR TABLE, AND ASSUMING SO IS A REAL BUG THIS TEST
    /// CAUGHT.** The Nordic parts and the RP2350 do. The RP2040 does not: its mask ROM checksums a
    /// 256-byte stage 2 at flash offset 0 and runs it from SRAM, so the vector table follows at
    /// `+0x100`. A single "word 0 is the stack pointer" rule read the RP2040's boot2 code as a
    /// stack pointer and failed with `0x88042014`, which is boot2, exactly as it should have.
    fn vector_offset(aot_target: &str) -> usize {
        match aot_target {
            "rp2040" => 0x100,
            _ => 0,
        }
    }

    #[test]
    fn every_programmable_board_builds_a_bootable_image_from_a_blink_program() {
        if lamella_wire_host::engine::LcscCompiler::discover().is_err() {
            return;
        }
        let path = Path::new("Blink.cs");
        for row in PROGRAMMING {
            let image = build_image(path, BLINK, row.aot_target, true)
                .unwrap_or_else(|error| panic!("{}: {error}", row.board));
            assert!(
                image.len() > 64,
                "{}: {} B is too small to be a program plus a vector table",
                row.board,
                image.len()
            );
            let at = vector_offset(row.aot_target);
            let sp = u32::from_le_bytes(image[at..at + 4].try_into().expect("four bytes"));
            let reset = u32::from_le_bytes(image[at + 4..at + 8].try_into().expect("four bytes"));
            assert!(
                (0x2000_0000..=0x2010_0000).contains(&sp),
                "{}: initial SP {sp:#010x} at offset {at:#x} is not in SRAM",
                row.board
            );
            assert!(reset & 1 == 1, "{}: reset vector {reset:#010x} has no Thumb bit", row.board);
        }
    }

    /// **THE UF2 A BOARD GETS MUST NAME THAT BOARD'S CHIP FAMILY**, or its bootloader refuses it.
    /// Asserted against the table rather than a literal at the call site, because the family is a
    /// property of the part and the two Pico generations do not share one.
    #[test]
    fn a_uf2_board_names_its_chip_family_and_a_probe_board_names_none() {
        assert_eq!(uf2_family_for_board("rpi-pico2"), Some(RP2350_UF2_FAMILY));
        assert_eq!(uf2_family_for_board("rpi-pico2-w"), Some(RP2350_UF2_FAMILY));
        assert_eq!(uf2_family_for_board("rpi-pico"), Some(RP2040_UF2_FAMILY));
        assert_ne!(
            RP2350_UF2_FAMILY, RP2040_UF2_FAMILY,
            "the two generations must not share a family, or each would accept the other's image"
        );
        assert_eq!(uf2_family_for_board("micro-bit-v2"), None, "written over a probe, not a volume");
    }

    /// A program with no static `Main` is refused BEFORE an image exists, because the reset vector
    /// would otherwise point at whatever lowered first -- which looks like a board that took the
    /// write and then misbehaved.
    #[test]
    fn a_program_with_no_entry_is_refused_by_name() {
        if lamella_wire_host::engine::LcscCompiler::discover().is_err() {
            return;
        }
        let library = "public sealed class Blink { public static unsafe void Run() { } }";
        let error = build_image(Path::new("Blink.cs"), library, "microbit", true)
            .expect_err("a library is not a program");
        assert!(error.contains("no static Main"), "it names the contract: {error}");
        assert!(error.contains("Run()"), "and the shape it is telling apart: {error}");
    }

    /// Two boards must not claim one entry, and one board must not appear twice with different
    /// mechanisms -- the second would make which one runs depend on table order.
    #[test]
    fn no_board_appears_twice() {
        let mut seen: Vec<&str> = PROGRAMMING.iter().map(|row| row.board).collect();
        seen.sort_unstable();
        let count = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), count, "a board is listed twice: {seen:?}");
    }

    /// **THE MESSAGE FOR AN UNWRITABLE BOARD IS THE PRODUCT HERE**, since most boards are in that
    /// case. It has to separate "nobody taught the tool" from "this cannot work", and leave the
    /// reader something that still works today.
    #[test]
    fn the_refusal_names_the_missing_fact_and_what_still_works() {
        let text = cannot_write("rpi-pico2");
        assert!(text.contains("micro-bit-v1"), "it lists what CAN be written: {text}");
        assert!(text.contains("not yet stated in any board file"), "and why the list is short");
        assert!(text.contains("lamella build"), "and what still works for that board");
        assert!(text.contains("rpi-pico2"), "and names the board asked for");
    }

    /// The coverage column `boards` prints has to agree with the table `flash` dispatches on.
    #[test]
    fn the_coverage_column_agrees_with_the_table() {
        assert!(can_flash("micro-bit-v1"));
        assert!(can_flash("rpi-pico2"));
        assert!(!can_flash("nucleo-f429zi"), "no mechanism is stated for the ST boards yet");
        assert!(!can_flash("no-such-board"));
    }

    /// **WITH NO TERMINAL THERE IS NOBODY TO ASK, AND THE ANSWER MUST STILL BE A REFUSAL.** A test
    /// process has no terminal, which is what makes this assertable here -- and it is the case
    /// that matters, because a script, a build, or an agent driving this tool is the situation in
    /// which a silent fallback would write somebody else's board.
    #[test]
    fn an_ambiguous_bench_with_no_terminal_refuses_and_names_the_candidates() {
        let candidates =
            vec!["E664A836A3198437".to_owned(), "E664A836A329AB37".to_owned()];
        let error = ask(&candidates).expect_err("no terminal in a test process");
        assert!(error.contains("E664A836A3198437"), "it names every candidate: {error}");
        assert!(error.contains("E664A836A329AB37"), "both of them: {error}");
        assert!(error.contains("--probe"), "and the way to choose: {error}");
        assert!(error.contains("succeeds and reports nothing"), "and why it will not guess");
    }

    /// The numbered list a person reads has to be the list the answer indexes into.
    #[test]
    fn the_candidate_list_is_numbered_from_one() {
        let text = list_of(&["AAA".to_owned(), "BBB".to_owned()]);
        assert!(text.contains("1. AAA"), "got {text}");
        assert!(text.contains("2. BBB"), "got {text}");
    }

    /// **AN EXPLICIT SERIAL MUST NOT REACH THE PROMPT.** The interactive rung sits below the
    /// refusal, which is below every rung that names a board -- so a named board is written
    /// without a question even on an ambiguous bench. Asserted without hardware: a serial nothing
    /// matches still comes back as itself, because the decision was already made.
    #[test]
    fn a_named_board_is_passed_through_without_asking() {
        let chosen = choose_board(Programmer::MicrobitV1Daplink, Some("A-SERIAL"))
            .expect("an explicit serial never consults the bench");
        assert_eq!(chosen.as_deref(), Some("A-SERIAL"));
    }
}
