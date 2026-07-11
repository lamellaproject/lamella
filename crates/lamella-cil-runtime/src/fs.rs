//! The file-system seam: files and directories behind a trait the embedder supplies (host =
//! `std::fs`; a device = a FAT driver over an SD block device; tests and a browser = an
//! in-memory tree). Unlike the socket seam ([`crate::net`]) every operation is BLOCKING:
//! file operations complete in bounded time on every target (host syscalls, FatFs sector
//! reads), the NETMF file API this surface mirrors was synchronous, and a native call cannot
//! be preempted mid-flight anyway -- so there is no `WouldBlock`, no parking, and no reactor
//! involvement. A long read stalls sibling green threads for its (bounded) duration.

use alloc::string::String;
use alloc::vec::Vec;

/// An open file the backend hands out: an index into the backend's own table, opaque to the
/// interpreter (it just passes the handle back to identify the file).
pub type FileHandle = u32;

/// Why a file-system operation failed. Each variant maps 1:1 to a managed exception type;
/// [`FsError::code`] is the negative sentinel the intrinsics return across the seam.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FsError {
    /// The file does not exist (managed: `FileNotFoundException`).
    NotFound,
    /// A directory on the path (or the directory itself) does not exist
    /// (managed: `DirectoryNotFoundException`).
    DirectoryNotFound,
    /// The caller may not perform the operation -- a read-only file opened for write, a
    /// permission denial, a directory opened as a file (managed: `UnauthorizedAccessException`).
    AccessDenied,
    /// The target already exists and the mode forbids that (`CreateNew` on an existing file,
    /// a move onto an existing name) (managed: `IOException`).
    AlreadyExists,
    /// The path names a directory where a file was required (managed: `IOException`).
    IsDirectory,
    /// A directory delete found the directory not empty (managed: `IOException`).
    NotEmpty,
    /// The path is syntactically unusable -- empty, or malformed for the backend
    /// (managed: `ArgumentException`).
    InvalidPath,
    /// Any other I/O failure (media gone, hardware error) (managed: `IOException`).
    Io,
}

impl FsError {
    /// The negative sentinel this error crosses the seam as (handles and byte counts are
    /// `>= 0`; `-1` is reserved so the socket seam's WouldBlock habit never aliases a real
    /// file error).
    #[must_use]
    pub fn code(self) -> i32 {
        match self {
            FsError::NotFound => -2,
            FsError::DirectoryNotFound => -3,
            FsError::AccessDenied => -4,
            FsError::AlreadyExists => -5,
            FsError::IsDirectory => -6,
            FsError::NotEmpty => -7,
            FsError::InvalidPath => -8,
            FsError::Io => -9,
        }
    }
}

/// The outcome of a file-system operation.
pub type FsResult<T> = Result<T, FsError>;

/// How [`FsBackend::open`] treats an existing (or missing) file -- the managed
/// `System.IO.FileMode` values, crossed as their .NET integer codes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileMode {
    /// Create a new file; fail with [`FsError::AlreadyExists`] if it exists (.NET 1).
    CreateNew,
    /// Create or truncate (.NET 2).
    Create,
    /// Open an existing file; fail with [`FsError::NotFound`] if missing (.NET 3).
    Open,
    /// Open, creating an empty file if missing (.NET 4).
    OpenOrCreate,
    /// Open an existing file and truncate it (.NET 5).
    Truncate,
    /// Open or create, positioned at the end (.NET 6). The managed layer enforces
    /// append-only semantics; the backend just starts at the end.
    Append,
}

impl FileMode {
    /// Decodes the managed `FileMode` integer; anything unknown is `None` (the intrinsic
    /// reports [`FsError::InvalidPath`]-class misuse loudly rather than guessing).
    #[must_use]
    pub fn from_i32(value: i32) -> Option<FileMode> {
        Some(match value {
            1 => FileMode::CreateNew,
            2 => FileMode::Create,
            3 => FileMode::Open,
            4 => FileMode::OpenOrCreate,
            5 => FileMode::Truncate,
            6 => FileMode::Append,
            _ => return None,
        })
    }
}

/// Which directions an open file permits -- the managed `System.IO.FileAccess` values.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FileAccess {
    /// Read only (.NET 1).
    Read,
    /// Write only (.NET 2).
    Write,
    /// Both (.NET 3).
    ReadWrite,
}

impl FileAccess {
    /// Decodes the managed `FileAccess` integer.
    #[must_use]
    pub fn from_i32(value: i32) -> Option<FileAccess> {
        Some(match value {
            1 => FileAccess::Read,
            2 => FileAccess::Write,
            3 => FileAccess::ReadWrite,
            _ => return None,
        })
    }
}

/// One directory entry from [`FsBackend::list_dir`]: its name (no path) and whether it is
/// itself a directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// The entry's own name (`file.txt`, `sub`), not a full path.
    pub name: String,
    /// `true` for a subdirectory, `false` for a file.
    pub is_dir: bool,
}

/// The file-system seam. `Debug` is a supertrait so the [`crate::interp::Vm`] -- which holds
/// an `Option<Box<dyn FsBackend>>` -- still derives `Debug`. A device with no file system
/// simply installs no backend: every managed operation then reports [`FsError::Io`] and the
/// corlib throws `IOException` -- catchable and message-bearing, not a trap.
pub trait FsBackend: core::fmt::Debug {
    /// Opens (or creates, per `mode`) the file at `path` for `access`, returning its handle.
    /// The initial position is 0, except [`FileMode::Append`] which starts at the end.
    fn open(&mut self, path: &str, mode: FileMode, access: FileAccess) -> FsResult<FileHandle>;

    /// Reads at the current position into `buf`, advancing it; `Ok(0)` at end of file.
    fn read(&mut self, file: FileHandle, buf: &mut [u8]) -> FsResult<usize>;

    /// Writes `buf` at the current position, advancing it; returns the bytes written
    /// (a backend may write fewer than offered; the managed layer loops).
    fn write(&mut self, file: FileHandle, buf: &[u8]) -> FsResult<usize>;

    /// Moves the position: `origin` 0 = from start, 1 = from current, 2 = from end
    /// (`System.IO.SeekOrigin`). Returns the new absolute position. Seeking past the end is
    /// legal (a later write extends the file, zero-filling the gap).
    fn seek(&mut self, file: FileHandle, offset: i64, origin: i32) -> FsResult<i64>;

    /// The file's current length in bytes.
    fn length(&mut self, file: FileHandle) -> FsResult<i64>;

    /// Truncates or zero-extends the file to `length` bytes (the position is unchanged,
    /// clamped by the managed layer where .NET requires it).
    fn set_length(&mut self, file: FileHandle, length: i64) -> FsResult<()>;

    /// Forces buffered writes to the medium.
    fn flush(&mut self, file: FileHandle) -> FsResult<()>;

    /// Closes the file and releases its handle (idempotent; closing an unknown handle is a
    /// no-op -- Close runs from finalizers too).
    fn close(&mut self, file: FileHandle);

    /// Whether a FILE exists at `path` (`false` for a directory -- matching `File.Exists`).
    fn file_exists(&mut self, path: &str) -> bool;

    /// Whether a DIRECTORY exists at `path` (matching `Directory.Exists`).
    fn dir_exists(&mut self, path: &str) -> bool;

    /// Deletes the file at `path`. Deleting a missing file is a SUCCESS (matching
    /// `File.Delete`); a missing parent directory is [`FsError::DirectoryNotFound`].
    fn delete_file(&mut self, path: &str) -> FsResult<()>;

    /// Creates the directory at `path`, including any missing parents (matching
    /// `Directory.CreateDirectory`); an existing directory is a success.
    fn create_dir(&mut self, path: &str) -> FsResult<()>;

    /// Deletes the directory at `path`: empty-only unless `recursive` (matching
    /// `Directory.Delete`).
    fn delete_dir(&mut self, path: &str, recursive: bool) -> FsResult<()>;

    /// Renames/moves a file or directory. The destination must not exist
    /// ([`FsError::AlreadyExists`]).
    fn move_entry(&mut self, from: &str, to: &str) -> FsResult<()>;

    /// The entries of the directory at `path`, in the backend's order (the managed layer
    /// does not sort; .NET's order is unspecified too).
    fn list_dir(&mut self, path: &str) -> FsResult<Vec<DirEntry>>;
}

/// A boxed file-system backend, as the [`crate::interp::Vm`] stores it.
pub type BoxedFsBackend = alloc::boxed::Box<dyn FsBackend>;
