//! The digest this protocol verifies a flash write with.

use alloc::vec::Vec;

/// The digest's size in bytes.
pub const DIGEST_LEN: usize = 16;

/// The block size the compression function consumes.
const BLOCK: usize = 64;

/// The per-operation left-rotation amounts: four rounds of four values, each repeated four times.
const SHIFTS: [u32; 64] = [
    7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22,
    5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20,
    4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23,
    6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
];

/// The per-operation additive constants, as published.
///
/// Each is the integer part of `2^32 * abs(sin(i + 1))` with `i` in radians -- so they could be
/// derived rather than transcribed, but deriving them needs a sine function this crate has no
/// dependency to supply. Transcribed instead, and the published test vectors are what prove the
/// transcription: a single wrong digit here changes the digest of a one-byte input.
const K: [u32; 64] = [
    0xd76a_a478, 0xe8c7_b756, 0x2420_70db, 0xc1bd_ceee,
    0xf57c_0faf, 0x4787_c62a, 0xa830_4613, 0xfd46_9501,
    0x6980_98d8, 0x8b44_f7af, 0xffff_5bb1, 0x895c_d7be,
    0x6b90_1122, 0xfd98_7193, 0xa679_438e, 0x49b4_0821,
    0xf61e_2562, 0xc040_b340, 0x265e_5a51, 0xe9b6_c7aa,
    0xd62f_105d, 0x0244_1453, 0xd8a1_e681, 0xe7d3_fbc8,
    0x21e1_cde6, 0xc337_07d6, 0xf4d5_0d87, 0x455a_14ed,
    0xa9e3_e905, 0xfcef_a3f8, 0x676f_02d9, 0x8d2a_4c8a,
    0xfffa_3942, 0x8771_f681, 0x6d9d_6122, 0xfde5_380c,
    0xa4be_ea44, 0x4bde_cfa9, 0xf6bb_4b60, 0xbebf_bc70,
    0x289b_7ec6, 0xeaa1_27fa, 0xd4ef_3085, 0x0488_1d05,
    0xd9d4_d039, 0xe6db_99e5, 0x1fa2_7cf8, 0xc4ac_5665,
    0xf429_2244, 0x432a_ff97, 0xab94_23a7, 0xfc93_a039,
    0x655b_59c3, 0x8f0c_cc92, 0xffef_f47d, 0x8584_5dd1,
    0x6fa8_7e4f, 0xfe2c_e6e0, 0xa301_4314, 0x4e08_11a1,
    0xf753_7e82, 0xbd3a_f235, 0x2ad7_d2bb, 0xeb86_d391,
];

/// The digest of `data`, in the byte order the protocol's response carries.
#[must_use]
pub fn md5(data: &[u8]) -> [u8; DIGEST_LEN] {
    let mut state = Md5::new();
    state.update(data);
    state.finish()
}

/// The digest of `data` as lowercase hexadecimal, which is the form the target's response carries.
///
/// **The target answers this command with ASCII, not with the sixteen raw bytes.** A caller
/// comparing raw digest bytes against that response finds them unequal for a reason having nothing
/// to do with flash -- so the comparison this crate performs is against this form, and the function
/// exists so that fact lives in one place rather than at every comparison site.
#[must_use]
pub fn md5_hex(data: &[u8]) -> Vec<u8> {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = Vec::with_capacity(DIGEST_LEN * 2);
    for byte in md5(data) {
        out.push(HEX[usize::from(byte >> 4)]);
        out.push(HEX[usize::from(byte & 0x0F)]);
    }
    out
}

/// Accumulating form, for data that does not arrive at once.
///
/// The one-shot [`md5`] covers this crate's own use (the caller holds the whole image). This exists
/// because the padding rule is the part of the algorithm that is easy to get wrong at a block
/// boundary, and only an accumulating form lets a test feed the same input in every chunking and
/// require one answer.
#[derive(Debug, Clone)]
pub struct Md5 {
    /// The four state words.
    state: [u32; 4],
    /// Bytes not yet consumed as a whole block.
    buffered: Vec<u8>,
    /// The message length in bytes, which the padding encodes as a bit count.
    length: u64,
}

impl Default for Md5 {
    fn default() -> Md5 {
        Md5::new()
    }
}

impl Md5 {
    /// A fresh state, at the published initial values.
    #[must_use]
    pub fn new() -> Md5 {
        Md5 {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            buffered: Vec::new(),
            length: 0,
        }
    }

    /// Absorbs more of the message.
    pub fn update(&mut self, data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);
        self.buffered.extend_from_slice(data);
        let mut consumed = 0;
        while self.buffered.len() - consumed >= BLOCK {
            let block: [u8; BLOCK] = self.buffered[consumed..consumed + BLOCK]
                .try_into()
                .expect("a BLOCK-sized window is BLOCK bytes");
            compress(&mut self.state, &block);
            consumed += BLOCK;
        }
        self.buffered.drain(..consumed);
    }

    /// Pads and returns the digest.
    #[must_use]
    pub fn finish(mut self) -> [u8; DIGEST_LEN] {
        let bits = self.length.wrapping_mul(8);
        let mut padding = alloc::vec![0x80u8];
        while (self.length as usize + padding.len()) % BLOCK != 56 {
            padding.push(0);
        }
        padding.extend_from_slice(&bits.to_le_bytes());
        let padded_from = self.length;
        self.update(&padding);
        debug_assert_eq!(
            self.buffered.len(),
            0,
            "padding must land exactly on a block boundary (message was {padded_from} bytes)"
        );
        let mut out = [0u8; DIGEST_LEN];
        for (word, chunk) in self.state.iter().zip(out.chunks_mut(4)) {
            chunk.copy_from_slice(&word.to_le_bytes());
        }
        out
    }
}

/// One block through the compression function.
fn compress(state: &mut [u32; 4], block: &[u8; BLOCK]) {
    let mut words = [0u32; 16];
    for (word, chunk) in words.iter_mut().zip(block.chunks_exact(4)) {
        *word = u32::from_le_bytes(chunk.try_into().expect("a 4-byte chunk is 4 bytes"));
    }
    let [mut a, mut b, mut c, mut d] = *state;
    for step in 0..64 {
        let (mixed, index) = match step / 16 {
            0 => ((b & c) | (!b & d), step),
            1 => ((d & b) | (!d & c), (5 * step + 1) % 16),
            2 => (b ^ c ^ d, (3 * step + 5) % 16),
            _ => (c ^ (b | !d), (7 * step) % 16),
        };
        let sum = a
            .wrapping_add(mixed)
            .wrapping_add(K[step])
            .wrapping_add(words[index]);
        a = d;
        d = c;
        c = b;
        b = b.wrapping_add(sum.rotate_left(SHIFTS[step]));
    }
    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The specification's own test suite: seven inputs and their published digests.
    ///
    /// **This is the oracle for sixty-four transcribed constants, four shift schedules, two derived
    /// index formulas and a padding rule.** Any single one of them wrong changes at least one of
    /// these digests, so the suite passing is what makes the transcription a fact rather than a
    /// hope. The inputs are chosen by the specification to span the cases that matter: empty, one
    /// byte, under one block, and -- the last one -- longer than one block.
    #[test]
    fn the_published_test_suite_passes() {
        for (input, expected) in [
            ("", "d41d8cd98f00b204e9800998ecf8427e"),
            ("a", "0cc175b9c0f1b6a831c399e269772661"),
            ("abc", "900150983cd24fb0d6963f7d28e17f72"),
            ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
            ("abcdefghijklmnopqrstuvwxyz", "c3fcd3d76192e4007dfb496cca67e13b"),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "d174ab98d277d9f5a5611c2c9f419d9f",
            ),
            (
                "1234567890123456789012345678901234567890\
                 1234567890123456789012345678901234567890",
                "57edf4a22be3c955ac49da2e2107b67a",
            ),
        ] {
            let got = md5_hex(input.as_bytes());
            assert_eq!(
                core::str::from_utf8(&got).expect("hex is ASCII"),
                expected,
                "digest of {input:?}"
            );
        }
    }

    /// **The lengths where a padding rule breaks, and none of them has a published digest.** The
    /// padding must reach a residue of 56 within a block, so 55 bytes pads into the same block, 56
    /// and 57 push into a second one, and 64 is exactly full. Rather than invent expected digests,
    /// this asserts the property that the rule is FOR: the padded message is a whole number of
    /// blocks, which is what `finish`'s own debug assertion checks and what a wrong residue breaks.
    #[test]
    fn every_length_around_a_block_boundary_pads_to_a_whole_block() {
        for length in 0..(BLOCK * 3) {
            let mut state = Md5::new();
            state.update(&alloc::vec![0x5A; length]);
            assert_eq!(state.finish().len(), DIGEST_LEN, "length {length}");
        }
    }

    /// **A digest must not depend on how the data was handed over.** This is the property the
    /// accumulating form exists to be tested against: the buffer carries a partial block across an
    /// `update`, and forgetting that is a defect visible only at particular chunk sizes -- the same
    /// shape as the escape pair straddling a read in the frame reader.
    #[test]
    fn chunking_the_input_cannot_change_the_digest() {
        let data: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
        let whole = md5(&data);
        for chunk in 1..=data.len() {
            let mut state = Md5::new();
            for piece in data.chunks(chunk) {
                state.update(piece);
            }
            assert_eq!(state.finish(), whole, "fed {chunk} bytes at a time");
        }
    }

    /// The hexadecimal form is lowercase and twice the digest's length -- the form the target sends,
    /// so a comparison against the response is byte-for-byte.
    #[test]
    fn the_hex_form_is_lowercase_ascii_of_twice_the_length() {
        let hex = md5_hex(b"abc");
        assert_eq!(hex.len(), DIGEST_LEN * 2);
        assert!(hex.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()));
    }
}
