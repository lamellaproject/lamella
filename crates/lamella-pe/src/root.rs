//! The metadata root: the `BSJB` header and stream directory (II.24.2.1).

use alloc::vec::Vec;

/// The `BSJB` little-endian signature that opens the metadata root.
const SIGNATURE: u32 = 0x424A_5342;

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

/// A null-terminated stream name padded to a four-byte boundary.
fn padded_name(name: &str) -> Vec<u8> {
    let mut bytes = Vec::from(name.as_bytes());
    bytes.push(0);
    while bytes.len() % 4 != 0 {
        bytes.push(0);
    }
    bytes
}

/// Assembles the metadata root around the already-serialized streams, naming each
/// present stream in the directory. `#~`, `#Strings`, and `#Blob` are always
/// present; `#US` and `#GUID` only when they hold something. `version` is the
/// target-runtime version string (e.g. `v4.0.30319`).
#[must_use]
pub fn metadata_root(
    version: &str,
    tables: &[u8],
    strings: &[u8],
    user_strings: Option<&[u8]>,
    guids: &[u8],
    blob: &[u8],
) -> Vec<u8> {
    let mut streams: Vec<(&str, &[u8])> = Vec::new();
    streams.push(("#~", tables));
    streams.push(("#Strings", strings));
    if let Some(user_strings) = user_strings {
        streams.push(("#US", user_strings));
    }
    if !guids.is_empty() {
        streams.push(("#GUID", guids));
    }
    streams.push(("#Blob", blob));

    let version_len = align4(version.len() + 1);

    let mut header_size = 4 + 2 + 2 + 4 + 4 + version_len + 2 + 2;
    for (name, _) in &streams {
        header_size += 8 + padded_name(name).len();
    }

    let mut offset = header_size;
    let mut offsets = Vec::with_capacity(streams.len());
    for (_, bytes) in &streams {
        offsets.push(offset);
        offset += align4(bytes.len());
    }

    let mut out = Vec::with_capacity(offset);
    out.extend_from_slice(&SIGNATURE.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&(version_len as u32).to_le_bytes());
    let mut version_field = Vec::from(version.as_bytes());
    version_field.resize(version_len, 0);
    out.extend_from_slice(&version_field);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&(streams.len() as u16).to_le_bytes());

    for ((name, bytes), &body_offset) in streams.iter().zip(&offsets) {
        out.extend_from_slice(&(body_offset as u32).to_le_bytes());
        out.extend_from_slice(&(align4(bytes.len()) as u32).to_le_bytes());
        out.extend_from_slice(&padded_name(name));
    }

    for (_, bytes) in &streams {
        out.extend_from_slice(bytes);
        while out.len() % 4 != 0 {
            out.push(0);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u16_at(bytes: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
    }
    fn u32_at(bytes: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
    }

    #[test]
    fn root_opens_with_bsjb_and_a_version_string() {
        let root = metadata_root("v4.0.30319", &[1, 2, 3, 4], &[0], None, &[], &[0]);
        assert_eq!(u32_at(&root, 0), SIGNATURE);
        assert_eq!(u32_at(&root, 12), 12);
        assert_eq!(&root[16..26], b"v4.0.30319");
    }

    #[test]
    fn streams_are_listed_and_their_offsets_point_at_the_bodies() {
        let tables = [0xAA, 0xBB, 0xCC, 0xDD];
        let root = metadata_root("v4", &tables, &[0], None, &[], &[0]);
        let version_len = 4usize;
        let count_at = 4 + 2 + 2 + 4 + 4 + version_len + 2;
        assert_eq!(u16_at(&root, count_at), 3);
        let first_offset = u32_at(&root, count_at + 2) as usize;
        assert_eq!(&root[first_offset..first_offset + 4], &tables);
    }

    #[test]
    fn user_strings_and_guids_appear_only_when_present() {
        let without = metadata_root("v4", &[0; 4], &[0], None, &[], &[0]);
        let with = metadata_root("v4", &[0; 4], &[0], Some(&[0]), &[7; 16], &[0]);
        let version_len = 4usize;
        let count_at = 4 + 2 + 2 + 4 + 4 + version_len + 2;
        assert_eq!(u16_at(&without, count_at), 3);
        assert_eq!(u16_at(&with, count_at), 5);
    }
}
