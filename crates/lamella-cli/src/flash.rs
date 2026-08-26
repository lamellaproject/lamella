//! Writing bytes to a chip: the `flash` verb, and the mechanism `deploy --board` writes through.

use crate::args::{self, Spec};
pub use lamella_flash_routes::{can_flash, uf2_family_for_board};
use lamella_flash_routes::{
    Programmer, check_base, check_rp2350_stamp, is_uf2, programmer_for, route_for,
    selector_for, write,
};
use std::path::Path;
use std::process::ExitCode;







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



/// `lamella flash <artifact> --board <id>`: write bytes that are already an image.
///
/// **IT TAKES AN IMAGE, NEVER SOURCE.** Compiling a program and putting it on a board is `deploy`;
/// this writes bytes somebody else's toolchain may have produced. Keeping the two apart is the
/// whole reason the verb exists separately -- a tool with two words for one job teaches nobody
/// anything, and "flash a `.cs` file" is not a sentence about the hardware.
pub fn flash_command(args: &[String]) -> ExitCode {
    let spec = Spec {
        verb: "flash",
        usage: Some(USAGE),
        values: &["--board", "--probe", "--volume", "--via"],
        flags: &[],
    };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    let path = match parsed.only_positional("flash", POSITIONAL) {
        Ok(path) => Path::new(path).to_path_buf(),
        Err(error) => {
            eprintln!("{error}\n\n{USAGE}");
            return ExitCode::FAILURE;
        }
    };
    let manifest = match lamella_flash_routes::manifest::read(&path) {
        Ok(manifest) => manifest,
        Err(why) => {
            eprintln!(
                "lamella flash: {why}\n\n\
                 A sidecar that is present and cannot be read is a claim about this image that \
                 nobody can\ncheck, which is why it stops the write. Delete it to flash the bytes \
                 unchecked."
            );
            return ExitCode::FAILURE;
        }
    };
    let board_id = match (parsed.value("--board"), manifest.as_ref()) {
        (Some(named), Some(manifest)) => {
            if let Err(why) = lamella_flash_routes::manifest::check_board(manifest, named) {
                eprintln!("lamella flash: {why}");
                return ExitCode::FAILURE;
            }
            named
        }
        (Some(named), None) => named,
        (None, Some(manifest)) => manifest.board.as_str(),
        (None, None) => {
            eprintln!(
                "lamella flash: --board is required -- it says which chip is written.\n\n{USAGE}"
            );
            return ExitCode::FAILURE;
        }
    };
    if let Some(manifest) = manifest.as_ref() {
        let shipped = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!("lamella flash: read {}: {error}", path.display());
                return ExitCode::FAILURE;
            }
        };
        let extension = lamella_flash_routes::artifact::classify_format(&path);
        if let Err(why) =
            lamella_flash_routes::manifest::check_identity(manifest, &shipped, extension.as_deref())
        {
            eprintln!("lamella flash: {why}");
            return ExitCode::FAILURE;
        }
        println!("{}", lamella_flash_routes::manifest::attestation(manifest));
    }
    let row = match programmer_for(board_id) {
        Ok(row) => row,
        Err(error) => {
            eprint!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let chosen = match route_for(row, parsed.value("--via")) {
        Ok(programmer) => programmer,
        Err(error) => {
            eprintln!("lamella flash: {error}");
            return ExitCode::FAILURE;
        }
    };

    match lamella_flash_routes::artifact::classify(&path) {
        lamella_flash_routes::artifact::Kind::ChipImage => {}
        lamella_flash_routes::artifact::Kind::Source => {
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
        lamella_flash_routes::artifact::Kind::WirePayload => {
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
    let (bytes, described) = match chosen.required_format() {
        Some(required) => {
            let named = lamella_flash_routes::artifact::classify_format(&path);
            let raw = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) => {
                    eprintln!("lamella flash: read {}: {error}", path.display());
                    return ExitCode::FAILURE;
                }
            };
            match named.as_deref() {
                Some(extension) if extension == required.extension() => {
                    let count = raw.len();
                    (raw, format!("{count} B of {}", required.description()))
                }
                Some("bin") => {
                    if let Err(why) = check_rp2350_stamp(&raw, row.aot_target) {
                        eprintln!("lamella flash: {why}");
                        return ExitCode::FAILURE;
                    }
                    let count = raw.len();
                    (raw, format!("{count} B of raw binary"))
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
            let artifact = match lamella_flash_routes::artifact::read(&path) {
                Ok(artifact) => artifact,
                Err(error) => {
                    eprintln!("lamella flash: {error}");
                    return ExitCode::FAILURE;
                }
            };
            if let Err(error) = check_base(&artifact, chosen) {
                eprintln!("lamella flash: {error}");
                return ExitCode::FAILURE;
            }
            if let Err(why) = check_rp2350_stamp(&artifact.bytes, row.aot_target) {
                eprintln!("lamella flash: {why}");
                return ExitCode::FAILURE;
            }
            let described = format!("{} B of {}", artifact.bytes.len(), artifact.format);
            (artifact.bytes, described)
        }
    };
    println!("read {described} from {}", path.display());
    let selector = match selector_for(chosen, parsed.value("--probe"), parsed.value("--volume")) {
        Ok(selector) => selector,
        Err(error) => {
            eprintln!("lamella flash: {error}");
            return ExitCode::FAILURE;
        }
    };
    write_image(chosen, row.aot_target, &bytes, selector.as_deref())
}




/// What to tell the reader about a completed write.
///
/// **IT IS NOT THE SAME SENTENCE ON BOTH ROUTES.** A probe write reads every word back and may
/// therefore report a verification; a bootloader-volume write cannot, so it says what the
/// bootloader checked and states plainly that nothing read the flash. Most boards this build can write
/// take the volume route, so a single shared sentence claiming verification would
/// be wrong more often than right -- and wrong in the direction that reassures, since a reader
/// checking whether their image landed would be told a check had passed that never ran.
///
/// A pure function so the wording is testable without a board.
fn completion_line(programmer: Programmer, report: &lamella_flash_backend::Report) -> String {
    let units = programmer.units(report.bytes);
    match report.verification {
        lamella_flash_backend::Verification::ReadBack => {
            format!("wrote and verified {} B ({units}); the board is running it.", report.bytes)
        }
        lamella_flash_backend::Verification::NotPossible(_) => {
            let mut line =
                format!("wrote {} B ({units}); the board is running it.
", report.bytes);
            line.push_str(
                "The bootloader admitted the image -- its family id and every block's magic and ",
            );
            line.push_str(
                "index checked out --
but NOTHING READ THE FLASH BACK: this route hands over a ",
            );
            line.push_str("file and the volume unmounts.");
            line
        }
        lamella_flash_backend::Verification::Skipped => format!(
            "wrote {} B ({units}); the board is running it.
VERIFICATION WAS SKIPPED at your              request -- this route can read every byte back and was told not to.",
            report.bytes
        ),
    }
}




/// Write `image` to the board `row` describes, settling which physical board first.
///
/// Shared by `flash` and by `deploy --board`, so the probe ladder, the interactive rung and the
/// reporting are identical whether the bytes were compiled a moment ago or read off disk. The
/// board cannot tell the difference and neither should the output.
fn write_image(
    programmer: Programmer,
    _aot_target: Option<&str>,
    image: &[u8],
    probe: Option<&str>,
) -> ExitCode {
    let probe = match choose_board(programmer, probe) {
        Ok(chosen) => chosen,
        Err(error) => {
            eprintln!("lamella: {error}");
            return ExitCode::FAILURE;
        }
    };
    let wrapped;
    let image = match programmer.required_format() {
        Some(lamella_flash_routes::artifact::Format::Uf2 { family }) if !is_uf2(image) => {
            wrapped = lamella_flash_routes::artifact::Format::Uf2 { family }
                .render(image, programmer.flash_base());
            println!(
                "wrapped {} B as UF2 at {:#010x} (family {family:#010x})",
                image.len(),
                programmer.flash_base()
            );
            &wrapped[..]
        }
        _ => image,
    };
    println!("writing over {}...", programmer.description());
    match write(programmer, image, probe.as_deref()) {
        Ok(report) => {
            println!(
                "  the part answered {:#x} -- {}",
                report.identity.value, report.identity.what
            );
            println!("{}", completion_line(programmer, &report));
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
    via: Option<&str>,
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
    let Some(aot_target) = row.aot_target else {
        eprintln!("{}", cannot_build_for(board_id));
        return ExitCode::FAILURE;
    };
    let image = match build_image(path, &source, aot_target, unsafe_code) {
        Ok(image) => image,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    println!(
        "built {} B for {board_id} ({}), ahead of time -- no firmware needed on the board",
        image.len(),
        aot_target
    );
    let chosen = match route_for(row, via) {
        Ok(programmer) => programmer,
        Err(error) => {
            eprintln!("lamella deploy: {error}");
            return ExitCode::FAILURE;
        }
    };
    write_image(chosen, row.aot_target, &image, probe)
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
    let aot_target = row.aot_target.ok_or_else(|| cannot_build_for(board_id))?;
    let image = build_image(path, source, aot_target, unsafe_code)?;
    Ok((image, row.programmer.flash_base()))
}


/// The message for a board this tree can WRITE and cannot BUILD FOR.
///
/// **THE TWO VERBS DIVERGE HERE AND A READER HAS TO BE TOLD WHICH ONE THEY WANT.** `flash` takes an
/// image that already exists and does not care what built it; `deploy` compiles first, so a board
/// with no ahead-of-time target has nothing for it to compile TO. Saying "unsupported board" would
/// be false -- the board is in the table precisely because it can be written.
fn cannot_build_for(board_id: &str) -> String {
    let mut message = format!(
        "lamella deploy: {board_id} can be FLASHED but not BUILT FOR -- the ahead-of-time
"
    );
    message.push_str("backend has no target for its chip, so there is nothing to compile this
");
    message.push_str("program into.

");
    message.push_str(&format!("    lamella flash <image> --board {board_id}

"));
    message.push_str("writes an image that already exists, which is the verb this board
");
    message.push_str("supports today.");
    message
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













const USAGE: &str = "\
usage: lamella flash <image> [--board <id>] [--via probe|volume]
                          [--probe <serial>]  which probe, on a probe route
                          [--volume <name>]   which drive, on a volume route

Writes an image that ALREADY EXISTS to the board's chip, over its debug probe. It does not compile
anything -- `lamella build <file> --board <id> --format <f>` produces what this takes, and
`lamella deploy` is the two steps in one. The board needs nothing on it first.

The image is read by extension: .hex, .bin, .s19, .elf, or the .uf2 a bootloader-volume board
takes. A linked .elf is flattened the way `objcopy -O binary` would, by physical address, so an
image another toolchain produced needs no conversion step. A .bin for a bootloader-volume board is
wrapped into a .uf2 here, because the address and the family id that requires are facts about the
board and are already known.

--probe names WHICH probe when more than one is attached; --volume names which drive. They are
different questions and each belongs to one route, so naming the wrong one is refused rather than
ignored. Without either, LAMELLA_PROBE_SERIAL is used, then the sole candidate, and otherwise the
write is REFUSED with every candidate named. A write to the wrong board succeeds and reports
nothing, so it is never guessed at.

WITHOUT --via, the board is written by ITS OWN mechanism: a debugger soldered to it if it has one,
otherwise its bootloader drive. That needs no hardware you do not already own, and it is never a
guess -- a debugger on the board cannot be the one wired to something else, so having an external
probe plugged in as well changes nothing and you are not asked to choose.

--via asks for a DIFFERENT route than the board's own:

  volume   the bootloader drive. What a Pico takes by default, since it has no debugger of its
           own. It CANNOT read the flash back; what it can tell you is whether the board
           rebooted, which is the bootloader's acknowledgement that it took the image.
  probe    an EXTERNAL SWD probe. Reads every byte back and compares it, which is the only way
           to know the image is really there. Takes .bin/.hex/.elf/.s19, never a .uf2.

An external probe could be wired to any board, so when more than one is attached --via probe
REFUSES until you name one with --probe <serial>. That is the same rule as everywhere else and not
a stricter one: with a board's own debugger there is nothing to disambiguate, and here there is.

A SIDECAR beside the image -- <image>.manifest.json -- says which board it was built for and what
its bytes hash to. When one is there, --board is optional and is checked against it, and the image
is checked against its own digest before any probe is opened. A file of bytes cannot say which
board it belongs to, and two boards on a bench with two images in a directory is how the wrong one
gets written. An absent sidecar changes nothing; a sidecar that will not parse stops the write,
because a claim nobody can check is worse than no claim.
";

/// What `flash` calls the word it wants, in the one place both the error and the usage read it
/// from.
///
/// **THE ERROR AND THE USAGE ARE PRINTED TOGETHER, SO THEY MUST NAME THE SAME THING.**
/// `only_positional` builds the complaint from this noun and [`USAGE`] is printed directly beneath
/// it; taking both from here is what keeps the two sentences a reader sees from disagreeing.
const POSITIONAL: &str = "prebuilt image";


#[cfg(test)]
mod tests {
    use super::*;
    use lamella_flash_routes::{PROGRAMMING, RP2_XIP_BASE, RP2350_UF2_FAMILY, cannot_write};

    /// **THE TWO STRINGS ARE PRINTED TOGETHER, SO THEY MUST AGREE.** `eprintln!("{error}\n\n{USAGE}")`
    /// puts *give a prebuilt image* directly above the usage, and for as long as this verb compiled
    /// source and then stopped, the usage below it said `<file.cs>` and *Builds the program ahead of
    /// time*. Nothing could see it: the module header, `main.rs` and the error line were all right,
    /// and the one wrong string was the one a reader is shown at the moment they need it.
    ///
    /// Asserting the NOUN rather than the whole sentence is what makes this cheap enough to keep:
    /// the usage may be rewritten freely, and it may not stop naming the thing the verb asks for.
    /// **AND IT MUST NOT ASK FOR SOURCE.** The positive check above passes on a usage that says
    /// `<image>` and then goes on to describe compiling one, which is exactly the state this was
    /// found in. `flash` takes what `build` produced; `deploy` is the verb that compiles.
    /// **A BOARD WITH ITS OWN DEBUGGER IS NEVER AMBIGUOUS WITH AN EXTERNAL PROBE**, and this is
    /// the assertion that keeps it that way.
    ///
    /// A debugger soldered to a board cannot be wired to a different one, so naming the board has
    /// already named the debugger. Somebody who owns a micro:bit AND a Debug Probe must not be
    /// asked which to use -- the answer is the board's own, every time, without a prompt.
    ///
    /// The vendor/product filter is what delivers that: the external probe does not match it and
    /// is never a candidate. A future change that widened these routes to "any attached probe"
    /// would turn a bench with two pieces of hardware into a refusal, which is why the filter's
    /// PRESENCE is asserted rather than left as an implementation detail.
    /// **A DRIVE AND A PROBE ARE DIFFERENT QUESTIONS**, so naming the wrong one is refused rather
    /// than quietly dropped -- a reader who typed `--probe` at a volume route believed they had
    /// said which board.
    /// `--via probe` must be a deliberate act, never something a board falls into.
    ///
    /// **THE DEFAULT MATTERS MORE THAN THE OPTION.** Somebody opening a new Pico owns no probe, and
    /// a tool that reached for one by default would be unusable to them at exactly the moment they
    /// are deciding whether it works at all.
    /// A route a board does not have is refused BY NAME rather than silently ignored.
    ///
    /// Ignoring it would write the board over the volume while the reader believed a probe had
    /// been used -- and believed, therefore, that the image had been read back.
    /// An unknown `--via` value names both routes rather than restating the grammar.
    /// **THE PROBE ROUTE TAKES RAW BYTES AND THE VOLUME ROUTE TAKES A UF2**, so the artifact a verb
    /// demands has to follow the route rather than the board. Getting this backwards would write a
    /// UF2 CONTAINER into flash -- headers and all -- which boots into nothing.
    /// A UF2 must not be wrapped twice, and a flat image must be wrapped once.
    ///
    /// A report for wording tests, so the tests state the SITUATION and not the sentence.
    fn report(bytes: usize, verification: lamella_flash_backend::Verification)
        -> lamella_flash_backend::Report {
        lamella_flash_backend::Report {
            mechanism: "test",
            identity: lamella_flash_backend::PartIdentity { value: 0, what: "test" },
            base: 0,
            bytes,
            verification,
        }
    }

    /// **THE THREE OUTCOMES MUST READ DIFFERENTLY, AND ONE SHARED SENTENCE CANNOT CARRY THEM.** Most
    /// boards this build can write take the bootloader-volume route, where nothing reads anything
    /// back -- so a shared "wrote and verified" would be false more often than true, and false in
    /// the direction that reassures a reader checking whether their image landed.
    /// A read-back that really did happen must still be reported as one, or avoiding that claim
    /// would trade one false sentence for another.
    /// A skipped verification is a THIRD thing and must not read as either of the others.
    ///
    /// It is the state a reader is most likely to misread, because the write succeeded and the
    /// board is running: nothing about the outcome hints that a check the route CAN do was not done.
    ///
    /// **AN UNSTAMPED RP2350 IMAGE MUST BE REFUSED HERE, BECAUSE THE BOARD WILL NOT SAY ANYTHING.**
    /// The bootrom scans the first 4 KB for a PICOBIN block and, finding none, does not boot: no
    /// fault, no output, nothing to read back. A correct-but-unstamped image is indistinguishable
    /// from a blank chip and from a program that hung on its first instruction.
    /// **AND A STAMPED ONE MUST PASS, WHEREVER THE BLOCK SITS.** `lamella_aot` puts it at 0x40,
    /// right after the vector table; another toolchain may place it anywhere the bootrom looks, so
    /// a guard stricter than the bootrom would refuse images that boot. Both positions, because a
    /// fixed-offset check passes the first and fails the second.
    /// **THE GUARD IS FOR ONE PART AND MUST NOT REACH THE OTHERS.** A micro:bit image carries no
    /// PICOBIN block and never should; a guard that fired on every board would refuse every image
    /// this tool has ever written.
    /// An image shorter than the scan window is scanned as far as it goes rather than indexing off
    /// the end -- a trivial program builds to a few hundred bytes, well under the 4 KB window.
    /// **THE POSITIVE CONTROL, AND IT CROSSES THE CRATE BOUNDARY ON PURPOSE.** A guard that has
    /// only ever been seen to refuse is not a guard: this asserts that the image `lamella_aot`
    /// emits for an RP2350 passes it. The two sides state the same magic word independently -- the
    /// builder writes it, this reads it -- so a change to either that broke the pair would fail
    /// here rather than on a board that says nothing.
    /// **A CENSUS OVER THE TABLE, BECAUSE THE TABLE IS HAND-MAINTAINED AND IN THE WRONG PLACE.**
    /// Every board it names must exist in the catalog and must name a chip the backend knows. A
    /// typo in either column would otherwise surface as "cannot write that board" or as a build
    /// failure at a user's prompt, neither of which points at this file.
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

    /// **THE UF2 A BOARD GETS MUST NAME THAT BOARD'S CHIP FAMILY**, or its bootloader refuses it.
    /// Asserted against the table rather than a literal at the call site, because the family is a
    /// property of the part and the two Pico generations do not share one.
    /// A program with no static `Main` is refused BEFORE an image exists, because the reset vector
    /// would otherwise point at whatever lowered first -- which looks like a board that took the
    /// write and then misbehaved.
    /// Two boards must not claim one entry, and one board must not appear twice with different
    /// mechanisms -- the second would make which one runs depend on table order.
    /// **THE MESSAGE FOR AN UNWRITABLE BOARD IS THE PRODUCT HERE**, since most boards are in that
    /// case. It has to separate "nobody taught the tool" from "this cannot work", and leave the
    /// reader something that still works today.
    /// The coverage column `boards` prints has to agree with the table `flash` dispatches on.
    /// **WITH NO TERMINAL THERE IS NOBODY TO ASK, AND THE ANSWER MUST STILL BE A REFUSAL.** A test
    /// process has no terminal, which is what makes this assertable here -- and it is the case
    /// that matters, because a script, a build, or an agent driving this tool is the situation in
    /// which a silent fallback would write somebody else's board.
    /// The numbered list a person reads has to be the list the answer indexes into.
    /// **AN EXPLICIT SERIAL MUST NOT REACH THE PROMPT.** The interactive rung sits below the
    /// refusal, which is below every rung that names a board -- so a named board is written
    /// without a question even on an ambiguous bench. Asserted without hardware: a serial nothing
    /// matches still comes back as itself, because the decision was already made.
    #[test]
    fn the_usage_names_the_same_thing_the_error_asks_for() {
        assert!(
            USAGE.contains(POSITIONAL) || USAGE.contains("<image>"),
            "the error says {POSITIONAL:?} and the usage printed beside it does not mention it:\n{USAGE}"
        );
    }

    #[test]
    fn the_usage_does_not_promise_to_compile() {
        assert!(!USAGE.contains("file.cs"), "flash takes an image, not source:\n{USAGE}");
        assert!(
            !USAGE.contains("Builds the program"),
            "that sentence describes `deploy`, not `flash`:\n{USAGE}"
        );
        assert!(USAGE.contains("does not compile"), "it has to say so outright:\n{USAGE}");
    }

    #[test]
    fn a_write_that_could_not_be_checked_does_not_claim_it_was() {
        let volume = Programmer::Uf2Volume { family: RP2350_UF2_FAMILY, base: RP2_XIP_BASE };
        let line = completion_line(
            volume,
            &report(4096, lamella_flash_backend::Verification::NotPossible("the bootloader")),
        );
        assert!(!line.contains("verified"), "this route verifies nothing: {line}");
        assert!(line.contains("NOTHING READ THE FLASH BACK"), "and it must say so: {line}");
        assert!(
            line.contains("bootloader admitted"),
            "while crediting the check that DID run: {line}"
        );
    }

    #[test]
    fn a_write_that_was_checked_reports_it() {
        for probe in [Programmer::MicrobitV1Daplink, Programmer::MicrobitV2Daplink] {
            let line =
                completion_line(probe, &report(270, lamella_flash_backend::Verification::ReadBack));
            assert!(line.contains("verified"), "a probe write reads every word back: {line}");
            assert!(!line.contains("NOTHING READ"), "and must not disclaim it: {line}");
        }
    }

    #[test]
    fn a_skipped_verification_reads_as_neither_of_the_other_two() {
        let line = completion_line(
            Programmer::MicrobitV2Daplink,
            &report(64, lamella_flash_backend::Verification::Skipped),
        );
        assert!(line.contains("SKIPPED"), "the reader has to be told: {line}");
        assert!(!line.contains("and verified"), "nothing was verified: {line}");
        assert!(
            !line.contains("NOTHING READ THE FLASH BACK"),
            "that sentence belongs to a route that CANNOT read back, not one that was told not to:              {line}"
        );
    }

    #[test]
    fn every_programmable_board_builds_a_bootable_image_from_a_blink_program() {
        if lamella_wire_host::engine::LcscCompiler::discover().is_err() {
            return;
        }
        let path = Path::new("Blink.cs");
        let mut built = 0;
        for row in PROGRAMMING {
            let Some(target) = row.aot_target else {
                let refusal = cannot_build_for(row.board);
                assert!(refusal.contains(row.board), "{}: {refusal}", row.board);
                assert!(refusal.contains("lamella flash"), "{}: {refusal}", row.board);
                assert!(
                    image_for_board(path, BLINK, row.board, true).is_err(),
                    "{}: names no target, so building for it must refuse",
                    row.board
                );
                continue;
            };
            built += 1;
            let image = build_image(path, BLINK, target, true)
                .unwrap_or_else(|error| panic!("{}: {error}", row.board));
            assert!(
                image.len() > 64,
                "{}: {} B is too small to be a program plus a vector table",
                row.board,
                image.len()
            );
            let at = vector_offset(target);
            let sp = u32::from_le_bytes(image[at..at + 4].try_into().expect("four bytes"));
            let reset = u32::from_le_bytes(image[at + 4..at + 8].try_into().expect("four bytes"));
            assert!(
                (0x2000_0000..=0x2010_0000).contains(&sp),
                "{}: initial SP {sp:#010x} at offset {at:#x} is not in SRAM",
                row.board
            );
            assert!(reset & 1 == 1, "{}: reset vector {reset:#010x} has no Thumb bit", row.board);
        }
        assert!(built > 0, "no row named a target, so this proved nothing");
    }

    #[test]
    fn the_cannot_build_message_renders_without_stray_columns() {
        let message = cannot_build_for("nucleo-l053r8");
        for line in message.lines() {
            assert!(line == line.trim_end(), "a line ends in whitespace:
{message}");
            assert!(
                !line.starts_with(' ') || line.starts_with("    lamella "),
                "a continuation kept its source indentation:
{message}"
            );
            if !line.starts_with("    lamella ") {
                assert!(!line.contains("  "), "a doubled space mid-line:
{message}");
            }
        }
        assert!(message.contains("nucleo-l053r8"), "{message}");
        assert!(message.contains("lamella flash"), "it names the verb that works:
{message}");
    }

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

    #[test]
    fn the_refusal_names_the_missing_fact_and_what_still_works() {
        let text = cannot_write("rpi-pico2");
        assert!(text.contains("micro-bit-v1"), "it lists what CAN be written: {text}");
        assert!(text.contains("not yet stated in any board file"), "and why the list is short");
        assert!(text.contains("lamella build"), "and what still works for that board");
        assert!(text.contains("rpi-pico2"), "and names the board asked for");
    }

    #[test]
    fn the_coverage_column_agrees_with_the_table() {
        assert!(can_flash("micro-bit-v1"));
        assert!(can_flash("rpi-pico2"));
        assert!(!can_flash("nucleo-f429zi"), "no mechanism is stated for the ST boards yet");
        assert!(!can_flash("no-such-board"));
    }

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

    #[test]
    fn the_candidate_list_is_numbered_from_one() {
        let text = list_of(&["AAA".to_owned(), "BBB".to_owned()]);
        assert!(text.contains("1. AAA"), "got {text}");
        assert!(text.contains("2. BBB"), "got {text}");
    }

    #[test]
    fn a_named_board_is_passed_through_without_asking() {
        let chosen = choose_board(Programmer::MicrobitV1Daplink, Some("A-SERIAL"))
            .expect("an explicit serial never consults the bench");
        assert_eq!(chosen.as_deref(), Some("A-SERIAL"));
    }
}
