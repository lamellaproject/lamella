//! The four metadata heaps and compressed integers (ECMA-335 1st ed, II.24.2).

/// An error reading a heap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeapError {
    /// An offset or index pointed outside the heap.
    OutOfBounds,
    /// A `#Strings` entry was not valid UTF-8 (II.24.2.3).
    InvalidUtf8,
    /// A compressed integer was malformed (II.23.2).
    BadCompressedInteger,
}

/// Reads a compressed unsigned integer (II.23.2), returning its value and the
/// number of bytes it occupied (1, 2, or 4).
///
/// The length is encoded in the high bits of the first byte: `0xxxxxxx` is a
/// one-byte value, `10xxxxxx` a two-byte value, and `110xxxxx` a four-byte value.
pub fn read_compressed_u32(bytes: &[u8]) -> Result<(u32, usize), HeapError> {
    let first = *bytes.first().ok_or(HeapError::OutOfBounds)?;
    if first & 0x80 == 0 {
        Ok((u32::from(first), 1))
    } else if first & 0xC0 == 0x80 {
        let second = *bytes.get(1).ok_or(HeapError::OutOfBounds)?;
        Ok(((u32::from(first & 0x3F) << 8) | u32::from(second), 2))
    } else if first & 0xE0 == 0xC0 {
        let second = *bytes.get(1).ok_or(HeapError::OutOfBounds)?;
        let third = *bytes.get(2).ok_or(HeapError::OutOfBounds)?;
        let fourth = *bytes.get(3).ok_or(HeapError::OutOfBounds)?;
        Ok((
            (u32::from(first & 0x1F) << 24)
                | (u32::from(second) << 16)
                | (u32::from(third) << 8)
                | u32::from(fourth),
            4,
        ))
    } else {
        Err(HeapError::BadCompressedInteger)
    }
}

/// Reads a compressed *signed* integer (II.23.2), returning its value and length.
/// The value is the unsigned form rotated right by one (the sign sits in the low
/// bit) and sign-extended from the high bit of its width (7, 14, or 28 magnitude
/// bits for a 1-, 2-, or 4-byte encoding).
pub fn read_compressed_i32(bytes: &[u8]) -> Result<(i32, usize), HeapError> {
    let (value, length) = read_compressed_u32(bytes)?;
    let magnitude_bits = match length {
        1 => 6,
        2 => 13,
        _ => 28,
    };
    let magnitude = (value >> 1) as i32;
    let result = if value & 1 != 0 {
        magnitude - (1 << magnitude_bits)
    } else {
        magnitude
    };
    Ok((result, length))
}

/// Returns the length-prefixed run at `offset`: a compressed-integer length then
/// that many bytes. Shared by the `#Blob` and `#US` heaps (II.24.2.4).
fn length_prefixed(bytes: &[u8], offset: u32) -> Result<&[u8], HeapError> {
    let start = offset as usize;
    let header = bytes.get(start..).ok_or(HeapError::OutOfBounds)?;
    let (length, consumed) = read_compressed_u32(header)?;
    let data_start = start + consumed;
    let data_end = data_start
        .checked_add(length as usize)
        .ok_or(HeapError::OutOfBounds)?;
    bytes
        .get(data_start..data_end)
        .ok_or(HeapError::OutOfBounds)
}

/// The `#Strings` heap: NUL-terminated UTF-8 strings indexed by byte offset
/// (II.24.2.3). Offset 0 is the empty string.
#[derive(Debug, Clone, Copy)]
pub struct StringsHeap<'a> {
    bytes: &'a [u8],
}

impl<'a> StringsHeap<'a> {
    /// Wraps the raw `#Strings` heap bytes.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> StringsHeap<'a> {
        StringsHeap { bytes }
    }

    /// The string starting at `offset`, up to the next NUL.
    pub fn get(&self, offset: u32) -> Result<&'a str, HeapError> {
        let start = offset as usize;
        let rest = self.bytes.get(start..).ok_or(HeapError::OutOfBounds)?;
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        core::str::from_utf8(&rest[..end]).map_err(|_| HeapError::InvalidUtf8)
    }
}

/// The `#Blob` heap: length-prefixed byte blobs indexed by byte offset
/// (II.24.2.4). Offset 0 is the empty blob.
#[derive(Debug, Clone, Copy)]
pub struct BlobHeap<'a> {
    bytes: &'a [u8],
}

impl<'a> BlobHeap<'a> {
    /// Wraps the raw `#Blob` heap bytes.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> BlobHeap<'a> {
        BlobHeap { bytes }
    }

    /// The blob at `offset`: its length prefix decoded, then its bytes.
    pub fn get(&self, offset: u32) -> Result<&'a [u8], HeapError> {
        length_prefixed(self.bytes, offset)
    }
}

/// The `#US` (user-strings) heap: length-prefixed UTF-16 strings indexed by byte
/// offset (II.24.2.4). The raw bytes are the UTF-16 code units followed by a
/// final flag byte; decoding is left to a higher layer.
#[derive(Debug, Clone, Copy)]
pub struct UserStringsHeap<'a> {
    bytes: &'a [u8],
}

impl<'a> UserStringsHeap<'a> {
    /// Wraps the raw `#US` heap bytes.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> UserStringsHeap<'a> {
        UserStringsHeap { bytes }
    }

    /// The raw bytes of the user string at `offset` (UTF-16 units plus the final
    /// flag byte).
    pub fn get(&self, offset: u32) -> Result<&'a [u8], HeapError> {
        length_prefixed(self.bytes, offset)
    }
}

/// The `#GUID` heap: a sequence of 16-byte GUIDs indexed 1-based (II.24.2.5).
/// Index 0 means "no GUID".
#[derive(Debug, Clone, Copy)]
pub struct GuidHeap<'a> {
    bytes: &'a [u8],
}

impl<'a> GuidHeap<'a> {
    /// Wraps the raw `#GUID` heap bytes.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> GuidHeap<'a> {
        GuidHeap { bytes }
    }

    /// The 16-byte GUID at 1-based `index`, or `None` if `index` is 0.
    pub fn get(&self, index: u32) -> Result<Option<&'a [u8; 16]>, HeapError> {
        if index == 0 {
            return Ok(None);
        }
        let start = (index as usize - 1) * 16;
        let end = start + 16;
        let slice = self.bytes.get(start..end).ok_or(HeapError::OutOfBounds)?;
        let guid: &[u8; 16] = slice.try_into().map_err(|_| HeapError::OutOfBounds)?;
        Ok(Some(guid))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compressed_integers_decode_per_the_spec() {
        assert_eq!(read_compressed_u32(&[0x03]), Ok((0x03, 1)));
        assert_eq!(read_compressed_u32(&[0x7F]), Ok((0x7F, 1)));
        assert_eq!(read_compressed_u32(&[0x80, 0x80]), Ok((0x80, 2)));
        assert_eq!(read_compressed_u32(&[0xBF, 0xFF]), Ok((0x3FFF, 2)));
        assert_eq!(
            read_compressed_u32(&[0xC0, 0x00, 0x40, 0x00]),
            Ok((0x4000, 4))
        );
        assert_eq!(
            read_compressed_u32(&[0xDF, 0xFF, 0xFF, 0xFF]),
            Ok((0x1FFF_FFFF, 4))
        );
    }

    #[test]
    fn signed_compressed_integers_decode_per_the_spec() {
        assert_eq!(read_compressed_i32(&[0x06]), Ok((3, 1)));
        assert_eq!(read_compressed_i32(&[0x7B]), Ok((-3, 1)));
        assert_eq!(read_compressed_i32(&[0x80, 0x80]), Ok((64, 2)));
        assert_eq!(read_compressed_i32(&[0x01]), Ok((-64, 1)));
        assert_eq!(
            read_compressed_i32(&[0xC0, 0x00, 0x40, 0x00]),
            Ok((8192, 4))
        );
        assert_eq!(read_compressed_i32(&[0x80, 0x01]), Ok((-8192, 2)));
        assert_eq!(
            read_compressed_i32(&[0xDF, 0xFF, 0xFF, 0xFE]),
            Ok((268_435_455, 4))
        );
        assert_eq!(
            read_compressed_i32(&[0xC0, 0x00, 0x00, 0x01]),
            Ok((-268_435_456, 4))
        );
    }

    #[test]
    fn compressed_integers_reject_truncation_and_bad_tags() {
        assert_eq!(read_compressed_u32(&[]), Err(HeapError::OutOfBounds));
        assert_eq!(read_compressed_u32(&[0x80]), Err(HeapError::OutOfBounds));
        assert_eq!(
            read_compressed_u32(&[0xC0, 0x00]),
            Err(HeapError::OutOfBounds)
        );
        assert_eq!(
            read_compressed_u32(&[0xFF]),
            Err(HeapError::BadCompressedInteger)
        );
    }

    #[test]
    fn strings_heap_reads_to_the_next_nul() {
        let heap = StringsHeap::new(b"\0Foo\0Bar\0");
        assert_eq!(heap.get(0), Ok(""));
        assert_eq!(heap.get(1), Ok("Foo"));
        assert_eq!(heap.get(5), Ok("Bar"));
        assert_eq!(heap.get(100), Err(HeapError::OutOfBounds));
    }

    #[test]
    fn blob_heap_reads_a_length_prefixed_run() {
        let heap = BlobHeap::new(&[0x00, 0x03, 0x0A, 0x0B, 0x0C]);
        assert_eq!(heap.get(0), Ok(&[][..]));
        assert_eq!(heap.get(1), Ok(&[0x0A, 0x0B, 0x0C][..]));
        assert_eq!(
            BlobHeap::new(&[0x05, 0x01]).get(0),
            Err(HeapError::OutOfBounds)
        );
    }

    #[test]
    fn guid_heap_is_one_based() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0xAA;
        bytes[16] = 0xBB;
        let heap = GuidHeap::new(&bytes);
        assert_eq!(heap.get(0), Ok(None));
        assert_eq!(heap.get(1).unwrap().unwrap()[0], 0xAA);
        assert_eq!(heap.get(2).unwrap().unwrap()[0], 0xBB);
        assert_eq!(heap.get(3), Err(HeapError::OutOfBounds));
    }
}
