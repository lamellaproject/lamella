//! Where the class-library tier's runtime-support archive comes from, and which targets have one.

use std::path::{Path, PathBuf};

/// A target the class-library tier can build for, and the Rust target triple whose runtime-support
/// archive it links against.
struct LinkedTarget {
    /// The ahead-of-time target name, as a board's fact row gives it.
    aot_target: &'static str,
    /// The triple the archive beside it was compiled for.
    triple: &'static str,
    /// Whether the archive carries the board's own half -- the startup its clock plan names, its
    /// console and its millisecond clock -- so that each board needs an archive of its own.
    ///
    /// **A BOARD BUILD IS FOUND BY THE BOARD, NEVER BY THE TRIPLE ALONE.** An archive built for
    /// the triple without a board starts no clock and writes its console through a debugger, so on
    /// a board with nothing attached the image runs at the clock it was reset into and faults at
    /// its first `Console.Write` -- an image that links, writes and says nothing about why.
    per_board: bool,
}

/// Every target the class-library tier has a plan for, with the archive each one needs.
///
/// **ONE TABLE, SO THE REACH AND THE INGREDIENT CANNOT DISAGREE.** A target the tier claims to cover
/// but has no archive named for would be accepted at the flag and refused at the link, which is the
/// worst place to discover it -- after a compile, with nothing to act on.
const LINKED_TARGETS: &[LinkedTarget] = &[
    LinkedTarget {
        aot_target: "microbit",
        triple: "thumbv6m-none-eabi",
        per_board: false,
    },
    LinkedTarget {
        aot_target: "nrf52833",
        triple: "thumbv6m-none-eabi",
        per_board: false,
    },
    LinkedTarget {
        aot_target: "rp2350",
        triple: "thumbv8m.main-none-eabi",
        per_board: true,
    },
];

/// The archive file name, which is the staticlib's own and is the same for every ARM triple.
const ARCHIVE_NAME: &str = "liblamella_runtime_support.a";

/// The environment variable naming an archive explicitly.
pub const ARCHIVE_ENV: &str = "LAMELLA_RUNTIME_ARCHIVE";

/// Cargo's own variable for where a build writes, honored so a development tree that redirects its
/// output is still a tree where this can find what it built.
const CARGO_TARGET_DIR_ENV: &str = "CARGO_TARGET_DIR";

/// Whether the class-library tier has an image plan for `aot_target`.
#[must_use]
pub fn covers(aot_target: &str) -> bool {
    LINKED_TARGETS
        .iter()
        .any(|row| row.aot_target == aot_target)
}

/// Every `aot_target` the tier covers, for a refusal that has to list them.
pub fn targets() -> impl Iterator<Item = &'static str> {
    LINKED_TARGETS.iter().map(|row| row.aot_target)
}

/// The triple whose archive `aot_target` links against.
#[cfg(test)]
fn triple_for(aot_target: &str) -> Option<&'static str> {
    row_for(aot_target).map(|row| row.triple)
}

/// The table's row for `aot_target`.
fn row_for(aot_target: &str) -> Option<&'static LinkedTarget> {
    LINKED_TARGETS
        .iter()
        .find(|row| row.aot_target == aot_target)
}

/// The runtime-support feature that compiles `board`'s half into an archive.
///
/// The feature is named after the board id, as `lamella boards` lists it, so the name a reader
/// typed after `--board` is the name they pass to cargo.
fn board_feature(board: &str) -> String {
    format!("board-{board}")
}

/// The machine an archive for `aot_target` must have been built for.
fn machine_for(triple: &str) -> Option<lamella_elf::Machine> {
    if triple.starts_with("thumb") || triple.starts_with("arm") {
        Some(lamella_elf::Machine::Arm)
    } else if triple.starts_with("riscv") {
        Some(lamella_elf::Machine::RiscV)
    } else {
        None
    }
}

/// The runtime-support archive for `aot_target`: where it was found, and its bytes.
///
/// **IT NEVER FALLS BACK, AND IT NEVER FALLS THROUGH AN EXPLICIT OVERRIDE.** A missing archive is a
/// refusal naming every place that was looked in, because the person reading it is the one who has
/// to put the file somewhere. And an override that a default can silence is not an override: with
/// [`ARCHIVE_ENV`] set, the named file is the answer or there is no answer, so a typo in it is
/// reported rather than quietly replaced by a different archive that happens to be installed.
///
/// `board` is the `--board` id the image is for. It decides which archive is looked for only on a
/// target whose archive carries the board's own half; elsewhere one archive serves every board.
///
/// # Errors
/// No archive found, a file that is not an `ar` archive, or one built for another instruction set.
/// Each names the path it is talking about.
pub fn runtime_archive(board: &str, aot_target: &str) -> Result<(PathBuf, Vec<u8>), String> {
    named_runtime_archive(board, aot_target, std::env::var_os(ARCHIVE_ENV).map(PathBuf::from))
}

/// [`runtime_archive`] with the override handed in rather than read from the environment.
///
/// **THE ENVIRONMENT IS READ IN ONE PLACE AND THE RULE IS TESTED IN ANOTHER.** A process-wide
/// variable is shared by every test in the binary at once, so a test that set one would be deciding
/// what its neighbors see; taking the override as an argument makes each case an ordinary call.
///
/// # Errors
/// As [`runtime_archive`].
fn named_runtime_archive(
    board: &str,
    aot_target: &str,
    named: Option<PathBuf>,
) -> Result<(PathBuf, Vec<u8>), String> {
    let Some(row) = row_for(aot_target) else {
        return Err(format!(
            "{aot_target} has no runtime support archive because the class-library tier has no \
             plan for it"
        ));
    };
    let triple = row.triple;
    if let Some(path) = named {
        let bytes = std::fs::read(&path).map_err(|error| {
            format!(
                "{ARCHIVE_ENV} names {}, which cannot be read: {error}\n\n\
                 Nothing else was looked at. An override that a default can silence is not an \
                 override, so\nthe file named here is the answer or there is none.",
                path.display()
            )
        })?;
        return check(path, bytes, triple);
    }
    let board = row.per_board.then_some(board);
    let looked: Vec<PathBuf> = candidates(
        triple,
        board,
        std::env::var_os(CARGO_TARGET_DIR_ENV)
            .map(PathBuf::from)
            .as_deref(),
    );
    for path in &looked {
        if let Ok(bytes) = std::fs::read(path) {
            return check(path.clone(), bytes, triple);
        }
    }
    Err(no_archive_refusal(aot_target, triple, board, &looked))
}

/// The refusal when no candidate held an archive, naming every place that was looked in.
///
/// **BUILT FROM ITS INPUTS, SO IT CAN BE CHECKED WITHOUT ARRANGING FOR THE FAILURE.** Whether this
/// machine has a discoverable archive decides whether the search can fail at all, and the wording
/// here is what a person with no archive has to act on -- so it must not be verifiable only on
/// machines that cannot build.
///
/// **A BOARD BUILD IS GIVEN AS THE COMMAND THAT MAKES IT.** Its feature, its triple and the
/// directory it must be written to are three facts nobody can guess, and the directory is one this
/// search reads -- so the command below produces exactly the file the next build finds.
///
/// **AND THE CHECKOUT'S DIRECTORY IS REMAPPED AWAY IN IT.** The crates the archive takes from this
/// checkout reach the compiler by absolute path, so without the remap every panic location in the
/// archive names the directory the checkout sits in, and every image linked against it carries
/// that directory onto the board.
fn no_archive_refusal(
    aot_target: &str,
    triple: &str,
    board: Option<&str>,
    looked: &[PathBuf],
) -> String {
    let places = looked
        .iter()
        .map(|path| format!("    {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    let Some(board) = board else {
        return format!(
            "no runtime support archive for {aot_target} ({triple}).\n\n\
             The class-library tier links your program against this archive, so it cannot be built \
             without one.\nLooked in:\n{places}\n\n\
             Name one with {ARCHIVE_ENV}, or build this target with {} for the flat tier.",
            crate::flash::FLAT_FLAG
        );
    };
    format!(
        "no runtime support archive for {board} ({aot_target}, {triple}).\n\n\
         The class-library tier links your program against this archive, so it cannot be built \
         without one.\nOn this target the archive also carries the board's own startup, console \
         and clock, so each\nboard has an archive of its own. Looked in:\n{places}\n\n\
         Build it from this checkout with:\n\
         \x20   cargo build --release --manifest-path {} --target {triple} --no-default-features \
         --features {}{} --target-dir {}\n\n\
         or name one with {ARCHIVE_ENV}.",
        development_manifest().display(),
        board_feature(board),
        remap_option(triple),
        development_board_dir(board).display()
    )
}

/// The cargo option that maps the checkout's directory to nothing in what `triple`'s build
/// compiles, with a space in front of it -- the remap the tree's own archive builder passes.
///
/// Empty for a checkout whose path holds a quote, which the option's TOML string and the shell's
/// quoting could not both carry; the build then works and keeps the directory.
fn remap_option(triple: &str) -> String {
    let root = checkout();
    let root = root.display().to_string();
    if root.contains('\'') || root.contains('"') {
        return String::new();
    }
    format!(" --config \"target.{triple}.rustflags=['--remap-path-prefix={root}=']\"")
}

/// The checkout this crate sits in: two directories above it.
fn checkout() -> PathBuf {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest.ancestors().nth(2).unwrap_or(manifest).to_path_buf()
}

/// The development tree's runtime-support crate: where its sources live, relative to this crate.
///
/// Joined a component at a time from the checkout, two directories above this crate, so the path a
/// refusal prints is one a reader can paste into a shell on any system.
fn development_crate() -> PathBuf {
    checkout().join("tools").join("runtime").join("runtime-support")
}

/// The manifest the board build in [`no_archive_refusal`] names.
fn development_manifest() -> PathBuf {
    development_crate().join("Cargo.toml")
}

/// The directory a board's archive is built into in the development tree: one per board, under the
/// crate's own `target`, so one board's archive never overwrites another's.
fn development_board_dir(board: &str) -> PathBuf {
    development_crate().join("target").join(board)
}

/// Where an archive for `triple` is looked for, in order. `board` is set for a target whose archive
/// carries the board's own half, and then names the board whose archive is wanted.
///
/// Beside the executable first, because that is where a published toolchain puts it and where a
/// person who installed one can be told to look; the development tree last, so a checkout that has
/// built the staticlib works without any setting at all.
fn candidates(triple: &str, board: Option<&str>, cargo_target_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Some(board) = board {
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            paths.push(
                dir.join("runtime-support")
                    .join(triple)
                    .join(board)
                    .join(ARCHIVE_NAME),
            );
        }
        paths.push(
            development_board_dir(board)
                .join(triple)
                .join("release")
                .join(ARCHIVE_NAME),
        );
        return paths;
    }
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        paths.push(dir.join("runtime-support").join(triple).join(ARCHIVE_NAME));
    }
    paths.push(
        development_crate()
            .join("target")
            .join(triple)
            .join("release")
            .join(ARCHIVE_NAME),
    );
    if let Some(target_dir) = cargo_target_dir {
        paths.push(
            target_dir
                .join(triple)
                .join("release")
                .join(ARCHIVE_NAME),
        );
    }
    paths
}

/// Whether `bytes` are an archive, and one for the right instruction set.
///
/// **A MISMATCHED ARCHIVE IS REFUSED HERE RATHER THAN DEEP IN THE LINK.** An archive for another
/// machine parses, links partially, and fails as an unresolved symbol or a relocation the target
/// has no encoding for -- reported against a symbol name, which tells the reader nothing about the
/// file they pointed at. The machine is in the first member's ELF header and costs one read.
fn check(path: PathBuf, bytes: Vec<u8>, triple: &str) -> Result<(PathBuf, Vec<u8>), String> {
    let archive = lamella_elf::read_archive(&bytes).map_err(|error| {
        format!(
            "{} is not a runtime support archive: {error:?}\n\n\
             It should be the `.a` staticlib built from tools/runtime/runtime-support for {triple}.",
            path.display()
        )
    })?;
    let wanted = machine_for(triple);
    if let Some(wanted) = wanted
        && let Some(member) = archive.members.first()
        && member.object.machine != wanted
    {
        return Err(format!(
            "{} was built for {:?} and {triple} needs {wanted:?}.\n\n\
             Nothing was built. Linking it would fail against a symbol name rather than against \
             the file,\nwhich is why this is checked here.",
            path.display(),
            member.object.machine
        ));
    }
    Ok((path, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `ar` archive holding one object for `machine`, which is all the machine check reads.
    fn archive_of(machine: lamella_elf::Machine) -> Vec<u8> {
        let object = lamella_elf::write_relocatable_object(machine, &[0, 0, 0, 0], &[], &[]);
        let mut out = b"!<arch>\n".to_vec();
        let mut header = format!(
            "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}",
            "one.o",
            0,
            0,
            0,
            "644",
            object.len()
        );
        header.push_str("\x60\n");
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&object);
        if out.len() % 2 == 1 {
            out.push(b'\n');
        }
        out
    }

    fn write_temp(name: &str, bytes: &[u8]) -> PathBuf {
        let path = std::env::temp_dir().join(format!("lamella-tiers-{name}"));
        std::fs::write(&path, bytes).expect("write the fixture");
        path
    }

    #[test]
    fn the_table_answers_reach_and_ingredient_together() {
        for target in targets() {
            assert!(covers(target), "{target} is listed, so it is covered");
            assert!(
                triple_for(target).is_some(),
                "{target} names the archive it needs"
            );
        }
        assert!(!covers("ch32v003"), "an unlisted target is not covered");
        assert!(triple_for("ch32v003").is_none());
    }

    /// Every ARM triple in the table wants an ARM archive. The machine is read from the triple so a
    /// RISC-V row added later cannot inherit an ARM default.
    #[test]
    fn a_triple_names_the_machine_its_archive_must_carry() {
        for target in targets() {
            let triple = triple_for(target).expect("a triple");
            assert_eq!(
                machine_for(triple),
                Some(lamella_elf::Machine::Arm),
                "{target}"
            );
        }
        assert_eq!(
            machine_for("riscv32im-unknown-none-elf"),
            Some(lamella_elf::Machine::RiscV)
        );
    }

    /// **A MISSING ARCHIVE NAMES EVERY PLACE IT WAS LOOKED FOR.** The reader is the one who has to
    /// put the file somewhere, and a bare "not found" leaves them guessing where.
    /// **THE ARCHIVE IS LOOKED FOR WHERE THIS TREE ACTUALLY BUILDS IT.** `tools/runtime-support.ps1`
    /// -- the one place twelve gate scripts get their archive from -- passes no `--target-dir`, so
    /// cargo honors `CARGO_TARGET_DIR` and writes under it.
    ///
    /// Every seat here is required to set that variable to a private directory, so following this
    /// project's own build instructions produced an archive no candidate could see, and the linked
    /// tier could not be used without naming the file by hand.
    #[test]
    fn the_archive_is_looked_for_where_a_redirected_build_writes_it() {
        let redirected = Path::new("F:/build/lamella/target-devtools");
        let looked = candidates("thumbv6m-none-eabi", None, Some(redirected));
        assert!(
            looked.contains(
                &redirected
                    .join("thumbv6m-none-eabi")
                    .join("release")
                    .join(ARCHIVE_NAME)
            ),
            "the redirected build output is a candidate: {looked:?}"
        );
    }

    /// **AND A TREE THAT REDIRECTS NOTHING GAINS NO EMPTY PATH.** `Path::join` on an empty base
    /// yields a usable relative path rather than failing, so an absent variable turned into a
    /// candidate would be looked for under the working directory and reported as somewhere we
    /// searched -- the same quiet degradation that cost the project reader its source glob.
    #[test]
    fn a_tree_that_redirects_nothing_gains_no_candidate() {
        assert_eq!(
            candidates("thumbv6m-none-eabi", None, None).len(),
            candidates("thumbv6m-none-eabi", None, Some(Path::new("F:/x"))).len() - 1,
            "the redirected path is the only difference"
        );
    }
    /// **THE REFUSAL IS CHECKED WITHOUT ARRANGING FOR THE FAILURE**, because whether this machine
    /// has a discoverable archive is not something a case about WORDING should depend on.
    #[test]
    fn a_missing_archive_is_refused_with_the_places_it_was_looked_for() {
        let error = no_archive_refusal(
            "microbit",
            "thumbv6m-none-eabi",
            None,
            &candidates("thumbv6m-none-eabi", None, None),
        );
        assert!(
            error.contains("microbit (thumbv6m-none-eabi)"),
            "it names the target and the triple it needs: {error}"
        );
        assert!(
            error.lines().any(|line| line.starts_with("    ")
                && line.contains("runtime-support")
                && line.ends_with(ARCHIVE_NAME)),
            "and lists a place it looked, as a path: {error}"
        );
        assert!(
            error.contains(crate::flash::FLAT_FLAG),
            "and the option that builds without it: {error}"
        );
    }

    /// **AN OVERRIDE THAT A DEFAULT CAN SILENCE IS NOT AN OVERRIDE.** A typo in the variable must be
    /// reported, never replaced by whatever happens to be installed -- which would link a different
    /// archive from the one that was named and say nothing.
    #[test]
    fn a_named_archive_that_cannot_be_read_refuses_rather_than_falling_through() {
        let missing = std::env::temp_dir().join("lamella-tiers-does-not-exist.a");
        let _ = std::fs::remove_file(&missing);
        let error = named_runtime_archive("bbc-micro-bit-v1", "microbit", Some(missing))
            .expect_err("a named archive that is absent is an error, not a hint");
        assert!(
            error.contains(ARCHIVE_ENV),
            "it names the variable: {error}"
        );
        assert!(
            error.contains("Nothing else was looked at"),
            "and that it stopped: {error}"
        );
    }

    #[test]
    fn a_file_that_is_not_an_archive_is_refused_by_name() {
        let path = write_temp("not-an-archive.a", b"MZ this is a dll, not a staticlib");
        let error = named_runtime_archive("bbc-micro-bit-v1", "microbit", Some(path))
            .expect_err("a file that is not an archive cannot be linked");
        assert!(error.contains("not a runtime support archive"), "{error}");
        assert!(
            error.contains("not-an-archive.a"),
            "it names the file: {error}"
        );
    }

    /// **THE WRONG INSTRUCTION SET IS CAUGHT AT THE FILE, NOT AT A SYMBOL.** A RISC-V archive given
    /// to a Thumb target parses and links partially, then fails against a symbol name that tells the
    /// reader nothing about the file they pointed at.
    #[test]
    fn an_archive_for_another_machine_is_refused_naming_both() {
        let path = write_temp("riscv.a", &archive_of(lamella_elf::Machine::RiscV));
        let error = named_runtime_archive("bbc-micro-bit-v1", "microbit", Some(path))
            .expect_err("a RISC-V archive cannot serve a Thumb target");
        assert!(
            error.contains("RiscV"),
            "it names what the file is: {error}"
        );
        assert!(error.contains("Arm"), "and what was needed: {error}");
        assert!(
            error.contains("Nothing was built"),
            "and that it stopped: {error}"
        );
    }

    #[test]
    fn a_matching_archive_is_accepted_and_reports_where_it_came_from() {
        let path = write_temp("arm.a", &archive_of(lamella_elf::Machine::Arm));
        let (found, bytes) = named_runtime_archive("bbc-micro-bit-v1", "microbit", Some(path.clone()))
            .expect("an ARM archive serves a Thumb target");
        assert_eq!(found, path, "it reports the file it used");
        assert!(!bytes.is_empty());
    }

    #[test]
    fn a_target_with_no_plan_has_no_archive_to_look_for() {
        let error = named_runtime_archive("bbc-micro-bit-v1", "ch32v003", None).expect_err("no plan, no archive");
        assert!(error.contains("no plan for it"), "{error}");
    }

    /// **A BOARD BUILD IS LOOKED FOR UNDER ITS BOARD, AND NOWHERE A TRIPLE'S OWN ARCHIVE LIVES.**
    /// The archive at the triple's path is built with no board in it, so finding it would link an
    /// image that starts no clock and faults at its first console write.
    #[test]
    fn a_board_build_is_looked_for_under_its_board_and_never_by_the_triple_alone() {
        let redirected = Path::new("F:/build/lamella/target-devtools");
        let looked = candidates("thumbv8m.main-none-eabi", Some("rpi-pico2"), Some(redirected));
        assert!(!looked.is_empty());
        for path in &looked {
            assert!(
                path.components().any(|part| part.as_os_str() == "rpi-pico2"),
                "every place is the board's own: {}",
                path.display()
            );
            assert!(
                !path.starts_with(redirected),
                "the redirected output holds the triple's archive, not the board's: {}",
                path.display()
            );
        }
        let other = candidates("thumbv8m.main-none-eabi", Some("rpi-pico2-w"), None);
        assert!(
            looked.iter().all(|path| !other.contains(path)),
            "two boards never share a place: {looked:?} and {other:?}"
        );
    }

    /// **THE COMMAND A BOARD's REFUSAL GIVES WRITES THE FILE THE NEXT BUILD FINDS.** The feature,
    /// the triple and the directory are the three facts nobody can guess, and a directory that is
    /// not one of the places looked in would send the reader round the same refusal again.
    #[test]
    fn a_missing_board_archive_is_refused_with_the_command_that_builds_it() {
        let triple = "thumbv8m.main-none-eabi";
        let looked = candidates(triple, Some("pimoroni-pico-plus-2-w"), None);
        let error = no_archive_refusal("rp2350", triple, Some("pimoroni-pico-plus-2-w"), &looked);
        assert!(
            error.contains("pimoroni-pico-plus-2-w (rp2350, thumbv8m.main-none-eabi)"),
            "it names the board, the target and the triple: {error}"
        );
        let command = error
            .lines()
            .find(|line| line.trim_start().starts_with("cargo build"))
            .expect("the refusal gives the command");
        for part in [
            "--release",
            "--target thumbv8m.main-none-eabi",
            "--no-default-features",
            "--features board-pimoroni-pico-plus-2-w",
        ] {
            assert!(command.contains(part), "the command passes {part}: {command}");
        }
        assert!(
            command.contains(&format!(
                "--config \"target.thumbv8m.main-none-eabi.rustflags=['--remap-path-prefix={}=']\"",
                checkout().display()
            )),
            "and keeps the checkout's directory out of the archive: {command}"
        );
        let manifest = development_manifest();
        assert!(
            command.contains(&format!("--manifest-path {}", manifest.display())),
            "and the crate's own manifest: {command}"
        );
        let out = development_board_dir("pimoroni-pico-plus-2-w");
        assert!(
            command.ends_with(&format!("--target-dir {}", out.display())),
            "and the directory: {command}"
        );
        assert!(
            looked.contains(&out.join(triple).join("release").join(ARCHIVE_NAME)),
            "cargo writes the archive where the lookup reads it: {looked:?}"
        );
        assert!(error.contains(ARCHIVE_ENV), "the override is still offered: {error}");
    }

    /// **EVERY BOARD WHOSE TARGET TAKES A BOARD BUILD HAS A FEATURE TO BUILD IT WITH.** The refusal
    /// above names `board-<id>`, a claim about another crate's manifest that would otherwise be
    /// tested only by somebody running the command it prints.
    #[test]
    fn every_board_on_a_per_board_target_has_a_feature_in_the_runtime_archive() {
        let manifest = include_str!("../../../tools/runtime/runtime-support/Cargo.toml");
        let boards: Vec<&str> = lamella_flash_routes::PROGRAMMING
            .iter()
            .filter(|route| {
                route
                    .aot_target
                    .and_then(row_for)
                    .is_some_and(|row| row.per_board)
            })
            .map(|route| route.board)
            .collect();
        assert_eq!(
            boards.len(),
            4,
            "the four RP2350 boards take a board build: {boards:?}"
        );
        for board in boards {
            let feature = board_feature(board);
            assert!(
                manifest
                    .lines()
                    .any(|line| line.trim_start().starts_with(&format!("{feature} = ["))),
                "the runtime-support manifest declares {feature}"
            );
        }
    }

    #[test]
    fn a_board_archive_for_the_right_machine_is_accepted() {
        let path = write_temp("arm-board.a", &archive_of(lamella_elf::Machine::Arm));
        let (found, _) = named_runtime_archive("rpi-pico2", "rp2350", Some(path.clone()))
            .expect("an ARM archive serves the Cortex-M33");
        assert_eq!(found, path, "it reports the file it used");
    }

    /// **THE TABLE IS A CLAIM ABOUT ANOTHER CRATE, AND THIS IS THE ONLY PLACE IT IS CHECKED.**
    /// Every row above is hand-carried from `build_linked_cortex_m`'s own target match, so the two
    /// can drift in either direction and each way is silent: a target dropped there and left here
    /// is accepted at the flag and refused after a compile, with nothing to act on; one added there
    /// and missing here is a board the tier could serve while the tool says it cannot.
    ///
    /// **THE EMPTY INPUTS ARE THE POINT, NOT A SHORTCUT.** The target match is the first thing that
    /// function does, before it reads an assembly or an archive, so a listed target gets past it
    /// and fails later on the empty bytes while an unlisted one never gets that far. That makes
    /// `UnsupportedTarget` precisely the signal under test, with no corlib, no archive, no compile
    /// and no board.
    #[cfg(feature = "class-library")]
    #[test]
    fn every_listed_target_is_one_the_linked_build_has_a_plan_for() {
        use lamella_aot::build::{BuildError, build_linked_cortex_m};
        for target in targets() {
            assert!(
                !matches!(
                    build_linked_cortex_m(&[], &[], &[], target),
                    Err(BuildError::UnsupportedTarget)
                ),
                "{target} is listed here, so the linked build must have a plan for it"
            );
        }
        assert!(
            matches!(
                build_linked_cortex_m(&[], &[], &[], "ch32v003"),
                Err(BuildError::UnsupportedTarget)
            ),
            "and a target the table does not list is refused as unsupported"
        );
    }
}
