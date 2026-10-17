//! Where the boards live: one directory per board under `bsp/boards/`, named by the board's id.
//!
//! Every Rust reader that lists the boards lists them through [`board_dirs`], so which directories
//! are boards is stated once. `build-boards.ps1 -PrintBoards` states the same list for PowerShell.

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
