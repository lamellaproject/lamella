//! The checksum `FW_RESULT` and `FW_COMMIT_RESULT` carry, named rather than assumed.

/// CRC-32/ISO-HDLC's polynomial, 0x04C11DB7, bit-reflected for a right-shifting loop.
const REFLECTED_POLYNOMIAL: u32 = 0xEDB8_8320;

/// Bit-reflected CRC-32/ISO-HDLC, computed a byte at a time with no table.
///
/// **NO 1 KB TABLE, DELIBERATELY.** This runs on parts whose whole SRAM is 2 KB, and the bytes it
/// folds arrive at flash-programming speed rather than at line rate -- so a table would cost a
/// quarter of the smallest target's memory to save time nothing is waiting on.
#[must_use]
pub fn update(seed: u32, data: &[u8]) -> u32 {
    let mut crc = !seed;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            let lsb = crc & 1;
            crc >>= 1;
            if lsb != 0 {
                crc ^= REFLECTED_POLYNOMIAL;
            }
        }
    }
    !crc
}

/// The CRC of `data` alone.
#[must_use]
pub fn of(data: &[u8]) -> u32 {
    update(0, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_published_check_value_holds() {
        assert_eq!(of(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn it_is_not_one_of_the_variants_it_could_have_been_mistaken_for() {
        let ours = of(b"123456789");
        assert_ne!(ours, 0xFC89_1918, "that is CRC-32/BZIP2");
        assert_ne!(ours, 0x0376_E6E7, "that is CRC-32/MPEG-2, which this tree also uses elsewhere");
        assert_ne!(ours, 0x340B_C6D9, "that is CRC-32/JAMCRC");
        assert_ne!(ours, 0x765E_7680, "that is CRC-32/POSIX");
        assert_ne!(ours, 0xBD0B_E338, "that is CRC-32/XFER");
    }

    #[test]
    fn folding_in_pieces_equals_folding_at_once() {
        let whole: [u8; 12] = *b"the quick br";
        let one_pass = of(&whole);
        for split in 0..whole.len() {
            let mut running = update(0, &whole[..split]);
            running = update(running, &whole[split..]);
            assert_eq!(running, one_pass, "split at {split} disagreed with one pass");
        }
    }

    #[test]
    fn an_empty_input_leaves_the_seed_alone() {
        assert_eq!(update(0, &[]), 0);
        assert_eq!(update(0xDEAD_BEEF, &[]), 0xDEAD_BEEF);
    }
}
