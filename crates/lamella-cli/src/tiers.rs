//! Where the class-library tier's runtime-support archive comes from, and which targets have one.

use std::path::{Path, PathBuf};

/// A target the class-library tier can build for, and the Rust target triple whose runtime-support
/// archive it links against.
struct LinkedTarget {
    /// The ahead-of-time target name, as a board's fact row gives it.
    aot_target: &'static str,
    /// The triple the archive beside it was compiled for.
    triple: &'static str,
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
    },
    LinkedTarget {
        aot_target: "nrf52833",
        triple: "thumbv6m-none-eabi",
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
fn triple_for(aot_target: &str) -> Option<&'static str> {
    LINKED_TARGETS
        .iter()
        .find(|row| row.aot_target == aot_target)
        .map(|row| row.triple)
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
/// # Errors
/// No archive found, a file that is not an `ar` archive, or one built for another instruction set.
/// Each names the path it is talking about.
pub fn runtime_archive(aot_target: &str) -> Result<(PathBuf, Vec<u8>), String> {
    named_runtime_archive(aot_target, std::env::var_os(ARCHIVE_ENV).map(PathBuf::from))
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
    aot_target: &str,
    named: Option<PathBuf>,
) -> Result<(PathBuf, Vec<u8>), String> {
    let Some(triple) = triple_for(aot_target) else {
        return Err(format!(
            "{aot_target} has no runtime support archive because the class-library tier has no \
             plan for it"
        ));
    };
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
    let looked: Vec<PathBuf> = candidates(
        triple,
        std::env::var_os(CARGO_TARGET_DIR_ENV)
            .map(PathBuf::from)
            .as_deref(),
    );
    for path in &looked {
        if let Ok(bytes) = std::fs::read(path) {
            return check(path.clone(), bytes, triple);
        }
    }
    Err(no_archive_refusal(aot_target, triple, &looked))
}

/// The refusal when no candidate held an archive, naming every place that was looked in.
///
/// **BUILT FROM ITS INPUTS, SO IT CAN BE CHECKED WITHOUT ARRANGING FOR THE FAILURE.** Whether this
/// machine has a discoverable archive decides whether the search can fail at all, and the wording
/// here is what a person with no archive has to act on -- so it must not be verifiable only on
/// machines that cannot build.
fn no_archive_refusal(aot_target: &str, triple: &str, looked: &[PathBuf]) -> String {
    format!(
        "no runtime support archive for {aot_target} ({triple}).\n\n\
         The class-library tier links your program against this archive, so it cannot be built \
         without one.\nLooked in:\n{}\n\n\
         Name one with {ARCHIVE_ENV}, or build this target without {}.",
        looked
            .iter()
            .map(|path| format!("    {}", path.display()))
            .collect::<Vec<_>>()
            .join("\n"),
        crate::flash::CLASS_LIBRARY_FLAG
    )
}

/// Where an archive for `triple` is looked for, in order.
///
/// Beside the executable first, because that is where a published toolchain puts it and where a
/// person who installed one can be told to look; the development tree last, so a checkout that has
/// built the staticlib works without any setting at all.
fn candidates(triple: &str, cargo_target_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
    {
        paths.push(dir.join("runtime-support").join(triple).join(ARCHIVE_NAME));
    }
    paths.push(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tools/runtime/runtime-support/target")
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
        let looked = candidates("thumbv6m-none-eabi", Some(redirected));
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
            candidates("thumbv6m-none-eabi", None).len(),
            candidates("thumbv6m-none-eabi", Some(Path::new("F:/x"))).len() - 1,
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
            &candidates("thumbv6m-none-eabi", None),
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
            error.contains(crate::flash::CLASS_LIBRARY_FLAG),
            "and the flag: {error}"
        );
    }

    /// **AN OVERRIDE THAT A DEFAULT CAN SILENCE IS NOT AN OVERRIDE.** A typo in the variable must be
    /// reported, never replaced by whatever happens to be installed -- which would link a different
    /// archive from the one that was named and say nothing.
    #[test]
    fn a_named_archive_that_cannot_be_read_refuses_rather_than_falling_through() {
        let missing = std::env::temp_dir().join("lamella-tiers-does-not-exist.a");
        let _ = std::fs::remove_file(&missing);
        let error = named_runtime_archive("microbit", Some(missing))
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
        let error = named_runtime_archive("microbit", Some(path))
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
        let error = named_runtime_archive("microbit", Some(path))
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
        let (found, bytes) = named_runtime_archive("microbit", Some(path.clone()))
            .expect("an ARM archive serves a Thumb target");
        assert_eq!(found, path, "it reports the file it used");
        assert!(!bytes.is_empty());
    }

    #[test]
    fn a_target_with_no_plan_has_no_archive_to_look_for() {
        let error = named_runtime_archive("ch32v003", None).expect_err("no plan, no archive");
        assert!(error.contains("no plan for it"), "{error}");
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
