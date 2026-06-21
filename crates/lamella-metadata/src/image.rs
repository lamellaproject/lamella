//! The CLI header, metadata root, and stream headers (II.25.3.3, II.24.2.1).

use crate::bytes::{ReadError, Reader};
use crate::heaps::{BlobHeap, GuidHeap, StringsHeap, UserStringsHeap};
use crate::pe::{PeError, PeImage};

/// The metadata root signature `BSJB` (II.24.2.1), little-endian.
const METADATA_SIGNATURE: u32 = 0x424A_5342;

/// An error locating or parsing metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetadataError {
    /// The PE structure could not be parsed.
    Pe(PeError),
    /// A read ran past the end of the metadata.
    Truncated,
    /// The metadata root did not begin with the `BSJB` signature.
    BadSignature,
    /// The version string or a stream name was not valid UTF-8.
    BadText,
}

impl From<PeError> for MetadataError {
    fn from(error: PeError) -> MetadataError {
        MetadataError::Pe(error)
    }
}

impl From<ReadError> for MetadataError {
    fn from(_: ReadError) -> MetadataError {
        MetadataError::Truncated
    }
}

/// The metadata of a managed image: the version, the four heaps, and the tables
/// stream, each borrowed from the underlying bytes.
#[derive(Debug, Clone, Copy)]
pub struct MetadataImage<'a> {
    version: &'a str,
    strings: &'a [u8],
    user_strings: &'a [u8],
    blob: &'a [u8],
    guid: &'a [u8],
    tables: &'a [u8],
    pdb: &'a [u8],
    flags: u32,
    entry_point_token: u32,
}

impl<'a> MetadataImage<'a> {
    /// Reads the metadata of a whole managed PE file.
    pub fn read(file: &'a [u8]) -> Result<MetadataImage<'a>, MetadataError> {
        let pe = PeImage::parse(file)?;
        let cli = pe.slice_at_rva(pe.cli_header_rva(), pe.cli_header_size() as usize)?;
        let mut header = Reader::new(cli);
        header.skip(8)?;
        let metadata_rva = header.read_u32()?;
        let metadata_size = header.read_u32()?;
        let flags = header.read_u32()?;
        let entry_point_token = header.read_u32()?;
        let root = pe.slice_at_rva(metadata_rva, metadata_size as usize)?;
        let mut image = MetadataImage::parse_metadata_root(root)?;
        image.flags = flags;
        image.entry_point_token = entry_point_token;
        Ok(image)
    }

    /// Parses the metadata root (II.24.2.1) directly: signature, version, and the
    /// stream headers, slicing each stream out of `root`.
    pub fn parse_metadata_root(root: &'a [u8]) -> Result<MetadataImage<'a>, MetadataError> {
        let mut reader = Reader::new(root);
        if reader.read_u32()? != METADATA_SIGNATURE {
            return Err(MetadataError::BadSignature);
        }
        reader.skip(4)?;
        reader.skip(4)?;
        let version_length = reader.read_u32()? as usize;
        let version_bytes = reader.read_bytes(version_length)?;
        let version = nul_terminated_str(version_bytes)?;
        reader.skip(2)?;
        let stream_count = reader.read_u16()?;

        let mut image = MetadataImage {
            version,
            strings: &[],
            user_strings: &[],
            blob: &[],
            guid: &[],
            tables: &[],
            pdb: &[],
            flags: 0,
            entry_point_token: 0,
        };
        for _ in 0..stream_count {
            let offset = reader.read_u32()? as usize;
            let size = reader.read_u32()? as usize;
            let name_start = reader.position();
            let name_region = root.get(name_start..).ok_or(MetadataError::Truncated)?;
            let name_length = name_region
                .iter()
                .position(|&b| b == 0)
                .ok_or(MetadataError::Truncated)?;
            let name = core::str::from_utf8(&name_region[..name_length])
                .map_err(|_| MetadataError::BadText)?;
            let padded = (name_length + 1 + 3) & !3;
            reader.seek(name_start + padded)?;
            let end = offset.checked_add(size).ok_or(MetadataError::Truncated)?;
            let data = root.get(offset..end).ok_or(MetadataError::Truncated)?;
            match name {
                "#Strings" => image.strings = data,
                "#US" => image.user_strings = data,
                "#Blob" => image.blob = data,
                "#GUID" => image.guid = data,
                "#~" | "#-" => image.tables = data,
                "#Pdb" => image.pdb = data,
                _ => {}
            }
        }
        Ok(image)
    }

    /// The metadata version string (for example `v1.0.3705`).
    #[must_use]
    pub fn version(&self) -> &'a str {
        self.version
    }

    /// The `#Strings` heap.
    #[must_use]
    pub fn strings(&self) -> StringsHeap<'a> {
        StringsHeap::new(self.strings)
    }

    /// The `#US` (user strings) heap.
    #[must_use]
    pub fn user_strings(&self) -> UserStringsHeap<'a> {
        UserStringsHeap::new(self.user_strings)
    }

    /// The `#Blob` heap.
    #[must_use]
    pub fn blob(&self) -> BlobHeap<'a> {
        BlobHeap::new(self.blob)
    }

    /// The `#GUID` heap.
    #[must_use]
    pub fn guid(&self) -> GuidHeap<'a> {
        GuidHeap::new(self.guid)
    }

    /// The raw tables-stream (`#~`) bytes.
    #[must_use]
    pub fn tables(&self) -> &'a [u8] {
        self.tables
    }

    /// The raw `#Pdb` stream bytes (a standalone Portable PDB only), empty otherwise.
    #[must_use]
    pub fn pdb(&self) -> &'a [u8] {
        self.pdb
    }

    /// The CLI header flags (II.25.3.3.1), 0 if parsed from a bare root.
    #[must_use]
    pub fn flags(&self) -> u32 {
        self.flags
    }

    /// The entry-point metadata token, 0 if none or parsed from a bare root.
    #[must_use]
    pub fn entry_point_token(&self) -> u32 {
        self.entry_point_token
    }
}

/// Reads a NUL-terminated UTF-8 string from a possibly NUL-padded field.
fn nul_terminated_str(bytes: &[u8]) -> Result<&str, MetadataError> {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    core::str::from_utf8(&bytes[..end]).map_err(|_| MetadataError::BadText)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// A minimal metadata root: version "v1", a `#Strings` stream and a `#~`
    /// stream, with their data laid out right after the stream headers.
    fn synthetic_root() -> Vec<u8> {
        let mut root = Vec::new();
        root.extend_from_slice(&METADATA_SIGNATURE.to_le_bytes());
        root.extend_from_slice(&1u16.to_le_bytes());
        root.extend_from_slice(&1u16.to_le_bytes());
        root.extend_from_slice(&0u32.to_le_bytes());
        root.extend_from_slice(&4u32.to_le_bytes());
        root.extend_from_slice(b"v1\0\0");
        root.extend_from_slice(&0u16.to_le_bytes());
        root.extend_from_slice(&2u16.to_le_bytes());
        root.extend_from_slice(&56u32.to_le_bytes());
        root.extend_from_slice(&5u32.to_le_bytes());
        root.extend_from_slice(b"#Strings\0\0\0\0");
        root.extend_from_slice(&61u32.to_le_bytes());
        root.extend_from_slice(&2u32.to_le_bytes());
        root.extend_from_slice(b"#~\0\0");
        assert_eq!(root.len(), 56);
        root.extend_from_slice(b"\0Foo\0");
        root.extend_from_slice(&[0xAA, 0xBB]);
        root
    }

    #[test]
    fn parses_the_root_version_and_streams() {
        let root = synthetic_root();
        let image = MetadataImage::parse_metadata_root(&root).unwrap();
        assert_eq!(image.version(), "v1");
        assert_eq!(image.strings().get(1), Ok("Foo"));
        assert_eq!(image.tables(), &[0xAA, 0xBB]);
    }

    #[test]
    fn rejects_a_bad_signature() {
        assert_eq!(
            MetadataImage::parse_metadata_root(&[0; 32]).unwrap_err(),
            MetadataError::BadSignature
        );
    }
}
