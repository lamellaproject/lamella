//! A bounds-checked cursor over a debug section's bytes.

use crate::DwarfError;

/// A read position within a byte slice.
#[derive(Debug, Clone)]
pub struct Cursor<'a> {
    bytes: &'a [u8],
    offset: usize,
    /// Where `bytes` began in whatever this cursor was split from, so a position can be reported
    /// in the frame a caller means rather than only in the one it is being read in. Zero for a
    /// cursor built over a whole section.
    origin: usize,
}

impl<'a> Cursor<'a> {
    /// Starts a cursor at the beginning of `bytes`.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Cursor {
            bytes,
            offset: 0,
            origin: 0,
        }
    }

    /// Starts a cursor at `offset` within `bytes`, which may be one past the end (an empty rest).
    pub fn at(bytes: &'a [u8], offset: usize) -> Result<Self, DwarfError> {
        if offset > bytes.len() {
            return Err(DwarfError::Truncated);
        }
        Ok(Cursor {
            bytes,
            offset,
            origin: 0,
        })
    }

    /// The current offset from the start of the slice the cursor was built over.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// The current offset in the frame the slice was SPLIT FROM, rather than in the slice itself.
    ///
    /// # WHY BOTH EXIST, AND WHY GETTING IT WRONG LOOKS LIKE NOTHING
    ///
    /// [`Cursor::split`] confines a unit to its own length, and the cursor it hands back counts
    /// from the unit's BODY. Several DWARF forms count from somewhere else: `DW_FORM_ref4` and its
    /// siblings are offsets from the first byte of the containing unit's HEADER, and
    /// `DW_FORM_ref_addr` from the beginning of the section. A reference resolved against the wrong
    /// one lands a whole header early and still finds a well-formed entry -- so an attribute reads
    /// back as another entry's, and nothing anywhere reports a problem. A debugger built on it
    /// shows real names against the wrong variables.
    ///
    /// So the frame is asked for rather than assumed. This reports the position a caller holding
    /// the OUTER slice would see; [`Cursor::offset`] reports the inner one, and neither has to be
    /// reconstructed by adding a header size the caller had to know.
    #[must_use]
    pub fn origin_offset(&self) -> usize {
        self.origin + self.offset
    }

    /// True once every byte has been read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.offset >= self.bytes.len()
    }

    /// The number of bytes left.
    #[must_use]
    pub fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.offset)
    }

    /// Every byte the cursor was built over, whatever has been read.
    ///
    /// A line-number program starts where its header's `header_length` field says it does, which is
    /// an offset from within the unit rather than from wherever the header parse happened to stop,
    /// so re-entering the unit needs the unit's whole span.
    #[must_use]
    pub fn whole(&self) -> &'a [u8] {
        self.bytes
    }

    /// Moves the read position forward by `count` bytes.
    pub fn skip(&mut self, count: usize) -> Result<(), DwarfError> {
        let end = self.offset.checked_add(count).ok_or(DwarfError::Truncated)?;
        if end > self.bytes.len() {
            return Err(DwarfError::Truncated);
        }
        self.offset = end;
        Ok(())
    }

    /// Takes the next `count` bytes.
    pub fn take(&mut self, count: usize) -> Result<&'a [u8], DwarfError> {
        let end = self.offset.checked_add(count).ok_or(DwarfError::Truncated)?;
        let slice = self.bytes.get(self.offset..end).ok_or(DwarfError::Truncated)?;
        self.offset = end;
        Ok(slice)
    }

    /// Splits off the next `count` bytes as a cursor of their own, leaving this one past them.
    ///
    /// This is how a unit is confined to its own `unit_length`: a run-off inside the returned
    /// cursor reports truncation rather than reading into whatever unit follows.
    ///
    /// The returned cursor remembers where it began here, which [`Cursor::origin_offset`] reports.
    /// Without that a caller resolving a form counted from outside the split has to add a header
    /// size it was never told, and the failure is silent -- see that method.
    pub fn split(&mut self, count: usize) -> Result<Cursor<'a>, DwarfError> {
        let origin = self.origin_offset();
        let bytes = self.take(count)?;
        Ok(Cursor {
            bytes,
            offset: 0,
            origin,
        })
    }

    /// Reads one byte.
    pub fn u8(&mut self) -> Result<u8, DwarfError> {
        Ok(self.take(1)?[0])
    }

    /// Reads a little-endian `u16`.
    pub fn u16(&mut self) -> Result<u16, DwarfError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    /// Reads a little-endian `u32`.
    pub fn u32(&mut self) -> Result<u32, DwarfError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// Reads a little-endian `u64`.
    pub fn u64(&mut self) -> Result<u64, DwarfError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    /// Reads an address of `size` bytes (1, 2, 4 or 8) as a `u64`.
    ///
    /// The size comes from the unit rather than from the host: a 32-bit target's DWARF stores
    /// 4-byte addresses whatever the machine reading it is, and this crate must read a 64-bit
    /// producer's output on the same run.
    pub fn address(&mut self, size: u8) -> Result<u64, DwarfError> {
        match size {
            1 => Ok(u64::from(self.u8()?)),
            2 => Ok(u64::from(self.u16()?)),
            4 => Ok(u64::from(self.u32()?)),
            8 => self.u64(),
            other => Err(DwarfError::UnsupportedAddressSize(other)),
        }
    }

    /// Reads an unsigned LEB128.
    ///
    /// A run of continuation bytes is refused past 10 bytes rather than shifted out of a `u64`:
    /// 10 groups of 7 bits is the most that can encode one, so an eleventh means the bytes are not
    /// a LEB128 at all.
    pub fn uleb128(&mut self) -> Result<u64, DwarfError> {
        let mut result: u64 = 0;
        let mut shift = 0;
        for _ in 0..10 {
            let byte = self.u8()?;
            result |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(result);
            }
            shift += 7;
        }
        Err(DwarfError::MalformedLeb128)
    }

    /// Reads a signed LEB128, sign-extending the final group.
    pub fn sleb128(&mut self) -> Result<i64, DwarfError> {
        let mut result: i64 = 0;
        let mut shift = 0;
        for _ in 0..10 {
            let byte = self.u8()?;
            result |= i64::from(byte & 0x7f) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if shift < 64 && byte & 0x40 != 0 {
                    result |= -1i64 << shift;
                }
                return Ok(result);
            }
        }
        Err(DwarfError::MalformedLeb128)
    }

    /// Reads a NUL-terminated string, leaving the cursor past the NUL.
    ///
    /// The bytes are returned as they were stored. DWARF does not require them to be UTF-8 and a
    /// path from another host may not be, so validation is the caller's to do where it needs a
    /// `str`.
    pub fn cstr(&mut self) -> Result<&'a [u8], DwarfError> {
        let rest = self.bytes.get(self.offset..).ok_or(DwarfError::Truncated)?;
        let end = rest
            .iter()
            .position(|&b| b == 0)
            .ok_or(DwarfError::Truncated)?;
        let out = &rest[..end];
        self.offset += end + 1;
        Ok(out)
    }

    /// Reads an `initial length` and reports which of the two DWARF formats it selects.
    ///
    /// The 32-bit format stores the length in 4 bytes; the 64-bit format stores `0xffffffff` and
    /// then the real length in 8. The distinction is not the target's address size and does not
    /// follow from it: a 32-bit target can carry 64-bit-format DWARF and a 64-bit one usually does
    /// not. It also sets the width of every section offset in the unit that follows, which is why
    /// it is read once here and carried rather than assumed per field.
    pub fn initial_length(&mut self) -> Result<(u64, Format), DwarfError> {
        let first = self.u32()?;
        match first {
            0xffff_ffff => Ok((self.u64()?, Format::Dwarf64)),
            0xffff_fff0..=0xffff_fffe => Err(DwarfError::ReservedInitialLength(first)),
            len => Ok((u64::from(len), Format::Dwarf32)),
        }
    }

    /// Reads a section offset of the width `format` selects.
    pub fn offset_of(&mut self, format: Format) -> Result<u64, DwarfError> {
        match format {
            Format::Dwarf32 => Ok(u64::from(self.u32()?)),
            Format::Dwarf64 => self.u64(),
        }
    }
}

/// Which of the two DWARF formats a unit uses, which fixes the width of its section offsets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// 4-byte lengths and section offsets. What every producer this tree has met emits.
    Dwarf32,
    /// 8-byte lengths and section offsets, introduced by a `0xffffffff` escape.
    Dwarf64,
}

impl Format {
    /// The width in bytes of a section offset in this format.
    #[must_use]
    pub fn offset_size(self) -> u8 {
        match self {
            Format::Dwarf32 => 4,
            Format::Dwarf64 => 8,
        }
    }
}
