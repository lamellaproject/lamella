//! A four-byte hardware seed for the radio's retry spread, or no seed when entropy is unavailable.

use crate::{rp2350_instances as chip, rp2350_trng};
use rp2350_trng::Registers;

/// Reads one hardware word. A failed reset, entropy check or deadline gives no seed.
pub fn read(registers: &mut impl Registers, now_ms: &mut dyn FnMut() -> u64) -> Option<u32> {
    if !rp2350_trng::start(
        registers,
        chip::RESETS_BASE as usize,
        chip::RESETS_CLR_BASE as usize,
        chip::TRNG_RESET_MASK,
        chip::TRNG_BASE as usize,
    ) {
        return None;
    }
    let mut bytes = [0u8; 4];
    rp2350_trng::fill(registers, chip::TRNG_BASE as usize, &mut bytes, now_ms)
        .then(|| u32::from_le_bytes(bytes))
}
