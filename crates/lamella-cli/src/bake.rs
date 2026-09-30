//! Baking a compiled assembly into the flash image a device runs.

use lamella_cil_runtime::module::ProfileViolation;
use lamella_metadata::Assembly;
use lamella_metadata::signature::SigType;
use lamella_metadata::tables::table;
use lamella_wire_host::engine::LcscCompiler;

/// The assembly slots the lazy corlib load gives its two inputs, as `load_with_corlib` documents
/// them: the corlib first, the program next.
const CORLIB_ASM: u8 = 0;
const PROGRAM_ASM: u8 = 1;

/// The name ECMA-335 gives the kernel library, which the compiler scopes its well-known types to.
/// A reference by this name is a reference to the corlib, whatever the corlib calls itself.
const KERNEL_LIBRARY: &str = "mscorlib";

/// Bake a compiled assembly into a `.lmli` flash image, with the corlib it was compiled against.
///
/// **THE CORLIB GOES INTO THE IMAGE, BECAUSE FIRMWARE HAS NONE TO OFFER.** A board runs the
/// interpreter and its intrinsics, and a call into a managed corlib body -- `Thread.Sleep`,
/// `String.Concat` over five pieces, `Stack<int>` -- needs that body to be somewhere. An image of
/// the program alone refused every such call, so a program that slept could not be sent at all.
///
/// Only what the program reaches is taken: the corlib members its code names, what those bodies
/// call in turn, and each reached type's static constructor, then trimmed to what `Main` can reach.
/// A program that sleeps and prints bakes to about five kilobytes.
///
/// `corlib` is the corlib the program was COMPILED against, never a second one looked up here: a
/// program bound against one corlib and baked with another would call members the image may not
/// carry under those names.
///
/// The program is checked against the capability profile this build carries BEFORE an image is
/// written, so a body this tier cannot execute is refused here rather than on the board.
///
/// # Errors
/// When either assembly does not parse, the program calls into a library other than the corlib or
/// calls a member neither the corlib nor this runtime provides, the program exceeds the capability
/// profile, or it cannot be laid out for flash. Each refusal names the member.
pub fn bake(assembly: Vec<u8>, corlib: &[u8]) -> Result<Vec<u8>, String> {
    let program: &'static [u8] = Box::leak(assembly.into_boxed_slice());
    let corlib: &'static [u8] = Box::leak(corlib.to_vec().into_boxed_slice());
    let program = Assembly::read(program)
        .map_err(|error| format!("the emitted assembly does not parse: {error:?}"))?;
    let corlib = Assembly::read(corlib).map_err(|error| {
        format!("the corlib the program was compiled against does not parse: {error:?}")
    })?;
    if let Some((member, library)) = first_library_member(&program, &corlib) {
        return Err(library_refusal(&member, &library));
    }
    let mut loaded = lamella_load::load_program_lazy_corlib_unfrozen(&corlib, &program).map_err(
        |error| match error {
            lamella_load::LazyLoadError::UnresolvedMember(member) => format!(
                "bake: no image written. The program calls {member}, which neither the corlib \
                 it was\ncompiled against nor this build's runtime provides, so the board would \
                 stop at that call.\n\n\
                 It can be sent once it no longer calls {member}."
            ),
            lamella_load::LazyLoadError::Load(error) => format!("load: {error}"),
        },
    )?;
    let (methods, types, strings) = loaded.module.reachable_set(Some(loaded.entry));
    let violations = loaded.module.validate_profile(Some(&methods));
    if !violations.is_empty() {
        return Err(profile_refusal(&violations, &loaded.module, &corlib, &program));
    }
    loaded.module.retain_reachable(&methods, &types, &strings);
    loaded
        .module
        .write_baked(Some(loaded.entry))
        .map_err(|error| format!("bake: {error:?}"))
}

/// The first member the program reaches in an assembly other than the corlib, and that assembly's
/// name.
///
/// **A LIBRARY IS NAMED BY THE REFERENCE, NOT GUESSED FROM THE MEMBER.** Every reference to a type
/// in another assembly carries that assembly as its scope, so which library a member lives in is a
/// fact of the program's own metadata.
fn first_library_member(program: &Assembly, corlib: &Assembly) -> Option<(String, String)> {
    program.member_refs().find_map(|member| {
        let library = scope_assembly(program, member.parent())?;
        if is_corlib_name(&library, corlib) {
            return None;
        }
        Some((member_ref_name(program, &member), library))
    })
}

/// Whether an assembly reference by `name` means the corlib.
fn is_corlib_name(name: &str, corlib: &Assembly) -> bool {
    name == KERNEL_LIBRARY || corlib.assembly_name() == Some(name)
}

/// The name of the assembly a type reference resolves in, following a nested type out to the type
/// that encloses it and an instantiation to its generic definition. `None` for a type this program
/// defines itself.
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

/// The refusal for a program that calls into a library other than the corlib: the member, the
/// library, and the way to use that library on a board.
fn library_refusal(member: &str, library: &str) -> String {
    format!(
        "bake: no image written. The program calls {member}, which is in {library}.\n\n\
         A program sent to firmware is baked with the corlib and no other library, so a call \
         into\n{library} would have nothing to run on the board.\n\n\
         To use {library} there, compile its sources into the program: a project whose \
         <Compile Include=\"...\" />\nelements name them makes them part of the program rather \
         than a library beside it."
    )
}

/// The refusal text for a program whose baked image would not run on the device.
///
/// Every violation names the method it is in and the member it could not reach, so the list IS the
/// diagnosis. It is capped because a program that misses the tier badly misses it in hundreds of
/// places, and a wall of them buries the first one -- which is the one worth reading.
fn profile_refusal(
    violations: &[ProfileViolation],
    module: &lamella_cil_runtime::Module,
    corlib: &Assembly,
    program: &Assembly,
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
            describe(violation, module, corlib, program)
        ));
    }
    if violations.len() > SHOWN {
        text.push_str(&format!("\n    ... and {} more", violations.len() - SHOWN));
    }
    text.push_str(
        "\n\nEach line names the method the board would stop in and what it could not do there. \
         The\nprogram can be sent once it no longer reaches them.",
    );
    text
}

/// One violation as a sentence naming the method and the member, never a bare token.
fn describe(
    violation: &ProfileViolation,
    module: &lamella_cil_runtime::Module,
    corlib: &Assembly,
    program: &Assembly,
) -> String {
    let (method, recorded) = match violation {
        ProfileViolation::UnsupportedOpcode { method, name, .. }
        | ProfileViolation::UnresolvedCall { method, name, .. }
        | ProfileViolation::UnloweredGeneric { method, name, .. } => (*method, name.as_deref()),
    };
    let asm = module.method_asm(method);
    let assembly = match asm {
        CORLIB_ASM => Some(corlib),
        PROGRAM_ASM => Some(program),
        _ => None,
    };
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
        ProfileViolation::UnresolvedCall { token, .. } => format!(
            "{caller} calls {}, which nothing in the image provides",
            member(*token)
        ),
        ProfileViolation::UnloweredGeneric { token, .. } => format!(
            "{caller} reaches {} through a generic instantiation the bake did not lower",
            member(*token)
        ),
    }
}

/// Compile `source` and bake it in one step, with the corlib `compiler` bound it against -- what a
/// `--target` route sends.
///
/// # Errors
/// A compile failure, reported as the compiler wrote it, or any bake failure.
pub fn compile_and_bake(compiler: &LcscCompiler, source: &str) -> Result<Vec<u8>, String> {
    use lamella_wire_host::engine::{CompileFailure, ReplCompiler};
    let assembly = compiler.compile(source).map_err(|failure| match failure {
        CompileFailure::Diagnostics(text) => text,
        CompileFailure::Toolchain(text) => format!("toolchain error: {text}"),
    })?;
    let Some(corlib) = compiler.references().first() else {
        return Err("the compiler found no reference assemblies".to_owned());
    };
    bake(assembly, corlib)
}

/// The baked image a `--target` route sends to firmware already running on a board, compiled from
/// the C# file or project at `path`, or why it cannot be.
///
/// **ONE COMPILE FOR BOTH VERBS THAT SEND TO FIRMWARE** -- `run --target` and `deploy --target` --
/// so a program the one sends is a program the other sends. What differs is the sentence for a file
/// that is not C#, which each verb words for what it can offer instead, so `refusal` supplies it.
///
/// A project is compiled as `build` compiles it. The image carries the corlib the program was
/// compiled against and no other library, so a project that names libraries of its own is refused,
/// pointing at the deploy that links them.
///
/// # Errors
/// A file that is not C#, one that cannot be read, a project that is not a program this route can
/// run, no compiler, the compiler's diagnostics, or any bake failure.
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
            &format!("lamella deploy {} --board <id>", path.display()),
        )?;
        let (assembly, corlib) = crate::program::compile_project_assembly(&project, &[], verb)?;
        return bake(assembly, &corlib);
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

    /// **A CALL INTO ANOTHER LIBRARY IS REFUSED BY THE MEMBER AND THE LIBRARY, NEVER A TOKEN.** The
    /// image carries the corlib alone, so `GpioController` has no body on the board; the refusal
    /// says so and says how to use that library there.
    #[test]
    fn a_call_into_another_library_names_the_member_and_the_library() {
        let Some(refusal) = baked(
            "using System.Device.Gpio;\npublic static class Program\n{\n    public static void Main()\n    \
             {\n        GpioController controller = new GpioController();\n        \
             controller.OpenPin(25, PinMode.Output);\n    }\n}\n",
        ) else {
            return;
        };
        let refusal = refusal.expect_err("the image carries no library but the corlib");
        assert!(
            refusal.contains("calls System.Device.Gpio.GpioController::.ctor, which is in System.Device.Gpio."),
            "{refusal}"
        );
        assert!(refusal.contains("<Compile Include="), "it says what to do: {refusal}");
        assert!(!refusal.contains("token"), "it names no token: {refusal}");
        crate::rendered::assert_renders_cleanly(&refusal, crate::rendered::four_space_sample);
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
        let refusal = profile_refusal(&[violation], &loaded.module, &corlib, &program);
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
                && refusal.contains("--board <id>")
                && !refusal.contains("--class-library"),
            "{refusal}"
        );
        crate::rendered::assert_renders_cleanly(&refusal, crate::rendered::four_space_sample);
        let _ = std::fs::remove_dir_all(path.parent().expect("its directory"));
    }
}
