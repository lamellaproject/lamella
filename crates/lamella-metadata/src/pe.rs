//! The PE/COFF wrapper around metadata (ECMA-335 1st ed, II.25.2-25.3).

use crate::bytes::{ReadError, Reader};
use alloc::vec::Vec;

/// An error parsing the PE structure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeError {
    /// A header read ran past the end of the image.
    Truncated,
    /// The image did not start with the `MZ` DOS signature.
    BadDosSignature,
    /// The PE signature `PE\0\0` was not found where the DOS header pointed.
    BadPeSignature,
    /// The optional header magic was neither PE32 (`0x10B`) nor PE32+ (`0x20B`).
    BadOptionalMagic,
    /// Data directory 14 (the CLI header) was empty: not a managed image.
    NoCliHeader,
    /// An RVA fell outside every section.
    UnmappedRva,
}

impl From<ReadError> for PeError {
    fn from(_: ReadError) -> PeError {
        PeError::Truncated
    }
}

/// One section's address mapping: where it lives in virtual space and on disk.
#[derive(Debug, Clone, Copy)]
struct Section {
    virtual_address: u32,
    virtual_size: u32,
    raw_pointer: u32,
    raw_size: u32,
}

/// A parsed PE image, with the CLI header location and a section map.
#[derive(Debug, Clone)]
pub struct PeImage<'a> {
    data: &'a [u8],
    sections: Vec<Section>,
    cli_header_rva: u32,
    cli_header_size: u32,
}

impl<'a> PeImage<'a> {
    /// Parses the PE headers of `data`, far enough to locate the CLI header and
    /// resolve RVAs.
    pub fn parse(data: &'a [u8]) -> Result<PeImage<'a>, PeError> {
        if data.get(0..2) != Some(b"MZ") {
            return Err(PeError::BadDosSignature);
        }
        let mut reader = Reader::new(data);
        let pe_offset = reader.u32_at(0x3C)? as usize;
        reader.seek(pe_offset)?;
        if reader.read_bytes(4)? != b"PE\0\0" {
            return Err(PeError::BadPeSignature);
        }
        let _machine = reader.read_u16()?;
        let number_of_sections = reader.read_u16()?;
        reader.skip(12)?;
        let size_of_optional_header = reader.read_u16()? as usize;
        let _characteristics = reader.read_u16()?;
        let optional_start = reader.position();
        let magic = reader.read_u16()?;
        let directories_offset = match magic {
            0x10B => 96,
            0x20B => 112,
            _ => return Err(PeError::BadOptionalMagic),
        };
        let cli_directory = optional_start + directories_offset + 14 * 8;
        let cli_header_rva = reader.u32_at(cli_directory)?;
        let cli_header_size = reader.u32_at(cli_directory + 4)?;
        if cli_header_rva == 0 {
            return Err(PeError::NoCliHeader);
        }
        reader.seek(optional_start + size_of_optional_header)?;
        let mut sections = Vec::new();
        for _ in 0..number_of_sections {
            reader.skip(8)?;
            let virtual_size = reader.read_u32()?;
            let virtual_address = reader.read_u32()?;
            let raw_size = reader.read_u32()?;
            let raw_pointer = reader.read_u32()?;
            reader.skip(16)?;
            sections.push(Section {
                virtual_address,
                virtual_size,
                raw_pointer,
                raw_size,
            });
        }
        Ok(PeImage {
            data,
            sections,
            cli_header_rva,
            cli_header_size,
        })
    }

    /// The RVA of the CLI header (II.25.3.3).
    #[must_use]
    pub fn cli_header_rva(&self) -> u32 {
        self.cli_header_rva
    }

    /// The size of the CLI header.
    #[must_use]
    pub fn cli_header_size(&self) -> u32 {
        self.cli_header_size
    }

    /// Maps a relative virtual address to a file offset using the section map.
    pub fn rva_to_offset(&self, rva: u32) -> Result<usize, PeError> {
        for section in &self.sections {
            let span = section.virtual_size.max(section.raw_size);
            if rva >= section.virtual_address && rva < section.virtual_address + span {
                let delta = rva - section.virtual_address;
                if delta < section.raw_size {
                    return Ok((section.raw_pointer + delta) as usize);
                }
            }
        }
        Err(PeError::UnmappedRva)
    }

    /// The `length` bytes of file data backing an RVA.
    pub fn slice_at_rva(&self, rva: u32, length: usize) -> Result<&'a [u8], PeError> {
        let offset = self.rva_to_offset(rva)?;
        let end = offset.checked_add(length).ok_or(PeError::Truncated)?;
        self.data.get(offset..end).ok_or(PeError::Truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but well-formed PE32 image: one `.text` section at RVA 0x2000 /
    /// file 0x400, and a CLI header directory pointing at RVA 0x2000.
    fn synthetic_pe() -> Vec<u8> {
        let mut image = vec![0u8; 0x600];
        image[0] = b'M';
        image[1] = b'Z';
        let pe = 0x80usize;
        image[0x3C..0x40].copy_from_slice(&(pe as u32).to_le_bytes());
        image[pe..pe + 4].copy_from_slice(b"PE\0\0");
        let coff = pe + 4;
        image[coff..coff + 2].copy_from_slice(&0x014Cu16.to_le_bytes());
        image[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes());
        let optional_size: u16 = 224;
        image[coff + 16..coff + 18].copy_from_slice(&optional_size.to_le_bytes());
        let optional = coff + 20;
        image[optional..optional + 2].copy_from_slice(&0x010Bu16.to_le_bytes());
        let cli = optional + 96 + 14 * 8;
        image[cli..cli + 4].copy_from_slice(&0x2000u32.to_le_bytes());
        image[cli + 4..cli + 8].copy_from_slice(&0x48u32.to_le_bytes());
        let section = optional + optional_size as usize;
        image[section..section + 8].copy_from_slice(b".text\0\0\0");
        image[section + 8..section + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        image[section + 12..section + 16].copy_from_slice(&0x2000u32.to_le_bytes());
        image[section + 16..section + 20].copy_from_slice(&0x200u32.to_le_bytes());
        image[section + 20..section + 24].copy_from_slice(&0x400u32.to_le_bytes());
        image[0x400] = 0xCA;
        image
    }

    #[test]
    fn parses_headers_and_finds_the_cli_directory() {
        let image = synthetic_pe();
        let pe = PeImage::parse(&image).unwrap();
        assert_eq!(pe.cli_header_rva(), 0x2000);
        assert_eq!(pe.cli_header_size(), 0x48);
    }

    #[test]
    fn maps_rvas_to_file_offsets() {
        let image = synthetic_pe();
        let pe = PeImage::parse(&image).unwrap();
        assert_eq!(pe.rva_to_offset(0x2000), Ok(0x400));
        assert_eq!(pe.rva_to_offset(0x2005), Ok(0x405));
        assert_eq!(pe.slice_at_rva(0x2000, 1).unwrap()[0], 0xCA);
        assert_eq!(pe.rva_to_offset(0x9999), Err(PeError::UnmappedRva));
    }

    #[test]
    fn rejects_non_pe_input() {
        assert_eq!(
            PeImage::parse(b"not a pe").unwrap_err(),
            PeError::BadDosSignature
        );
        assert_eq!(PeImage::parse(b"MZ").unwrap_err(), PeError::Truncated);
    }
}
