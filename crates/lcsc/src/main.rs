//! `lcsc` -- the Lamella C# compiler driver.

use lamella_assemble::{LineMap, compile_source_with};
use lamella_metadata::Assembly;
use lamella_syntax::decode::decode_source;
use lamella_syntax::lexer::Normalization;
use std::process::ExitCode;

/// The parsed command line.
struct Options {
    source: String,
    output: Option<String>,
    references: Vec<String>,
    emit_debug: bool,
    /// How identifiers are compared (9.4.2); `None` (raw, matching csc) by default.
    normalization: Normalization,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let options = match parse_args(&args) {
        Ok(options) => options,
        Err(usage) => {
            eprintln!("{usage}");
            return ExitCode::from(2);
        }
    };
    match compile(&options) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("lcsc: {message}");
            ExitCode::from(2)
        }
    }
}

/// Parses csc-style options. The first bare argument is the source file;
/// `/reference:` (`-r:`) names a reference assembly, `/out:` the output, and
/// `/debug-` suppresses the PDB (it is emitted by default).
fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut source = None;
    let mut output = None;
    let mut references = Vec::new();
    let mut emit_debug = true;
    let mut normalization = Normalization::None;
    for arg in args {
        if let Some(path) = strip_option(arg, &["/reference:", "-r:", "--reference="]) {
            references.push(path.to_owned());
        } else if let Some(path) = strip_option(arg, &["/out:", "-o:", "--out="]) {
            output = Some(path.to_owned());
        } else if matches!(arg.as_str(), "/debug" | "/debug+" | "--debug") {
            emit_debug = true;
        } else if matches!(arg.as_str(), "/debug-" | "/debug:none" | "--no-debug") {
            emit_debug = false;
        } else if matches!(
            arg.as_str(),
            "/normalize-identifiers" | "--normalize-identifiers"
        ) {
            normalization = Normalization::Nfc;
        } else if arg.starts_with("/target:") || arg == "/nologo" {
        } else if arg.starts_with('-') || (arg.starts_with('/') && !arg[1..].contains('/')) {
            return Err(format!("unknown option '{arg}'\n{USAGE}"));
        } else if source.replace(arg.to_owned()).is_some() {
            return Err(format!("more than one source file given\n{USAGE}"));
        }
    }
    let source = source.ok_or_else(|| String::from(USAGE))?;
    Ok(Options {
        source,
        output,
        references,
        emit_debug,
        normalization,
    })
}

const USAGE: &str = "usage: lcsc <source.cs> [/out:<path>] [/reference:<dll>]... [/debug-] \
     [/normalize-identifiers]\n\
     compiles a single source file; multi-file compilation is a planned follow-up.\n\
     /normalize-identifiers folds identifiers to NFC per ECMA-334 9.4.2 (off by default, to \
     match csc).";

/// The first matching prefix's tail, if `arg` starts with one of `prefixes`.
fn strip_option<'a>(arg: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    prefixes.iter().find_map(|prefix| arg.strip_prefix(prefix))
}

/// The host's default ANSI code page -- the code page csc decodes a non-Unicode source (no BOM,
/// not valid UTF-8) in. On Windows this is the locale's code page from Win32 `GetACP`; off Windows
/// there is no ANSI code page, so assume Windows-1252 (the Western default, matching the US-Windows
/// csc oracle the differential runs against).
#[cfg(windows)]
fn host_ansi_code_page() -> u16 {
    unsafe { windows_sys::Win32::Globalization::GetACP() as u16 }
}

#[cfg(not(windows))]
fn host_ansi_code_page() -> u16 {
    1252
}

/// Compiles per `options`, printing diagnostics. Returns whether an assembly was
/// produced (no compile errors), or an `Err` for a usage/IO failure.
fn compile(options: &Options) -> Result<bool, String> {
    let bytes = std::fs::read(&options.source)
        .map_err(|error| format!("cannot read '{}': {error}", options.source))?;
    let (text, _encoding) = decode_source(&bytes, host_ansi_code_page());

    let reference_bytes = options
        .references
        .iter()
        .map(|path| {
            std::fs::read(path).map_err(|error| format!("cannot read reference '{path}': {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let references = reference_bytes
        .iter()
        .map(|bytes| {
            Assembly::read(bytes)
                .map_err(|_| String::from("a reference assembly could not be parsed"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let output = options
        .output
        .clone()
        .unwrap_or_else(|| replace_extension(&options.source, "dll"));
    let module = file_name(&output);
    let assembly = stem(module);

    let result = compile_source_with(
        &text,
        &options.source,
        module,
        assembly,
        &references,
        options.emit_debug,
        options.normalization,
    );

    let lines = LineMap::new(&text);
    for diagnostic in &result.diagnostics {
        let (line, column) = lines.position(&text, diagnostic.span.start);
        let severity = if diagnostic.is_error() {
            "error"
        } else {
            "warning"
        };
        println!(
            "{}({line},{column}): {severity} CS{:04}: {}",
            options.source, diagnostic.code, diagnostic.message
        );
    }

    match result.image {
        Some(image) => {
            std::fs::write(&output, &image)
                .map_err(|error| format!("cannot write '{output}': {error}"))?;
            if let Some(pdb) = result.pdb {
                let pdb_path = replace_extension(&output, "pdb");
                std::fs::write(&pdb_path, &pdb)
                    .map_err(|error| format!("cannot write '{pdb_path}': {error}"))?;
            }
            Ok(true)
        }
        None => {
            if !result
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.is_error())
            {
                if let Some(error) = result.emit_error {
                    println!("{}: error: not lowered yet: {error:?}", options.source);
                }
            }
            Ok(false)
        }
    }
}

/// `path` with its final extension replaced by `extension`.
fn replace_extension(path: &str, extension: &str) -> String {
    let stem = path.rsplit_once('.').map_or(path, |(stem, _)| stem);
    format!("{stem}.{extension}")
}

/// The final path component (after the last `/` or `\`).
fn file_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// A file name without its extension.
fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_source_references_and_output() {
        let args = [
            String::from("/reference:a.dll"),
            String::from("App.cs"),
            String::from("/out:bin/App.dll"),
        ];
        let options = parse_args(&args).unwrap();
        assert_eq!(options.source, "App.cs");
        assert_eq!(options.output.as_deref(), Some("bin/App.dll"));
        assert_eq!(options.references, ["a.dll"]);
        assert!(options.emit_debug);
    }

    #[test]
    fn debug_minus_suppresses_the_pdb() {
        let options = parse_args(&[String::from("App.cs"), String::from("/debug-")]).unwrap();
        assert!(!options.emit_debug);
    }

    #[test]
    fn missing_or_duplicate_source_is_a_usage_error() {
        assert!(parse_args(&[]).is_err());
        assert!(parse_args(&[String::from("a.cs"), String::from("b.cs")]).is_err());
    }

    #[test]
    fn path_helpers_split_names_and_extensions() {
        assert_eq!(replace_extension("a/b/App.cs", "dll"), "a/b/App.dll");
        assert_eq!(file_name("a/b/App.dll"), "App.dll");
        assert_eq!(file_name("a\\b\\App.dll"), "App.dll");
        assert_eq!(stem("App.dll"), "App");
    }

    #[test]
    fn an_absolute_unix_path_is_a_source_file_not_an_option() {
        let options = parse_args(&[String::from("/home/me/Program.cs")]).unwrap();
        assert_eq!(options.source, "/home/me/Program.cs");
        assert!(parse_args(&[String::from("/unsafe")]).is_err());
    }
}
