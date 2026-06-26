//! The native off-heap memory seam (`Marshal.AllocHGlobal`/`FreeHGlobal` + raw `Read*`/`Write*`),
//! behind a trait the embedder selects at startup. Two backends ship (see the design note):

use alloc::vec;
use alloc::vec::Vec;

/// The native-memory seam. `Debug` is a supertrait so the [`crate::interp::Vm`] -- which holds a
/// `Box<dyn MemoryBackend>` -- still derives `Debug`.
pub trait MemoryBackend: core::fmt::Debug {
    /// Allocates `size` zeroed bytes and returns the `IntPtr`-encoded base address (0 == out of
    /// memory, i.e. `IntPtr.Zero`).
    fn alloc(&mut self, size: u64) -> u64;

    /// Frees a block previously returned by [`MemoryBackend::alloc`]. A no-op for an unknown or
    /// already-freed pointer.
    fn free(&mut self, ptr: u64);

    /// Reads `dst.len()` bytes starting at `ptr` into `dst`. Returns `false` (leaving `dst`
    /// untouched) if the range is out of bounds / the block is freed.
    fn read(&self, ptr: u64, dst: &mut [u8]) -> bool;

    /// Writes `src` starting at `ptr`. Returns `false` (writing nothing) if the range is out of
    /// bounds / the block is freed.
    fn write(&mut self, ptr: u64, src: &[u8]) -> bool;
}

/// The safe backend: a bounds-checked block table. An `IntPtr` packs `block index + 1` in the high 32
/// bits (so the first block's base is non-null) and a byte offset in the low 32 bits, so `ptr + n`
/// pointer arithmetic stays within a block. Freeing drops the block's bytes; a later access then
/// fails the bounds check rather than reading stale or foreign memory.
#[derive(Debug, Default)]
pub struct SafeMemory {
    /// Allocated blocks; `None` marks a freed slot (kept so later indices stay stable).
    blocks: Vec<Option<Vec<u8>>>,
}

impl SafeMemory {
    /// A new, empty block table.
    #[must_use]
    pub fn new() -> Self {
        Self { blocks: Vec::new() }
    }

    /// Splits a pointer into `(block index, byte offset)`, or `None` if the block id is null / out of
    /// range. The bytes may still be a freed (`None`) slot -- the caller checks that.
    fn resolve(&self, ptr: u64) -> Option<(usize, usize)> {
        let id = (ptr >> 32) as usize;
        if id == 0 || id > self.blocks.len() {
            return None;
        }
        Some((id - 1, (ptr & 0xFFFF_FFFF) as usize))
    }
}

impl MemoryBackend for SafeMemory {
    fn alloc(&mut self, size: u64) -> u64 {
        self.blocks.push(Some(vec![0u8; size as usize]));
        (self.blocks.len() as u64) << 32
    }

    fn free(&mut self, ptr: u64) {
        if let Some((block, _)) = self.resolve(ptr) {
            self.blocks[block] = None;
        }
    }

    fn read(&self, ptr: u64, dst: &mut [u8]) -> bool {
        if let Some((block, offset)) = self.resolve(ptr) {
            if let Some(Some(bytes)) = self.blocks.get(block) {
                if let Some(slice) = bytes.get(offset..offset.wrapping_add(dst.len())) {
                    dst.copy_from_slice(slice);
                    return true;
                }
            }
        }
        false
    }

    fn write(&mut self, ptr: u64, src: &[u8]) -> bool {
        if let Some((block, offset)) = self.resolve(ptr) {
            if let Some(Some(bytes)) = self.blocks.get_mut(block) {
                if let Some(slice) = bytes.get_mut(offset..offset.wrapping_add(src.len())) {
                    slice.copy_from_slice(src);
                    return true;
                }
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alloc_read_write_roundtrip() {
        let mut mem = SafeMemory::new();
        let ptr = mem.alloc(16);
        assert_ne!(ptr, 0, "a fresh allocation is non-null");
        assert!(mem.write(ptr, &[1, 2, 3, 4]));
        let mut buf = [0u8; 4];
        assert!(mem.read(ptr, &mut buf));
        assert_eq!(buf, [1, 2, 3, 4]);
    }

    #[test]
    fn offset_within_block_is_addressable() {
        let mut mem = SafeMemory::new();
        let ptr = mem.alloc(8);
        assert!(mem.write(ptr + 4, &[9]));
        let mut buf = [0u8; 1];
        assert!(mem.read(ptr + 4, &mut buf));
        assert_eq!(buf[0], 9);
    }

    #[test]
    fn out_of_bounds_and_freed_access_fail_safely() {
        let mut mem = SafeMemory::new();
        let ptr = mem.alloc(4);
        let mut buf = [0u8; 4];
        assert!(!mem.read(ptr + 2, &mut buf), "a 4-byte read 2 in past the end is rejected");
        mem.free(ptr);
        assert!(!mem.read(ptr, &mut buf), "a freed block is no longer readable");
        assert!(!mem.write(ptr, &[0]), "a freed block is no longer writable");
    }

    #[test]
    fn null_pointer_resolves_to_nothing() {
        let mem = SafeMemory::new();
        let mut buf = [0u8; 1];
        assert!(!mem.read(0, &mut buf));
    }
}
