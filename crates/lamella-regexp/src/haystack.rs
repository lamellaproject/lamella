//! The subject being searched, behind a trait, because the languages disagree about what a
//! position is.

/// A subject the matcher can read forwards and backwards.
///
/// Positions are in the unit the CONSUMER counts in, which for both ECMAScript and the CLI is the
/// UTF-16 code unit. The matcher never manufactures a position; it only ever adds a width this
/// trait reported.
pub trait Haystack {
    /// The number of positions in the subject. A position equal to this is the end, and is valid.
    fn len(&self) -> usize;

    /// Reads the character starting at `index`, with the number of positions it occupies.
    ///
    /// Answers `None` exactly at and beyond the end, so a caller can use it as the bounds check.
    fn at(&self, index: usize) -> Option<(u32, usize)>;

    /// Reads the character ENDING at `index`, with the number of positions it occupies.
    ///
    /// Answers `None` at position zero. The width is reported so the caller can subtract it and
    /// arrive at a position this trait would agree is a character boundary.
    fn before(&self, index: usize) -> Option<(u32, usize)>;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// UTF-16 code units read one at a time, surrogates included and never paired.
///
/// This is ECMAScript matching WITHOUT the `u` flag, and the lack of pairing is the specified
/// behavior rather than a simplification: outside `u` mode a surrogate pair is two characters, so
/// `/^.$/` does not match an astral character and `/./` matches half of one.
pub struct CodeUnitInput<'a> {
    units: &'a [u16],
}

impl<'a> CodeUnitInput<'a> {
    #[must_use]
    pub fn new(units: &'a [u16]) -> Self {
        Self { units }
    }
}

impl Haystack for CodeUnitInput<'_> {
    fn len(&self) -> usize {
        self.units.len()
    }

    fn at(&self, index: usize) -> Option<(u32, usize)> {
        self.units.get(index).map(|&unit| (u32::from(unit), 1))
    }

    fn before(&self, index: usize) -> Option<(u32, usize)> {
        if index == 0 {
            return None;
        }
        self.units.get(index - 1).map(|&unit| (u32::from(unit), 1))
    }
}

/// UTF-16 code units with surrogate pairs combined, still INDEXED by code unit.
///
/// This is ECMAScript matching under the `u` flag. A well-formed pair answers the code point it
/// denotes and a width of two; an unpaired surrogate answers itself and a width of one, because the
/// standard does not require a subject to be well formed and a lone surrogate has to match
/// something.
pub struct CodePointInput<'a> {
    units: &'a [u16],
}

impl<'a> CodePointInput<'a> {
    #[must_use]
    pub fn new(units: &'a [u16]) -> Self {
        Self { units }
    }
}

/// Combines a surrogate pair into the code point it denotes.
fn combine(high: u16, low: u16) -> u32 {
    0x10000 + ((u32::from(high) - 0xD800) << 10) + (u32::from(low) - 0xDC00)
}

fn is_high_surrogate(unit: u16) -> bool {
    (0xD800..0xDC00).contains(&unit)
}

fn is_low_surrogate(unit: u16) -> bool {
    (0xDC00..0xE000).contains(&unit)
}

impl Haystack for CodePointInput<'_> {
    fn len(&self) -> usize {
        self.units.len()
    }

    fn at(&self, index: usize) -> Option<(u32, usize)> {
        let first = *self.units.get(index)?;
        if is_high_surrogate(first) {
            if let Some(&second) = self.units.get(index + 1) {
                if is_low_surrogate(second) {
                    return Some((combine(first, second), 2));
                }
            }
        }
        Some((u32::from(first), 1))
    }

    fn before(&self, index: usize) -> Option<(u32, usize)> {
        if index == 0 {
            return None;
        }
        let last = *self.units.get(index - 1)?;
        if is_low_surrogate(last) && index >= 2 {
            let previous = self.units[index - 2];
            if is_high_surrogate(previous) {
                return Some((combine(previous, last), 2));
            }
        }
        Some((u32::from(last), 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two modes disagree about an astral character, and that disagreement is the `u` flag.
    #[test]
    fn a_pair_is_two_characters_without_u_and_one_with_it() {
        let units = [0xD834u16, 0xDF06];

        let plain = CodeUnitInput::new(&units);
        assert_eq!(plain.at(0), Some((0xD834, 1)), "the high surrogate, alone");
        assert_eq!(plain.at(1), Some((0xDF06, 1)));

        let unicode = CodePointInput::new(&units);
        assert_eq!(unicode.at(0), Some((0x1D306, 2)), "the character, and it spans two positions");
    }

    /// THE POSITION UNIT DOES NOT CHANGE WITH THE MODE. Both inputs are two positions long, so an
    /// index a program observes means the same thing either way.
    #[test]
    fn indexing_stays_in_code_units_in_both_modes() {
        let units = [0xD834u16, 0xDF06];
        assert_eq!(CodeUnitInput::new(&units).len(), 2);
        assert_eq!(CodePointInput::new(&units).len(), 2);
    }

    /// A subject is not required to be well formed, so an unpaired surrogate has to match itself
    /// rather than a replacement character or nothing at all.
    #[test]
    fn an_unpaired_surrogate_answers_itself_even_in_code_point_mode() {
        let lone_high = [0xD834u16];
        assert_eq!(CodePointInput::new(&lone_high).at(0), Some((0xD834, 1)));

        let low_then_high = [0xDF06u16, 0xD834];
        assert_eq!(CodePointInput::new(&low_then_high).at(0), Some((0xDF06, 1)), "not a pair");
    }

    /// Reading backwards has to re-pair, or a lookbehind would see half a character.
    #[test]
    fn reading_backwards_recombines_a_pair() {
        let units = [0x61u16, 0xD834, 0xDF06];
        let unicode = CodePointInput::new(&units);
        assert_eq!(unicode.before(3), Some((0x1D306, 2)));
        assert_eq!(unicode.before(1), Some((0x61, 1)));
        assert_eq!(unicode.before(0), None, "nothing ends at the start");

        let plain = CodeUnitInput::new(&units);
        assert_eq!(plain.before(3), Some((0xDF06, 1)), "and without u it is half a character");
    }
}
