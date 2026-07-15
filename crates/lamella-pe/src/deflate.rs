//! A minimal RFC 1951 DEFLATE encoder that emits only STORED (uncompressed) blocks: a
//! spec-valid raw-deflate stream any inflater decompresses back to the input, with zero
//! dependencies and no `no_std` cost beyond `alloc`. It wraps the Portable PDB in the
//! `EmbeddedPortablePdb` debug directory entry (PE-COFF type 17: `"MPDB"` + the uncompressed
//! size + a raw-deflate stream). Stored blocks trade the optional size win for a tiny encoder
//! with no supply-chain surface; a compressing tier can replace this without touching callers.

use alloc::vec::Vec;

/// The most bytes a single stored block's `LEN` field (a `u16`) can carry.
const MAX_BLOCK: usize = 0xFFFF;

/// Encodes `data` as a raw-DEFLATE (RFC 1951, section 3.2.4) stream of STORED blocks. For each
/// chunk of up to 65535 bytes: a one-byte header (`BFINAL` in bit 0, `BTYPE` = 00 in bits 1..3,
/// the remaining bits padding to the byte boundary), then `LEN` and `~LEN` as little-endian
/// `u16`s, then the literal bytes. The final chunk sets `BFINAL`. Empty input yields a single
/// final empty block, which inflates to nothing.
#[must_use]
pub fn deflate_store(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 5 * (data.len() / MAX_BLOCK + 1));
    let mut offset = 0;
    loop {
        let len = (data.len() - offset).min(MAX_BLOCK);
        let is_final = offset + len == data.len();
        out.push(u8::from(is_final));
        let len16 = len as u16;
        out.extend_from_slice(&len16.to_le_bytes());
        out.extend_from_slice(&(!len16).to_le_bytes());
        out.extend_from_slice(&data[offset..offset + len]);
        offset += len;
        if is_final {
            return out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A STORED-block-only inflater, to prove the encoding round-trips (and is a conformant
    /// stream: `NLEN` is `~LEN`, exactly one final block, no trailing bytes).
    fn inflate_stored(stream: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut rest = stream;
        loop {
            let is_final = rest[0] & 1 == 1;
            assert_eq!(rest[0] & 0b110, 0, "BTYPE is 00 (stored)");
            let len = u16::from_le_bytes([rest[1], rest[2]]);
            let nlen = u16::from_le_bytes([rest[3], rest[4]]);
            assert_eq!(nlen, !len, "NLEN must be ~LEN");
            out.extend_from_slice(&rest[5..5 + len as usize]);
            rest = &rest[5 + len as usize..];
            if is_final {
                assert!(rest.is_empty(), "no bytes past the final block");
                return out;
            }
        }
    }

    #[test]
    fn stored_blocks_round_trip_empty_small_and_multi_block() {
        for data in [Vec::new(), b"a portable pdb".to_vec(), vec![0xA5u8; 200_000]] {
            let encoded = deflate_store(&data);
            assert_eq!(inflate_stored(&encoded), data, "round-trips for len {}", data.len());
        }
    }

    #[test]
    fn a_block_boundary_length_stays_conformant() {
        for len in [MAX_BLOCK, MAX_BLOCK + 1, 2 * MAX_BLOCK] {
            let data = vec![0x3Cu8; len];
            assert_eq!(inflate_stored(&deflate_store(&data)), data, "len {len}");
        }
    }
}
