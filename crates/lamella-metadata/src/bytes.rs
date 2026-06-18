//! A little-endian byte reader.

/// An error reading past the end of the input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadError;

/// A forward cursor over a byte slice, reading little-endian integers.
#[derive(Debug, Clone)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    /// Creates a reader positioned at the start of `bytes`.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Reader<'a> {
        Reader { bytes, position: 0 }
    }

    /// The current byte offset from the start.
    #[must_use]
    pub fn position(&self) -> usize {
        self.position
    }

    /// The number of bytes left to read.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.position
    }

    /// Whether the cursor is at the end.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.position >= self.bytes.len()
    }

    /// Moves the cursor to an absolute offset, which may be the end.
    pub fn seek(&mut self, position: usize) -> Result<(), ReadError> {
        if position > self.bytes.len() {
            return Err(ReadError);
        }
        self.position = position;
        Ok(())
    }

    /// Advances the cursor by `count` bytes.
    pub fn skip(&mut self, count: usize) -> Result<(), ReadError> {
        self.seek(self.position.checked_add(count).ok_or(ReadError)?)
    }

    /// Reads `count` bytes, advancing the cursor.
    pub fn read_bytes(&mut self, count: usize) -> Result<&'a [u8], ReadError> {
        let end = self.position.checked_add(count).ok_or(ReadError)?;
        let slice = self.bytes.get(self.position..end).ok_or(ReadError)?;
        self.position = end;
        Ok(slice)
    }

    /// Reads one byte.
    pub fn read_u8(&mut self) -> Result<u8, ReadError> {
        Ok(self.read_bytes(1)?[0])
    }

    /// Returns the byte at the cursor without advancing.
    pub fn peek_u8(&self) -> Result<u8, ReadError> {
        self.bytes.get(self.position).copied().ok_or(ReadError)
    }

    /// Reads a little-endian `u16`.
    pub fn read_u16(&mut self) -> Result<u16, ReadError> {
        let bytes = self.read_bytes(2)?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    /// Reads a little-endian `u32`.
    pub fn read_u32(&mut self) -> Result<u32, ReadError> {
        let bytes = self.read_bytes(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Reads a little-endian `u64`.
    pub fn read_u64(&mut self) -> Result<u64, ReadError> {
        let bytes = self.read_bytes(8)?;
        let mut array = [0u8; 8];
        array.copy_from_slice(bytes);
        Ok(u64::from_le_bytes(array))
    }

    /// Reads a compressed unsigned integer (II.23.2), advancing the cursor by the
    /// 1, 2, or 4 bytes it occupies.
    pub fn read_compressed_u32(&mut self) -> Result<u32, ReadError> {
        let first = self.read_u8()?;
        if first & 0x80 == 0 {
            Ok(u32::from(first))
        } else if first & 0xC0 == 0x80 {
            let second = self.read_u8()?;
            Ok((u32::from(first & 0x3F) << 8) | u32::from(second))
        } else if first & 0xE0 == 0xC0 {
            let second = self.read_u8()?;
            let third = self.read_u8()?;
            let fourth = self.read_u8()?;
            Ok((u32::from(first & 0x1F) << 24)
                | (u32::from(second) << 16)
                | (u32::from(third) << 8)
                | u32::from(fourth))
        } else {
            Err(ReadError)
        }
    }

    /// Reads a little-endian `u32` at an absolute offset without moving the cursor.
    pub fn u32_at(&self, offset: usize) -> Result<u32, ReadError> {
        let end = offset.checked_add(4).ok_or(ReadError)?;
        let bytes = self.bytes.get(offset..end).ok_or(ReadError)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_little_endian_integers_in_sequence() {
        let mut reader = Reader::new(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07]);
        assert_eq!(reader.read_u8(), Ok(0x01));
        assert_eq!(reader.read_u16(), Ok(0x0302));
        assert_eq!(reader.read_u32(), Ok(0x0706_0504));
        assert_eq!(reader.position(), 7);
        assert!(reader.is_empty());
    }

    #[test]
    fn reading_past_the_end_errors() {
        let mut reader = Reader::new(&[0x01, 0x02]);
        assert_eq!(reader.read_u32(), Err(ReadError));
        assert_eq!(reader.position(), 0);
        assert_eq!(reader.read_u16(), Ok(0x0201));
    }

    #[test]
    fn seek_skip_and_absolute_reads() {
        let mut reader = Reader::new(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11]);
        reader.skip(4).unwrap();
        assert_eq!(reader.read_u8(), Ok(0xEE));
        assert_eq!(reader.u32_at(0), Ok(0xDDCC_BBAA));
        reader.seek(0).unwrap();
        assert_eq!(reader.read_u8(), Ok(0xAA));
        assert_eq!(reader.seek(9), Err(ReadError));
    }

    #[test]
    fn read_bytes_borrows_a_run() {
        let mut reader = Reader::new(b"lamella");
        assert_eq!(reader.read_bytes(3), Ok(&b"lam"[..]));
        assert_eq!(reader.read_bytes(10), Err(ReadError));
    }
}
