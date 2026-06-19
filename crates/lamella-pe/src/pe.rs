//! The managed PE wrapper around the metadata (ECMA-335 1st ed, II.25).

use alloc::vec;
use alloc::vec::Vec;

/// CLI header flag: the image contains only IL, no native code (II.25.3.3.1).
pub const COMIMAGE_FLAGS_ILONLY: u32 = 0x0000_0001;

/// Where the single `.text` section is mapped in virtual space.
pub const TEXT_RVA: u32 = 0x2000;
/// The section alignment in virtual space (II.25.2.3.1).
const SECTION_ALIGNMENT: u32 = 0x2000;
/// The file alignment of section data.
const FILE_ALIGNMENT: u32 = 0x200;
/// The preferred load address.
const IMAGE_BASE: u32 = 0x0040_0000;
/// Where the PE signature begins (right after a bare 64-byte DOS header).
const PE_OFFSET: u32 = 0x40;
/// The PE32 optional header size: 96 fixed bytes plus 16 eight-byte directories.
const OPTIONAL_HEADER_SIZE: u32 = 224;

fn align(value: u32, alignment: u32) -> u32 {
    (value + alignment - 1) & !(alignment - 1)
}

fn put_u16(out: &mut [u8], at: usize, value: u16) {
    out[at..at + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut [u8], at: usize, value: u32) {
    out[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Writes a managed PE around `metadata`, naming `entry_point_token` (a `MethodDef`
/// token, or 0 for a library). `is_dll` selects the image characteristics. The
/// CLI header is placed first in `.text`, then the metadata root.
#[must_use]
pub fn write_pe(metadata: &[u8], entry_point_token: u32, is_dll: bool) -> Vec<u8> {
    let metadata_rva = TEXT_RVA + CLI_HEADER_SIZE;
    let cli = cli_header(
        metadata_rva,
        metadata.len() as u32,
        COMIMAGE_FLAGS_ILONLY,
        entry_point_token,
    );

    let mut text = Vec::with_capacity(cli.len() + metadata.len());
    text.extend_from_slice(&cli);
    text.extend_from_slice(metadata);
    write_image(&text, is_dll)
}

/// The size of one `IMAGE_DEBUG_DIRECTORY` entry (II PE/COFF).
const DEBUG_DIRECTORY_SIZE: u32 = 28;

/// Writes the PE headers around a prebuilt `.text` section whose CLI header sits
/// at its start (so the COM-descriptor directory points at [`TEXT_RVA`]).
#[must_use]
pub fn write_image(text: &[u8], is_dll: bool) -> Vec<u8> {
    write_image_with_debug(text, is_dll, None)
}

/// Builds one `IMAGE_DEBUG_DIRECTORY` entry of type `CODEVIEW` (2), addressing the
/// CodeView record by both RVA and file offset.
fn debug_directory_entry(
    address_of_raw_data: u32,
    pointer_to_raw_data: u32,
    size_of_data: u32,
) -> [u8; DEBUG_DIRECTORY_SIZE as usize] {
    let mut entry = [0u8; DEBUG_DIRECTORY_SIZE as usize];
    put_u32(&mut entry, 12, 2);
    put_u32(&mut entry, 16, size_of_data);
    put_u32(&mut entry, 20, address_of_raw_data);
    put_u32(&mut entry, 24, pointer_to_raw_data);
    entry
}

/// Writes the PE headers around `text`, optionally appending a debug directory and
/// `codeview` record (an `RSDS` blob) so a debugger can find and match the PDB.
#[must_use]
pub fn write_image_with_debug(text: &[u8], is_dll: bool, codeview: Option<&[u8]>) -> Vec<u8> {
    let optional_start = (PE_OFFSET + 4 + 20) as usize;
    let headers_end = optional_start as u32 + OPTIONAL_HEADER_SIZE + 40;
    let size_of_headers = align(headers_end, FILE_ALIGNMENT);

    let mut body = Vec::from(text);
    let mut debug_directory = None;
    if let Some(codeview) = codeview {
        let entry_offset = body.len() as u32;
        let data_offset = entry_offset + DEBUG_DIRECTORY_SIZE;
        let entry = debug_directory_entry(
            TEXT_RVA + data_offset,
            size_of_headers + data_offset,
            codeview.len() as u32,
        );
        body.extend_from_slice(&entry);
        body.extend_from_slice(codeview);
        debug_directory = Some((TEXT_RVA + entry_offset, DEBUG_DIRECTORY_SIZE));
    }

    let text = body.as_slice();
    let text_virtual_size = text.len() as u32;
    let text_raw_size = align(text_virtual_size, FILE_ALIGNMENT);
    let size_of_image = align(TEXT_RVA + text_virtual_size, SECTION_ALIGNMENT);

    let mut out = vec![0u8; size_of_headers as usize];

    out[0] = b'M';
    out[1] = b'Z';
    put_u32(&mut out, 0x3C, PE_OFFSET);

    let pe = PE_OFFSET as usize;
    out[pe..pe + 4].copy_from_slice(b"PE\0\0");
    put_u16(&mut out, pe + 4, 0x014C);
    put_u16(&mut out, pe + 6, 1);
    put_u16(&mut out, pe + 20, OPTIONAL_HEADER_SIZE as u16);
    let characteristics: u16 = if is_dll { 0x2102 } else { 0x0102 };
    put_u16(&mut out, pe + 22, characteristics);

    let opt = optional_start;
    put_u16(&mut out, opt, 0x010B);
    out[opt + 2] = 8;
    put_u32(&mut out, opt + 0x04, text_raw_size);
    put_u32(&mut out, opt + 0x14, 0);
    put_u32(&mut out, opt + 0x18, TEXT_RVA);
    put_u32(&mut out, opt + 0x1C, IMAGE_BASE);
    put_u32(&mut out, opt + 0x20, SECTION_ALIGNMENT);
    put_u32(&mut out, opt + 0x24, FILE_ALIGNMENT);
    put_u16(&mut out, opt + 0x28, 4);
    put_u16(&mut out, opt + 0x30, 4);
    put_u32(&mut out, opt + 0x38, size_of_image);
    put_u32(&mut out, opt + 0x3C, size_of_headers);
    put_u16(&mut out, opt + 0x44, 3);
    put_u32(&mut out, opt + 0x48, 0x0010_0000);
    put_u32(&mut out, opt + 0x4C, 0x1000);
    put_u32(&mut out, opt + 0x50, 0x0010_0000);
    put_u32(&mut out, opt + 0x54, 0x1000);
    put_u32(&mut out, opt + 0x5C, 16);
    let cli_directory = opt + 0x60 + 14 * 8;
    put_u32(&mut out, cli_directory, TEXT_RVA);
    put_u32(&mut out, cli_directory + 4, CLI_HEADER_SIZE);
    if let Some((rva, size)) = debug_directory {
        let debug = opt + 0x60 + 6 * 8;
        put_u32(&mut out, debug, rva);
        put_u32(&mut out, debug + 4, size);
    }

    let section = opt + OPTIONAL_HEADER_SIZE as usize;
    out[section..section + 5].copy_from_slice(b".text");
    put_u32(&mut out, section + 0x08, text_virtual_size);
    put_u32(&mut out, section + 0x0C, TEXT_RVA);
    put_u32(&mut out, section + 0x10, text_raw_size);
    put_u32(&mut out, section + 0x14, size_of_headers);
    put_u32(&mut out, section + 0x24, 0x6000_0020);

    out.extend_from_slice(text);
    out.resize((size_of_headers + text_raw_size) as usize, 0);
    out
}

/// The size of the CLI header in bytes (II.25.3.3).
pub const CLI_HEADER_SIZE: u32 = 72;

/// Builds the CLI header (`IMAGE_COR20_HEADER`): it records its own size, the CLI
/// version it targets, the RVA and size of the metadata root, the image flags,
/// and the entry-point token (a `MethodDef` token for an executable, 0 for a
/// library). The remaining directories -- resources, strong name, fixups -- are
/// left empty.
#[must_use]
pub fn cli_header(
    metadata_rva: u32,
    metadata_size: u32,
    flags: u32,
    entry_point_token: u32,
) -> [u8; CLI_HEADER_SIZE as usize] {
    let mut header = [0u8; CLI_HEADER_SIZE as usize];
    header[0..4].copy_from_slice(&CLI_HEADER_SIZE.to_le_bytes());
    header[4..6].copy_from_slice(&2u16.to_le_bytes());
    header[6..8].copy_from_slice(&5u16.to_le_bytes());
    header[8..12].copy_from_slice(&metadata_rva.to_le_bytes());
    header[12..16].copy_from_slice(&metadata_size.to_le_bytes());
    header[16..20].copy_from_slice(&flags.to_le_bytes());
    header[20..24].copy_from_slice(&entry_point_token.to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn a_written_pe_round_trips_through_the_reader() {
        use crate::heap::{GuidHeapBuilder, StringHeapBuilder};
        use crate::root::metadata_root;
        use crate::tables::{Column, HeapSizes, TableStream};
        use lamella_metadata::tables::table;

        let mut strings = StringHeapBuilder::new();
        let name = strings.intern("test.dll");
        let mut guids = GuidHeapBuilder::new();
        let mvid = guids.add([0x11; 16]);
        let mut tables = TableStream::new();
        tables.add_row(
            table::MODULE,
            alloc::vec![
                Column::U16(0),
                Column::StringRef(name),
                Column::GuidRef(mvid),
                Column::GuidRef(0),
                Column::GuidRef(0),
            ],
        );
        let metadata = metadata_root(
            "v4.0.30319",
            &tables.serialize(HeapSizes::default()),
            &strings.into_bytes(),
            None,
            &guids.into_bytes(),
            &[0],
        );

        let pe = write_pe(&metadata, 0, true);

        let image = lamella_metadata::pe::PeImage::parse(&pe).expect("valid PE");
        assert_eq!(image.cli_header_rva(), TEXT_RVA);
        assert_eq!(image.cli_header_size(), CLI_HEADER_SIZE);
        assert!(lamella_metadata::image::MetadataImage::read(&pe).is_ok());
    }

    #[test]
    fn cli_header_records_its_size_metadata_and_entry_point() {
        let header = cli_header(0x2100, 500, COMIMAGE_FLAGS_ILONLY, 0x0600_0001);
        assert_eq!(u32_at(&header, 0), 72);
        assert_eq!(u32_at(&header, 8), 0x2100);
        assert_eq!(u32_at(&header, 12), 500);
        assert_eq!(u32_at(&header, 16), COMIMAGE_FLAGS_ILONLY);
        assert_eq!(u32_at(&header, 20), 0x0600_0001);
        assert_eq!(header[4], 2);
        assert_eq!(header[6], 5);
    }
}
