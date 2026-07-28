//! The FAT boot sector (BPB) and the volume geometry derived from it.

use crate::block::{BlockError, SECTOR_SIZE};
use crate::{u16le, u32le};

/// Which FAT width a mounted volume uses. Selected by the number of data clusters, never by any
/// field on the medium -- the FAT type is a DERIVED property, and reading it off a label instead
/// is the classic way to mis-mount a volume.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FatType {
    /// 12-bit FAT entries (small volumes: fewer than 4085 clusters).
    Fat12,
    /// 16-bit FAT entries (4085 .. 65525 clusters).
    Fat16,
    /// 32-bit FAT entries (65525 or more clusters); the root directory is a cluster chain.
    Fat32,
}

/// Why a volume could not be mounted. Distinct from [`crate::fs::FsError`] because a mount
/// failure is not a per-file error: it is reported once, when the backend is installed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MountError {
    /// Sector 0 did not end in the `0xAA55` boot signature -- not a FAT volume (or an unformatted
    /// medium).
    BadBootSignature,
    /// `BPB_BytsPerSec` is not 512. The [`crate::block::BlockDevice`] seam presents 512-byte
    /// sectors, so a volume formatted with a larger logical sector is not mountable here -- a
    /// medium with a different physical block adapts beneath the block seam, but the FILE SYSTEM's
    /// unit is fixed. Carries the offending value.
    UnsupportedSectorSize(u16),
    /// A BPB field is structurally impossible (zero sectors-per-cluster, zero FATs, a data region
    /// that runs off the end of the medium): the volume is corrupt or is not FAT.
    BadBpb,
    /// The underlying block device failed while reading the boot sector.
    Device(BlockError),
}

/// The geometry of a mounted volume: everything the FAT and directory layers need, computed once
/// at mount so no hot path re-derives it. All sector figures are absolute LBAs unless named
/// otherwise.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Geometry {
    /// Which FAT width, derived from [`Geometry::count_of_clusters`].
    pub fat_type: FatType,
    /// Sectors per cluster (a power of two, 1..=128). The cluster is the allocation unit.
    pub sectors_per_cluster: u8,
    /// Reserved sectors before the first FAT (at least 1; the boot sector lives in this region).
    /// Equals the LBA of the first FAT.
    pub reserved_sectors: u16,
    /// How many FATs the volume keeps (normally 2 -- a primary and a mirror).
    pub num_fats: u8,
    /// The length of ONE FAT in sectors.
    pub fat_size_sectors: u32,
    /// The fixed root-directory entry count (FAT12/16). Zero on FAT32, whose root is a chain.
    pub root_entry_count: u16,
    /// Total sectors in the volume.
    pub total_sectors: u32,
    /// Sectors occupied by the fixed root directory (FAT12/16); zero on FAT32.
    pub root_dir_sectors: u32,
    /// LBA of the first data sector (cluster 2 begins here).
    pub first_data_sector: u32,
    /// The number of DATA clusters. Valid cluster numbers are `2 ..= count_of_clusters + 1`.
    pub count_of_clusters: u32,
    /// FAT32 root directory's first cluster; unused (0) on FAT12/16.
    pub root_cluster: u32,
    /// FAT32 FSInfo sector LBA; unused (0) on FAT12/16.
    pub fsinfo_sector: u16,
    /// The BPB media byte, echoed in the low byte of `FAT[0]`.
    pub media: u8,
}

impl Geometry {
    /// LBA of the first FAT (the reserved region ends here).
    #[must_use]
    pub fn fat_start_sector(&self) -> u32 {
        u32::from(self.reserved_sectors)
    }

    /// LBA of the fixed root directory region (FAT12/16 only; meaningless on FAT32).
    #[must_use]
    pub fn root_dir_start_sector(&self) -> u32 {
        u32::from(self.reserved_sectors) + u32::from(self.num_fats) * self.fat_size_sectors
    }

    /// LBA of the first sector of cluster `n` (spec: `((n - 2) * SecPerClus) + FirstDataSector`).
    /// `n` must be a valid data cluster (`>= 2`); callers gate on [`Geometry::is_valid_cluster`].
    #[must_use]
    pub fn cluster_start_sector(&self, n: u32) -> u32 {
        (n - 2) * u32::from(self.sectors_per_cluster) + self.first_data_sector
    }

    /// Whether `n` names an actual data cluster (`2 ..= count_of_clusters + 1`). A cluster field
    /// outside this range is a bad or terminal marker, never a location to read.
    #[must_use]
    pub fn is_valid_cluster(&self, n: u32) -> bool {
        n >= 2 && n <= self.count_of_clusters + 1
    }

    /// Bytes per cluster: the allocation unit in bytes.
    #[must_use]
    pub fn bytes_per_cluster(&self) -> u32 {
        u32::from(self.sectors_per_cluster) * SECTOR_SIZE as u32
    }
}

/// Parses and validates the boot sector, returning the volume geometry. `boot` must be at least
/// one sector; only the first [`SECTOR_SIZE`] bytes are read.
///
/// The FAT type is DERIVED from the cluster count per the Microsoft rule, not read from the
/// `BS_FilSysType` label (which the spec explicitly says is informational and must not be trusted
/// for type detection).
pub fn parse(boot: &[u8]) -> Result<Geometry, MountError> {
    if boot.len() < SECTOR_SIZE {
        return Err(MountError::BadBpb);
    }

    if boot[510] != 0x55 || boot[511] != 0xAA {
        return Err(MountError::BadBootSignature);
    }

    let bytes_per_sector = u16le(boot, 11);
    if bytes_per_sector as usize != SECTOR_SIZE {
        return Err(MountError::UnsupportedSectorSize(bytes_per_sector));
    }

    let sectors_per_cluster = boot[13];
    if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
        return Err(MountError::BadBpb);
    }

    let reserved_sectors = u16le(boot, 14);
    if reserved_sectors == 0 {
        return Err(MountError::BadBpb);
    }

    let num_fats = boot[16];
    if num_fats == 0 {
        return Err(MountError::BadBpb);
    }

    let root_entry_count = u16le(boot, 17);
    let media = boot[21];

    let total_sectors_16 = u16le(boot, 19);
    let total_sectors_32 = u32le(boot, 32);
    let total_sectors = if total_sectors_16 != 0 {
        u32::from(total_sectors_16)
    } else {
        total_sectors_32
    };

    let fat_size_16 = u16le(boot, 22);
    let fat_size_32 = u32le(boot, 36);
    let fat_size_sectors = if fat_size_16 != 0 {
        u32::from(fat_size_16)
    } else {
        fat_size_32
    };
    if total_sectors == 0 || fat_size_sectors == 0 {
        return Err(MountError::BadBpb);
    }

    let bytes_per_sector_u32 = u32::from(bytes_per_sector);
    let root_dir_sectors =
        (u32::from(root_entry_count) * 32 + (bytes_per_sector_u32 - 1)) / bytes_per_sector_u32;
    let meta_sectors = u32::from(reserved_sectors)
        + u32::from(num_fats) * fat_size_sectors
        + root_dir_sectors;
    let data_sectors = total_sectors
        .checked_sub(meta_sectors)
        .ok_or(MountError::BadBpb)?;
    let count_of_clusters = data_sectors / u32::from(sectors_per_cluster);

    let fat_type = if count_of_clusters < 4085 {
        FatType::Fat12
    } else if count_of_clusters < 65525 {
        FatType::Fat16
    } else {
        FatType::Fat32
    };

    let (root_cluster, fsinfo_sector) = match fat_type {
        FatType::Fat32 => (u32le(boot, 44), u16le(boot, 48)),
        FatType::Fat12 | FatType::Fat16 => (0, 0),
    };

    Ok(Geometry {
        fat_type,
        sectors_per_cluster,
        reserved_sectors,
        num_fats,
        fat_size_sectors,
        root_entry_count,
        total_sectors,
        root_dir_sectors,
        first_data_sector: meta_sectors,
        count_of_clusters,
        root_cluster,
        fsinfo_sector,
        media,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Builds a 512-byte boot sector from field values placed at their spec offsets. Authored from
    /// the documented layout, NOT from [`parse`], so a parse test grounded on it is grounded on
    /// the spec rather than on the reader's own behavior.
    fn boot_sector(
        bytes_per_sector: u16,
        sectors_per_cluster: u8,
        reserved: u16,
        num_fats: u8,
        root_entries: u16,
        total_16: u16,
        fat_size_16: u16,
        total_32: u32,
        fat_size_32: u32,
        root_cluster: u32,
    ) -> vec::Vec<u8> {
        let mut b = vec![0u8; SECTOR_SIZE];
        b[0] = 0xEB;
        b[1] = 0x3C;
        b[2] = 0x90;
        b[11..13].copy_from_slice(&bytes_per_sector.to_le_bytes());
        b[13] = sectors_per_cluster;
        b[14..16].copy_from_slice(&reserved.to_le_bytes());
        b[16] = num_fats;
        b[17..19].copy_from_slice(&root_entries.to_le_bytes());
        b[19..21].copy_from_slice(&total_16.to_le_bytes());
        b[21] = 0xF8;
        b[22..24].copy_from_slice(&fat_size_16.to_le_bytes());
        b[32..36].copy_from_slice(&total_32.to_le_bytes());
        b[36..40].copy_from_slice(&fat_size_32.to_le_bytes());
        b[44..48].copy_from_slice(&root_cluster.to_le_bytes());
        b[510] = 0x55;
        b[511] = 0xAA;
        b
    }

    #[test]
    fn rejects_a_sector_without_the_boot_signature() {
        let mut b = boot_sector(512, 1, 1, 2, 512, 0, 16, 8192, 0, 0);
        b[511] = 0x00;
        assert_eq!(parse(&b), Err(MountError::BadBootSignature));
    }

    #[test]
    fn rejects_a_non_512_sector_size() {
        let b = boot_sector(1024, 1, 1, 2, 512, 0, 16, 8192, 0, 0);
        assert_eq!(parse(&b), Err(MountError::UnsupportedSectorSize(1024)));
    }

    #[test]
    fn rejects_structural_impossibilities() {
        assert_eq!(parse(&boot_sector(512, 0, 1, 2, 512, 8192, 16, 0, 0, 0)), Err(MountError::BadBpb));
        assert_eq!(parse(&boot_sector(512, 3, 1, 2, 512, 8192, 16, 0, 0, 0)), Err(MountError::BadBpb));
        assert_eq!(parse(&boot_sector(512, 1, 1, 0, 512, 8192, 16, 0, 0, 0)), Err(MountError::BadBpb));
        assert_eq!(parse(&boot_sector(512, 1, 1, 2, 512, 4, 16, 0, 0, 0)), Err(MountError::BadBpb));
    }

    #[test]
    fn computes_fat16_geometry() {
        let g = parse(&boot_sector(512, 1, 1, 2, 512, 8192, 16, 0, 0, 0)).unwrap();
        assert_eq!(g.fat_type, FatType::Fat16);
        assert_eq!(g.root_dir_sectors, 32);
        assert_eq!(g.fat_start_sector(), 1);
        assert_eq!(g.root_dir_start_sector(), 1 + 2 * 16);
        assert_eq!(g.first_data_sector, 65);
        assert_eq!(g.count_of_clusters, 8127);
        assert_eq!(g.cluster_start_sector(2), 65);
        assert_eq!(g.cluster_start_sector(3), 66);
        assert!(g.is_valid_cluster(2));
        assert!(g.is_valid_cluster(8128));
        assert!(!g.is_valid_cluster(1));
        assert!(!g.is_valid_cluster(8129));
    }

    #[test]
    fn computes_fat32_geometry_with_a_chained_root() {
        let b = boot_sector(512, 1, 32, 2, 0, 0, 0, 131072, 512, 2);
        let g = parse(&b).unwrap();
        assert_eq!(g.fat_type, FatType::Fat32);
        assert_eq!(g.root_dir_sectors, 0);
        assert_eq!(g.root_cluster, 2);
        assert_eq!(g.first_data_sector, 1056);
        assert_eq!(g.count_of_clusters, 130016);
    }

    #[test]
    fn type_thresholds_follow_the_spec_off_by_one() {
        let count_to = |count: u32| boot_sector(512, 1, 1, 1, 0, 0, 1, count + 2, 0, 0);
        assert_eq!(parse(&count_to(4084)).unwrap().fat_type, FatType::Fat12);
        assert_eq!(parse(&count_to(4085)).unwrap().fat_type, FatType::Fat16);
        assert_eq!(parse(&count_to(65524)).unwrap().fat_type, FatType::Fat16);
        assert_eq!(parse(&count_to(65525)).unwrap().fat_type, FatType::Fat32);
    }
}
