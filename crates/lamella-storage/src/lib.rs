//! Storage composition glue: a memory card mounted as a file system for the interpreter's fs seam.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

extern crate alloc;

/// Reaching a FAT volume that sits inside an MBR partition, as a real removable card carries it.
pub mod partition;

use lamella_cil_runtime::block::{BlockDevice, SECTOR_SIZE};
use lamella_cil_runtime::fs::BoxedFsBackend;
use lamella_fs_fat::{FatFs, MountError};
use lamella_sd_spi::card::{InitError, SdCard};
use lamella_sd_spi::SdSpiBus;
use partition::PartitionBlockDevice;

/// Re-exported so a consumer naming a mount's FAT width (see [`SdFatMount::fat_type`]) does not need
/// to depend on `lamella-fs-fat` directly.
pub use lamella_fs_fat::FatType;

/// Why mounting a card as a file system failed: bringing the card up over its bus, or finding a
/// mountable FAT volume on it once it is up. The two stages fail for different reasons -- a missing
/// or unresponsive card versus a card holding no (or an unrecognized) file system -- so they are
/// kept distinct rather than flattened to one error.
#[derive(Debug)]
pub enum MountStorageError<E> {
    /// The card could not be identified or brought up over the bus (no card, a protocol fault, a
    /// timeout). Carries the driver's [`InitError`].
    Card(InitError<E>),
    /// The card came up but does not hold a mountable FAT volume. Carries the FAT layer's
    /// [`MountError`].
    Volume(MountError),
}

/// Brings up the memory card on `bus`, mounts the FAT volume on it, and returns it as a boxed
/// [`FsBackend`](lamella_cil_runtime::fs::FsBackend) ready to install in the interpreter's fs seam.
///
/// The card must already hold a FAT file system; this mounts it, it does not format it (formatting
/// is a deliberate, destructive act driven from a higher layer). The returned backend owns the card
/// and its bus, so dropping it releases the medium.
///
/// # Errors
///
/// [`MountStorageError::Card`] if the card cannot be brought up on the bus;
/// [`MountStorageError::Volume`] if it comes up but carries no mountable FAT volume.
pub fn mount_sd_fat<B>(bus: B) -> Result<BoxedFsBackend, MountStorageError<B::Error>>
where
    B: SdSpiBus + 'static,
{
    let card = SdCard::init(bus).map_err(MountStorageError::Card)?;
    let volume = FatFs::mount(card).map_err(MountStorageError::Volume)?;
    Ok(alloc::boxed::Box::new(volume))
}

/// A mounted SD/MMC card, plus what was learned mounting it: the boxed backend to install, the FAT
/// width, and whether the volume sat inside an MBR partition or filled the whole medium.
pub struct SdFatMount {
    /// The mounted volume, ready to install in the interpreter's fs seam.
    pub fs: BoxedFsBackend,
    /// The mounted volume's FAT width.
    pub fat_type: FatType,
    /// `true` if the volume was found inside an MBR partition; `false` for a whole-medium volume.
    pub partitioned: bool,
}

/// Brings the card up and mounts its FAT volume WHEREVER it lives -- inside the first FAT partition
/// of an MBR (a real removable card) or filling the whole medium (a freshly-`mkfs`'d superfloppy).
///
/// This is the entry point for a card of unknown provenance: it reads sector 0, mounts the first FAT
/// partition if that sector is a partition table, and otherwise mounts sector 0 as the volume's own
/// boot sector. It reports the FAT width and which layout it found alongside the backend.
///
/// # Errors
///
/// [`MountStorageError::Card`] if the card cannot be brought up on the bus;
/// [`MountStorageError::Volume`] if sector 0 cannot be read, or the located volume is not a mountable
/// FAT file system.
pub fn mount_sd_fat_auto<B>(bus: B) -> Result<SdFatMount, MountStorageError<B::Error>>
where
    B: SdSpiBus + 'static,
{
    let mut card = SdCard::init(bus).map_err(MountStorageError::Card)?;
    let mut sector0 = [0u8; SECTOR_SIZE];
    card.read_sectors(0, &mut sector0)
        .map_err(|error| MountStorageError::Volume(MountError::Device(error)))?;
    if let Some(part) = partition::first_fat_partition(&sector0) {
        let window =
            PartitionBlockDevice::new(card, u64::from(part.start_lba), u64::from(part.sector_count));
        let volume = FatFs::mount(window).map_err(MountStorageError::Volume)?;
        let fat_type = volume.geometry().fat_type;
        Ok(SdFatMount { fs: alloc::boxed::Box::new(volume), fat_type, partitioned: true })
    } else {
        let volume = FatFs::mount(card).map_err(MountStorageError::Volume)?;
        let fat_type = volume.geometry().fat_type;
        Ok(SdFatMount { fs: alloc::boxed::Box::new(volume), fat_type, partitioned: false })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use alloc::vec;
    use alloc::vec::Vec;
    use lamella_cil_runtime::fs::{FileAccess, FileMode, FsBackend};
    use lamella_fs_fat::format::{format, FormatOptions};
    use lamella_fs_fat::FatType;
    use lamella_sd_spi::card::CardType;
    use lamella_sd_spi::sim::SimCard;

    /// A position-sensitive byte pattern, so a mis-addressed sector reads back as wrong values
    /// rather than as a plausible fill. `seed` distinguishes files that share a length.
    fn pattern(len: usize, seed: u8) -> Vec<u8> {
        (0..len).map(|i| (((i * 31 + 7) & 0xFF) as u8) ^ seed).collect()
    }

    /// Creates a file and writes all of `data`, looping until it is fully written.
    fn write_file(fs: &mut dyn FsBackend, path: &str, data: &[u8]) {
        let handle = fs.open(path, FileMode::CreateNew, FileAccess::Write).unwrap();
        let mut offset = 0;
        while offset < data.len() {
            let n = fs.write(handle, &data[offset..]).unwrap();
            assert!(n > 0, "write made no progress");
            offset += n;
        }
        fs.close(handle);
    }

    /// Opens `path`, reads it whole, and closes it.
    fn read_file(fs: &mut dyn FsBackend, path: &str) -> Vec<u8> {
        let handle = fs.open(path, FileMode::Open, FileAccess::Read).unwrap();
        let len = fs.length(handle).unwrap() as usize;
        let mut buf = vec![0u8; len];
        let mut done = 0;
        while done < len {
            let n = fs.read(handle, &mut buf[done..]).unwrap();
            if n == 0 {
                break;
            }
            done += n;
        }
        buf.truncate(done);
        fs.close(handle);
        buf
    }

    /// Formats a fresh simulated card of `kind` THROUGH the driver, mounts FAT over it, writes a
    /// small tree, then unmounts and remounts over the SAME medium via a fresh driver init and reads
    /// everything back. The remount is the load-bearing step: it proves the bytes reached the card's
    /// sectors through the SD write path and read back through the SD read path, at the LBAs FAT
    /// computed -- so a wrong `block_arg` (byte-offset vs sector-index) for this card family would
    /// surface as corrupt data, not pass.
    fn format_write_remount_read(kind: CardType, store_sectors: u64, fat_type: FatType) {
        let mut card = SdCard::init(SimCard::with_capacity(kind, store_sectors)).unwrap();
        assert_eq!(card.card_type(), kind, "the simulated card identifies as its family");
        format(&mut card, FormatOptions { fat_type, sectors_per_cluster: 1 }).unwrap();
        let mut fs = FatFs::mount(card).unwrap();
        assert_eq!(fs.geometry().fat_type, fat_type);

        let notes = b"the quick brown fox jumps".to_vec();
        let data = pattern(3000, 0x5A);
        let deep = pattern(600, 0x11);
        let report = pattern(1500, 0xC3);
        write_file(&mut fs, "/notes.txt", &notes);
        write_file(&mut fs, "/data.bin", &data);
        fs.create_dir("/logs").unwrap();
        write_file(&mut fs, "/logs/day1.log", &deep);
        write_file(&mut fs, "/My Report.txt", &report);

        let sim = fs.into_device().release_bus();

        let mut fs = FatFs::mount(SdCard::init(sim).unwrap()).unwrap();
        assert_eq!(read_file(&mut fs, "/notes.txt"), notes);
        assert_eq!(read_file(&mut fs, "/data.bin"), data);
        assert_eq!(read_file(&mut fs, "/logs/day1.log"), deep);
        assert_eq!(read_file(&mut fs, "/My Report.txt"), report);
        assert_eq!(read_file(&mut fs, "/my report.txt"), report);
        let mut root: Vec<(alloc::string::String, bool)> = fs
            .list_dir("/")
            .unwrap()
            .into_iter()
            .map(|entry| (entry.name, entry.is_dir))
            .collect();
        root.sort();
        assert_eq!(
            root,
            vec![
                ("My Report.txt".to_string(), false),
                ("data.bin".to_string(), false),
                ("logs".to_string(), true),
                ("notes.txt".to_string(), false),
            ]
        );
        assert!(!fs.file_exists("/nope.txt"));
    }

    #[test]
    fn fat16_over_a_block_addressed_sdhc_card() {
        format_write_remount_read(CardType::Sdhc, 8192, FatType::Fat16);
    }

    #[test]
    fn fat12_over_a_byte_addressed_sd_card() {
        format_write_remount_read(CardType::Sd2, 2048, FatType::Fat12);
    }

    #[test]
    fn fat12_over_an_mmc_card() {
        format_write_remount_read(CardType::Mmc, 2048, FatType::Fat12);
    }

    #[test]
    fn mount_sd_fat_produces_a_working_backend() {
        let mut card = SdCard::init(SimCard::with_capacity(CardType::Sdhc, 8192)).unwrap();
        format(&mut card, FormatOptions { fat_type: FatType::Fat16, sectors_per_cluster: 1 }).unwrap();
        let sim = card.release_bus();

        let mut backend = mount_sd_fat(sim).expect("a formatted card mounts");
        let payload = pattern(1234, 0x77);
        write_file(&mut *backend, "/glue.bin", &payload);
        assert!(backend.file_exists("/glue.bin"));
        assert_eq!(read_file(&mut *backend, "/glue.bin"), payload);
    }

    #[test]
    fn mount_sd_fat_reports_an_unformatted_card_and_an_empty_slot() {
        let blank = SimCard::with_capacity(CardType::Sdhc, 8192);
        assert!(matches!(mount_sd_fat(blank), Err(MountStorageError::Volume(_))));
        assert!(matches!(mount_sd_fat(SimCard::absent()), Err(MountStorageError::Card(_))));
    }

    #[test]
    fn mount_sd_fat_auto_reads_a_file_from_an_mbr_partition() {
        let store_sectors = 32_768u64;
        let part_start = 2_048u64;
        let part_sectors = store_sectors - part_start;

        let mut card = SdCard::init(SimCard::with_capacity(CardType::Sdhc, store_sectors)).unwrap();
        let mut mbr = [0u8; SECTOR_SIZE];
        let base = 446;
        mbr[base] = 0x00;
        mbr[base + 4] = 0x0E;
        mbr[base + 8..base + 12].copy_from_slice(&(part_start as u32).to_le_bytes());
        mbr[base + 12..base + 16].copy_from_slice(&(part_sectors as u32).to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        card.write_sectors(0, &mbr).unwrap();

        let data = pattern(2600, 0x3C);
        {
            let mut window = PartitionBlockDevice::new(card, part_start, part_sectors);
            format(&mut window, FormatOptions { fat_type: FatType::Fat16, sectors_per_cluster: 1 })
                .unwrap();
            let mut fs = FatFs::mount(window).unwrap();
            write_file(&mut fs, "/report.txt", &data);
            card = fs.into_device().into_inner();
        }
        let sim = card.release_bus();

        let mut mount = mount_sd_fat_auto(sim).expect("the partitioned card mounts");
        assert!(mount.partitioned, "the volume was inside an MBR partition");
        assert_eq!(mount.fat_type, FatType::Fat16);
        assert_eq!(read_file(&mut *mount.fs, "/report.txt"), data);
        assert!(!mount.fs.file_exists("/nope.txt"));
    }

    #[test]
    fn mount_sd_fat_auto_reads_from_a_byte_addressed_partition() {
        let store_sectors = 32_768u64;
        let part_start = 2_048u64;
        let part_sectors = store_sectors - part_start;

        let mut card = SdCard::init(SimCard::with_capacity(CardType::Sd2, store_sectors)).unwrap();
        let mut mbr = [0u8; SECTOR_SIZE];
        let base = 446;
        mbr[base + 4] = 0x0E;
        mbr[base + 8..base + 12].copy_from_slice(&(part_start as u32).to_le_bytes());
        mbr[base + 12..base + 16].copy_from_slice(&(part_sectors as u32).to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        card.write_sectors(0, &mbr).unwrap();

        let data = pattern(1800, 0x9E);
        {
            let mut window = PartitionBlockDevice::new(card, part_start, part_sectors);
            format(&mut window, FormatOptions { fat_type: FatType::Fat16, sectors_per_cluster: 1 })
                .unwrap();
            let mut fs = FatFs::mount(window).unwrap();
            write_file(&mut fs, "/data.bin", &data);
            card = fs.into_device().into_inner();
        }
        let sim = card.release_bus();

        let mut mount = mount_sd_fat_auto(sim).expect("the byte-addressed partitioned card mounts");
        assert!(mount.partitioned);
        assert_eq!(mount.fat_type, FatType::Fat16);
        assert_eq!(read_file(&mut *mount.fs, "/data.bin"), data);
        assert!(mount.fs.file_exists("/data.bin"));
    }

    #[test]
    fn a_freshly_formatted_partition_lists_an_empty_root() {
        let mut card = SdCard::init(SimCard::with_capacity(CardType::Sd2, 32_768)).unwrap();
        let mut mbr = [0u8; SECTOR_SIZE];
        mbr[446 + 4] = 0x0E;
        mbr[446 + 8..446 + 12].copy_from_slice(&2_048u32.to_le_bytes());
        mbr[446 + 12..446 + 16].copy_from_slice(&30_720u32.to_le_bytes());
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        card.write_sectors(0, &mbr).unwrap();
        {
            let mut window = PartitionBlockDevice::new(card, 2_048, 30_720);
            format(&mut window, FormatOptions { fat_type: FatType::Fat16, sectors_per_cluster: 1 })
                .unwrap();
            card = window.into_inner();
        }
        let mut mount = mount_sd_fat_auto(card.release_bus()).unwrap();
        assert_eq!(mount.fat_type, FatType::Fat16);
        assert!(mount.partitioned);
        assert!(mount.fs.list_dir("/").unwrap().is_empty());
    }

    #[test]
    fn mount_sd_fat_auto_still_mounts_a_whole_medium_volume() {
        let mut card = SdCard::init(SimCard::with_capacity(CardType::Sd2, 2_048)).unwrap();
        format(&mut card, FormatOptions { fat_type: FatType::Fat12, sectors_per_cluster: 1 }).unwrap();
        let sim = card.release_bus();
        let mut mount = mount_sd_fat_auto(sim).expect("the superfloppy mounts");
        assert!(!mount.partitioned);
        assert_eq!(mount.fat_type, FatType::Fat12);
        write_file(&mut *mount.fs, "/x.txt", b"hi");
        assert_eq!(read_file(&mut *mount.fs, "/x.txt"), b"hi");
    }
}
