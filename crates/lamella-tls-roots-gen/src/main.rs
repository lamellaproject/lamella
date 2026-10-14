//! Generates the root-certificate store that `lamella-tls-roots` carries, from curl's copy of the
//! Mozilla CA store (`cacert.pem`): `roots.der`, the DER of every root certificate concatenated,
//! and `bundle.rs`, an index of those certificates sorted by subject Name, which the crate searches
//! with a child certificate's issuer Name.
//!
//! Usage: `lamella-tls-roots-gen <cacert.pem> <output-dir>`. Both files in the output directory
//! are replaced, and only after every certificate in the input has parsed, so a truncated or
//! corrupted download leaves the files that were there untouched. The same input always produces
//! the same bytes.

mod der;
mod emit;
mod pem;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
usage: lamella-tls-roots-gen <cacert.pem> <output-dir>

Reads curl's copy of the Mozilla CA store (https://curl.se/ca/cacert.pem) and writes roots.der
and bundle.rs into <output-dir>, replacing both. Nothing is written unless every certificate in
the file parses.";

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if let Some(option) = args
        .iter()
        .find(|arg| arg.to_string_lossy().starts_with('-'))
    {
        eprintln!(
            "lamella-tls-roots-gen: unknown option '{}'\n\n{USAGE}",
            option.to_string_lossy()
        );
        return ExitCode::from(2);
    }
    let [pem_path, output_dir] = args.as_slice() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    match run(Path::new(pem_path), Path::new(output_dir)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("lamella-tls-roots-gen: {message}");
            ExitCode::FAILURE
        }
    }
}

/// Reads `pem_path`, generates the store in memory, and only then replaces `roots.der` and
/// `bundle.rs` in `output_dir`, which must already exist.
fn run(pem_path: &Path, output_dir: &Path) -> Result<(), String> {
    if !output_dir.is_dir() {
        return Err(format!(
            "the output directory {} does not exist",
            output_dir.display()
        ));
    }
    let bytes =
        std::fs::read(pem_path).map_err(|error| format!("{}: {error}", pem_path.display()))?;
    let text = String::from_utf8_lossy(&bytes);
    let in_file = |error: String| format!("{}: {error}", pem_path.display());
    let snapshot = pem::snapshot(&text).map_err(in_file)?;
    let blocks = pem::certificates(&text).map_err(in_file)?;
    let store = emit::store(snapshot, &blocks).map_err(in_file)?;

    let der_path = output_dir.join("roots.der");
    let rust_path = output_dir.join("bundle.rs");
    write_replacing(&der_path, &store.der)?;
    write_replacing(&rust_path, store.rust.as_bytes())?;
    println!(
        "lamella-tls-roots-gen: {} roots, Mozilla store as of {snapshot}",
        store.roots
    );
    println!("  {}: {} bytes", der_path.display(), store.der.len());
    println!("  {}: {} index entries", rust_path.display(), store.roots);
    Ok(())
}

/// Writes `contents` beside `path` and renames it over `path`, so an interrupted run never leaves
/// a file half written.
fn write_replacing(path: &Path, contents: &[u8]) -> Result<(), String> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    std::fs::write(&temporary, contents)
        .map_err(|error| format!("{}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, path).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("{}: {error}", path.display())
    })
}
