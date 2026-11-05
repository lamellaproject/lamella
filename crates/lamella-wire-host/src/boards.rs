//! A build for a board: the reference set it compiles against, with the board's chip-family builds and
//! its own facts in place of the shared ones, the bus table its image carries, and the board that
//! answers a HELLO.

use crate::engine::{LcscCompiler, ReplCompiler};
use lamella_metadata::Assembly;

/// The class library a chip family builds with its bus drivers, which read the table.
pub const BUS_LIBRARY: &str = "System.Device.Gpio";

/// The library every board's facts are built as, and the class library's boardless one: a build for
/// a board takes the board's in the boardless one's place, and `Lamella.Hardware.TargetBoard` asks it
/// which board the program was built for.
pub const FACTS_LIBRARY: &str = "Lamella.Hardware.Facts";

/// The bus table an image built for `board` carries, as the words `Module::set_board_buses` and the
/// class-library link take.
///
/// `None` for a build that names no board, and for a board whose family has no table. An image
/// built so carries none, and every bus then reads as absent.
#[must_use]
pub fn bus_table(board: Option<&str>) -> Option<&'static [u32]> {
    board.and_then(lamella_catalog::bus_table)
}

/// The board this build knows that answers a HELLO as product model `model`: the one board whose
/// facts declare that model. `None` for a board that names no model, and for a model no board this
/// build knows declares.
///
/// A route to firmware already running builds its image for this board, so the image carries the
/// bus table of the board that answered.
#[must_use]
pub fn board_answering(model: u16) -> Option<String> {
    if model == lamella_wire::product_model::UNKNOWN {
        return None;
    }
    let mut answering = lamella_catalog::BOARDS
        .iter()
        .filter(|(id, _)| lamella_catalog::load_board(id).is_some_and(|row| row.product_model == i64::from(model)))
        .map(|(id, _)| (*id).to_owned());
    let board = answering.next()?;
    answering.next().is_none().then_some(board)
}

/// The reference set a build compiles against: the discovered one, and for a build that names a
/// board, with that board's family builds in place of the shared assemblies of the same names.
pub struct ReferenceSet {
    /// The compiler, over the corlib and every library a program may bind against.
    compiler: LcscCompiler,
    /// For a set [`Self::naming`] cut to the libraries a program names, the set it was cut from.
    /// `None` for a set a program binds against whole.
    whole: Option<LcscCompiler>,
    /// The board the build is for, when it names one.
    board: Option<String>,
    /// The chip family whose builds stand in for shared ones in this set, and the names of the
    /// assemblies they stand in for.
    family_builds: Option<(String, Vec<String>)>,
    /// The board's chip family, when the board's image carries a bus table and the set's folder
    /// holds no family build of [`BUS_LIBRARY`] for it.
    unmet: Option<String>,
    /// Whether the set's folder holds the boardless [`FACTS_LIBRARY`] and no build of it for the
    /// board, so a program that uses `Lamella.Hardware` would be told it was built for no board.
    facts_unmet: bool,
}

impl ReferenceSet {
    /// The discovered reference set, for `board` when the build names one.
    ///
    /// # Errors
    /// No class library to compile against.
    pub fn discover(board: Option<&str>) -> Result<Self, String> {
        Ok(Self::for_board(LcscCompiler::discover()?, board))
    }

    /// `compiler`'s set, for `board` when the build names one: the board's family builds and then
    /// the board's own builds in place of the shared ones, a board's own winning where both build a
    /// library.
    #[must_use]
    pub fn for_board(compiler: LcscCompiler, board: Option<&str>) -> Self {
        let row = board.and_then(lamella_catalog::load_board);
        let (Some(board), Some(row)) = (board, row) else {
            return Self {
                compiler,
                whole: None,
                board: board.map(str::to_owned),
                family_builds: None,
                unmet: None,
                facts_unmet: false,
            };
        };
        let family = lamella_catalog::family_of(&row);
        let (compiler, mut replaced) = match &family {
            Some(family) => compiler.for_family(family),
            None => (compiler, Vec::new()),
        };
        let (compiler, own) = compiler.for_board_folder(board);
        let facts_unmet = !own.iter().any(|name| name == FACTS_LIBRARY)
            && compiler.library_names().iter().any(|name| name == FACTS_LIBRARY);
        for name in own.into_iter().filter(|name| name != FACTS_LIBRARY) {
            if !replaced.contains(&name) {
                replaced.push(name);
            }
        }
        let met = replaced.iter().any(|name| name == BUS_LIBRARY);
        let unmet = family.clone().filter(|_| bus_table(Some(board)).is_some() && !met);
        let family_builds = family.filter(|_| !replaced.is_empty()).map(|family| (family, replaced));
        Self { compiler, whole: None, board: Some(board.to_owned()), family_builds, unmet, facts_unmet }
    }

    /// This set cut to the corlib and the libraries `names` names, as a .NET project names the
    /// libraries it references: a program compiled against it binds against those alone.
    ///
    /// **THE CORLIB IS IMPLICIT.** A name the corlib answers to is taken as naming it, and changes
    /// nothing. Every other name has to be a library the set holds, matched as .NET matches an
    /// assembly name, without regard to case.
    ///
    /// A bake against the cut set may still carry the libraries a named one uses, as a .NET build
    /// copies a reference's own references beside it: [`Self::carried`] is the whole set.
    ///
    /// # Errors
    /// A name that is neither the corlib's nor a library's in the set.
    pub fn naming(self, names: &[String]) -> Result<Self, String> {
        let offered = self.compiler.library_names();
        let corlib = self.compiler.references().first().and_then(|bytes| Assembly::read(bytes).ok());
        let mut kept: Vec<String> = Vec::new();
        for name in names {
            if corlib.as_ref().is_some_and(|corlib| crate::refusal::names_the_corlib(name, corlib)) {
                continue;
            }
            let Some(library) = offered.iter().find(|offered| offered.eq_ignore_ascii_case(name)) else {
                let mut listed = offered.clone();
                listed.sort();
                return Err(format!(
                    "no library named {name:?} is in the class library at {}. It holds:\n    {}\n\n\
                     The corlib is always referenced, so it is never named.",
                    self.compiler.folder().display(),
                    listed.join(", ")
                ));
            };
            if !kept.contains(library) {
                kept.push(library.clone());
            }
        }
        let cut = self.compiler.keeping(|name| kept.iter().any(|kept| kept == name));
        let whole = self.whole.unwrap_or(self.compiler);
        Ok(Self { compiler: cut, whole: Some(whole), ..self })
    }

    /// The compiler, over this set.
    #[must_use]
    pub fn compiler(&self) -> &LcscCompiler {
        &self.compiler
    }

    /// This set, its compiler reporting against `path`, the file the source was read from.
    #[must_use]
    pub fn for_source_file(self, path: &str) -> Self {
        Self {
            compiler: self.compiler.for_source_file(path),
            whole: self.whole.map(|whole| whole.for_source_file(path)),
            ..self
        }
    }

    /// The board this set builds for, when it names one.
    #[must_use]
    pub fn board(&self) -> Option<&str> {
        self.board.as_deref()
    }

    /// The bus table an image built against this set carries: its board's.
    #[must_use]
    pub fn board_buses(&self) -> Option<&'static [u32]> {
        bus_table(self.board.as_deref())
    }

    /// Every assembly a bake against this set may carry, the corlib first: the whole set, of which
    /// a bake takes the libraries a program uses and the ones those use in turn.
    #[must_use]
    pub fn carried(&self) -> &[Vec<u8>] {
        self.whole.as_ref().unwrap_or(&self.compiler).references()
    }

    /// The libraries a program compiled against this set may name: every library the set holds, or
    /// the ones a set cut by [`Self::naming`] names.
    #[must_use]
    pub fn named(&self) -> Vec<String> {
        self.compiler.library_names()
    }

    /// Every library the class library offers for this set's board, whether or not the set names it.
    #[must_use]
    pub fn offered(&self) -> Vec<String> {
        self.whole.as_ref().unwrap_or(&self.compiler).library_names()
    }

    /// The libraries the class library holds that some chip family builds its own of, such as
    /// [`BUS_LIBRARY`]. An image built for no board carries the build every family shares, which
    /// has none of a family's drivers.
    #[must_use]
    pub fn built_per_family(&self) -> Vec<String> {
        self.whole.as_ref().unwrap_or(&self.compiler).built_per_family()
    }

    /// The chip family whose build of `library` this set holds, or `None` where it holds the build
    /// every family shares.
    #[must_use]
    pub fn family_build_of(&self, library: &str) -> Option<&str> {
        let (family, replaced) = self.family_builds.as_ref()?;
        replaced.iter().any(|name| name == library).then_some(family.as_str())
    }

    /// The libraries `source` binds against that a set cut by [`Self::naming`] does not name, in
    /// the order the program references them: what a .NET build would say is a missing reference.
    ///
    /// Found by compiling `source` against the whole set and reading which libraries the program it
    /// makes references, so each one is a library the program really uses, never a guess from a
    /// type's name. Empty for a set nobody cut, and for a source that does not compile against the
    /// whole set either, whose diagnostics say what is wrong without this.
    #[must_use]
    pub fn unnamed_libraries_in(&self, source: &str) -> Vec<String> {
        let Some(whole) = &self.whole else {
            return Vec::new();
        };
        let Ok(program) = whole.compile(source) else {
            return Vec::new();
        };
        let Ok(program) = Assembly::read(&program) else {
            return Vec::new();
        };
        let (named, offered) = (self.named(), whole.library_names());
        let mut unnamed: Vec<String> = Vec::new();
        for reference in program.assembly_refs() {
            if let Some(name) = reference.name()
                && offered.iter().any(|offered| offered == name)
                && !named.iter().any(|named| named == name)
                && !unnamed.iter().any(|unnamed| unnamed == name)
            {
                unnamed.push(name.to_owned());
            }
        }
        unnamed
    }

    /// Refuse a program that would take the shared build of [`BUS_LIBRARY`] into an image for a
    /// board whose buses only its family's build reaches, or the boardless [`FACTS_LIBRARY`] into an
    /// image for a board whose own facts the set's folder does not hold.
    ///
    /// `program` is the compiled program and `references` the set it may draw on, the corlib first:
    /// [`Self::carried`], with any library of the program's own beside it. A program that does not
    /// use the library, directly or through a library it uses, is not refused: its image needs no
    /// family build, or no facts.
    ///
    /// # Errors
    /// The program uses the library, and the set's folder holds no family build of it, or no facts
    /// for the board.
    pub fn refuse_unmet(&self, program: &[u8], references: &[Vec<u8>]) -> Result<(), String> {
        let Some(board) = &self.board else {
            return Ok(());
        };
        if self.unmet.is_none() && !self.facts_unmet {
            return Ok(());
        }
        let Ok(program) = Assembly::read(program) else {
            return Ok(());
        };
        let available: Vec<Assembly> =
            references.iter().skip(1).filter_map(|bytes| Assembly::read(bytes).ok()).collect();
        let used: Vec<&str> = Assembly::referenced_assemblies(&program, &available)
            .into_iter()
            .filter_map(|position| available[position].assembly_name())
            .collect();
        if let Some(family) = &self.unmet
            && used.contains(&BUS_LIBRARY)
        {
            let wanted = self
                .compiler
                .folder()
                .join("families")
                .join(family)
                .join(format!("{BUS_LIBRARY}.dll"));
            return Err(format!(
                "the program uses {BUS_LIBRARY}, and the class library it was compiled against has\nno \
                 build of it for the {family} chip family, which {board} carries. That build holds the\n\
                 family's bus drivers, and without them the program cannot open a bus on {board}.\n\n\
                 It is missing from:\n    {}\n\n\
                 {}\n\n\
                 Nothing was built.",
                wanted.display(),
                restore_family_build(cfg!(feature = "packaged"))
            ));
        }
        if self.facts_unmet && used.contains(&FACTS_LIBRARY) {
            let wanted = self
                .compiler
                .folder()
                .join("boards")
                .join(board)
                .join(format!("{FACTS_LIBRARY}.dll"));
            return Err(format!(
                "the program uses Lamella.Hardware, and the class library it was compiled against has\nno \
                 facts for {board}: the {FACTS_LIBRARY} that tells TargetBoard which board the program\n\
                 was built for. Without it, the program would be told it was built for no board.\n\n\
                 It is missing from:\n    {}\n\n\
                 {}\n\n\
                 Nothing was built.",
                wanted.display(),
                restore_board_facts(cfg!(feature = "packaged"))
            ));
        }
        Ok(())
    }
}

/// What to do about a class library missing part of what a release ships it with.
///
/// **A PACKAGED TOOL NEVER NAMES A SCRIPT**: the scripts are in the source tree, and what the reader has is
/// an archive or an SDK package, either of which holds the file.
const RESTORE_WHAT_IT_CAME_WITH: &str = "The class library that comes with this tool holds it, so this copy is not whole. If it came in\n\
     this tool's archive, unpack the archive again into an empty folder. If it came with Lamella.Sdk,\n\
     delete that package from the NuGet folder and restore the project again.";

/// What a refusal says to do about a missing family build, in a packaged build or a source tree's.
fn restore_family_build(packaged: bool) -> &'static str {
    if packaged {
        RESTORE_WHAT_IT_CAME_WITH
    } else {
        "csharp/build-managed.ps1 (Windows) and csharp/build-managed.sh (Linux, macOS) write it beside the rest of the\n\
         class library. Rebuild the class library with either."
    }
}

/// What a refusal says to do about a board's missing facts, in a packaged build or a source tree's.
fn restore_board_facts(packaged: bool) -> &'static str {
    if packaged {
        RESTORE_WHAT_IT_CAME_WITH
    } else {
        "csharp/build-boards.ps1 writes it there, after csharp/build-managed.ps1 (Windows) or\n\
         csharp/build-managed.sh (Linux, macOS) writes the class library."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **WHAT A PACKAGED TOOL SAYS NAMES NO SCRIPT, AND A SOURCE TREE'S NAMES THE ONE THAT WRITES THE FILE.**
    #[test]
    fn a_refusal_names_a_script_only_in_a_source_tree() {
        for said in [restore_family_build(true), restore_board_facts(true)] {
            assert!(!said.contains(".ps1") && !said.contains(".sh") && !said.contains("csharp/"), "{said}");
            assert!(said.contains("unpack the archive again") && said.contains("restore the project"), "{said}");
        }
        assert!(restore_family_build(false).contains("csharp/build-managed.ps1"));
        assert!(restore_board_facts(false).contains("csharp/build-boards.ps1"));
    }

    /// **EVERY PRODUCT MODEL A BOARD DECLARES ANSWERS AS THAT BOARD**, so a route to firmware
    /// already running builds its image for the board that answered. No two boards declare one
    /// model, which would leave the answer naming neither; a model no board declares, and the
    /// unknown model, name none.
    #[test]
    fn every_declared_product_model_answers_as_its_own_board() {
        let mut declared = 0;
        for (id, _) in lamella_catalog::BOARDS {
            let row = lamella_catalog::load_board(id).unwrap_or_else(|| panic!("{id}: board.toml parses"));
            let Ok(model) = u16::try_from(row.product_model) else { continue };
            if model == lamella_wire::product_model::UNKNOWN {
                continue;
            }
            assert_eq!(board_answering(model).as_deref(), Some(*id), "product model {model}");
            declared += 1;
        }
        assert!(declared > 20, "the census reached the catalog: {declared} models");
        assert_eq!(board_answering(lamella_wire::product_model::PICO2_W).as_deref(), Some("raspberrypi-pico-2-w"));
        assert_eq!(board_answering(lamella_wire::product_model::UNKNOWN), None);
        assert_eq!(board_answering(u16::MAX), None);
    }
}

