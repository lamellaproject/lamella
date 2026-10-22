//! Baking a compiled assembly into the flash image a device runs.

use lamella_wire_host::bake as shared;
use lamella_wire_host::boards::ReferenceSet;

/// What a refusal tells a reader whose program calls into an assembly its image cannot carry: the
/// image carries only what the program was compiled against, and this is how a project names that.
pub(crate) const NAME_EACH_LIBRARY: &str =
    "An assembly goes into the image only when the program was compiled against it. A project names\n\
     each library it uses with a <Reference> and its <HintPath>, the ones its libraries use included.";

/// Bake a compiled assembly into a `.lmli` flash image, with the corlib and the libraries it was
/// compiled against ([`shared::bake`]), for the board whose bus table is `board_buses`.
///
/// # Errors
/// Anything [`shared::bake`] refuses, in its words.
pub fn bake(assembly: Vec<u8>, references: &[Vec<u8>], board_buses: Option<&[u32]>) -> Result<Vec<u8>, String> {
    shared::bake(assembly, references, board_buses, NAME_EACH_LIBRARY).map(|baked| baked.image)
}

/// Compile `source` and bake it in one step, against `set` and with the bus table of the board the
/// set is for. Only tests make an image from source text this way; a `--device` route compiles the
/// file or project as `build` does ([`image_for_firmware`]), and both refuse an unmet family build
/// with [`ReferenceSet::refuse_unmet`]'s words.
///
/// # Errors
/// A compile failure, reported as the compiler wrote it, a program the set cannot build for its
/// board ([`ReferenceSet::refuse_unmet`]), or any bake failure.
#[cfg(test)]
pub fn compile_and_bake(set: &ReferenceSet, source: &str, verb: &str) -> Result<Vec<u8>, String> {
    use lamella_wire_host::bake::Refused;
    use lamella_wire_host::engine::{CompileFailure, ReplCompiler};
    let compiler = set.compiler();
    let assembly = compiler.compile(source).map_err(|failure| match failure {
        CompileFailure::Diagnostics(text) => text,
        CompileFailure::Toolchain(text) => format!("toolchain error: {text}"),
    })?;
    shared::bake_for(set, assembly, NAME_EACH_LIBRARY).map(|baked| baked.image).map_err(|refused| match refused {
        Refused::Unmet(why) => format!("lamella {verb}: {why}"),
        Refused::Bake(why) => why,
    })
}

/// Refuse a file that a `--device` route cannot compile, in the words `refusal` gives each verb --
/// asked before the board is opened, so a file nothing could send is refused without touching it.
///
/// # Errors
/// A file that is not C# and not a project.
pub fn refuse_uncompilable(
    path: &std::path::Path,
    refusal: impl Fn(&std::path::Path, &crate::deploy::Uncompilable) -> String,
) -> Result<(), String> {
    if crate::flash::is_project(path) {
        return Ok(());
    }
    match crate::deploy::uncompilable_source(path) {
        Some(what) => Err(refusal(path, &what)),
        None => Ok(()),
    }
}

/// The baked image a `--device` route sends to firmware already running on a board, compiled from
/// the C# file or project at `path`, or why it cannot be.
///
/// **ONE COMPILE FOR BOTH VERBS THAT SEND TO FIRMWARE** -- `run --device` and `deploy --device` --
/// so a program the one sends is a program the other sends. What differs is the sentence for a file
/// that is not C#, which each verb words for what it can offer instead, so `refusal` supplies it.
///
/// A project is compiled as `build` compiles it, against the libraries its `<Reference>` elements
/// name, and the image carries the ones the program references.
///
/// `board` is the board that answered: the image is built for it, with its bus table and against
/// its family's builds ([`lamella_wire_host::boards`]). `None` builds for no board.
///
/// # Errors
/// A file that is not C#, one that cannot be read, a project that is a class library or names a
/// library that cannot be read, no compiler, the compiler's diagnostics, or any bake failure.
pub fn image_for_firmware(
    path: &std::path::Path,
    verb: &str,
    refusal: impl Fn(&std::path::Path, &crate::deploy::Uncompilable) -> String,
    board: Option<&str>,
) -> Result<Vec<u8>, String> {
    if crate::flash::is_project(path) {
        let project = crate::program::runnable_project(path, verb)?;
        let libraries = crate::flash::project_references(&project, verb)?;
        let (assembly, references) = crate::program::compile_project_assembly_with_references(
            &project, &libraries, board, verb,
        )?;
        return bake(assembly, &references, lamella_wire_host::boards::bus_table(board));
    }
    refuse_uncompilable(path, refusal)?;
    let source = crate::text::read_text(path)
        .map_err(|error| format!("lamella {verb}: read {}: {error}", path.display()))?;
    let (assembly, references) =
        crate::program::compile_csharp_assembly_with_references(path, &source, false, &[], board, verb)?;
    bake(assembly, &references, lamella_wire_host::boards::bus_table(board))
}

/// The baked image of `path`, an assembly a build already made, for the board that answered.
///
/// **ITS LIBRARIES ARE THE CLASS LIBRARY'S AND THE ONES BESIDE IT**, as a .NET build leaves a
/// program's own libraries beside it: every library the program references that the class library
/// does not hold is read from the program's folder, and the libraries those reference with it.
///
/// # Errors
/// An assembly that cannot be read, a library it references that is in neither place, a program
/// the set cannot build for the board, or any bake failure.
pub fn image_of_built(path: &std::path::Path, verb: &str, board: Option<&str>) -> Result<Vec<u8>, String> {
    let read = |path: &std::path::Path| {
        std::fs::read(path).map_err(|error| format!("lamella {verb}: read {}: {error}", path.display()))
    };
    let assembly = read(path)?;
    let set = ReferenceSet::discover(board).map_err(|error| format!("lamella {verb}: {error}"))?;
    let offered = set.compiler().library_names();
    let folder = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let mut references = set.carried().to_vec();
    let mut pending = vec![assembly.clone()];
    let mut taken: Vec<String> = Vec::new();
    while let Some(next) = pending.pop() {
        let Ok(parsed) = lamella_metadata::Assembly::read(&next) else {
            continue;
        };
        for reference in parsed.assembly_refs() {
            let Some(name) = reference.name() else {
                continue;
            };
            if offered.iter().any(|offered| offered.eq_ignore_ascii_case(name)) || taken.iter().any(|one| one == name) {
                continue;
            }
            let beside = folder.join(format!("{name}.dll"));
            if beside.is_file() {
                let bytes = read(&beside)?;
                taken.push(name.to_owned());
                pending.push(bytes.clone());
                references.push(bytes);
            }
        }
    }
    set.refuse_unmet(&assembly, &references).map_err(|why| format!("lamella {verb}: {why}"))?;
    bake(assembly, &references, lamella_wire_host::boards::bus_table(board))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use lamella_metadata::Assembly;
    use lamella_wire_host::engine::LcscCompiler;

    /// A directory of the test's own holding `App.csproj` (an executable) and `Program.cs`.
    fn project(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lamella-bake-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        std::fs::write(
            dir.join("App.csproj"),
            "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    <OutputType>Exe</OutputType>\n    \
             <TargetFramework>lamella1.0</TargetFramework>\n  </PropertyGroup>\n</Project>\n",
        )
        .expect("write the project");
        std::fs::write(
            dir.join("Program.cs"),
            "class Program\n{\n    static int Main()\n    {\n        return 42;\n    }\n}\n",
        )
        .expect("write the program");
        dir.join("App.csproj")
    }

    /// What a baked image prints when it is booted and run here, as the firmware boots it.
    pub(crate) fn run_baked(image: Vec<u8>) -> String {
        let image: &'static [u8] = Box::leak(image.into_boxed_slice());
        let (module, entry) = lamella_cil_runtime::Module::from_baked(image).expect("it boots");
        let entry = entry.expect("it records its entry point");
        let mut vm = lamella_cil_runtime::Vm::new();
        vm.set_memory_backend(Box::new(lamella_cil_runtime::memory::SafeMemory::new()));
        let entry = lamella_cil_runtime::boot_baked(&module, &mut vm, entry)
            .expect("its static constructors run");
        lamella_cil_runtime::run(&module, &mut vm, entry, Vec::new()).expect("it runs to the end");
        String::from_utf16_lossy(vm.output())
    }

    /// A program reading a board's bus table, from the fixture lamella-wire-host's tests read too.
    pub(crate) const READS_THE_BUS_TABLE: &str =
        include_str!("../../lamella-wire-host/tests/fixtures/ReadsTheBusTable.cs");

    /// **A MISSING FAMILY BUILD IS REFUSED IN THE VERB's WORDS**, naming the file and the step that
    /// writes it, in prose that renders.
    #[test]
    fn a_missing_family_build_is_refused_in_the_verbs_words() {
        let fixtures = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../lamella-load/tests/fixtures");
        let dir = std::env::temp_dir().join(format!("lamella-bake-{}-unmet", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        for file in ["corlib.dll", "System.Device.Gpio.dll", "Lamella.Hardware.dll"] {
            std::fs::copy(fixtures.join(file), dir.join(file)).expect("stage the class library");
        }
        let set = ReferenceSet::for_board(LcscCompiler::in_folder(&dir).expect("the set"), Some("rpi-pico2"));
        let refusal = compile_and_bake(
            &set,
            "using System.Device.Gpio;\npublic static class Program\n{\n    public static void Main()\n    {\n        \
             GpioController controller = new GpioController();\n    }\n}\n",
            "deploy",
        )
        .expect_err("the family build is missing");
        assert!(refusal.starts_with("lamella deploy: the program uses System.Device.Gpio"), "{refusal}");
        crate::rendered::assert_renders_cleanly(&refusal, crate::rendered::four_space_sample);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A `--device` ROUTE BAKES FOR THE BOARD THAT ANSWERED**, a file and a project alike: the
    /// image carries that board's table, and with no board it carries none.
    #[test]
    fn the_image_sent_to_firmware_carries_the_table_of_the_board_that_answered() {
        if LcscCompiler::discover().is_err() {
            return;
        }
        let project_file = project("answered");
        let file = project_file.with_file_name("Program.cs");
        std::fs::write(&file, READS_THE_BUS_TABLE).expect("write the program");
        for path in [&project_file, &file] {
            for (board, read) in [
                (Some("rpi-pico2-w"), "8 1074331648 12 17 2 595591168 0\n"),
                (None, "-1 0 -1 0 -1 0 -1\n"),
            ] {
                let image = image_for_firmware(path, "run", |path, _| format!("refused {}", path.display()), board)
                    .unwrap_or_else(|error| panic!("{}, {board:?}: {error}", path.display()));
                assert_eq!(run_baked(image), read, "{}, {board:?}", path.display());
            }
        }
        let _ = std::fs::remove_dir_all(project_file.parent().expect("its directory"));
    }

    /// **A FILE NO `--device` ROUTE CAN COMPILE IS REFUSED BEFORE THE BOARD IS OPENED**, in the
    /// verb's own words, while a C# file and a project pass.
    #[test]
    fn a_file_no_route_compiles_is_refused_before_the_board_is_opened() {
        let refusal = |path: &std::path::Path, _: &crate::deploy::Uncompilable| format!("refused {}", path.display());
        let python = std::path::Path::new("blink.py");
        assert_eq!(refuse_uncompilable(python, refusal), Err("refused blink.py".to_owned()));
        assert_eq!(refuse_uncompilable(std::path::Path::new("Program.cs"), refusal), Ok(()));
        assert_eq!(refuse_uncompilable(std::path::Path::new("App.csproj"), refusal), Ok(()));
    }

    /// **A PROJECT GOES TO FIRMWARE AS A FILE DOES.** Both `--device` routes called a `.csproj`
    /// "not a C# file" -- `deploy`'s usage names one -- because the source gate asks whether a FILE
    /// compiles, and a project is not one: it names the files.
    #[test]
    fn a_project_is_baked_for_firmware_as_a_file_is() {
        if LcscCompiler::discover().is_err() {
            return;
        }
        let path = project("firmware");
        for verb in ["run", "deploy"] {
            let image =
                image_for_firmware(&path, verb, |path, _| format!("refused {}", path.display()), None)
                    .unwrap_or_else(|error| panic!("{verb}: {error}"));
            assert_eq!(&image[..4], b"LML1", "{verb}: a baked image");
        }
        let _ = std::fs::remove_dir_all(path.parent().expect("its directory"));
    }

    /// `source` compiled as a class library named `name` into `dir`, against the discovered reference
    /// set and `also`, or `None` where no corlib is discoverable.
    fn library(dir: &std::path::Path, name: &str, source: &str, also: &[&str]) -> Option<std::path::PathBuf> {
        let compiler = LcscCompiler::discover().ok()?;
        let extra: Vec<Vec<u8>> = also
            .iter()
            .map(|other| std::fs::read(dir.join(format!("{other}.dll"))).expect("a library built first"))
            .collect();
        let references: Vec<Assembly> = compiler
            .references()
            .iter()
            .chain(extra.iter())
            .filter_map(|bytes| Assembly::read(bytes).ok())
            .collect();
        let options = lamella_syntax::lexer::LexOptions {
            target: lamella_syntax::lexer::OutputKind::Library,
            ..Default::default()
        };
        let file = format!("{name}.cs");
        let compiled = lamella_assemble::compile_source_with(
            source, &file, name, name, &references, false, options,
        );
        let image = compiled.image.unwrap_or_else(|| panic!("{name} compiles as a library"));
        let path = dir.join(format!("{name}.dll"));
        std::fs::write(&path, image).expect("write the library");
        Some(path)
    }

    /// A directory holding `Punctuation.dll`, and `Greeting.dll` built against it, or `None` where no
    /// corlib is discoverable.
    fn two_libraries(name: &str) -> Option<std::path::PathBuf> {
        let dir = std::env::temp_dir().join(format!("lamella-bake-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        library(
            &dir,
            "Punctuation",
            "namespace Greetings\n{\n    public static class Punctuation\n    {\n        \
             public static string Exclaim(string text) { return text + \"!\"; }\n    }\n}\n",
            &[],
        )?;
        library(
            &dir,
            "Greeting",
            "namespace Greetings\n{\n    public static class Greeting\n    {\n        \
             public static string For(string name) { return Punctuation.Exclaim(\"hello, \" + name); }\n    \
             }\n}\n",
            &["Punctuation"],
        )?;
        Some(dir)
    }

    /// `App.csproj` and `Program.cs` beside the libraries in `dir`, the project naming `named` with a
    /// `<Reference>` and a `<HintPath>` each, and the program greeting a board.
    fn greeting_project(dir: &std::path::Path, named: &[&str]) -> std::path::PathBuf {
        let references: String = named
            .iter()
            .map(|name| {
                format!(
                    "    <Reference Include=\"{name}\">\n      <HintPath>{}</HintPath>\n    </Reference>\n",
                    dir.join(format!("{name}.dll")).display()
                )
            })
            .collect();
        std::fs::write(
            dir.join("App.csproj"),
            format!(
                "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    \
                 <OutputType>Exe</OutputType>\n    <TargetFramework>lamella1.0</TargetFramework>\n  \
                 </PropertyGroup>\n  <ItemGroup>\n{references}  </ItemGroup>\n</Project>\n"
            ),
        )
        .expect("write the project");
        std::fs::write(
            dir.join("Program.cs"),
            "public static class Program\n{\n    public static void Main()\n    {\n        \
             System.Console.WriteLine(Greetings.Greeting.For(\"board\"));\n    }\n}\n",
        )
        .expect("write the program");
        dir.join("App.csproj")
    }

    /// **A PROJECT GOES TO FIRMWARE WITH THE LIBRARIES ITS REFERENCES NAME**, and with a library one of
    /// them references in turn. Both `--device` routes refused any project with a `<Reference>`,
    /// pointing at a linked build, because the image carried the corlib alone.
    #[test]
    fn a_project_goes_to_firmware_with_the_libraries_it_names() {
        let Some(dir) = two_libraries("named") else {
            return;
        };
        let path = greeting_project(&dir, &["Greeting", "Punctuation"]);
        for verb in ["run", "deploy"] {
            let image =
                image_for_firmware(&path, verb, |path, _| format!("refused {}", path.display()), None)
                    .unwrap_or_else(|error| panic!("{verb}: {error}"));
            assert_eq!(run_baked(image), "hello, board!\n", "{verb}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    /// **A LIBRARY THE PROGRAM WAS NOT COMPILED AGAINST IS NAMED, WITH WHAT TO DO.** The project names
    /// `Greeting` and not the `Punctuation` it calls, so the image cannot carry `Punctuation` and the
    /// board would stop inside `Greeting.For`. The refusal names the call, the assembly it is in, and
    /// the element that adds it -- in prose that renders.
    #[test]
    fn a_library_the_program_was_not_compiled_against_is_named_with_what_to_do() {
        let Some(dir) = two_libraries("unnamed") else {
            return;
        };
        let path = greeting_project(&dir, &["Greeting"]);
        let refusal = image_for_firmware(&path, "deploy", |path, _| format!("refused {}", path.display()), None)
            .expect_err("the image cannot carry Punctuation");
        assert!(
            refusal.contains(
                "Greetings.Greeting::For calls Greetings.Punctuation::Exclaim, which is in Punctuation, \
                 an assembly the image does not carry"
            ) && refusal.contains("<Reference> and its <HintPath>"),
            "{refusal}"
        );
        crate::rendered::assert_renders_cleanly(&refusal, crate::rendered::four_space_sample);
        let _ = std::fs::remove_dir_all(dir);
    }
}
