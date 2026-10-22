//! What `Lamella.Sdk` hands this tool from `dotnet build` and `dotnet run`, and what it hands back.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lamella_wire_host::engine::LcscCompiler;

use crate::ending::Ended;
use crate::packages::PackageReference;

/// `lamella build`'s option that answers which assemblies a project's packages are, for MSBuild.
pub(crate) const REFERENCES: &str = "--msbuild-references";

/// `lamella build`'s option that compiles what MSBuild evaluated.
pub(crate) const COMPILE: &str = "--msbuild-compile";

/// `lamella run`'s option naming an assembly a build already made.
pub(crate) const BUILT: &str = "--built";

/// The framework's assemblies beside the corlib: every program may use them and none names them,
/// as a .NET program never names `System.Net.NetworkInformation`.
const FRAMEWORK: &[&str] = &["System.Net.NetworkInformation"];

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
    /// The assembly's name.
    assembly_name: Option<String>,
    /// Where the assembly goes.
    out: Option<PathBuf>,
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
                "assemblyname" if !value.is_empty() => inputs.assembly_name = Some(value.to_owned()),
                "out" => inputs.out = Some(PathBuf::from(value)),
                _ => {}
            }
        }
        Ok(inputs)
    }
}

/// A package's assembly in the class library, chosen for a project.
struct Resolved {
    id: String,
    version: String,
    path: PathBuf,
}

/// `lamella build --msbuild-references <inputs>` and `lamella build --msbuild-compile <inputs>`.
pub fn build(args: &[String]) -> ExitCode {
    let [option, inputs] = args else {
        eprintln!("lamella: error: {REFERENCES} and {COMPILE} each take the build's inputs file, alone.");
        return ExitCode::FAILURE;
    };
    let inputs = match Inputs::read(Path::new(inputs)) {
        Ok(inputs) => inputs,
        Err(error) => {
            eprintln!("{error}");
            return ExitCode::FAILURE;
        }
    };
    let outcome = if option == REFERENCES { references(&inputs) } else { compile(&inputs) };
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
                .and_then(|(_, versions)| versions.first().map(ToString::to_string))
                .unwrap_or_default();
            resolved.push(Resolved { id: name, version, path });
        }
    }
    Ok(resolved)
}

/// Write the project's package assemblies as the MSBuild project the SDK reads, whose target
/// `LamellaReferences` returns one `LamellaReference` per assembly.
fn references(inputs: &Inputs) -> Result<(), String> {
    let resolved = resolve(inputs, true)?;
    let Some(out) = &inputs.references_out else {
        return Err("lamella: error: the build's inputs name no references-out file.".to_owned());
    };
    let mut text = String::from(
        "<Project>\n  <!-- Written by the lamella build command: the assemblies of the packages this project references. -->\n",
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
    let mut names: Vec<String> = FRAMEWORK.iter().map(|name| (*name).to_owned()).collect();
    names.extend(resolved.iter().map(|one| one.id.clone()));
    let set = lamella_wire_host::boards::ReferenceSet::discover(None)
        .and_then(|set| set.naming(&names))
        .map_err(|error| format!("lamella: error: {error}"))?;
    let project_path = Path::new(&inputs.project);
    let assembly_name = inputs.assembly_name.clone().unwrap_or_else(|| {
        project_path.file_stem().and_then(|stem| stem.to_str()).unwrap_or("Program").to_owned()
    });
    let project = crate::project::Project {
        output_type: crate::project::OutputType::Exe,
        assembly_name,
        lang_version: None,
        allow_unsafe: inputs.allow_unsafe,
        references: inputs.references.clone(),
        sources: inputs.sources.clone(),
        board: None,
        packages: Vec::new(),
        package_warnings: Vec::new(),
        defines: inputs.defines.clone(),
    };
    let libraries = crate::flash::project_references(&project, "build")?;
    let assembly = crate::program::compile_project_assembly_against(&project, &libraries, &set, "build")?;
    if let Some(parent) = out.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(out, assembly).map_err(|error| format!("lamella: error: write {}: {error}", out.display()))
}

/// `lamella run --built <assembly> [--device <id> [--board <id>]] -- [<arguments>]`: run an assembly
/// a build made, here or on the board `--device` names. `local` is this computer, as
/// `dotnet run --list-devices` lists it. `--board` is the board the project names, which the board
/// that answers must be. `program_args`, what followed the `--`, are the program's own arguments:
/// here a `Main(string[] args)` receives them, and a board is not sent the program at all, since a
/// program on a board cannot take arguments yet.
///
/// **THE BOARD IS CHECKED WHEN IT ANSWERS**, because the listing does not ask it: an id is a USB
/// identity, and a probe's bridge can carry any board.
pub fn run_built(options: &[String], program_args: &[String]) -> Ended {
    let (mut built, mut device, mut board) = (None, None, None);
    let mut given = options.iter();
    while let Some(option) = given.next() {
        match option.as_str() {
            BUILT => built = given.next(),
            "--device" => device = given.next(),
            "--board" => board = given.next(),
            other => {
                eprintln!(
                    "lamella: error: {BUILT} takes {BUILT} <assembly>, --device <id> and --board <id>; \
                     {other:?} is none of them."
                );
                return Ended::Failed;
            }
        }
    }
    let Some(built) = built.map(PathBuf::from) else {
        eprintln!("lamella: error: {BUILT} wants the assembly a build made.");
        return Ended::Failed;
    };
    let board = match board.map(|id| crate::project::BoardCheck::for_board("run", id)).transpose() {
        Ok(board) => board,
        Err(refusal) => {
            eprintln!("{refusal}");
            return Ended::Failed;
        }
    };
    let here = device.is_none_or(|device| device.eq_ignore_ascii_case("local"));
    if !here && crate::program::refuses_arguments_on_a_board(program_args) {
        return Ended::Failed;
    }
    match device {
        Some(device) if !here => crate::program::run_built_on_target(&built, device, board.as_ref()),
        _ => run_here(&built, program_args),
    }
}

/// The assembly at `path`, run on this computer under Lamella's interpreter.
///
/// **THIS COMPUTER RUNS A PROGRAM AGAINST THE CORLIB ALONE**, so a program that references any
/// other assembly -- a package's, `System.Net.NetworkInformation`, a library of its own -- is
/// refused before it starts, naming them, rather than stopping at its first call into one with a
/// trap that names a metadata token.
fn run_here(path: &Path, program_args: &[String]) -> Ended {
    let assembly = match std::fs::read(path) {
        Ok(assembly) => assembly,
        Err(error) => {
            eprintln!("lamella: error: read {}: {error}", path.display());
            return Ended::Failed;
        }
    };
    let corlib = match LcscCompiler::discover().map(|compiler| compiler.references().first().cloned()) {
        Ok(Some(corlib)) => corlib,
        Ok(None) => {
            eprintln!("lamella: error: the class library holds no corlib");
            return Ended::Failed;
        }
        Err(error) => {
            eprintln!("lamella: error: {error}");
            return Ended::Failed;
        }
    };
    let beyond = references_beyond(&assembly, &corlib);
    if !beyond.is_empty() {
        eprintln!("{}", runs_against_the_corlib_alone(path, &beyond));
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
        "lamella run: {file} references {named}, and on this computer a program runs against the core \
         library alone. Run it on a board: dotnet run --list-devices lists the connected boards, and \
         dotnet run --device <id> runs it on one."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A RUN HERE NAMES WHAT THE CORLIB ALONE CANNOT ANSWER**, and never the corlib, by either of
    /// its names: the class library's own assemblies reference it as `mscorlib` and as `corlib`.
    #[test]
    fn a_run_here_names_the_assemblies_beyond_the_corlib() {
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("../lamella-load/tests/fixtures");
        let read = |name: &str| std::fs::read(fixtures.join(name)).expect("a fixture");
        let corlib = read("corlib.dll");
        assert!(references_beyond(&corlib, &corlib).is_empty());
        assert!(references_beyond(&read("System.Device.Gpio.dll"), &corlib).is_empty());
        assert_eq!(references_beyond(&read("Lamella.Hardware.dll"), &corlib), ["System.Device.Analog"]);

        let said = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
            runs_against_the_corlib_alone(Path::new("bin/Debug/lamella1.0/App.dll"), &names)
        };
        assert!(said(&["System.Device.Gpio"]).starts_with("lamella run: App.dll references System.Device.Gpio, and"));
        assert!(said(&["A", "B"]).contains("references A and B, and"));
        assert!(said(&["A", "B", "C"]).contains("references A, B and C, and"));
        crate::rendered::assert_renders_cleanly(&said(&["System.Device.Gpio"]), crate::rendered::four_space_sample);
    }
}
