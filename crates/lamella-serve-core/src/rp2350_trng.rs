//! The RP2350's true random number generator (datasheet 12.12) as an entropy source.
//!
//! Used the way the datasheet's 12.12.3 describes the block: every built-in entropy check left on, a
//! generation started through `RND_SOURCE_ENABLE`, and its 192 bits read from `EHR_DATA0..5` once
//! `RNG_ISR` reports them valid, `EHR_DATA5` last because reading it clears them all. A run that
//! fails a check presents no result, so it is started again; an autocorrelation failure stops the
//! generator until the block is reset, so it is reset first.
//!
//! A fill that cannot finish reports it rather than waiting forever. A generation's time is not
//! deterministic and can exceed 100 times its average (12.12.4), so each one gets [`PATIENCE_MS`]. A
//! consumer that is refused entropy, such as a TLS configuration, fails loudly instead of running on
//! weak keys.

#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_trng_layout.rs"]
mod layout;
#[allow(dead_code)]
#[path = "../../../csp/rp2350/rust/rp2350_resets_layout.rs"]
mod resets;

use layout::*;

/// The generator's registers by address, so the sequence runs against the part or against a
/// test's model of it.
pub trait Registers {
    /// Reads the 32-bit register at `address`.
    fn read(&mut self, address: usize) -> u32;
    /// Writes `value` to the 32-bit register at `address`.
    fn write(&mut self, address: usize, value: u32);
}

/// How long one generation may take before a fill gives up, in milliseconds: about 100 times the
/// average at [`SAMPLE_COUNT`], taking the datasheet's 2 ms at 20-25 as proportional.
pub const PATIENCE_MS: u64 = 2_000;

/// How many generations in a row may fail an entropy check before a fill gives up.
pub const ATTEMPTS: u32 = 8;

/// The system clock ticks between two samples of the ring oscillator. The register resets to 0xFFFF,
/// the slowest rate.
///
/// Not the datasheet's 20-25. Its 12.12.2 gives that range for an average generation of about
/// 2 ms, and adds that a low count raises the chance of failed entropy checks; the ring oscillator
/// differs from chip to chip. At clk_sys 150 MHz a Raspberry Pi Pico 2 W can pass every check at 25
/// while the RP2350B on a Pimoroni Pico Plus 2 W can fail the autocorrelation test within one to
/// three generations at counts from 19 to 64, and after twelve at 128; at 256 it gives ten valid
/// generations in ten.
pub const SAMPLE_COUNT: u32 = 256;

/// The bytes one generation yields.
const GENERATION_BYTES: usize = (EHR_WORDS * 4) as usize;

/// How many times [`start`] polls for the block to leave reset.
const RESET_POLLS: u32 = 100_000;

/// Takes the generator out of reset and configures it: the shortest oscillator chain, every entropy
/// check on, and [`SAMPLE_COUNT`]. Safe to call again.
///
/// `resets_base` and `resets_clr_base` are the RESETS block and its atomic-clear alias, `reset_mask`
/// the generator's bit in it, and `base` the generator itself. `false` when the block never
/// reports that it has left reset.
pub fn start(
    registers: &mut impl Registers,
    resets_base: usize,
    resets_clr_base: usize,
    reset_mask: u32,
    base: usize,
) -> bool {
    registers.write(resets_clr_base + resets::RESET_OFF as usize, reset_mask);
    let mut polls = 0;
    while registers.read(resets_base + resets::RESET_DONE_OFF as usize) & reset_mask == 0 {
        polls += 1;
        if polls >= RESET_POLLS {
            return false;
        }
    }
    configure(registers, base);
    true
}

/// Fills `buffer` from the generator, which [`start`] has configured. `now_ms` is a monotonic
/// millisecond clock, read to bound each generation by [`PATIENCE_MS`].
///
/// `false` when a generation outlasts its patience or [`ATTEMPTS`] runs in a row fail a check;
/// `buffer` is not to be used then. The source is switched off again either way, as 12.12.3 asks
/// of a generator that is not in use.
pub fn fill(registers: &mut impl Registers, base: usize, buffer: &mut [u8], now_ms: &mut dyn FnMut() -> u64) -> bool {
    let mut filled = true;
    for chunk in buffer.chunks_mut(GENERATION_BYTES) {
        let Some(words) = generate(registers, base, now_ms) else {
            filled = false;
            break;
        };
        let bytes = words.iter().flat_map(|word| word.to_le_bytes());
        for (slot, byte) in chunk.iter_mut().zip(bytes) {
            *slot = byte;
        }
    }
    registers.write(base + RND_SOURCE_ENABLE_OFF as usize, 0);
    filled
}

/// One generation's 192 bits, or `None` when none arrives.
fn generate(registers: &mut impl Registers, base: usize, now_ms: &mut dyn FnMut() -> u64) -> Option<[u32; EHR_WORDS as usize]> {
    for _ in 0..ATTEMPTS {
        registers.write(base + RND_SOURCE_ENABLE_OFF as usize, 0);
        registers.write(base + RNG_ICR_OFF as usize, RNG_ICR_EHR_VALID | RNG_ICR_CRNGT_ERR | RNG_ICR_VN_ERR);
        registers.write(base + RND_SOURCE_ENABLE_OFF as usize, RND_SOURCE_ENABLE_RND_SRC_EN);
        let deadline = now_ms().saturating_add(PATIENCE_MS);
        loop {
            let status = registers.read(base + RNG_ISR_OFF as usize);
            if status & RNG_ISR_EHR_VALID != 0 {
                let mut words = [0u32; EHR_WORDS as usize];
                for (index, word) in words.iter_mut().enumerate() {
                    *word = registers.read(base + (EHR_DATA0_OFF + index as u32 * EHR_STRIDE) as usize);
                }
                registers.write(base + RNG_ICR_OFF as usize, RNG_ICR_EHR_VALID);
                return Some(words);
            }
            if status & RNG_ISR_AUTOCORR_ERR != 0 {
                reset(registers, base);
                break;
            }
            if status & (RNG_ISR_CRNGT_ERR | RNG_ISR_VN_ERR) != 0 {
                break;
            }
            if now_ms() >= deadline {
                return None;
            }
        }
    }
    None
}

/// Resets the generator's internal state, which is the only thing that clears an autocorrelation
/// failure, and configures it again.
fn reset(registers: &mut impl Registers, base: usize) {
    registers.write(base + TRNG_SW_RESET_OFF as usize, TRNG_SW_RESET_TRNG_SW_RESET);
    let _ = registers.read(base + TRNG_SW_RESET_OFF as usize);
    let _ = registers.read(base + TRNG_SW_RESET_OFF as usize);
    configure(registers, base);
}

/// The configuration [`start`] describes.
fn configure(registers: &mut impl Registers, base: usize) {
    registers.write(base + RND_SOURCE_ENABLE_OFF as usize, 0);
    registers.write(base + TRNG_CONFIG_OFF as usize, 0);
    registers.write(base + TRNG_DEBUG_CONTROL_OFF as usize, 0);
    registers.write(base + SAMPLE_CNT1_OFF as usize, SAMPLE_COUNT);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    const BASE: usize = 0x400f_0000;

    /// The generator as a script: each run it is started for answers with the next outcome.
    struct Generator {
        outcomes: VecDeque<Outcome>,
        current: Option<Outcome>,
        started_runs: u32,
        soft_resets: u32,
        enabled: bool,
        result: [u32; 6],
        read_order: Vec<usize>,
    }

    #[derive(Clone, Copy)]
    enum Outcome {
        Valid,
        CheckFailed,
        AutocorrelationFailed,
        Silent,
    }

    impl Generator {
        fn new(outcomes: &[Outcome]) -> Self {
            Generator {
                outcomes: outcomes.iter().copied().collect(),
                current: None,
                started_runs: 0,
                soft_resets: 0,
                enabled: false,
                result: [0x0302_0100, 0x0706_0504, 0x0b0a_0908, 0x0f0e_0d0c, 0x1312_1110, 0x1716_1514],
                read_order: Vec::new(),
            }
        }
    }

    impl Registers for Generator {
        fn read(&mut self, address: usize) -> u32 {
            let offset = (address - BASE) as u32;
            if offset == RNG_ISR_OFF {
                return match self.current {
                    Some(Outcome::Valid) => RNG_ISR_EHR_VALID,
                    Some(Outcome::CheckFailed) => RNG_ISR_CRNGT_ERR,
                    Some(Outcome::AutocorrelationFailed) => RNG_ISR_AUTOCORR_ERR,
                    Some(Outcome::Silent) | None => 0,
                };
            }
            if (EHR_DATA0_OFF..EHR_DATA0_OFF + EHR_WORDS * EHR_STRIDE).contains(&offset) {
                let index = ((offset - EHR_DATA0_OFF) / EHR_STRIDE) as usize;
                self.read_order.push(index);
                return self.result[index];
            }
            0
        }

        fn write(&mut self, address: usize, value: u32) {
            let offset = (address - BASE) as u32;
            if offset == RND_SOURCE_ENABLE_OFF {
                let enabling = value & RND_SOURCE_ENABLE_RND_SRC_EN != 0;
                if enabling && !self.enabled {
                    self.started_runs += 1;
                    self.current = self.outcomes.pop_front();
                }
                self.enabled = enabling;
            } else if offset == TRNG_SW_RESET_OFF {
                self.soft_resets += 1;
            }
        }
    }

    fn clock() -> impl FnMut() -> u64 {
        let mut now = 0u64;
        move || {
            now += 1;
            now
        }
    }

    /// A valid generation fills the buffer from EHR_DATA0..5 in order, reading EHR_DATA5 last, and
    /// leaves the source off.
    #[test]
    fn a_valid_generation_fills_the_buffer_and_reads_the_last_word_last() {
        let mut generator = Generator::new(&[Outcome::Valid, Outcome::Valid]);
        let mut buffer = [0u8; 32];
        assert!(fill(&mut generator, BASE, &mut buffer, &mut clock()));
        let expected: Vec<u8> = (0u8..24).chain(0u8..8).collect();
        assert_eq!(buffer.to_vec(), expected, "24 bytes a generation, the second one cut to what was asked");
        assert_eq!(generator.read_order, vec![0, 1, 2, 3, 4, 5, 0, 1, 2, 3, 4, 5]);
        assert!(!generator.enabled, "the source is off once the fill is done");
    }

    /// A run that fails a check presents nothing and is started again; an autocorrelation failure
    /// resets the block first, because nothing else clears it.
    #[test]
    fn a_failed_check_starts_another_run_and_an_autocorrelation_failure_resets_the_block_first() {
        let mut generator = Generator::new(&[Outcome::CheckFailed, Outcome::AutocorrelationFailed, Outcome::Valid]);
        let mut buffer = [0u8; 24];
        assert!(fill(&mut generator, BASE, &mut buffer, &mut clock()));
        assert_eq!(generator.started_runs, 3);
        assert_eq!(generator.soft_resets, 1);
        assert_eq!(buffer.to_vec(), (0u8..24).collect::<Vec<u8>>());
    }

    /// Starting releases the generator from reset through the atomic-clear alias, so no other block's
    /// reset bit is touched, waits for it to leave reset, and configures it with every check on. A
    /// block that never leaves reset is reported.
    #[test]
    fn start_releases_only_the_generator_from_reset_and_configures_it() {
        const RESETS: usize = 0x4002_0000;
        const RESETS_CLR: usize = 0x4002_3000;
        const MASK: u32 = 1 << 25;
        struct Part {
            writes: Vec<(usize, u32)>,
            leaves_reset: bool,
        }
        impl Registers for Part {
            fn read(&mut self, address: usize) -> u32 {
                let done = address == RESETS + resets::RESET_DONE_OFF as usize && self.leaves_reset;
                if done { MASK } else { 0 }
            }
            fn write(&mut self, address: usize, value: u32) {
                self.writes.push((address, value));
            }
        }

        let mut part = Part { writes: Vec::new(), leaves_reset: true };
        assert!(start(&mut part, RESETS, RESETS_CLR, MASK, BASE));
        assert_eq!(part.writes[0], (RESETS_CLR, MASK));
        assert!(part.writes.contains(&(BASE + SAMPLE_CNT1_OFF as usize, SAMPLE_COUNT)));
        assert!(part.writes.contains(&(BASE + TRNG_DEBUG_CONTROL_OFF as usize, 0)), "no check bypassed");

        let mut stuck = Part { writes: Vec::new(), leaves_reset: false };
        assert!(!start(&mut stuck, RESETS, RESETS_CLR, MASK, BASE));
    }

    /// A generator that never answers, or fails every run, is reported as giving no entropy rather
    /// than waited on forever, and the source is left off.
    #[test]
    fn a_generator_that_never_delivers_is_refused_not_waited_on() {
        let mut silent = Generator::new(&[Outcome::Silent]);
        assert!(!fill(&mut silent, BASE, &mut [0u8; 8], &mut clock()));
        assert!(!silent.enabled);

        let mut failing = Generator::new(&[Outcome::CheckFailed; ATTEMPTS as usize]);
        assert!(!fill(&mut failing, BASE, &mut [0u8; 8], &mut clock()));
        assert_eq!(failing.started_runs, ATTEMPTS);
        assert!(!failing.enabled);
    }
}
