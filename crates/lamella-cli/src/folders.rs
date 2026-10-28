//! The two folders a build reads from, named on the command line: the class library and the runtime
//! support archives.

use std::path::{Path, PathBuf};

use lamella_wire_host::boards::ReferenceSet;
use lamella_wire_host::engine::LcscCompiler;

/// The option naming the class library's folder, in place of `LAMELLA_REF_DIR`.
pub const REF_DIR: &str = "--ref-dir";

/// The option naming the runtime support archives' folder, in place of
/// [`crate::tiers::FOLDER_ENV`].
pub const RUNTIME_SUPPORT_DIR: &str = "--runtime-support-dir";

/// The folders a command line names. A folder it does not name is found as the tool finds it
/// without one: from its variable, then where the tool looks by itself.
#[derive(Debug, Default)]
pub struct Folders {
    ref_dir: Option<PathBuf>,
    runtime_support_dir: Option<PathBuf>,
}

impl Folders {
    /// The folders `parsed` names, each checked as [`Self::checked`] checks it.
    ///
    /// # Errors
    /// A value that names no folder.
    pub fn from_options(parsed: &crate::args::Options, verb: &str) -> Result<Self, String> {
        Self::checked(verb, parsed.value(REF_DIR), parsed.value(RUNTIME_SUPPORT_DIR))
    }

    /// The folders `ref_dir` and `runtime_support_dir` name, the values given for [`REF_DIR`] and
    /// [`RUNTIME_SUPPORT_DIR`].
    ///
    /// **EACH IS CHECKED HERE, BEFORE ANY WORK.** A folder that is not there would otherwise be met
    /// after a compile, or after a board was opened, or not at all on a route that never reads it.
    ///
    /// # Errors
    /// A value that names no folder, in a sentence that names `verb`, the option and the value.
    pub fn checked(verb: &str, ref_dir: Option<&str>, runtime_support_dir: Option<&str>) -> Result<Self, String> {
        Ok(Self {
            ref_dir: ref_dir
                .map(|value| folder(verb, REF_DIR, value, "corlib.dll and the libraries a program is compiled against"))
                .transpose()?,
            runtime_support_dir: runtime_support_dir
                .map(|value| folder(verb, RUNTIME_SUPPORT_DIR, value, "the runtime support archives an image for a board is linked with"))
                .transpose()?,
        })
    }

    /// The compiler over the class library: the one [`REF_DIR`] names, or else the one the tool finds
    /// without it ([`LcscCompiler::discover_from`]).
    ///
    /// # Errors
    /// No class library to compile against, naming [`REF_DIR`] beside its variable.
    pub fn compiler(&self) -> Result<LcscCompiler, String> {
        LcscCompiler::discover_from(self.ref_dir.as_deref(), REF_DIR)
    }

    /// The reference set a build for `board` compiles against, from [`Self::compiler`]'s class
    /// library.
    ///
    /// # Errors
    /// As [`Self::compiler`].
    pub fn reference_set(&self, board: Option<&str>) -> Result<ReferenceSet, String> {
        Ok(ReferenceSet::for_board(self.compiler()?, board))
    }

    /// The runtime support archives' folder [`RUNTIME_SUPPORT_DIR`] names, if it named one.
    #[must_use]
    pub fn runtime_support_dir(&self) -> Option<&Path> {
        self.runtime_support_dir.as_deref()
    }
}

/// `value` as a folder, or the refusal that says `option` names none, and what the folder holds.
fn folder(verb: &str, option: &str, value: &str, holds: &str) -> Result<PathBuf, String> {
    let path = PathBuf::from(value);
    if path.is_dir() {
        return Ok(path);
    }
    Err(format!(
        "lamella {verb}: {option} names \"{value}\", and there is no folder there.\n\
         It names the folder holding {holds}.\n\nNothing was run, sent or written."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch folder of this test's own.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lamella-folders-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch folder");
        dir
    }

    /// **A VALUE THAT NAMES NO FOLDER IS REFUSED, NAMING THE OPTION AND THE VALUE**: a path where
    /// nothing is, and a file, which is not a folder either.
    #[test]
    fn a_value_that_names_no_folder_is_refused_by_option_and_value() {
        let dir = scratch("refused");
        let missing = dir.join("absent");
        let file = dir.join("corlib.dll");
        std::fs::write(&file, b"a file").expect("a file");
        for (option, value) in [(REF_DIR, &missing), (RUNTIME_SUPPORT_DIR, &file)] {
            let value = value.display().to_string();
            let (ref_dir, runtime_support_dir) =
                if option == REF_DIR { (Some(value.as_str()), None) } else { (None, Some(value.as_str())) };
            let error = Folders::checked("run", ref_dir, runtime_support_dir).expect_err("no folder there");
            assert!(
                error.starts_with(&format!("lamella run: {option} names \"{value}\", and there is no folder there.")),
                "{error}"
            );
            assert!(error.contains("Nothing was run, sent or written."), "{error}");
            crate::rendered::assert_renders_cleanly(&error, crate::rendered::four_space_sample);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **A FOLDER THAT IS THERE IS TAKEN AS GIVEN**, and an option not given names nothing.
    #[test]
    fn a_folder_that_is_there_is_taken_and_one_not_given_names_nothing() {
        let dir = scratch("taken");
        let value = dir.display().to_string();
        let folders = Folders::checked("run", None, Some(&value)).expect("a folder");
        assert_eq!(folders.runtime_support_dir(), Some(dir.as_path()));
        assert_eq!(folders.ref_dir, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **THE CLASS LIBRARY IS READ FROM THE FOLDER GIVEN**: a folder holding no corlib is refused
    /// by its path, where the search would otherwise have found the test corlib.
    #[test]
    fn the_class_library_is_read_from_the_folder_given() {
        let dir = scratch("no-corlib");
        let value = dir.display().to_string();
        let folders = Folders::checked("run", Some(&value), None).expect("a folder");
        let error = folders.compiler().err().expect("a folder with no corlib holds no class library");
        assert!(error.contains(&value) && error.contains("no corlib.dll"), "{error}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
