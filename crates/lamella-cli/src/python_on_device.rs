//! A Python program sent to a board whose Lamella firmware runs Python: `lamella deploy program.py
//! --device` and `lamella run program.py --device` compile it into a bundle for the board that
//! answers, and `lamella deploy program.lpyc --device` sends a bundle `lamella build` already made.
//! The bundle is stored in the board's flash as a C# image is, then started and followed the same way.
//!
//! A board says in its HELLO which runtime it holds, so a Python program is refused by a board that
//! runs C# and a C# program by one that runs Python, before anything is built or sent.

use crate::deploy::AfterDeploy;
use lamella_wire::msg::tier;
use lamella_wire::{Capabilities, Negotiated};
use lamella_wire_host::board_name;
use lamella_wire_host::on_device::{self, the_board};
use std::path::Path;
use std::process::ExitCode;

/// Whether `path` names a Python program: its source, or a bundle `lamella build` made of it.
#[must_use]
pub(crate) fn is_python(path: &Path) -> bool {
    matches!(extension_of(path).as_deref(), Some("py" | "lpyc"))
}

/// Whether `path` names a bundle rather than source.
pub(crate) fn is_bundle(path: &Path) -> bool {
    extension_of(path).as_deref() == Some("lpyc")
}

/// `path`'s extension in lowercase, as the artifact classifier reads it, so a file takes one route
/// whatever its extension's case.
fn extension_of(path: &Path) -> Option<String> {
    path.extension().and_then(|extension| extension.to_str()).map(str::to_ascii_lowercase)
}

/// Whether the board `session` answered for runs Python programs: a Python runtime is among the ones
/// it holds, and it takes bundles.
#[must_use]
pub(crate) fn runs_python(session: &Negotiated) -> bool {
    holds(session, tier::PYTHON) && session.target_caps.has(Capabilities::BUNDLE)
}

/// Whether the board `session` answered for holds the runtime `tier` names.
fn holds(session: &Negotiated, tier: u8) -> bool {
    session.identity.surfaces.iter().any(|surface| surface.tier == tier)
}

/// The board as a sentence names it, and where it was reached.
fn board_at(session: &Negotiated, target: &str) -> String {
    format!("{} at {target}", the_board(board_name(session.identity.product_model)))
}

/// `Ok` when the board `session` answered for takes a C# program, and the refusal in `verb`'s words
/// when it runs Python and not C#. `program` names what would have been sent.
///
/// # Errors
/// The board runs Python programs only.
pub(crate) fn takes_csharp(verb: &str, program: &str, target: &str, session: &Negotiated) -> Result<(), String> {
    if !on_device::runs_python_only(session) {
        return Ok(());
    }
    Err(format!(
        "lamella {verb}: {program} is C#, and {} runs Python programs. Nothing was sent.",
        board_at(session, target)
    ))
}

/// `Ok` when the board `session` answered for runs Python programs, and the refusal in `verb`'s words
/// when it does not.
///
/// # Errors
/// The board runs C# programs, or holds no runtime that runs Python.
pub(crate) fn takes_python(verb: &str, program: &Path, target: &str, session: &Negotiated) -> Result<(), String> {
    if !runs_python(session) {
        let runs = if holds(session, tier::CIL) { "runs C# programs" } else { "does not run Python programs" };
        return Err(format!(
            "lamella {verb}: {} is a Python program, and {} {runs}. Nothing was sent.",
            program.display(),
            board_at(session, target)
        ));
    }
    let board = session.image_format(tier::PYTHON);
    match lamella_wire::image_format::refusal(board, lamella_py_bytecode::BUNDLE_FORMAT_VERSION) {
        Some(refusal) => Err(format!("lamella {verb}: {refusal}")),
        None => Ok(()),
    }
}

/// The bundle `path` names, for the board that answered as `answered`: its source compiled, with the
/// board's own `board` module where it imports one, or the bundle file as it stands.
///
/// # Errors
/// A source that does not read or compile, or a file that is not a bundle this tool builds.
pub(crate) fn bundle_for(verb: &str, path: &Path, answered: Option<&str>) -> Result<Vec<u8>, String> {
    if is_bundle(path) {
        let bytes = std::fs::read(path).map_err(|error| format!("lamella {verb}: read {}: {error}", path.display()))?;
        let format = bytes
            .get(..6)
            .filter(|head| head[..4] == lamella_py_bytecode::MAGIC)
            .map(|head| u32::from(u16::from_le_bytes([head[4], head[5]])));
        if let Some(refusal) = lamella_wire::image_format::file_refusal(format, lamella_py_bytecode::BUNDLE_FORMAT_VERSION) {
            return Err(format!("lamella {verb}: {}: {refusal}", path.display()));
        }
        if lamella_py_bytecode::Bundle::decode(&bytes).is_err() {
            return Err(format!("lamella {verb}: {}: {}", path.display(), lamella_wire::image_format::REBUILD_IMAGE));
        }
        return Ok(bytes);
    }
    let source =
        std::fs::read_to_string(path).map_err(|error| format!("lamella {verb}: read {}: {error}", path.display()))?;
    crate::program::python_bundle(path, &source, answered).map_err(|error| format!("lamella {verb}: {error}"))
}

/// `lamella deploy <program.py | program.lpyc> --device <target>`: the bundle stored on the board, and
/// started there as `after` says.
pub(crate) fn deploy(path: &Path, target: &str, after: AfterDeploy) -> ExitCode {
    if is_bundle(path) && let Err(refusal) = bundle_for("deploy", path, None) {
        eprintln!("{refusal}");
        return ExitCode::FAILURE;
    }
    let Some((mut transport, session)) = crate::deploy::connect("deploy", target, on_device::host_caps(), None) else {
        return ExitCode::FAILURE;
    };
    if let Err(refusal) = takes_python("deploy", path, target, &session) {
        eprintln!("{refusal}");
        return ExitCode::FAILURE;
    }
    let answered = lamella_wire_host::boards::board_answering(session.identity.product_model);
    let bundle = match bundle_for("deploy", path, answered.as_deref()) {
        Ok(bundle) => bundle,
        Err(refusal) => {
            eprintln!("{refusal}");
            return ExitCode::FAILURE;
        }
    };
    if is_bundle(path) {
        println!("read {} B from {} -- sending it as it stands", bundle.len(), path.display());
    } else {
        println!("built {} B from {}", bundle.len(), path.display());
    }
    crate::deploy::deliver_bundle(&mut transport, &session, target, &bundle, after)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lamella_wire::{Surface, TargetIdentity};

    /// A session with a board that holds the runtimes `tiers` and offers `caps`: its Python runtime
    /// loads this tool's bundle format, and any other loads no image this tool writes.
    fn session(tiers: &[u8], caps: u64) -> Negotiated {
        let mut identity = TargetIdentity { product_model: lamella_wire::product_model::PICO2_W, ..TargetIdentity::default() };
        for &tier in tiers {
            identity = identity.with_surface(Surface { tier, ..Surface::default() });
        }
        let formats = tiers
            .iter()
            .map(|&tier| if tier == tier::PYTHON { lamella_py_bytecode::BUNDLE_FORMAT_VERSION } else { 0 })
            .collect();
        Negotiated {
            version: lamella_wire::PROTOCOL_VERSION,
            caps: Capabilities(caps),
            target_caps: Capabilities(caps),
            identity,
            max_inbound_payload: None,
            image_formats: Some(formats),
        }
    }

    /// **EACH RUNTIME TAKES ITS OWN LANGUAGE**, decided from the HELLO before anything is built.
    #[test]
    fn a_board_takes_the_programs_of_the_runtime_it_holds() {
        let python = session(&[tier::PYTHON], Capabilities::BUNDLE);
        let csharp = session(&[tier::CIL], Capabilities::BAKED_IMAGE);
        assert!(takes_python("run", Path::new("app.py"), "usb", &python).is_ok());
        assert!(takes_csharp("run", "Program.cs", "usb", &csharp).is_ok());
        assert_eq!(
            takes_python("run", Path::new("app.py"), "usb:E664", &csharp),
            Err("lamella run: app.py is a Python program, and the Raspberry Pi Pico 2 W at usb:E664 runs \
                 C# programs. Nothing was sent."
                .to_owned())
        );
        assert_eq!(
            takes_csharp("deploy", "Program.cs", "usb:E664", &python),
            Err("lamella deploy: Program.cs is C#, and the Raspberry Pi Pico 2 W at usb:E664 runs Python \
                 programs. Nothing was sent."
                .to_owned())
        );
    }

    /// A board that holds no runtime, or a Python one that takes no bundles, does not run Python, and
    /// a board that says nothing of its runtimes is not refused a C# program.
    #[test]
    fn only_a_board_that_says_it_runs_python_is_sent_a_python_program() {
        let bare = session(&[], 0);
        let refusal = takes_python("deploy", Path::new("app.py"), "usb", &bare).expect_err("no runtime");
        assert!(refusal.ends_with("does not run Python programs. Nothing was sent."), "{refusal}");
        assert!(takes_python("deploy", Path::new("app.py"), "usb", &session(&[tier::PYTHON], 0)).is_err());
        assert!(takes_csharp("deploy", "Program.cs", "usb", &bare).is_ok());
    }

    /// Source and bundles are both Python; nothing else is.
    #[test]
    fn python_is_its_source_and_its_bundles() {
        assert!(is_python(Path::new("app.py")) && is_python(Path::new("app.lpyc")));
        assert!(!is_python(Path::new("Program.cs")) && !is_python(Path::new("app.lmli")));
    }

    /// **AN EXTENSION IS READ WHATEVER ITS CASE**, as the artifact classifier reads it, so `APP.LPYC`
    /// takes the Python route rather than the one a C# image takes.
    #[test]
    fn a_python_files_extension_is_read_whatever_its_case() {
        assert!(is_python(Path::new("MAIN.PY")) && is_python(Path::new("APP.LPYC")));
        assert!(is_bundle(Path::new("App.Lpyc")) && !is_bundle(Path::new("App.Py")));
        assert_eq!(lamella_flash_routes::artifact::classify(Path::new("APP.LPYC")), lamella_flash_routes::artifact::Kind::WirePayload);
    }

    /// **A FILE THAT IS NOT A BUNDLE IS REFUSED BEFORE THE BOARD IS OPENED**, in a short sentence.
    #[test]
    fn a_file_that_is_not_a_bundle_this_tool_builds_is_refused() {
        let folder = std::env::temp_dir().join(format!("lamella-not-a-bundle-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("old.lpyc");
        std::fs::write(&file, b"LPYC\x00\x00 not this format").unwrap();
        let refused = bundle_for("deploy", &file, None).expect_err("an old format");
        assert!(refused.ends_with(&format!(": {}", lamella_wire::image_format::REBUILD_IMAGE)), "{refused}");
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// **A FILE WITH THIS FORMAT's HEADER IS NOT YET A BUNDLE**: one cut short, or no more than its
    /// six-byte header, is refused before the board is opened, so the board keeps the program it holds.
    /// The whole bundle the same source builds is taken, as it stands.
    #[test]
    fn a_bundle_file_cut_short_is_refused_before_the_board_is_opened() {
        let folder = std::env::temp_dir().join(format!("lamella-cut-bundle-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let whole = crate::program::python_bundle(Path::new("app.py"), "print('ok')\n", None).expect("a bundle");
        let mut header_only = lamella_py_bytecode::MAGIC.to_vec();
        header_only.extend_from_slice(&lamella_py_bytecode::BUNDLE_FORMAT_VERSION.to_le_bytes());
        for (name, bytes, whole_bundle) in [
            ("whole.lpyc", whole.clone(), true),
            ("header.lpyc", header_only, false),
            ("cut.lpyc", whole[..whole.len() - 1].to_vec(), false),
        ] {
            let file = folder.join(name);
            std::fs::write(&file, &bytes).unwrap();
            let answer = bundle_for("deploy", &file, None);
            if whole_bundle {
                assert_eq!(answer.as_deref(), Ok(&whole[..]), "{name}");
            } else {
                let refused = answer.expect_err(name);
                assert!(refused.ends_with(&format!(": {}", lamella_wire::image_format::REBUILD_IMAGE)), "{name}: {refused}");
            }
        }
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// **A BOARD's PYTHON FORMAT IS ASKED BEFORE ANYTHING IS BUILT**: one number against this tool's,
    /// and the answer in the shared words, by which side is older.
    #[test]
    fn a_board_whose_python_format_is_another_is_refused_in_the_shared_words() {
        use lamella_wire::image_format::{PRE_RELEASE, UPDATE_FIRMWARE, UPDATE_LAMELLA};
        let ours = lamella_py_bytecode::BUNDLE_FORMAT_VERSION;
        let mut python = session(&[tier::PYTHON], Capabilities::BUNDLE);
        python.image_formats = Some(vec![ours]);
        assert!(takes_python("run", Path::new("app.py"), "usb", &python).is_ok());
        for (board, said) in [(None, UPDATE_FIRMWARE), (Some(PRE_RELEASE), UPDATE_FIRMWARE), (Some(1), UPDATE_LAMELLA)] {
            python.image_formats = board.map(|format| vec![format]);
            let refused = takes_python("run", Path::new("app.py"), "usb", &python).expect_err("another format");
            assert_eq!(refused, format!("lamella run: {said}"), "a board stating {board:?}");
        }
    }
}
