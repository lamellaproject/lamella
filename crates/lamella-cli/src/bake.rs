//! Baking a compiled assembly into the flash image a device runs.

use lamella_wire_host::bake as shared;

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
/// its family's builds ([`lamella_wire_host::boards`]). `None` builds for no board. The class
/// library is the one `folders` names, or else the one the tool finds without it.
///
/// # Errors
/// A file that is not C#, one that cannot be read, a project that is a class library or names a
/// library that cannot be read, no compiler, the compiler's diagnostics, or any bake failure.
pub fn image_for_firmware(
    path: &std::path::Path,
    verb: &str,
    refusal: impl Fn(&std::path::Path, &crate::deploy::Uncompilable) -> String,
    board: Option<&str>,
    folders: &crate::folders::Folders,
) -> Result<Vec<u8>, String> {
    if crate::flash::is_project(path) {
        let project = crate::program::runnable_project(path, verb)?;
        let libraries = crate::flash::project_references(&project, verb)?;
        let (assembly, references) = crate::program::compile_project_assembly_with_references(
            &project, &libraries, board, verb, folders,
        )?;
        return bake(assembly, &references, lamella_wire_host::boards::bus_table(board));
    }
    refuse_uncompilable(path, refusal)?;
    let source = crate::text::read_text(path)
        .map_err(|error| format!("lamella {verb}: read {}: {error}", path.display()))?;
    let (assembly, references) =
        crate::program::compile_csharp_assembly_with_references(path, &source, false, &[], board, verb, folders)?;
    bake(assembly, &references, lamella_wire_host::boards::bus_table(board))
}

/// The baked image of `path`, an assembly a build already made, for the board that answered.
///
/// **ITS LIBRARIES ARE THE CLASS LIBRARY'S AND THE ONES BESIDE IT**, as a .NET build leaves a
/// program's own libraries beside it: every library the program references that the class library
/// does not hold is read from the program's folder, and the libraries those reference with it. The
/// class library is the one `folders` names, or else the one the tool finds without it.
///
/// # Errors
/// An assembly that cannot be read, a library it references that is in neither place, a program
/// the set cannot build for the board, or any bake failure.
pub fn image_of_built(
    path: &std::path::Path,
    verb: &str,
    board: Option<&str>,
    folders: &crate::folders::Folders,
) -> Result<Vec<u8>, String> {
    let read = |path: &std::path::Path| {
        std::fs::read(path).map_err(|error| format!("lamella {verb}: read {}: {error}", path.display()))
    };
    let assembly = read(path)?;
    let set = folders.reference_set(board).map_err(|error| format!("lamella {verb}: {error}"))?;
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
mod tests {
    use super::*;

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
}

