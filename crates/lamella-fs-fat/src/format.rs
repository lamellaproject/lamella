//! Formatting: writing a fresh FAT file system onto a block device (mkfs).

use crate::block::{BlockDevice, BlockError, SECTOR_SIZE};
use crate::FatType;

/// How to lay out the new volume.
pub struct FormatOptions {
    /// Which FAT width to write. The device must be large enough that its usable cluster count lands
    /// in this width's band (else [`FormatError::Unsuitable`]).
    pub fat_type: FatType,
    /// Sectors per cluster: a power of two in 1..=128. Larger clusters shrink the FAT and raise the
    /// minimum volume size for a given FAT type.
    pub sectors_per_cluster: u8,
}

/// Why a format failed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FormatError {
    /// `sectors_per_cluster` is not a power of two in 1..=128.
    BadOptions,
    /// The device is too small to hold even the requested layout's metadata.
    TooSmall,
    /// The device's usable cluster count does not fall in the requested FAT type's band.
    Unsuitable,
    /// The underlying block device failed.
    Device(BlockError),
}

impl From<BlockError> for FormatError {
    fn from(error: BlockError) -> FormatError {
        FormatError::Device(error)
    }
}

/// Writes a fresh FAT file system of `options.fat_type` onto `device`, using its whole capacity.
/// After it returns, `device` mounts as an empty volume.
pub fn format<D: BlockDevice>(device: &mut D, options: FormatOptions) -> Result<(), FormatError> {
    let spc = options.sectors_per_cluster;
    if spc == 0 || spc > 128 || !spc.is_power_of_two() {
        return Err(FormatError::BadOptions);
    }
    let total = u32::try_from(device.sector_count()?).map_err(|_| FormatError::Unsuitable)?;
    let fat32 = options.fat_type == FatType::Fat32;
    let reserved: u32 = if fat32 { 32 } else { 1 };
    let num_fats: u32 = 2;
    let root_entries: u32 = if fat32 { 0 } else { 512 };
    let root_dir_sectors = (root_entries * 32 + (SECTOR_SIZE as u32 - 1)) / SECTOR_SIZE as u32;

    let tmpval1 = total
        .checked_sub(reserved + root_dir_sectors)
        .ok_or(FormatError::TooSmall)?;
    let mut tmpval2 = 256 * u32::from(spc) + num_fats;
    if fat32 {
        tmpval2 /= 2;
    }
    let fat_size = (tmpval1 + (tmpval2 - 1)) / tmpval2;
    if fat_size == 0 {
        return Err(FormatError::TooSmall);
    }

    let first_data_sector = reserved + num_fats * fat_size + root_dir_sectors;
    let data_sectors = total
        .checked_sub(first_data_sector)
        .ok_or(FormatError::TooSmall)?;
    let count = data_sectors / u32::from(spc);
    let in_band = match options.fat_type {
        FatType::Fat12 => count < 4085,
        FatType::Fat16 => (4085..65525).contains(&count),
        FatType::Fat32 => count >= 65525,
    };
    if count == 0 || !in_band {
        return Err(FormatError::Unsuitable);
    }

    let media = 0xF8u8;
    let zero = [0u8; SECTOR_SIZE];

    for lba in 1..reserved {
        device.write_sectors(u64::from(lba), &zero)?;
    }
    for i in 0..(num_fats * fat_size) {
        device.write_sectors(u64::from(reserved + i), &zero)?;
    }
    if fat32 {
        for i in 0..u32::from(spc) {
            device.write_sectors(u64::from(first_data_sector + i), &zero)?;
        }
    } else {
        let root_start = reserved + num_fats * fat_size;
        for i in 0..root_dir_sectors {
            device.write_sectors(u64::from(root_start + i), &zero)?;
        }
    }

    let mut fat_head = [0u8; SECTOR_SIZE];
    match options.fat_type {
        FatType::Fat12 => {
            let v0 = 0x0F00u16 | u16::from(media);
            let v1 = 0x0FFFu16;
            fat_head[0] = (v0 & 0xFF) as u8;
            fat_head[1] = (((v0 >> 8) & 0x0F) as u8) | (((v1 & 0x0F) << 4) as u8);
            fat_head[2] = (v1 >> 4) as u8;
        }
        FatType::Fat16 => {
            fat_head[0..2].copy_from_slice(&(0xFF00u16 | u16::from(media)).to_le_bytes());
            fat_head[2..4].copy_from_slice(&0xFFFFu16.to_le_bytes());
        }
        FatType::Fat32 => {
            fat_head[0..4].copy_from_slice(&(0x0FFF_FF00u32 | u32::from(media)).to_le_bytes());
            fat_head[4..8].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
            fat_head[8..12].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
        }
    }
    for copy in 0..num_fats {
        device.write_sectors(u64::from(reserved + copy * fat_size), &fat_head)?;
    }

    if fat32 {
        let mut fsinfo = [0u8; SECTOR_SIZE];
        fsinfo[0..4].copy_from_slice(&0x4161_5252u32.to_le_bytes());
        fsinfo[484..488].copy_from_slice(&0x6141_7272u32.to_le_bytes());
        fsinfo[488..492].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        fsinfo[492..496].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        fsinfo[508..512].copy_from_slice(&0xAA55_0000u32.to_le_bytes());
        device.write_sectors(1, &fsinfo)?;
    }

    let mut boot = [0u8; SECTOR_SIZE];
    build_boot_sector(
        &mut boot,
        options.fat_type,
        total,
        spc,
        reserved,
        num_fats as u8,
        root_entries,
        fat_size,
        media,
    );
    device.write_sectors(0, &boot)?;
    if fat32 {
        device.write_sectors(6, &boot)?;
    }

    device.flush()?;
    Ok(())
}

/// Fills a zeroed sector with the BPB for the chosen layout (the boot code region stays zero -- a
/// data volume is not made bootable).
#[allow(clippy::too_many_arguments)]
fn build_boot_sector(
    boot: &mut [u8; SECTOR_SIZE],
    fat_type: FatType,
    total: u32,
    spc: u8,
    reserved: u32,
    num_fats: u8,
    root_entries: u32,
    fat_size: u32,
    media: u8,
) {
    boot[0] = 0xEB;
    boot[1] = 0x3C;
    boot[2] = 0x90;
    boot[3..11].copy_from_slice(b"LAMELLA ");
    boot[11..13].copy_from_slice(&(SECTOR_SIZE as u16).to_le_bytes());
    boot[13] = spc;
    boot[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
    boot[16] = num_fats;
    boot[17..19].copy_from_slice(&(root_entries as u16).to_le_bytes());
    boot[21] = media;
    boot[24..26].copy_from_slice(&63u16.to_le_bytes());
    boot[26..28].copy_from_slice(&255u16.to_le_bytes());
    let volume_id = 0x4C4D_4C31u32;
    match fat_type {
        FatType::Fat32 => {
            boot[32..36].copy_from_slice(&total.to_le_bytes());
            boot[36..40].copy_from_slice(&fat_size.to_le_bytes());
            boot[44..48].copy_from_slice(&2u32.to_le_bytes());
            boot[48..50].copy_from_slice(&1u16.to_le_bytes());
            boot[50..52].copy_from_slice(&6u16.to_le_bytes());
            boot[64] = 0x80;
            boot[66] = 0x29;
            boot[67..71].copy_from_slice(&volume_id.to_le_bytes());
            boot[71..82].copy_from_slice(b"NO NAME    ");
            boot[82..90].copy_from_slice(b"FAT32   ");
        }
        _ => {
            if total <= u32::from(u16::MAX) {
                boot[19..21].copy_from_slice(&(total as u16).to_le_bytes());
            } else {
                boot[32..36].copy_from_slice(&total.to_le_bytes());
            }
            boot[22..24].copy_from_slice(&(fat_size as u16).to_le_bytes());
            boot[36] = 0x80;
            boot[38] = 0x29;
            boot[39..43].copy_from_slice(&volume_id.to_le_bytes());
            boot[43..54].copy_from_slice(b"NO NAME    ");
            let label: &[u8; 8] = if fat_type == FatType::Fat12 {
                b"FAT12   "
            } else {
                b"FAT16   "
            };
            boot[54..62].copy_from_slice(label);
        }
    }
    boot[510] = 0x55;
    boot[511] = 0xAA;
}
