//! The runtime's virtual file system: a table of `prefix -> FsBackend` mounts.

use alloc::string::String;
use alloc::vec::Vec;

use crate::fs::{BoxedFsBackend, DirEntry, FileAccess, FileHandle, FileMode, FsError, FsResult};

/// The kind of medium behind a mount -- the managed `System.IO.DriveType` (nanoFramework's values,
/// which renumber .NET's). Metadata set when the mount is ESTABLISHED (a removable SD, a fixed eMMC,
/// a RAM disk), fed by the `[[storage]]` `kind` fact -- not derived from the backend.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum DriveType {
    /// The medium is unknown (nano `0`).
    Unknown = 0,
    /// The mount has no root directory (nano `1`).
    NoRootDirectory = 1,
    /// Removable media -- an SD card, a USB stick (nano `2`).
    Removable = 2,
    /// Fixed media -- an on-board eMMC / flash volume. The default for an unlabeled mount (nano `3`).
    #[default]
    Fixed = 3,
    /// A RAM-backed volume (nano `4`).
    Ram = 4,
}

impl DriveType {
    /// The managed `DriveType` integer.
    #[must_use]
    pub fn code(self) -> i32 {
        self as i32
    }

    /// Decodes a managed `DriveType` integer; anything out of range is [`DriveType::Fixed`].
    #[must_use]
    pub fn from_i32(value: i32) -> DriveType {
        match value {
            0 => DriveType::Unknown,
            1 => DriveType::NoRootDirectory,
            2 => DriveType::Removable,
            4 => DriveType::Ram,
            _ => DriveType::Fixed,
        }
    }
}

/// The embedder-supplied constructor of on-demand mount backends. The FAT / SD / RAM-disk crates live
/// ABOVE the interpreter (they depend on it), so the runtime cannot build their backends itself; an
/// embedder that links them installs a provider, and the managed `Storage.Mount*` calls route through
/// it. Every method defaults to [`FsError::Unsupported`], so a provider implements only the media it
/// supports -- and a build with no provider (or an unimplemented medium) reports NotSupported cleanly
/// rather than silently succeeding.
pub trait StorageProvider: core::fmt::Debug {
    /// Constructs a fresh, formatted RAM-backed volume of `size_bytes`, ready to mount.
    fn mount_ram(&mut self, size_bytes: u64) -> FsResult<BoxedFsBackend> {
        let _ = size_bytes;
        Err(FsError::Unsupported)
    }

    /// Constructs an SD-over-SPI FAT volume on the bus `bus_identity` names, with `chip_select` as
    /// the software chip-select pin the SD driver toggles.
    ///
    /// # `bus_identity` is an IDENTIFIER, never a pointer
    ///
    /// It is the value by which a NATIVE owner names one SPI peripheral instance, and this crate
    /// only carries it: the runtime never dereferences it, never adds an offset to it, and reads no
    /// meaning into it beyond `0 == none`. A provider matches it against the buses the embedder
    /// registered and answers [`FsError::Unsupported`] when it recognizes none.
    ///
    /// On a memory-mapped chip that value IS the peripheral's register base, and that is the point:
    /// the managed driver composes its registers from the chip package's base, the native driver is
    /// built from the same base in the same package, so the two halves agree on which instance is
    /// meant WITHOUT either of them inventing a bus-naming scheme for the other. A non-mapped
    /// backend is free to use any value both of its halves derive the same way.
    ///
    /// The mount OWNS the bus for its lifetime. Managed code that keeps driving the same peripheral
    /// after a successful mount corrupts the card; the managed facade documents that as its
    /// contract, and there is nothing this seam can do to enforce it.
    fn mount_sd_over_spi(&mut self, bus_identity: u32, chip_select: i32) -> FsResult<BoxedFsBackend> {
        let _ = (bus_identity, chip_select);
        Err(FsError::Unsupported)
    }

    /// Constructs an SD-over-SPI FAT volume on the bus `bus_number` names -- the SAME medium
    /// [`StorageProvider::mount_sd_over_spi`] mounts, named the way the nanoFramework-shaped surface
    /// names it.
    ///
    /// # Why this is a SECOND method and not a second meaning for the first
    ///
    /// A bus NUMBER (`spiBus = 1`) and a peripheral REGISTER BASE are two different namespaces for
    /// the same thing, and a `u32` cannot say which it is holding. One method taking either would
    /// make `1` mean the second SPI peripheral to one embedder and address `0x00000001` to the next,
    /// and the failure would be a mount succeeding on the WRONG bus rather than an error. Two names
    /// cost one defaulted method and remove the ambiguity entirely; an embedder implements whichever
    /// spellings its board actually offers and answers [`FsError::Unsupported`] for the rest, which
    /// the managed tier raises as `NotSupportedException`.
    fn mount_sd_over_spi_bus(&mut self, bus_number: u32, chip_select: i32) -> FsResult<BoxedFsBackend> {
        let _ = (bus_number, chip_select);
        Err(FsError::Unsupported)
    }
}

/// One mounted file system: the path prefix it owns (as segments, so matching is on boundaries), the
/// prefix VERBATIM (for `DriveInfo.Name`), the medium kind, and the backend behind it.
struct Mount {
    segments: Vec<String>,
    display: String,
    drive_type: DriveType,
    backend: BoxedFsBackend,
}

impl core::fmt::Debug for Mount {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Mount")
            .field("display", &self.display)
            .field("drive_type", &self.drive_type)
            .finish_non_exhaustive()
    }
}

/// A global open-file handle: which mount owns it, and the backend's own handle within that mount.
#[derive(Debug)]
struct OpenFile {
    mount: usize,
    inner: FileHandle,
}

/// The runtime's mount table (the VFS), held by the [`crate::Vm`]. The embedder mounts backends and
/// the `System.IO` intrinsics drive it. An empty table has no file system: every operation reports
/// [`FsError::Io`] and the corlib throws a catchable `IOException`, exactly as no backend did.
#[derive(Debug, Default)]
pub struct MountTable {
    /// `None` slots are unmounted tombstones -- kept so a live handle's stored mount index stays
    /// valid (removal would shift every later index).
    mounts: Vec<Option<Mount>>,
    open: Vec<Option<OpenFile>>,
}

impl MountTable {
    /// Splits a path (or a mount prefix) into normalized segments: both separators accepted, empty
    /// and `.` segments dropped, `..` popped. The root (`/`, `D:`, or empty) is the empty segment
    /// list. A path that escapes the root is [`FsError::InvalidPath`].
    fn segments(path: &str) -> FsResult<Vec<String>> {
        let mut segments: Vec<String> = Vec::new();
        for segment in path.split(['/', '\\']) {
            match segment {
                "" | "." => {}
                ".." => {
                    if segments.pop().is_none() {
                        return Err(FsError::InvalidPath);
                    }
                }
                other => segments.push(String::from(other)),
            }
        }
        Ok(segments)
    }

    /// Mounts `backend` at `prefix` (`"/"`, `"/sd"`, `"D:"`, ...) as [`DriveType::Fixed`]. Replacing
    /// an existing mount at the exact same prefix swaps the backend.
    pub fn mount(&mut self, prefix: &str, backend: BoxedFsBackend) -> FsResult<()> {
        self.mount_as(prefix, backend, DriveType::Fixed)
    }

    /// Mounts `backend` at `prefix` with an explicit medium kind (for `DriveInfo.DriveType`).
    /// Replacing an existing mount at the exact same prefix swaps both the backend and the kind.
    pub fn mount_as(
        &mut self,
        prefix: &str,
        backend: BoxedFsBackend,
        drive_type: DriveType,
    ) -> FsResult<()> {
        let segments = Self::segments(prefix)?;
        let mount = Mount {
            segments,
            display: String::from(prefix),
            drive_type,
            backend,
        };
        if let Some(slot) = self
            .mounts
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|m| m.segments == mount.segments))
        {
            *slot = Some(mount);
        } else if let Some(slot) = self.mounts.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(mount);
        } else {
            self.mounts.push(Some(mount));
        }
        Ok(())
    }

    /// The verbatim prefix of every live mount (for `DriveInfo.GetDrives` -> `DriveInfo.Name`), in
    /// mount order.
    #[must_use]
    pub fn drive_names(&self) -> Vec<String> {
        self.mounts
            .iter()
            .filter_map(|slot| slot.as_ref().map(|m| m.display.clone()))
            .collect()
    }

    /// The index of the mount named `name` (any spelling that normalizes to the same segments -- so
    /// `D:` and `D:\` both find the `D:` mount). A drive query targets the mount ITSELF, hence an
    /// exact-segment match, not the longest-prefix match [`resolve`](Self::resolve) does for a path.
    fn index_by_name(&self, name: &str) -> Option<usize> {
        let segments = Self::segments(name).ok()?;
        self.mounts
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|m| m.segments == segments))
    }

    /// The [`DriveType`] of the mount named `name` (for `DriveInfo.DriveType`), or `None` if none.
    #[must_use]
    pub fn drive_type_of(&self, name: &str) -> Option<DriveType> {
        let index = self.index_by_name(name)?;
        self.mounts[index].as_ref().map(|m| m.drive_type)
    }

    /// The total capacity in bytes of the mount named `name` (for `DriveInfo.TotalSize`).
    pub fn total_size_of(&mut self, name: &str) -> FsResult<u64> {
        let index = self.index_by_name(name).ok_or(FsError::Io)?;
        self.backend(index)?.total_size()
    }

    /// Reformats the mount named `name` (for `DriveInfo.Format`). DESTRUCTIVE; behind the backend's
    /// format capability -- [`FsError::Unsupported`] if the backend or build cannot format.
    pub fn reformat(&mut self, name: &str, fs_hint: &str, param: u32) -> FsResult<()> {
        let index = self.index_by_name(name).ok_or(FsError::Io)?;
        self.backend(index)?.reformat(fs_hint, param)
    }

    /// Unmounts the backend at `prefix` exactly, returning whether one was removed. Handles still
    /// open on it go stale -- their next use reports [`FsError::Io`].
    pub fn unmount(&mut self, prefix: &str) -> FsResult<bool> {
        let segments = Self::segments(prefix)?;
        for slot in &mut self.mounts {
            if slot.as_ref().is_some_and(|m| m.segments == segments) {
                *slot = None;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Whether any backend is mounted (the file-system-present test).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mounts.iter().all(Option::is_none)
    }

    /// Resolves `path` to the longest-matching mount index and the sub-path within it (a rooted path
    /// of the remaining segments). The root mount matches everything; a deeper mount wins by matching
    /// more leading segments. [`FsError::Io`] if no mount covers the path (not even a root).
    fn resolve(&self, path: &str) -> FsResult<(usize, String)> {
        let segments = Self::segments(path)?;
        let mut best: Option<(usize, usize)> = None;
        for (index, slot) in self.mounts.iter().enumerate() {
            let Some(mount) = slot else { continue };
            let plen = mount.segments.len();
            if plen <= segments.len()
                && segments[..plen] == mount.segments[..]
                && best.is_none_or(|(_, best_len)| plen > best_len)
            {
                best = Some((index, plen));
            }
        }
        let (index, plen) = best.ok_or(FsError::Io)?;
        let mut sub = String::from("/");
        sub.push_str(&segments[plen..].join("/"));
        Ok((index, sub))
    }

    /// The backend at `mount`, or [`FsError::Io`] if that slot was unmounted (a stale handle).
    fn backend(&mut self, mount: usize) -> FsResult<&mut BoxedFsBackend> {
        self.mounts
            .get_mut(mount)
            .and_then(Option::as_mut)
            .map(|m| &mut m.backend)
            .ok_or(FsError::Io)
    }

    /// The (mount, backend handle) a global handle routes to.
    fn locate(&self, handle: FileHandle) -> FsResult<(usize, FileHandle)> {
        self.open
            .get(handle as usize)
            .and_then(Option::as_ref)
            .map(|open| (open.mount, open.inner))
            .ok_or(FsError::Io)
    }

    /// Wraps a backend handle from `mount` in a fresh global handle.
    fn install(&mut self, mount: usize, inner: FileHandle) -> FileHandle {
        let open = OpenFile { mount, inner };
        if let Some(index) = self.open.iter().position(Option::is_none) {
            self.open[index] = Some(open);
            index as FileHandle
        } else {
            self.open.push(Some(open));
            (self.open.len() - 1) as FileHandle
        }
    }


    /// Opens `path` on the mount that owns it, returning a global handle.
    pub fn open(&mut self, path: &str, mode: FileMode, access: FileAccess) -> FsResult<FileHandle> {
        let (mount, sub) = self.resolve(path)?;
        let inner = self.backend(mount)?.open(&sub, mode, access)?;
        Ok(self.install(mount, inner))
    }

    /// Reads into `buf` at the file position (the caller supplies the buffer -- for a packed array
    /// the heap slice is obtained separately and passed straight in).
    pub fn read(&mut self, handle: FileHandle, buf: &mut [u8]) -> FsResult<usize> {
        let (mount, inner) = self.locate(handle)?;
        self.backend(mount)?.read(inner, buf)
    }

    /// Writes `buf` at the file position.
    pub fn write(&mut self, handle: FileHandle, buf: &[u8]) -> FsResult<usize> {
        let (mount, inner) = self.locate(handle)?;
        self.backend(mount)?.write(inner, buf)
    }

    /// Moves the file position; returns the new absolute position.
    pub fn seek(&mut self, handle: FileHandle, offset: i64, origin: i32) -> FsResult<i64> {
        let (mount, inner) = self.locate(handle)?;
        self.backend(mount)?.seek(inner, offset, origin)
    }

    /// The open file's length.
    pub fn length(&mut self, handle: FileHandle) -> FsResult<i64> {
        let (mount, inner) = self.locate(handle)?;
        self.backend(mount)?.length(inner)
    }

    /// Truncates or extends the open file.
    pub fn set_length(&mut self, handle: FileHandle, length: i64) -> FsResult<()> {
        let (mount, inner) = self.locate(handle)?;
        self.backend(mount)?.set_length(inner, length)
    }

    /// Flushes the open file to its medium.
    pub fn flush(&mut self, handle: FileHandle) -> FsResult<()> {
        let (mount, inner) = self.locate(handle)?;
        self.backend(mount)?.flush(inner)
    }

    /// Closes a global handle (idempotent; an unknown handle is a no-op).
    pub fn close(&mut self, handle: FileHandle) {
        if let Ok((mount, inner)) = self.locate(handle) {
            if let Ok(backend) = self.backend(mount) {
                backend.close(inner);
            }
            self.open[handle as usize] = None;
        }
    }

    /// Whether a FILE exists at `path`.
    pub fn file_exists(&mut self, path: &str) -> bool {
        match self.resolve(path) {
            Ok((mount, sub)) => self.backend(mount).is_ok_and(|b| b.file_exists(&sub)),
            Err(_) => false,
        }
    }

    /// Whether a DIRECTORY exists at `path`.
    pub fn dir_exists(&mut self, path: &str) -> bool {
        match self.resolve(path) {
            Ok((mount, sub)) => self.backend(mount).is_ok_and(|b| b.dir_exists(&sub)),
            Err(_) => false,
        }
    }

    /// Deletes the file at `path`.
    pub fn delete_file(&mut self, path: &str) -> FsResult<()> {
        let (mount, sub) = self.resolve(path)?;
        self.backend(mount)?.delete_file(&sub)
    }

    /// Creates the directory at `path` (with any missing parents).
    pub fn create_dir(&mut self, path: &str) -> FsResult<()> {
        let (mount, sub) = self.resolve(path)?;
        self.backend(mount)?.create_dir(&sub)
    }

    /// Deletes the directory at `path`.
    pub fn delete_dir(&mut self, path: &str, recursive: bool) -> FsResult<()> {
        let (mount, sub) = self.resolve(path)?;
        self.backend(mount)?.delete_dir(&sub, recursive)
    }

    /// Renames/moves an entry. Cross-mount moves are not a seam operation (a backend cannot rename
    /// onto another medium), so `from` and `to` must resolve to the SAME mount; otherwise
    /// [`FsError::Io`] (a higher layer would copy-then-delete).
    pub fn move_entry(&mut self, from: &str, to: &str) -> FsResult<()> {
        let (from_mount, from_sub) = self.resolve(from)?;
        let (to_mount, to_sub) = self.resolve(to)?;
        if from_mount != to_mount {
            return Err(FsError::Io);
        }
        self.backend(from_mount)?.move_entry(&from_sub, &to_sub)
    }

    /// Lists the directory at `path`.
    pub fn list_dir(&mut self, path: &str) -> FsResult<Vec<DirEntry>> {
        let (mount, sub) = self.resolve(path)?;
        self.backend(mount)?.list_dir(&sub)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::FsBackend;
    use alloc::boxed::Box;

    /// A recording FsBackend: it answers `file_exists` for a known set and records the exact
    /// sub-path it was called with, so a routing test can prove the prefix was stripped correctly.
    #[derive(Debug, Default)]
    struct RecordingFs {
        tag: u8,
        total: u64,
        can_format: bool,
        last_path: alloc::rc::Rc<core::cell::RefCell<String>>,
    }

    impl FsBackend for RecordingFs {
        fn open(&mut self, path: &str, _m: FileMode, _a: FileAccess) -> FsResult<FileHandle> {
            *self.last_path.borrow_mut() = String::from(path);
            Ok(u32::from(self.tag))
        }
        fn read(&mut self, _f: FileHandle, _b: &mut [u8]) -> FsResult<usize> {
            Ok(usize::from(self.tag))
        }
        fn write(&mut self, _f: FileHandle, b: &[u8]) -> FsResult<usize> {
            Ok(b.len())
        }
        fn seek(&mut self, _f: FileHandle, _o: i64, _r: i32) -> FsResult<i64> {
            Ok(0)
        }
        fn length(&mut self, _f: FileHandle) -> FsResult<i64> {
            Ok(0)
        }
        fn set_length(&mut self, _f: FileHandle, _l: i64) -> FsResult<()> {
            Ok(())
        }
        fn flush(&mut self, _f: FileHandle) -> FsResult<()> {
            Ok(())
        }
        fn close(&mut self, _f: FileHandle) {}
        fn file_exists(&mut self, path: &str) -> bool {
            *self.last_path.borrow_mut() = String::from(path);
            true
        }
        fn dir_exists(&mut self, _path: &str) -> bool {
            false
        }
        fn delete_file(&mut self, _path: &str) -> FsResult<()> {
            Ok(())
        }
        fn create_dir(&mut self, _path: &str) -> FsResult<()> {
            Ok(())
        }
        fn delete_dir(&mut self, _path: &str, _r: bool) -> FsResult<()> {
            Ok(())
        }
        fn move_entry(&mut self, _from: &str, _to: &str) -> FsResult<()> {
            Ok(())
        }
        fn list_dir(&mut self, _path: &str) -> FsResult<Vec<DirEntry>> {
            Ok(Vec::new())
        }
        fn total_size(&mut self) -> FsResult<u64> {
            Ok(self.total)
        }
        fn reformat(&mut self, fs_hint: &str, _param: u32) -> FsResult<()> {
            if self.can_format {
                *self.last_path.borrow_mut() = String::from(fs_hint);
                Ok(())
            } else {
                Err(FsError::Unsupported)
            }
        }
    }

    fn recorder(tag: u8) -> (RecordingFs, alloc::rc::Rc<core::cell::RefCell<String>>) {
        let last = alloc::rc::Rc::new(core::cell::RefCell::new(String::new()));
        (
            RecordingFs {
                tag,
                last_path: last.clone(),
                ..Default::default()
            },
            last,
        )
    }

    #[test]
    fn longest_prefix_matches_on_segment_boundaries() {
        let mut table = MountTable::default();
        let (root, root_path) = recorder(1);
        let (sd, sd_path) = recorder(2);
        table.mount("/", Box::new(root)).unwrap();
        table.mount("/sd", Box::new(sd)).unwrap();

        assert!(table.file_exists("/sd/log.txt"));
        assert_eq!(*sd_path.borrow(), "/log.txt");
        assert!(table.file_exists("/sdcard/x"));
        assert_eq!(*root_path.borrow(), "/sdcard/x");
        assert!(table.file_exists("/notes.txt"));
        assert_eq!(*root_path.borrow(), "/notes.txt");
        assert!(table.file_exists("/sd"));
        assert_eq!(*sd_path.borrow(), "/");
    }

    #[test]
    fn a_drive_letter_is_just_a_prefix() {
        let mut table = MountTable::default();
        let (drive, drive_path) = recorder(3);
        table.mount("D:", Box::new(drive)).unwrap();
        assert!(table.file_exists("D:\\log.txt"));
        assert_eq!(*drive_path.borrow(), "/log.txt");
    }

    #[test]
    fn handles_are_re_homed_to_their_issuing_mount() {
        let mut table = MountTable::default();
        table.mount("/", Box::new(recorder(1).0)).unwrap();
        table.mount("/sd", Box::new(recorder(2).0)).unwrap();

        let root_handle = table.open("/a.txt", FileMode::Open, FileAccess::Read).unwrap();
        let sd_handle = table.open("/sd/b.txt", FileMode::Open, FileAccess::Read).unwrap();
        assert_ne!(root_handle, sd_handle);
        assert_eq!(table.read(root_handle, &mut [0u8; 4]).unwrap(), 1);
        assert_eq!(table.read(sd_handle, &mut [0u8; 4]).unwrap(), 2);

        table.close(root_handle);
        assert_eq!(table.read(root_handle, &mut [0u8; 4]), Err(FsError::Io));
    }

    #[test]
    fn a_single_mount_at_root_is_a_plain_backend() {
        let mut table = MountTable::default();
        assert!(table.is_empty());
        table.mount("/", Box::new(recorder(1).0)).unwrap();
        assert!(!table.is_empty());
        assert!(table.file_exists("/anything/at/all.txt"));
    }

    #[test]
    fn unmount_makes_a_backend_and_its_handles_go_away() {
        let mut table = MountTable::default();
        let (root, root_path) = recorder(1);
        table.mount("/", Box::new(root)).unwrap();
        table.mount("/sd", Box::new(recorder(2).0)).unwrap();
        let handle = table.open("/sd/x", FileMode::Open, FileAccess::Read).unwrap();
        assert!(table.unmount("/sd").unwrap());
        assert_eq!(table.read(handle, &mut [0u8; 4]), Err(FsError::Io));
        assert!(table.file_exists("/sd/x"));
        assert_eq!(*root_path.borrow(), "/sd/x");
    }

    #[test]
    fn cross_mount_move_is_refused() {
        let mut table = MountTable::default();
        table.mount("/", Box::new(recorder(1).0)).unwrap();
        table.mount("/sd", Box::new(recorder(2).0)).unwrap();
        assert!(table.move_entry("/a.txt", "/b.txt").is_ok());
        assert_eq!(table.move_entry("/a.txt", "/sd/b.txt"), Err(FsError::Io));
    }

    #[test]
    fn drives_enumerate_with_type_size_and_format() {
        let mut table = MountTable::default();
        table
            .mount_as(
                "D:",
                Box::new(RecordingFs {
                    tag: 2,
                    total: 1_900_000_000,
                    can_format: true,
                    ..Default::default()
                }),
                DriveType::Removable,
            )
            .unwrap();
        table.mount("/", Box::new(recorder(1).0)).unwrap();

        assert_eq!(
            table.drive_names(),
            alloc::vec![String::from("D:"), String::from("/")]
        );
        assert_eq!(table.drive_type_of("D:\\"), Some(DriveType::Removable));
        assert_eq!(table.drive_type_of("/"), Some(DriveType::Fixed));
        assert_eq!(table.total_size_of("D:").unwrap(), 1_900_000_000);
        assert_eq!(table.total_size_of("/").unwrap(), 0);
        assert!(table.reformat("D:", "FAT32", 0).is_ok());
        assert_eq!(table.reformat("/", "FAT", 0), Err(FsError::Unsupported));
        assert_eq!(table.drive_type_of("Z:"), None);
        assert_eq!(table.total_size_of("Z:"), Err(FsError::Io));
    }

    #[test]
    fn a_storage_provider_backs_mount_ram() {
        use crate::Vm;

        #[derive(Debug)]
        struct RamProvider;
        impl StorageProvider for RamProvider {
            fn mount_ram(&mut self, _size: u64) -> FsResult<BoxedFsBackend> {
                Ok(Box::new(RecordingFs {
                    tag: 9,
                    ..Default::default()
                }))
            }
        }

        let mut vm = Vm::new();
        assert_eq!(vm.mount_ram("/ram", 4096), Err(FsError::Unsupported));
        vm.set_storage_provider(Box::new(RamProvider));
        vm.mount_ram("/ram", 4096).unwrap();
        assert_eq!(vm.mounts().drive_type_of("/ram"), Some(DriveType::Ram));
        assert!(vm.mounts().file_exists("/ram/anything"));
    }
}
