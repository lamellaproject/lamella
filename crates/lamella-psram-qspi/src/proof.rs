//! The read-back proof: patterns written over the window and read back
//! through both of its views, so that a wrong size (a smaller part aliases
//! onto itself), an unwired line, a wrong timing or a window left read-only
//! each produce a mismatch naming the address, the expected value and the
//! value read. Written against [`Memory`], so a host test runs the walks
//! over a buffer and over doctored buffers the walks must refuse.
//!
//! Two walks: the stride walk, one word per page and the window's last
//! word, cheap enough for every boot; and the full walk, every word of the
//! window under two patterns, for a gate and a flight record. Both write
//! their patterns through the uncached view (one transfer per word, no
//! cache line left dirty), read them back through the uncached view (the
//! memory itself) and through the cached view (the path a program takes),
//! then restore zeros through the uncached view and flush the cache, so the
//! window is zero in memory and nothing stale remains cached.

use crate::Window;

/// The window's two views as the proof reaches them.
pub trait Memory {
    /// Write one word at an address of either view.
    ///
    /// # Safety
    /// The address lies inside a window the implementation may write.
    unsafe fn write(&mut self, addr: usize, v: u32);
    /// Read one word at an address of either view.
    ///
    /// # Safety
    /// The address lies inside a window the implementation may read.
    unsafe fn read(&mut self, addr: usize) -> u32;
    /// Write every dirty cache line downstream and drop every line, so the
    /// next cached read fetches from memory. Nothing on a host without a
    /// cache in the path.
    ///
    /// # Safety
    /// No other user of the cache is in flight.
    unsafe fn flush(&mut self);
}

/// Which view a mismatch was read through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View {
    Cached,
    Uncached,
}

/// Which pattern a mismatch was found under.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// Each word holds its own cached-view address, salted.
    Address,
    /// Each word holds the complement of that.
    Complement,
}

/// A word that did not read back as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Mismatch {
    /// The cached-view address of the word.
    pub addr: usize,
    pub expected: u32,
    pub read: u32,
    pub view: View,
    pub pattern: Pattern,
}

/// The stride walk's step: one word per page of the part on the boards
/// stated today.
pub const STRIDE_BYTES: usize = 1024;

#[inline(always)]
fn expected(addr: usize, salt: u32, pattern: Pattern) -> u32 {
    let v = (addr as u32) ^ salt;
    match pattern {
        Pattern::Address => v,
        Pattern::Complement => !v,
    }
}

/// Write the pattern at the offsets `offsets` yields, read it back through
/// both views, then zero those words and flush.
unsafe fn walk<M: Memory, I: Iterator<Item = usize> + Clone>(
    m: &mut M,
    w: &Window,
    salt: u32,
    pattern: Pattern,
    offsets: I,
) -> Result<(), Mismatch> {
    unsafe {
        m.flush();
        for off in offsets.clone() {
            m.write(w.uncached_base + off, expected(w.base + off, salt, pattern));
        }
        for off in offsets.clone() {
            let want = expected(w.base + off, salt, pattern);
            let got = m.read(w.uncached_base + off);
            if got != want {
                restore(m, w, offsets);
                return Err(Mismatch { addr: w.base + off, expected: want, read: got, view: View::Uncached, pattern });
            }
        }
        for off in offsets.clone() {
            let want = expected(w.base + off, salt, pattern);
            let got = m.read(w.base + off);
            if got != want {
                restore(m, w, offsets);
                return Err(Mismatch { addr: w.base + off, expected: want, read: got, view: View::Cached, pattern });
            }
        }
        restore(m, w, offsets);
    }
    Ok(())
}

/// Zeros at the offsets through the uncached view, then the flush that
/// drops the cached copies of the pattern.
unsafe fn restore<M: Memory, I: Iterator<Item = usize>>(m: &mut M, w: &Window, offsets: I) {
    unsafe {
        for off in offsets {
            m.write(w.uncached_base + off, 0);
        }
        m.flush();
    }
}

/// The offsets of the stride walk: the first word of every page and the
/// window's last word.
fn stride_offsets(bytes: usize) -> impl Iterator<Item = usize> + Clone {
    let last = bytes.saturating_sub(4);
    (0..bytes).step_by(STRIDE_BYTES).chain(core::iter::once(last).filter(move |l| l % STRIDE_BYTES != 0))
}

/// The stride walk: one word per page and the last word, under the address
/// pattern. The window's size must be a multiple of four and at least four.
///
/// # Safety
/// The window is the caller's to write, and no other user of it runs.
pub unsafe fn stride_walk<M: Memory>(m: &mut M, w: &Window, salt: u32) -> Result<(), Mismatch> {
    unsafe { walk(m, w, salt, Pattern::Address, stride_offsets(w.bytes)) }
}

/// The full walk: every word of the window under the address pattern, then
/// under its complement.
///
/// # Safety
/// The window is the caller's to write, and no other user of it runs.
pub unsafe fn full_walk<M: Memory>(m: &mut M, w: &Window, salt: u32) -> Result<(), Mismatch> {
    unsafe {
        walk(m, w, salt, Pattern::Address, (0..w.bytes).step_by(4))?;
        walk(m, w, salt, Pattern::Complement, (0..w.bytes).step_by(4))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::{Id, WindowTiming};
    use std::vec;
    use std::vec::Vec;

    const BASE: usize = 0x1100_0000;
    const UNCACHED: usize = 0x1500_0000;

    /// A buffer both views alias, with three doctorings a walk must refuse.
    struct Buf {
        words: Vec<u32>,
        /// The address mask a smaller part would apply: the upper half
        /// aliases onto the lower.
        alias_mask: Option<usize>,
        /// One address whose reads lose a bit.
        drop_bit_at: Option<usize>,
        /// Writes ignored, as a window left read-only downgrades them.
        read_only: bool,
        flushes: u32,
    }

    impl Buf {
        fn new(bytes: usize) -> Buf {
            Buf { words: vec![0; bytes / 4], alias_mask: None, drop_bit_at: None, read_only: false, flushes: 0 }
        }
        fn index(&self, addr: usize) -> usize {
            let off = if addr >= UNCACHED { addr - UNCACHED } else { addr - BASE };
            let off = match self.alias_mask {
                Some(mask) => off & mask,
                None => off,
            };
            off / 4
        }
    }

    impl Memory for Buf {
        unsafe fn write(&mut self, addr: usize, v: u32) {
            if !self.read_only {
                let i = self.index(addr);
                self.words[i] = v;
            }
        }
        unsafe fn read(&mut self, addr: usize) -> u32 {
            let i = self.index(addr);
            let v = self.words[i];
            match self.drop_bit_at {
                Some(a) if a == addr || a + (UNCACHED - BASE) == addr => v & !(1 << 5),
                _ => v,
            }
        }
        unsafe fn flush(&mut self) {
            self.flushes += 1;
        }
    }

    fn window(bytes: usize) -> Window {
        Window {
            base: BASE,
            bytes,
            uncached_base: UNCACHED,
            id: Id::NONE,
            timing: WindowTiming {
                clkdiv: 2,
                rxdelay: 1,
                min_deselect: 3,
                max_select: 17,
                select_hold: 3,
                select_setup: false,
                pagebreak: 2,
                cooldown: 1,
            },
        }
    }

    #[test]
    fn the_stride_walk_holds_over_a_good_buffer_and_leaves_it_zero() {
        let w = window(64 * 1024);
        let mut b = Buf::new(w.bytes);
        assert_eq!(unsafe { stride_walk(&mut b, &w, 0xA5A5_0000) }, Ok(()));
        assert!(b.words.iter().all(|&v| v == 0), "the window is zero after the walk");
        assert_eq!(b.flushes, 2, "one flush before the writes, one after the zeros");
    }

    #[test]
    fn the_full_walk_holds_over_a_good_buffer_and_leaves_it_zero() {
        let w = window(16 * 1024);
        let mut b = Buf::new(w.bytes);
        assert_eq!(unsafe { full_walk(&mut b, &w, 0x5A5A_FFFF) }, Ok(()));
        assert!(b.words.iter().all(|&v| v == 0));
    }

    #[test]
    fn the_stride_walk_reaches_the_last_word_of_a_window() {
        let w = window(64 * 1024);
        let offs: Vec<usize> = stride_offsets(w.bytes).collect();
        assert_eq!(offs.len(), 65, "64 pages and the last word");
        assert_eq!(*offs.last().unwrap(), 64 * 1024 - 4);
    }

    #[test]
    fn a_smaller_part_aliasing_onto_itself_is_refused_by_the_stride_walk() {
        let w = window(64 * 1024);
        let mut b = Buf::new(w.bytes);
        b.alias_mask = Some(32 * 1024 - 1);
        let r = unsafe { stride_walk(&mut b, &w, 0) };
        let m = r.unwrap_err();
        assert_eq!(m.addr, BASE, "the first page's word was overwritten by the aliased upper half");
        assert_eq!(m.expected, BASE as u32);
        assert_eq!(m.read, (BASE + 32 * 1024) as u32);
        assert_eq!(m.view, View::Uncached);
        assert!(b.words.iter().all(|&v| v == 0), "restored to zero on refusal");
    }

    #[test]
    fn a_dropped_bit_is_refused_by_the_full_walk_at_its_address() {
        let w = window(8 * 1024);
        let mut b = Buf::new(w.bytes);
        let bad = BASE + 0x1234;
        b.drop_bit_at = Some(bad);
        // Under the address pattern the word's bit 5 is set at this address
        // (0x1234 ^ salt 0 has bit 5 set), so the loss shows at once.
        let m = unsafe { full_walk(&mut b, &w, 0) }.unwrap_err();
        assert_eq!(m.addr, bad);
        assert_eq!(m.read, m.expected & !(1 << 5));
        assert_eq!(m.pattern, Pattern::Address);
    }

    #[test]
    fn a_window_left_read_only_is_refused() {
        let w = window(16 * 1024);
        let mut b = Buf::new(w.bytes);
        b.read_only = true;
        let m = unsafe { stride_walk(&mut b, &w, 0x0F0F_0F0F) }.unwrap_err();
        assert_eq!(m.addr, BASE);
        assert_eq!(m.read, 0, "the downgraded write left the zero in place");
        assert_eq!(m.expected, (BASE as u32) ^ 0x0F0F_0F0F);
    }
}
