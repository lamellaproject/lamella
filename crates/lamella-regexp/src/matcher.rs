//! The backtracking matcher: one loop, one explicit stack, no recursion on the subject.

use crate::ast::{Assertion, ClassEntry};
use crate::haystack::Haystack;
use crate::program::{Direction, Fold, Instruction, Program};
use crate::Vec;

/// The most deeply a lookaround may nest before the matcher refuses.
///
/// It bounds native stack use. Patterns written by people nest a handful deep; the limit exists for
/// generated or hostile ones, and being refused loudly is the intended outcome for those.
pub const MAX_LOOK_DEPTH: u32 = 64;

/// A budget in matcher steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fuel(u64);

impl Fuel {
    /// No budget. The matcher runs until it answers, which for some patterns is a very long time.
    pub const UNLIMITED: Fuel = Fuel(u64::MAX);

    #[must_use]
    pub fn new(steps: u64) -> Self {
        Fuel(steps)
    }
}

/// What a match produced: the capture slots, in the layout [`Instruction::Save`] describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub slots: Vec<Option<usize>>,
}

impl Match {
    /// The half-open span a group matched, or `None` when it did not participate.
    #[must_use]
    pub fn group(&self, index: usize) -> Option<(usize, usize)> {
        let start = (*self.slots.get(index * 2)?)?;
        let end = (*self.slots.get(index * 2 + 1)?)?;
        Some((start, end))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Match(Match),
    NoMatch,
    /// The step budget ran out before the matcher could answer.
    Fuel,
    /// Lookarounds nested past [`MAX_LOOK_DEPTH`].
    TooDeep,
}

/// A backtrack-stack entry: either a choice to return to, or a write to undo on the way.
enum Entry {
    Branch { pc: u32, pos: usize },
    Capture { slot: u32, previous: Option<usize> },
    Counter { counter: u32, previous: u32 },
    Register { register: u32, previous: usize },
    /// Restores the whole capture array, pushed once by a lookaround that kept its body's writes.
    Captures(Vec<Option<usize>>),
}

struct State {
    slots: Vec<Option<usize>>,
    counters: Vec<u32>,
    registers: Vec<usize>,
    stack: Vec<Entry>,
    fuel: u64,
    depth: u32,
}

/// Runs `program` against `haystack` beginning at `start`.
///
/// The match is ANCHORED at `start`: this finds whether the pattern matches THERE, not whether it
/// matches anywhere. Scanning forward is the caller's job, because the caller is the one that knows
/// whether its language's sticky flag forbids it.
pub fn run<H: Haystack>(program: &Program, haystack: &H, start: usize, fuel: Fuel) -> Outcome {
    let mut state = State {
        slots: crate::vec![None; program.slots],
        counters: crate::vec![0; program.counters],
        registers: crate::vec![0; program.registers],
        stack: Vec::new(),
        fuel: fuel.0,
        depth: 0,
    };
    match execute(program, haystack, &mut state, 0, start) {
        Step::Matched => Outcome::Match(Match { slots: state.slots }),
        Step::Failed => Outcome::NoMatch,
        Step::Fuel => Outcome::Fuel,
        Step::TooDeep => Outcome::TooDeep,
    }
}

enum Step {
    /// Reached a terminator.
    Matched,
    Failed,
    Fuel,
    TooDeep,
}

/// Runs from `pc`/`pos` until a terminator, a failure with no choices left, or an exhausted budget.
///
/// The stack below `floor` belongs to a caller and is never popped, which is what makes a
/// lookaround's failure local rather than a failure of the whole match.
fn execute<H: Haystack>(
    program: &Program,
    haystack: &H,
    state: &mut State,
    entry_pc: u32,
    entry_pos: usize,
) -> Step {
    let floor = state.stack.len();
    let mut pc = entry_pc;
    let mut pos = entry_pos;

    loop {
        if state.fuel == 0 {
            return Step::Fuel;
        }
        state.fuel -= 1;

        let instruction = match program.instructions.get(pc as usize) {
            Some(instruction) => instruction,
            None => return Step::Failed,
        };

        let advanced = match instruction {
            Instruction::Match | Instruction::LookEnd => return Step::Matched,

            Instruction::Char { ch, direction, fold } => match read(haystack, pos, *direction) {
                Some((value, width)) if fold.same(value, *ch) => {
                    pos = step(pos, width, *direction);
                    true
                }
                _ => false,
            },

            Instruction::Class { start, len, negated, direction, fold } => {
                match read(haystack, pos, *direction) {
                    Some((value, width)) => {
                        let from = *start as usize;
                        let to = from + *len as usize;
                        let inside = program.classes[from..to].iter().any(|e| match e {
                            ClassEntry::Single(member) => fold.same(*member, value),
                            ClassEntry::Range(..) => e.contains(value),
                        });
                        if inside != *negated {
                            pos = step(pos, width, *direction);
                            true
                        } else {
                            false
                        }
                    }
                    None => false,
                }
            }

            Instruction::Any { dot_all, direction } => match read(haystack, pos, *direction) {
                Some((value, width)) if *dot_all || !is_line_terminator(value) => {
                    pos = step(pos, width, *direction);
                    true
                }
                _ => false,
            },

            Instruction::Split { first, second } => {
                state.stack.push(Entry::Branch { pc: *second, pos });
                pc = *first;
                continue;
            }

            Instruction::Jump(target) => {
                pc = *target;
                continue;
            }

            Instruction::Save { slot } => {
                let slot = *slot;
                state.stack.push(Entry::Capture {
                    slot,
                    previous: state.slots[slot as usize],
                });
                state.slots[slot as usize] = Some(pos);
                true
            }

            Instruction::ClearCaptures { from, to } => {
                for slot in *from..*to {
                    state.stack.push(Entry::Capture {
                        slot,
                        previous: state.slots[slot as usize],
                    });
                    state.slots[slot as usize] = None;
                }
                true
            }

            Instruction::Assert { assertion, multiline } => {
                holds(*assertion, *multiline, haystack, pos, program, state)
            }

            Instruction::Backreference { group, direction, fold } => {
                match backreference(haystack, state, *group, pos, *direction, *fold) {
                    Some(next) => {
                        pos = next;
                        true
                    }
                    None => false,
                }
            }

            Instruction::Mark { register } => {
                let register = *register;
                state.stack.push(Entry::Register {
                    register,
                    previous: state.registers[register as usize],
                });
                state.registers[register as usize] = pos;
                true
            }

            Instruction::Progress { register, counter, min } => {
                let consumed = state.registers[*register as usize] != pos;
                consumed || state.counters[*counter as usize] < *min
            }

            Instruction::CounterInit { counter } => {
                let counter = *counter;
                state.stack.push(Entry::Counter {
                    counter,
                    previous: state.counters[counter as usize],
                });
                state.counters[counter as usize] = 0;
                true
            }

            Instruction::CounterSplit { counter, min, max, body, exit, greedy } => {
                let count = state.counters[*counter as usize];
                if count < *min {
                    pc = *body;
                } else if max.is_some_and(|limit| count >= limit) {
                    pc = *exit;
                } else if *greedy {
                    state.stack.push(Entry::Branch { pc: *exit, pos });
                    pc = *body;
                } else {
                    state.stack.push(Entry::Branch { pc: *body, pos });
                    pc = *exit;
                }
                continue;
            }

            Instruction::CounterNext { counter, head } => {
                let counter = *counter;
                state.stack.push(Entry::Counter {
                    counter,
                    previous: state.counters[counter as usize],
                });
                state.counters[counter as usize] += 1;
                pc = *head;
                continue;
            }

            Instruction::Look { negate, body } => {
                if state.depth >= MAX_LOOK_DEPTH {
                    return Step::TooDeep;
                }
                let negate = *negate;
                let body = *body;

                let before = state.slots.clone();
                let counters = state.counters.clone();
                let registers = state.registers.clone();
                let mark = state.stack.len();

                state.depth += 1;
                let result = execute(program, haystack, state, body, pos);
                state.depth -= 1;

                state.stack.truncate(mark);
                state.counters = counters;
                state.registers = registers;

                match result {
                    Step::Fuel => return Step::Fuel,
                    Step::TooDeep => return Step::TooDeep,
                    Step::Matched => {
                        if negate {
                            state.slots = before;
                            false
                        } else {
                            state.stack.push(Entry::Captures(before));
                            true
                        }
                    }
                    Step::Failed => {
                        state.slots = before;
                        negate
                    }
                }
            }
        };

        if advanced {
            pc += 1;
            continue;
        }

        loop {
            if state.stack.len() <= floor {
                return Step::Failed;
            }
            match state.stack.pop() {
                Some(Entry::Branch { pc: target, pos: saved }) => {
                    pc = target;
                    pos = saved;
                    break;
                }
                Some(Entry::Capture { slot, previous }) => {
                    state.slots[slot as usize] = previous;
                }
                Some(Entry::Counter { counter, previous }) => {
                    state.counters[counter as usize] = previous;
                }
                Some(Entry::Register { register, previous }) => {
                    state.registers[register as usize] = previous;
                }
                Some(Entry::Captures(previous)) => {
                    state.slots = previous;
                }
                None => return Step::Failed,
            }
        }
    }
}

fn read<H: Haystack>(haystack: &H, pos: usize, direction: Direction) -> Option<(u32, usize)> {
    match direction {
        Direction::Forward => haystack.at(pos),
        Direction::Backward => haystack.before(pos),
    }
}

fn step(pos: usize, width: usize, direction: Direction) -> usize {
    match direction {
        Direction::Forward => pos + width,
        Direction::Backward => pos - width,
    }
}

/// The four code points the standard calls line terminators.
fn is_line_terminator(ch: u32) -> bool {
    matches!(ch, 0x0A | 0x0D | 0x2028 | 0x2029)
}

/// Word characters for the boundary assertion, which the standard pins to ASCII.
fn is_word(ch: u32) -> bool {
    matches!(ch, 0x30..=0x39 | 0x41..=0x5A | 0x5F | 0x61..=0x7A)
}

fn holds<H: Haystack>(
    assertion: Assertion,
    multiline: bool,
    haystack: &H,
    pos: usize,
    _program: &Program,
    _state: &State,
) -> bool {
    match assertion {
        Assertion::Start => {
            pos == 0
                || (multiline
                    && haystack.before(pos).is_some_and(|(ch, _)| is_line_terminator(ch)))
        }
        Assertion::End => {
            pos == haystack.len()
                || (multiline && haystack.at(pos).is_some_and(|(ch, _)| is_line_terminator(ch)))
        }
        Assertion::WordBoundary | Assertion::NotWordBoundary => {
            let before = haystack.before(pos).is_some_and(|(ch, _)| is_word(ch));
            let after = haystack.at(pos).is_some_and(|(ch, _)| is_word(ch));
            (before != after) == matches!(assertion, Assertion::WordBoundary)
        }
    }
}

/// Matches the text a group captured, answering the position afterwards.
///
/// A group that did not participate matches the empty string. That is not an edge case to be
/// tolerated -- `/(?:(a)|b)\1/` relies on it -- and treating it as a failure changes which strings
/// the pattern accepts.
fn backreference<H: Haystack>(
    haystack: &H,
    state: &State,
    group: u32,
    pos: usize,
    direction: Direction,
    fold: Fold,
) -> Option<usize> {
    let start = state.slots.get(group as usize * 2).copied().flatten();
    let end = state.slots.get(group as usize * 2 + 1).copied().flatten();
    let (start, end) = match (start, end) {
        (Some(start), Some(end)) => (start, end),
        _ => return Some(pos),
    };

    let length = end.saturating_sub(start);
    if length == 0 {
        return Some(pos);
    }

    match direction {
        Direction::Forward => {
            let mut source = start;
            let mut cursor = pos;
            while source < end {
                let (want, want_width) = haystack.at(source)?;
                let (have, have_width) = haystack.at(cursor)?;
                if !fold.same(want, have) {
                    return None;
                }
                source += want_width;
                cursor += have_width;
            }
            Some(cursor)
        }
        Direction::Backward => {
            if pos < length {
                return None;
            }
            let mut source = end;
            let mut cursor = pos;
            while source > start {
                let (want, want_width) = haystack.before(source)?;
                let (have, have_width) = haystack.before(cursor)?;
                if !fold.same(want, have) {
                    return None;
                }
                source -= want_width;
                cursor -= have_width;
            }
            Some(cursor)
        }
    }
}
