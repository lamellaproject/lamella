//! The metadata heaps a managed image references (ECMA-335 1st ed, II.24.2).

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Appends `value` in the compressed unsigned integer encoding (II.23.2): one
/// byte below `0x80`, two below `0x4000`, four otherwise, big-endian with the top
/// bits tagging the width.
pub fn compress_u32(value: u32, out: &mut Vec<u8>) {
    if value < 0x80 {
        out.push(value as u8);
    } else if value < 0x4000 {
        out.push((value >> 8) as u8 | 0x80);
        out.push(value as u8);
    } else {
        out.push((value >> 24) as u8 | 0xC0);
        out.push((value >> 16) as u8);
        out.push((value >> 8) as u8);
        out.push(value as u8);
    }
}

/// Appends `value` in the compressed *signed* integer encoding (II.23.2): the value
/// is rotated left by one within its bit-width so the sign lands in the low bit. The
/// width is chosen by the value's range (not the rotated magnitude, since a boundary
/// like `-2^13` rotates to a small number that must still occupy two bytes).
pub fn compress_i32(value: i32, out: &mut Vec<u8>) {
    let sign = u32::from(value < 0);
    if (-(1 << 6)..(1 << 6)).contains(&value) {
        let n = ((value & 0x3F) as u32) << 1 | sign;
        out.push(n as u8);
    } else if (-(1 << 13)..(1 << 13)).contains(&value) {
        let n = ((value & 0x1FFF) as u32) << 1 | sign;
        out.push((n >> 8) as u8 | 0x80);
        out.push(n as u8);
    } else {
        let n = ((value & 0x0FFF_FFFF) as u32) << 1 | sign;
        out.push((n >> 24) as u8 | 0xC0);
        out.push((n >> 16) as u8);
        out.push((n >> 8) as u8);
        out.push(n as u8);
    }
}

/// Builds the `#Strings` heap: the empty string sits at offset 0, and every other
/// distinct string is stored once as its UTF-8 bytes plus a null terminator.
#[derive(Debug)]
pub struct StringHeapBuilder {
    bytes: Vec<u8>,
    offsets: BTreeMap<String, u32>,
}

impl Default for StringHeapBuilder {
    fn default() -> StringHeapBuilder {
        StringHeapBuilder::new()
    }
}

impl StringHeapBuilder {
    /// A heap holding only the empty string at offset 0.
    #[must_use]
    pub fn new() -> StringHeapBuilder {
        StringHeapBuilder {
            bytes: vec![0],
            offsets: BTreeMap::new(),
        }
    }

    /// Interns `text`, returning its heap offset (0 for the empty string).
    pub fn intern(&mut self, text: &str) -> u32 {
        if text.is_empty() {
            return 0;
        }
        if let Some(&offset) = self.offsets.get(text) {
            return offset;
        }
        let offset = self.bytes.len() as u32;
        self.bytes.extend_from_slice(text.as_bytes());
        self.bytes.push(0);
        self.offsets.insert(text.into(), offset);
        offset
    }

    /// The heap's bytes, ready to place in the image.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// The heap's current size in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the heap holds only the empty string.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.len() <= 1
    }
}

/// Builds the `#Blob` heap: the empty blob sits at offset 0, and every other
/// distinct blob is stored once as a compressed length followed by its bytes.
#[derive(Debug)]
pub struct BlobHeapBuilder {
    bytes: Vec<u8>,
    offsets: BTreeMap<Vec<u8>, u32>,
}

impl Default for BlobHeapBuilder {
    fn default() -> BlobHeapBuilder {
        BlobHeapBuilder::new()
    }
}

impl BlobHeapBuilder {
    /// A heap holding only the empty blob at offset 0.
    #[must_use]
    pub fn new() -> BlobHeapBuilder {
        BlobHeapBuilder {
            bytes: vec![0],
            offsets: BTreeMap::new(),
        }
    }

    /// Interns `blob`, returning its heap offset (0 for the empty blob).
    pub fn intern(&mut self, blob: &[u8]) -> u32 {
        if blob.is_empty() {
            return 0;
        }
        if let Some(&offset) = self.offsets.get(blob) {
            return offset;
        }
        let offset = self.bytes.len() as u32;
        compress_u32(blob.len() as u32, &mut self.bytes);
        self.bytes.extend_from_slice(blob);
        self.offsets.insert(blob.to_vec(), offset);
        offset
    }

    /// The heap's bytes, ready to place in the image.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// The heap's current size in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the heap holds only the empty blob.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.len() <= 1
    }
}

/// Builds the `#US` user-string heap (II.24.2.4): like `#Blob`, but each entry is
/// a string's UTF-16LE bytes followed by a terminal byte that flags whether any
/// character needs more than the default code page. The empty blob sits at offset
/// 0; `ldstr` tokens are `0x70` plus an entry's offset.
#[derive(Debug, Default)]
pub struct UserStringHeapBuilder {
    bytes: Vec<u8>,
    offsets: BTreeMap<Vec<u16>, u32>,
}

impl UserStringHeapBuilder {
    /// A heap holding only the empty blob at offset 0.
    #[must_use]
    pub fn new() -> UserStringHeapBuilder {
        UserStringHeapBuilder {
            bytes: vec![0],
            offsets: BTreeMap::new(),
        }
    }

    /// Interns the UTF-16 `text`, returning its heap offset.
    pub fn intern(&mut self, text: &[u16]) -> u32 {
        if let Some(&offset) = self.offsets.get(text) {
            return offset;
        }
        let offset = self.bytes.len() as u32;
        let mut blob = Vec::with_capacity(text.len() * 2 + 1);
        for unit in text {
            blob.extend_from_slice(&unit.to_le_bytes());
        }
        blob.push(user_string_terminal(text));
        compress_u32(blob.len() as u32, &mut self.bytes);
        self.bytes.extend_from_slice(&blob);
        self.offsets.insert(text.to_vec(), offset);
        offset
    }

    /// The heap's bytes, ready to place in the image.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Whether the heap holds only the empty blob.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.len() <= 1
    }
}

/// The `#US` terminal byte: 1 if any character has a non-zero high byte or a low
/// byte the default code page would mishandle, else 0 (II.24.2.4).
fn user_string_terminal(text: &[u16]) -> u8 {
    let needs_marking = text
        .iter()
        .any(|&unit| unit > 0xFF || matches!(unit, 0x01..=0x08 | 0x0E..=0x1F | 0x27 | 0x2D | 0x7F));
    u8::from(needs_marking)
}

/// Builds the `#GUID` heap (II.24.2.5): a flat sequence of 16-byte GUIDs addressed
/// by a 1-based index, each distinct GUID stored once.
#[derive(Debug, Default)]
pub struct GuidHeapBuilder {
    guids: Vec<[u8; 16]>,
}

impl GuidHeapBuilder {
    /// An empty GUID heap.
    #[must_use]
    pub fn new() -> GuidHeapBuilder {
        GuidHeapBuilder { guids: Vec::new() }
    }

    /// Adds `guid`, returning its 1-based index (0 denotes no GUID).
    pub fn add(&mut self, guid: [u8; 16]) -> u32 {
        if let Some(position) = self.guids.iter().position(|existing| *existing == guid) {
            return position as u32 + 1;
        }
        self.guids.push(guid);
        self.guids.len() as u32
    }

    /// The heap's bytes, ready to place in the image.
    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.guids.into_iter().flatten().collect()
    }

    /// Whether the heap holds no GUIDs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.guids.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compressed(value: u32) -> Vec<u8> {
        let mut out = Vec::new();
        compress_u32(value, &mut out);
        out
    }

    #[test]
    fn compression_matches_the_spec_examples() {
        assert_eq!(compressed(0x03), [0x03]);
        assert_eq!(compressed(0x7F), [0x7F]);
        assert_eq!(compressed(0x80), [0x80, 0x80]);
        assert_eq!(compressed(0x2E57), [0xAE, 0x57]);
        assert_eq!(compressed(0x3FFF), [0xBF, 0xFF]);
        assert_eq!(compressed(0x4000), [0xC0, 0x00, 0x40, 0x00]);
        assert_eq!(compressed(0x1FFF_FFFF), [0xDF, 0xFF, 0xFF, 0xFF]);
    }

    fn compressed_signed(value: i32) -> Vec<u8> {
        let mut out = Vec::new();
        compress_i32(value, &mut out);
        out
    }

    #[test]
    fn signed_compression_matches_the_spec_examples() {
        assert_eq!(compressed_signed(3), [0x06]);
        assert_eq!(compressed_signed(-3), [0x7B]);
        assert_eq!(compressed_signed(64), [0x80, 0x80]);
        assert_eq!(compressed_signed(-64), [0x01]);
        assert_eq!(compressed_signed(8192), [0xC0, 0x00, 0x40, 0x00]);
        assert_eq!(compressed_signed(-8192), [0x80, 0x01]);
        assert_eq!(compressed_signed(268_435_455), [0xDF, 0xFF, 0xFF, 0xFE]);
        assert_eq!(compressed_signed(-268_435_456), [0xC0, 0x00, 0x00, 0x01]);
    }

    #[test]
    fn strings_intern_once_after_the_empty_string() {
        let mut heap = StringHeapBuilder::new();
        assert_eq!(heap.intern(""), 0);
        let foo = heap.intern("Foo");
        assert_eq!(foo, 1);
        assert_eq!(heap.intern("Bar"), 5);
        assert_eq!(heap.intern("Foo"), foo);
        assert_eq!(
            heap.into_bytes(),
            [0, b'F', b'o', b'o', 0, b'B', b'a', b'r', 0]
        );
    }

    #[test]
    fn blobs_are_length_prefixed_after_the_empty_blob() {
        let mut heap = BlobHeapBuilder::new();
        assert_eq!(heap.intern(&[]), 0);
        let three = heap.intern(&[1, 2, 3]);
        assert_eq!(three, 1);
        assert_eq!(heap.intern(&[1, 2, 3]), three);
        assert_eq!(heap.into_bytes(), [0, 3, 1, 2, 3]);
    }

    #[test]
    fn user_strings_are_utf16_with_a_terminal_byte() {
        let mut heap = UserStringHeapBuilder::new();
        let a = heap.intern(&[0x41]);
        assert_eq!(a, 1);
        assert_eq!(heap.intern(&[0x41]), a);
        assert_eq!(heap.into_bytes(), [0, 3, 0x41, 0x00, 0x00]);
    }

    #[test]
    fn user_string_terminal_flags_wide_characters() {
        assert_eq!(user_string_terminal(&[0x41, 0x42]), 0);
        assert_eq!(user_string_terminal(&[0x20AC]), 1);
        assert_eq!(user_string_terminal(&[0x7F]), 1);
    }

    #[test]
    fn guids_are_indexed_from_one() {
        let mut heap = GuidHeapBuilder::new();
        assert_eq!(heap.add([1; 16]), 1);
        assert_eq!(heap.add([2; 16]), 2);
        assert_eq!(heap.add([1; 16]), 1);
        assert_eq!(heap.into_bytes().len(), 32);
    }
}
