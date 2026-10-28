//! The console ring a Lamella image keeps in RAM: its layout, the words that say how the program
//! ended, and the writes that keep them.
//!
//! A debug probe reads the ring while the core runs, without halting it, so a program's output and
//! its ending reach a host over the debug port alone. The ring is one object at the symbol
//! [`SYMBOL`]: a header of four little-endian `u32` words, then the transcript. The words are, in
//! order, the magic, the transcript's size in bytes, the count of bytes written since boot, and the
//! exit code. Each console byte lands at `written mod size`, so a count past the size means the
//! oldest `written - size` bytes were overwritten before anyone read them.
//!
//! The magic says what the program is doing: [`RUNNING`] while it runs, [`RETURNED`] once its entry
//! point has returned, and [`ABORTED`] once it has ended without returning. Any other value means the
//! ring is not armed: this boot has not reached its first write yet, or no ring is at the address.
//! The exit code means something only beside an end magic. A returned program's code is its own,
//! whole; an aborted program's is one of the codes in [`abort`].
//!
//! The writes keep a reader's view whole. A byte lands before the count that covers it, and the exit
//! code lands before the end magic, with a barrier between, so a reader that sees an end magic sees
//! the code, the whole transcript and its count beside it.

#![no_std]
#![allow(
    unsafe_code,
    reason = "the ring is RAM a debug probe reads while the program runs, so every write is a volatile store through a raw pointer"
)]

use core::ptr::{read_volatile, write_volatile};
use core::sync::atomic::{Ordering, fence};

/// The symbol an image's console ring is defined under.
pub const SYMBOL: &str = "__lamella_console_log";

/// The magic while the program runs: "RLOG" in a byte dump.
pub const RUNNING: u32 = 0x474F_4C52;

/// The magic once the program's entry point has returned: "RRET" in a byte dump.
pub const RETURNED: u32 = 0x5445_5252;

/// The magic once the program has ended without returning: "RABT" in a byte dump.
pub const ABORTED: u32 = 0x5442_4152;

/// The header's size in bytes: the four words ahead of the transcript.
pub const HEADER_BYTES: u32 = 16;

/// The magic's offset in the header.
pub const MAGIC_OFFSET: u32 = 0;

/// The offset of the transcript's size in bytes.
pub const SIZE_OFFSET: u32 = 4;

/// The offset of the count of bytes written since boot.
pub const WRITTEN_OFFSET: u32 = 8;

/// The offset of the exit code.
pub const EXIT_CODE_OFFSET: u32 = 12;

/// The codes an aborted program's ring carries, one per way a program can end without returning.
pub mod abort {
    /// The program ended on an exception nothing caught, or on a fail-fast.
    pub const UNCAUGHT_EXCEPTION: i32 = 134;
    /// The processor faulted.
    pub const FAULT: i32 = 2;
    /// The runtime stopped the program: its heap was exhausted, its threads deadlocked, or it used a
    /// lock in a way the runtime cannot honor.
    pub const FATAL_STOP: i32 = 1;
}

/// A console ring with a transcript of `N` bytes, laid out as the header and the transcript, in that
/// order.
///
/// An image defines one, all zeros, at [`SYMBOL`]: zero-filled RAM holds it unarmed until the
/// startup calls [`Ring::arm`]. Every other method writes through a raw pointer to it, because the
/// ring is shared with a reader outside the program and is never borrowed.
#[repr(C)]
pub struct Ring<const N: usize> {
    magic: u32,
    size: u32,
    written: u32,
    exit_code: i32,
    bytes: [u8; N],
}

impl<const N: usize> Ring<N> {
    /// The transcript's size as the header records it.
    const SIZE: u32 = {
        assert!(N > 0, "a console ring needs a transcript of at least one byte");
        assert!(N <= u32::MAX as usize, "a console ring's size is one header word");
        N as u32
    };

    /// A ring of zeros, which is unarmed and which a static lays in zero-filled RAM.
    #[must_use]
    #[inline(always)]
    pub const fn new() -> Self {
        Self {
            magic: 0,
            size: 0,
            written: 0,
            exit_code: 0,
            bytes: [0; N],
        }
    }

    /// Arms the ring for this boot: it clears the magic first, writes the size, a count of 0 and an
    /// exit code of 0, then [`RUNNING`] last, so a reader that sees [`RUNNING`] sees this boot's
    /// words beside it.
    ///
    /// # Safety
    /// `ring` points to a `Ring<N>` that nothing else writes while this runs.
    #[inline(always)]
    pub unsafe fn arm(ring: *mut Self) {
        unsafe {
            write_volatile(&raw mut (*ring).magic, 0);
            write_volatile(&raw mut (*ring).size, Self::SIZE);
            write_volatile(&raw mut (*ring).written, 0);
            write_volatile(&raw mut (*ring).exit_code, 0);
            fence(Ordering::SeqCst);
            write_volatile(&raw mut (*ring).magic, RUNNING);
        }
    }

    /// Appends one console byte: the byte lands at `written mod size`, and then the count moves, so
    /// a reader never sees a count that covers a byte not yet written.
    ///
    /// # Safety
    /// `ring` points to a `Ring<N>` that nothing else writes while this runs.
    #[inline(always)]
    pub unsafe fn put(ring: *mut Self, byte: u8) {
        unsafe {
            let written = read_volatile(&raw const (*ring).written);
            let slot = (written % Self::SIZE) as usize;
            write_volatile((&raw mut (*ring).bytes).cast::<u8>().add(slot), byte);
            write_volatile(&raw mut (*ring).written, written.wrapping_add(1));
        }
    }

    /// Ends the run: the exit code first, then a barrier, then `magic` in one store, so a reader that
    /// sees the end magic sees the code and every byte before it. `magic` is [`RETURNED`] or
    /// [`ABORTED`].
    ///
    /// # Safety
    /// `ring` points to a `Ring<N>` that nothing else writes while this runs.
    #[inline(always)]
    pub unsafe fn end(ring: *mut Self, magic: u32, code: i32) {
        unsafe {
            write_volatile(&raw mut (*ring).exit_code, code);
            fence(Ordering::SeqCst);
            write_volatile(&raw mut (*ring).magic, magic);
        }
    }

    /// Ends the run when the program's entry point has come back with `code`: [`RETURNED`] and that
    /// code, or, when an exception was still in flight as it came back (`exception_in_flight` is
    /// nonzero), [`ABORTED`] and [`abort::UNCAUGHT_EXCEPTION`]. Returns the code it recorded.
    ///
    /// # Safety
    /// `ring` points to a `Ring<N>` that nothing else writes while this runs.
    #[inline(always)]
    pub unsafe fn finish(ring: *mut Self, code: i32, exception_in_flight: u32) -> i32 {
        let (magic, code) = if exception_in_flight == 0 {
            (RETURNED, code)
        } else {
            (ABORTED, abort::UNCAUGHT_EXCEPTION)
        };
        unsafe { Self::end(ring, magic, code) };
        code
    }
}

impl<const N: usize> Default for Ring<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four header words of `ring`, read as a probe reads them: little-endian words at the
    /// header's offsets.
    fn header<const N: usize>(ring: &Ring<N>) -> [u32; 4] {
        let base = core::ptr::from_ref(ring).cast::<u8>();
        let word = |offset: u32| unsafe {
            u32::from_le_bytes(core::ptr::read(base.add(offset as usize).cast::<[u8; 4]>()))
        };
        [word(MAGIC_OFFSET), word(SIZE_OFFSET), word(WRITTEN_OFFSET), word(EXIT_CODE_OFFSET)]
    }

    /// The transcript bytes of `ring`, read at the header's end.
    fn transcript<const N: usize>(ring: &Ring<N>) -> [u8; N] {
        let base = core::ptr::from_ref(ring).cast::<u8>();
        unsafe { core::ptr::read(base.add(HEADER_BYTES as usize).cast::<[u8; N]>()) }
    }

    #[test]
    fn the_magic_values_spell_their_names_in_a_byte_dump() {
        assert_eq!(RUNNING.to_le_bytes(), *b"RLOG");
        assert_eq!(RETURNED.to_le_bytes(), *b"RRET");
        assert_eq!(ABORTED.to_le_bytes(), *b"RABT");
    }

    #[test]
    fn the_header_is_four_words_ahead_of_the_transcript() {
        assert_eq!(core::mem::size_of::<Ring<8>>(), HEADER_BYTES as usize + 8);
        assert_eq!(core::mem::size_of::<Ring<16384>>(), HEADER_BYTES as usize + 16384);
    }

    /// A ring left by an earlier run reads as this boot's only once it is armed: the arm writes the
    /// size, a count of 0 and a code of 0 under [`RUNNING`].
    #[test]
    fn arming_writes_this_boots_words_under_the_running_magic() {
        let mut ring = Ring::<8>::new();
        unsafe {
            Ring::put(&raw mut ring, b'x');
            Ring::end(&raw mut ring, RETURNED, 42);
        }
        unsafe { Ring::arm(&raw mut ring) };
        assert_eq!(header(&ring), [RUNNING, 8, 0, 0]);
    }

    #[test]
    fn each_byte_lands_at_the_count_and_the_count_moves_past_it() {
        let mut ring = Ring::<8>::new();
        unsafe {
            Ring::arm(&raw mut ring);
            for byte in *b"hi\n" {
                Ring::put(&raw mut ring, byte);
            }
        }
        assert_eq!(header(&ring), [RUNNING, 8, 3, 0]);
        assert_eq!(&transcript(&ring)[..3], b"hi\n");
    }

    /// Past the size, a byte overwrites the oldest one, and the count keeps counting, so a reader
    /// knows how many were lost.
    #[test]
    fn a_full_transcript_wraps_and_the_count_says_how_many_bytes_were_lost() {
        let mut ring = Ring::<4>::new();
        unsafe {
            Ring::arm(&raw mut ring);
            for byte in *b"abcdef" {
                Ring::put(&raw mut ring, byte);
            }
        }
        assert_eq!(header(&ring), [RUNNING, 4, 6, 0]);
        assert_eq!(transcript(&ring), *b"efcd");
    }

    #[test]
    fn a_return_ends_the_ring_with_the_programs_own_code_whole() {
        let mut ring = Ring::<8>::new();
        unsafe {
            Ring::arm(&raw mut ring);
            assert_eq!(Ring::finish(&raw mut ring, 70_000, 0), 70_000, "the code it recorded");
        }
        assert_eq!(header(&ring), [RETURNED, 8, 0, 70_000]);

        let mut negative = Ring::<8>::new();
        unsafe {
            Ring::arm(&raw mut negative);
            Ring::finish(&raw mut negative, -1, 0);
        }
        assert_eq!(header(&negative), [RETURNED, 8, 0, u32::MAX]);
    }

    /// The entry point's own result means nothing when an exception was still in flight as it came
    /// back: the program did not return, it was ended.
    #[test]
    fn an_exception_still_in_flight_ends_the_ring_as_aborted_134() {
        let mut ring = Ring::<8>::new();
        unsafe {
            Ring::arm(&raw mut ring);
            assert_eq!(Ring::finish(&raw mut ring, 0, 0x1234_5678), 134, "the code it recorded");
        }
        assert_eq!(header(&ring), [ABORTED, 8, 0, 134]);
    }

    #[test]
    fn an_abort_carries_the_code_it_is_given() {
        let mut ring = Ring::<8>::new();
        unsafe {
            Ring::arm(&raw mut ring);
            Ring::put(&raw mut ring, b'!');
            Ring::end(&raw mut ring, ABORTED, abort::FATAL_STOP);
        }
        assert_eq!(header(&ring), [ABORTED, 8, 1, 1]);
    }
}
