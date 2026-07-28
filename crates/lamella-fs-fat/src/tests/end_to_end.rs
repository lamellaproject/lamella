//! End-to-end read-path tests: a [`FatFs`] mounted over a [`MemoryBlockDevice`], with no hardware.

use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;

use crate::block::{MemoryBlockDevice, SECTOR_SIZE};
use crate::dir::{ATTR_ARCHIVE, ATTR_DIRECTORY, ATTR_LONG_NAME, ATTR_VOLUME_ID};
use crate::fs::{FileAccess, FileMode, FsBackend, FsError};
use crate::{FatFs, FatType};

/// Which FAT width the fixture targets; picks a cluster count inside that width's band.
#[derive(Clone, Copy)]
enum Target {
    Fat12,
    Fat16,
    Fat32,
}

/// Builds a valid FAT volume image in a byte vector. Fixed one-sector clusters and two FATs;
/// only FAT #0 is populated (the reader never consults the mirror). See the module note on why
/// this encoder does not make the tests circular.
struct ImageBuilder {
    target: Target,
    sectors_per_cluster: u8,
    num_fats: u8,
    reserved: u16,
    root_entries: u16,
    fat_size: u32,
    total_sectors: u32,
    root_cluster: u32,
    first_data_sector: u32,
    next_free: u32,
    image: Vec<u8>,
}

impl ImageBuilder {
    fn new(target: Target) -> ImageBuilder {
        let sectors_per_cluster: u8 = 1;
        let num_fats: u8 = 2;
        let (reserved, root_entries, count) = match target {
            Target::Fat12 => (1u16, 512u16, 512u32),
            Target::Fat16 => (1u16, 512u16, 4200u32),
            Target::Fat32 => (32u16, 0u16, 65600u32),
        };
        let root_dir_sectors = (u32::from(root_entries) * 32 + (SECTOR_SIZE as u32 - 1)) / SECTOR_SIZE as u32;
        let table_entries = count + 2;
        let fat_bytes = match target {
            Target::Fat12 => (table_entries * 3).div_ceil(2),
            Target::Fat16 => table_entries * 2,
            Target::Fat32 => table_entries * 4,
        };
        let fat_size = fat_bytes.div_ceil(SECTOR_SIZE as u32);
        let first_data_sector =
            u32::from(reserved) + u32::from(num_fats) * fat_size + root_dir_sectors;
        let total_sectors = first_data_sector + count * u32::from(sectors_per_cluster);
        let (root_cluster, next_free) = match target {
            Target::Fat32 => (2, 3),
            Target::Fat12 | Target::Fat16 => (0, 2),
        };

        let mut builder = ImageBuilder {
            target,
            sectors_per_cluster,
            num_fats,
            reserved,
            root_entries,
            fat_size,
            total_sectors,
            root_cluster,
            first_data_sector,
            next_free,
            image: vec![0u8; total_sectors as usize * SECTOR_SIZE],
        };

        let media = 0xF8u32;
        match target {
            Target::Fat12 => {
                builder.set_fat(0, 0x0F00 | media);
                builder.set_fat(1, 0x0FFF);
            }
            Target::Fat16 => {
                builder.set_fat(0, 0xFF00 | media);
                builder.set_fat(1, 0xFFFF);
            }
            Target::Fat32 => {
                builder.set_fat(0, 0x0FFF_FF00 | media);
                builder.set_fat(1, 0x0FFF_FFFF);
                builder.set_fat(2, 0x0FFF_FFFF);
            }
        }
        builder
    }

    /// The end-of-chain value for this width.
    fn eoc(&self) -> u32 {
        match self.target {
            Target::Fat12 => 0x0FFF,
            Target::Fat16 => 0xFFFF,
            Target::Fat32 => 0x0FFF_FFFF,
        }
    }

    /// Writes cluster `n`'s FAT #0 entry to `value`, matching the exact encoding the reader
    /// decodes (including FAT12 nibble packing).
    fn set_fat(&mut self, n: u32, value: u32) {
        let fat_start = self.reserved as usize * SECTOR_SIZE;
        match self.target {
            Target::Fat12 => {
                let abs = fat_start + (n + n / 2) as usize;
                let current = u16::from_le_bytes([self.image[abs], self.image[abs + 1]]);
                let updated = if n & 1 == 0 {
                    (current & 0xF000) | (value as u16 & 0x0FFF)
                } else {
                    (current & 0x000F) | ((value as u16 & 0x0FFF) << 4)
                };
                self.image[abs..abs + 2].copy_from_slice(&updated.to_le_bytes());
            }
            Target::Fat16 => {
                let abs = fat_start + (n * 2) as usize;
                self.image[abs..abs + 2].copy_from_slice(&(value as u16).to_le_bytes());
            }
            Target::Fat32 => {
                let abs = fat_start + (n * 4) as usize;
                self.image[abs..abs + 4].copy_from_slice(&value.to_le_bytes());
            }
        }
    }

    /// The LBA of a data cluster's first sector.
    fn cluster_first_sector(&self, n: u32) -> u32 {
        self.first_data_sector + (n - 2) * u32::from(self.sectors_per_cluster)
    }

    /// Allocates enough clusters to hold `content`, links them into a chain, writes the bytes, and
    /// returns the first cluster (0 for empty content).
    fn alloc_data(&mut self, content: &[u8]) -> u32 {
        if content.is_empty() {
            return 0;
        }
        let bytes_per_cluster = self.sectors_per_cluster as usize * SECTOR_SIZE;
        let cluster_count = content.len().div_ceil(bytes_per_cluster);
        let clusters: Vec<u32> = (0..cluster_count)
            .map(|_| {
                let c = self.next_free;
                self.next_free += 1;
                c
            })
            .collect();
        for (i, &cluster) in clusters.iter().enumerate() {
            let link = if i + 1 < cluster_count {
                clusters[i + 1]
            } else {
                self.eoc()
            };
            self.set_fat(cluster, link);
        }
        let mut written = 0;
        for &cluster in &clusters {
            let base = self.cluster_first_sector(cluster) as usize * SECTOR_SIZE;
            let take = core::cmp::min(bytes_per_cluster, content.len() - written);
            self.image[base..base + take].copy_from_slice(&content[written..written + take]);
            written += take;
        }
        clusters[0]
    }

    /// Reserves one cluster for a (single-cluster) directory, marking it end-of-chain.
    fn alloc_dir_cluster(&mut self) -> u32 {
        let cluster = self.next_free;
        self.next_free += 1;
        self.set_fat(cluster, self.eoc());
        cluster
    }

    /// Writes directory slots into a cluster's sectors (used for subdirectories and the FAT32
    /// root). The caller keeps entries within one cluster.
    fn write_dir_cluster(&mut self, cluster: u32, entries: &[[u8; 32]]) {
        let base = self.cluster_first_sector(cluster) as usize * SECTOR_SIZE;
        for (i, entry) in entries.iter().enumerate() {
            self.image[base + i * 32..base + i * 32 + 32].copy_from_slice(entry);
        }
    }

    /// Writes the root directory's slots: into the fixed root region on FAT12/16, or into the
    /// root cluster on FAT32.
    fn write_root(&mut self, entries: &[[u8; 32]]) {
        match self.target {
            Target::Fat12 | Target::Fat16 => {
                let base = (u32::from(self.reserved)
                    + u32::from(self.num_fats) * self.fat_size) as usize
                    * SECTOR_SIZE;
                for (i, entry) in entries.iter().enumerate() {
                    self.image[base + i * 32..base + i * 32 + 32].copy_from_slice(entry);
                }
            }
            Target::Fat32 => {
                let root = self.root_cluster;
                self.write_dir_cluster(root, entries);
            }
        }
    }

    /// Writes the boot sector last and yields the finished image.
    fn finish(mut self) -> Vec<u8> {
        self.image[0] = 0xEB;
        self.image[1] = 0x3C;
        self.image[2] = 0x90;
        self.image[11..13].copy_from_slice(&(SECTOR_SIZE as u16).to_le_bytes());
        self.image[13] = self.sectors_per_cluster;
        self.image[14..16].copy_from_slice(&self.reserved.to_le_bytes());
        self.image[16] = self.num_fats;
        self.image[17..19].copy_from_slice(&self.root_entries.to_le_bytes());
        self.image[21] = 0xF8;
        match self.target {
            Target::Fat12 | Target::Fat16 => {
                self.image[19..21].copy_from_slice(&(self.total_sectors as u16).to_le_bytes());
                self.image[22..24].copy_from_slice(&(self.fat_size as u16).to_le_bytes());
                let label: &[u8; 8] = match self.target {
                    Target::Fat12 => b"FAT12   ",
                    _ => b"FAT16   ",
                };
                self.image[54..62].copy_from_slice(label);
            }
            Target::Fat32 => {
                self.image[32..36].copy_from_slice(&self.total_sectors.to_le_bytes());
                self.image[36..40].copy_from_slice(&self.fat_size.to_le_bytes());
                self.image[44..48].copy_from_slice(&self.root_cluster.to_le_bytes());
                self.image[48..50].copy_from_slice(&1u16.to_le_bytes());
                self.image[82..90].copy_from_slice(b"FAT32   ");
            }
        }
        self.image[510] = 0x55;
        self.image[511] = 0xAA;
        self.image
    }
}

/// A 32-byte short directory slot built from an 8.3 name field and the load-bearing fields.
fn dir_entry(name: &[u8; 11], attr: u8, cluster: u32, size: u32) -> [u8; 32] {
    let mut e = [0u8; 32];
    e[0..11].copy_from_slice(name);
    e[11] = attr;
    e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
    e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
    e[28..32].copy_from_slice(&size.to_le_bytes());
    e
}

/// A position-sensitive byte pattern, so a mis-assembled cluster chain reads as wrong data rather
/// than as a plausible fill.
fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + 7) % 256) as u8).collect()
}

/// Mounts an image on a fresh in-memory block device.
fn mount(image: Vec<u8>) -> FatFs<MemoryBlockDevice> {
    let mut device = MemoryBlockDevice::new(image.len() / SECTOR_SIZE);
    device.bytes().copy_from_slice(&image);
    FatFs::mount(device).expect("fixture image mounts")
}

/// A FAT12 image with a nested directory, a multi-cluster file, and slots the reader must skip
/// (deleted / long-name / volume-label / dot).
fn build_fat12() -> Vec<u8> {
    let mut builder = ImageBuilder::new(Target::Fat12);

    let inner = pattern(300);
    let inner_cluster = builder.alloc_data(&inner);
    let sub_cluster = builder.alloc_dir_cluster();
    builder.write_dir_cluster(
        sub_cluster,
        &[
            dir_entry(b".          ", ATTR_DIRECTORY, sub_cluster, 0),
            dir_entry(b"..         ", ATTR_DIRECTORY, 0, 0),
            dir_entry(b"INNER   BIN", ATTR_ARCHIVE, inner_cluster, inner.len() as u32),
        ],
    );

    let hello = pattern(700);
    let hello_cluster = builder.alloc_data(&hello);

    let mut deleted = dir_entry(b"OLD     TXT", ATTR_ARCHIVE, 9, 10);
    deleted[0] = 0xE5;
    builder.write_root(&[
        dir_entry(b"MYVOLUME   ", ATTR_VOLUME_ID, 0, 0),
        dir_entry(b"HELLO   TXT", ATTR_ARCHIVE, hello_cluster, hello.len() as u32),
        deleted,
        dir_entry(b"XXXXXXXXXXX", ATTR_LONG_NAME, 0, 0),
        dir_entry(b"SUB        ", ATTR_DIRECTORY, sub_cluster, 0),
    ]);

    builder.finish()
}

/// A minimal image of `target` with one root file, for a per-width mount-and-read smoke test.
fn build_single_file(target: Target) -> (Vec<u8>, Vec<u8>) {
    let mut builder = ImageBuilder::new(target);
    let content = pattern(1000);
    let cluster = builder.alloc_data(&content);
    builder.write_root(&[dir_entry(b"DATA    BIN", ATTR_ARCHIVE, cluster, content.len() as u32)]);
    (builder.finish(), content)
}

/// Reads a whole open file into a buffer, looping until end of file.
fn read_all(fs: &mut FatFs<MemoryBlockDevice>, handle: u32, len: usize) -> Vec<u8> {
    let mut buf = vec![0u8; len];
    let mut done = 0;
    loop {
        let n = fs.read(handle, &mut buf[done..]).unwrap();
        if n == 0 {
            break;
        }
        done += n;
    }
    buf.truncate(done);
    buf
}

#[test]
fn fat12_read_path_end_to_end() {
    let mut fs = mount(build_fat12());
    assert_eq!(fs.geometry().fat_type, FatType::Fat12);

    let mut listing: Vec<(String, bool)> = fs
        .list_dir("/")
        .unwrap()
        .into_iter()
        .map(|entry| (entry.name, entry.is_dir))
        .collect();
    listing.sort();
    assert_eq!(
        listing,
        vec![("HELLO.TXT".to_string(), false), ("SUB".to_string(), true)]
    );

    assert!(fs.file_exists("/HELLO.TXT"));
    assert!(!fs.file_exists("/SUB"));
    assert!(fs.dir_exists("/SUB"));
    assert!(fs.dir_exists("/"));
    assert!(!fs.file_exists("/NOPE.TXT"));

    let hello = pattern(700);
    let handle = fs.open("/HELLO.TXT", FileMode::Open, FileAccess::Read).unwrap();
    assert_eq!(fs.length(handle).unwrap(), 700);
    assert_eq!(read_all(&mut fs, handle, 700), hello);
    assert_eq!(fs.read(handle, &mut [0u8; 16]).unwrap(), 0);

    assert_eq!(fs.seek(handle, 500, 0).unwrap(), 500);
    let mut window = [0u8; 24];
    assert_eq!(fs.read(handle, &mut window).unwrap(), 24);
    assert_eq!(window[..], hello[500..524]);
    assert_eq!(fs.seek(handle, 9000, 0).unwrap(), 9000);
    assert_eq!(fs.read(handle, &mut window).unwrap(), 0);
    fs.close(handle);

    let inner = pattern(300);
    let handle = fs.open("/sub/inner.bin", FileMode::Open, FileAccess::Read).unwrap();
    assert_eq!(read_all(&mut fs, handle, 300), inner);
    fs.close(handle);

    let sub: Vec<String> = fs.list_dir("/SUB").unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(sub, vec!["INNER.BIN".to_string()]);

    let handle = fs.open("/SUB/../HELLO.TXT", FileMode::Open, FileAccess::Read).unwrap();
    assert_eq!(fs.length(handle).unwrap(), 700);
    fs.close(handle);
}

/// Opens `path` in `mode` for writing, writes all of `data`, and closes it.
fn write_file(fs: &mut FatFs<MemoryBlockDevice>, path: &str, mode: FileMode, data: &[u8]) {
    let handle = fs.open(path, mode, FileAccess::ReadWrite).unwrap();
    let mut offset = 0;
    while offset < data.len() {
        let n = fs.write(handle, &data[offset..]).unwrap();
        assert!(n > 0, "write made no progress");
        offset += n;
    }
    fs.close(handle);
}

/// Opens `path`, reads it whole, and closes it.
fn read_file(fs: &mut FatFs<MemoryBlockDevice>, path: &str) -> Vec<u8> {
    let handle = fs.open(path, FileMode::Open, FileAccess::Read).unwrap();
    let len = fs.length(handle).unwrap() as usize;
    let data = read_all(fs, handle, len);
    fs.close(handle);
    data
}

#[test]
fn error_cases_map_to_the_seam_codes() {
    let mut fs = mount(build_fat12());
    assert_eq!(fs.open("/NOPE.TXT", FileMode::Open, FileAccess::Read).unwrap_err(), FsError::NotFound);
    assert_eq!(fs.open("/SUB", FileMode::Open, FileAccess::Read).unwrap_err(), FsError::IsDirectory);
    assert_eq!(fs.open("/NODIR/x.txt", FileMode::Open, FileAccess::Read).unwrap_err(), FsError::DirectoryNotFound);
    assert_eq!(fs.list_dir("/HELLO.TXT").unwrap_err(), FsError::DirectoryNotFound);
    assert_eq!(fs.open("/HELLO.TXT", FileMode::CreateNew, FileAccess::Write).unwrap_err(), FsError::AlreadyExists);
    assert_eq!(fs.open("/X.TXT", FileMode::Create, FileAccess::Read).unwrap_err(), FsError::InvalidPath);
    let read_handle = fs.open("/HELLO.TXT", FileMode::Open, FileAccess::Read).unwrap();
    assert_eq!(fs.write(read_handle, b"x").unwrap_err(), FsError::AccessDenied);
    fs.close(read_handle);
    let too_long = "x".repeat(300);
    assert_eq!(
        fs.open(&alloc::format!("/{too_long}"), FileMode::CreateNew, FileAccess::Write).unwrap_err(),
        FsError::InvalidPath
    );
    assert_eq!(fs.delete_dir("/SUB", false).unwrap_err(), FsError::NotEmpty);
}

#[test]
fn create_write_and_read_back() {
    let mut fs = mount(build_fat12());
    let data = pattern(1500);
    write_file(&mut fs, "/notes.txt", FileMode::CreateNew, &data);
    assert!(fs.file_exists("/NOTES.TXT"));
    assert_eq!(read_file(&mut fs, "/notes.txt"), data);
    let names: Vec<String> = fs.list_dir("/").unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"notes.txt".to_string()));
}

#[test]
fn write_persists_across_a_remount() {
    let data = pattern(2000);
    let mut fs = mount(build_fat12());
    write_file(&mut fs, "/PERSIST.BIN", FileMode::CreateNew, &data);
    let device = fs.into_device();
    assert!(device.flushes > 0, "a write must flush the medium");
    let mut remounted = FatFs::mount(device).unwrap();
    assert_eq!(read_file(&mut remounted, "/persist.bin"), data);
    assert_eq!(read_file(&mut remounted, "/HELLO.TXT"), pattern(700));
}

#[test]
fn overwrite_append_and_resize() {
    let mut fs = mount(build_fat12());
    write_file(&mut fs, "/F.BIN", FileMode::CreateNew, &pattern(600));

    let overlay: Vec<u8> = (0..100).map(|i| 0x80 | i as u8).collect();
    let handle = fs.open("/F.BIN", FileMode::Open, FileAccess::Write).unwrap();
    assert_eq!(fs.write(handle, &overlay).unwrap(), 100);
    fs.close(handle);
    let mut expected = pattern(600);
    expected[..100].copy_from_slice(&overlay);
    assert_eq!(read_file(&mut fs, "/F.BIN"), expected);

    let tail = pattern(300);
    let handle = fs.open("/F.BIN", FileMode::Append, FileAccess::Write).unwrap();
    assert_eq!(fs.length(handle).unwrap(), 600);
    assert_eq!(fs.write(handle, &tail).unwrap(), 300);
    fs.close(handle);
    expected.extend_from_slice(&tail);
    assert_eq!(read_file(&mut fs, "/F.BIN"), expected);

    let handle = fs.open("/F.BIN", FileMode::Open, FileAccess::ReadWrite).unwrap();
    fs.set_length(handle, 1000).unwrap();
    fs.close(handle);
    let extended = read_file(&mut fs, "/F.BIN");
    assert_eq!(extended.len(), 1000);
    assert_eq!(&extended[..900], &expected[..]);
    assert!(extended[900..].iter().all(|&b| b == 0), "the extension must be zero-filled");

    let handle = fs.open("/F.BIN", FileMode::Open, FileAccess::ReadWrite).unwrap();
    fs.set_length(handle, 50).unwrap();
    fs.close(handle);
    assert_eq!(read_file(&mut fs, "/F.BIN"), expected[..50].to_vec());
}

#[test]
fn delete_frees_clusters_for_reuse() {
    let mut fs = mount(build_fat12());
    write_file(&mut fs, "/BIG.BIN", FileMode::CreateNew, &pattern(2000));
    assert!(fs.file_exists("/BIG.BIN"));
    fs.delete_file("/BIG.BIN").unwrap();
    assert!(!fs.file_exists("/BIG.BIN"));
    fs.delete_file("/BIG.BIN").unwrap();
    write_file(&mut fs, "/AGAIN.BIN", FileMode::CreateNew, &pattern(2000));
    assert_eq!(read_file(&mut fs, "/AGAIN.BIN"), pattern(2000));
}

#[test]
fn create_and_delete_directories() {
    let mut fs = mount(build_fat12());
    fs.create_dir("/a/b/c").unwrap();
    assert!(fs.dir_exists("/a") && fs.dir_exists("/a/b") && fs.dir_exists("/a/b/c"));
    write_file(&mut fs, "/a/b/c/deep.txt", FileMode::CreateNew, b"hello");
    assert_eq!(read_file(&mut fs, "/A/B/C/DEEP.TXT"), b"hello");
    fs.create_dir("/a/b").unwrap();
    assert_eq!(fs.delete_dir("/a", false).unwrap_err(), FsError::NotEmpty);
    fs.create_dir("/empty").unwrap();
    fs.delete_dir("/empty", false).unwrap();
    assert!(!fs.dir_exists("/empty"));
    fs.delete_dir("/a", true).unwrap();
    assert!(!fs.dir_exists("/a"));
    assert!(!fs.file_exists("/a/b/c/deep.txt"));
}

#[test]
fn rename_and_move_entries() {
    let mut fs = mount(build_fat12());
    let hello = read_file(&mut fs, "/HELLO.TXT");
    fs.move_entry("/HELLO.TXT", "/RENAMED.TXT").unwrap();
    assert!(!fs.file_exists("/HELLO.TXT") && fs.file_exists("/RENAMED.TXT"));
    assert_eq!(read_file(&mut fs, "/RENAMED.TXT"), hello);

    fs.create_dir("/DEST").unwrap();
    fs.move_entry("/RENAMED.TXT", "/DEST/MOVED.TXT").unwrap();
    assert_eq!(read_file(&mut fs, "/DEST/MOVED.TXT"), hello);

    write_file(&mut fs, "/OCCUPIED.TXT", FileMode::CreateNew, b"x");
    assert_eq!(
        fs.move_entry("/DEST/MOVED.TXT", "/OCCUPIED.TXT").unwrap_err(),
        FsError::AlreadyExists
    );

    fs.move_entry("/SUB", "/DEST/SUB").unwrap();
    assert!(fs.dir_exists("/DEST/SUB"));
    assert!(fs.file_exists("/DEST/SUB/INNER.BIN"));

    fs.create_dir("/TREE/LEAF").unwrap();
    assert_eq!(fs.move_entry("/TREE", "/TREE/LEAF/X").unwrap_err(), FsError::Io);
}

#[test]
fn fat32_write_and_remount() {
    let (image, original) = build_single_file(Target::Fat32);
    let mut fs = mount(image);
    assert_eq!(fs.geometry().fat_type, FatType::Fat32);
    let data = pattern(3000);
    write_file(&mut fs, "/NEW32.BIN", FileMode::CreateNew, &data);
    let device = fs.into_device();
    let mut remounted = FatFs::mount(device).unwrap();
    assert_eq!(read_file(&mut remounted, "/NEW32.BIN"), data);
    assert_eq!(read_file(&mut remounted, "/DATA.BIN"), original);
}


/// Builds the directory slots for a long-named file: the long-name run (highest ordinal first, per
/// spec) followed by its 8.3 short entry. Uses the crate's own LDIR primitives (each pinned to the
/// spec by a `dir` unit test), so this fixture exercises the enumerator's ASSEMBLY, not the writer.
fn long_name_entries(long: &str, short: &[u8; 11], cluster: u32, size: u32) -> Vec<[u8; 32]> {
    let checksum = crate::dir::lfn_checksum(short);
    let units: Vec<u16> = long.encode_utf16().collect();
    let lfn_count = units.len().div_ceil(crate::dir::LFN_CHARS_PER_ENTRY);
    let mut padded = units;
    let total = lfn_count * crate::dir::LFN_CHARS_PER_ENTRY;
    if padded.len() < total {
        padded.push(0x0000);
        padded.resize(total, 0xFFFF);
    }
    let mut entries = Vec::new();
    for i in 0..lfn_count {
        let ordinal = (lfn_count - i) as u8;
        let order = if i == 0 {
            ordinal | crate::dir::LAST_LONG_ENTRY
        } else {
            ordinal
        };
        let start = (ordinal as usize - 1) * crate::dir::LFN_CHARS_PER_ENTRY;
        let mut chars = [0u16; crate::dir::LFN_CHARS_PER_ENTRY];
        chars.copy_from_slice(&padded[start..start + crate::dir::LFN_CHARS_PER_ENTRY]);
        let mut slot = [0u8; 32];
        crate::dir::put_lfn_entry(&mut slot, order, &chars, checksum);
        entries.push(slot);
    }
    entries.push(dir_entry(short, ATTR_ARCHIVE, cluster, size));
    entries
}

#[test]
fn reads_a_long_name_from_a_built_fixture() {
    let mut builder = ImageBuilder::new(Target::Fat12);
    let content = pattern(200);
    let cluster = builder.alloc_data(&content);
    let root = long_name_entries("A Long File Name.txt", b"ALONGF~1TXT", cluster, content.len() as u32);
    builder.write_root(&root);
    let mut fs = mount(builder.finish());
    assert_eq!(
        fs.list_dir("/").unwrap().into_iter().map(|e| e.name).collect::<Vec<_>>(),
        vec!["A Long File Name.txt".to_string()]
    );
    assert_eq!(read_file(&mut fs, "/A Long File Name.txt"), content);
    assert_eq!(read_file(&mut fs, "/a long file name.txt"), content);
    assert_eq!(read_file(&mut fs, "/ALONGF~1.TXT"), content);
}

#[test]
fn creates_and_persists_a_long_named_file() {
    let data = pattern(1500);
    let long = "/My Report (Final Version).txt";
    let mut fs = mount(build_fat12());
    write_file(&mut fs, long, FileMode::CreateNew, &data);
    assert!(fs.file_exists(long));
    assert_eq!(read_file(&mut fs, long), data);
    assert!(fs
        .list_dir("/")
        .unwrap()
        .into_iter()
        .any(|e| e.name == "My Report (Final Version).txt"));
    let device = fs.into_device();
    let mut remounted = FatFs::mount(device).unwrap();
    assert_eq!(read_file(&mut remounted, long), data);
    assert!(remounted.file_exists("/my report (final version).txt"));
}

#[test]
fn colliding_long_names_get_distinct_aliases() {
    let mut fs = mount(build_fat12());
    write_file(&mut fs, "/Meeting Notes One.txt", FileMode::CreateNew, b"one");
    write_file(&mut fs, "/Meeting Notes Two.txt", FileMode::CreateNew, b"two");
    let device = fs.into_device();
    let mut remounted = FatFs::mount(device).unwrap();
    assert_eq!(read_file(&mut remounted, "/Meeting Notes One.txt"), b"one");
    assert_eq!(read_file(&mut remounted, "/Meeting Notes Two.txt"), b"two");
    let names: Vec<String> = remounted.list_dir("/").unwrap().into_iter().map(|e| e.name).collect();
    assert!(names.contains(&"Meeting Notes One.txt".to_string()));
    assert!(names.contains(&"Meeting Notes Two.txt".to_string()));
}

#[test]
fn deleting_a_long_named_file_leaves_no_orphan_long_entries() {
    let (image, _) = build_single_file(Target::Fat12);
    let mut fs = mount(image);
    write_file(&mut fs, "/Some Long Document.txt", FileMode::CreateNew, &pattern(100));
    assert!(fs.file_exists("/Some Long Document.txt"));
    fs.delete_file("/Some Long Document.txt").unwrap();
    assert!(!fs.file_exists("/Some Long Document.txt"));

    let geom = fs.geometry();
    let mut device = fs.into_device();
    let image = device.bytes();
    let start = geom.root_dir_start_sector() as usize * SECTOR_SIZE;
    let end = start + geom.root_dir_sectors as usize * SECTOR_SIZE;
    let live_long = image[start..end]
        .chunks_exact(32)
        .any(|slot| slot[11] == 0x0F && slot[0] != 0x00 && slot[0] != 0xE5);
    assert!(!live_long, "delete left a live orphan long-name entry");
}

#[test]
fn long_directory_names_are_created_and_traversed() {
    let mut fs = mount(build_fat12());
    fs.create_dir("/My Documents/Work Files").unwrap();
    assert!(fs.dir_exists("/My Documents"));
    assert!(fs.dir_exists("/My Documents/Work Files"));
    write_file(&mut fs, "/My Documents/Work Files/report.txt", FileMode::CreateNew, b"data");
    assert_eq!(read_file(&mut fs, "/my documents/work files/report.txt"), b"data");
}


#[cfg(feature = "format")]
mod format_tests {
    use super::*;
    use crate::format::{format, FormatError, FormatOptions};

    /// Formats a fresh device of the given size and returns it mounted.
    fn format_and_mount(sectors: usize, fat_type: FatType) -> FatFs<MemoryBlockDevice> {
        let mut device = MemoryBlockDevice::new(sectors);
        format(&mut device, FormatOptions { fat_type, sectors_per_cluster: 1 }).unwrap();
        FatFs::mount(device).unwrap()
    }

    #[test]
    fn formats_each_fat_type_then_reads_and_writes() {
        for (sectors, fat_type) in [(600, FatType::Fat12), (5000, FatType::Fat16), (70_000, FatType::Fat32)] {
            let mut fs = format_and_mount(sectors, fat_type);
            assert_eq!(fs.geometry().fat_type, fat_type);
            assert!(fs.list_dir("/").unwrap().is_empty());
            write_file(&mut fs, "/notes.txt", FileMode::CreateNew, b"hello");
            fs.create_dir("/My Folder").unwrap();
            write_file(&mut fs, "/My Folder/A Long File.dat", FileMode::CreateNew, &pattern(2000));
            assert_eq!(read_file(&mut fs, "/notes.txt"), b"hello");
            assert_eq!(read_file(&mut fs, "/my folder/a long file.dat"), pattern(2000));
        }
    }

    #[test]
    fn free_space_tracks_what_the_fat_says_rather_than_a_figure_taken_at_mount() {
        for (sectors, fat_type) in
            [(600, FatType::Fat12), (5000, FatType::Fat16), (70_000, FatType::Fat32)]
        {
            let mut fs = format_and_mount(sectors, fat_type);
            let cluster = u64::from(fs.geometry().sectors_per_cluster) * SECTOR_SIZE as u64;
            let total = fs.total_size().unwrap();

            let empty = fs.free_space().unwrap();
            assert!(empty > 0 && empty < total, "{fat_type:?}: {empty} of {total}");
            assert_eq!(empty % cluster, 0, "{fat_type:?}: free space is whole clusters");

            let payload = pattern((cluster * 3 + cluster / 2) as usize);
            write_file(&mut fs, "/big.bin", FileMode::CreateNew, &payload);
            let used = empty - fs.free_space().unwrap();
            assert_eq!(used, cluster * 4, "{fat_type:?}: charged whole clusters");

            fs.delete_file("/big.bin").unwrap();
            assert_eq!(fs.free_space().unwrap(), empty, "{fat_type:?}: delete restores it");
        }
    }

    #[test]
    fn free_space_is_reachable_through_the_backend_seam_a_mount_holds() {
        let mut fs = format_and_mount(5000, FatType::Fat16);
        let inherent = fs.free_space().unwrap();
        let backend: &mut dyn FsBackend = &mut fs;
        assert_eq!(backend.free_space().unwrap(), inherent);
        assert!(backend.free_space().unwrap() < backend.total_size().unwrap());
    }

    #[test]
    fn free_space_survives_a_remount_because_it_is_read_from_the_medium() {
        let mut fs = format_and_mount(5000, FatType::Fat16);
        write_file(&mut fs, "/keep.bin", FileMode::CreateNew, &pattern(3000));
        let before = fs.free_space().unwrap();
        let device = fs.into_device();
        let mut remounted = FatFs::mount(device).unwrap();
        assert_eq!(remounted.free_space().unwrap(), before);
    }

    #[test]
    fn a_formatted_volume_persists_across_a_remount() {
        let mut fs = format_and_mount(5000, FatType::Fat16);
        write_file(&mut fs, "/keep.bin", FileMode::CreateNew, &pattern(1000));
        let device = fs.into_device();
        let mut remounted = FatFs::mount(device).unwrap();
        assert_eq!(read_file(&mut remounted, "/keep.bin"), pattern(1000));
    }

    #[test]
    fn format_writes_spec_grounded_bytes() {
        let mut device = MemoryBlockDevice::new(5000);
        format(&mut device, FormatOptions { fat_type: FatType::Fat16, sectors_per_cluster: 1 }).unwrap();
        let image = device.bytes();
        assert_eq!(&image[510..512], &[0x55, 0xAA]);
        assert_eq!(image[21], 0xF8);
        assert_eq!(image[SECTOR_SIZE], 0xF8);
        assert_eq!(&image[54..62], b"FAT16   ");
    }

    #[test]
    fn format_rejects_an_unsuitable_size_or_bad_options() {
        let mut tiny = MemoryBlockDevice::new(600);
        assert_eq!(
            format(&mut tiny, FormatOptions { fat_type: FatType::Fat32, sectors_per_cluster: 1 }).unwrap_err(),
            FormatError::Unsuitable
        );
        let mut device = MemoryBlockDevice::new(5000);
        assert_eq!(
            format(&mut device, FormatOptions { fat_type: FatType::Fat16, sectors_per_cluster: 3 }).unwrap_err(),
            FormatError::BadOptions
        );
    }

    #[test]
    fn reformat_wipes_and_remounts_in_place() {
        let mut fs = format_and_mount(5000, FatType::Fat16);
        write_file(&mut fs, "/old.txt", FileMode::CreateNew, b"stale");
        assert!(fs.file_exists("/old.txt"));
        assert!(fs.total_sectors().unwrap() >= 5000);

        fs.reformat(FatType::Fat16, 1).unwrap();
        assert_eq!(fs.geometry().fat_type, FatType::Fat16);
        assert!(fs.list_dir("/").unwrap().is_empty());
        assert!(!fs.file_exists("/old.txt"));
        write_file(&mut fs, "/new.txt", FileMode::CreateNew, b"fresh");
        assert_eq!(read_file(&mut fs, "/new.txt"), b"fresh");

        assert!(fs.reformat(FatType::Fat32, 1).is_err());
        assert_eq!(read_file(&mut fs, "/new.txt"), b"fresh");
    }

    #[test]
    fn seam_total_size_and_string_reformat() {
        let mut fs = format_and_mount(5000, FatType::Fat16);
        assert_eq!(
            FsBackend::total_size(&mut fs).unwrap(),
            5000 * SECTOR_SIZE as u64
        );
        write_file(&mut fs, "/old.txt", FileMode::CreateNew, b"stale");
        FsBackend::reformat(&mut fs, "FAT16", 1).unwrap();
        assert!(!fs.file_exists("/old.txt"));
        FsBackend::reformat(&mut fs, "FAT", 0).unwrap();
        assert_eq!(fs.geometry().fat_type, FatType::Fat16);
        assert_eq!(
            FsBackend::reformat(&mut fs, "NTFS", 0),
            Err(FsError::InvalidPath)
        );
    }
}

#[test]
fn fat16_mount_and_read() {
    let (image, content) = build_single_file(Target::Fat16);
    let mut fs = mount(image);
    assert_eq!(fs.geometry().fat_type, FatType::Fat16);
    let handle = fs.open("/DATA.BIN", FileMode::Open, FileAccess::Read).unwrap();
    assert_eq!(fs.length(handle).unwrap(), content.len() as i64);
    assert_eq!(read_all(&mut fs, handle, content.len()), content);
}

#[test]
fn fat32_mount_and_read_a_chained_root() {
    let (image, content) = build_single_file(Target::Fat32);
    let mut fs = mount(image);
    assert_eq!(fs.geometry().fat_type, FatType::Fat32);
    let names: Vec<String> = fs.list_dir("/").unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, vec!["DATA.BIN".to_string()]);
    let handle = fs.open("/DATA.BIN", FileMode::Open, FileAccess::Read).unwrap();
    assert_eq!(read_all(&mut fs, handle, content.len()), content);
}

#[test]
fn builder_writes_spec_grounded_bytes() {
    let fat12 = build_fat12();
    assert_eq!(&fat12[510..512], &[0x55, 0xAA]);
    assert_eq!(fat12[SECTOR_SIZE], 0xF8);
    assert_eq!(fat12[SECTOR_SIZE + 1] & 0x0F, 0x0F);

    let (fat32, _) = build_single_file(Target::Fat32);
    assert_eq!(&fat32[510..512], &[0x55, 0xAA]);
    assert_eq!(&fat32[82..90], b"FAT32   ");
    assert_eq!(fat32[32 * SECTOR_SIZE], 0xF8);
}

#[test]
fn a_sized_file_with_no_first_cluster_is_corrupt_not_empty() {
    let mut builder = ImageBuilder::new(Target::Fat12);
    builder.write_root(&[dir_entry(b"BROKEN  TXT", ATTR_ARCHIVE, 0, 100)]);
    let mut fs = mount(builder.finish());
    let handle = fs.open("/BROKEN.TXT", FileMode::Open, FileAccess::Read).unwrap();
    assert_eq!(fs.length(handle).unwrap(), 100);
    assert_eq!(fs.read(handle, &mut [0u8; 16]).unwrap_err(), FsError::Io);
}
