//! What `Lamella.Sdk` hands this tool from `dotnet build` and `dotnet run`, and what it hands back.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lamella_aot::BuildKind;
use lamella_wire_host::engine::LcscCompiler;

use crate::ending::Ended;
use crate::folders::{Folders, REF_DIR, RUNTIME_SUPPORT_DIR};
use crate::packages::PackageReference;

/// `lamella build`'s option that answers which assemblies a project's packages are, for MSBuild.
pub(crate) const REFERENCES: &str = "--msbuild-references";

/// `lamella build`'s option that compiles what MSBuild evaluated.
pub(crate) const COMPILE: &str = "--msbuild-compile";

/// `lamella build`'s option that writes what `dotnet publish` makes of a project: its image for
/// the board the project names.
pub(crate) const PUBLISH: &str = "--msbuild-publish";

/// `lamella build`'s option that writes, for `Lamella.Sdk` when it is packed, the packages this
/// tool provides from the class library, which a `lamella1.0` restore keeps from NuGet.
pub(crate) const PACKAGES: &str = "--msbuild-packages";

/// `lamella build`'s option that writes a debug session's manifest: what the build made, for the
/// debugger, lamella-debug.
pub(crate) const DEBUG: &str = "--msbuild-debug";

/// The version of the debug manifest this tool writes, which the debugger checks.
const DEBUG_MANIFEST_FORMAT: u64 = 1;

/// `lamella run`'s option naming an assembly a build already made.
pub(crate) const BUILT: &str = "--built";

/// The framework's assemblies beside the corlib: every program may use them and none names them,
/// as a .NET program never names `System.Net.NetworkInformation`.
const FRAMEWORK: &[&str] = &["System.Net.NetworkInformation"];

/// The C# compiler's feature that reads a file-based app's sources, `#:` directives and all, as
/// MSBuild's `Features` names it in the project `dotnet run app.cs` writes for a file.
const FILE_BASED_PROGRAM: &str = "FileBasedProgram";

/// What the SDK wrote under `obj\`: one `key=value` per line, a key repeated for each value.
#[derive(Debug, Default)]
struct Inputs {
    /// The project's full path, which every diagnostic names.
    project: String,
    /// The packages the project names, as MSBuild evaluated them.
    packages: Vec<PackageReference>,
    /// Where the references' item list goes.
    references_out: Option<PathBuf>,
    /// The sources to compile.
    sources: Vec<PathBuf>,
    /// The project's own libraries, each a built `.dll`.
    references: Vec<PathBuf>,
    /// The preprocessor symbols.
    defines: Vec<String>,
    /// Whether unsafe code is on.
    allow_unsafe: bool,
    /// The C# version the project's `LangVersion` selects, where the inputs name one.
    lang_version: Option<lamella_syntax::version::LanguageVersion>,
    /// Whether the sources are a file-based app's: MSBuild's `Features` names
    /// [`FILE_BASED_PROGRAM`].
    file_based: bool,
    /// The assembly's name.
    assembly_name: Option<String>,
    /// Where the assembly goes.
    out: Option<PathBuf>,
    /// Where the assembly's symbol file goes, a portable PDB, when the build makes one.
    pdb: Option<PathBuf>,
    /// The board the project names, `LamellaTargetBoard`.
    board: Option<String>,
    /// What a publish writes: `elf`, the native image, or `lmli`, the image Lamella's firmware runs.
    format: Option<String>,
    /// The assembly the build made, beside the libraries it copied there, which a publish bakes.
    built: Option<PathBuf>,
    /// Where a publish writes its image; for a debug manifest, the native image the build published.
    image: Option<PathBuf>,
    /// The project's `Optimize`, which says what kind of build it is ([`Inputs::build_kind`]): empty where the
    /// project sets none.
    optimize: Option<String>,
    /// The target framework the build was for: `lamella1.0`, or `net11.0` for Lamella RHU.
    framework: Option<String>,
    /// What a debug session debugs: `native`, `host` or `interpreted`.
    kind: Option<String>,
    /// The device a debug session's build was made for, as `dotnet run` names one.
    device: Option<String>,
    /// MSBuild's `PathMap`, as the C# compiler takes it.
    path_map: Option<String>,
    /// The debugger the SDK checked, which the manifest names.
    debugger: Option<PathBuf>,
    /// Where the debug manifest goes.
    manifest: Option<PathBuf>,
}

impl Inputs {
    /// The file at `path`.
    fn read(path: &Path) -> Result<Self, String> {
        let text = crate::text::read_text(path)
            .map_err(|error| format!("lamella: error: read the build's inputs {}: {error}", path.display()))?;
        let mut inputs = Self::default();
        let mut lang_version = None;
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "project" => inputs.project = value.to_owned(),
                "package" => inputs.packages.push(PackageReference::from_directive(value, 0)),
                "references-out" => inputs.references_out = Some(PathBuf::from(value)),
                "source" => inputs.sources.push(PathBuf::from(value)),
                "reference" => inputs.references.push(PathBuf::from(value)),
                "define" if !value.is_empty() => inputs.defines.push(value.to_owned()),
                "allowunsafe" => inputs.allow_unsafe = value.eq_ignore_ascii_case("true"),
                "langversion" => lang_version = (!value.is_empty()).then(|| value.to_owned()),
                "features" => inputs.file_based |= names_file_based(value),
                "assemblyname" if !value.is_empty() => inputs.assembly_name = Some(value.to_owned()),
                "out" => inputs.out = Some(PathBuf::from(value)),
                "pdb" if !value.is_empty() => inputs.pdb = Some(PathBuf::from(value)),
                "board" if !value.is_empty() => inputs.board = Some(value.to_owned()),
                "format" => inputs.format = Some(value.to_owned()),
                "built" => inputs.built = Some(PathBuf::from(value)),
                "image" => inputs.image = Some(PathBuf::from(value)),
                "optimize" => inputs.optimize = Some(value.to_owned()),
                "framework" if !value.is_empty() => inputs.framework = Some(value.to_owned()),
                "kind" if !value.is_empty() => inputs.kind = Some(value.to_owned()),
                "device" if !value.is_empty() => inputs.device = Some(value.to_owned()),
                "pathmap" if !value.is_empty() => inputs.path_map = Some(value.to_owned()),
                "debugger" if !value.is_empty() => inputs.debugger = Some(PathBuf::from(value)),
                "manifest" if !value.is_empty() => inputs.manifest = Some(PathBuf::from(value)),
                _ => {}
            }
        }
        if let Some(value) = lang_version {
            let selected = crate::project::language_version(&value)
                .map_err(|why| format!("lamella: error: {}: LangVersion {why}.", inputs.project))?;
            inputs.lang_version = Some(selected);
        }
        Ok(inputs)
    }

    /// The kind of build the SDK says this is: the project's `Optimize`, read as the C# compiler reads it. `true`
    /// is a Release build; `false`, or none set, is a Debug build.
    ///
    /// # Errors
    /// The inputs name no build kind, or the project's `Optimize` is neither `true` nor `false`.
    fn build_kind(&self) -> Result<BuildKind, String> {
        let Some(optimize) = self.optimize.as_deref() else {
            return Err("lamella: error: the publish's inputs name no build kind, and a native image is built as \
                        one: Debug or Release, from the project's Optimize."
                .to_owned());
        };
        if optimize.eq_ignore_ascii_case("true") {
            Ok(BuildKind::from_optimize(true))
        } else if optimize.is_empty() || optimize.eq_ignore_ascii_case("false") {
            Ok(BuildKind::from_optimize(false))
        } else {
            Err(format!("lamella: error: the project's Optimize is \"{optimize}\", and the C# compiler takes true or false."))
        }
    }
}

/// Whether `features`, MSBuild's `Features` as the C# compiler takes it, names [`FILE_BASED_PROGRAM`]:
/// features separated by semicolons, commas or spaces, each a name or `name=value`.
fn names_file_based(features: &str) -> bool {
    features
        .split([';', ',', ' '])
        .filter_map(|feature| feature.split('=').next())
        .any(|name| name.trim().eq_ignore_ascii_case(FILE_BASED_PROGRAM))
}

/// A package's assembly in the class library, chosen for a project.
struct Resolved {
    id: String,
    version: String,
    path: PathBuf,
}

/// `lamella build --msbuild-references <inputs>`, `lamella build --msbuild-compile <inputs>` and
/// `lamella build --msbuild-publish <inputs>`.
pub fn build(args: &[String]) -> ExitCode {
    let [option, inputs] = args else {
        eprintln!(
            "lamella: error: {REFERENCES}, {COMPILE}, {PUBLISH} and {DEBUG} each take the build's inputs file, \
             alone, and {PACKAGES} the file it writes."
        );
        return ExitCode::FAILURE;
    };
    if option == PACKAGES {
        return match provided_packages(Path::new(inputs)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::FAILURE
            }
        };
    }
    let inputs = match Inputs::read(Path::new(inputs)) {
        Ok(inputs) => inputs,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let outcome = match option.as_str() {
        REFERENCES => references(&inputs),
        PUBLISH => publish(&inputs).map_err(|error| as_msbuild_error(&inputs.project, &error)),
        DEBUG => debug_manifest(&inputs).map_err(|error| as_msbuild_error(&inputs.project, &error)),
        _ => compile(&inputs),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if !error.is_empty() {
                eprintln!("{error}");
            }
            ExitCode::FAILURE
        }
    }
}

/// `refusal` as MSBuild shows an error: a sentence the tool worded for a reader, which MSBuild would
/// otherwise show as a message beside an error saying only that the tool "exited with code 1",
/// becomes one line in MSBuild's canonical form, `<project> : error : <its first paragraph>`, with
/// the rest after it. A refusal already in that form, as the compiler's diagnostics are, is left as
/// it is.
fn as_msbuild_error(project: &str, refusal: &str) -> String {
    let first_line = refusal.lines().next().unwrap_or_default();
    if first_line.contains(": error ") || first_line.contains(": error:") {
        return refusal.to_owned();
    }
    let (first, rest) = refusal.split_once("\n\n").unwrap_or((refusal, ""));
    let joined = first.lines().map(str::trim).collect::<Vec<_>>().join(" ");
    if rest.is_empty() { format!("{project} : error : {joined}") } else { format!("{project} : error : {joined}\n\n{rest}") }
}

/// Writes to `out` the packages this tool provides from the class library it finds: one
/// `_LamellaProvidedPackage` per assembly of the class library that [`crate::packages::provided`]
/// builds, by its id. `Lamella.Sdk` keeps a `PackageReference` to one of them from NuGet, and hands
/// it to this tool, and every other package to NuGet.
fn provided_packages(out: &Path) -> Result<(), String> {
    let compiler = LcscCompiler::discover().map_err(|error| format!("lamella: error: {error}"))?;
    let mut ids: Vec<String> = compiler
        .library_names()
        .into_iter()
        .filter(|name| !FRAMEWORK.iter().any(|framework| framework.eq_ignore_ascii_case(name)))
        .filter_map(|name| crate::packages::provided(&name).map(|provided| provided.id))
        .collect();
    ids.sort();
    ids.dedup();
    let mut text = String::from(
        "<Project>\n  <!-- The packages the lamella tool provides from the class library, written when this SDK was packed: a lamella1.0 project's reference to one of them is the tool's, and every other package is NuGet's. -->\n  <ItemGroup>\n",
    );
    for id in &ids {
        let _ = writeln!(text, "    <_LamellaProvidedPackage Include=\"{}\" />", crate::devices::attribute(id));
    }
    text.push_str("  </ItemGroup>\n</Project>\n");
    std::fs::write(out, text).map_err(|error| format!("lamella: error: write {}: {error}", out.display()))
}

/// The project MSBuild evaluated, as this tool compiles it, built for `board` where a publish names
/// one.
fn project_of(inputs: &Inputs, board: Option<&str>) -> crate::project::Project {
    let project_path = Path::new(&inputs.project);
    let assembly_name = inputs.assembly_name.clone().unwrap_or_else(|| {
        project_path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("Program").to_owned()
    });
    crate::project::Project {
        output_type: crate::project::OutputType::Exe,
        assembly_name,
        lang_version: inputs.lang_version,
        allow_unsafe: inputs.allow_unsafe,
        references: inputs.references.clone(),
        sources: inputs.sources.clone(),
        board: board.map(|id| crate::project::NamedBoard { id: id.to_owned(), line: 0 }),
        packages: Vec::new(),
        package_warnings: Vec::new(),
        defines: inputs.defines.clone(),
        file_based: inputs.file_based,
    }
}

/// What `dotnet publish` makes of the project, for the board it names: the image `format` names,
/// written where `image` says.
/// - `elf`, the native image: the program compiled for a debugger and linked for the board, as
///   `lamella build --format elf` links it, with its record beside it.
/// - `lmli`, the image Lamella's firmware runs a program from: the assembly the build made, baked
///   for the board with its libraries, as `dotnet run` sends it to a board running the firmware,
///   with its record beside it, whose model is the interpreter's.
fn publish(inputs: &Inputs) -> Result<(), String> {
    let Some(board) = inputs.board.as_deref() else {
        return Err("lamella: error: the publish's inputs name no board, and an image is written for one.".to_owned());
    };
    let Some(image) = &inputs.image else {
        return Err("lamella: error: the publish's inputs name no image file.".to_owned());
    };
    if let Some(parent) = image.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let folders = Folders::default();
    match inputs.format.as_deref() {
        Some("elf") => {
            let kind = inputs.build_kind()?;
            let resolved = resolve(inputs, false)?;
            let set = bound_set_for(&resolved, Some(board))?;
            let project = project_of(inputs, Some(board));
            let libraries = crate::flash::project_references(&project, "publish")?;
            let debuggable =
                crate::program::compile_project_for_debugging_against(&project, &libraries, &set, "publish")?;
            let elf = crate::flash::debug_elf_of(&debuggable, board, "publish", Path::new(&inputs.project), &folders, kind)?;
            let loaded = lamella_elf::flat_image(&elf)
                .map_err(|error| format!("lamella publish: the ELF this built does not read back as one: {error:?}"))?;
            crate::flash::refuse_unless_it_fits("publish", board, &loaded.bytes)?;
            crate::program::write_elf_with_record(image, &elf, board, "publish").map(|_| ())
        }
        Some("lmli") => publish_baked(inputs, board, image, &folders),
        other => Err(format!("lamella: error: the publish's inputs name the format {other:?}; a publish writes elf or lmli.")),
    }
}

/// The `lmli` publish: the assembly the build made, baked for `board` as `dotnet run` sends it to a
/// board running Lamella's firmware, written to `image`.
#[cfg(feature = "bake")]
fn publish_baked(inputs: &Inputs, board: &str, image: &Path, folders: &Folders) -> Result<(), String> {
    let Some(built) = &inputs.built else {
        return Err("lamella: error: the publish's inputs name no built assembly to bake.".to_owned());
    };
    let baked = crate::bake::image_of_built(built, "publish", Some(board), folders)?;
    crate::program::write_lmli_with_record(image, &baked, board, "publish")
}

/// In a build that cannot bake, the sentence that names the missing feature.
#[cfg(not(feature = "bake"))]
fn publish_baked(_inputs: &Inputs, _board: &str, _image: &Path, _folders: &Folders) -> Result<(), String> {
    Err("lamella publish: this lamella cannot write the image Lamella's firmware runs a program from, which \
         LamellaPublishAot false asks for.\n\n\
         Baking one needs the tool's `bake` feature, which a released lamella has:\n\
         \x20   cargo build -p lamella-cli --features bake\n\n\
         With LamellaPublishAot true, the default, it writes the native image."
        .to_owned())
}

/// The debug manifest of a debug session's build, written where the SDK names: what the build made
/// for the device it was given, for the debugger, lamella-debug, which reads it and never builds.
/// Every path in it is absolute, so the debugger resolves nothing against a working directory.
/// - `native`, through a debug probe: the ELF the build published, which carries its debug
///   information, with its record beside it; the board; and the path map the compiler named the
///   sources by, which Lamella RHU sets.
/// - `host`, this computer: the assembly, its portable PDB and the class library's folder.
/// - `interpreted`, a board running Lamella's firmware: the assembly baked into the image that board
///   runs, for the board the project names, or else the board the device answers as, as a run bakes
///   it; the image's line map beside it; the PDB; and the board.
///
/// Each kind also names the framework, the device as the build was given it, and the debugger the
/// SDK checked. Nothing is written when a file the kind needs is missing.
fn debug_manifest(inputs: &Inputs) -> Result<(), String> {
    use serde_json::{Value, json};
    let named = |value: &Option<String>, what: &str| {
        value.clone().ok_or_else(|| format!("lamella: error: the debug build's inputs name no {what}."))
    };
    let Some(out) = &inputs.manifest else {
        return Err("lamella: error: the debug build's inputs name no manifest file.".to_owned());
    };
    let Some(debugger) = &inputs.debugger else {
        return Err("lamella: error: the debug build's inputs name no debugger.".to_owned());
    };
    let (framework, kind, device) = (named(&inputs.framework, "framework")?, named(&inputs.kind, "kind")?, named(&inputs.device, "device")?);
    let absolute = |path: &Path| crate::program::document_path(path).unwrap_or_else(|_| path.display().to_string());
    let mut manifest = serde_json::Map::new();
    manifest.insert("format".to_owned(), json!(DEBUG_MANIFEST_FORMAT));
    manifest.insert("framework".to_owned(), json!(framework));
    manifest.insert("kind".to_owned(), json!(kind));
    match kind.as_str() {
        "native" => {
            let Some(image) = &inputs.image else {
                return Err("lamella: error: the debug build's inputs name no image.".to_owned());
            };
            if !image.is_file() {
                return Err(format!(
                    "The build published no image at {}, and a debug session through a debug probe writes the \
                     image the build publishes for it.",
                    image.display()
                ));
            }
            let record = lamella_flash_routes::manifest::path_for(image);
            if !record.is_file() {
                return Err(format!(
                    "The image {} has no record beside it, {}, which says which board it was built for.",
                    image.display(),
                    record.display()
                ));
            }
            manifest.insert("image".to_owned(), json!(absolute(image)));
            manifest.insert("board".to_owned(), json!(named(&inputs.board, "board")?));
        }
        "host" => {
            let (built, pdb) = built_and_symbols(inputs)?;
            let library = Folders::default().compiler().map_err(|error| format!("lamella: error: {error}"))?;
            manifest.insert("image".to_owned(), json!(absolute(built)));
            manifest.insert("symbols".to_owned(), json!(absolute(pdb)));
            manifest.insert("references".to_owned(), json!(absolute(library.folder())));
        }
        "interpreted" => {
            let (built, pdb) = built_and_symbols(inputs)?;
            let Some(image) = &inputs.image else {
                return Err("lamella: error: the debug build's inputs name no image.".to_owned());
            };
            let board = match &inputs.board {
                Some(board) => board.clone(),
                None => crate::devices::board_of(&device)?,
            };
            let symbols = std::fs::read(pdb).map_err(|error| format!("lamella: error: read {}: {error}", pdb.display()))?;
            let (baked, map) = baked_for_debugging(built, &symbols, &board)?;
            let line_map = with_suffix(image, ".srcmap.json");
            for (file, bytes) in [(image, &baked), (&line_map, &map)] {
                if let Some(parent) = file.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(file, bytes).map_err(|error| format!("lamella: error: write {}: {error}", file.display()))?;
            }
            manifest.insert("image".to_owned(), json!(absolute(image)));
            manifest.insert("symbols".to_owned(), json!(absolute(pdb)));
            manifest.insert("lineMap".to_owned(), json!(absolute(&line_map)));
            manifest.insert("board".to_owned(), json!(board));
        }
        other => return Err(format!("lamella: error: the debug build's inputs name the kind {other:?}.")),
    }
    manifest.insert("device".to_owned(), json!(device));
    let pairs = inputs.path_map.as_deref().map(path_map).unwrap_or_default();
    if !pairs.is_empty() {
        let map: Vec<Value> = pairs.into_iter().map(|(from, to)| json!({ "from": from, "to": to })).collect();
        manifest.insert("pathMap".to_owned(), Value::Array(map));
    }
    manifest.insert("debugger".to_owned(), json!(absolute(debugger)));
    let mut text = serde_json::to_string_pretty(&Value::Object(manifest)).map_err(|error| format!("lamella: error: {error}"))?;
    text.push('\n');
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(out, text).map_err(|error| format!("lamella: error: write {}: {error}", out.display()))
}

/// The assembly a debug build made and its symbol file, both there, or why a debugger cannot have
/// them.
fn built_and_symbols(inputs: &Inputs) -> Result<(&PathBuf, &PathBuf), String> {
    let Some(built) = &inputs.built else {
        return Err("lamella: error: the debug build's inputs name no built assembly.".to_owned());
    };
    let Some(pdb) = &inputs.pdb else {
        return Err(format!(
            "The build wrote no symbol file beside {}, and a debugger maps the program's lines with one. \
             Set DebugType to portable, which is .NET's default, and build again.",
            built.display()
        ));
    };
    for file in [built, pdb] {
        if !file.is_file() {
            return Err(format!("The build wrote no file at {}, which a debug session reads.", file.display()));
        }
    }
    Ok((built, pdb))
}

/// `path` with `suffix` after its whole file name: `App.debug.lmli` becomes `App.debug.lmli.srcmap.json`,
/// beside it, where a Lamella Link debugger looks for an image's line map.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

/// The image the board `board` runs the assembly at `built` from, and that image's line map, from
/// `symbols`, the assembly's PDB, as [`crate::bake::debug_image_of_built`] makes them.
#[cfg(feature = "bake")]
fn baked_for_debugging(built: &Path, symbols: &[u8], board: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    crate::bake::debug_image_of_built(built, symbols, "build", board, &Folders::default())
}

/// In a build that cannot bake, the sentence that names the missing feature.
#[cfg(not(feature = "bake"))]
fn baked_for_debugging(_built: &Path, _symbols: &[u8], _board: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    Err("lamella build: this lamella cannot write the image a board running Lamella's firmware runs, which a debug \
         session on that board takes.\n\n\
         Baking one needs the tool's `bake` feature, which a released lamella has:\n\
         \x20   cargo build -p lamella-cli --features bake"
        .to_owned())
}

/// MSBuild's `PathMap` as the C# compiler reads it: `from=to` pairs separated by commas, where a
/// doubled comma or equals sign stands for one inside a path; each side ends with a separator. A pair
/// with no `=`, or with nothing on one side, is left out, as the compiler refuses it.
fn path_map(value: &str) -> Vec<(String, String)> {
    split_doubled(value, ',')
        .into_iter()
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| match split_doubled(&pair, '=').as_slice() {
            [from, to] if !from.is_empty() && !to.is_empty() => Some((ending_in_separator(from), ending_in_separator(to))),
            _ => None,
        })
        .collect()
}

/// `value` split at each `separator`, a doubled one standing for itself.
fn split_doubled(value: &str, separator: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut part = String::new();
    let mut chars = value.chars().peekable();
    while let Some(char) = chars.next() {
        if char != separator {
            part.push(char);
        } else if chars.peek() == Some(&separator) {
            chars.next();
            part.push(separator);
        } else {
            parts.push(std::mem::take(&mut part));
        }
    }
    parts.push(part);
    parts
}

/// `path` ending in a separator, as the C# compiler ends each side of a path map: the kind it already
/// uses, when it uses one kind, and else this platform's.
fn ending_in_separator(path: &str) -> String {
    if path.ends_with(['/', '\\']) {
        return path.to_owned();
    }
    let separator = match (path.contains('/'), path.contains('\\')) {
        (true, false) => '/',
        (false, true) => '\\',
        _ => std::path::MAIN_SEPARATOR,
    };
    format!("{path}{separator}")
}

/// The package assemblies a project gets, its packages' own dependencies included, as NuGet makes
/// a package's dependencies visible to the program. `warn` prints the warnings choosing them raised;
/// errors are printed always, in MSBuild's canonical form.
fn resolve(inputs: &Inputs, warn: bool) -> Result<Vec<Resolved>, String> {
    let compiler = LcscCompiler::discover().map_err(|error| format!("lamella: error: {error}"))?;
    let folder = compiler.folder().to_path_buf();
    let offered = compiler.library_names();
    let library = |name: &str| offered.iter().find(|offered| offered.eq_ignore_ascii_case(name)).cloned();
    let (mut resolved, mut failed): (Vec<Resolved>, bool) = (Vec::new(), false);
    for reference in &inputs.packages {
        match crate::packages::select(reference) {
            Ok(selected) => {
                if warn && let Some(warning) = &selected.warning {
                    println!("{}", warning.render(&inputs.project));
                }
                let Some(name) = library(&selected.id) else {
                    println!(
                        "{} : error NU1101: Lamella provides no package {}: the class library at {} has no {}.dll.",
                        inputs.project,
                        selected.id,
                        folder.display(),
                        selected.id
                    );
                    failed = true;
                    continue;
                };
                let path = folder.join(format!("{name}.dll"));
                if !resolved.iter().any(|one| one.path == path) {
                    resolved.push(Resolved { id: selected.id, version: selected.version.to_string(), path });
                }
            }
            Err(error) => {
                println!("{}", error.render(&inputs.project));
                failed = true;
            }
        }
    }
    if failed {
        return Err(String::new());
    }
    for path in &inputs.references {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        let Ok(assembly) = lamella_metadata::Assembly::read(&bytes) else {
            continue;
        };
        for reference in assembly.assembly_refs() {
            let Some(name) = reference.name().and_then(library) else {
                continue;
            };
            let path = folder.join(format!("{name}.dll"));
            if FRAMEWORK.iter().any(|framework| framework.eq_ignore_ascii_case(&name)) || resolved.iter().any(|one| one.path == path) {
                continue;
            }
            let version = crate::packages::provided(&name)
                .map(|provided| provided.version.to_string())
                .unwrap_or_default();
            resolved.push(Resolved { id: name, version, path });
        }
    }
    let mut next = 0;
    while next < resolved.len() {
        let bytes = std::fs::read(&resolved[next].path)
            .map_err(|error| format!("lamella: error: read {}: {error}", resolved[next].path.display()))?;
        next += 1;
        let Ok(assembly) = lamella_metadata::Assembly::read(&bytes) else {
            continue;
        };
        for reference in assembly.assembly_refs() {
            let Some(name) = reference.name().and_then(library) else {
                continue;
            };
            let path = folder.join(format!("{name}.dll"));
            if FRAMEWORK.iter().any(|framework| framework.eq_ignore_ascii_case(&name))
                || resolved.iter().any(|one| one.path == path)
            {
                continue;
            }
            let version = crate::packages::provided(&name)
                .map(|provided| provided.version.to_string())
                .unwrap_or_default();
            resolved.push(Resolved { id: name, version, path });
        }
    }
    Ok(resolved)
}

/// The set a project's program binds against: the corlib, the framework's assemblies, and the
/// assemblies of the packages `resolved` holds, and nothing else.
fn bound_set(resolved: &[Resolved]) -> Result<lamella_wire_host::boards::ReferenceSet, String> {
    bound_set_for(resolved, None)
}

/// [`bound_set`] for an image for `board`, whose chip family's builds stand in for the shared ones.
fn bound_set_for(resolved: &[Resolved], board: Option<&str>) -> Result<lamella_wire_host::boards::ReferenceSet, String> {
    let mut names: Vec<String> = FRAMEWORK.iter().map(|name| (*name).to_owned()).collect();
    names.extend(resolved.iter().map(|one| one.id.clone()));
    lamella_wire_host::boards::ReferenceSet::discover(board)
        .and_then(|set| set.naming(&names))
        .map_err(|error| format!("lamella: error: {error}"))
}

/// The namespaces that hold a public type in `assemblies`, and every namespace enclosing one, as C#
/// finds a namespace: `System.Threading` exists where only `System.Threading.Tasks` holds a type. An
/// assembly that cannot be read holds none.
fn namespaces<'a>(assemblies: impl IntoIterator<Item = &'a [u8]>) -> std::collections::BTreeSet<String> {
    let mut found = std::collections::BTreeSet::new();
    for bytes in assemblies {
        let Ok(assembly) = lamella_metadata::Assembly::read(bytes) else {
            continue;
        };
        for type_def in assembly.type_defs().filter(lamella_metadata::TypeDef::is_public) {
            let Some(mut namespace) = type_def.name().map(|name| name.namespace).filter(|namespace| !namespace.is_empty())
            else {
                continue;
            };
            while found.insert(namespace.to_owned()) {
                let Some(dot) = namespace.rfind('.') else {
                    break;
                };
                namespace = &namespace[..dot];
            }
        }
    }
    found
}

/// Write the project's package assemblies as the MSBuild project the SDK reads, whose target
/// `LamellaReferences` returns one `LamellaReference` per assembly, and whose target
/// `LamellaNamespaces` returns one `LamellaNamespace` per namespace the program can use: those the
/// corlib, the framework's assemblies and the packages' assemblies define ([`namespaces`]). The SDK
/// keeps an implicit `using` only for a namespace among them, as a namespace the build's class
/// library does not provide is left out of the implicit list rather than refused.
fn references(inputs: &Inputs) -> Result<(), String> {
    let resolved = resolve(inputs, true)?;
    let Some(out) = &inputs.references_out else {
        return Err("lamella: error: the build's inputs name no references-out file.".to_owned());
    };
    let set = bound_set(&resolved)?;
    let mut text = String::from(
        "<Project>\n  <!-- Written by the lamella build command: the assemblies of the packages this project references, and the namespaces the program can use. -->\n",
    );
    text.push_str("  <Target Name=\"LamellaReferences\" Returns=\"@(LamellaReference)\">\n    <ItemGroup>\n");
    for one in &resolved {
        let _ = writeln!(
            text,
            "      <LamellaReference Include=\"{}\" PackageId=\"{}\" PackageVersion=\"{}\" />",
            crate::devices::attribute(&one.path.display().to_string()),
            crate::devices::attribute(&one.id),
            crate::devices::attribute(&one.version),
        );
    }
    text.push_str("    </ItemGroup>\n  </Target>\n");
    text.push_str("  <Target Name=\"LamellaNamespaces\" Returns=\"@(LamellaNamespace)\">\n    <ItemGroup>\n");
    for namespace in namespaces(set.compiler().references().iter().map(Vec::as_slice)) {
        let _ = writeln!(text, "      <LamellaNamespace Include=\"{}\" />", crate::devices::attribute(&namespace));
    }
    text.push_str("    </ItemGroup>\n  </Target>\n</Project>\n");
    std::fs::write(out, text).map_err(|error| format!("lamella: error: write {}: {error}", out.display()))
}

/// Compile what MSBuild evaluated into the assembly the SDK names, against the corlib, the
/// framework's assemblies and what the project references, with its symbol file where the SDK names
/// one: a portable PDB that names each source by its full path, as a debugger is given it.
fn compile(inputs: &Inputs) -> Result<(), String> {
    let resolved = resolve(inputs, false)?;
    let Some(out) = &inputs.out else {
        return Err("lamella: error: the build's inputs name no out file.".to_owned());
    };
    let set = bound_set(&resolved)?;
    let project = project_of(inputs, None);
    let libraries = crate::flash::project_references(&project, "build")?;
    let (assembly, symbols) = match &inputs.pdb {
        Some(pdb) => {
            let debuggable = crate::program::compile_project_for_debugging_against(&project, &libraries, &set, "build")?;
            (debuggable.assembly, Some((pdb, debuggable.pdb)))
        }
        None => (crate::program::compile_project_assembly_against(&project, &libraries, &set, "build")?, None),
    };
    for file in std::iter::once(out).chain(symbols.as_ref().map(|(pdb, _)| *pdb)) {
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
    }
    std::fs::write(out, assembly).map_err(|error| format!("lamella: error: write {}: {error}", out.display()))?;
    match symbols {
        Some((pdb, bytes)) => {
            std::fs::write(pdb, bytes).map_err(|error| format!("lamella: error: write {}: {error}", pdb.display()))
        }
        None => Ok(()),
    }
}

/// `lamella run --built <assembly> [--ref-dir <folder>] [--runtime-support-dir <folder>]
/// [--device <id> [--board <id>]] -- [<arguments>]`: run an assembly a build made, here or on the
/// board `--device` names. `local` is this computer, as `dotnet run --list-devices` lists it.
/// `--board` is the board the project names, which the board that answers must be. The two folders
/// are the class library's and the runtime support archives', as `run` takes them
/// ([`crate::folders`]). `program_args`, what followed the `--`, are the program's own arguments:
/// here a `Main(string[] args)` receives them, and a board is not sent the program at all, since a
/// program on a board cannot take arguments.
///
/// **THE BOARD IS CHECKED WHEN IT ANSWERS**, because the listing does not ask it: an id is a USB
/// identity, and a probe's bridge can carry any board.
pub fn run_built(options: &[String], program_args: &[String]) -> Ended {
    use crate::own_lines::{blank_line, fail, running_interpreted, say};
    let mut err = std::io::stderr();
    let (mut built, mut device, mut board) = (None, None, None);
    let (mut ref_dir, mut runtime_support_dir) = (None, None);
    let mut given = options.iter();
    while let Some(option) = given.next() {
        let slot = match option.as_str() {
            BUILT => &mut built,
            "--device" => &mut device,
            "--board" => &mut board,
            REF_DIR => &mut ref_dir,
            RUNTIME_SUPPORT_DIR => &mut runtime_support_dir,
            other => {
                fail(&mut err, &format!(
                    "{BUILT} takes {BUILT} <assembly>, {REF_DIR} <folder>, \
                     {RUNTIME_SUPPORT_DIR} <folder>, --device <device> and --board <board>; {other:?} is none \
                     of them."
                ));
                return Ended::Failed;
            }
        };
        let Some(value) = given.next() else {
            fail(&mut err, &format!("{option} wants a value."));
            return Ended::Failed;
        };
        if slot.is_some() {
            fail(&mut err, &format!("{option} is given more than once."));
            return Ended::Failed;
        }
        *slot = Some(value.as_str());
    }
    let folders = match Folders::checked("run", ref_dir, runtime_support_dir) {
        Ok(folders) => folders,
        Err(refusal) => {
            fail(&mut err, &refusal);
            return Ended::Failed;
        }
    };
    let Some(built) = built.map(PathBuf::from) else {
        fail(&mut err, &format!("{BUILT} wants the assembly a build made."));
        return Ended::Failed;
    };
    let board_id = board;
    let board = match board.map(|id| crate::project::BoardCheck::for_board("run", id)).transpose() {
        Ok(board) => board,
        Err(refusal) => {
            fail(&mut err, &refusal);
            return Ended::Failed;
        }
    };
    let here = device.is_none_or(|device| device.eq_ignore_ascii_case("local"));
    if !here && crate::program::refuses_arguments_on_a_board(program_args) {
        return Ended::Failed;
    }
    match device {
        Some(device) if !here => crate::program::run_built_on_target(
            &built,
            device,
            board.as_ref(),
            |answered| model_line(board_id, answered),
            &folders,
        ),
        _ => {
            let mut introduction = err.lock();
            let _interrupt = crate::ending::watch_local_run();
            say(&mut introduction, &running_interpreted("this computer"));
            blank_line(&mut introduction);
            drop(introduction);
            run_here(&built, program_args, &folders)
        }
    }
}

/// The first line of a run of a built assembly on a board running Lamella's firmware, once the board
/// has answered a HELLO as product model `answered`, using its human name. The project's board
/// supplies a name only when the firmware's product model has none.
fn model_line(board: Option<&str>, answered: u16) -> String {
    let name = lamella_wire_host::board_name(answered).map(str::to_owned)
        .or_else(|| board.map(crate::own_lines::board_name))
        .unwrap_or_else(|| crate::devices::answered_as(answered));
    crate::own_lines::running_interpreted(&name)
}

/// The assembly at `path`, run on this computer under Lamella's interpreter, against the corlib of
/// the class library `folders` names, or else of the one the tool finds without it.
///
/// **THIS COMPUTER RUNS A PROGRAM AGAINST THE CORLIB ALONE**, so a program that references any
/// other assembly -- a package's, `System.Net.NetworkInformation`, a library of its own -- is
/// refused before it starts, naming them, rather than stopping at its first call into one with a
/// trap that names a metadata token.
fn run_here(path: &Path, program_args: &[String], folders: &Folders) -> Ended {
    use crate::own_lines::fail;
    let mut err = std::io::stderr();
    let assembly = match std::fs::read(path) {
        Ok(assembly) => assembly,
        Err(error) => {
            fail(&mut err, &format!("Could not read \"{}\": {error}", path.display()));
            return Ended::Failed;
        }
    };
    let corlib = match folders.compiler().map(|compiler| compiler.references().first().cloned()) {
        Ok(Some(corlib)) => corlib,
        Ok(None) => {
            fail(&mut err, "The class library holds no corlib.");
            return Ended::Failed;
        }
        Err(error) => {
            fail(&mut err, &error);
            return Ended::Failed;
        }
    };
    let beyond = references_beyond(&assembly, &corlib);
    if !beyond.is_empty() {
        fail(&mut err, &runs_against_the_corlib_alone(path, &beyond));
        return Ended::Failed;
    }
    crate::program::report(crate::program::run_here(corlib, &assembly, program_args))
}

/// The names of the assemblies `assembly` references besides `corlib`, by either of the corlib's
/// names ([`names_the_corlib`]), in the order it lists them. An assembly that cannot be read
/// references nothing here, and its run says why.
///
/// [`names_the_corlib`]: lamella_wire_host::refusal::names_the_corlib
fn references_beyond(assembly: &[u8], corlib: &[u8]) -> Vec<String> {
    let (Ok(program), Ok(corlib)) = (lamella_metadata::Assembly::read(assembly), lamella_metadata::Assembly::read(corlib))
    else {
        return Vec::new();
    };
    program
        .assembly_refs()
        .filter_map(|reference| reference.name())
        .filter(|name| !lamella_wire_host::refusal::names_the_corlib(name, &corlib))
        .map(str::to_owned)
        .collect()
}

/// Why the program at `path`, which references the assemblies `beyond`, is not run on this computer.
fn runs_against_the_corlib_alone(path: &Path, beyond: &[String]) -> String {
    let file = path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned());
    let named = match beyond {
        [one] => one.clone(),
        [first @ .., last] => format!("{} and {last}", first.join(", ")),
        [] => String::new(),
    };
    format!(
        "lamella run: \"{file}\" references {named}, and on this computer a program runs against the core \
         library alone. Run it on a board: dotnet run --list-devices lists the connected boards, and \
         dotnet run --device <device> runs it on one."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local refusal keeps the commands for running a library-dependent program on a board.
    #[test]
    fn a_run_refusal_keeps_the_board_run_instructions() {
        let refusal = runs_against_the_corlib_alone(Path::new("App.dll"), &["Sensors".to_owned()]);
        let mut err = Vec::new();
        crate::own_lines::fail(&mut err, &refusal);
        let shown = String::from_utf8(err).unwrap();
        assert!(shown.starts_with("\"App.dll\" references Sensors"), "{shown}");
        assert!(shown.contains("dotnet run --list-devices"), "{shown}");
        assert!(shown.contains("dotnet run --device <device> runs it on one."), "{shown}");
    }

    /// **A RUN ON A BOARD SAYS FIRST WHICH BOARD IT RUNS ON, UNDER THE INTERPRETER**: the project's
    /// board where it names one, and where it does not, the board that answered, as its firmware names
    /// it, never the device's ID. A product model this build does not know is said as `lamella devices
    /// --identify` says it.
    #[test]
    fn a_runs_first_line_names_the_board_it_runs_on() {
        use lamella_wire::product_model::{PICO2, PICO2_W};
        let version = env!("CARGO_PKG_VERSION");
        assert_eq!(model_line(None, PICO2_W), format!("Lamella v{version}, Raspberry Pi Pico 2 W (C# interpreter)"));
        assert_eq!(
            model_line(None, 0x7fff),
            format!("Lamella v{version}, a board this build does not know (product model 32767) (C# interpreter)")
        );
        assert_eq!(model_line(Some("raspberrypi-pico-2"), PICO2), format!("Lamella v{version}, Raspberry Pi Pico 2 (C# interpreter)"));
    }

    /// The SDK's `langversion=` line selects the version as the project reader does, and reaches the
    /// project the compile is given. An empty value leaves the default, and a value naming no version
    /// the compiler can select is refused with the project named.
    #[test]
    fn the_inputs_langversion_line_selects_the_version() {
        use lamella_syntax::version::LanguageVersion;
        let dir = std::env::temp_dir().join(format!("lamella-msbuild-langversion-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let file = dir.join("lamella.compile.txt");
        let read = |value: &str| {
            std::fs::write(&file, format!("project=C:\\app\\App.csproj\nlangversion={value}\n")).expect("the inputs");
            Inputs::read(&file)
        };
        assert_eq!(read("10").unwrap().lang_version, Some(LanguageVersion::CSharp10));
        assert_eq!(project_of(&read("ISO-1").unwrap(), None).lang_version, Some(LanguageVersion::CSharp1));
        assert_eq!(read("").unwrap().lang_version, None);
        let refused = read("14").unwrap_err();
        assert!(
            refused.contains("C:\\app\\App.csproj: LangVersion is \"14\", a C# version this compiler cannot select yet"),
            "{refused}"
        );
        // Read once every line is in: a refusal names the project wherever its line comes, and the last
        // `langversion=` line is the one in effect, an empty one leaving the default.
        std::fs::write(&file, "langversion=banana\nproject=C:\\app\\App.csproj\n").expect("the inputs");
        let early = Inputs::read(&file).unwrap_err();
        assert!(early.contains("C:\\app\\App.csproj: LangVersion is \"banana\""), "{early}");
        std::fs::write(&file, "project=C:\\app\\App.csproj\nlangversion=1\nlangversion=\n").expect("the inputs");
        assert_eq!(Inputs::read(&file).unwrap().lang_version, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_inputs_file_is_read_key_by_key() {
        let dir = std::env::temp_dir().join(format!("lamella-msbuild-inputs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let file = dir.join("lamella.inputs.txt");
        std::fs::write(
            &file,
            "project=C:\\app\\App.csproj\npackage=System.Device.Gpio@[4.2.0, 5.0.0)\npackage=Lamella.Hardware\n\
             source=C:\\app\\Program.cs\ndefine=DEBUG\ndefine=\nallowunsafe=True\nunknown=kept quiet\n",
        )
        .expect("write");
        let inputs = Inputs::read(&file).expect("read");
        assert_eq!(inputs.project, "C:\\app\\App.csproj");
        assert_eq!(inputs.packages[0].version.as_deref(), Some("[4.2.0, 5.0.0)"));
        assert_eq!(inputs.packages[1].version, None);
        assert_eq!(inputs.defines, ["DEBUG"]);
        assert!(inputs.allow_unsafe);
        assert!(!inputs.file_based, "no features line is a project's compile");
        std::fs::write(&file, "project=C:\\app\\app.csproj\nfeatures=;FileBasedProgram\n").expect("write");
        assert!(Inputs::read(&file).expect("read").file_based, "the line dotnet run app.cs writes");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A PUBLISH's LINES ARE READ BESIDE THE COMPILE's**, and one that names no board or no known
    /// format is refused before anything is compiled.
    #[test]
    fn a_publish_names_its_board_its_format_and_its_files() {
        let dir = std::env::temp_dir().join(format!("lamella-msbuild-publish-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let file = dir.join("lamella.publish.txt");
        let read = |text: &str| {
            std::fs::write(&file, text).expect("write the inputs");
            Inputs::read(&file).expect("read")
        };
        let inputs = read("project=C:\\app\\App.csproj\nboard=raspberrypi-pico-2\nformat=elf\nbuilt=C:\\app\\bin\\App.dll\nimage=C:\\app\\obj\\App.elf\n");
        assert_eq!(inputs.board.as_deref(), Some("raspberrypi-pico-2"));
        assert_eq!(inputs.format.as_deref(), Some("elf"));
        assert_eq!(inputs.built, Some(PathBuf::from("C:\\app\\bin\\App.dll")));
        assert_eq!(inputs.image, Some(PathBuf::from("C:\\app\\obj\\App.elf")));
        let boardless = publish(&read("project=C:\\app\\App.csproj\nboard=\nformat=elf\nimage=C:\\app\\obj\\App.elf\n"))
            .expect_err("no board");
        assert!(boardless.contains("name no board"), "{boardless}");
        let other = publish(&read(&format!("project=C:\\app\\App.csproj\nboard=raspberrypi-pico-2\nformat=hex\nimage={}\n", dir.join("App.hex").display())))
            .expect_err("no such format");
        assert!(other.contains("\"hex\"") && other.contains("elf or lmli"), "{other}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A NATIVE PUBLISH NAMES ITS BUILD KIND**, as the project's `Optimize`, and one whose inputs name none is
    /// refused before anything is compiled.
    #[test]
    fn a_native_publish_names_its_build_kind() {
        let dir = std::env::temp_dir().join(format!("lamella-msbuild-kind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let file = dir.join("lamella.publish.txt");
        let read = |text: &str| {
            std::fs::write(&file, text).expect("write the inputs");
            Inputs::read(&file).expect("read")
        };
        let image = dir.join("App.elf");
        let kindless = publish(&read(&format!("project=C:\\app\\App.csproj\nboard=raspberrypi-pico-2\nformat=elf\nimage={}\n", image.display())))
            .expect_err("no build kind");
        assert!(kindless.contains("name no build kind"), "{kindless}");
        for (optimize, kind) in [("true", BuildKind::Release), ("True", BuildKind::Release), ("false", BuildKind::Debug), ("", BuildKind::Debug)] {
            assert_eq!(read(&format!("project=C:\\app\\App.csproj\noptimize={optimize}\n")).build_kind(), Ok(kind), "optimize={optimize}");
        }
        let odd = read("project=C:\\app\\App.csproj\noptimize=yes\n").build_kind().expect_err("neither true nor false");
        assert!(odd.contains("\"yes\"") && odd.contains("true or false"), "{odd}");
        let given = publish(&read(&format!("project=C:\\app\\App.csproj\nboard=raspberrypi-pico-2\nformat=elf\noptimize=false\nimage={}\n", image.display())))
            .expect_err("a project with nothing in it");
        assert!(!given.contains("build kind"), "a publish that names its kind goes on to the compile: {given}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A REFUSAL REACHES MSBuild AS AN ERROR**: a sentence worded for a reader becomes one line in
    /// MSBuild's canonical form, its first paragraph joined, the rest after it; one already in that
    /// form, as the compiler's diagnostics are, is left as it is.
    #[test]
    fn a_publish_refusal_is_an_msbuild_error() {
        assert_eq!(
            as_msbuild_error("C:\\app\\App.csproj", "lamella publish: nothing here names a target for x, so\nthis verb has nothing.\n\nMore."),
            "C:\\app\\App.csproj : error : lamella publish: nothing here names a target for x, so this verb has nothing.\n\nMore."
        );
        assert_eq!(as_msbuild_error("App.csproj", "one line"), "App.csproj : error : one line");
        for canonical in ["lamella: error: the publish's inputs name no board.", "Program.cs(1,2): error CS1002: ; expected"] {
            assert_eq!(as_msbuild_error("App.csproj", canonical), canonical);
        }
    }

    /// **A PATH MAP IS READ AS THE C# COMPILER READS ONE**: pairs separated by commas, `from=to`, a
    /// doubled separator standing for one inside a path, and each side ending with a separator of the
    /// kind it already uses, so a prefix match cannot take `App2` for `App`.
    #[test]
    fn a_path_map_is_read_as_the_compiler_reads_it() {
        assert_eq!(
            path_map("C:\\w\\App\\obj\\Debug=/rhu/obj/App,C:\\w\\App=/rhu/src/App"),
            [("C:\\w\\App\\obj\\Debug\\".to_owned(), "/rhu/obj/App/".to_owned()), ("C:\\w\\App\\".to_owned(), "/rhu/src/App/".to_owned())]
        );
        assert_eq!(path_map("/w/a,,b/App/=/rhu/src/App"), [("/w/a,b/App/".to_owned(), "/rhu/src/App/".to_owned())]);
        assert_eq!(path_map("/x==y=/z"), [("/x=y/".to_owned(), "/z/".to_owned())]);
        assert!(path_map("").is_empty());
        assert!(path_map(",no-equals,=/empty-from,/empty-to=").is_empty(), "malformed pairs are left out, as the compiler refuses them");
    }

    /// **A NATIVE IMAGE's MANIFEST NAMES WHAT A PROBE SESSION NEEDS**: the published ELF and nothing
    /// for symbols, since the ELF carries them, the board, the device as given, the path map and the
    /// debugger, every path absolute; and an image the build did not publish, or published without its
    /// record, is refused before anything is written.
    #[test]
    fn a_native_images_manifest_names_the_published_elf() {
        let dir = std::env::temp_dir().join(format!("lamella-msbuild-debug-native-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("publish")).expect("a scratch directory");
        let (elf, out, file) = (dir.join("publish").join("App.elf"), dir.join("debug.json"), dir.join("lamella.debug.txt"));
        let write = |text: &str| {
            std::fs::write(&file, text).expect("write the inputs");
            debug_manifest(&Inputs::read(&file).expect("read"))
        };
        let inputs = format!(
            "project={}\nframework=net11.0\nkind=native\ndevice=probe:E6640000000000A1\nboard=raspberrypi-pico-2-w\n\
             built={}\nimage={}\npathmap={}=/rhu/src/App\ndebugger={}\nmanifest={}\n",
            dir.join("App.csproj").display(),
            dir.join("App.dll").display(),
            elf.display(),
            dir.display(),
            dir.join("lamella-debug.exe").display(),
            out.display(),
        );
        let missing = write(&inputs).expect_err("no image was published");
        assert!(missing.contains(&elf.display().to_string()), "{missing}");
        assert!(!out.exists(), "nothing written");
        std::fs::write(&elf, b"\x7fELF").expect("an image");
        let unrecorded = write(&inputs).expect_err("no record beside the image");
        assert!(unrecorded.contains("App.elf.manifest.json"), "{unrecorded}");
        std::fs::write(dir.join("publish").join("App.elf.manifest.json"), "{}").expect("a record");
        write(&inputs).expect("written");
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).expect("the manifest")).expect("JSON");
        assert_eq!(manifest["format"], 1);
        assert_eq!(manifest["framework"], "net11.0");
        assert_eq!(manifest["kind"], "native");
        assert_eq!(manifest["image"], elf.display().to_string());
        assert!(manifest.get("symbols").is_none(), "the ELF carries its debug information: {manifest}");
        assert_eq!(manifest["board"], "raspberrypi-pico-2-w");
        assert_eq!(manifest["device"], "probe:E6640000000000A1");
        assert_eq!(manifest["debugger"], dir.join("lamella-debug.exe").display().to_string());
        let separator = std::path::MAIN_SEPARATOR;
        assert_eq!(manifest["pathMap"], serde_json::json!([{ "from": format!("{}{separator}", dir.display()), "to": "/rhu/src/App/" }]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **EVERY KIND THE SDK HANDS OVER IS ONE THIS TOOL WRITES A MANIFEST FOR.** The kinds are read from
    /// the SDK's own targets, so a kind the SDK writes and this tool does not take fails here.
    #[test]
    fn every_kind_the_sdk_hands_over_is_one_the_tool_takes() {
        let targets = include_str!("../../../csharp/sdk/Lamella.Sdk/Sdk/Lamella.Debug.targets");
        let pieces: Vec<&str> = targets.split("</_LamellaDebugKind>").collect();
        let kinds: Vec<&str> = pieces[..pieces.len() - 1]
            .iter()
            .filter_map(|before| before.rsplit('>').next())
            .collect();
        assert!(kinds.contains(&"host") && kinds.contains(&"native"), "the kinds read from the targets: {kinds:?}");
        let dir = std::env::temp_dir().join(format!("lamella-msbuild-debug-kinds-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let file = dir.join("lamella.debug.txt");
        for kind in &kinds {
            std::fs::write(
                &file,
                format!(
                    "project={}\nframework=lamella1.0\nkind={kind}\ndevice=local\nbuilt={}\ndebugger={}\nmanifest={}\n",
                    dir.join("App.csproj").display(),
                    dir.join("App.dll").display(),
                    dir.join("lamella-debug.exe").display(),
                    dir.join("debug.json").display(),
                ),
            )
            .expect("write the inputs");
            // Each kind is refused here for a file it lacks; what matters is that none is refused for its kind.
            if let Err(refusal) = debug_manifest(&Inputs::read(&file).expect("read")) {
                assert!(!refusal.contains("name the kind"), "the SDK hands over the kind {kind}, and this tool refuses it: {refusal}");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A RUN ON THIS COMPUTER's MANIFEST NAMES THE ASSEMBLY, ITS SYMBOLS AND THE CLASS LIBRARY**, and
    /// a build that wrote no symbol file is refused in words that say how to get one.
    #[test]
    fn a_host_manifest_names_the_assembly_and_its_symbols() {
        let dir = std::env::temp_dir().join(format!("lamella-msbuild-debug-host-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).expect("a scratch directory");
        let (dll, pdb, out, file) = (dir.join("bin").join("App.dll"), dir.join("bin").join("App.pdb"), dir.join("debug.json"), dir.join("lamella.debug.txt"));
        std::fs::write(&dll, b"MZ").expect("an assembly");
        let inputs = |with_pdb: bool| {
            format!(
                "project={}\nframework=lamella1.0\nkind=host\ndevice=local\nbuilt={}\n{}debugger={}\nmanifest={}\n",
                dir.join("App.csproj").display(),
                dll.display(),
                if with_pdb { format!("pdb={}\n", pdb.display()) } else { String::new() },
                dir.join("lamella-debug.exe").display(),
                out.display(),
            )
        };
        let write = |text: String| {
            std::fs::write(&file, text).expect("write the inputs");
            debug_manifest(&Inputs::read(&file).expect("read"))
        };
        let no_symbols = write(inputs(false)).expect_err("no symbol file");
        assert!(no_symbols.contains("DebugType"), "{no_symbols}");
        let unwritten = write(inputs(true)).expect_err("the PDB is named and absent");
        assert!(unwritten.contains("App.pdb"), "{unwritten}");
        std::fs::write(&pdb, b"BSJB").expect("a symbol file");
        if Folders::default().compiler().is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        write(inputs(true)).expect("the manifest is written");
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).expect("the manifest")).expect("JSON");
        assert_eq!(manifest["kind"], "host");
        assert_eq!(manifest["image"], dll.display().to_string());
        assert_eq!(manifest["symbols"], pdb.display().to_string());
        assert_eq!(manifest["device"], "local");
        assert!(manifest["references"].as_str().is_some_and(|folder| std::path::Path::new(folder).join("corlib.dll").is_file()), "{manifest}");
        assert!(manifest.get("board").is_none() && manifest.get("pathMap").is_none(), "{manifest}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **EVERY PATH IN THE MANIFEST IS ABSOLUTE WITH NO `.` OR `..` IN IT**, whatever the inputs file
    /// says, on every system: the debugger compares them with the paths an editor sends.
    #[test]
    fn a_manifests_paths_have_no_dot_segments() {
        let dir = std::env::temp_dir().join(format!("lamella-msbuild-debug-dots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).expect("a scratch directory");
        std::fs::create_dir_all(dir.join("sub")).expect("a scratch directory");
        std::fs::write(dir.join("bin").join("App.dll"), b"MZ").expect("an assembly");
        std::fs::write(dir.join("bin").join("App.pdb"), b"BSJB").expect("a symbol file");
        if Folders::default().compiler().is_err() {
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }
        let (out, file) = (dir.join("debug.json"), dir.join("lamella.debug.txt"));
        let roundabout = dir.join("sub").join("..");
        std::fs::write(
            &file,
            format!(
                "project={}\nframework=lamella1.0\nkind=host\ndevice=local\nbuilt={}\npdb={}\ndebugger={}\nmanifest={}\n",
                roundabout.join("App.csproj").display(),
                roundabout.join("bin").join("App.dll").display(),
                roundabout.join("bin").join(".").join("App.pdb").display(),
                roundabout.join("lamella-debug.exe").display(),
                out.display(),
            ),
        )
        .expect("write the inputs");
        debug_manifest(&Inputs::read(&file).expect("read")).expect("the manifest is written");
        let manifest: serde_json::Value = serde_json::from_slice(&std::fs::read(&out).expect("the manifest")).expect("JSON");
        let clean = |key: &str| {
            let path = std::path::PathBuf::from(manifest[key].as_str().unwrap_or_else(|| panic!("no {key}: {manifest}")));
            assert!(path.is_absolute(), "{key}: {}", path.display());
            assert!(
                !path.components().any(|part| matches!(part, std::path::Component::CurDir | std::path::Component::ParentDir)),
                "{key} keeps a . or ..: {}",
                path.display()
            );
            path
        };
        assert_eq!(clean("image"), dir.join("bin").join("App.dll"));
        assert_eq!(clean("symbols"), dir.join("bin").join("App.pdb"));
        assert_eq!(clean("debugger"), dir.join("lamella-debug.exe"));
        clean("references");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **`Features` IS READ AS THE C# COMPILER READS IT**: names separated by semicolons, commas or
    /// spaces, each a name or `name=value`, so `FileBasedProgram` is found among others and a
    /// feature that only begins with the name is not it.
    #[test]
    fn file_based_mode_is_the_feature_file_based_program() {
        for on in [";FileBasedProgram", "FileBasedProgram", "strict;FileBasedProgram", "a,FileBasedProgram b", "FileBasedProgram=true"] {
            assert!(names_file_based(on), "{on:?}");
        }
        for off in ["", ";", "strict", "FileBasedProgramX", "NotFileBasedProgram"] {
            assert!(!names_file_based(off), "{off:?}");
        }
    }
}

