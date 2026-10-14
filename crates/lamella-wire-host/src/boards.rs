//! A build for a board: the reference set it compiles against, with the board's chip-family builds in
//! place of the shared ones, the bus table its image carries, and the board that answers a HELLO.

use crate::engine::{LcscCompiler, ReplCompiler};
use lamella_metadata::Assembly;

/// The class library a chip family builds with its bus drivers, which read the table.
pub const BUS_LIBRARY: &str = "System.Device.Gpio";

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
}

impl ReferenceSet {
    /// The discovered reference set, for `board` when the build names one.
    ///
    /// # Errors
    /// No class library to compile against.
    pub fn discover(board: Option<&str>) -> Result<Self, String> {
        Ok(Self::for_board(LcscCompiler::discover()?, board))
    }

    /// `compiler`'s set, for `board` when the build names one.
    #[must_use]
    pub fn for_board(compiler: LcscCompiler, board: Option<&str>) -> Self {
        let family = board
            .and_then(lamella_catalog::load_board)
            .and_then(|row| lamella_catalog::family_of(&row));
        let (Some(board), Some(family)) = (board, family) else {
            return Self { compiler, whole: None, board: board.map(str::to_owned), family_builds: None, unmet: None };
        };
        let (compiler, replaced) = compiler.for_family(&family);
        let met = replaced.iter().any(|name| name == BUS_LIBRARY);
        let unmet = (bus_table(Some(board)).is_some() && !met).then(|| family.clone());
        let family_builds = (!replaced.is_empty()).then_some((family, replaced));
        Self { compiler, whole: None, board: Some(board.to_owned()), family_builds, unmet }
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
    /// board whose buses only its family's build reaches.
    ///
    /// `program` is the compiled program and `references` the set it may draw on, the corlib first:
    /// [`Self::carried`], with any library of the program's own beside it. A program that does not
    /// use the library, directly or through a library it uses, is not refused: its image needs no
    /// family build.
    ///
    /// # Errors
    /// The program uses the library, and the set's folder holds no family build of it.
    pub fn refuse_unmet(&self, program: &[u8], references: &[Vec<u8>]) -> Result<(), String> {
        let (Some(board), Some(family)) = (&self.board, &self.unmet) else {
            return Ok(());
        };
        let Ok(program) = Assembly::read(program) else {
            return Ok(());
        };
        let available: Vec<Assembly> =
            references.iter().skip(1).filter_map(|bytes| Assembly::read(bytes).ok()).collect();
        let uses = Assembly::referenced_assemblies(&program, &available)
            .into_iter()
            .any(|position| available[position].assembly_name() == Some(BUS_LIBRARY));
        if !uses {
            return Ok(());
        }
        let wanted = self
            .compiler
            .folder()
            .join("families")
            .join(family)
            .join(format!("{BUS_LIBRARY}.dll"));
        Err(format!(
            "the program uses {BUS_LIBRARY}, and the class library it was compiled against has\nno \
             build of it for the {family} chip family, which {board} carries. That build holds the\n\
             family's bus drivers, and without them the program cannot open a bus on {board}.\n\n\
             It is missing from:\n    {}\n\n\
             build-managed.ps1 (Windows) and build-managed.sh (Linux, macOS) write it beside the \
             rest of the\nclass library. Rebuild the class library with either.\n\n\
             Nothing was built.",
            wanted.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixture folder the test build's reference set comes from.
    fn fixtures() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../lamella-load/tests/fixtures")
    }

    /// A scratch folder of the test's own, empty.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lamella-boards-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir
    }

    /// A class-library folder holding the fixture corlib, `System.Device.Gpio` and
    /// `Lamella.Hardware`, and for `family`, a family build: the shared `System.Device.Gpio` with
    /// four bytes after its last section, which no reader of it reads, so the set can tell the two
    /// apart.
    fn class_library(name: &str, family: Option<&str>) -> Option<(std::path::PathBuf, Option<Vec<u8>>)> {
        if !fixtures().join("corlib.dll").is_file() {
            return None;
        }
        let dir = scratch(name);
        for file in ["corlib.dll", "System.Device.Gpio.dll", "Lamella.Hardware.dll"] {
            std::fs::copy(fixtures().join(file), dir.join(file)).expect("stage the class library");
        }
        let built = family.map(|family| {
            let mut bytes = std::fs::read(dir.join("System.Device.Gpio.dll")).expect("the shared build");
            bytes.extend_from_slice(b"FAMB");
            let folder = dir.join("families").join(family);
            std::fs::create_dir_all(&folder).expect("create families/<family>");
            std::fs::write(folder.join("System.Device.Gpio.dll"), &bytes).expect("write the family build");
            bytes
        });
        Some((dir, built))
    }

    /// `source` compiled against `set`.
    fn compiled(set: &ReferenceSet, source: &str) -> Vec<u8> {
        set.compiler().compile(source).unwrap_or_else(|_| panic!("it compiles"))
    }

    const USES_GPIO: &str = "using System.Device.Gpio;\npublic static class Program\n{\n    \
        public static void Main()\n    {\n        GpioController controller = new GpioController();\n    \
        }\n}\n";

    const PRINTS: &str = "public static class Program\n{\n    public static void Main()\n    {\n        \
        System.Console.WriteLine(\"hello\");\n    }\n}\n";

    /// **A BUILD FOR A BOARD COMPILES AGAINST ITS FAMILY's BUILD**, which stands in for the shared
    /// one; a build that names no board, or a board of another family, keeps the shared set.
    #[test]
    fn a_build_for_a_board_takes_its_familys_build_of_the_bus_library() {
        let Some((dir, built)) = class_library("family", Some("rp2350")) else {
            return;
        };
        let built = built.expect("a family build");
        let shared = std::fs::read(dir.join("System.Device.Gpio.dll")).expect("the shared build");
        let set = |board: Option<&str>| {
            ReferenceSet::for_board(LcscCompiler::in_folder(&dir).expect("the set"), board)
        };

        let pico = set(Some("rpi-pico2-w"));
        assert!(pico.compiler().references().contains(&built), "the family build is in the set");
        assert!(!pico.compiler().references().contains(&shared), "in the shared build's place");
        assert_eq!(pico.board_buses(), lamella_catalog::bus_table("rpi-pico2-w"));
        assert!(pico.board_buses().is_some(), "and the image carries the board's table");
        assert_eq!(pico.family_build_of(BUS_LIBRARY), Some("rp2350"), "and names the build it holds");
        assert_eq!(pico.refuse_unmet(&compiled(&pico, USES_GPIO), pico.carried()), Ok(()));

        for other in [None, Some("bbc-micro-bit-v2")] {
            let kept = set(other);
            assert!(kept.compiler().references().contains(&shared), "{other:?} keeps the shared build");
            assert_eq!(kept.board_buses(), None, "{other:?} carries no table");
            assert_eq!(kept.family_build_of(BUS_LIBRARY), None, "{other:?} holds the shared build");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A PROGRAM THAT USES THE BUS LIBRARY IS REFUSED WHERE ITS BOARD's FAMILY BUILD IS MISSING**,
    /// naming the file and the step that writes it. One that does not use it is built: its image
    /// needs no family build.
    #[test]
    fn a_missing_family_build_refuses_only_a_program_that_uses_the_bus_library() {
        let Some((dir, _)) = class_library("unmet", None) else {
            return;
        };
        let set = ReferenceSet::for_board(LcscCompiler::in_folder(&dir).expect("the set"), Some("rpi-pico2"));

        let refusal =
            set.refuse_unmet(&compiled(&set, USES_GPIO), set.carried()).expect_err("the family build is missing");
        let wanted = dir.join("families").join("rp2350").join("System.Device.Gpio.dll");
        assert!(refusal.starts_with("the program uses System.Device.Gpio"), "{refusal}");
        assert!(refusal.contains("for the rp2350 chip family, which rpi-pico2"), "{refusal}");
        assert!(refusal.contains(&wanted.display().to_string()), "{refusal}");
        assert!(refusal.contains("build-managed.ps1") && refusal.ends_with("Nothing was built."), "{refusal}");

        assert_eq!(set.refuse_unmet(&compiled(&set, PRINTS), set.carried()), Ok(()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A SET CUT TO THE LIBRARIES A PROGRAM NAMES BINDS AGAINST THOSE ALONE**, the corlib always
    /// among them: a program using `System.Device.Gpio` compiles where the set names it and not
    /// where it does not, and the missing one is named, as a .NET build names a missing reference.
    /// A bake may still carry the whole set.
    #[test]
    fn a_set_cut_to_named_libraries_binds_those_alone_and_names_a_missing_one() {
        let Some((dir, _)) = class_library("named", Some("rp2350")) else {
            return;
        };
        let set = |names: &[&str]| {
            let names: Vec<String> = names.iter().map(|name| (*name).to_owned()).collect();
            ReferenceSet::for_board(LcscCompiler::in_folder(&dir).expect("the set"), Some("rpi-pico2-w"))
                .naming(&names)
        };

        let named = set(&["system.device.gpio", "mscorlib"]).expect("a library and the corlib's name");
        assert_eq!(named.named(), [String::from(BUS_LIBRARY)], "matched without regard to case");
        assert_eq!(named.compiler().references().len(), 2, "the corlib and the one library");
        assert_eq!(named.carried().len(), 3, "a bake may carry the whole set");
        assert!(named.compiler().compile(USES_GPIO).is_ok(), "the program binds");
        assert_eq!(named.family_build_of(BUS_LIBRARY), Some("rp2350"));

        let bare = set(&[]).expect("no library named");
        assert!(bare.named().is_empty());
        assert!(bare.compiler().compile(USES_GPIO).is_err(), "the program does not bind without it");
        assert_eq!(bare.unnamed_libraries_in(USES_GPIO), [String::from(BUS_LIBRARY)]);
        assert!(bare.unnamed_libraries_in(PRINTS).is_empty(), "a program using none misses none");
        assert!(bare.compiler().compile(PRINTS).is_ok(), "and binds against the corlib alone");

        let refusal = set(&["System.Device.Gpioo"]).err().expect("no such library");
        assert!(refusal.starts_with("no library named \"System.Device.Gpioo\""), "{refusal}");
        assert!(refusal.contains("Lamella.Hardware, System.Device.Gpio"), "it lists what there is: {refusal}");
        let _ = std::fs::remove_dir_all(&dir);
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
        assert_eq!(board_answering(lamella_wire::product_model::PICO2_W).as_deref(), Some("rpi-pico2-w"));
        assert_eq!(board_answering(lamella_wire::product_model::UNKNOWN), None);
        assert_eq!(board_answering(u16::MAX), None);
    }
}
