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
mod tests {
    use super::*;

    /// **A REFERENCE IS HELD ONCE, HOWEVER MANY BAKES BRING IT.** The same bytes come back as the
    /// same buffer; different bytes of the same length do not.
    #[test]
    fn a_reference_is_leaked_once_for_each_distinct_content() {
        let first = leaked_once(b"the same eleven");
        assert!(std::ptr::eq(first, leaked_once(b"the same eleven")), "held once");
        assert!(!std::ptr::eq(first, leaked_once(b"not the same 11")), "content, not length");
    }
}

