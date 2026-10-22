//! Where the boards live: one directory per board under `bsp/boards/`, named by the board's id.
//!
//! Every Rust reader that lists the boards lists them through [`board_dirs`], so which directories
//! are boards is stated once. `csharp/build-boards.ps1 -PrintBoards` states the same list for PowerShell.

use std::path::{Path, PathBuf};

/// The directory that holds the boards, relative to the repository root.
pub const BOARDS_DIR: &str = "bsp/boards";

/// The directory of the board `id` under `repo_root`.
pub fn board_dir(repo_root: &Path, id: &str) -> PathBuf {
    repo_root.join(BOARDS_DIR).join(id)
}

/// Every board's directory under `repo_root`, sorted by name: each directory in `bsp/boards/` that
/// holds a `board.toml`.
pub fn board_dirs(repo_root: &Path) -> Result<Vec<PathBuf>, String> {
    let root = repo_root.join(BOARDS_DIR);
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)
        .map_err(|error| format!("{}: {error}", root.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|dir| dir.join("board.toml").is_file())
        .collect();
    dirs.sort();
    Ok(dirs)
}

/// The name of the board `id`'s C# assembly, `Lamella.Boards.<Vendor>.<Board>`, by the rule
/// `csharp/build-boards.ps1` names it by: the vendor segment the board's generated bindings declare as
/// `BOARD_VENDOR`, and the board's class name, its bindings name less the maker its id begins with.
///
/// A host test that compiles a board's package with a program of its own takes this name, because a
/// board's package is what `Lamella.Hardware` grants its internal seams to.
pub fn board_assembly(repo_root: &Path, id: &str) -> Result<String, String> {
    let csharp = board_dir(repo_root, id).join("csharp");
    let bindings: Vec<PathBuf> = std::fs::read_dir(&csharp)
        .map_err(|error| format!("{}: {error}", csharp.display()))?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.file_name().is_some_and(|name| name.to_string_lossy().ends_with("Bindings.g.cs")))
        .collect();
    let [bindings] = bindings.as_slice() else {
        return Err(format!("{}: expected exactly one *Bindings.g.cs, found {}", csharp.display(), bindings.len()));
    };
    let text = std::fs::read_to_string(bindings).map_err(|error| format!("{}: {error}", bindings.display()))?;
    let vendor = text
        .split("BOARD_VENDOR = \"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .ok_or_else(|| format!("{}: no BOARD_VENDOR", bindings.display()))?;
    Ok(format!("Lamella.Boards.{vendor}.{}", crate::strata::board_class(id)))
}
