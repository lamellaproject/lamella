//! `lcsc` -- the Lamella C# compiler driver.

use lamella_assemble::{Diagnostic, LineMap, compile_source_with, compile_sources_with};
use lamella_metadata::Assembly;
use lamella_syntax::decode::decode_source;
use lamella_syntax::lexer::{LexOptions, Normalization};
use lamella_syntax::version::{LanguageVersion, LanguageVersionError};
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
    if wants_help(&args) {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let options = match parse_args(&args) {
        Ok(options) => options,
        Err(usage) => {
            eprintln!("{usage}");
            return ExitCode::from(2);
        }
    };
    match run_on_compile_stack(move || compile(&options)) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(message) => {
            eprintln!("lcsc: {message}");
            ExitCode::from(2)
        }
    }
}

/// Runs `work` on a freshly spawned thread whose stack is large enough for the front end's
/// recursion to follow deeply nested source (see the call site). Fully synchronous -- the
/// calling thread waits for the compile and gets its result back -- and a panic inside the
/// compile re-raises on return, so behaviour is unchanged but for the roomier stack.
fn run_on_compile_stack<T: Send>(work: impl FnOnce() -> T + Send) -> T {
    const COMPILE_STACK_BYTES: usize = 64 * 1024 * 1024;
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name(String::from("lcsc-compile"))
            .stack_size(COMPILE_STACK_BYTES)
            .spawn_scoped(scope, work)
            .expect("spawn the lcsc compile thread")
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
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
    lex.version = LanguageVersion::SELECTABLE_MAX;
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
        } else if matches!(arg.as_str(), "/unsafe" | "/unsafe+" | "--unsafe") {
            lex.unsafe_code = true;
        } else if matches!(arg.as_str(), "/unsafe-") {
            lex.unsafe_code = false;
        } else if matches!(arg.as_str(), "/typedref" | "--typedref") {
            lex.typedref = true;
        } else if matches!(arg.as_str(), "/native-interop" | "--native-interop") {
            lex.native_interop = true;
        } else if let Some(version) = strip_option(arg, &["/langversion:", "--langversion="]) {
            match LanguageVersion::parse_flag(version) {
                Ok(selected) => lex.version = selected,
                Err(LanguageVersionError::Unsupported) => {
                    return Err(format!(
                        "/langversion:{version} names a C# version this compiler cannot gate \
                         against yet; the newest it can is {}",
                        LanguageVersion::SELECTABLE_MAX.flag_value()
                    ));
                }
                Err(LanguageVersionError::Invalid) => {
                    return Err(format!("/langversion:{version} is not a C# language version"));
                }
            }
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

const USAGE: &str = "\
lcsc -- the Lamella C# compiler (C# 1.0 / ECMA-334 first edition).

usage: lcsc <source.cs>... [options]

  <source.cs>...          one or more C# sources; all compile into ONE assembly
                          (each is its own PDB document).
  /out:<path>             output assembly path (default: the first source's name, .dll).
  /reference:<dll>        reference a metadata assembly (repeatable; also -r:, --reference=).
  /define:A;B             seed #if preprocessor symbols (9.5.3); ';' or ',' separated, repeatable.
  /debug-                 suppress the Portable PDB (it is emitted by default).
  /normalize-identifiers  fold identifiers to NFC (ECMA-334 9.4.2; off by default, to match csc).
  /unsafe                 permit unsafe code (pointers, stackalloc, fixed); off by default, as
                          csc's is. Writing `unsafe` without it is CS0227. Also /unsafe+, /unsafe-.
  /typedref               enable csc's undocumented __makeref/__refvalue/__reftype (not in ECMA-334).
  /native-interop         enable [DllImport] P/Invoke (off by default; pure-managed targets omit it).
  /help, -h, /?           print this help.

Diagnostics use csc's form: path(line,col): error CSxxxx: message.";

/// Whether the arguments request help (`--help`, `-h`, `/help`, `/?`, `-?`) -- printed to stdout
/// with a success exit, unlike a usage error.
fn wants_help(args: &[String]) -> bool {
    args.iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h" | "/help" | "/?" | "-?"))
}

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
                publish(&output, &image, result.pdb.as_deref())?;
                Ok(true)
            }
            None => {
                if !result.diagnostics.iter().any(Diagnostic::is_error) {
                    if let Some(error) = result.emit_error {
                        println!(
                            "{source_path}: error: this construct is not yet supported by lcsc: {error}"
                        );
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
            publish(&output, &image, result.pdb.as_deref())?;
            Ok(true)
        }
        None => {
            if !any_error {
                if let Some(error) = result.emit_error {
                    println!(
                        "{}: error: this construct is not yet supported by lcsc: {error}",
                        options.sources[0]
                    );
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
            "{path}({line},{column}): {severity} {}{:04}: {}",
            diagnostic.namespace.prefix(),
            diagnostic.code,
            diagnostic.message
        );
    }
}

/// `path` with its final extension replaced by `extension`.
fn replace_extension(path: &str, extension: &str) -> String {
    let stem = path.rsplit_once('.').map_or(path, |(stem, _)| stem);
    format!("{stem}.{extension}")
}

/// Writes the emitted assembly (and its PDB, if present) to disk atomically -- each artifact is
/// staged in a temporary sibling and renamed into place -- so an interrupted or failing write can
/// never leave a partial `.dll` (or `.pdb`) where the runtime loader or a later compile would pick
/// it up. The image is published before the PDB: the PDB is a debug adjunct, so an image that lands
/// even if a following PDB write fails is still a complete, loadable assembly.
fn publish(output: &str, image: &[u8], pdb: Option<&[u8]>) -> Result<(), String> {
    write_atomic(output, image).map_err(|error| format!("cannot write '{output}': {error}"))?;
    if let Some(pdb) = pdb {
        let pdb_path = replace_extension(output, "pdb");
        write_atomic(&pdb_path, pdb)
            .map_err(|error| format!("cannot write '{pdb_path}': {error}"))?;
    }
    Ok(())
}

/// Writes `bytes` to `path` atomically: stage them in a uniquely named temporary file in the SAME
/// directory (so the rename stays on one volume -- a cross-volume rename is a copy, not atomic),
/// then rename it over `path`. `fs::rename` replaces the destination atomically on Windows
/// (`MoveFileEx` with `MOVEFILE_REPLACE_EXISTING`) and POSIX alike. On any failure the temporary
/// file is removed and `path` is left exactly as it was.
fn write_atomic(path: &str, bytes: &[u8]) -> std::io::Result<()> {
    let temp = format!("{path}.tmp.{}", std::process::id());
    let staged = stage_temp(&temp, bytes).and_then(|()| std::fs::rename(&temp, path));
    if staged.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    staged
}

/// Stages `bytes` into `temp`: create, write, and sync to disk. The file is closed when this
/// returns, so the caller can rename it -- Windows cannot rename a file it still holds open. The
/// sync makes the bytes durable before the rename, so a crash cannot surface a zero-length file
/// under the final path.
fn stage_temp(temp: &str, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut file = std::fs::File::create(temp)?;
    file.write_all(bytes)?;
    file.sync_all()
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
    fn langversion_selects_a_dialect_and_refuses_only_what_it_cannot_gate() {
        let selected = |v: &str| -> LanguageVersion {
            let args = [String::from("App.cs"), format!("/langversion:{v}")];
            parse_args(&args).unwrap_or_else(|e| panic!("{v} should parse: {e}")).lex.version
        };
        assert_eq!(selected("ISO-1"), LanguageVersion::CSharp1);
        assert_eq!(selected("iso-1"), LanguageVersion::CSharp1);
        assert_eq!(selected("ISO-2"), LanguageVersion::CSharp2);
        assert_eq!(selected("7"), LanguageVersion::CSharp7);
        assert_eq!(selected("9"), LanguageVersion::CSharp9);
        assert_eq!(selected("11"), LanguageVersion::CSharp11);
        assert_eq!(selected("latest"), LanguageVersion::SELECTABLE_MAX);

        let bare = [String::from("App.cs")];
        assert_eq!(
            parse_args(&bare).expect("bare parse").lex.version,
            LanguageVersion::SELECTABLE_MAX,
            "the driver ships the product rung"
        );
        assert_ne!(
            LanguageVersion::DEFAULT,
            LanguageVersion::SELECTABLE_MAX,
            "the conformance default and the product ceiling answer different questions"
        );
        assert_eq!(LanguageVersion::DEFAULT, LanguageVersion::CSharp1);

        for v in ["/langversion:12", "/langversion:14", "/langversion:preview"] {
            let args = [String::from("App.cs"), String::from(v)];
            let message = parse_args(&args).err().unwrap_or_else(|| panic!("{v} should be refused"));
            assert!(message.contains("cannot gate"), "{v} should say why: {message}");
        }
        for v in ["/langversion:banana", "/langversion:1.5"] {
            let args = [String::from("App.cs"), String::from(v)];
            let message = parse_args(&args).err().unwrap_or_else(|| panic!("{v} should be refused"));
            assert!(message.contains("not a C# language version"), "{v}: {message}");
        }
    }

    #[test]
    fn help_flags_are_recognized() {
        for flag in ["--help", "-h", "/help", "/?", "-?"] {
            assert!(wants_help(&[String::from(flag)]), "{flag} should request help");
        }
        assert!(wants_help(&[String::from("App.cs"), String::from("--help")]));
        assert!(!wants_help(&[String::from("App.cs")]));
        assert!(!wants_help(&[String::from("/helper")]));
    }

    #[test]
    fn path_helpers_split_names_and_extensions() {
        assert_eq!(replace_extension("a/b/App.cs", "dll"), "a/b/App.dll");
        assert_eq!(file_name("a/b/App.dll"), "App.dll");
        assert_eq!(file_name("a\\b\\App.dll"), "App.dll");
        assert_eq!(stem("App.dll"), "App");
    }

    #[test]
    fn write_atomic_replaces_in_place_and_leaves_no_temp() {
        let path = std::env::temp_dir().join(format!("lcsc-atomic-{}.bin", std::process::id()));
        let path = path.to_str().expect("temp path is valid UTF-8");
        write_atomic(path, b"first").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"first");
        write_atomic(path, b"second is longer than first").unwrap();
        assert_eq!(std::fs::read(path).unwrap(), b"second is longer than first");
        let temp = format!("{path}.tmp.{}", std::process::id());
        assert!(!std::path::Path::new(&temp).exists(), "staging temp {temp} should be gone");
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn an_absolute_unix_path_is_a_source_file_not_an_option() {
        let options = parse_args(&[String::from("/opt/src/Program.cs")]).unwrap();
        assert_eq!(options.sources, ["/opt/src/Program.cs"]);
        let unsafe_options = parse_args(&[String::from("/unsafe"), String::from("a.cs")]).unwrap();
        assert!(unsafe_options.lex.unsafe_code);
        assert_eq!(unsafe_options.sources, ["a.cs"]);
        assert!(parse_args(&[String::from("/nonsense")]).is_err());
    }

    #[test]
    fn unsafe_code_is_opt_in_exactly_as_it_is_for_csc() {
        let parse = |args: &[&str]| {
            let owned: Vec<String> = args
                .iter()
                .map(|arg| String::from(*arg))
                .chain(core::iter::once(String::from("a.cs")))
                .collect();
            parse_args(&owned).unwrap().lex.unsafe_code
        };
        assert!(!parse(&[]));
        assert!(parse(&["/unsafe"]));
        assert!(parse(&["/unsafe+"]));
        assert!(parse(&["--unsafe"]));
        assert!(!parse(&["/unsafe-"]));
        assert!(!parse(&["/unsafe", "/unsafe-"]));
        assert!(parse(&["/unsafe-", "/unsafe"]));
    }
}
