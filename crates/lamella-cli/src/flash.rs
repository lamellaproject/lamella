//! Writing bytes to a chip: the `flash` verb, and the mechanism `deploy --board` writes through.

use crate::args::{self, Spec};
pub use lamella_flash_routes::{can_flash, uf2_family_for_board};
use lamella_flash_routes::placement::{BootloaderChoice, Placement, placement_for};
use lamella_flash_routes::{
    NoAotTarget, Programmer, Programming, aot_target_for, prepare_image, programmer_for,
    route_for, selector_for, wrap_for_route, write,
};
use std::path::{Path, PathBuf};
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
pub(crate) fn choose_board(
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
        values: &[
            "--board",
            "--probe",
            "--volume",
            "--device",
            "--via",
            crate::bootprot::RESTORE_USER_ROW,
        ],
        flags: &[
            crate::bootprot::CLEAR_BOOTPROT,
            crate::bootprot::DRY_RUN,
            REPLACE_BOOTLOADER,
        ],
    };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };
    if let Some(step) = user_row_step_beside_a_replace(
        parsed.flag(REPLACE_BOOTLOADER),
        parsed.flag(crate::bootprot::CLEAR_BOOTPROT),
        parsed.value(crate::bootprot::RESTORE_USER_ROW).is_some(),
    ) {
        eprintln!(
            "lamella flash: {REPLACE_BOOTLOADER} says where an image is written, and {step} \
             writes no image.\nRun them as two commands.\n\n{USAGE}"
        );
        return ExitCode::FAILURE;
    }
    if let Some(file) = parsed.value(crate::bootprot::RESTORE_USER_ROW) {
        return crate::bootprot::restore_user_row_command(&parsed, file);
    }
    if parsed.flag(crate::bootprot::CLEAR_BOOTPROT) {
        return crate::bootprot::clear_bootprot_command(&parsed);
    }
    if parsed.flag(crate::bootprot::DRY_RUN) {
        eprintln!(
            "lamella flash: {} plans {} or {}, and an image write has no dry run.\n\n{USAGE}",
            crate::bootprot::DRY_RUN,
            crate::bootprot::CLEAR_BOOTPROT,
            crate::bootprot::RESTORE_USER_ROW
        );
        return ExitCode::FAILURE;
    }
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
    let choice = if parsed.flag(REPLACE_BOOTLOADER) {
        BootloaderChoice::Replace
    } else {
        BootloaderChoice::Keep
    };
    let placement = match placement_for(row, chosen, choice) {
        Ok(placement) => placement,
        Err(why) => {
            eprintln!("lamella flash: {why}");
            return ExitCode::FAILURE;
        }
    };
    let prepared = match prepare_image(&path, row, chosen, &placement) {
        Ok(prepared) => prepared,
        Err(why) => {
            eprintln!("lamella flash: {why}");
            return ExitCode::FAILURE;
        }
    };
    println!("read {} from {}", prepared.read, path.display());
    if let Some(wrapped) = &prepared.wrapped {
        println!("{wrapped}");
    }
    let selector = match selector_for(
        chosen,
        parsed.value("--probe"),
        parsed.value("--volume"),
        parsed.value("--device"),
    ) {
        Ok(selector) => selector,
        Err(error) => {
            eprintln!("lamella flash: {error}");
            return ExitCode::FAILURE;
        }
    };
    write_image(chosen, &placement, &prepared.bytes, selector.as_deref())
}

/// The user-row step asked for alongside `--replace-bootloader`, which the command refuses: the
/// option says where an image is written, and neither step writes one.
fn user_row_step_beside_a_replace(
    replace: bool,
    clear_bootprot: bool,
    restore_user_row: bool,
) -> Option<&'static str> {
    if !replace {
        return None;
    }
    if restore_user_row {
        return Some(crate::bootprot::RESTORE_USER_ROW);
    }
    clear_bootprot.then_some(crate::bootprot::CLEAR_BOOTPROT)
}

/// What a person is told before a write about the bootloader it keeps or replaces, or `None` on a
/// board whose facts state no bootloader.
fn placement_line(placement: &Placement) -> Option<String> {
    match placement {
        Placement::Start { .. } => None,
        Placement::Behind(bootloader) => Some(format!(
            "keeping the {} at {:#010x}-{:#010x}; the image is written behind it, from {:#010x}",
            bootloader.name,
            bootloader.base,
            bootloader.end() - 1,
            bootloader.end()
        )),
        Placement::Over(bootloader) => Some(format!(
            "replacing the {} at {:#010x}-{:#010x}. Until a bootloader is written back the board \
             has none:\nnothing uploads through it, an IDE over USB included, and no double-tap \
             reset enters it.\nThe way back is {REPLACE_BOOTLOADER} with the bootloader's own \
             file.",
            bootloader.name,
            bootloader.base,
            bootloader.end() - 1
        )),
    }
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
/// **A WRITE THROUGH A SYSTEM BOOTLOADER'S DFU INTERFACE DOES NOT SEE ITS IMAGE START.** The
/// bootloader reads every byte back, so that write is verified, but leaving DFU mode ends with the
/// bootloader gone and nothing reporting whether its jump to the image ran. That route says the
/// start was asked for, and what to do when the board does not run the image.
///
/// A pure function so the wording is testable without a board.
fn completion_line(programmer: Programmer, report: &lamella_flash_backend::Report) -> String {
    let units = programmer.units(report.bytes);
    match report.verification {
        lamella_flash_backend::Verification::ReadBack => match programmer {
            Programmer::StDfu { .. } => format!(
                "wrote and verified {} B ({units}); the bootloader was told to start it and does not \
                 report whether it did -- reset the board if it does not run it.",
                report.bytes
            ),
            _ => format!(
                "wrote and verified {} B ({units}); the board is running it.",
                report.bytes
            ),
        },
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
VERIFICATION WAS SKIPPED at your \
             request -- this route can read every byte back and was told not to.",
            report.bytes
        ),
    }
}




/// Write `image` through `programmer` at the address `placement` gives, settling which physical
/// board first.
///
/// Shared by `flash` and by `deploy --board`, so the probe ladder, the interactive rung and the
/// reporting are identical whether the bytes were compiled a moment ago or read off disk. The
/// board cannot tell the difference and neither should the output.
fn write_image(
    programmer: Programmer,
    placement: &Placement,
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
    let wrapped = wrap_for_route(programmer, image);
    let image = match &wrapped {
        Some((bytes, line)) => {
            println!("{line}");
            &bytes[..]
        }
        None => image,
    };
    if let Some(line) = placement_line(placement) {
        println!("{line}");
    }
    println!("writing over {}...", programmer.description());
    match write(programmer, placement, image, probe.as_deref()) {
        Ok(report) => {
            println!(
                "  the part answered {:#x} -- {}",
                report.identity.value, report.identity.what
            );
            println!("{}", completion_line(programmer, &report));
            if let Some(bootloader) = placement.kept() {
                let wait = match bootloader.reset_wait_ms {
                    Some(ms) => format!(", after the {ms} ms it waits on a reset"),
                    None => String::new(),
                };
                println!("  the {} starts it{wait}", bootloader.name);
            }
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
    volume: Option<&str>,
    device: Option<&str>,
    via: Option<&str>,
    unsafe_code: bool,
    tier: Tier,
    libraries: &[Library],
) -> ExitCode {
    let row = match programmer_for(board_id) {
        Ok(row) => row,
        Err(error) => {
            eprint!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let project = if is_project(path) {
        match crate::project::Project::read_file(path, "deploy") {
            Ok(project) => Some(project),
            Err(error) => {
                eprintln!("{error}");
                return ExitCode::FAILURE;
            }
        }
    } else {
        if let Some(what) = crate::deploy::uncompilable_source(path) {
            eprintln!("{}", crate::deploy::deploy_refusal(path, &what));
            return ExitCode::FAILURE;
        }
        None
    };
    let source = match (&project, std::fs::read_to_string(path)) {
        (Some(_), _) => String::new(),
        (None, Ok(source)) => source,
        (None, Err(error)) => {
            eprintln!("lamella deploy: read {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };
    let Some(aot_target) = row.aot_target else {
        eprintln!(
            "{}",
            cannot_build_for(board_id, "deploy", &NoAotTarget::WritableButNotBuildable)
        );
        return ExitCode::FAILURE;
    };
    let built = match &project {
        Some(project) => build_project_image(project, aot_target, tier, "deploy", path),
        None => build_image(path, &source, aot_target, unsafe_code, tier, libraries, "deploy"),
    };
    let image = match built {
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
    println!("  {}", tier.line());
    let chosen = match route_for(row, via) {
        Ok(programmer) => programmer,
        Err(error) => {
            eprintln!("lamella deploy: {error}");
            return ExitCode::FAILURE;
        }
    };
    let placement = match deploy_placement(row, chosen) {
        Ok(placement) => placement,
        Err(why) => {
            eprintln!("lamella deploy: {why}");
            return ExitCode::FAILURE;
        }
    };
    let selector = match selector_for(chosen, probe, volume, device) {
        Ok(selector) => selector,
        Err(error) => {
            eprintln!("lamella deploy: {error}");
            return ExitCode::FAILURE;
        }
    };
    write_image(chosen, &placement, &image, selector.as_deref())
}

/// Where `deploy --board` writes the image it compiled for `row`'s board through `route`.
///
/// It keeps a bootloader the board's product ships, as every write does unless asked otherwise,
/// and a compiled image is linked to run from where the route writes. So a board that keeps a
/// bootloader there is refused rather than written: behind the bootloader the image would not run,
/// and over it the bootloader would be gone without anybody asking.
///
/// # Errors
/// A board that keeps a bootloader where its route writes from, and a board whose facts do not
/// give a placement.
fn deploy_placement(row: &Programming, route: Programmer) -> Result<Placement, String> {
    let placement = placement_for(row, route, BootloaderChoice::Keep)?;
    if let Some(bootloader) = placement.kept() {
        return Err(format!(
            "{} keeps the {} at {:#010x}-{:#010x}, and this deploy links an image to run from \
             {:#010x},\nwhere the bootloader is. Nothing was written.",
            row.board,
            bootloader.name,
            bootloader.base,
            bootloader.end() - 1,
            route.flash_base()
        ));
    }
    Ok(placement)
}

/// The bare-metal image for `board_id`, and the address it belongs at -- what `build --format`
/// writes to a file and `deploy --board` writes to a chip.
///
/// **ONE FUNCTION, SO THE FILE AND THE CHIP GET THE SAME BYTES.** A `build` that produced anything
/// other than what `deploy` writes would make the two verbs disagree exactly where somebody is
/// least able to check -- an image handed to a colleague, or archived for a release.
///
/// **`build` IS THE ONLY VERB THAT REACHES HERE**, which is why the refusals below name it: the
/// `deploy` path holds its own copy of the same two steps. A second caller would have to pass its
/// own verb rather than inherit this one, or it would answer a reader about a verb they never typed.
///
/// # Errors
/// An unwritable board, or any compile or lowering failure, already worded for a reader.
pub fn image_for_board(
    path: &Path,
    source: &str,
    board_id: &str,
    unsafe_code: bool,
    tier: Tier,
    libraries: &[Library],
) -> Result<(Vec<u8>, u32), String> {
    let aot_target = target_to_build_for(board_id)?;
    let row = programmer_for(board_id)?;
    let image = if is_project(path) {
        let project = crate::project::Project::read_file(path, "build")?;
        build_project_image(&project, aot_target, tier, "build", path)?
    } else {
        build_image(path, source, aot_target, unsafe_code, tier, libraries, "build")?
    };
    Ok((image, row.programmer.flash_base()))
}

/// The ahead-of-time target `build` compiles for `board_id`, or the refusal worded for `build`.
fn target_to_build_for(board_id: &str) -> Result<&'static str, String> {
    aot_target_for(board_id).map_err(|reason| match &reason {
        NoAotTarget::UnknownBoard(error) => format!("lamella build: {error}"),
        _ => cannot_build_for(board_id, "build", &reason),
    })
}

/// The class-library image for `board_id` as a linked ELF that also carries the program's debug
/// information -- what `build --class-library --format elf` writes.
///
/// **THE PROGRAM IS COMPILED FOR A DEBUGGER AND LINKED BY THE PIPELINE THE CLASS-LIBRARY IMAGE IS
/// LINKED BY**, so the debug information is placed by the link that placed the code. Only the
/// program is described; corlib, the libraries beside it and the runtime support archive are linked
/// as the image links them.
///
/// **`build` IS THE ONLY VERB THAT REACHES HERE**, as for [`image_for_board`], and the refusals
/// name it.
///
/// # Errors
/// As [`image_for_board`] on the class-library tier.
pub fn debug_elf_for_board(
    path: &Path,
    source: &str,
    board_id: &str,
    unsafe_code: bool,
    libraries: &[Library],
) -> Result<Vec<u8>, String> {
    let aot_target = target_to_build_for(board_id)?;
    if is_project(path) {
        let project = crate::project::Project::read_file(path, "build")?;
        let libraries = project_libraries(&project, Tier::ClassLibrary, "build")?;
        let debuggable =
            crate::program::compile_project_for_debugging(&project, &libraries, "build")?;
        debug_elf_from(&debuggable, &libraries, aot_target, "build", path)
    } else {
        let debuggable = crate::program::compile_csharp_for_debugging(
            path,
            source,
            unsafe_code,
            libraries,
            "build",
        )?;
        debug_elf_from(&debuggable, libraries, aot_target, "build", path)
    }
}

/// Link a program compiled for a debugger into the class-library tier's ELF for `aot_target`.
///
/// `path` is what the reader named, as for [`image_from_assembly`].
fn debug_elf_from(
    debuggable: &crate::program::Debuggable,
    libraries: &[Library],
    aot_target: &str,
    verb: &str,
    path: &Path,
) -> Result<Vec<u8>, String> {
    require_static_main(&debuggable.assembly, verb, path)?;
    class_library_build(aot_target, verb, |archive| {
        linked_debug_build(
            &debuggable.assembly,
            &debuggable.pdb,
            &debuggable.corlib,
            libraries,
            archive,
            aot_target,
        )
    })
}


/// The message for a board this tree can WRITE and cannot BUILD FOR.
///
/// **THE TWO VERBS DIVERGE HERE AND A READER HAS TO BE TOLD WHICH ONE THEY WANT.** `flash` takes an
/// image that already exists and does not care what built it; `deploy` compiles first, so a board
/// with no ahead-of-time target has nothing for it to compile TO. Saying "unsupported board" would
/// be false -- the board is in the table precisely because it can be written.
fn cannot_build_for(board_id: &str, verb: &str, reason: &NoAotTarget) -> String {
    let mut message = format!(
        "lamella {verb}: nothing here names an ahead-of-time target for {board_id}, so this
"
    );
    message.push_str("verb has nothing to compile the program into.

");
    match reason {
        NoAotTarget::WritableButNotBuildable => {
            message.push_str(&format!("    lamella flash <image> --board {board_id}

"));
            message.push_str("writes an image that already exists, which is the verb this board
");
            message.push_str("supports today.

");
        }
        _ => {
            message.push_str("nothing states how to write this board either, so `lamella flash` is
");
            message.push_str("not a way around it.

");
        }
    }
    message.push_str("a board reaches this two ways, and they are a different wait: no code
");
    message.push_str("generator exists for its instruction set, or one exists and nobody has
");
    message.push_str(&format!("wired it to this board. this build generates {}.
", generators_in_this_build()));
    message
}

// AT LEAST ONE CODE GENERATOR, AND THE FLOOR IS STATED HERE RATHER THAN DISCOVERED.
//
// The generator features are additive, so nothing stops a build selecting none of them -- and a
// build with none can lower a program for no board in the catalog at all. `lamella-aot` compiles
// its build entry point only when some instruction set is selected, and this file calls that
// entry point unconditionally, so the configuration fails in ANOTHER crate's vocabulary: three
// "cannot find `build` in `lamella_aot`" against a gate the reader never wrote. One sentence
// about this crate's own features is a better answer than three about somebody else's.
#[cfg(not(any(feature = "aot-arm32", feature = "aot-riscv32")))]
compile_error!(
    "lamella-cli needs at least one code generator: build it with `aot-arm32`, with \
     `aot-riscv32`, or with the default feature set, which carries both. A build with neither \
     can generate code for no board this tool knows."
);

/// The instruction sets THIS BINARY can generate code for, as a phrase for a refusal to quote.
///
/// **A tool cannot report a capability it was not linked with, and it must not report one it was.**
/// The ahead-of-time backend selects its code generators by cargo feature, so what the project can
/// compile and what the program in front of a reader can compile are two different questions --
/// and the second is the one a refusal is answering. Deriving the list keeps them apart: a build
/// configured differently says something different, without anyone editing this sentence.
fn generators_in_this_build() -> String {
    let mut sets: Vec<&str> = Vec::new();
    if cfg!(feature = "aot-arm32") {
        sets.push("Cortex-M");
    }
    if cfg!(feature = "aot-riscv32") {
        sets.push("RISC-V");
    }
    match sets.as_slice() {
        // A match over a slice has to cover the empty case, and the guard above is what makes
        // this one unreachable: a binary with no generator does not link. Kept as the honest
        // answer to the question rather than an `unreachable!`, which would turn a build
        // configuration into a panic a reader meets at run time.
        [] => "no instruction set at all, which is a build configuration error".to_owned(),
        [only] => format!("code for {only}"),
        [first, rest @ ..] => format!("code for {first} and {}", rest.join(" and ")),
    }
}

/// Which ahead-of-time tier a build asks for.
///
/// **THE TIER IS ASKED FOR, NEVER INFERRED, AND NEVER FALLEN BACK FROM.** The two tiers differ in
/// what a program may contain and in how much flash the result needs, so choosing between them on
/// the program's behalf would mean a build that quietly changes shape when an edit adds a call --
/// and an image that fits one board and not the next. A person who asks for one and cannot have it
/// is told; they are not handed the other.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// The flat, linker-free path: nothing outside the program resolves, and the image needs
    /// nothing on the board.
    Flat,
    /// Linked against the class library and the runtime-support archive, which is what
    /// `--class-library` asks for.
    ClassLibrary,
}

/// The option that asks for the class-library tier, spelled once.
///
/// **.NET's OWN WORD FOR WHAT IT ADDS.** The flag names what the reader gains rather than the
/// mechanism that delivers it: `--linked` describes a link step to somebody who never asked for
/// one, and the sentence the flat tier's refusal already prints is about the class library.
pub const CLASS_LIBRARY_FLAG: &str = "--class-library";

impl Tier {
    /// The tier a parsed command line asks for.
    ///
    /// **ONE MAPPING, READ BY EVERY VERB THAT TAKES THE FLAG.** `build`, `deploy` and the debug
    /// server all offer it, and a flag that meant the flat tier at one verb and the linked one at
    /// another would be discovered as an image that does not match the command that produced it.
    #[must_use]
    pub fn from_options(parsed: &crate::args::Options) -> Self {
        if parsed.flag(CLASS_LIBRARY_FLAG) {
            Self::ClassLibrary
        } else {
            Self::Flat
        }
    }

    /// The one line every build states about itself.
    ///
    /// **STATED ALWAYS, NOT ONLY WHEN IT IS THE UNUSUAL ONE.** Which tier built an image decides
    /// what the program was allowed to contain and how large the result is, and a reader comparing
    /// two builds -- or reading a log of somebody else's -- cannot recover it from the byte count.
    /// A line that appears only in the non-default case is a line nobody learns to look for.
    #[must_use]
    pub fn line(self) -> &'static str {
        match self {
            Self::Flat => {
                "tier: flat -- linker-free, no class library, nothing needed on the board"
            }
            Self::ClassLibrary => {
                "tier: class library -- linked against corlib and the runtime support archive"
            }
        }
    }
}

/// A class library named on the command line, with the bytes it was read from.
///
/// **THE PATH TRAVELS WITH THE BYTES BECAUSE EVERY REFUSAL BELOW IT NAMES A FILE.** A library that
/// cannot be read, or that the link then rejects, is somebody's typo in a path -- and an error
/// holding only an assembly's internal name asks them to work out which of their `-r` arguments
/// produced it.
#[derive(Debug)]
pub struct Library {
    /// The path as it was typed, for a message that sends the reader back to their command line.
    path: PathBuf,
    /// What was read from it.
    bytes: Vec<u8>,
}

impl Library {
    /// The assembly's bytes, as the build entry point wants them.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The path as it was typed, for a refusal that sends the reader back to their command line.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// The class libraries a build asks to link, **in the order they were declared**, or why not.
///
/// `named` are the paths a PROJECT FILE gave, in document order.
///
/// **THE ORDER IS SEMANTIC AND IS CARRIED THROUGH UNTOUCHED.** Precedence across the reference set
/// is first-declarer-wins, so sorting these, de-duplicating them or reading them out of a set would
/// silently change which definition of a name the program links against. That is a defect with no
/// symptom at build time: the image is produced, it is the wrong program, and nothing said so.
///
/// # Errors
/// A reference declared without the tier that can link it, or a file that cannot be read -- each
/// naming the path the project gave.
pub fn libraries_from(named: &[&str], tier: Tier, verb: &str) -> Result<Vec<Library>, String> {
    if named.is_empty() {
        return Ok(Vec::new());
    }
    if tier != Tier::ClassLibrary {
        return Err(reference_without_the_tier(verb, named));
    }
    named
        .iter()
        .map(|named| {
            let path = PathBuf::from(named);
            std::fs::read(&path)
                .map(|bytes| Library { path, bytes })
                .map_err(|error| {
                    format!(
                        "lamella {verb}: read the class library {named}: {error}\n\n\
                         A <Reference> names an assembly to link, by a path to a built \
                         `.dll`.\nNothing was built."
                    )
                })
        })
        .collect()
}

/// Why a reference cannot be honored on the flat tier.
///
/// **IGNORING IT IS THE ONE THING THIS MUST NOT DO.** The flat tier resolves no call outside the
/// program, so a library named at it can have no effect -- and an input accepted and then
/// discarded is this crate's oldest defect shape: a `--target` quietly ignored looks exactly like a
/// board that did not respond. The reader is told, and nothing is built.
fn reference_without_the_tier(verb: &str, named: &[&str]) -> String {
    let listed = named
        .iter()
        .map(|named| format!("    {named}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "lamella {verb}: this project declares a reference, and {CLASS_LIBRARY_FLAG} was not \
         asked for.\n\n\
         Declared:\n{listed}\n\n\
         The flat tier is linker-free and resolves no call outside the program, so it cannot \
         link a class\nlibrary and these would have no effect. Add {CLASS_LIBRARY_FLAG} to link \
         them, or drop them to\nbuild the program on its own.\n\n\
         Nothing was built."
    )
}

/// Compile `source` and lower it ahead of time to a bare-metal image for `aot_target`, on `tier`.
///
/// **ON THE CLASS-LIBRARY TIER THE PROGRAM IS COMPILED AS A DEBUG BUILD IS**, so the image is the
/// one `build --format elf` describes; see [`compiles_as_debug_build`].
///
/// `verb` is the command the reader typed, so a refusal names the verb they ran rather than a
/// sibling that shares this path.
fn build_image(
    path: &Path,
    source: &str,
    aot_target: &str,
    unsafe_code: bool,
    tier: Tier,
    libraries: &[Library],
    verb: &str,
) -> Result<Vec<u8>, String> {
    let (assembly, corlib) = if compiles_as_debug_build(tier) {
        let compiled = crate::program::compile_csharp_for_debugging(
            path,
            source,
            unsafe_code,
            libraries,
            verb,
        )?;
        (compiled.assembly, compiled.corlib)
    } else {
        crate::program::compile_csharp_assembly_with_corlib(
            path,
            source,
            unsafe_code,
            libraries,
            verb,
        )?
    };
    image_from_assembly(&assembly, &corlib, aot_target, tier, libraries, verb, path)
}

/// Whether a program built on `tier` is compiled as a debug build is, even for an image that is
/// only written to a board.
///
/// **THE CLASS-LIBRARY TIER IS THE ONE WHOSE IMAGE A DEBUGGER IS GIVEN**, as the ELF
/// `build --format elf` writes, and that ELF is only true of the board if the board runs the same
/// bytes. A debug compile does not lower to the same bytes as a plain one: its IL differs -- a
/// method that returns a value gets a return slot -- and so does the assembly file, whose hash is
/// part of symbol names the link orders by. So every class-library image comes from the debug
/// compile, and the debug information is set aside where it is not written. The flat tier carries
/// no debug information into an image and is compiled without it.
fn compiles_as_debug_build(tier: Tier) -> bool {
    tier == Tier::ClassLibrary
}

/// The image for a PROJECT: every source it names compiled into one assembly, then lowered.
///
/// **THE SAME LOWERING A SINGLE FILE TAKES.** What a project changes is which sources compile and
/// what they bind against; what happens to the resulting assembly is not a property of how it was
/// described, and two paths to an image would be two places for a tier to be chosen.
///
/// # Errors
/// As the single-file path, plus anything wrong with the project itself.
fn build_project_image(
    project: &crate::project::Project,
    aot_target: &str,
    tier: Tier,
    verb: &str,
    path: &Path,
) -> Result<Vec<u8>, String> {
    let libraries = project_libraries(project, tier, verb)?;
    let (assembly, corlib) = if compiles_as_debug_build(tier) {
        let compiled =
            crate::program::compile_project_for_debugging(project, &libraries, verb)?;
        (compiled.assembly, compiled.corlib)
    } else {
        crate::program::compile_project_assembly(project, &libraries, verb)?
    };
    image_from_assembly(&assembly, &corlib, aot_target, tier, &libraries, verb, path)
}

/// The class libraries `project` references, read and in the order it declares them.
///
/// # Errors
/// As [`libraries_from`].
fn project_libraries(
    project: &crate::project::Project,
    tier: Tier,
    verb: &str,
) -> Result<Vec<Library>, String> {
    let named: Vec<&str> = project
        .references
        .iter()
        .filter_map(|one| one.to_str())
        .collect();
    libraries_from(&named, tier, verb)
}

/// Lower an assembly that is already compiled, on `tier`.
///
/// `path` is what the reader named -- a source file or a project -- so a refusal points at the
/// thing they typed rather than at whichever file inside it happened to be compiled first.
fn image_from_assembly(
    assembly: &[u8],
    corlib: &[u8],
    aot_target: &str,
    tier: Tier,
    libraries: &[Library],
    verb: &str,
    path: &Path,
) -> Result<Vec<u8>, String> {
    require_static_main(assembly, verb, path)?;
    if tier == Tier::ClassLibrary {
        return class_library_build(aot_target, verb, |archive| {
            linked_build(assembly, corlib, libraries, archive, aot_target)
        });
    }
    lamella_aot::build::build(assembly, aot_target)
        .map_err(|error| flat_refusal(verb, aot_target, &error))
}

/// Refuse an assembly with no static `Main`, naming `path`, the thing the reader typed.
///
/// **THE ENTRY CONTRACT IS CHECKED BEFORE AN IMAGE IS BUILT.** The boot image's reset vector points
/// at the entry, so an assembly with no static `Main` would produce an image that boots into
/// whatever lowered first -- which looks exactly like a board that took the write and then
/// misbehaved.
fn require_static_main(assembly: &[u8], verb: &str, path: &Path) -> Result<(), String> {
    if has_static_main(assembly) {
        return Ok(());
    }
    Err(format!(
        "lamella {verb}: {} declares no static Main.\n\n\
         A flashed image IS the program: the chip resets straight into it, so it needs one \
         entry point.\nAdd `static void Main()` (or `static int Main()`) to a class in this \
         file. A sample written as a\nlibrary -- a `Run()` that a harness calls -- has to gain \
         a Main before it can be deployed on its own.",
        path.display()
    ))
}

/// Run `link` on the class-library tier for `aot_target` with the runtime support archive it
/// needs, or say why not.
///
/// **ONE COPY OF THE TIER'S REFUSALS, ITS ARCHIVE DISCOVERY AND ITS FAILURE WORDING**, shared by the
/// image a board is written with and the ELF a debugger is given, so the two builds cannot come to
/// disagree about when the tier is available or which archive they link.
///
/// **THE REFUSALS ARE ORDERED BY WHAT A READER CAN DO ABOUT THEM, MOST FUNDAMENTAL FIRST.** A
/// target the tier has no plan for is not fixed by finding an archive, and an archive is not worth
/// looking for in a binary that could not link it -- so sending somebody to hunt for a file when
/// the answer is neither would waste their afternoon.
fn class_library_build<T>(
    aot_target: &str,
    verb: &str,
    link: impl FnOnce(&[u8]) -> Result<T, String>,
) -> Result<T, String> {
    if !crate::tiers::covers(aot_target) || !linked_tier_compiled_in() {
        return Err(class_library_refusal(verb, aot_target));
    }
    let (archive_path, archive) = crate::tiers::runtime_archive(aot_target)
        .map_err(|reason| format!("lamella {verb}: {reason}"))?;
    link(&archive).map_err(|error| {
        let head =
            wrapped(&format!("lamella {verb}: the class-library build failed: {error}"));
        format!(
            "{head}\n\nThe program and the class library were linked against {}.\n\
             Nothing was written.",
            archive_path.display()
        )
    })
}

/// The library set as the linked entry point takes it: bytes alone, **in declaration order**.
///
/// **NOTHING HERE SORTS OR DE-DUPLICATES.** The entry point searches these in the order given, so
/// precedence across a reference set is first-declarer-wins -- reordering them would change which
/// definition of a duplicated name the program binds, and it would do it with a successful build to
/// hide it. Which order a project wants is the project's to say, and [`Project::references`] already
/// keeps the order it was written in.
fn library_bytes(libraries: &[Library]) -> Vec<&[u8]> {
    libraries.iter().map(Library::bytes).collect()
}

/// The linked build itself, present only where the tier was compiled in.
///
/// **TWO ARMS RATHER THAN A `cfg!` INSIDE ONE**, because the entry point does not EXIST without the
/// feature: a single body would not compile in a build that left the tier out, which is the
/// configuration the refusal above is written for.
#[cfg(feature = "class-library")]
fn linked_build(
    assembly: &[u8],
    corlib: &[u8],
    libraries: &[Library],
    archive: &[u8],
    aot_target: &str,
) -> Result<Vec<u8>, String> {
    lamella_aot::build::build_linked_cortex_m_with_libraries(
        assembly,
        corlib,
        &library_bytes(libraries),
        archive,
        aot_target,
    )
    .map_err(|error| format!("{error}"))
}

/// A refusal's first sentence, wrapped so a terminal does not choose the break for it.
///
/// **THE BACKEND'S OWN PROSE ARRIVES AS ONE LINE.** A `BuildError` renders as a sentence naming
/// what could not be built and where -- which is what a reader wants, and is routinely longer
/// than a terminal is wide. These messages list boards underneath that sentence, so a line the
/// terminal soft-wraps pushes the list out of alignment and the refusal reads as ragged output
/// rather than as an answer. Wrapping is the caller's job: only the caller knows its message is
/// a block rather than a line.
fn wrapped(text: &str) -> String {
    const WIDTH: usize = 96;
    let mut out = String::with_capacity(text.len() + text.len() / WIDTH + 1);
    for (index, paragraph) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let mut column = 0;
        for word in paragraph.split(' ') {
            let width = word.chars().count();
            if column > 0 && column + 1 + width > WIDTH {
                out.push('\n');
                column = 0;
            } else if column > 0 {
                out.push(' ');
                column += 1;
            }
            out.push_str(word);
            column += width;
        }
    }
    out
}

/// Unreachable in a build without the tier -- [`linked_tier_compiled_in`] refuses above it -- and
/// present so that this file compiles when the entry point does not exist.
#[cfg(not(feature = "class-library"))]
fn linked_build(
    _assembly: &[u8],
    _corlib: &[u8],
    _libraries: &[Library],
    _archive: &[u8],
    _aot_target: &str,
) -> Result<Vec<u8>, String> {
    Err("the class-library tier is not compiled into this build".to_owned())
}

/// [`linked_build`] for a program compiled for a debugger, with `pdb` the Portable PDB it was
/// compiled with: the linked ELF carrying its debug information. Present only where the tier was
/// compiled in, for the same reason.
#[cfg(feature = "class-library")]
fn linked_debug_build(
    assembly: &[u8],
    pdb: &[u8],
    corlib: &[u8],
    libraries: &[Library],
    archive: &[u8],
    aot_target: &str,
) -> Result<Vec<u8>, String> {
    let pdb = lamella_metadata::PortablePdb::read(pdb).map_err(|error| {
        format!("the debug information the compiler wrote does not read back: {error:?}")
    })?;
    lamella_aot::build::build_linked_cortex_m_debug(
        assembly,
        &pdb,
        corlib,
        &library_bytes(libraries),
        archive,
        aot_target,
    )
    .map_err(|error| format!("{error}"))
}

/// Unreachable in a build without the tier, as the [`linked_build`] beside it is.
#[cfg(not(feature = "class-library"))]
fn linked_debug_build(
    _assembly: &[u8],
    _pdb: &[u8],
    _corlib: &[u8],
    _libraries: &[Library],
    _archive: &[u8],
    _aot_target: &str,
) -> Result<Vec<u8>, String> {
    Err("the class-library tier is not compiled into this build".to_owned())
}

/// Whether this binary carries the class-library tier.
///
/// **ONE FUNCTION, SO TURNING THE TIER ON IS ONE EDIT.** It mirrors the `aot-arm32` and
/// `aot-riscv32` passthroughs, which exist because a cargo feature is not visible to `cfg` outside
/// the crate that declares it -- and a refusal that answers from a hand-written list is answering
/// from memory rather than from the build.
const fn linked_tier_compiled_in() -> bool {
    cfg!(feature = "class-library")
}

/// Why the flat tier did not build this program, and -- where it applies -- which tier would.
///
/// **A LIMIT STATED WITH NO WAY OUT OF IT READS AS A LIMIT OF THE PROJECT.** This message said that
/// the flat tier resolves no call outside the program and stopped there, which is true and was read
/// by three separate parties as "C# on a board can do nothing". The other tier already existed. The
/// sentence naming it is the whole difference.
///
/// **THE WAY OUT IS OFFERED ONLY WHERE IT LEADS SOMEWHERE.** It is named for the one failure it
/// answers -- a call the flat tier cannot resolve -- and only for a target the linked tier has a
/// plan for. A flag suggested to somebody it would then refuse costs them a build to find out, and
/// teaches them the suggestion is not worth reading next time.
fn flat_refusal(verb: &str, aot_target: &str, error: &lamella_aot::build::BuildError) -> String {
    let head = wrapped(&format!(
        "lamella {verb}: the ahead-of-time build failed: {error}"
    ));
    let limits = format!(
        "{head}\n\n\
         This is the flat tier: it is linker-free and resolves no call outside the program, so \
         floating\npoint, allocation, and anything reaching the class library are unavailable in \
         it. A program that\nwrites device registers and loops is the shape it covers."
    );
    if !is_unresolvable_call(error) || !crate::tiers::covers(aot_target) {
        return limits;
    }
    format!(
        "{limits}\n\n\
         That is a limit of the tier and not of the board. To build this program against the class \
         library,\nwhich links it with corlib and the runtime support archive, add \
         {CLASS_LIBRARY_FLAG}."
    )
}

/// The `--board` values whose target the class-library tier has a plan for, as an indented list.
///
/// **BOARDS, BECAUSE A BOARD IS WHAT THE READER TYPED.** `--class-library` is refused against an
/// AOT target name, which is a fact about the chip and appears on no command line; answering a
/// person holding a micro:bit with the word `microbit` when they wrote `--board bbc-micro-bit-v1`
/// asks them to work out the mapping. Derived from the route table, so a board gaining the tier
/// appears here without this function being touched.
fn class_library_boards() -> String {
    let mut listed: Vec<&str> = lamella_flash_routes::PROGRAMMING
        .iter()
        .filter(|row| {
            row.aot_target
                .is_some_and(crate::tiers::covers)
        })
        .map(|row| row.board)
        .collect();
    listed.sort_unstable();
    if listed.is_empty() {
        return "    (none in this build)".to_owned();
    }
    listed
        .iter()
        .map(|board| format!("    {board}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Whether `error` is the flat tier declining a call it cannot resolve -- the one failure the
/// class-library tier exists to answer.
///
#[cfg(feature = "aot-arm32")]
fn is_unresolvable_call(error: &lamella_aot::build::BuildError) -> bool {
    matches!(
        error,
        lamella_aot::build::BuildError::LowerArm(lamella_aot::arm32::LowerError::CallUnsupported)
    )
}

/// Without the Thumb generator there is no such variant to match: the linked tier is Cortex-M only,
/// so a build that cannot generate Thumb has nothing to offer here either way.
#[cfg(not(feature = "aot-arm32"))]
fn is_unresolvable_call(_error: &lamella_aot::build::BuildError) -> bool {
    false
}

/// Why `--class-library` cannot be honored, naming what is missing rather than building something
/// else.
///
/// **IT NEVER FALLS BACK TO THE FLAT TIER.** The two tiers accept different programs and produce
/// images of very different sizes, so a silent downgrade would answer a request for the class
/// library with an image that has none -- and the discovery would be a call that refuses to lower,
/// or a board that does nothing, long after the build said it had succeeded.
fn class_library_refusal(verb: &str, aot_target: &str) -> String {
    if !crate::tiers::covers(aot_target) {
        return format!(
            "lamella {verb}: --class-library has no image plan for {aot_target}.\n\n\
             The class-library tier lays out a part's RAM explicitly -- where the statics window \
             sits, where the\nheap starts and where it must stop -- so it covers a part only once \
             those addresses are stated.\n\n\
             Boards it covers today:\n{}\n\n\
             Build this target without --class-library to use the flat tier, whose limits are \
             stated when a\nprogram exceeds them.",
            class_library_boards()
        );
    }
    format!(
        "lamella {verb}: --class-library was asked for and this build cannot supply it.\n\n\
         The class-library tier links your program against corlib and the runtime support archive, \
         and this\nbinary was compiled without it.\n\n\
         Nothing was built. The flat tier would accept a narrower program and produce a \
         different\nimage, so it is not substituted for what you asked for."
    )
}


/// Why the tier flag cannot be honored on a route that builds no image.
///
/// **AN OPTION ACCEPTED AND THEN DROPPED IS THIS CRATE'S OLDEST DEFECT SHAPE.** Its own parser says
/// so: a `--target` quietly ignored looks exactly like a board that did not respond. This flag
/// chooses what a program is LINKED against, and two of the routes that take it link nothing -- so
/// somebody passing it there believes they have the class-library tier and does not have it.
///
/// **IT NAMES THE ROUTE THAT WORKS.** A refusal that only declines reproduces the defect one level
/// up: the reader still does not learn where the flag applies.
pub fn tier_flag_where_nothing_links(
    verb: &str,
    route: &str,
    instead: &str,
    outcome: &str,
) -> String {
    format!(
        "lamella {verb}: {CLASS_LIBRARY_FLAG} chooses what a program is LINKED against, and \
         nothing is linked\non this route.\n\n{route}\n\n{instead}\n\n{outcome}"
    )
}

/// Whether `path` names a project file rather than a source file.
///
/// **THE EXTENSION, BECAUSE THAT IS WHAT EVERY OTHER TOOL USES.** `dotnet build` decides the same
/// way, and a reader who renamed a project to something else has already left the convention every
/// editor and build server depends on.
pub fn is_project(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("csproj"))
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













/// The option that writes an image over the bootloader a board's product ships in flash, rather
/// than behind it.
const REPLACE_BOOTLOADER: &str = "--replace-bootloader";

const USAGE: &str = "\
usage: lamella flash <image> [--board <id>] [--via probe|volume]
                          [--probe <serial>]      which probe, on a probe route
                          [--volume <name>]       which drive, on a volume route
                          [--device <serial>]     which bootloader, on a USB DFU route
                          [--replace-bootloader]  over the board's bootloader, not behind it
       lamella flash --board <id> --clear-bootprot [--dry-run] [--probe <serial>]
       lamella flash --board <id> --restore-user-row <file> [--dry-run] [--probe <serial>]

Writes an image that ALREADY EXISTS to the board's chip, over its debug probe. It does not compile
anything -- `lamella build <file> --board <id> --format <f>` produces what this takes, and
`lamella deploy` is the two steps in one. The board needs nothing on it first.

The image is read by extension: .hex, .bin, .s19, .elf, or the .uf2 a bootloader-volume board
takes. A linked .elf is flattened the way `objcopy -O binary` would, by physical address, so an
image another toolchain produced needs no conversion step. For a bootloader-volume board, any of
them but a .uf2 is wrapped into one here, because the address and the family id a .uf2 carries are
facts about the board and are already known. An image larger than the board's flash is refused
before anything is opened.

--probe names WHICH probe when more than one is attached; --volume names which drive; --device
names which system bootloader, over USB DFU. They are different questions and each belongs to one
route, so naming the wrong one is refused rather than ignored. Without one, a probe route takes
LAMELLA_PROBE_SERIAL and then the sole attached probe, the other routes take the sole candidate,
and otherwise the write is REFUSED with every candidate named. A write to the wrong board succeeds
and reports nothing, so it is never guessed at.

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

On a board whose product ships a bootloader in flash, an Arduino Zero among them, the image is
written BEHIND the bootloader, which stays and starts it, so an IDE can still upload through it.
The write first reads the start of flash and is refused when no bootloader is there, before
anything is erased. --replace-bootloader writes the image at the start of flash instead, over the
bootloader, and the board then has none until one is written back. The way back is the same
option with the bootloader's own file; none is included here. An image linked for the other
layout is refused, naming the option that writes it where it was linked to run.

--clear-bootprot writes no image. On a SAM D21 board it clears the part's bootloader protection,
the BOOTPROT field of its NVM user row, which a write at address 0 needs. It prints every field of
the row as it is and as it will be, then saves the row to a file in the working directory, named
for the part's serial number. It erases the row, writes it back with only BOOTPROT changed, reads
it back and compares it, and resets the part so that the new value is in force. With --dry-run it
prints the same table and writes nothing.

--restore-user-row <file> is the way back: it writes a saved row back over the part's row, and it
takes only a file saved from the same part. A written row takes effect only at the part's next
reset, so a rewrite that is interrupted leaves the part running on its previous configuration
until then; put the row back before anything resets the board. One case is out of reach: a part
that has already reset with an erased row can hold itself in brown-out reset, and the plain attach
these steps make does not reach a part held there.
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
    /// **THE INVARIANT THAT REPLACED A SECOND PREDICATE.** The wording tests above are only as
    /// honest as the verification handed to them, so this asserts the thing that actually decides
    /// it: the volume mechanism declares its own impossibility by answering `None`, which is what
    /// makes a "verified" sentence unreachable on that route rather than merely unwritten.
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
    /// which a silent fallback would write the wrong board.
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

    /// A write through a system bootloader's DFU interface is read back, and its start is only asked
    /// for: leaving DFU mode ends with the bootloader gone and nothing reporting the jump.
    #[test]
    fn a_dfu_write_says_the_start_was_asked_for_and_not_seen() {
        let dfu = Programmer::StDfu {
            family: lamella_flash_routes::StFamily::H7,
        };
        let line = completion_line(
            dfu,
            &report(2048, lamella_flash_backend::Verification::ReadBack),
        );
        assert!(
            line.contains("verified"),
            "the bootloader read every byte back: {line}"
        );
        assert!(
            !line.contains("is running it"),
            "a leave does not show a start: {line}"
        );
        assert!(
            line.contains("reset the board"),
            "and says what to do when nothing runs: {line}"
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
            "that sentence belongs to a route that CANNOT read back, not one that was told not to: {line}"
        );
    }

    /// Whether the C# compiler and its reference assemblies are present in this checkout.
    ///
    /// **THE ABSENCE THIS COVERS IS A NAMED ONE, AND NAMING IT IS WHAT MAKES A SKIP LEGITIMATE.**
    /// `lcsc` cannot compile anything without reference assemblies. A development checkout has
    /// them; a stripped copy of the tree does not, and there a case that compiles C# would be
    /// measuring the checkout rather than the code. Skipping on that is the answer to a question
    /// somebody asked.
    fn csharp_compiler_is_available() -> bool {
        lamella_wire_host::engine::LcscCompiler::discover().is_ok()
    }

    #[test]
    fn every_programmable_board_builds_a_bootable_image_from_a_blink_program() {
        if !csharp_compiler_is_available() {
            return;
        }
        let path = Path::new("Blink.cs");
        let mut built = 0;
        for row in PROGRAMMING {
            let Some(target) = row.aot_target else {
                let refusal =
                    cannot_build_for(row.board, "deploy", &NoAotTarget::WritableButNotBuildable);
                assert!(refusal.contains(row.board), "{}: {refusal}", row.board);
                assert!(refusal.contains("lamella flash"), "{}: {refusal}", row.board);
                assert!(
                    refusal.starts_with("lamella deploy:"),
                    "{}: {refusal}",
                    row.board
                );
                assert!(
                    cannot_build_for(row.board, "build", &NoAotTarget::WritableButNotBuildable)
                        .starts_with("lamella build:"),
                    "{}: the build path must answer as build",
                    row.board
                );
                assert!(
                    image_for_board(path, BLINK, row.board, true, Tier::Flat, &[]).is_err(),
                    "{}: names no target, so building for it must refuse",
                    row.board
                );
                continue;
            };
            built += 1;
            let image = build_image(path, BLINK, target, true, Tier::Flat, &[], "deploy")
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

    /// The refusal offers `lamella flash` only where flashing that board is possible.
    ///
    /// **A SUGGESTION THAT FAILS IS WORSE THAN NO SUGGESTION.** This one is false wherever nothing
    /// states how to write the board: the catalog carries it, and the verb being recommended
    /// refuses it too, so a reader spends a round trip finding that out.
    ///
    /// ONE BOARD, TWO REASONS, so the suggestion is the only thing that differs. Giving each state
    /// its own board id would leave the comparison confounded by the name in the text.
    #[test]
    fn a_refusal_offers_flashing_only_where_this_tree_can_flash_the_board() {
        let board = "st-nucleo-l053r8";
        let writable = cannot_build_for(board, "build", &NoAotTarget::WritableButNotBuildable);
        let neither = cannot_build_for(board, "build", &NoAotTarget::NeitherBuildableNorWritable);

        assert!(
            writable.contains("lamella flash <image>"),
            "a writable board is told the verb that works: {writable}"
        );
        assert!(
            !neither.contains("lamella flash <image>"),
            "a board nothing can write is NOT sent to flash: {neither}"
        );
        assert!(
            neither.contains("not a way around it"),
            "and is told why, rather than left with a gap: {neither}"
        );
        for message in [&writable, &neither] {
            assert!(
                message.starts_with("lamella build:"),
                "the verb is the one that was run: {message}"
            );
        }
    }

    #[test]
    fn the_cannot_build_message_renders_without_stray_columns() {
        let message =
            cannot_build_for("st-nucleo-l053r8", "deploy", &NoAotTarget::WritableButNotBuildable);
        crate::rendered::assert_renders_cleanly(&message, |line| {
            line.starts_with("    lamella ")
        });
        assert!(message.contains("st-nucleo-l053r8"), "{message}");
        assert!(message.contains("lamella flash"), "it names the verb that works:
{message}");
    }

    #[test]
    fn the_tool_carries_a_code_generator_for_every_target_it_can_be_handed() {
        if !csharp_compiler_is_available() {
            return;
        }
        let path = Path::new("Blink.cs");
        for target in ["microbit", "ch32v003"] {
            let image = build_image(path, BLINK, target, true, Tier::Flat, &[], "deploy")
                .unwrap_or_else(|error| {
                    panic!("{target}: this build cannot compile for it: {error}")
                });
            assert!(
                image.len() > 64,
                "{target}: {} B is too small to be a program",
                image.len()
            );
        }
    }

    const CONSOLE: &str = "\
class Program
{
    static int Main()
    {
        System.Console.Write(\"x\");
        return 0;
    }
}
";

    /// **THE REFUSAL THREE SEPARATE PARTIES READ AS \"C# ON A BOARD CAN DO NOTHING\".** The flat
    /// tier resolves no call outside the program and said so plainly, and a limit stated with no
    /// way out of it reads as a limit of the project rather than of one tier.
    #[test]
    fn a_call_the_flat_tier_cannot_resolve_names_the_tier_that_can() {
        if !csharp_compiler_is_available() {
            return;
        }
        let error = build_image(
            Path::new("Hello.cs"),
            CONSOLE,
            "microbit",
            false,
            Tier::Flat,
            &[],
            "build",
        )
        .expect_err("the flat tier resolves no call outside the program");
        assert!(
            error.contains(CLASS_LIBRARY_FLAG),
            "it names the way out: {error}"
        );
        assert!(
            error.contains("lamella build"),
            "and the verb that was run: {error}"
        );
    }

    /// **A TARGET WITH NO LINKED PLAN MUST NOT BE SENT TO THE FLAG.** `--class-library` covers two
    /// targets against a board table of dozens, so naming it on every refusal would hand a reader
    /// holding a RISC-V part a flag that refuses them -- which is worse than the limit it replaced,
    /// because it costs them a build to find out.
    #[test]
    fn a_target_with_no_linked_plan_is_not_sent_to_a_flag_that_would_refuse_it() {
        if !csharp_compiler_is_available() {
            return;
        }
        let Err(error) = build_image(
            Path::new("Hello.cs"),
            CONSOLE,
            "ch32v003",
            false,
            Tier::Flat,
            &[],
            "build",
        ) else {
            return;
        };
        assert!(
            !error.contains(CLASS_LIBRARY_FLAG),
            "ch32v003 has no linked plan, so the refusal must not name the flag: {error}"
        );
    }

    /// **ASKED FOR AND UNAVAILABLE IS A REFUSAL, NEVER THE OTHER TIER.** The two tiers accept
    /// different programs and produce images of very different sizes, so substituting one for the
    /// other would answer a request for the class library with an image that has none -- and the
    /// discovery would come at a board.
    ///
    /// **BOTH OUTCOMES ARE ASSERTED, BECAUSE WHICH ONE HAPPENS IS A PROPERTY OF THE MACHINE.**
    /// Whether a runtime-support archive is discoverable decides whether the class-library request
    /// can be satisfied at all, so a case asserting only one of the two describes the tier decision
    /// on some machines and nothing at all on the rest.
    #[test]
    fn asking_for_the_class_library_refuses_by_name_rather_than_falling_back() {
        if !csharp_compiler_is_available() {
            return;
        }
        let flat = build_image(
            Path::new("Blink.cs"),
            BLINK,
            "microbit",
            true,
            Tier::Flat,
            &[],
            "deploy",
        )
        .expect("BLINK lowers on the flat tier");
        match build_image(
            Path::new("Blink.cs"),
            BLINK,
            "microbit",
            true,
            Tier::ClassLibrary,
            &[],
            "deploy",
        ) {
            Err(error) => {
                assert!(
                    error.contains(CLASS_LIBRARY_FLAG),
                    "it names what was asked for: {error}"
                );
                assert!(
                    error.contains("lamella deploy"),
                    "and the verb that was run: {error}"
                );
            }
            Ok(linked) => assert_ne!(
                linked, flat,
                "a class-library request is answered with a linked image or not at all"
            ),
        }
    }

    /// The reach of the flag is stated where somebody meets its edge, not only in the help.
    #[test]
    fn a_target_the_linked_tier_has_no_plan_for_is_refused_with_the_ones_it_does() {
        let error = class_library_refusal("build", "ch32v003");
        assert!(
            error.contains("ch32v003"),
            "it names the target asked for: {error}"
        );
        let boards = class_library_boards();
        assert!(
            boards.contains("micro-bit"),
            "the table names a micro:bit board: {boards}"
        );
        for board in boards.lines() {
            assert!(
                error.contains(board.trim()),
                "and the refusal lists it: {board}
{error}"
            );
        }
    }

    /// **THE VERB IN A MESSAGE IS THE VERB THAT WAS TYPED.** Both verbs share this path, and every
    /// message on it said `lamella deploy` -- so `lamella build --format` refused in the name of a
    /// command with a different command line, sending the reader to the wrong help.
    #[test]
    fn a_refusal_names_the_verb_that_was_run_rather_than_its_sibling() {
        if !csharp_compiler_is_available() {
            return;
        }
        let library = "public sealed class Blink { public static unsafe void Run() { } }";
        let error = build_image(
            Path::new("Blink.cs"),
            library,
            "microbit",
            true,
            Tier::Flat,
            &[],
            "build",
        )
        .expect_err("a library is not a program");
        assert!(
            error.contains("lamella build"),
            "it names the verb run: {error}"
        );
        assert!(
            !error.contains("lamella deploy"),
            "and not its sibling: {error}"
        );
    }

    #[test]
    fn the_tier_messages_render_without_stray_columns() {
        let mut rendered = vec![
            class_library_refusal("build", "ch32v003"),
            class_library_refusal("deploy", "microbit"),
        ];
        if lamella_wire_host::engine::LcscCompiler::discover().is_ok() {
            rendered.push(
                build_image(
                    Path::new("Hello.cs"),
                    CONSOLE,
                    "microbit",
                    false,
                    Tier::Flat,
                    &[],
                    "build",
                )
                .expect_err("the flat tier refuses this call"),
            );
        }
        for message in &rendered {
            crate::rendered::assert_renders_cleanly(message, crate::rendered::four_space_sample);
            for line in message.lines() {
                assert!(
                    line.len() <= 100,
                    "a line runs to {}:\n{message}",
                    line.len()
                );
            }
        }
    }

    /// Every build states its tier, and the two never read the same.
    #[test]
    fn each_tier_states_itself_and_they_are_distinguishable() {
        let flat = Tier::Flat.line();
        let linked = Tier::ClassLibrary.line();
        assert_ne!(flat, linked);
        assert!(flat.contains("flat"), "{flat}");
        assert!(linked.contains("class library"), "{linked}");
    }

    #[test]
    fn a_program_with_no_entry_is_refused_by_name() {
        if !csharp_compiler_is_available() {
            return;
        }
        let library = "public sealed class Blink { public static unsafe void Run() { } }";
        let error = build_image(
            Path::new("Blink.cs"),
            library,
            "microbit",
            true,
            Tier::Flat,
            &[],
            "deploy",
        )
        .expect_err("a library is not a program");
        assert!(
            error.contains("no static Main"),
            "it names the contract: {error}"
        );
        assert!(
            error.contains("Run()"),
            "and the shape it is telling apart: {error}"
        );
    }

    #[test]
    fn the_refusal_names_the_missing_fact_and_what_still_works() {
        let text = cannot_write("rpi-pico2");
        assert!(text.contains("bbc-micro-bit-v1"), "it lists what CAN be written: {text}");
        assert!(text.contains("not yet stated in any board file"), "and why the list is short");
        assert!(text.contains("lamella build"), "and what still works for that board");
        assert!(text.contains("rpi-pico2"), "and names the board asked for");
        assert!(
            !text.contains("this build"),
            "no build writes a board another refuses, so the refusal must not imply one does: {text}"
        );
    }

    #[test]
    fn the_coverage_column_agrees_with_the_table() {
        assert!(can_flash("bbc-micro-bit-v1"));
        assert!(can_flash("rpi-pico2"));
        assert!(!can_flash("st-nucleo-f429zi"), "no mechanism is stated for the ST boards yet");
        assert!(!can_flash("no-such-board"));
    }

    #[test]
    fn ambiguous_hardware_with_no_terminal_refuses_and_names_the_candidates() {
        let candidates =
            vec!["SERIAL0000000002".to_owned(), "SERIAL0000000003".to_owned()];
        let error = ask(&candidates).expect_err("no terminal in a test process");
        assert!(error.contains("SERIAL0000000002"), "it names every candidate: {error}");
        assert!(error.contains("SERIAL0000000003"), "both of them: {error}");
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

    /// **THE ORDER DECLARED IS THE ORDER LINKED, AND A REPEAT IS NOT A SLIP.** Precedence across
    /// the reference set is first-declarer-wins, so a reader that sorted these, or kept only the
    /// first, would change which definition of a name the program links against -- with no symptom
    /// at build time.
    #[test]
    fn references_keep_the_order_the_project_declared_them_in() {
        let error = libraries_from(&["A.dll", "B.dll", "C.dll"], Tier::Flat, "deploy")
            .expect_err("the flat tier links nothing");
        let a = error.find("A.dll").expect("A is listed");
        let b = error.find("B.dll").expect("B is listed");
        let c = error.find("C.dll").expect("C is listed");
        assert!(a < b && b < c, "in the order declared: {error}");
    }

    /// **A LIBRARY DECLARED AT THE FLAT TIER CANNOT TAKE EFFECT, SO IT IS REFUSED RATHER THAN
    /// DROPPED.** An input accepted and then discarded is this crate's oldest defect shape, and
    /// the refusal has to carry the way out -- which is the flag, not the removal of the library.
    #[test]
    fn a_reference_without_the_tier_is_refused_and_names_both() {
        let error = libraries_from(&["Gpio.dll"], Tier::Flat, "deploy")
            .expect_err("the flat tier links nothing");
        assert!(error.contains(CLASS_LIBRARY_FLAG), "names the flag: {error}");
        assert!(error.contains("Gpio.dll"), "and the library: {error}");
        assert!(
            error.contains("Nothing was built"),
            "and says nothing happened: {error}"
        );
    }

    /// **A PATH THAT CANNOT BE READ NAMES ITSELF.** It came out of a project file the reader
    /// wrote, and an error carrying only an assembly's internal name asks them to work out which
    /// `<Reference>` produced it.
    #[test]
    fn an_unreadable_library_names_the_path_the_project_gave() {
        let error = libraries_from(
            &["no-such-directory/Absent.dll"],
            Tier::ClassLibrary,
            "deploy",
        )
        .expect_err("the file is not there");
        assert!(error.contains("Absent.dll"), "names the path: {error}");
        assert!(
            error.contains("<Reference>"),
            "and the element it came from: {error}"
        );
    }

    /// **A REFUSAL PRINTS AS PROSE, NOT AS ITS SOURCE'S INDENTATION.** A line break written into a
    /// literal as a raw newline carries the next source line's indentation into the output, so the
    /// continuation prints at the column of the code around it. No line may be indented further than
    /// the four columns a listed path takes, and no sentence may hold a run of spaces.
    #[test]
    fn the_reference_refusals_print_as_prose_without_source_columns() {
        let refusals = [
            reference_without_the_tier("build", &["Bsp.dll", "Gpio.dll"]),
            libraries_from(&["no/such/library.dll"], Tier::ClassLibrary, "build")
                .expect_err("there is no such file"),
        ];
        for refusal in &refusals {
            for line in refusal.lines() {
                let indent = line.len() - line.trim_start().len();
                assert!(indent <= 4, "a line indented {indent} columns:\n{refusal}");
                assert!(!line.trim().contains("   "), "a run of spaces inside a sentence:\n{refusal}");
            }
        }
    }

    /// **NO REFERENCES MEANS NO CHANGE**, which is what keeps every existing command line working.
    #[test]
    fn a_build_with_no_references_reads_back_empty() {
        assert!(libraries_from(&[], Tier::ClassLibrary, "deploy")
            .expect("nothing to read")
            .is_empty());
        assert!(libraries_from(&[], Tier::Flat, "deploy")
            .expect("and the flat tier is untouched")
            .is_empty());
    }

    /// **A LIBRARY THAT IS NOT AN ASSEMBLY IS REFUSED BY THE VERB THAT WAS RUN**, on both ways a
    /// single file is compiled. Compiling for a debugger is the one a library reaches, because every
    /// class-library image is compiled that way, `deploy`'s included; the plain compile shares the
    /// refusal. Each is asked with a different verb, so neither can answer with a fixed one. This
    /// refusal opened `lamella:` with no verb, the one message on this path that did not say which
    /// command it came from.
    #[test]
    fn a_library_that_is_not_an_assembly_is_refused_by_the_verb_that_was_run() {
        if !csharp_compiler_is_available() {
            return;
        }
        let broken = [Library {
            path: PathBuf::from("Broken.dll"),
            bytes: vec![1, 2, 3],
        }];
        let program = "class P\n{\n    static void Main() { }\n}\n";
        let Err(debug) = crate::program::compile_csharp_for_debugging(
            Path::new("P.cs"),
            program,
            false,
            &broken,
            "deploy",
        ) else {
            panic!("three bytes are not an assembly");
        };
        let Err(plain) = crate::program::compile_csharp_assembly_with_corlib(
            Path::new("P.cs"),
            program,
            false,
            &broken,
            "build",
        ) else {
            panic!("three bytes are not an assembly, compiled plainly either");
        };
        for (refusal, verb) in [(debug, "deploy"), (plain, "build")] {
            assert!(
                refusal.starts_with(&format!(
                    "lamella {verb}: Broken.dll is not a readable .NET assembly"
                )),
                "{refusal}"
            );
            crate::rendered::assert_renders_cleanly(&refusal, crate::rendered::four_space_sample);
        }
    }

    /// **THE LIBRARY SET REACHES THE LINKED BUILD IN THE ORDER IT WAS DECLARED, AND WHOLE.**
    /// Precedence across a reference set is first-declarer-wins, so sorting or de-duplicating here
    /// would change which definition of a duplicated name the program binds -- with a successful
    /// build to hide it. A duplicated file name with different contents is the case that tells a
    /// de-duplicating mapping from a faithful one, so it is the case this uses.
    ///
    /// This replaces a test that pinned a refusal: until `build_linked_cortex_m_with_libraries`
    /// merged, a named library could not be linked and was refused by name rather than dropped.
    /// The capability exists now, so the refusal is gone and what needs holding is the hand-off.
    #[test]
    fn the_library_set_reaches_the_build_in_declaration_order() {
        let libraries = vec![
            Library { path: PathBuf::from("Bsp.dll"), bytes: vec![2] },
            Library { path: PathBuf::from("Gpio.dll"), bytes: vec![1] },
            Library { path: PathBuf::from("Bsp.dll"), bytes: vec![3] },
        ];
        assert_eq!(
            library_bytes(&libraries),
            vec![&[2u8][..], &[1u8][..], &[3u8][..]],
            "declaration order, nothing sorted and nothing dropped"
        );
    }

    /// `--replace-bootloader` is refused beside either user-row step, which writes no image for
    /// the option to place.
    #[test]
    fn replace_bootloader_is_refused_beside_either_user_row_step() {
        use crate::bootprot::{CLEAR_BOOTPROT, RESTORE_USER_ROW};
        assert_eq!(user_row_step_beside_a_replace(true, true, false), Some(CLEAR_BOOTPROT));
        assert_eq!(user_row_step_beside_a_replace(true, false, true), Some(RESTORE_USER_ROW));
        assert_eq!(user_row_step_beside_a_replace(true, false, false), None, "an image write");
        assert_eq!(
            user_row_step_beside_a_replace(false, true, true),
            None,
            "without the option, the two steps are for their own refusal to settle"
        );
    }

    /// Before a write, a kept bootloader is named with where the image goes, and a replaced one
    /// with what the board loses and the way back. A board with no bootloader is told nothing.
    #[test]
    fn a_write_says_what_it_does_with_the_bootloader() {
        let zero = programmer_for("arduino-zero").expect("the Zero is routed");
        let keep = placement_for(zero, zero.programmer, BootloaderChoice::Keep).expect("kept");
        let line = placement_line(&keep).expect("a line");
        assert!(
            line.contains("keeping the Arduino Zero Bootloader at 0x00000000-0x00001fff"),
            "{line}"
        );
        assert!(line.contains("from 0x00002000"), "{line}");
        let over = placement_for(zero, zero.programmer, BootloaderChoice::Replace).expect("over");
        let line = placement_line(&over).expect("a line");
        assert!(line.contains("replacing the Arduino Zero Bootloader"), "{line}");
        assert!(line.contains("an IDE over USB") && line.contains("double-tap"), "lost: {line}");
        assert!(
            line.contains("--replace-bootloader with the bootloader's own file"),
            "the way back: {line}"
        );
        let xpro = programmer_for("microchip-samd21-xpro").expect("the XPro is routed");
        let start = placement_for(xpro, xpro.programmer, BootloaderChoice::Keep).expect("placed");
        assert_eq!(placement_line(&start), None);
    }

    /// `deploy --board` links an image to run where its route writes, so a board that keeps its
    /// bootloader there is refused rather than written behind the bootloader or over it.
    #[test]
    fn deploy_refuses_a_board_that_keeps_its_bootloader() {
        let zero = programmer_for("arduino-zero").expect("the Zero is routed");
        let why = deploy_placement(zero, zero.programmer).expect_err("it keeps its bootloader");
        assert!(why.contains("Arduino Zero Bootloader"), "{why}");
        assert!(why.contains("Nothing was written"), "{why}");
        let xpro = programmer_for("microchip-samd21-xpro").expect("the XPro is routed");
        assert_eq!(
            deploy_placement(xpro, xpro.programmer),
            Ok(Placement::Start { base: xpro.programmer.flash_base() })
        );
    }

    /// The usage says a board's bootloader is kept by default, names the option that replaces it,
    /// and says that no bootloader is included.
    #[test]
    fn the_usage_says_a_bootloader_is_kept_unless_replaced() {
        assert!(USAGE.contains(REPLACE_BOOTLOADER), "{USAGE}");
        assert!(USAGE.contains("written BEHIND the bootloader"), "{USAGE}");
        assert!(USAGE.contains("none is included here"), "{USAGE}");
    }
}
