//! A FAT file-system backend for the interpreter's [`fs::FsBackend`] seam.

#![no_std]
#![forbid(unsafe_code)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

pub(crate) use lamella_cil_runtime::{block, fs};

mod boot;
mod dir;
mod fat;

#[cfg(feature = "format")]
pub mod format;

#[cfg(test)]
mod tests;

pub use boot::{FatType, Geometry, MountError};

use block::{BlockDevice, BlockError, SECTOR_SIZE};
use dir::{encode_short_name, RawEntry, ATTR_ARCHIVE, ATTR_DIRECTORY, DIR_ENTRY_SIZE};
use fat::FatEntry;
use fs::{DirEntry, FileAccess, FileHandle, FileMode, FsBackend, FsError, FsResult};

/// Reads a little-endian `u16` at byte offset `off`. Callers guarantee `off + 2 <= b.len()`.
pub(crate) fn u16le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

/// Reads a little-endian `u32` at byte offset `off`. Callers guarantee `off + 4 <= b.len()`.
pub(crate) fn u32le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Writes a fresh 32-byte short directory entry into `slot` (which it first clears): the 8.3 name,
/// the attribute byte, the split first-cluster fields, and the size. Times are left zero -- the
/// device has no wall clock to stamp, and no read path depends on them.
fn put_dir_entry(slot: &mut [u8], name: &[u8; 11], attr: u8, ntres: u8, first_cluster: u32, size: u32) {
    slot[..DIR_ENTRY_SIZE].fill(0);
    slot[0..11].copy_from_slice(name);
    slot[11] = attr;
    slot[12] = ntres;
    slot[20..22].copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
    slot[26..28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
    slot[28..32].copy_from_slice(&size.to_le_bytes());
}

/// The on-disk location of one 32-byte directory entry: the sector holding it and its byte offset
/// within that sector. Kept on an open file so its size/first-cluster can be written back, and
/// returned by lookups so a delete/rename can find the slot again.
#[derive(Clone, Copy, Debug)]
struct EntryLoc {
    sector: u32,
    offset: usize,
}

/// Where a directory lives: the fixed root region (FAT12/16) or a cluster chain (any subdirectory,
/// and the FAT32 root). Unifying the two lets one enumerator serve both.
#[derive(Clone, Copy)]
enum DirLocation {
    /// The fixed-size root directory region of a FAT12/16 volume.
    FixedRoot,
    /// A directory stored as a cluster chain starting at this cluster.
    Chain(u32),
}

/// One entry read out of a directory: the owned data the driver acts on, plus where the raw slots
/// live (for write-back, delete and rename).
struct FoundEntry {
    /// Display name: the long name when the entry has one, else the 8.3 name.
    name: String,
    /// The 8.3 short name, always -- so a path segment can match the alias as well as the long name.
    short_name: String,
    is_dir: bool,
    first_cluster: u32,
    size: u32,
    /// The short (8.3) entry's slot.
    loc: EntryLoc,
    /// The long-name entry slots that precede it (empty for a short-only entry), so a delete or
    /// rename clears them too rather than orphaning them.
    lfn_slots: Vec<EntryLoc>,
}

impl FoundEntry {
    /// Whether `segment` names this entry -- matching the long name OR the 8.3 alias,
    /// case-insensitively.
    fn matches(&self, segment: &str) -> bool {
        self.name.eq_ignore_ascii_case(segment) || self.short_name.eq_ignore_ascii_case(segment)
    }
}

/// Reassembles a long name from the run of long directory entries that precede a short entry, as a
/// directory is walked in order. The entries appear highest-ordinal-first; the accumulator verifies
/// the ordinals descend contiguously to 1 with a single shared checksum, then finalizes against the
/// short entry's own checksum.
#[derive(Default)]
struct LfnAcc {
    parts: Vec<(u8, [u16; dir::LFN_CHARS_PER_ENTRY])>,
    slots: Vec<EntryLoc>,
    checksum: u8,
    /// The ordinal expected next (counting down); 0 once the whole set (1..count) is collected.
    expected: u8,
    count: u8,
    ok: bool,
}

impl LfnAcc {
    /// Discards any partial set (a gap or inconsistency broke it).
    fn reset(&mut self) {
        self.parts.clear();
        self.slots.clear();
        self.checksum = 0;
        self.expected = 0;
        self.count = 0;
        self.ok = false;
    }

    /// Folds one long directory entry into the set.
    fn push(&mut self, raw: &RawEntry, loc: EntryLoc) {
        let order = raw.lfn_order();
        let ordinal = order & dir::LFN_ORDINAL_MASK;
        let checksum = raw.lfn_checksum_byte();
        if order & dir::LAST_LONG_ENTRY != 0 {
            self.parts.clear();
            self.slots.clear();
            self.count = ordinal;
            self.checksum = checksum;
            self.expected = ordinal;
            self.ok = ordinal >= 1 && ordinal as usize <= dir::MAX_LFN_CHARS.div_ceil(dir::LFN_CHARS_PER_ENTRY);
        }
        if self.ok && ordinal == self.expected && checksum == self.checksum {
            self.parts.push((ordinal, raw.lfn_units()));
            self.slots.push(loc);
            self.expected -= 1;
        } else {
            self.ok = false;
        }
    }

    /// If a complete, consistent set was collected AND its checksum matches `short_field`, returns
    /// the reassembled long name and the LFN slot locations; otherwise `None`.
    fn finalize(&mut self, short_field: &[u8; 11]) -> Option<(String, Vec<EntryLoc>)> {
        if !self.ok
            || self.expected != 0
            || self.count == 0
            || self.parts.len() != self.count as usize
            || self.checksum != dir::lfn_checksum(short_field)
        {
            return None;
        }
        let mut ordered = core::mem::take(&mut self.parts);
        ordered.sort_by_key(|(ordinal, _)| *ordinal);
        let mut units: Vec<u16> = Vec::with_capacity(ordered.len() * dir::LFN_CHARS_PER_ENTRY);
        for (ordinal, chars) in ordered.iter().enumerate() {
            if chars.0 != ordinal as u8 + 1 {
                return None;
            }
            units.extend_from_slice(&chars.1);
        }
        Some((dir::lfn_name_from_units(&units), core::mem::take(&mut self.slots)))
    }
}

/// One open file: which chain it addresses, the stream cursor, the directory slot to write its
/// metadata back to, and whether that metadata has changed since it was last flushed.
#[derive(Debug)]
struct OpenFile {
    first_cluster: u32,
    size: u32,
    position: u64,
    access: FileAccess,
    entry: EntryLoc,
    /// `first_cluster`/`size` differ from the on-disk entry and need a write-back on flush/close.
    dirty: bool,
}

/// A mounted FAT volume, implementing [`fs::FsBackend`] over a [`block::BlockDevice`].
///
/// Owns its block device and a one-sector scratch buffer, so operations touch at most 512 bytes of
/// RAM beyond the caller's buffer -- the constrained-target discipline, not a whole-cluster staging
/// area.
#[derive(Debug)]
pub struct FatFs<D: BlockDevice> {
    device: D,
    geom: Geometry,
    open: Vec<Option<OpenFile>>,
    /// Where the next free-cluster scan begins, so allocation does not restart from cluster 2 each
    /// time. Purely a hint; correctness never depends on it.
    next_free_hint: u32,
    /// Reused across every sector read/write; never holds state between operations.
    sector: [u8; SECTOR_SIZE],
}

impl<D: BlockDevice> FatFs<D> {
    /// Mounts the volume on `device` by reading and validating its boot sector. Takes ownership of
    /// the device (recover it with [`FatFs::into_device`]). Fails with a [`MountError`] for a
    /// medium that is not a mountable FAT volume.
    pub fn mount(mut device: D) -> Result<FatFs<D>, MountError> {
        let mut boot = [0u8; SECTOR_SIZE];
        device.read_sectors(0, &mut boot).map_err(MountError::Device)?;
        let geom = boot::parse(&boot)?;
        Ok(FatFs {
            device,
            geom,
            open: Vec::new(),
            next_free_hint: 2,
            sector: [0u8; SECTOR_SIZE],
        })
    }

    /// The mounted volume's geometry (FAT type, cluster layout). Useful to a caller deciding how
    /// to present the medium, and to tests.
    #[must_use]
    pub fn geometry(&self) -> Geometry {
        self.geom
    }

    /// Recovers the underlying block device (unmount).
    pub fn into_device(self) -> D {
        self.device
    }

    /// The medium's size in sectors -- what a managed `DriveInfo.TotalSize` reports (times 512).
    pub fn total_sectors(&mut self) -> FsResult<u64> {
        self.device.sector_count().map_err(BlockError::to_fs_error)
    }

    /// Reformats the underlying medium IN PLACE to `fat_type` / `sectors_per_cluster`, discarding all
    /// data, and re-mounts the fresh empty volume. Open handles are invalidated (the table is
    /// cleared). This is the below-the-seam operation a managed `DriveInfo.Format` drives -- format
    /// writes raw sectors, so it cannot go through the file seam, but a `FatFs` owns its device and
    /// can reformat it without giving it up. Behind the `format` feature.
    #[cfg(feature = "format")]
    pub fn reformat(&mut self, fat_type: FatType, sectors_per_cluster: u8) -> FsResult<()> {
        use crate::format::{format, FormatError, FormatOptions};
        format(
            &mut self.device,
            FormatOptions {
                fat_type,
                sectors_per_cluster,
            },
        )
        .map_err(|error| match error {
            FormatError::BadOptions => FsError::InvalidPath,
            FormatError::TooSmall | FormatError::Unsuitable => FsError::Io,
            FormatError::Device(block) => block.to_fs_error(),
        })?;
        let mut boot = [0u8; SECTOR_SIZE];
        self.device
            .read_sectors(0, &mut boot)
            .map_err(BlockError::to_fs_error)?;
        self.geom = boot::parse(&boot).map_err(|_| FsError::Io)?;
        self.open.clear();
        self.next_free_hint = 2;
        Ok(())
    }

    /// Picks a FAT width for a bare `"FAT"` format request from the medium's sector count, at the
    /// cluster-count boundaries the spec fixes (evaluated at one sector per cluster, the safe worst
    /// case -- a larger cluster only lowers the count, so the chosen width still fits): under 4085
    /// clusters is FAT12, under 65525 is FAT16, larger is FAT32. `format` re-validates and reports
    /// [`FsError::Io`] if the choice is still unsuitable.
    #[cfg(feature = "format")]
    fn auto_fat_type(total_sectors: u64) -> FatType {
        match total_sectors {
            0..=4_084 => FatType::Fat12,
            4_085..=65_524 => FatType::Fat16,
            _ => FatType::Fat32,
        }
    }


    /// Reads sector `lba` into the scratch buffer, projecting a block error onto the file seam.
    fn read_sector(&mut self, lba: u32) -> FsResult<()> {
        self.device
            .read_sectors(u64::from(lba), &mut self.sector)
            .map_err(BlockError::to_fs_error)
    }

    /// Writes the scratch buffer back to sector `lba`.
    fn write_current_sector(&mut self, lba: u32) -> FsResult<()> {
        self.device
            .write_sectors(u64::from(lba), &self.sector)
            .map_err(BlockError::to_fs_error)
    }


    /// Reads the FAT entry for `cluster` and returns what it says about the chain. Handles the
    /// FAT12 case where a 12-bit entry straddles a sector boundary.
    fn next_cluster(&mut self, cluster: u32) -> FsResult<FatEntry> {
        let geom = self.geom;
        let byte_off = fat::entry_byte_offset(geom.fat_type, cluster);
        let width = fat::entry_read_width(geom.fat_type);
        let sector = geom.fat_start_sector() + byte_off / SECTOR_SIZE as u32;
        let off_in_sector = (byte_off % SECTOR_SIZE as u32) as usize;

        let mut window = [0u8; 4];
        self.read_sector(sector)?;
        let available = SECTOR_SIZE - off_in_sector;
        if available >= width {
            window[..width].copy_from_slice(&self.sector[off_in_sector..off_in_sector + width]);
        } else {
            window[..available].copy_from_slice(&self.sector[off_in_sector..]);
            self.read_sector(sector + 1)?;
            window[available..width].copy_from_slice(&self.sector[..width - available]);
        }
        Ok(fat::decode(geom.fat_type, cluster, &window[..width]))
    }

    /// The end-of-chain value to write for this volume's FAT width.
    fn eoc_value(&self) -> u32 {
        match self.geom.fat_type {
            FatType::Fat12 => 0x0FFF,
            FatType::Fat16 => 0xFFFF,
            FatType::Fat32 => 0x0FFF_FFFF,
        }
    }

    /// Writes cluster `cluster`'s FAT entry to `value` in EVERY FAT copy, so the mirror never
    /// drifts from the primary. Read-modify-writes the affected sector(s), preserving the bits the
    /// entry width does not own (FAT12's shared nibble, FAT32's reserved top four).
    fn set_fat_entry(&mut self, cluster: u32, value: u32) -> FsResult<()> {
        let geom = self.geom;
        let width = fat::entry_read_width(geom.fat_type);
        let byte_off = fat::entry_byte_offset(geom.fat_type, cluster);
        for copy in 0..u32::from(geom.num_fats) {
            let fat_base = geom.fat_start_sector() + copy * geom.fat_size_sectors;
            let sector = fat_base + byte_off / SECTOR_SIZE as u32;
            let off = (byte_off % SECTOR_SIZE as u32) as usize;
            let available = SECTOR_SIZE - off;
            let mut window = [0u8; 4];
            if available >= width {
                self.read_sector(sector)?;
                window[..width].copy_from_slice(&self.sector[off..off + width]);
                fat::encode(geom.fat_type, cluster, value, &mut window[..width]);
                self.sector[off..off + width].copy_from_slice(&window[..width]);
                self.write_current_sector(sector)?;
            } else {
                self.read_sector(sector)?;
                window[..available].copy_from_slice(&self.sector[off..]);
                self.read_sector(sector + 1)?;
                window[available..width].copy_from_slice(&self.sector[..width - available]);
                fat::encode(geom.fat_type, cluster, value, &mut window[..width]);
                self.sector[..width - available].copy_from_slice(&window[available..width]);
                self.write_current_sector(sector + 1)?;
                self.read_sector(sector)?;
                self.sector[off..].copy_from_slice(&window[..available]);
                self.write_current_sector(sector)?;
            }
        }
        Ok(())
    }

    /// Allocates one free cluster, marks it end-of-chain, and returns it. Scans the FAT from the
    /// free hint, wrapping once. [`FsError::Io`] if the volume is full (mapped by the managed layer
    /// to an `IOException`).
    fn alloc_cluster(&mut self) -> FsResult<u32> {
        let total = self.geom.count_of_clusters;
        let mut cluster = self.next_free_hint;
        for _ in 0..=total {
            if cluster < 2 || cluster > total + 1 {
                cluster = 2;
            }
            if matches!(self.next_cluster(cluster)?, FatEntry::Free) {
                self.set_fat_entry(cluster, self.eoc_value())?;
                self.next_free_hint = cluster + 1;
                return Ok(cluster);
            }
            cluster += 1;
        }
        Err(FsError::Io)
    }

    /// The volume's unused capacity in bytes: every FAT entry in the data region that reads
    /// [`FatEntry::Free`], times the cluster size.
    ///
    /// Counted from the FAT itself on every call rather than cached, and the reason is the same one
    /// that makes a cached count wrong on a real card: this volume is not the only thing that has
    /// ever written to the medium. A card arrives with a history, and a figure carried over from
    /// mount time would drift from the FAT the moment anything allocated. FAT32 keeps a hint of this
    /// number in its FSInfo sector, and the specification is explicit that the hint may be wrong --
    /// so the FAT is the only answer worth reporting.
    ///
    /// Valid data clusters are `2 ..= count_of_clusters + 1`, the same range
    /// [`FatFs::alloc_cluster`] scans; entries 0 and 1 are the media descriptor and the end-of-chain
    /// marker, never allocatable, and counting them would overstate a full volume by two clusters.
    pub fn free_space(&mut self) -> FsResult<u64> {
        let total = self.geom.count_of_clusters;
        let mut free: u64 = 0;
        for cluster in 2..=total + 1 {
            if matches!(self.next_cluster(cluster)?, FatEntry::Free) {
                free += 1;
            }
        }
        Ok(free * u64::from(self.geom.sectors_per_cluster) * SECTOR_SIZE as u64)
    }

    /// Frees an entire cluster chain (sets each entry to 0), and lets the freed clusters be
    /// reused by pulling the allocation hint back to the lowest one freed.
    fn free_chain(&mut self, first: u32) -> FsResult<()> {
        let geom = self.geom;
        let mut cluster = first;
        let mut guard = geom.count_of_clusters + 2;
        while geom.is_valid_cluster(cluster) {
            let next = self.next_cluster(cluster)?;
            self.set_fat_entry(cluster, 0)?;
            if cluster < self.next_free_hint {
                self.next_free_hint = cluster;
            }
            match next {
                FatEntry::Next(n) => cluster = n,
                FatEntry::End | FatEntry::Free | FatEntry::Bad => break,
            }
            guard = guard.checked_sub(1).ok_or(FsError::Io)?;
        }
        Ok(())
    }

    /// Zero-fills every sector of `cluster` (used when a cluster joins a directory, or a file's
    /// data must read back as zero in a gap).
    fn zero_cluster(&mut self, cluster: u32) -> FsResult<()> {
        let geom = self.geom;
        let first = geom.cluster_start_sector(cluster);
        self.sector = [0u8; SECTOR_SIZE];
        for i in 0..u32::from(geom.sectors_per_cluster) {
            self.write_current_sector(first + i)?;
        }
        Ok(())
    }


    /// The location of the root directory for this volume's FAT type.
    fn root_location(&self) -> DirLocation {
        match self.geom.fat_type {
            FatType::Fat32 => DirLocation::Chain(self.geom.root_cluster),
            FatType::Fat12 | FatType::Fat16 => DirLocation::FixedRoot,
        }
    }

    /// The location a directory ENTRY points at. A directory reference of cluster 0 denotes the
    /// root (the convention a `..` entry in a first-level subdirectory uses).
    fn location_of(&self, first_cluster: u32) -> DirLocation {
        if first_cluster == 0 {
            self.root_location()
        } else {
            DirLocation::Chain(first_cluster)
        }
    }

    /// The cluster number to store in a `..` entry that points at directory `loc` -- always 0 when
    /// `loc` is the root (fixed or FAT32), per the spec.
    fn dotdot_cluster(&self, loc: DirLocation) -> u32 {
        match loc {
            DirLocation::FixedRoot => 0,
            DirLocation::Chain(c) => {
                if self.geom.fat_type == FatType::Fat32 && c == self.geom.root_cluster {
                    0
                } else {
                    c
                }
            }
        }
    }

    /// Reads one directory sector and appends its live 8.3 entries to `out`. Returns `true` if the
    /// end-of-directory marker was reached (enumeration should stop). `.`/`..`, deleted, long-name
    /// and volume-label slots are skipped.
    fn read_dir_sector(
        &mut self,
        lba: u32,
        out: &mut Vec<FoundEntry>,
        acc: &mut LfnAcc,
    ) -> FsResult<bool> {
        self.read_sector(lba)?;
        for slot in 0..(SECTOR_SIZE / DIR_ENTRY_SIZE) {
            let offset = slot * DIR_ENTRY_SIZE;
            let loc = EntryLoc { sector: lba, offset };
            let raw = RawEntry::new(&self.sector[offset..offset + DIR_ENTRY_SIZE]);
            if raw.is_end() {
                return Ok(true);
            }
            if raw.is_free() {
                acc.reset();
                continue;
            }
            if raw.is_long_name() {
                acc.push(&raw, loc);
                continue;
            }
            if raw.is_volume_id() || raw.is_dot_entry() {
                acc.reset();
                continue;
            }
            let short_name = raw.short_name();
            let field = raw.short_name_field();
            let is_dir = raw.is_dir();
            let first_cluster = raw.first_cluster();
            let size = raw.file_size();
            let (name, lfn_slots) = match acc.finalize(&field) {
                Some((long, slots)) => (long, slots),
                None => (short_name.clone(), Vec::new()),
            };
            acc.reset();
            out.push(FoundEntry {
                name,
                short_name,
                is_dir,
                first_cluster,
                size,
                loc,
                lfn_slots,
            });
        }
        Ok(false)
    }

    /// Collects every live entry of the directory at `loc`.
    fn collect_dir(&mut self, loc: DirLocation) -> FsResult<Vec<FoundEntry>> {
        let mut out = Vec::new();
        let mut acc = LfnAcc::default();
        match loc {
            DirLocation::FixedRoot => {
                let start = self.geom.root_dir_start_sector();
                for i in 0..self.geom.root_dir_sectors {
                    if self.read_dir_sector(start + i, &mut out, &mut acc)? {
                        break;
                    }
                }
            }
            DirLocation::Chain(start) => self.walk_dir_chain(start, &mut out, &mut acc)?,
        }
        Ok(out)
    }

    /// Walks a directory's cluster chain, appending entries, until an end marker (in the data) or
    /// the end of the chain. A cluster count bounds the walk so a corrupt self-referential chain
    /// cannot spin forever.
    fn walk_dir_chain(
        &mut self,
        start: u32,
        out: &mut Vec<FoundEntry>,
        acc: &mut LfnAcc,
    ) -> FsResult<()> {
        let geom = self.geom;
        let mut cluster = start;
        let mut guard = geom.count_of_clusters + 2;
        loop {
            if !geom.is_valid_cluster(cluster) {
                return Err(FsError::Io);
            }
            let first = geom.cluster_start_sector(cluster);
            for i in 0..u32::from(geom.sectors_per_cluster) {
                if self.read_dir_sector(first + i, out, acc)? {
                    return Ok(());
                }
            }
            match self.next_cluster(cluster)? {
                FatEntry::Next(next) => cluster = next,
                FatEntry::End => return Ok(()),
                FatEntry::Free | FatEntry::Bad => return Err(FsError::Io),
            }
            guard = guard.checked_sub(1).ok_or(FsError::Io)?;
        }
    }


    /// The first free slot (`0x00` or `0xE5`) in sector `lba`, if any.
    fn free_slot_in_sector(&mut self, lba: u32) -> FsResult<Option<EntryLoc>> {
        self.read_sector(lba)?;
        for slot in 0..(SECTOR_SIZE / DIR_ENTRY_SIZE) {
            let offset = slot * DIR_ENTRY_SIZE;
            let first_byte = self.sector[offset];
            if first_byte == 0x00 || first_byte == 0xE5 {
                return Ok(Some(EntryLoc { sector: lba, offset }));
            }
        }
        Ok(None)
    }

    /// Finds a free directory slot in `dir`, extending a chained directory by a zeroed cluster if
    /// it is full. A full FIXED root (FAT12/16) cannot grow, so it reports [`FsError::Io`].
    fn alloc_dir_slot(&mut self, dir: DirLocation) -> FsResult<EntryLoc> {
        match dir {
            DirLocation::FixedRoot => {
                let start = self.geom.root_dir_start_sector();
                for i in 0..self.geom.root_dir_sectors {
                    if let Some(loc) = self.free_slot_in_sector(start + i)? {
                        return Ok(loc);
                    }
                }
                Err(FsError::Io)
            }
            DirLocation::Chain(start) => {
                let geom = self.geom;
                let mut cluster = start;
                let mut guard = geom.count_of_clusters + 2;
                loop {
                    if !geom.is_valid_cluster(cluster) {
                        return Err(FsError::Io);
                    }
                    let first = geom.cluster_start_sector(cluster);
                    for i in 0..u32::from(geom.sectors_per_cluster) {
                        if let Some(loc) = self.free_slot_in_sector(first + i)? {
                            return Ok(loc);
                        }
                    }
                    match self.next_cluster(cluster)? {
                        FatEntry::Next(next) => cluster = next,
                        FatEntry::End => {
                            let grown = self.alloc_cluster()?;
                            self.zero_cluster(grown)?;
                            self.set_fat_entry(cluster, grown)?;
                            return Ok(EntryLoc {
                                sector: geom.cluster_start_sector(grown),
                                offset: 0,
                            });
                        }
                        FatEntry::Free | FatEntry::Bad => return Err(FsError::Io),
                    }
                    guard = guard.checked_sub(1).ok_or(FsError::Io)?;
                }
            }
        }
    }

    /// Writes a fresh entry into the slot at `loc`.
    fn write_new_entry(
        &mut self,
        loc: EntryLoc,
        name: &[u8; 11],
        attr: u8,
        ntres: u8,
        first_cluster: u32,
        size: u32,
    ) -> FsResult<()> {
        self.read_sector(loc.sector)?;
        put_dir_entry(
            &mut self.sector[loc.offset..loc.offset + DIR_ENTRY_SIZE],
            name,
            attr,
            ntres,
            first_cluster,
            size,
        );
        self.write_current_sector(loc.sector)
    }

    /// Updates just the first-cluster and size fields of the entry at `loc` (a file's metadata
    /// after a write), leaving name/attributes untouched.
    fn write_entry_metadata(
        &mut self,
        loc: EntryLoc,
        first_cluster: u32,
        size: u32,
    ) -> FsResult<()> {
        self.read_sector(loc.sector)?;
        let base = loc.offset;
        self.sector[base + 20..base + 22]
            .copy_from_slice(&((first_cluster >> 16) as u16).to_le_bytes());
        self.sector[base + 26..base + 28].copy_from_slice(&(first_cluster as u16).to_le_bytes());
        self.sector[base + 28..base + 32].copy_from_slice(&size.to_le_bytes());
        self.write_current_sector(loc.sector)
    }

    /// Marks the entry at `loc` deleted (first name byte `0xE5`).
    fn mark_entry_deleted(&mut self, loc: EntryLoc) -> FsResult<()> {
        self.read_sector(loc.sector)?;
        self.sector[loc.offset] = 0xE5;
        self.write_current_sector(loc.sector)
    }

    /// Seeds a freshly allocated (zeroed) directory cluster with its `.` and `..` entries. `.`
    /// points at the directory itself; `..` at its parent (0 for the root).
    fn seed_dir_cluster(&mut self, cluster: u32, parent_cluster: u32) -> FsResult<()> {
        let sector = self.geom.cluster_start_sector(cluster);
        self.read_sector(sector)?;
        put_dir_entry(
            &mut self.sector[0..DIR_ENTRY_SIZE],
            b".          ",
            ATTR_DIRECTORY,
            0,
            cluster,
            0,
        );
        put_dir_entry(
            &mut self.sector[DIR_ENTRY_SIZE..2 * DIR_ENTRY_SIZE],
            b"..         ",
            ATTR_DIRECTORY,
            0,
            parent_cluster,
            0,
        );
        self.write_current_sector(sector)
    }

    /// Points a directory's `..` entry (the second slot of its first cluster) at a new parent --
    /// needed when a directory is moved across parents.
    fn update_dotdot(&mut self, dir_cluster: u32, new_parent: u32) -> FsResult<()> {
        let sector = self.geom.cluster_start_sector(dir_cluster);
        self.read_sector(sector)?;
        let base = DIR_ENTRY_SIZE;
        self.sector[base + 20..base + 22]
            .copy_from_slice(&((new_parent >> 16) as u16).to_le_bytes());
        self.sector[base + 26..base + 28].copy_from_slice(&(new_parent as u16).to_le_bytes());
        self.write_current_sector(sector)
    }


    /// Splits a path into normalized segments: both separators accepted, `.` dropped, `..`
    /// popped, a trailing separator ignored. The root is the empty segment list. A path that
    /// escapes the root, or an empty path, is [`FsError::InvalidPath`].
    fn normalize(path: &str) -> FsResult<Vec<String>> {
        if path.is_empty() {
            return Err(FsError::InvalidPath);
        }
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

    /// Resolves a run of segments naming directories, returning the final directory's location.
    /// A missing or non-directory segment is [`FsError::DirectoryNotFound`].
    fn locate_dir(&mut self, segments: &[String]) -> FsResult<DirLocation> {
        let mut loc = self.root_location();
        for segment in segments {
            let entries = self.collect_dir(loc)?;
            let child = entries
                .iter()
                .find(|entry| entry.matches(segment))
                .ok_or(FsError::DirectoryNotFound)?;
            if !child.is_dir {
                return Err(FsError::DirectoryNotFound);
            }
            let first_cluster = child.first_cluster;
            loc = self.location_of(first_cluster);
        }
        Ok(loc)
    }

    /// Finds the entry named by a non-empty segment list: resolves the parent directories, then
    /// looks up the final name. `Ok(None)` if the name is absent from an existing parent;
    /// `Err(DirectoryNotFound)` if a parent directory is missing.
    fn find(&mut self, segments: &[String]) -> FsResult<Option<FoundEntry>> {
        let (last, parents) = segments
            .split_last()
            .expect("find is only called with a non-empty path");
        let loc = self.locate_dir(parents)?;
        let entries = self.collect_dir(loc)?;
        Ok(entries.into_iter().find(|entry| entry.matches(last)))
    }

    /// Creates an empty file entry for a non-empty path whose parent exists, returning the new short
    /// entry's location. A long name gets an LFN run + 8.3 alias; a plain 8.3 name gets one entry.
    fn create_file_entry(&mut self, segments: &[String]) -> FsResult<EntryLoc> {
        let (last, parents) = segments.split_last().expect("non-empty");
        let parent = self.locate_dir(parents)?;
        self.create_named_entry(parent, last, ATTR_ARCHIVE, 0, 0)
    }

    /// Writes a directory entry named `name` (long or short) into `parent`, returning the SHORT
    /// entry's location. A name already equal to its canonical uppercase 8.3 form gets a single
    /// short entry; otherwise an 8.3 alias plus a run of long-name entries. A name empty of legal
    /// characters, or longer than VFAT's 255 units, is [`FsError::InvalidPath`].
    fn create_named_entry(
        &mut self,
        parent: DirLocation,
        name: &str,
        attr: u8,
        first_cluster: u32,
        size: u32,
    ) -> FsResult<EntryLoc> {
        if let Some((field, ntres)) = dir::short_only_encoding(name) {
            let loc = self.alloc_dir_slot(parent)?;
            self.write_new_entry(loc, &field, attr, ntres, first_cluster, size)?;
            return Ok(loc);
        }
        let units: Vec<u16> = name.encode_utf16().collect();
        if units.is_empty() || units.len() > dir::MAX_LFN_CHARS {
            return Err(FsError::InvalidPath);
        }
        let alias = self.generate_alias(parent, name)?;
        let checksum = dir::lfn_checksum(&alias);
        let lfn_count = units.len().div_ceil(dir::LFN_CHARS_PER_ENTRY);
        let mut padded = units;
        let total = lfn_count * dir::LFN_CHARS_PER_ENTRY;
        if padded.len() < total {
            padded.push(0x0000);
            padded.resize(total, 0xFFFF);
        }
        let run = self.alloc_dir_run(parent, lfn_count + 1)?;
        for (i, &loc) in run.iter().take(lfn_count).enumerate() {
            let ordinal = (lfn_count - i) as u8;
            let order = if i == 0 {
                ordinal | dir::LAST_LONG_ENTRY
            } else {
                ordinal
            };
            let start = (ordinal as usize - 1) * dir::LFN_CHARS_PER_ENTRY;
            let mut chars = [0u16; dir::LFN_CHARS_PER_ENTRY];
            chars.copy_from_slice(&padded[start..start + dir::LFN_CHARS_PER_ENTRY]);
            self.write_lfn_slot(loc, order, &chars, checksum)?;
        }
        let short_loc = run[lfn_count];
        self.write_new_entry(short_loc, &alias, attr, 0, first_cluster, size)?;
        Ok(short_loc)
    }

    /// Generates a unique 8.3 alias (`BASE~N.EXT`) for `name` within `parent`.
    fn generate_alias(&mut self, parent: DirLocation, name: &str) -> FsResult<[u8; 11]> {
        let existing = self.collect_dir(parent)?;
        let (base_src, ext_src) = match name.rfind('.') {
            Some(dot) => (&name[..dot], &name[dot + 1..]),
            None => (name, ""),
        };
        let mut base = dir::clean_short_component(base_src, 8);
        if base.is_empty() {
            base = String::from("_");
        }
        let ext = dir::clean_short_component(ext_src, 3);
        for n in 1u32..=999_999 {
            let tail = alloc::format!("~{n}");
            let stem: String = base.chars().take(8 - tail.len()).collect();
            let candidate = if ext.is_empty() {
                alloc::format!("{stem}{tail}")
            } else {
                alloc::format!("{stem}{tail}.{ext}")
            };
            let field = encode_short_name(&candidate).ok_or(FsError::Io)?;
            let display = dir::short_field_display(&field);
            if !existing
                .iter()
                .any(|entry| entry.short_name.eq_ignore_ascii_case(&display))
            {
                return Ok(field);
            }
        }
        Err(FsError::Io)
    }

    /// Writes one long directory entry into the slot at `loc`.
    fn write_lfn_slot(
        &mut self,
        loc: EntryLoc,
        order: u8,
        chars: &[u16; dir::LFN_CHARS_PER_ENTRY],
        checksum: u8,
    ) -> FsResult<()> {
        self.read_sector(loc.sector)?;
        dir::put_lfn_entry(
            &mut self.sector[loc.offset..loc.offset + DIR_ENTRY_SIZE],
            order,
            chars,
            checksum,
        );
        self.write_current_sector(loc.sector)
    }

    /// Every 32-byte slot in `dir`'s current extent, each flagged free (`0x00`/`0xE5`) or used.
    fn collect_dir_slots(&mut self, dir: DirLocation) -> FsResult<Vec<(EntryLoc, bool)>> {
        let mut slots = Vec::new();
        match dir {
            DirLocation::FixedRoot => {
                let start = self.geom.root_dir_start_sector();
                for i in 0..self.geom.root_dir_sectors {
                    self.collect_slots_in_sector(start + i, &mut slots)?;
                }
            }
            DirLocation::Chain(first) => {
                let geom = self.geom;
                let mut cluster = first;
                let mut guard = geom.count_of_clusters + 2;
                loop {
                    if !geom.is_valid_cluster(cluster) {
                        return Err(FsError::Io);
                    }
                    let cfirst = geom.cluster_start_sector(cluster);
                    for i in 0..u32::from(geom.sectors_per_cluster) {
                        self.collect_slots_in_sector(cfirst + i, &mut slots)?;
                    }
                    match self.next_cluster(cluster)? {
                        FatEntry::Next(next) => cluster = next,
                        FatEntry::End => break,
                        _ => return Err(FsError::Io),
                    }
                    guard = guard.checked_sub(1).ok_or(FsError::Io)?;
                }
            }
        }
        Ok(slots)
    }

    /// Appends every slot of sector `lba` to `out`, flagged free or used.
    fn collect_slots_in_sector(
        &mut self,
        lba: u32,
        out: &mut Vec<(EntryLoc, bool)>,
    ) -> FsResult<()> {
        self.read_sector(lba)?;
        for slot in 0..(SECTOR_SIZE / DIR_ENTRY_SIZE) {
            let offset = slot * DIR_ENTRY_SIZE;
            let free = self.sector[offset] == 0x00 || self.sector[offset] == 0xE5;
            out.push((EntryLoc { sector: lba, offset }, free));
        }
        Ok(())
    }

    /// Finds `count` logically-consecutive free slots in `dir`, extending a chained directory as
    /// needed. Returns their locations in order. A full FIXED root cannot grow ([`FsError::Io`]).
    fn alloc_dir_run(&mut self, dir: DirLocation, count: usize) -> FsResult<Vec<EntryLoc>> {
        loop {
            let slots = self.collect_dir_slots(dir)?;
            let mut run_start = 0;
            let mut run_len = 0;
            for (i, (_, free)) in slots.iter().enumerate() {
                if *free {
                    if run_len == 0 {
                        run_start = i;
                    }
                    run_len += 1;
                    if run_len == count {
                        return Ok(slots[run_start..run_start + count]
                            .iter()
                            .map(|(loc, _)| *loc)
                            .collect());
                    }
                } else {
                    run_len = 0;
                }
            }
            match dir {
                DirLocation::Chain(start) => self.extend_dir(start)?,
                DirLocation::FixedRoot => return Err(FsError::Io),
            }
        }
    }

    /// Appends one zeroed cluster to a chained directory.
    fn extend_dir(&mut self, chain_start: u32) -> FsResult<()> {
        let geom = self.geom;
        let mut cluster = chain_start;
        let mut guard = geom.count_of_clusters + 2;
        loop {
            match self.next_cluster(cluster)? {
                FatEntry::Next(next) if geom.is_valid_cluster(next) => cluster = next,
                FatEntry::End => break,
                _ => return Err(FsError::Io),
            }
            guard = guard.checked_sub(1).ok_or(FsError::Io)?;
        }
        let grown = self.alloc_cluster()?;
        self.zero_cluster(grown)?;
        self.set_fat_entry(cluster, grown)
    }

    /// Marks an entry's short slot AND its long-name slots deleted, so no orphaned LFN entries are
    /// left behind.
    fn delete_entry_slots(&mut self, loc: EntryLoc, lfn_slots: &[EntryLoc]) -> FsResult<()> {
        for &slot in lfn_slots {
            self.mark_entry_deleted(slot)?;
        }
        self.mark_entry_deleted(loc)
    }


    /// Reads up to `buf.len()` bytes from the file that starts at `first_cluster` and is `size`
    /// bytes long, beginning at logical byte `position`. Returns the number of bytes read (0 at or
    /// past end of file). A chain shorter than `size` claims is [`FsError::Io`].
    fn read_at(
        &mut self,
        first_cluster: u32,
        size: u32,
        position: u64,
        buf: &mut [u8],
    ) -> FsResult<usize> {
        let geom = self.geom;
        let size = u64::from(size);
        if position >= size || buf.is_empty() {
            return Ok(0);
        }
        if first_cluster == 0 {
            return Err(FsError::Io);
        }
        let want = core::cmp::min(buf.len() as u64, size - position) as usize;
        let bytes_per_cluster = u64::from(geom.bytes_per_cluster());

        let mut cluster = first_cluster;
        for _ in 0..(position / bytes_per_cluster) {
            match self.next_cluster(cluster)? {
                FatEntry::Next(next) if geom.is_valid_cluster(next) => cluster = next,
                _ => return Err(FsError::Io),
            }
        }
        let mut offset_in_cluster = (position % bytes_per_cluster) as usize;

        let mut done = 0usize;
        while done < want {
            if !geom.is_valid_cluster(cluster) {
                return Err(FsError::Io);
            }
            let cluster_first = geom.cluster_start_sector(cluster);
            let mut sector_in_cluster = offset_in_cluster / SECTOR_SIZE;
            let mut off_in_sector = offset_in_cluster % SECTOR_SIZE;
            while sector_in_cluster < usize::from(geom.sectors_per_cluster) && done < want {
                self.read_sector(cluster_first + sector_in_cluster as u32)?;
                let n = core::cmp::min(SECTOR_SIZE - off_in_sector, want - done);
                buf[done..done + n].copy_from_slice(&self.sector[off_in_sector..off_in_sector + n]);
                done += n;
                off_in_sector = 0;
                sector_in_cluster += 1;
            }
            offset_in_cluster = 0;
            if done < want {
                match self.next_cluster(cluster)? {
                    FatEntry::Next(next) => cluster = next,
                    FatEntry::End | FatEntry::Free | FatEntry::Bad => return Err(FsError::Io),
                }
            }
        }
        Ok(done)
    }


    /// Ensures the chain starting at `first_cluster` has enough clusters to hold `need_bytes`,
    /// allocating and zeroing new clusters and linking them. Returns the (possibly newly
    /// allocated) first cluster.
    fn ensure_chain(&mut self, first_cluster: u32, need_bytes: u64) -> FsResult<u32> {
        let geom = self.geom;
        let bytes_per_cluster = u64::from(geom.bytes_per_cluster());
        let need = if need_bytes == 0 {
            0
        } else {
            need_bytes.div_ceil(bytes_per_cluster)
        };
        if need == 0 {
            return Ok(first_cluster);
        }
        let first = if first_cluster == 0 {
            let c = self.alloc_cluster()?;
            self.zero_cluster(c)?;
            c
        } else {
            first_cluster
        };
        let mut cluster = first;
        let mut have = 1u64;
        while have < need {
            match self.next_cluster(cluster)? {
                FatEntry::Next(next) if geom.is_valid_cluster(next) => cluster = next,
                FatEntry::End => {
                    let grown = self.alloc_cluster()?;
                    self.zero_cluster(grown)?;
                    self.set_fat_entry(cluster, grown)?;
                    cluster = grown;
                }
                _ => return Err(FsError::Io),
            }
            have += 1;
        }
        Ok(first)
    }

    /// Applies `len` bytes at file offset `offset` along the chain from `first` (which must already
    /// cover the region): the bytes come from `source` when `Some`, or are zeros when `None`.
    /// Whole sectors are written without a read-back; partial sectors read-modify-write.
    fn apply_region(
        &mut self,
        first: u32,
        offset: u64,
        len: u64,
        source: Option<&[u8]>,
    ) -> FsResult<usize> {
        let geom = self.geom;
        if len == 0 {
            return Ok(0);
        }
        let bytes_per_cluster = u64::from(geom.bytes_per_cluster());

        let mut cluster = first;
        for _ in 0..(offset / bytes_per_cluster) {
            match self.next_cluster(cluster)? {
                FatEntry::Next(next) if geom.is_valid_cluster(next) => cluster = next,
                _ => return Err(FsError::Io),
            }
        }

        let mut remaining = len;
        let mut abs = offset;
        let mut src_off = 0usize;
        while remaining > 0 {
            if !geom.is_valid_cluster(cluster) {
                return Err(FsError::Io);
            }
            let cluster_first = geom.cluster_start_sector(cluster);
            let mut off_in_cluster = (abs % bytes_per_cluster) as usize;
            while off_in_cluster < geom.bytes_per_cluster() as usize && remaining > 0 {
                let sector = cluster_first + (off_in_cluster / SECTOR_SIZE) as u32;
                let off_in_sector = off_in_cluster % SECTOR_SIZE;
                let n = core::cmp::min(SECTOR_SIZE - off_in_sector, remaining as usize);
                if n == SECTOR_SIZE {
                    match source {
                        Some(buf) => self.sector.copy_from_slice(&buf[src_off..src_off + n]),
                        None => self.sector = [0u8; SECTOR_SIZE],
                    }
                } else {
                    self.read_sector(sector)?;
                    match source {
                        Some(buf) => self.sector[off_in_sector..off_in_sector + n]
                            .copy_from_slice(&buf[src_off..src_off + n]),
                        None => self.sector[off_in_sector..off_in_sector + n].fill(0),
                    }
                }
                self.write_current_sector(sector)?;
                remaining -= n as u64;
                abs += n as u64;
                src_off += n;
                off_in_cluster += n;
            }
            if remaining > 0 {
                match self.next_cluster(cluster)? {
                    FatEntry::Next(next) => cluster = next,
                    _ => return Err(FsError::Io),
                }
            }
        }
        Ok(len as usize)
    }

    /// Writes `buf` at file offset `position`, extending the chain and zero-filling any gap left by
    /// a write past the current end. Returns the new first cluster, new size, and bytes written.
    fn write_at(
        &mut self,
        first_cluster: u32,
        size: u64,
        position: u64,
        buf: &[u8],
    ) -> FsResult<(u32, u64, usize)> {
        if buf.is_empty() {
            return Ok((first_cluster, size, 0));
        }
        let end = position + buf.len() as u64;
        let first = self.ensure_chain(first_cluster, end)?;
        if position > size {
            self.apply_region(first, size, position - size, None)?;
        }
        let written = self.apply_region(first, position, buf.len() as u64, Some(buf))?;
        Ok((first, core::cmp::max(size, end), written))
    }

    /// Shrinks the chain to hold exactly `length` bytes, freeing the tail. Returns the new first
    /// cluster (0 when truncated to empty).
    fn truncate_chain(&mut self, first_cluster: u32, length: u64) -> FsResult<u32> {
        let geom = self.geom;
        let bytes_per_cluster = u64::from(geom.bytes_per_cluster());
        if length == 0 {
            if first_cluster != 0 {
                self.free_chain(first_cluster)?;
            }
            return Ok(0);
        }
        let keep = length.div_ceil(bytes_per_cluster);
        let mut cluster = first_cluster;
        for _ in 1..keep {
            match self.next_cluster(cluster)? {
                FatEntry::Next(next) if geom.is_valid_cluster(next) => cluster = next,
                _ => return Err(FsError::Io),
            }
        }
        let tail = match self.next_cluster(cluster)? {
            FatEntry::Next(next) => Some(next),
            FatEntry::End => None,
            _ => return Err(FsError::Io),
        };
        self.set_fat_entry(cluster, self.eoc_value())?;
        if let Some(t) = tail {
            if geom.is_valid_cluster(t) {
                self.free_chain(t)?;
            }
        }
        Ok(first_cluster)
    }


    /// The open-file record for a handle, or [`FsError::Io`] for a stale/unknown handle.
    fn slot(&mut self, handle: FileHandle) -> FsResult<&mut OpenFile> {
        self.open
            .get_mut(handle as usize)
            .and_then(Option::as_mut)
            .ok_or(FsError::Io)
    }

    /// Installs an open file, reusing a freed slot where possible so long-lived sessions do not
    /// grow the table without bound.
    fn allocate_handle(&mut self, file: OpenFile) -> FileHandle {
        if let Some(index) = self.open.iter().position(Option::is_none) {
            self.open[index] = Some(file);
            index as FileHandle
        } else {
            self.open.push(Some(file));
            (self.open.len() - 1) as FileHandle
        }
    }

    /// Writes an open file's dirty metadata back to its directory entry and forces the medium.
    fn flush_open(&mut self, file: FileHandle) -> FsResult<()> {
        let (dirty, entry, first_cluster, size) = {
            let open = self.slot(file)?;
            (open.dirty, open.entry, open.first_cluster, open.size)
        };
        if dirty {
            self.write_entry_metadata(entry, first_cluster, size)?;
            self.slot(file)?.dirty = false;
        }
        self.device.flush().map_err(BlockError::to_fs_error)
    }

    /// Recursively frees a directory's contents and the directory itself (its own chain and the
    /// entry at `entry_loc`). Recursion depth is the directory nesting depth.
    fn delete_dir_tree(
        &mut self,
        first_cluster: u32,
        entry_loc: EntryLoc,
        lfn_slots: &[EntryLoc],
    ) -> FsResult<()> {
        let children = self.collect_dir(self.location_of(first_cluster))?;
        for child in children {
            if child.is_dir {
                self.delete_dir_tree(child.first_cluster, child.loc, &child.lfn_slots)?;
            } else {
                if child.first_cluster != 0 {
                    self.free_chain(child.first_cluster)?;
                }
                self.delete_entry_slots(child.loc, &child.lfn_slots)?;
            }
        }
        if first_cluster != 0 {
            self.free_chain(first_cluster)?;
        }
        self.delete_entry_slots(entry_loc, lfn_slots)
    }
}

impl<D: BlockDevice> FsBackend for FatFs<D> {
    fn open(&mut self, path: &str, mode: FileMode, access: FileAccess) -> FsResult<FileHandle> {
        let requires_write = matches!(
            mode,
            FileMode::CreateNew | FileMode::Create | FileMode::Truncate | FileMode::Append
        );
        if requires_write && access == FileAccess::Read {
            return Err(FsError::InvalidPath);
        }

        let segments = Self::normalize(path)?;
        if segments.is_empty() {
            return Err(FsError::IsDirectory);
        }

        match self.find(&segments)? {
            Some(entry) if entry.is_dir => Err(FsError::IsDirectory),
            Some(entry) => match mode {
                FileMode::CreateNew => Err(FsError::AlreadyExists),
                FileMode::Create | FileMode::Truncate => {
                    if entry.first_cluster != 0 {
                        self.free_chain(entry.first_cluster)?;
                    }
                    self.write_entry_metadata(entry.loc, 0, 0)?;
                    self.device.flush().map_err(BlockError::to_fs_error)?;
                    Ok(self.allocate_handle(OpenFile {
                        first_cluster: 0,
                        size: 0,
                        position: 0,
                        access,
                        entry: entry.loc,
                        dirty: false,
                    }))
                }
                FileMode::Append => Ok(self.allocate_handle(OpenFile {
                    first_cluster: entry.first_cluster,
                    size: entry.size,
                    position: u64::from(entry.size),
                    access,
                    entry: entry.loc,
                    dirty: false,
                })),
                FileMode::Open | FileMode::OpenOrCreate => Ok(self.allocate_handle(OpenFile {
                    first_cluster: entry.first_cluster,
                    size: entry.size,
                    position: 0,
                    access,
                    entry: entry.loc,
                    dirty: false,
                })),
            },
            None => match mode {
                FileMode::Open | FileMode::Truncate => Err(FsError::NotFound),
                FileMode::CreateNew
                | FileMode::Create
                | FileMode::OpenOrCreate
                | FileMode::Append => {
                    let loc = self.create_file_entry(&segments)?;
                    self.device.flush().map_err(BlockError::to_fs_error)?;
                    Ok(self.allocate_handle(OpenFile {
                        first_cluster: 0,
                        size: 0,
                        position: 0,
                        access,
                        entry: loc,
                        dirty: false,
                    }))
                }
            },
        }
    }

    fn read(&mut self, file: FileHandle, buf: &mut [u8]) -> FsResult<usize> {
        let (first_cluster, size, position) = {
            let open = self.slot(file)?;
            (open.first_cluster, open.size, open.position)
        };
        let read = self.read_at(first_cluster, size, position, buf)?;
        self.slot(file)?.position += read as u64;
        Ok(read)
    }

    fn write(&mut self, file: FileHandle, buf: &[u8]) -> FsResult<usize> {
        let (first_cluster, size, position, access) = {
            let open = self.slot(file)?;
            (open.first_cluster, u64::from(open.size), open.position, open.access)
        };
        if access == FileAccess::Read {
            return Err(FsError::AccessDenied);
        }
        if position + buf.len() as u64 > u64::from(u32::MAX) {
            return Err(FsError::Io);
        }
        let (new_first, new_size, written) = self.write_at(first_cluster, size, position, buf)?;
        let open = self.slot(file)?;
        open.first_cluster = new_first;
        open.size = new_size as u32;
        open.position += written as u64;
        open.dirty = true;
        Ok(written)
    }

    fn seek(&mut self, file: FileHandle, offset: i64, origin: i32) -> FsResult<i64> {
        let (size, position) = {
            let open = self.slot(file)?;
            (open.size as i64, open.position as i64)
        };
        let base = match origin {
            0 => 0,
            1 => position,
            2 => size,
            _ => return Err(FsError::InvalidPath),
        };
        let target = base.checked_add(offset).ok_or(FsError::InvalidPath)?;
        if target < 0 {
            return Err(FsError::InvalidPath);
        }
        self.slot(file)?.position = target as u64;
        Ok(target)
    }

    fn length(&mut self, file: FileHandle) -> FsResult<i64> {
        Ok(i64::from(self.slot(file)?.size))
    }

    fn set_length(&mut self, file: FileHandle, length: i64) -> FsResult<()> {
        if length < 0 || length > i64::from(u32::MAX) {
            return Err(FsError::InvalidPath);
        }
        let length = length as u64;
        let (first_cluster, size, access) = {
            let open = self.slot(file)?;
            (open.first_cluster, u64::from(open.size), open.access)
        };
        if access == FileAccess::Read {
            return Err(FsError::AccessDenied);
        }
        let new_first = if length < size {
            self.truncate_chain(first_cluster, length)?
        } else if length > size {
            let first = self.ensure_chain(first_cluster, length)?;
            self.apply_region(first, size, length - size, None)?;
            first
        } else {
            first_cluster
        };
        let open = self.slot(file)?;
        open.first_cluster = new_first;
        open.size = length as u32;
        open.dirty = true;
        if open.position > length {
            open.position = length;
        }
        Ok(())
    }

    fn flush(&mut self, file: FileHandle) -> FsResult<()> {
        self.flush_open(file)
    }

    fn close(&mut self, file: FileHandle) {
        let dirty = self.slot(file).map(|open| open.dirty).unwrap_or(false);
        if dirty {
            let _ = self.flush_open(file);
        }
        if let Some(slot) = self.open.get_mut(file as usize) {
            *slot = None;
        }
    }

    fn file_exists(&mut self, path: &str) -> bool {
        let Ok(segments) = Self::normalize(path) else {
            return false;
        };
        if segments.is_empty() {
            return false;
        }
        matches!(self.find(&segments), Ok(Some(entry)) if !entry.is_dir)
    }

    fn dir_exists(&mut self, path: &str) -> bool {
        let Ok(segments) = Self::normalize(path) else {
            return false;
        };
        if segments.is_empty() {
            return true;
        }
        matches!(self.find(&segments), Ok(Some(entry)) if entry.is_dir)
    }

    fn delete_file(&mut self, path: &str) -> FsResult<()> {
        let segments = Self::normalize(path)?;
        if segments.is_empty() {
            return Err(FsError::AccessDenied);
        }
        match self.find(&segments)? {
            None => Ok(()),
            Some(entry) if entry.is_dir => Err(FsError::AccessDenied),
            Some(entry) => {
                if entry.first_cluster != 0 {
                    self.free_chain(entry.first_cluster)?;
                }
                self.delete_entry_slots(entry.loc, &entry.lfn_slots)?;
                self.device.flush().map_err(BlockError::to_fs_error)
            }
        }
    }

    fn create_dir(&mut self, path: &str) -> FsResult<()> {
        let segments = Self::normalize(path)?;
        if segments.is_empty() {
            return Ok(());
        }
        let mut loc = self.root_location();
        let mut current_cluster = 0u32;
        for segment in &segments {
            let entries = self.collect_dir(loc)?;
            if let Some(child) = entries
                .iter()
                .find(|entry| entry.matches(segment))
            {
                if !child.is_dir {
                    return Err(FsError::AlreadyExists);
                }
                current_cluster = child.first_cluster;
                loc = self.location_of(child.first_cluster);
            } else {
                let new_cluster = self.alloc_cluster()?;
                self.zero_cluster(new_cluster)?;
                self.seed_dir_cluster(new_cluster, self.dotdot_cluster(loc))?;
                self.create_named_entry(loc, segment, ATTR_DIRECTORY, new_cluster, 0)?;
                current_cluster = new_cluster;
                loc = DirLocation::Chain(new_cluster);
            }
        }
        let _ = current_cluster;
        self.device.flush().map_err(BlockError::to_fs_error)
    }

    fn delete_dir(&mut self, path: &str, recursive: bool) -> FsResult<()> {
        let segments = Self::normalize(path)?;
        if segments.is_empty() {
            return Err(FsError::AccessDenied);
        }
        let entry = match self.find(&segments)? {
            Some(entry) if entry.is_dir => entry,
            Some(_) => return Err(FsError::DirectoryNotFound),
            None => return Err(FsError::DirectoryNotFound),
        };
        let children = self.collect_dir(self.location_of(entry.first_cluster))?;
        if !children.is_empty() && !recursive {
            return Err(FsError::NotEmpty);
        }
        if recursive {
            for child in children {
                if child.is_dir {
                    self.delete_dir_tree(child.first_cluster, child.loc, &child.lfn_slots)?;
                } else {
                    if child.first_cluster != 0 {
                        self.free_chain(child.first_cluster)?;
                    }
                    self.delete_entry_slots(child.loc, &child.lfn_slots)?;
                }
            }
        }
        if entry.first_cluster != 0 {
            self.free_chain(entry.first_cluster)?;
        }
        self.delete_entry_slots(entry.loc, &entry.lfn_slots)?;
        self.device.flush().map_err(BlockError::to_fs_error)
    }

    fn move_entry(&mut self, from: &str, to: &str) -> FsResult<()> {
        let from_segments = Self::normalize(from)?;
        let to_segments = Self::normalize(to)?;
        if from_segments.is_empty() || to_segments.is_empty() {
            return Err(FsError::AccessDenied);
        }
        let source = self.find(&from_segments)?.ok_or(FsError::NotFound)?;
        if source.is_dir
            && to_segments.len() > from_segments.len()
            && from_segments
                .iter()
                .zip(&to_segments)
                .all(|(from_seg, to_seg)| from_seg.eq_ignore_ascii_case(to_seg))
        {
            return Err(FsError::Io);
        }
        if self.find(&to_segments)?.is_some() {
            return Err(FsError::AlreadyExists);
        }
        let (last, parents) = to_segments.split_last().expect("non-empty");
        let dest_dir = self.locate_dir(parents)?;
        let attr = if source.is_dir {
            ATTR_DIRECTORY
        } else {
            ATTR_ARCHIVE
        };
        self.create_named_entry(dest_dir, last, attr, source.first_cluster, source.size)?;
        if source.is_dir {
            self.update_dotdot(source.first_cluster, self.dotdot_cluster(dest_dir))?;
        }
        self.delete_entry_slots(source.loc, &source.lfn_slots)?;
        self.device.flush().map_err(BlockError::to_fs_error)
    }

    fn list_dir(&mut self, path: &str) -> FsResult<Vec<DirEntry>> {
        let segments = Self::normalize(path)?;
        let loc = if segments.is_empty() {
            self.root_location()
        } else {
            match self.find(&segments)? {
                Some(entry) if entry.is_dir => self.location_of(entry.first_cluster),
                _ => return Err(FsError::DirectoryNotFound),
            }
        };
        let entries = self.collect_dir(loc)?;
        Ok(entries
            .into_iter()
            .map(|entry| DirEntry {
                name: entry.name,
                is_dir: entry.is_dir,
            })
            .collect())
    }

    fn total_size(&mut self) -> FsResult<u64> {
        Ok(self.total_sectors()? * SECTOR_SIZE as u64)
    }

    fn free_space(&mut self) -> FsResult<u64> {
        FatFs::free_space(self)
    }

    /// Maps the managed `DriveInfo.Format(fileSystem, parameter)` onto the inherent
    /// [`FatFs::reformat`]: the `fileSystem` string picks the FAT width ("FAT" = auto-by-size), and
    /// `parameter` is the sectors-per-cluster (`0` = the formatter's default). Only compiled with the
    /// `format` feature -- without it the trait default reports [`FsError::Unsupported`], so a build
    /// that omits mkfs surfaces `NotSupportedException` rather than silently doing nothing.
    #[cfg(feature = "format")]
    fn reformat(&mut self, fs_hint: &str, param: u32) -> FsResult<()> {
        let fat_type = match fs_hint {
            "FAT12" => FatType::Fat12,
            "FAT16" => FatType::Fat16,
            "FAT32" => FatType::Fat32,
            "FAT" => Self::auto_fat_type(self.total_sectors()?),
            _ => return Err(FsError::InvalidPath),
        };
        let sectors_per_cluster = match u8::try_from(param).unwrap_or(0) {
            0 => 1,
            spc => spc,
        };
        FatFs::reformat(self, fat_type, sectors_per_cluster)
    }
}
