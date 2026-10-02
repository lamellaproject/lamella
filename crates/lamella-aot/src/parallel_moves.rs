//! The order in which to make a set of moves that must take effect as if simultaneous.
//!
//! A block's arguments reach its parameters all at once, so the copies on a loop's back edge are
//! one assignment: a swap carried around a loop is `(a, b) = (b, a)`. Made one move at a time in
//! the order given, a move can overwrite a value a later move still has to read. Every backend's
//! block-argument copies are ordered here, so the rule is written once.

use alloc::vec::Vec;

/// `moves`, as `(destination, source)` pairs, in an order that makes them take effect as if
/// simultaneous, with `scratch` holding a saved value where a cycle needs one.
///
/// A move comes out only when no move still to come reads its destination. When every remaining
/// destination is still to be read, the moves left are cycles: `n` moves read `n` sources and each
/// of the `n` destinations is read at least once, so each is read exactly once and nothing else is
/// read, `scratch` included. The first destination's old value is then copied to `scratch` and its
/// readers read `scratch` instead, which opens its cycle. By the same count no move still reads
/// `scratch` when the next cycle is reached, so one scratch location serves them all.
///
/// Destinations must be distinct and `scratch` must appear in no move. A move from a location to
/// itself is dropped. A location can be a register or one word of a stack slot, and a value wider
/// than a word contributes each of its words to the same set.
pub(crate) fn schedule<T: Copy + Eq>(moves: &[(T, T)], scratch: T) -> Vec<(T, T)> {
    debug_assert!(moves.iter().all(|&(d, s)| d != scratch && s != scratch));
    let mut pending: Vec<(T, T)> = moves.iter().copied().filter(|(d, s)| d != s).collect();
    let mut ordered = Vec::with_capacity(pending.len());
    while !pending.is_empty() {
        let ready = pending
            .iter()
            .position(|(d, _)| !pending.iter().any(|(_, s)| s == d));
        if let Some(i) = ready {
            ordered.push(pending.remove(i));
        } else {
            let saved = pending[0].0;
            ordered.push((scratch, saved));
            for (_, source) in &mut pending {
                if *source == saved {
                    *source = scratch;
                }
            }
        }
    }
    ordered
}

/// A location a stack-slot copy moves through: one word of a slot, at its byte offset from the
/// stack pointer, or the register that holds a cycle's saved word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SlotWord<Offset> {
    /// The slot word at this offset.
    At(Offset),
    /// The register a cycle's word is saved in.
    Saved,
}

#[cfg(test)]
mod tests {
    use super::schedule;
    use alloc::vec::Vec;

    /// Runs `moves` in `schedule`'s order over locations `0..5`, which start at `before`, with
    /// location 5 as the scratch, and returns where they end.
    fn run(moves: &[(usize, usize)], before: [i32; 6]) -> [i32; 6] {
        let mut after = before;
        for (dst, src) in schedule(moves, 5) {
            after[dst] = after[src];
        }
        after
    }

    /// Asserts that `moves` take effect as if simultaneous: every destination ends holding what its
    /// source held before, and every other location but the scratch keeps its value.
    fn assert_simultaneous(moves: &[(usize, usize)]) {
        let before = [11, 23, 37, 53, 71, -1];
        let mut expected = before;
        for &(dst, src) in moves {
            expected[dst] = before[src];
        }
        let after = run(moves, before);
        assert_eq!(after[..5], expected[..5], "{moves:?}");
    }

    #[test]
    fn a_swap_saves_one_value_and_keeps_both() {
        assert_simultaneous(&[(0, 1), (1, 0)]);
        assert_eq!(
            schedule(&[(0, 1), (1, 0)], 5).len(),
            3,
            "save, move, restore"
        );
    }

    #[test]
    fn a_rotation_is_one_cycle() {
        assert_simultaneous(&[(0, 1), (1, 2), (2, 0)]);
        assert_simultaneous(&[(2, 0), (1, 2), (0, 1)]);
    }

    #[test]
    fn a_chain_needs_no_scratch() {
        let ordered = schedule(&[(1, 2), (0, 1)], 5);
        assert_eq!(ordered, [(0, 1), (1, 2)]);
    }

    #[test]
    fn a_value_read_twice_survives_its_overwrite() {
        assert_simultaneous(&[(0, 1), (2, 1), (1, 3)]);
        assert_simultaneous(&[(1, 0), (0, 1), (2, 0), (3, 1)]);
    }

    #[test]
    fn every_assignment_over_four_locations_is_simultaneous() {
        for choice in 0..5usize.pow(4) {
            let mut rest = choice;
            let mut moves: Vec<(usize, usize)> = (0..4)
                .map(|dst| {
                    let src = rest % 5;
                    rest /= 5;
                    (dst, src)
                })
                .collect();
            assert_simultaneous(&moves);
            moves.reverse();
            assert_simultaneous(&moves);
        }
    }
}
