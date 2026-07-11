//! `lcsc` -- the Lamella C# compiler driver.

use lamella_assemble::{Diagnostic, LineMap, compile_source_with, compile_sources_with};
use lamella_metadata::Assembly;
use lamella_syntax::decode::decode_source;
use lamella_syntax::lexer::{LexOptions, Normalization};
use std::process::ExitCode;

/// The parsed command line.
struct Options {
    sources: Vec<String>,
    output: Option<String>,
    references: Vec<String>,
    emit_debug: bool,
    /// The lexer dialect knobs (9.4.2): identifier folding and the csc typed-reference
    /// operators. Both default off -- raw identifiers (matching csc) and strict ISO-1.
    lex: LexOptions,
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

/// Parses csc-style options. Bare arguments are the source files (one or more);
/// `/reference:` (`-r:`) names a reference assembly, `/out:` the output, and
/// `/debug-` suppresses the PDB (it is emitted by default).
fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut sources = Vec::new();
    let mut output = None;
    let mut references = Vec::new();
    let mut emit_debug = true;
    let mut lex = LexOptions::default();
    for arg in args {
        if let Some(path) = strip_option(arg, &["/reference:", "-r:", "--reference="]) {
            references.push(path.to_owned());
        } else if let Some(path) = strip_option(arg, &["/out:", "-o:", "--out="]) {
            output = Some(path.to_owned());
        } else if let Some(list) = strip_option(arg, &["/define:", "/d:", "-d:", "--define="]) {
            for symbol in list.split([';', ',']) {
                let symbol = symbol.trim();
                if !symbol.is_empty() {
                    lex.defines.insert(symbol.into());
                }
            }
        } else if matches!(arg.as_str(), "/debug" | "/debug+" | "--debug") {
            emit_debug = true;
        } else if matches!(arg.as_str(), "/debug-" | "/debug:none" | "--no-debug") {
            emit_debug = false;
        } else if matches!(
            arg.as_str(),
            "/normalize-identifiers" | "--normalize-identifiers"
        ) {
            lex.normalization = Normalization::Nfc;
        } else if matches!(arg.as_str(), "/typedref" | "--typedref") {
            lex.typedref = true;
        } else if matches!(arg.as_str(), "/native-interop" | "--native-interop") {
            lex.native_interop = true;
        } else if arg.starts_with("/target:") || arg == "/nologo" {
        } else if arg.starts_with('-') || (arg.starts_with('/') && !arg[1..].contains('/')) {
            return Err(format!("unknown option '{arg}'\n{USAGE}"));
        } else {
            sources.push(arg.to_owned());
        }
    }
    if sources.is_empty() {
        return Err(String::from(USAGE));
    }
    Ok(Options {
        sources,
        output,
        references,
        emit_debug,
        lex,
    })
}

const USAGE: &str = "usage: lcsc <source.cs>... [/out:<path>] [/reference:<dll>]... [/define:A;B]... \
     [/debug-] [/normalize-identifiers] [/typedref] [/native-interop]\n\
     /define:A;B seeds preprocessor symbols for #if (9.5.3), like csc's /define (repeatable).\n\
     Several source files compile into one assembly (each is its own PDB document).\n\
     /normalize-identifiers folds identifiers to NFC per ECMA-334 9.4.2 (off by default, to \
     match csc).\n\
     /typedref enables csc's undocumented __makeref/__refvalue/__reftype operators (off by \
     default; they are not in ECMA-334).\n\
     /native-interop enables [DllImport] P/Invoke (off by default; pure-managed targets \
     do not need it).";

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
        .unwrap_or_else(|| replace_extension(&options.sources[0], "dll"));
    let module = file_name(&output);
    let assembly = stem(module);

    if let [source_path] = options.sources.as_slice() {
        let bytes = std::fs::read(source_path)
            .map_err(|error| format!("cannot read '{source_path}': {error}"))?;
        let (text, _encoding) = decode_source(&bytes, host_ansi_code_page());
        let result = compile_source_with(
            &text,
            source_path,
            module,
            assembly,
            &references,
            options.emit_debug,
            options.lex.clone(),
        );
        print_diagnostics(source_path, &text, &result.diagnostics);
        return match result.image {
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
                if !result.diagnostics.iter().any(Diagnostic::is_error) {
                    if let Some(error) = result.emit_error {
                        println!("{source_path}: error: not lowered yet: {error:?}");
                    }
                }
                Ok(false)
            }
        };
    }

    let texts = options
        .sources
        .iter()
        .map(|path| {
            let bytes =
                std::fs::read(path).map_err(|error| format!("cannot read '{path}': {error}"))?;
            Ok(decode_source(&bytes, host_ansi_code_page()).0)
        })
        .collect::<Result<Vec<_>, String>>()?;
    let sources: Vec<(&str, &str)> = texts
        .iter()
        .zip(&options.sources)
        .map(|(text, path)| (text.as_str(), path.as_str()))
        .collect();
    let result = compile_sources_with(
        &sources,
        module,
        assembly,
        &references,
        options.emit_debug,
        options.lex.clone(),
    );
    let mut any_error = false;
    for ((text, path), diagnostics) in sources.iter().zip(&result.diagnostics) {
        print_diagnostics(path, text, diagnostics);
        any_error |= diagnostics.iter().any(Diagnostic::is_error);
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
            if !any_error {
                if let Some(error) = result.emit_error {
                    println!("{}: error: not lowered yet: {error:?}", options.sources[0]);
                }
            }
            Ok(false)
        }
    }
}

/// Prints one source's diagnostics in csc's `path(line,col): severity CSxxxx:` form.
fn print_diagnostics(path: &str, text: &str, diagnostics: &[Diagnostic]) {
    let lines = LineMap::new(text);
    for diagnostic in diagnostics {
        let (line, column) = lines.position(text, diagnostic.span.start);
        let severity = if diagnostic.is_error() {
            "error"
        } else {
            "warning"
        };
        println!(
            "{path}({line},{column}): {severity} CS{:04}: {}",
            diagnostic.code, diagnostic.message
        );
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
        assert_eq!(options.sources, ["App.cs"]);
        assert_eq!(options.output.as_deref(), Some("bin/App.dll"));
        assert_eq!(options.references, ["a.dll"]);
        assert!(options.emit_debug);
    }

    #[test]
    fn several_sources_compile_into_one_assembly() {
        let args = [String::from("A.cs"), String::from("B.cs")];
        let options = parse_args(&args).unwrap();
        assert_eq!(options.sources, ["A.cs", "B.cs"]);
    }

    #[test]
    fn debug_minus_suppresses_the_pdb() {
        let options = parse_args(&[String::from("App.cs"), String::from("/debug-")]).unwrap();
        assert!(!options.emit_debug);
    }

    #[test]
    fn missing_source_is_a_usage_error() {
        assert!(parse_args(&[]).is_err());
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
        assert_eq!(options.sources, ["/home/me/Program.cs"]);
        assert!(parse_args(&[String::from("/unsafe")]).is_err());
    }
}
