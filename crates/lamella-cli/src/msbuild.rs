//! What `Lamella.Sdk` hands this tool from `dotnet build` and `dotnet run`, and what it hands back.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

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
    /// Whether the sources are a file-based app's: MSBuild's `Features` names
    /// [`FILE_BASED_PROGRAM`].
    file_based: bool,
    /// The assembly's name.
    assembly_name: Option<String>,
    /// Where the assembly goes.
    out: Option<PathBuf>,
    /// The board the project names, `LamellaTargetBoard`.
    board: Option<String>,
    /// What a publish writes: `elf`, the native image, or `lmli`, the image Lamella's firmware runs.
    format: Option<String>,
    /// The assembly the build made, beside the libraries it copied there, which a publish bakes.
    built: Option<PathBuf>,
    /// Where a publish writes its image.
    image: Option<PathBuf>,
}

impl Inputs {
    /// The file at `path`.
    fn read(path: &Path) -> Result<Self, String> {
        let text = crate::text::read_text(path)
            .map_err(|error| format!("lamella: error: read the build's inputs {}: {error}", path.display()))?;
        let mut inputs = Self::default();
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
                "features" => inputs.file_based |= names_file_based(value),
                "assemblyname" if !value.is_empty() => inputs.assembly_name = Some(value.to_owned()),
                "out" => inputs.out = Some(PathBuf::from(value)),
                "board" if !value.is_empty() => inputs.board = Some(value.to_owned()),
                "format" => inputs.format = Some(value.to_owned()),
                "built" => inputs.built = Some(PathBuf::from(value)),
                "image" => inputs.image = Some(PathBuf::from(value)),
                _ => {}
            }
        }
        Ok(inputs)
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
            "lamella: error: {REFERENCES}, {COMPILE} and {PUBLISH} each take the build's inputs file, alone, \
             and {PACKAGES} the file it writes."
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

/// `refusal` as MSBuild shows an error: a sentence the tool worded for a reader, which `Exec` would
/// pass on as a message beside its own "exited with code 1", becomes one line in MSBuild's
/// canonical form, `<project> : error : <its first paragraph>`, with the rest after it. A refusal
/// already in that form, as the compiler's diagnostics are, is left as it is.
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
        lang_version: None,
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
            let resolved = resolve(inputs, false)?;
            let set = bound_set_for(&resolved, Some(board))?;
            let project = project_of(inputs, Some(board));
            let libraries = crate::flash::project_references(&project, "publish")?;
            let debuggable =
                crate::program::compile_project_for_debugging_against(&project, &libraries, &set, "publish")?;
            let elf = crate::flash::debug_elf_of(&debuggable, board, "publish", Path::new(&inputs.project), &folders)?;
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
/// framework's assemblies and what the project references.
fn compile(inputs: &Inputs) -> Result<(), String> {
    let resolved = resolve(inputs, false)?;
    let Some(out) = &inputs.out else {
        return Err("lamella: error: the build's inputs name no out file.".to_owned());
    };
    let set = bound_set(&resolved)?;
    let project = project_of(inputs, None);
    let libraries = crate::flash::project_references(&project, "build")?;
    let assembly = crate::program::compile_project_assembly_against(&project, &libraries, &set, "build")?;
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(out, assembly).map_err(|error| format!("lamella: error: write {}: {error}", out.display()))
}

/// `lamella run --built <assembly> [--ref-dir <folder>] [--runtime-support-dir <folder>]
/// [--device <id> [--board <id>]] -- [<arguments>]`: run an assembly a build made, here or on the
/// board `--device` names. `local` is this computer, as `dotnet run --list-devices` lists it.
/// `--board` is the board the project names, which the board that answers must be. The two folders
/// are the class library's and the runtime support archives', as `run` takes them
/// ([`crate::folders`]). `program_args`, what followed the `--`, are the program's own arguments:
/// here a `Main(string[] args)` receives them, and a board is not sent the program at all, since a
/// program on a board cannot take arguments yet.
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

