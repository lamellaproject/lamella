//! Baking a compiled assembly into the flash image a device runs.

use lamella_cil_runtime::module::ProfileViolation;
use lamella_metadata::Assembly;
use lamella_metadata::signature::SigType;
use lamella_metadata::tables::table;
use lamella_wire_host::engine::LcscCompiler;

/// Bake a compiled assembly into a `.lmli` flash image, with the corlib and the libraries it was
/// compiled against.
///
/// **THEY GO INTO THE IMAGE, BECAUSE FIRMWARE HAS NONE TO OFFER.** A board runs the interpreter and
/// its intrinsics, and a call into a managed body -- `Thread.Sleep`, `Stack<int>`,
/// `GpioController.OpenPin`, a board class binding its buses -- needs that body to be somewhere. An
/// image of the program alone refused every such call, so a program that slept could not be sent
/// at all, and one that blinked a pin could not be sent either.
///
/// `references` is the set the program was COMPILED against, the corlib first, and nothing is looked
/// up a second time here: a program bound against one assembly and baked with another would call
/// members the image may not carry under those names. Of that set the image carries the corlib and
/// the libraries the program references, directly or through one another.
///
/// Only what the program reaches is kept: the corlib members its code names and what those bodies
/// call in turn, and each library it references, all of it then trimmed to what `Main` can reach. A
/// program that sleeps and prints bakes to about five kilobytes.
///
/// The program is checked against the capability profile this build carries BEFORE an image is
/// written, so a body this tier cannot execute is refused here rather than on the board.
///
/// # Errors
/// When an assembly does not parse, the program calls a member that neither the assemblies it was
/// compiled against nor this runtime provides, the program exceeds the capability profile, or it
/// cannot be laid out for flash. Each refusal names the member.
pub fn bake(assembly: Vec<u8>, references: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    let Some((corlib, libraries)) = references.split_first() else {
        return Err("the compiler found no reference assemblies".to_owned());
    };
    let program = Assembly::read(leaked(assembly))
        .map_err(|error| format!("the emitted assembly does not parse: {error:?}"))?;
    let corlib = Assembly::read(leaked(corlib.clone())).map_err(|error| {
        format!("the corlib the program was compiled against does not parse: {error:?}")
    })?;
    let available = available_libraries(&corlib, libraries);
    let taken = lamella_load::referenced_libraries(&program, &available)
        .map_err(|error| format!("bake: no image written: {error}"))?;
    let libraries: Vec<Assembly<'static>> =
        taken.iter().map(|&position| available[position].clone()).collect();
    let mut loaded = lamella_load::load_program_lazy_corlib_with_libraries_unfrozen(
        &corlib, &libraries, &program,
    )
    .map_err(|error| match error {
        lamella_load::LazyLoadError::UnresolvedMember(member) => format!(
            "bake: no image written. The program calls {member}, which neither the assemblies \
             it was\ncompiled against nor this build's runtime provides, so the board would \
             stop at that call.\n\n\
             It can be sent once it no longer calls {member}."
        ),
        lamella_load::LazyLoadError::Load(error) => format!("load: {error}"),
    })?;
    let (methods, types, strings) = loaded.module.reachable_set(Some(loaded.entry));
    let violations = loaded.module.validate_profile(Some(&methods));
    if !violations.is_empty() {
        let assemblies: Vec<&Assembly<'static>> = core::iter::once(&corlib)
            .chain(libraries.iter())
            .chain(core::iter::once(&program))
            .collect();
        return Err(profile_refusal(&violations, &loaded.module, &assemblies));
    }
    loaded.module.retain_reachable(&methods, &types, &strings);
    loaded
        .module
        .write_baked(Some(loaded.entry))
        .map_err(|error| format!("bake: {error:?}"))
}


/// The libraries a bake may take from the rest of the reference set: each one that parses and is
/// not named as the corlib.
///
/// **AN ASSEMBLY NAMED AS THE CORLIB IS NOT A LIBRARY.** A program names the assembly of its
/// well-known types `mscorlib`, whatever the corlib calls itself, so a reference set that also held
/// an `mscorlib.dll` would have it taken as a library and loaded beside the corlib, each declaring
/// `System.Object`. The corlib takes its own slot.
///
/// One that does not parse is skipped as the compiler skipped it: nothing in the program can have
/// been bound against it.
fn available_libraries(corlib: &Assembly, libraries: &[Vec<u8>]) -> Vec<Assembly<'static>> {
    libraries
        .iter()
        .filter_map(|bytes| Assembly::read(leaked(bytes.clone())).ok())
        .filter(|library| !library.assembly_name().is_some_and(|name| crate::program::names_the_corlib(name, corlib)))
        .collect()
}


/// `bytes` with the `'static` lifetime a `code-in-place` load borrows its assemblies for.
///
/// One leaked buffer per assembly per bake -- the accepted host `Assembly<'static>` pattern under
/// `code-in-place`, and the same thing the MCP server's bake path does. A command-line tool bakes
/// once and exits, so the leak has a lifetime measured in milliseconds.
fn leaked(bytes: Vec<u8>) -> &'static [u8] {
    Box::leak(bytes.into_boxed_slice())
}

/// The name of the assembly a type reference resolves in, following a nested type out to the type
/// that encloses it and an instantiation to its generic definition. `None` for a type this
/// assembly defines itself.
///
/// **A LIBRARY IS NAMED BY THE REFERENCE, NOT GUESSED FROM THE MEMBER.** Every reference to a type
/// in another assembly carries that assembly as its scope, so which library a member lives in is a
/// fact of the referencing assembly's own metadata.
fn scope_assembly(assembly: &Assembly, token: lamella_token::Token) -> Option<String> {
    let mut current = token;
    for _ in 0..16 {
        match current.table() {
            table::TYPE_REF => {
                let scope = assembly.type_ref(current.row())?.resolution_scope();
                if scope.table() == table::ASSEMBLY_REF {
                    return assembly
                        .assembly_ref(scope.row())
                        .and_then(|reference| reference.name())
                        .map(String::from);
                }
                current = scope;
            }
            table::TYPE_SPEC => current = generic_definition(assembly, current)?,
            _ => return None,
        }
    }
    None
}

/// The generic definition an instantiation's `TypeSpec` names.
fn generic_definition(assembly: &Assembly, token: lamella_token::Token) -> Option<lamella_token::Token> {
    match assembly.type_spec_signature(token)? {
        SigType::GenericInst { definition, .. } => match *definition {
            SigType::Class(definition) | SigType::ValueType(definition) => Some(definition),
            _ => None,
        },
        _ => None,
    }
}

/// A type's name as a reader writes it: namespace, enclosing types and name, and for an
/// instantiation its generic definition's.
fn type_name(assembly: &Assembly, token: lamella_token::Token) -> Option<String> {
    let token = if token.table() == table::TYPE_SPEC {
        generic_definition(assembly, token)?
    } else {
        token
    };
    let (namespace, name) = assembly.type_token_full_name(token)?;
    Some(if namespace.is_empty() {
        name
    } else {
        format!("{namespace}.{name}")
    })
}

/// A member reference as `Namespace.Type::Member`, the loader's own spelling for a member it could
/// not bind, so the two refusals a reader can meet name a member the same way.
fn member_ref_name(assembly: &Assembly, member: &lamella_metadata::MemberRef<'_>) -> String {
    let name = member.name().unwrap_or("<unnamed>");
    match type_name(assembly, member.parent()) {
        Some(owner) => format!("{owner}::{name}"),
        None => name.to_owned(),
    }
}

/// The member a call token names, from the metadata of the assembly whose token space it is in.
///
/// `None` for a token that names no row there, so a refusal falls back to the token rather than
/// naming a member the program never called.
fn member_name(assembly: &Assembly, token: lamella_token::Token) -> Option<String> {
    match token.table() {
        table::MEMBER_REF => Some(member_ref_name(assembly, &assembly.member_ref(token.row())?)),
        table::METHOD_DEF => assembly.type_defs().find_map(|type_def| {
            let method = type_def.methods().find(|method| method.token() == token)?;
            let owner = type_name(assembly, type_def.token())?;
            Some(format!("{owner}::{}", method.name()?))
        }),
        table::METHOD_SPEC => member_name(assembly, assembly.method_spec_method(token)?),
        _ => None,
    }
}

/// The method a violation is in, as `Namespace.Type::Method`, from the definition it was loaded
/// from; its recorded debug name where no definition answers, as for a lowered instantiation.
///
/// **THE DEFINITION FIRST, SO ONE LINE NAMES CALLER AND MEMBER IN ONE SPELLING.** A debug name is
/// written `Type.Method`, and a line reading `Program.Main calls System.Threading.Thread::Sleep`
/// spells one thing two ways.
fn method_name(
    module: &lamella_cil_runtime::Module,
    method: lamella_cil_runtime::MethodId,
    recorded: Option<&str>,
    assembly: &Assembly,
    asm: u8,
) -> String {
    assembly
        .type_defs()
        .find_map(|type_def| {
            let defined = type_def
                .methods()
                .find(|candidate| module.resolve(asm, candidate.token()) == Some(method))?;
            let owner = type_name(assembly, type_def.token())?;
            Some(format!("{owner}::{}", defined.name()?))
        })
        .or_else(|| recorded.map(String::from))
        .unwrap_or_else(|| format!("method {method}"))
}

/// The refusal text for a program whose baked image would not run on the device.
///
/// Every violation names the method it is in and the member it could not reach, so the list IS the
/// diagnosis. It is capped because a program that misses the tier badly misses it in hundreds of
/// places, and a wall of them buries the first one -- which is the one worth reading.
fn profile_refusal(
    violations: &[ProfileViolation],
    module: &lamella_cil_runtime::Module,
    assemblies: &[&Assembly],
) -> String {
    const SHOWN: usize = 20;
    let places = if violations.len() == 1 {
        String::from("one place")
    } else {
        format!("{} places", violations.len())
    };
    let mut text =
        format!("bake: no image written. This program would trap on the device in {places}:");
    for violation in violations.iter().take(SHOWN) {
        text.push_str(&format!(
            "\n    {}",
            describe(violation, module, assemblies)
        ));
    }
    if violations.len() > SHOWN {
        text.push_str(&format!("\n    ... and {} more", violations.len() - SHOWN));
    }
    text.push_str(
        "\n\nEach line names the method the board would stop in and what it could not do there. \
         The\nprogram can be sent once it no longer reaches them.",
    );
    if violations
        .iter()
        .any(|violation| uncarried_assembly(violation, module, assemblies).is_some())
    {
        text.push_str(
            "\n\nAn assembly goes into the image only when the program was compiled against it. A \
             project names\neach library it uses with a <Reference> and its <HintPath>, the ones its \
             libraries use included.",
        );
    }
    text
}

/// The assembly a violation's member is in, when the image does not carry it: a library the program
/// reaches through another library and was not itself compiled against.
///
/// `None` for any other violation, and for a member of the corlib or of an assembly the image
/// carries, which is missing for some other reason.
fn uncarried_assembly(
    violation: &ProfileViolation,
    module: &lamella_cil_runtime::Module,
    assemblies: &[&Assembly],
) -> Option<String> {
    let ProfileViolation::UnresolvedCall { method, token, .. } = violation else {
        return None;
    };
    let assembly = assemblies.get(usize::from(module.method_asm(*method)))?;
    let token = lamella_token::Token(*token);
    if token.table() != table::MEMBER_REF {
        return None;
    }
    let library = scope_assembly(assembly, assembly.member_ref(token.row())?.parent())?;
    let corlib = assemblies.first()?;
    let carried = crate::program::names_the_corlib(&library, corlib)
        || assemblies.iter().any(|carried| carried.assembly_name() == Some(library.as_str()));
    (!carried).then_some(library)
}

/// One violation as a sentence naming the method and the member, never a bare token.
///
/// `assemblies` are the image's, indexed by assembly id.
fn describe(
    violation: &ProfileViolation,
    module: &lamella_cil_runtime::Module,
    assemblies: &[&Assembly],
) -> String {
    let (method, recorded) = match violation {
        ProfileViolation::UnsupportedOpcode { method, name, .. }
        | ProfileViolation::UnresolvedCall { method, name, .. }
        | ProfileViolation::UnloweredGeneric { method, name, .. } => (*method, name.as_deref()),
    };
    let asm = module.method_asm(method);
    let assembly = assemblies.get(usize::from(asm)).copied();
    let caller = match assembly {
        Some(assembly) => method_name(module, method, recorded, assembly, asm),
        None => recorded.map_or_else(|| format!("method {method}"), String::from),
    };
    let member = |token: u32| {
        assembly
            .and_then(|assembly| member_name(assembly, lamella_token::Token(token)))
            .unwrap_or_else(|| format!("the member at token 0x{token:08X}"))
    };
    match violation {
        ProfileViolation::UnsupportedOpcode { opcode, .. } => {
            format!("{caller} uses `{opcode}`, which this build's runtime does not support")
        }
        ProfileViolation::UnresolvedCall { token, .. } => {
            match uncarried_assembly(violation, module, assemblies) {
                Some(library) => format!(
                    "{caller} calls {}, which is in {library}, an assembly the image does not carry",
                    member(*token)
                ),
                None => format!(
                    "{caller} calls {}, which nothing in the image provides",
                    member(*token)
                ),
            }
        }
        ProfileViolation::UnloweredGeneric { token, .. } => format!(
            "{caller} reaches {} through a generic instantiation the bake did not lower",
            member(*token)
        ),
    }
}

/// Compile `source` and bake it in one step, with the assemblies `compiler` bound it against --
/// what a `--target` route sends.
///
/// # Errors
/// A compile failure, reported as the compiler wrote it, or any bake failure.
pub fn compile_and_bake(compiler: &LcscCompiler, source: &str) -> Result<Vec<u8>, String> {
    use lamella_wire_host::engine::{CompileFailure, ReplCompiler};
    let assembly = compiler.compile(source).map_err(|failure| match failure {
        CompileFailure::Diagnostics(text) => text,
        CompileFailure::Toolchain(text) => format!("toolchain error: {text}"),
    })?;
    bake(assembly, compiler.references())
}

/// The baked image a `--target` route sends to firmware already running on a board, compiled from
/// the C# file or project at `path`, or why it cannot be.
///
/// **ONE COMPILE FOR BOTH VERBS THAT SEND TO FIRMWARE** -- `run --target` and `deploy --target` --
/// so a program the one sends is a program the other sends. What differs is the sentence for a file
/// that is not C#, which each verb words for what it can offer instead, so `refusal` supplies it.
///
/// A project is compiled as `build` compiles it, against the libraries its `<Reference>` elements
/// name, and the image carries the ones the program references.
///
/// # Errors
/// A file that is not C#, one that cannot be read, a project that is a class library or names a
/// library that cannot be read, no compiler, the compiler's diagnostics, or any bake failure.
pub fn image_for_firmware(
    path: &std::path::Path,
    verb: &str,
    refusal: impl Fn(&std::path::Path, &crate::deploy::Uncompilable) -> String,
) -> Result<Vec<u8>, String> {
    if crate::flash::is_project(path) {
        let project = crate::program::runnable_project(path, verb)?;
        let libraries = crate::flash::project_references(&project, verb)?;
        let (assembly, references) =
            crate::program::compile_project_assembly_with_references(&project, &libraries, verb)?;
        return bake(assembly, &references);
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

    /// The image `source` bakes to, compiled against the discovered reference set, or `None` where
    /// no corlib is discoverable -- the guard `flash.rs`'s tests state once.
    fn baked(source: &str) -> Option<Result<Vec<u8>, String>> {
        let compiler = LcscCompiler::discover().ok()?;
        Some(compile_and_bake(&compiler, source))
    }

    /// What a baked image prints when it is booted and run here, as the firmware boots it.
    fn run_baked(image: Vec<u8>) -> String {
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

    /// **A PROGRAM THAT SLEEPS CAN BE SENT TO A BOARD.** `Thread.Sleep` is a managed corlib body
    /// over an intrinsic, and so is a five-piece string concatenation; an image of the program
    /// alone refused both, naming a token. Baked with the corlib, the image boots and prints what
    /// the program prints.
    #[test]
    fn a_program_calling_managed_corlib_bodies_bakes_with_the_corlib_and_runs() {
        let Some(image) = baked(
            "public static class Program\n{\n    public static void Main()\n    {\n        \
             System.Threading.Thread.Sleep(100);\n        int i = 3; int ms = 40;\n        \
             System.Console.WriteLine(\"tick \" + i + \" at +\" + ms + \" ms\");\n        \
             System.Console.WriteLine(\"slept\");\n    }\n}\n",
        ) else {
            return;
        };
        let image = image.expect("a program calling corlib bodies bakes");
        assert_eq!(&image[..4], b"LML1", "a baked image");
        assert_eq!(run_baked(image), "tick 3 at +40 ms\nslept\n");
    }

    /// **A GENERIC CORLIB TYPE IS BAKED AS ITS INSTANTIATION.** `Stack<int>` reaches the image as
    /// its own lowered copy, and the generic definition's template bodies -- which no image runs --
    /// are not held against the program.
    #[test]
    fn a_generic_corlib_type_and_a_stopwatch_bake_and_run() {
        let Some(image) = baked(
            "using System.Collections.Generic;\nusing System.Diagnostics;\n\
             public static class Program\n{\n    public static void Main()\n    {\n        \
             Stopwatch watch = Stopwatch.StartNew();\n        Stack<int> stack = new Stack<int>();\n        \
             for (int i = 0; i < 5; i++) { stack.Push(i * i); }\n        \
             watch.Stop();\n        \
             System.Console.WriteLine(stack.Pop() + \" \" + stack.Count + \" \" + (watch.ElapsedMilliseconds >= 0));\n    }\n}\n",
        ) else {
            return;
        };
        let image = image.expect("a Stack<int> program bakes");
        assert_eq!(run_baked(image), "16 4 True\n");
    }

    /// A program calling into `System.Device.Gpio` with no board bound: the library's own code runs
    /// and throws, and the program catches it.
    const GPIO_WITHOUT_A_BOARD: &str = "using System.Device.Gpio;\npublic static class Program\n{\n    \
        public static void Main()\n    {\n        System.Console.WriteLine(\"before\");\n        try\n        \
        {\n            GpioController controller = new GpioController();\n            \
        controller.OpenPin(25, PinMode.Output);\n        }\n        catch (System.Exception error)\n        \
        {\n            System.Console.WriteLine(\"threw \" + error.GetType().Name);\n        }\n        \
        System.Console.WriteLine(\"after\");\n    }\n}\n";

    /// **A CALL INTO A LIBRARY IS BAKED WITH THAT LIBRARY.** `GpioController` is in
    /// `System.Device.Gpio`, and an image of the corlib alone refused the call by name, so no
    /// dotnet/iot program could reach a board this way. Baked with the library, the image runs the
    /// library's own code: with no board bound, `new GpioController()` raises the .NET exception the
    /// library raises, and the program catches it.
    #[test]
    fn a_call_into_a_library_is_baked_with_that_library_and_runs() {
        let Some(image) = baked(GPIO_WITHOUT_A_BOARD) else {
            return;
        };
        let image = image.expect("a program calling into System.Device.Gpio bakes");
        assert_eq!(run_baked(image), "before\nthrew InvalidOperationException\nafter\n");
    }

    /// **A VIOLATION NAMES THE MEMBER AND THE METHOD, NEVER A TOKEN.** "calls token 0x0A000001,
    /// which resolves to nothing" was the whole refusal, and it gave the reader nothing to search
    /// for. The token is read in the calling method's own assembly, where it names the member.
    #[test]
    fn a_violation_names_the_caller_and_the_member_rather_than_a_token() {
        let Ok(compiler) = LcscCompiler::discover() else {
            return;
        };
        use lamella_wire_host::engine::ReplCompiler;
        let assembly = compiler
            .compile(
                "public static class Program\n{\n    public static void Main()\n    {\n        \
                 System.Threading.Thread.Sleep(100);\n    }\n}\n",
            )
            .unwrap_or_else(|_| panic!("it compiles"));
        let program: &'static [u8] = Box::leak(assembly.into_boxed_slice());
        let corlib: &'static [u8] =
            Box::leak(compiler.references()[0].clone().into_boxed_slice());
        let program = Assembly::read(program).expect("the program parses");
        let corlib = Assembly::read(corlib).expect("the corlib parses");
        let loaded =
            lamella_load::load_program_lazy_corlib_unfrozen(&corlib, &program).expect("it loads");
        let sleep = (1..)
            .map_while(|row| program.member_ref(row).map(|member| (row, member)))
            .find(|(_, member)| member.name() == Some("Sleep"))
            .map(|(row, _)| lamella_token::Token::new(table::MEMBER_REF, row))
            .expect("the program references Thread.Sleep");
        let violation = ProfileViolation::UnresolvedCall {
            method: loaded.entry,
            name: Some(String::from("Program.Main")),
            token: sleep.0,
        };
        let refusal = profile_refusal(&[violation], &loaded.module, &[&corlib, &program]);
        assert!(
            refusal.contains("Program::Main calls System.Threading.Thread::Sleep, which nothing in the image provides"),
            "{refusal}"
        );
        assert!(!refusal.contains("0x0A"), "it names no token: {refusal}");
        crate::rendered::assert_renders_cleanly(&refusal, crate::rendered::four_space_sample);
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

    /// **AN ASSEMBLY NAMED AS THE CORLIB IS NEVER OFFERED AS A LIBRARY**, whether it is named
    /// `mscorlib` -- the name a program gives its well-known types' assembly -- or as the corlib names
    /// itself. A program references `mscorlib`, so a reference set holding an `mscorlib.dll` beside the
    /// corlib had it taken as a library, which loads a second `System.Object` beside the corlib's.
    #[test]
    fn an_assembly_named_as_the_corlib_is_not_offered_as_a_library() {
        let Ok(compiler) = LcscCompiler::discover() else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("lamella-bake-{}-kernel", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let Some(impostor) = library(
            &dir,
            "mscorlib",
            "namespace Elsewhere\n{\n    public static class Nothing\n    {\n        \
             public static int Zero() { return 0; }\n    }\n}\n",
            &[],
        ) else {
            return;
        };
        let corlib_bytes = compiler.references()[0].clone();
        let gpio = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../lamella-load/tests/fixtures/System.Device.Gpio.dll"),
        )
        .expect("the System.Device.Gpio fixture");
        let corlib = Assembly::read(leaked(corlib_bytes.clone())).expect("the corlib parses");
        let offered = [
            std::fs::read(impostor).expect("read the mscorlib-named library"),
            corlib_bytes,
            gpio,
        ];
        let names: Vec<Option<String>> = available_libraries(&corlib, &offered)
            .iter()
            .map(|library| library.assembly_name().map(String::from))
            .collect();
        assert_eq!(names, [Some(String::from("System.Device.Gpio"))]);
        let _ = std::fs::remove_dir_all(dir);
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
    /// them references in turn. Both `--target` routes refused any project with a `<Reference>`,
    /// pointing at a linked build, because the image carried the corlib alone.
    #[test]
    fn a_project_goes_to_firmware_with_the_libraries_it_names() {
        let Some(dir) = two_libraries("named") else {
            return;
        };
        let path = greeting_project(&dir, &["Greeting", "Punctuation"]);
        for verb in ["run", "deploy"] {
            let image =
                image_for_firmware(&path, verb, |path, _| format!("refused {}", path.display()))
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
        let refusal = image_for_firmware(&path, "deploy", |path, _| format!("refused {}", path.display()))
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
