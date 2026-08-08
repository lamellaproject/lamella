#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! The code-point byte codec: ONE definition of the bytes our string tiers store.

extern crate alloc;

use alloc::vec::Vec;

/// The replacement character, substituted for a byte sequence this codec could not have produced.
pub const REPLACEMENT: u32 = 0xFFFD;

/// Appends `code` to `out` in UTF-8's byte form, WITHOUT judging it.
///
/// A surrogate code point (`U+D800..=U+DFFF`) gets its 3-byte pattern like any other value in that
/// range -- `U+D800` becomes `ED A0 80` -- which is ill-formed UTF-8 on purpose and is the whole
/// point of the WTF-8 tiers. A caller that must REFUSE lone surrogates checks before calling; a
/// caller that must preserve them simply calls.
pub fn push_code_point(code: u32, out: &mut Vec<u8>) {
    if code < 0x80 {
        out.push(code as u8);
    } else if code < 0x800 {
        out.push(0xC0 | (code >> 6) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    } else if code < 0x1_0000 {
        out.push(0xE0 | (code >> 12) as u8);
        out.push(0x80 | ((code >> 6) & 0x3F) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    } else {
        out.push(0xF0 | (code >> 18) as u8);
        out.push(0x80 | ((code >> 12) & 0x3F) as u8);
        out.push(0x80 | ((code >> 6) & 0x3F) as u8);
        out.push(0x80 | (code & 0x3F) as u8);
    }
}

/// Reads the code point starting at `at`, returning it and how many bytes it consumed.
///
/// `core::str::from_utf8` CANNOT stand in for this: it rejects the encoded-surrogate form these
/// tiers exist to store, so routing through `&str` would turn every lone surrogate into an error and
/// lose exactly the data being preserved.
///
/// MALFORMED INPUT RESYNCHRONIZES rather than failing. The bytes are produced by
/// [`push_code_point`], so their shape is an invariant rather than untrusted input -- but a
/// corrupted heap must not become an unbounded loop or a panic, so an impossible lead byte or a
/// truncated tail yields [`REPLACEMENT`] and advances. The width returned is always at least 1, which
/// is what guarantees a caller's loop terminates.
///
/// Returns `None` only when `at` is past the end.
#[must_use]
pub fn next_code_point(bytes: &[u8], at: usize) -> Option<(u32, usize)> {
    let lead = *bytes.get(at)?;
    let (mut code, width) = if lead < 0x80 {
        (u32::from(lead), 1)
    } else if lead & 0xE0 == 0xC0 {
        (u32::from(lead & 0x1F), 2)
    } else if lead & 0xF0 == 0xE0 {
        (u32::from(lead & 0x0F), 3)
    } else if lead & 0xF8 == 0xF0 {
        (u32::from(lead & 0x07), 4)
    } else {
        return Some((REPLACEMENT, 1));
    };
    if at + width > bytes.len() {
        return Some((REPLACEMENT, bytes.len() - at));
    }
    for continuation in &bytes[at + 1..at + width] {
        code = (code << 6) | u32::from(continuation & 0x3F);
    }
    Some((code, width))
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// One code point round-trips at every width, surrogates included.
    #[test]
    fn every_width_round_trips_and_a_surrogate_is_not_special() {
        for code in [0x00, 0x41, 0x7F, 0x80, 0x7FF, 0x800, 0xD800, 0xDFFF, 0xFFFF, 0x1_0000, 0x10_FFFF] {
            let mut bytes = Vec::new();
            push_code_point(code, &mut bytes);
            assert_eq!(
                next_code_point(&bytes, 0),
                Some((code, bytes.len())),
                "U+{code:04X} did not survive the round trip"
            );
        }
    }

    /// The byte patterns are pinned against known values, not just against this codec's own inverse.
    ///
    /// A round trip alone would pass on two consistent misreadings -- the encoder and decoder are
    /// written together and would agree with each other. These are the forms other implementations
    /// produce, which is what makes them an oracle rather than a restatement.
    #[test]
    fn the_byte_forms_match_what_other_implementations_produce() {
        let mut out = Vec::new();
        push_code_point(0xD800, &mut out);
        assert_eq!(out, vec![0xED, 0xA0, 0x80], "the lone-surrogate form is the shared one");

        out.clear();
        push_code_point(0x1_0000, &mut out);
        assert_eq!(out, vec![0xF0, 0x90, 0x80, 0x80]);

        out.clear();
        push_code_point(0x20AC, &mut out);
        assert_eq!(out, vec![0xE2, 0x82, 0xAC]);
    }

    /// TWO ADJACENT SURROGATES STAY TWO CODE POINTS HERE -- the property Python needs and the one
    /// C#'s caller deliberately does not have.
    ///
    /// This codec has no opinion about pairing, so encoding them separately and reading them back
    /// yields two. A caller that WANTS the combined supplementary form (C#'s does) combines before
    /// calling; nothing here can do it accidentally.
    #[test]
    fn a_surrogate_pair_is_not_combined_by_the_codec() {
        let mut bytes = Vec::new();
        push_code_point(0xD800, &mut bytes);
        push_code_point(0xDC00, &mut bytes);
        assert_eq!(bytes, vec![0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80]);

        let (first, width) = next_code_point(&bytes, 0).expect("a first code point");
        assert_eq!((first, width), (0xD800, 3));
        assert_eq!(next_code_point(&bytes, width), Some((0xDC00, 3)));
    }

    /// Malformed bytes resynchronize instead of looping or panicking, and always advance.
    #[test]
    fn malformed_input_yields_replacement_and_always_advances() {
        assert_eq!(next_code_point(&[0x80], 0), Some((REPLACEMENT, 1)));
        assert_eq!(next_code_point(&[0xE2, 0x82], 0), Some((REPLACEMENT, 2)));
        assert_eq!(next_code_point(&[], 0), None);
        assert_eq!(next_code_point(&[0x41], 1), None);

        let junk = [0xFF, 0x80, 0xE2, 0xC0, 0x41, 0xF8];
        let (mut at, mut steps) = (0usize, 0usize);
        while let Some((_, width)) = next_code_point(&junk, at) {
            assert!(width >= 1, "a zero width would hang every caller");
            at += width;
            steps += 1;
            assert!(steps <= junk.len(), "the walk must not revisit bytes");
        }
        assert_eq!(at, junk.len(), "a walk must land exactly on the end");
    }
}
