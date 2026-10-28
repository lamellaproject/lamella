//! Baking a compiled program into the flash image a board runs, with the corlib and the libraries it
//! was compiled against, and saying what the image carries.

use crate::boards::ReferenceSet;
use crate::refusal;
use lamella_cil_runtime::module::ProfileViolation;
use lamella_metadata::Assembly;

/// What a baked image's size leaves out, which is the firmware it is loaded into: a board's flash
/// holds that firmware too. A fit verdict over the size is therefore an upper bound on the room the
/// image has, and every tool that answers one says so in these words.
pub const SIZE_LEAVES_OUT: &str =
    "the resident interpreter firmware already on the board, which this image is loaded into rather than replacing";

/// A baked image, and what it carries.
#[derive(Debug)]
pub struct Baked {
    /// The `.lmli` image.
    pub image: Vec<u8>,
    /// Each assembly the image carries code from, in the order the image numbers them: the corlib
    /// first, the program last.
    pub carried: Vec<Carried>,
}

/// One assembly an image carries code from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carried {
    /// The name the assembly's own metadata gives it.
    pub name: String,
    /// What it is to the program.
    pub role: Role,
    /// How many of its methods the image carries.
    pub methods: usize,
    /// How many bytes of code those methods put in the image. A body two methods share is counted
    /// once, for the first of them, as the image stores it once.
    pub code_bytes: usize,
}

/// What an assembly an image carries is to the program.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The corlib, which every program references.
    Corlib,
    /// A library the program references, or one a library it references uses.
    Library,
    /// The program itself.
    Program,
}

/// Why [`bake_for`] wrote no image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refused {
    /// The set's folder holds no build of a library the program uses for the board's chip family
    /// ([`ReferenceSet::refuse_unmet`]).
    Unmet(String),
    /// The bake refused the program ([`bake`]).
    Bake(String),
}

impl Refused {
    /// The refusal's text.
    #[must_use]
    pub fn into_text(self) -> String {
        match self {
            Self::Unmet(text) | Self::Bake(text) => text,
        }
    }
}

/// Bake `program`, compiled against `set`, into the image a board of `set` runs: with the corlib and
/// the libraries it uses, bound against the board's family builds, and carrying the board's bus
/// table.
///
/// `uncarried_advice` is what the caller tells a reader whose program calls into an assembly the
/// image cannot carry, which differs by how the caller's users name a library.
///
/// # Errors
/// [`Refused::Unmet`] for a program using a library the set has no family build of, and
/// [`Refused::Bake`] for anything [`bake`] refuses.
pub fn bake_for(set: &ReferenceSet, program: Vec<u8>, uncarried_advice: &str) -> Result<Baked, Refused> {
    bake_for_inspecting(set, program, uncarried_advice, |_, _| {})
}

/// [`bake_for`], handing `inspect` the module the image is written from, as [`bake_inspecting`] does.
///
/// # Errors
/// As [`bake_for`].
pub fn bake_for_inspecting(
    set: &ReferenceSet,
    program: Vec<u8>,
    uncarried_advice: &str,
    inspect: impl FnOnce(&lamella_cil_runtime::Module, u8),
) -> Result<Baked, Refused> {
    set.refuse_unmet(&program, set.carried()).map_err(Refused::Unmet)?;
    bake_inspecting(program, set.carried(), set.board_buses(), uncarried_advice, inspect).map_err(Refused::Bake)
}

/// Bake a compiled assembly into a `.lmli` flash image, with the corlib and the libraries it was
/// compiled against.
///
/// **THEY GO INTO THE IMAGE, BECAUSE FIRMWARE HAS NONE TO OFFER.** A board runs the interpreter and
/// its intrinsics, and a call into a managed body -- `Thread.Sleep`, `Stack<int>`,
/// `GpioController.OpenPin`, a board class binding its buses -- needs that body to be somewhere. An
/// image of the program alone refused every such call, so a program that slept could not be sent
/// at all, and one that blinked a pin could not be sent either.
///
/// `references` is the set the program may draw on, the corlib first, and nothing is looked up a
/// second time here: a program bound against one assembly and baked with another would call members
/// the image may not carry under those names. Of that set the image carries the corlib and the
/// libraries the program references, directly or through one another.
///
/// Only what the program reaches is kept: the corlib members its code names and what those bodies
/// call in turn, and each library it references, all of it then trimmed to what `Main` can reach. A
/// program that sleeps and prints bakes to about five kilobytes.
///
/// The program is checked against the capability profile this build carries BEFORE an image is
/// written, so a body this tier cannot execute is refused here rather than on the board.
///
/// `board_buses` is the bus table of the board the image is built for
/// ([`crate::boards::bus_table`]), which the image carries for `System.Device`'s factories to find
/// each bus in. `None` writes no table, and every bus then reads as absent.
///
/// `uncarried_advice` closes a refusal naming an assembly the image cannot carry, as [`bake_for`]
/// takes it.
///
/// # Errors
/// When an assembly does not parse, the program calls a member that neither the assemblies it was
/// compiled against nor this runtime provides, the program exceeds the capability profile, the
/// board's bus table is not one this runtime reads, or the program cannot be laid out for flash.
/// Each refusal names the member or the fault.
pub fn bake(
    assembly: Vec<u8>,
    references: &[Vec<u8>],
    board_buses: Option<&[u32]>,
    uncarried_advice: &str,
) -> Result<Baked, String> {
    bake_inspecting(assembly, references, board_buses, uncarried_advice, |_, _| {})
}

/// [`bake`], handing `inspect` the module the image is written from, after the trim and before the
/// write, with the program's assembly id in it.
///
/// **FOR WHAT IS NUMBERED BY THE IMAGE's METHODS**, such as a debugger's source map: a method's id in
/// this module is its id in the image, and the trim empties a body without renumbering anything, so
/// a map built here names the methods the board runs.
///
/// # Errors
/// As [`bake`].
pub fn bake_inspecting(
    assembly: Vec<u8>,
    references: &[Vec<u8>],
    board_buses: Option<&[u32]>,
    uncarried_advice: &str,
    inspect: impl FnOnce(&lamella_cil_runtime::Module, u8),
) -> Result<Baked, String> {
    let Some((corlib, libraries)) = references.split_first() else {
        return Err("the compiler found no reference assemblies".to_owned());
    };
    let program = Assembly::read(Box::leak(assembly.into_boxed_slice()))
        .map_err(|error| format!("the emitted assembly does not parse: {error:?}"))?;
    let corlib = Assembly::read(leaked_once(corlib)).map_err(|error| {
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
    let assemblies: Vec<&Assembly<'static>> = core::iter::once(&corlib)
        .chain(libraries.iter())
        .chain(core::iter::once(&program))
        .collect();
    let (methods, types, strings) = loaded.module.reachable_set(Some(loaded.entry));
    let violations = loaded.module.validate_profile(Some(&methods));
    if !violations.is_empty() {
        return Err(profile_refusal(&violations, &loaded.module, &assemblies, uncarried_advice));
    }
    loaded.module.retain_reachable(&methods, &types, &strings);
    if let Some(table) = board_buses {
        loaded
            .module
            .set_board_buses(table)
            .map_err(|error| format!("bake: no image written: {error}"))?;
    }
    let carried = carried(&loaded.module, &assemblies, &methods);
    let program_id = u8::try_from(assemblies.len() - 1).map_err(|_| "bake: too many assemblies".to_owned())?;
    inspect(&loaded.module, program_id);
    let image = loaded
        .module
        .write_baked(Some(loaded.entry))
        .map_err(|error| format!("bake: {error:?}"))?;
    Ok(Baked { image, carried })
}

/// What an image of `module` carries from each of `assemblies`, indexed by assembly id: how many of
/// its methods `kept` keeps, and how many bytes of their code. An assembly the image keeps no method
/// of is left out.
///
/// **READ FROM THE MODULE THE IMAGE IS WRITTEN FROM**, after the trim, so each count is of what the
/// board runs rather than of what the assembly holds. A method the trim dropped keeps its place in
/// the image's numbering with an empty body, and is not counted.
fn carried(module: &lamella_cil_runtime::Module, assemblies: &[&Assembly], kept: &[bool]) -> Vec<Carried> {
    let mut counts = vec![(0_usize, 0_usize); assemblies.len()];
    let mut stored: std::collections::BTreeSet<&[u8]> = std::collections::BTreeSet::new();
    for id in 0..module.method_count() {
        if !kept.get(id).copied().unwrap_or(false) {
            continue;
        }
        let Ok(id) = lamella_cil_runtime::MethodId::try_from(id) else {
            break;
        };
        let Some(count) = counts.get_mut(usize::from(module.method_asm(id))) else {
            continue;
        };
        count.0 += 1;
        if let Some(code) = module.method_code_bytes(id)
            && stored.insert(code)
        {
            count.1 += code.len();
        }
    }
    let last = assemblies.len().saturating_sub(1);
    assemblies
        .iter()
        .zip(counts)
        .enumerate()
        .filter(|(_, (_, (methods, _)))| *methods > 0)
        .map(|(id, (assembly, (methods, code_bytes)))| Carried {
            name: assembly.assembly_name().unwrap_or("<unnamed>").to_owned(),
            role: match id {
                0 => Role::Corlib,
                id if id == last => Role::Program,
                _ => Role::Library,
            },
            methods,
            code_bytes,
        })
        .collect()
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
        .filter_map(|bytes| Assembly::read(leaked_once(bytes)).ok())
        .filter(|library| !library.assembly_name().is_some_and(|name| refusal::names_the_corlib(name, corlib)))
        .collect()
}

/// `bytes` with the `'static` lifetime a `code-in-place` load borrows its assemblies for, leaked
/// ONCE for each distinct content.
///
/// **A SERVER BAKES MANY PROGRAMS AGAINST ONE CLASS LIBRARY**, and a leak per bake of a corlib of
/// some 450 KB grew without bound in a process that stays up -- the MCP server's. Each reference is
/// held once and handed back to every later bake that brings the same bytes; the program, which
/// differs every time, is not held here.
fn leaked_once(bytes: &[u8]) -> &'static [u8] {
    static HELD: std::sync::Mutex<Vec<&'static [u8]>> = std::sync::Mutex::new(Vec::new());
    let mut held = HELD.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(found) = held.iter().find(|held| held.len() == bytes.len() && **held == bytes) {
        return found;
    }
    let leaked: &'static [u8] = Box::leak(bytes.to_vec().into_boxed_slice());
    held.push(leaked);
    leaked
}

/// The refusal text for a program whose baked image would not run on the device.
///
/// Every violation names the method it is in and the member it could not reach, so the list IS the
/// diagnosis ([`refusal::listed`]). `uncarried_advice` follows when one of them is in an assembly
/// the image does not carry.
fn profile_refusal(
    violations: &[ProfileViolation],
    module: &lamella_cil_runtime::Module,
    assemblies: &[&Assembly],
    uncarried_advice: &str,
) -> String {
    let corlib = assemblies.first().copied();
    let places = if violations.len() == 1 {
        String::from("one place")
    } else {
        format!("{} places", violations.len())
    };
    let mut text =
        format!("bake: no image written. This program would trap on the device in {places}:");
    text.push_str(&refusal::listed(violations, module, assemblies, corlib));
    text.push_str(
        "\n\nEach line names the method the board would stop in and what it could not do there. \
         The\nprogram can be sent once it no longer reaches them.",
    );
    if !refusal::uncarried_assemblies(violations, module, assemblies, corlib).is_empty() {
        text.push_str("\n\n");
        text.push_str(uncarried_advice);
    }
    text
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::engine::{LcscCompiler, ReplCompiler};
    use lamella_metadata::tables::table;

    /// What a refusal names an assembly the image does not carry with, in these tests.
    const ADVICE: &str = "Name the library.";

    /// The image `source` bakes to for `board`, compiled against the discovered reference set, or
    /// `None` where no corlib is discoverable.
    pub(crate) fn baked_for(source: &str, board: Option<&str>) -> Option<Result<Baked, String>> {
        let set = ReferenceSet::discover(board).ok()?;
        let program = set.compiler().compile(source).unwrap_or_else(|_| panic!("it compiles"));
        Some(bake_for(&set, program, ADVICE).map_err(Refused::into_text))
    }

    /// The image `source` bakes to for no board, as [`baked_for`] gives it.
    fn baked(source: &str) -> Option<Result<Baked, String>> {
        baked_for(source, None)
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

    /// A program reading a board's bus table through the two calls `System.Device.Gpio`'s
    /// factories make, from the fixture both crates' tests read.
    pub(crate) const READS_THE_BUS_TABLE: &str = include_str!("../tests/fixtures/ReadsTheBusTable.cs");

    /// **A PROGRAM THAT SLEEPS CAN BE SENT TO A BOARD.** `Thread.Sleep` is a managed corlib body
    /// over an intrinsic, and so is a five-piece string concatenation; an image of the program
    /// alone refused both, naming a token. Baked with the corlib, the image boots and prints what
    /// the program prints, and says it carries the corlib and the program.
    #[test]
    fn a_program_calling_managed_corlib_bodies_bakes_with_the_corlib_and_runs() {
        let Some(baked) = baked(
            "public static class Program\n{\n    public static void Main()\n    {\n        \
             System.Threading.Thread.Sleep(100);\n        int i = 3; int ms = 40;\n        \
             System.Console.WriteLine(\"tick \" + i + \" at +\" + ms + \" ms\");\n        \
             System.Console.WriteLine(\"slept\");\n    }\n}\n",
        ) else {
            return;
        };
        let baked = baked.expect("a program calling corlib bodies bakes");
        assert_eq!(&baked.image[..4], b"LML1", "a baked image");
        let roles: Vec<Role> = baked.carried.iter().map(|carried| carried.role).collect();
        assert_eq!(roles, [Role::Corlib, Role::Program], "{:?}", baked.carried);
        assert!(baked.carried.iter().all(|carried| carried.methods > 0 && carried.code_bytes > 0));
        let code: usize = baked.carried.iter().map(|carried| carried.code_bytes).sum();
        assert!(code < baked.image.len(), "the code is a part of the image: {code} of {}", baked.image.len());
        assert_eq!(run_baked(baked.image), "tick 3 at +40 ms\nslept\n");
    }

    /// **A GENERIC CORLIB TYPE IS BAKED AS ITS INSTANTIATION.** `Stack<int>` reaches the image as
    /// its own lowered copy, and the generic definition's template bodies -- which no image runs --
    /// are not held against the program.
    #[test]
    fn a_generic_corlib_type_and_a_stopwatch_bake_and_run() {
        let Some(baked) = baked(
            "using System.Collections.Generic;\nusing System.Diagnostics;\n\
             public static class Program\n{\n    public static void Main()\n    {\n        \
             Stopwatch watch = Stopwatch.StartNew();\n        Stack<int> stack = new Stack<int>();\n        \
             for (int i = 0; i < 5; i++) { stack.Push(i * i); }\n        \
             watch.Stop();\n        \
             System.Console.WriteLine(stack.Pop() + \" \" + stack.Count + \" \" + (watch.ElapsedMilliseconds >= 0));\n    }\n}\n",
        ) else {
            return;
        };
        let baked = baked.expect("a Stack<int> program bakes");
        assert_eq!(run_baked(baked.image), "16 4 True\n");
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
    /// library raises, and the program catches it. The image says it carries the library.
    #[test]
    fn a_call_into_a_library_is_baked_with_that_library_and_runs() {
        let Some(baked) = baked(GPIO_WITHOUT_A_BOARD) else {
            return;
        };
        let baked = baked.expect("a program calling into System.Device.Gpio bakes");
        assert!(
            baked.carried.iter().any(|carried| carried.role == Role::Library && carried.name == "System.Device.Gpio"),
            "{:?}",
            baked.carried
        );
        assert_eq!(run_baked(baked.image), "before\nthrew PlatformNotSupportedException\nafter\n");
    }

    /// A dotnet/iot program that names no board class: it opens a pin through `new GpioController()`
    /// alone, and says whether it could.
    const FACTORY_ALONE: &str = "using System;\nusing System.Device.Gpio;\n\npublic static class Program\n{\n    \
        public static void Main()\n    {\n        try\n        {\n            \
        GpioController controller = new GpioController();\n            controller.OpenPin(15, PinMode.Output);\n            \
        Console.WriteLine(\"gpio opened\");\n        }\n        catch (PlatformNotSupportedException)\n        {\n            \
        Console.WriteLine(\"gpio refused\");\n        }\n    }\n}\n";

    /// **UNMODIFIED dotnet/iot CODE OPENS A PIN FROM AN IMAGE BAKED FOR A BOARD.** Built for an
    /// RP2350 board, the image carries the board's table and its family's drivers, so
    /// `new GpioController()` alone finds the board's controller. Built for no board, it is refused,
    /// as .NET refuses it without a driver.
    #[test]
    fn a_dotnet_iot_controller_opens_from_the_table_of_the_board_an_image_is_built_for() {
        for (board, read) in [
            (Some("rpi-pico2-w"), "gpio opened\n"),
            (Some("pimoroni-pico-plus-2"), "gpio opened\n"),
            (None, "gpio refused\n"),
        ] {
            let Some(baked) = baked_for(FACTORY_ALONE, board) else {
                return;
            };
            let baked = baked.unwrap_or_else(|error| panic!("{board:?}: {error}"));
            assert_eq!(run_baked(baked.image), read, "{board:?}");
        }
    }

    /// **AN IMAGE FOR A BOARD CARRIES THAT BOARD's BUS TABLE, AND THE RUNTIME SERVES IT.** Read back
    /// through the two calls, the image built for each RP2350 board answers with the board's
    /// published words: I2C0 at 0x40090000 in eight words, SPI0 in twelve ending with its select,
    /// GP17, and the GPIO controller's reserved-pin mask, the radio's lines on the W boards. No
    /// board has an I2C1 row, so that bus answers 0. An image for no board carries no table, and
    /// every bus answers -1.
    #[test]
    fn an_image_for_a_board_carries_its_bus_table_and_one_for_none_carries_none() {
        for (board, read) in [
            (Some("rpi-pico2"), "8 1074331648 12 17 2 0 0\n"),
            (Some("rpi-pico2-w"), "8 1074331648 12 17 2 595591168 0\n"),
            (Some("pimoroni-pico-plus-2"), "8 1074331648 12 17 8 0 0\n"),
            (Some("pimoroni-pico-plus-2-w"), "8 1074331648 12 17 8 595591168 0\n"),
            (None, "-1 0 -1 0 -1 0 -1\n"),
        ] {
            let Some(baked) = baked_for(READS_THE_BUS_TABLE, board) else {
                return;
            };
            let baked = baked.unwrap_or_else(|error| panic!("{board:?}: {error}"));
            assert_eq!(run_baked(baked.image), read, "{board:?}");
        }
    }

    /// **A VIOLATION NAMES THE MEMBER AND THE METHOD, NEVER A TOKEN.** "calls token 0x0A000001,
    /// which resolves to nothing" was the whole refusal, and it gave the reader nothing to search
    /// for. The token is read in the calling method's own assembly, where it names the member.
    #[test]
    fn a_violation_names_the_caller_and_the_member_rather_than_a_token() {
        let Ok(compiler) = LcscCompiler::discover() else {
            return;
        };
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
        let refusal = profile_refusal(&[violation], &loaded.module, &[&corlib, &program], ADVICE);
        assert!(
            refusal.contains("Program::Main calls System.Threading.Thread::Sleep, which nothing in the image provides"),
            "{refusal}"
        );
        assert!(!refusal.contains("0x0A"), "it names no token: {refusal}");
        assert!(!refusal.contains(ADVICE), "no assembly is missing, so no advice: {refusal}");
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
        let references: Vec<Assembly> =
            compiler.references().iter().filter_map(|bytes| Assembly::read(bytes).ok()).collect();
        let options = lamella_syntax::lexer::LexOptions {
            target: lamella_syntax::lexer::OutputKind::Library,
            ..Default::default()
        };
        let impostor = lamella_assemble::compile_source_with(
            "namespace Elsewhere\n{\n    public static class Nothing\n    {\n        \
             public static int Zero() { return 0; }\n    }\n}\n",
            "mscorlib.cs",
            "mscorlib",
            "mscorlib",
            &references,
            false,
            options,
        )
        .image
        .expect("mscorlib compiles as a library");
        let corlib_bytes = compiler.references()[0].clone();
        let gpio = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../lamella-load/tests/fixtures/System.Device.Gpio.dll"),
        )
        .expect("the System.Device.Gpio fixture");
        let corlib = Assembly::read(leaked_once(&corlib_bytes)).expect("the corlib parses");
        let offered = [impostor, corlib_bytes.clone(), gpio];
        let names: Vec<Option<String>> = available_libraries(&corlib, &offered)
            .iter()
            .map(|library| library.assembly_name().map(String::from))
            .collect();
        assert_eq!(names, [Some(String::from("System.Device.Gpio"))]);
    }

    /// **A REFERENCE IS HELD ONCE, HOWEVER MANY BAKES BRING IT.** The same bytes come back as the
    /// same buffer; different bytes of the same length do not.
    #[test]
    fn a_reference_is_leaked_once_for_each_distinct_content() {
        let first = leaked_once(b"the same eleven");
        assert!(std::ptr::eq(first, leaked_once(b"the same eleven")), "held once");
        assert!(!std::ptr::eq(first, leaked_once(b"not the same 11")), "content, not length");
    }
}
