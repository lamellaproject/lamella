//! Baking a compiled assembly into the flash image a device runs.

use lamella_wire_host::engine::LcscCompiler;

/// Bake a compiled assembly into a `.lmli` flash image.
///
/// Single-assembly: BCL references resolve to the interpreter's intrinsics, which is the same
/// shape the baked-image link deploys over the wire.
///
/// The program is checked against the capability profile this build carries BEFORE an image is
/// written, so a body this tier cannot execute is refused here rather than on the board.
///
/// # Errors
/// When the emitted assembly does not parse, does not load, exceeds the capability profile, or
/// cannot be laid out for flash.
pub fn bake(assembly: Vec<u8>) -> Result<Vec<u8>, String> {
    let program: &'static [u8] = Box::leak(assembly.into_boxed_slice());
    let parsed = lamella_metadata::Assembly::read(program)
        .map_err(|error| format!("the emitted assembly does not parse: {error:?}"))?;
    let loaded = lamella_load::load(&parsed).map_err(|error| format!("load: {error}"))?;
    let mut module = loaded.module;
    let violations = module.validate_profile(None);
    if !violations.is_empty() {
        return Err(profile_refusal(&violations));
    }
    module
        .write_baked(Some(loaded.entry))
        .map_err(|error| format!("bake: {error:?}"))
}

/// The refusal text for a program whose baked image would not run on the device.
///
/// Every violation names the method it is in, so the list IS the diagnosis. It is capped because a
/// program that misses the tier badly misses it in hundreds of places, and a wall of them buries
/// the first one -- which is the one worth reading.
fn profile_refusal(violations: &[impl core::fmt::Display]) -> String {
    const SHOWN: usize = 20;
    let places = if violations.len() == 1 {
        String::from("one place")
    } else {
        format!("{} places", violations.len())
    };
    let mut text =
        format!("bake: no image written. This program would trap on the device in {places}:");
    for violation in violations.iter().take(SHOWN) {
        text.push_str(&format!("\n  {violation}"));
    }
    if violations.len() > SHOWN {
        text.push_str(&format!("\n  ... and {} more", violations.len() - SHOWN));
    }
    text
}

/// Compile `source` and bake it in one step -- what `deploy` needs and what `build` does under
/// this feature.
///
/// # Errors
/// A compile failure, reported as the compiler wrote it, or any bake failure.
pub fn compile_and_bake(compiler: &LcscCompiler, source: &str) -> Result<Vec<u8>, String> {
    use lamella_wire_host::engine::{CompileFailure, ReplCompiler};
    let assembly = compiler.compile(source).map_err(|failure| match failure {
        CompileFailure::Diagnostics(text) => text,
        CompileFailure::Toolchain(text) => format!("toolchain error: {text}"),
    })?;
    bake(assembly)
}

/// The baked image a `--target` route sends to firmware already running on a board, compiled from
/// the C# file or project at `path`, or why it cannot be.
///
/// **ONE COMPILE FOR BOTH VERBS THAT SEND TO FIRMWARE** -- `run --target` and `deploy --target` --
/// so a program the one sends is a program the other sends. What differs is the sentence for a file
/// that is not C#, which each verb words for what it can offer instead, so `refusal` supplies it.
///
/// A project is compiled as `build` compiles it. The firmware resolves a program against the class
/// library it carries and links nothing, so a project that names libraries of its own is refused,
/// pointing at the deploy that links them.
///
/// # Errors
/// A file that is not C#, one that cannot be read, a project that is not a program this route can
/// run, no compiler, or the compiler's diagnostics.
pub fn image_for_firmware(
    path: &std::path::Path,
    verb: &str,
    refusal: impl Fn(&std::path::Path, &crate::deploy::Uncompilable) -> String,
) -> Result<Vec<u8>, String> {
    if crate::flash::is_project(path) {
        let project = crate::program::program_project(
            path,
            verb,
            "Firmware on a board runs a program",
            &format!(
                "lamella deploy {} --board <id> --class-library",
                path.display()
            ),
        )?;
        let (assembly, _) = crate::program::compile_project_assembly(&project, &[], verb)?;
        return bake(assembly);
    }
    if let Some(what) = crate::deploy::uncompilable_source(path) {
        return Err(refusal(path, &what));
    }
    let source = std::fs::read_to_string(path)
        .map_err(|error| format!("lamella {verb}: read {}: {error}", path.display()))?;
    let compiler = LcscCompiler::discover()
        .map_err(|error| format!("lamella {verb}: {error}"))?
        .for_source_file(&path.display().to_string());
    compile_and_bake(&compiler, &source)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// **A PROJECT GOES TO FIRMWARE AS A FILE DOES.** Both `--target` routes called a `.csproj`
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
                image_for_firmware(&path, verb, |path, _| format!("refused {}", path.display()))
                    .unwrap_or_else(|error| panic!("{verb}: {error}"));
            assert_eq!(&image[..4], b"LML1", "{verb}: a baked image");
        }
        let _ = std::fs::remove_dir_all(path.parent().expect("its directory"));
    }

    /// A project naming libraries of its own is refused for firmware, which links nothing, and
    /// pointed at the deploy that links them -- in prose that renders.
    #[test]
    fn a_project_naming_libraries_is_pointed_at_the_deploy_that_links_them() {
        let path = project("libraries");
        let gpio = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../lamella-load/tests/fixtures/System.Device.Gpio.dll");
        std::fs::write(
            &path,
            format!(
                "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    \
                 <OutputType>Exe</OutputType>\n    <TargetFramework>lamella1.0</TargetFramework>\n  \
                 </PropertyGroup>\n  <ItemGroup>\n    <Reference Include=\"System.Device.Gpio\">\n      \
                 <HintPath>{}</HintPath>\n    </Reference>\n  </ItemGroup>\n</Project>\n",
                gpio.display()
            ),
        )
        .expect("write the project");
        let refusal = image_for_firmware(&path, "deploy", |path, _| {
            format!("refused {}", path.display())
        })
        .expect_err("firmware links nothing");
        assert!(
            refusal.starts_with("lamella deploy: ")
                && refusal.contains("Firmware on a board runs a program against the class library")
                && refusal.contains("--board <id> --class-library"),
            "{refusal}"
        );
        crate::rendered::assert_renders_cleanly(&refusal, crate::rendered::four_space_sample);
        let _ = std::fs::remove_dir_all(path.parent().expect("its directory"));
    }
}
