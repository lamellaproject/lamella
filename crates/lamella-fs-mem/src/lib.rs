//! An in-memory [`FsBackend`]: a flat map of normalized paths to nodes, plus an open-file
//! table. It pins the seam's SEMANTICS (open modes, parent-must-exist, delete/move rules,
//! seek-past-end zero-fill) in unit tests that touch no host disk, and it is the
//! file-system story for embedders with no medium at all -- a browser tab, a simulator, a
//! test harness. `no_std` + `alloc`, like the interpreter core it plugs into.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::borrow::ToOwned;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;

use lamella_cil_runtime::fs::{
    DirEntry, FileAccess, FileHandle, FileMode, FsBackend, FsError, FsResult,
};

/// One tree node: a file's bytes, or a directory marker.
#[derive(Clone, Debug)]
enum Node {
    File(Vec<u8>),
    Dir,
}

/// One open file: the node it addresses and the stream state.
#[derive(Clone, Debug)]
struct OpenFile {
    /// The normalized path key (operations re-look the node up, so a delete of an open
    /// file surfaces as [`FsError::Io`] on the next use rather than being masked).
    key: String,
    position: u64,
    access: FileAccess,
}

/// The in-memory file system. `Default` is an empty tree (just the root).
#[derive(Debug, Default)]
pub struct MemFs {
    nodes: BTreeMap<String, Node>,
    open: Vec<Option<OpenFile>>,
}

impl MemFs {
    /// An empty in-memory file system.
    #[must_use]
    pub fn new() -> MemFs {
        MemFs::default()
    }

    /// Normalizes a path to the canonical key: forward-slash separators, no empty/`.`
    /// segments, `..` popping, no trailing separator. The ROOT normalizes to `""`.
    /// A path that escapes the root (`..` past it) or is empty-and-relative is invalid.
    fn normalize(path: &str) -> FsResult<String> {
        if path.is_empty() {
            return Err(FsError::InvalidPath);
        }
        let mut segments: Vec<&str> = Vec::new();
        for segment in path.split(['/', '\\']) {
            match segment {
                "" | "." => {}
                ".." => {
                    if segments.pop().is_none() {
                        return Err(FsError::InvalidPath);
                    }
                }
                other => segments.push(other),
            }
        }
        Ok(segments.join("/"))
    }

    /// The parent key of a normalized key (`None` for the root itself).
    fn parent(key: &str) -> Option<&str> {
        if key.is_empty() {
            return None;
        }
        Some(match key.rfind('/') {
            Some(index) => &key[..index],
            None => "",
        })
    }

    /// Whether a normalized key names an existing directory (the root always does).
    fn is_dir(&self, key: &str) -> bool {
        key.is_empty() || matches!(self.nodes.get(key), Some(Node::Dir))
    }

    /// The open-file slot for a handle.
    fn slot(&mut self, handle: FileHandle) -> FsResult<&mut OpenFile> {
        self.open
            .get_mut(handle as usize)
            .and_then(Option::as_mut)
            .ok_or(FsError::Io)
    }

    /// The byte content of an open file's node (the file may have been deleted or
    /// replaced by a directory since it was opened -- both surface as [`FsError::Io`]).
    fn content_of<'nodes>(
        nodes: &'nodes mut BTreeMap<String, Node>,
        key: &str,
    ) -> FsResult<&'nodes mut Vec<u8>> {
        match nodes.get_mut(key) {
            Some(Node::File(bytes)) => Ok(bytes),
            _ => Err(FsError::Io),
        }
    }
}

impl FsBackend for MemFs {
    fn open(&mut self, path: &str, mode: FileMode, access: FileAccess) -> FsResult<FileHandle> {
        let key = MemFs::normalize(path)?;
        if key.is_empty() || self.is_dir(&key) {
            return Err(FsError::IsDirectory);
        }
        if let Some(parent) = MemFs::parent(&key) {
            if !self.is_dir(parent) {
                return Err(FsError::DirectoryNotFound);
            }
        }
        let exists = matches!(self.nodes.get(&key), Some(Node::File(_)));
        match mode {
            FileMode::CreateNew if exists => return Err(FsError::AlreadyExists),
            FileMode::Open | FileMode::Truncate if !exists => return Err(FsError::NotFound),
            _ => {}
        }
        let truncate = matches!(mode, FileMode::Create | FileMode::Truncate);
        if !exists || truncate {
            self.nodes.insert(key.clone(), Node::File(Vec::new()));
        }
        let position = if matches!(mode, FileMode::Append) {
            match self.nodes.get(&key) {
                Some(Node::File(bytes)) => bytes.len() as u64,
                _ => 0,
            }
        } else {
            0
        };
        let file = OpenFile { key, position, access };
        let handle = match self.open.iter().position(Option::is_none) {
            Some(index) => {
                self.open[index] = Some(file);
                index
            }
            None => {
                self.open.push(Some(file));
                self.open.len() - 1
            }
        };
        Ok(handle as FileHandle)
    }

    fn read(&mut self, file: FileHandle, buf: &mut [u8]) -> FsResult<usize> {
        let (key, position, access) = {
            let open = self.slot(file)?;
            (open.key.clone(), open.position, open.access)
        };
        if matches!(access, FileAccess::Write) {
            return Err(FsError::AccessDenied);
        }
        let content = MemFs::content_of(&mut self.nodes, &key)?;
        let start = (position as usize).min(content.len());
        let count = buf.len().min(content.len() - start);
        buf[..count].copy_from_slice(&content[start..start + count]);
        self.slot(file)?.position = (start + count) as u64;
        Ok(count)
    }

    fn write(&mut self, file: FileHandle, buf: &[u8]) -> FsResult<usize> {
        let (key, position, access) = {
            let open = self.slot(file)?;
            (open.key.clone(), open.position, open.access)
        };
        if matches!(access, FileAccess::Read) {
            return Err(FsError::AccessDenied);
        }
        let content = MemFs::content_of(&mut self.nodes, &key)?;
        let start = position as usize;
        if start > content.len() {
            content.resize(start, 0);
        }
        let overlap = buf.len().min(content.len().saturating_sub(start));
        content[start..start + overlap].copy_from_slice(&buf[..overlap]);
        content.extend_from_slice(&buf[overlap..]);
        self.slot(file)?.position = (start + buf.len()) as u64;
        Ok(buf.len())
    }

    fn seek(&mut self, file: FileHandle, offset: i64, origin: i32) -> FsResult<i64> {
        let (key, position) = {
            let open = self.slot(file)?;
            (open.key.clone(), open.position)
        };
        let length = MemFs::content_of(&mut self.nodes, &key)?.len() as i64;
        let base = match origin {
            0 => 0,
            1 => position as i64,
            2 => length,
            _ => return Err(FsError::Io),
        };
        let target = base.checked_add(offset).ok_or(FsError::Io)?;
        if target < 0 {
            return Err(FsError::Io);
        }
        self.slot(file)?.position = target as u64;
        Ok(target)
    }

    fn length(&mut self, file: FileHandle) -> FsResult<i64> {
        let key = self.slot(file)?.key.clone();
        Ok(MemFs::content_of(&mut self.nodes, &key)?.len() as i64)
    }

    fn set_length(&mut self, file: FileHandle, length: i64) -> FsResult<()> {
        if length < 0 {
            return Err(FsError::Io);
        }
        let (key, access) = {
            let open = self.slot(file)?;
            (open.key.clone(), open.access)
        };
        if matches!(access, FileAccess::Read) {
            return Err(FsError::AccessDenied);
        }
        MemFs::content_of(&mut self.nodes, &key)?.resize(length as usize, 0);
        Ok(())
    }

    fn flush(&mut self, _file: FileHandle) -> FsResult<()> {
        Ok(())
    }

    fn close(&mut self, file: FileHandle) {
        if let Some(slot) = self.open.get_mut(file as usize) {
            *slot = None;
        }
    }

    fn file_exists(&mut self, path: &str) -> bool {
        match MemFs::normalize(path) {
            Ok(key) => matches!(self.nodes.get(&key), Some(Node::File(_))),
            Err(_) => false,
        }
    }

    fn dir_exists(&mut self, path: &str) -> bool {
        match MemFs::normalize(path) {
            Ok(key) => self.is_dir(&key),
            Err(_) => false,
        }
    }

    fn delete_file(&mut self, path: &str) -> FsResult<()> {
        let key = MemFs::normalize(path)?;
        if let Some(parent) = MemFs::parent(&key) {
            if !self.is_dir(parent) {
                return Err(FsError::DirectoryNotFound);
            }
        }
        match self.nodes.get(&key) {
            Some(Node::File(_)) => {
                self.nodes.remove(&key);
                Ok(())
            }
            Some(Node::Dir) => Err(FsError::AccessDenied),
            None => Ok(()),
        }
    }

    fn create_dir(&mut self, path: &str) -> FsResult<()> {
        let key = MemFs::normalize(path)?;
        if key.is_empty() {
            return Ok(());
        }
        if matches!(self.nodes.get(&key), Some(Node::File(_))) {
            return Err(FsError::AlreadyExists);
        }
        let mut prefix = String::new();
        for segment in key.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            match self.nodes.get(&prefix) {
                Some(Node::File(_)) => return Err(FsError::AlreadyExists),
                Some(Node::Dir) => {}
                None => {
                    self.nodes.insert(prefix.clone(), Node::Dir);
                }
            }
        }
        Ok(())
    }

    fn delete_dir(&mut self, path: &str, recursive: bool) -> FsResult<()> {
        let key = MemFs::normalize(path)?;
        if key.is_empty() {
            return Err(FsError::AccessDenied);
        }
        if !matches!(self.nodes.get(&key), Some(Node::Dir)) {
            return Err(FsError::DirectoryNotFound);
        }
        let prefix = alloc::format!("{key}/");
        let has_children = self.nodes.keys().any(|other| other.starts_with(&prefix));
        if has_children && !recursive {
            return Err(FsError::NotEmpty);
        }
        self.nodes.retain(|other, _| other != &key && !other.starts_with(&prefix));
        Ok(())
    }

    fn move_entry(&mut self, from: &str, to: &str) -> FsResult<()> {
        let from_key = MemFs::normalize(from)?;
        let to_key = MemFs::normalize(to)?;
        if from_key.is_empty() || to_key.is_empty() {
            return Err(FsError::InvalidPath);
        }
        if self.nodes.contains_key(&to_key) {
            return Err(FsError::AlreadyExists);
        }
        if let Some(parent) = MemFs::parent(&to_key) {
            if !self.is_dir(parent) {
                return Err(FsError::DirectoryNotFound);
            }
        }
        match self.nodes.remove(&from_key) {
            Some(Node::File(bytes)) => {
                self.nodes.insert(to_key, Node::File(bytes));
                Ok(())
            }
            Some(Node::Dir) => {
                self.nodes.insert(to_key.clone(), Node::Dir);
                let prefix = alloc::format!("{from_key}/");
                let moved: Vec<(String, Node)> = self
                    .nodes
                    .iter()
                    .filter(|(other, _)| other.starts_with(&prefix))
                    .map(|(other, node)| {
                        (
                            alloc::format!("{to_key}/{}", &other[prefix.len()..]),
                            node.clone(),
                        )
                    })
                    .collect();
                self.nodes.retain(|other, _| !other.starts_with(&prefix));
                self.nodes.extend(moved);
                Ok(())
            }
            None => Err(FsError::NotFound),
        }
    }

    fn list_dir(&mut self, path: &str) -> FsResult<Vec<DirEntry>> {
        let key = MemFs::normalize(path)?;
        if !self.is_dir(&key) {
            return Err(FsError::DirectoryNotFound);
        }
        let prefix = if key.is_empty() {
            String::new()
        } else {
            alloc::format!("{key}/")
        };
        let mut entries = Vec::new();
        for (other, node) in &self.nodes {
            let Some(rest) = other.strip_prefix(&prefix) else {
                continue;
            };
            if rest.is_empty() || rest.contains('/') || (prefix.is_empty() && other.is_empty()) {
                continue;
            }
            entries.push(DirEntry {
                name: rest.to_owned(),
                is_dir: matches!(node, Node::Dir),
            });
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_rw(fs: &mut MemFs, path: &str, mode: FileMode) -> FileHandle {
        fs.open(path, mode, FileAccess::ReadWrite).expect("open")
    }

    #[test]
    fn write_read_roundtrip_with_seek() {
        let mut fs = MemFs::new();
        let f = open_rw(&mut fs, "a.txt", FileMode::CreateNew);
        assert_eq!(fs.write(f, b"hello world"), Ok(11));
        assert_eq!(fs.seek(f, 6, 0), Ok(6));
        let mut buf = [0u8; 16];
        assert_eq!(fs.read(f, &mut buf), Ok(5));
        assert_eq!(&buf[..5], b"world");
        assert_eq!(fs.read(f, &mut buf), Ok(0));
        assert_eq!(fs.length(f), Ok(11));
        fs.close(f);
        assert!(fs.file_exists("a.txt"));
        assert!(!fs.dir_exists("a.txt"));
    }

    #[test]
    fn open_modes() {
        let mut fs = MemFs::new();
        assert_eq!(
            fs.open("missing.txt", FileMode::Open, FileAccess::Read).unwrap_err(),
            FsError::NotFound
        );
        let f = open_rw(&mut fs, "x.bin", FileMode::CreateNew);
        assert_eq!(fs.write(f, b"1234"), Ok(4));
        fs.close(f);
        assert_eq!(
            fs.open("x.bin", FileMode::CreateNew, FileAccess::Write).unwrap_err(),
            FsError::AlreadyExists
        );
        let t = open_rw(&mut fs, "x.bin", FileMode::Truncate);
        assert_eq!(fs.length(t), Ok(0));
        assert_eq!(fs.write(t, b"ab"), Ok(2));
        fs.close(t);
        let a = fs.open("x.bin", FileMode::Append, FileAccess::Write).expect("append");
        assert_eq!(fs.write(a, b"cd"), Ok(2));
        fs.close(a);
        let r = fs.open("x.bin", FileMode::Open, FileAccess::Read).expect("read");
        let mut buf = [0u8; 8];
        assert_eq!(fs.read(r, &mut buf), Ok(4));
        assert_eq!(&buf[..4], b"abcd");
        assert_eq!(fs.write(r, b"zz").unwrap_err(), FsError::AccessDenied);
        fs.close(r);
    }

    #[test]
    fn missing_parent_is_directory_not_found() {
        let mut fs = MemFs::new();
        assert_eq!(
            fs.open("no/dir/file.txt", FileMode::Create, FileAccess::Write).unwrap_err(),
            FsError::DirectoryNotFound
        );
        assert_eq!(fs.delete_file("no/dir/file.txt").unwrap_err(), FsError::DirectoryNotFound);
    }

    #[test]
    fn directories_create_list_delete() {
        let mut fs = MemFs::new();
        assert_eq!(fs.create_dir("a/b/c"), Ok(()));
        assert!(fs.dir_exists("a") && fs.dir_exists("a/b") && fs.dir_exists("a/b/c"));
        let f = open_rw(&mut fs, "a/b/f.txt", FileMode::Create);
        fs.close(f);
        let entries = fs.list_dir("a/b").expect("list");
        let mut names: Vec<(String, bool)> =
            entries.into_iter().map(|entry| (entry.name, entry.is_dir)).collect();
        names.sort();
        assert_eq!(
            names,
            alloc::vec![("c".to_owned(), true), ("f.txt".to_owned(), false)]
        );
        assert_eq!(fs.delete_dir("a", false).unwrap_err(), FsError::NotEmpty);
        assert_eq!(fs.delete_dir("a", true), Ok(()));
        assert!(!fs.dir_exists("a"));
        assert!(!fs.file_exists("a/b/f.txt"));
    }

    #[test]
    fn move_file_and_directory_subtree() {
        let mut fs = MemFs::new();
        fs.create_dir("d1").expect("mkdir");
        let f = open_rw(&mut fs, "d1/f.txt", FileMode::Create);
        assert_eq!(fs.write(f, b"data"), Ok(4));
        fs.close(f);
        assert_eq!(fs.move_entry("d1/f.txt", "d1/g.txt"), Ok(()));
        assert!(!fs.file_exists("d1/f.txt") && fs.file_exists("d1/g.txt"));
        assert_eq!(fs.move_entry("d1", "d2"), Ok(()));
        assert!(fs.file_exists("d2/g.txt") && !fs.dir_exists("d1"));
        fs.create_dir("d3").expect("mkdir");
        assert_eq!(fs.move_entry("d2", "d3").unwrap_err(), FsError::AlreadyExists);
    }

    #[test]
    fn seek_past_end_zero_fills_on_write() {
        let mut fs = MemFs::new();
        let f = open_rw(&mut fs, "gap.bin", FileMode::Create);
        assert_eq!(fs.seek(f, 4, 0), Ok(4));
        assert_eq!(fs.write(f, b"x"), Ok(1));
        assert_eq!(fs.length(f), Ok(5));
        assert_eq!(fs.seek(f, 0, 0), Ok(0));
        let mut buf = [0xffu8; 5];
        assert_eq!(fs.read(f, &mut buf), Ok(5));
        assert_eq!(&buf, b"\0\0\0\0x");
        fs.close(f);
    }

    #[test]
    fn path_normalization() {
        let mut fs = MemFs::new();
        fs.create_dir("a\\b").expect("mkdir with backslashes");
        assert!(fs.dir_exists("a/b"));
        assert!(fs.dir_exists("a//b/."));
        assert!(fs.dir_exists("a/b/../b"));
        assert_eq!(MemFs::normalize("..").unwrap_err(), FsError::InvalidPath);
    }

    #[test]
    fn set_length_truncates_and_extends() {
        let mut fs = MemFs::new();
        let f = open_rw(&mut fs, "t.bin", FileMode::Create);
        assert_eq!(fs.write(f, b"abcdef"), Ok(6));
        assert_eq!(fs.set_length(f, 3), Ok(()));
        assert_eq!(fs.length(f), Ok(3));
        assert_eq!(fs.set_length(f, 5), Ok(()));
        assert_eq!(fs.seek(f, 0, 0), Ok(0));
        let mut buf = [0u8; 8];
        assert_eq!(fs.read(f, &mut buf), Ok(5));
        assert_eq!(&buf[..5], b"abc\0\0");
        fs.close(f);
    }
}
