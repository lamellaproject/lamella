//! Files: the host seam and the object `open()` returns.

use crate::trap::Trap;
use alloc::string::String;
use alloc::vec::Vec;

/// Which way an object was opened -- the part of the mode string that changes behaviour.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FileMode {
    /// Reads are permitted.
    pub read: bool,
    /// Writes are permitted.
    pub write: bool,
    /// Every write goes to the end, whatever `seek` said.
    pub append: bool,
    /// Bytes in and out, rather than str.
    pub binary: bool,
    /// The file must not already exist (`x`).
    pub exclusive: bool,
    /// An existing file is emptied on open (`w`).
    pub truncate: bool,
}

impl FileMode {
    /// Parses a CPython mode string, or reports which character was wrong.
    ///
    /// The error messages are CPython's own, and there are three distinct ones because the mistakes
    /// are distinct: a character that means nothing, a mode that says nothing about direction, and two
    /// directions at once.
    pub fn parse(mode: &str) -> Result<FileMode, String> {
        let mut read = false;
        let mut write = false;
        let mut append = false;
        let mut binary = false;
        let mut text = false;
        let mut exclusive = false;
        let mut truncate = false;
        let mut plus = false;
        for c in mode.chars() {
            match c {
                'r' => read = true,
                'w' => {
                    write = true;
                    truncate = true;
                }
                'a' => {
                    write = true;
                    append = true;
                }
                'x' => {
                    write = true;
                    exclusive = true;
                }
                'b' => binary = true,
                't' => text = true,
                '+' => plus = true,
                'U' => return Err(alloc::format!("invalid mode: '{mode}'")),
                other => return Err(alloc::format!("invalid mode: '{other}'")),
            }
        }
        let directions = usize::from(read) + usize::from(truncate) + usize::from(append) + usize::from(exclusive);
        if directions == 0 {
            return Err(String::from(
                "Must have exactly one of create/read/write/append mode and at most one plus",
            ));
        }
        if directions > 1 || (binary && text) {
            return Err(String::from(
                "must have exactly one of create/read/write/append mode",
            ));
        }
        if plus {
            read = true;
            write = true;
        }
        Ok(FileMode { read, write, append, binary, exclusive, truncate })
    }

    /// The mode string CPython's `file.mode` reports for this mode.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match (self.truncate, self.append, self.exclusive, self.read, self.write, self.binary) {
            (true, _, _, true, _, false) => "w+",
            (true, _, _, true, _, true) => "rb+",
            (true, _, _, _, _, false) => "w",
            (true, _, _, _, _, true) => "wb",
            (_, true, _, true, _, false) => "a+",
            (_, true, _, true, _, true) => "ab+",
            (_, true, _, _, _, false) => "a",
            (_, true, _, _, _, true) => "ab",
            (_, _, true, true, _, false) => "x+",
            (_, _, true, _, _, false) => "x",
            (_, _, true, _, _, true) => "xb",
            (_, _, _, true, true, false) => "r+",
            (_, _, _, true, true, true) => "rb+",
            (_, _, _, _, _, true) => "rb",
            _ => "r",
        }
    }
}

/// Why a file operation failed, in the terms the C library reports it -- so the exception carries
/// CPython's exact `[Errno n] text: 'path'` rather than a description invented here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileError {
    /// ENOENT (2).
    NotFound,
    /// EACCES (13).
    Permission,
    /// EEXIST (17).
    Exists,
    /// EISDIR (21).
    IsADirectory,
    /// ENOTDIR (20).
    NotADirectory,
    /// ENOTEMPTY (41 on Linux, 39 elsewhere) -- rmdir of a directory with contents.
    NotEmpty,
    /// EINVAL (22) -- a seek before the start, a bad argument the seam rejected.
    Invalid,
    /// EIO (5) -- everything else the host reported.
    Other,
}

impl FileError {
    /// The exception class name CPython raises for this failure.
    #[must_use]
    pub fn exception(self) -> &'static str {
        match self {
            FileError::NotFound => "FileNotFoundError",
            FileError::Permission => "PermissionError",
            FileError::Exists => "FileExistsError",
            FileError::IsADirectory => "IsADirectoryError",
            FileError::NotADirectory => "NotADirectoryError",
            FileError::NotEmpty => "OSError",
            FileError::Invalid => "OSError",
            FileError::Other => "OSError",
        }
    }

    /// The errno number and the C library's text for it, which together make CPython's message.
    #[must_use]
    pub fn errno(self) -> (i32, &'static str) {
        match self {
            FileError::NotFound => (2, "No such file or directory"),
            FileError::Permission => (13, "Permission denied"),
            FileError::Exists => (17, "File exists"),
            FileError::IsADirectory => (21, "Is a directory"),
            FileError::NotADirectory => (20, "Not a directory"),
            FileError::NotEmpty => (41, "Directory not empty"),
            FileError::Invalid => (22, "Invalid argument"),
            FileError::Other => (5, "Input/output error"),
        }
    }
}

/// What a file exists in relation to -- one entry of a directory listing, or a path's kind.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathKind {
    /// A regular file, with its size in bytes.
    File(u64),
    /// A directory.
    Directory,
}

/// The host operations a filesystem needs, installed by the embedder.
///
/// Function pointers rather than a trait object so the seam costs nothing to carry on a target with
/// no allocator behind it and no `dyn` dispatch. `handle` is whatever the embedder wants it to be --
/// this side only stores and returns it.
#[derive(Clone, Copy, Debug)]
pub struct FileOps {
    /// Open `path`, returning a handle. `truncate`/`append`/`exclusive` mirror the mode.
    pub open: fn(path: &str, mode: FileMode) -> Result<u32, FileError>,
    /// Read into `buf`, returning how many bytes were read (0 at end of file).
    pub read: fn(handle: u32, buf: &mut [u8]) -> Result<usize, FileError>,
    /// Write `data`, returning how many bytes were written.
    pub write: fn(handle: u32, data: &[u8]) -> Result<usize, FileError>,
    /// Move to `offset` relative to `whence` (0 start, 1 current, 2 end); the new position.
    pub seek: fn(handle: u32, offset: i64, whence: u8) -> Result<u64, FileError>,
    /// The current position.
    pub tell: fn(handle: u32) -> Result<u64, FileError>,
    /// Push buffered writes to the host.
    pub flush: fn(handle: u32) -> Result<(), FileError>,
    /// Release the handle.
    pub close: fn(handle: u32) -> Result<(), FileError>,
    /// What `path` is, or `NotFound`.
    pub kind: fn(path: &str) -> Result<PathKind, FileError>,
    /// The names directly inside directory `path`, in host order.
    pub listdir: fn(path: &str) -> Result<Vec<String>, FileError>,
    /// Delete the file `path`.
    pub remove: fn(path: &str) -> Result<(), FileError>,
    /// Create the directory `path` (its parent must exist).
    pub mkdir: fn(path: &str) -> Result<(), FileError>,
    /// Remove the EMPTY directory `path`.
    pub rmdir: fn(path: &str) -> Result<(), FileError>,
    /// Rename `from` to `to`.
    pub rename: fn(from: &str, to: &str) -> Result<(), FileError>,
}

/// Method ids for a file object, dispatched by `ObjectModel::call_file_method`.
pub(crate) const FILE_READ: u32 = 0;
pub(crate) const FILE_READLINE: u32 = 1;
pub(crate) const FILE_READLINES: u32 = 2;
pub(crate) const FILE_WRITE: u32 = 3;
pub(crate) const FILE_WRITELINES: u32 = 4;
pub(crate) const FILE_CLOSE: u32 = 5;
pub(crate) const FILE_FLUSH: u32 = 6;
pub(crate) const FILE_SEEK: u32 = 7;
pub(crate) const FILE_TELL: u32 = 8;
pub(crate) const FILE_ENTER: u32 = 9;
pub(crate) const FILE_EXIT: u32 = 10;
pub(crate) const FILE_ITER: u32 = 11;
pub(crate) const FILE_NEXT: u32 = 12;
pub(crate) const FILE_READABLE: u32 = 13;
pub(crate) const FILE_WRITABLE: u32 = 14;
pub(crate) const FILE_SEEKABLE: u32 = 15;
pub(crate) const FILE_TRUNCATE: u32 = 16;

/// The file method id for `name`, or `None`.
pub(crate) fn file_method_id(name: &str) -> Option<u32> {
    match name {
        "read" => Some(FILE_READ),
        "readline" => Some(FILE_READLINE),
        "readlines" => Some(FILE_READLINES),
        "write" => Some(FILE_WRITE),
        "writelines" => Some(FILE_WRITELINES),
        "close" => Some(FILE_CLOSE),
        "flush" => Some(FILE_FLUSH),
        "seek" => Some(FILE_SEEK),
        "tell" => Some(FILE_TELL),
        "__enter__" => Some(FILE_ENTER),
        "__exit__" => Some(FILE_EXIT),
        "__iter__" => Some(FILE_ITER),
        "__next__" => Some(FILE_NEXT),
        "readable" => Some(FILE_READABLE),
        "writable" => Some(FILE_WRITABLE),
        "seekable" => Some(FILE_SEEKABLE),
        "truncate" => Some(FILE_TRUNCATE),
        _ => None,
    }
}

/// Translates the bytes a text-mode read produced: `\r\n` and a lone `\r` both become `\n`, which is
/// CPython's universal-newline behaviour on every platform. A file written on one host is therefore
/// read the same on another, which is the property that makes it worth doing at all.
pub(crate) fn translate_newlines(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == b'\r' {
            out.push(b'\n');
            i += usize::from(data.get(i + 1) == Some(&b'\n')) + 1;
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

/// The `Trap` for using a file that has been closed -- CPython's own wording, and the same for every
/// verb, because the mistake is the same one.
pub(crate) fn closed_file_error(model: &mut crate::object::ObjectModel) -> Trap {
    model.raise_named_exception("ValueError", "I/O operation on closed file.")
}

/// A file's three read-only attributes, so `getattr` can answer them without a method call.
pub(crate) fn file_attribute(name: &str) -> bool {
    matches!(name, "name" | "mode" | "closed")
}
