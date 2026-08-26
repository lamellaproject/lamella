//! Lowering the shared tree to a flat program.

use crate::ast::{ClassEntry, Greed, Node};
use crate::program::{Direction, Fold, Instruction, Program};
use crate::Vec;

/// The settings in force where compilation STARTS, which the tree does not carry per node.
///
/// # NOT "PATTERN-WIDE", AND THE PROGRAM ALREADY KNEW THAT FOR TWO OF THE THREE
///
/// These arrive from the front end as one value each because a front end reads one flag string.
/// **The compiled program stores them per instruction**, and it did so before `fold` joined them:
/// [`Instruction::Assert`] carries its own `multiline` and [`Instruction::Any`] its own `dot_all`.
///
/// That is not incidental. `i`, `m` and `s` are exactly the three flags a scoped inline modifier
/// can change -- ECMA-262 17th ed, 22.2.2.7.4 (`UpdateModifiers`) sets `[[IgnoreCase]]`,
/// `[[Multiline]]` and `[[DotAll]]` and nothing else -- so all three belong on the instruction for
/// the same reason. `fold` was the one that had been hoisted, and it is back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Options {
    /// `^` and `$` also match at line terminators.
    pub multiline: bool,
    /// How the pattern canonicalizes case AT ITS TOP LEVEL, already resolved from the front end's
    /// flags.
    ///
    /// **This is the fold in force where compilation starts, not a property of the program.** The
    /// instructions carry their own, because a scoped inline modifier -- `(?i:...)`, which Python's
    /// `re` ships today -- changes the answer part-way down a pattern. [`Fold`] states that scope
    /// in full. A front end with no such construct passes one value and every instruction gets it.
    ///
    /// **A LITERAL AND A BACKREFERENCE NEED THE SAME CASE DATA, AND ONLY THE LITERAL CAN BE
    /// REFUSED.** A front end folds the literals and class members it emits, where it has its
    /// language's data and can reject a pattern it cannot fold, pointing at the offending
    /// character. **A backreference offers nothing to point at**: `/(.)\1/` contains no cased
    /// character at all, and the text being compared arrives from the SUBJECT at match time. So
    /// this is the one place the matcher itself must canonicalize, and it carries a resolved
    /// [`Fold`] rather than a flag so that the rule is decided where the flags are understood.
    ///
    /// # THE LITERAL HALF DOES NOT NEED THE DATA ITS REFUSAL CLAIMS IT DOES
    ///
    /// A front end that folds a literal at COMPILE time has to enumerate the fold EQUIVALENCE
    /// CLASS -- every code point folding to one target -- which the shared Unicode home does not
    /// expose. That is the stated blocker for a cased non-ASCII literal under `i`, and **it is a
    /// property of compile-time widening rather than of the problem.**
    ///
    /// **Canonicalizing at MATCH time needs only the FORWARD function**, which is already public
    /// and already used here: `Canonicalize(pattern) == Canonicalize(subject)` decides a literal
    /// without ever naming the other members of its class. [`Fold::same`] IS that comparison. So
    /// the literal half asks nothing of a crate two other languages depend on, and keeps one
    /// character as one instruction rather than a class of N -- which is the direction a part with
    /// 256 KB wants anyway.
    ///
    /// **The scope, stated exactly, because most of the surface does NOT follow:**
    ///
    /// ```text
    /// literal char, `u` mode           a match-time fold decides it
    /// class SINGLE member, `u` mode    the same comparison, the same answer
    /// class RANGE                      STILL REFUSED -- "does any member of `[a-b]`
    ///                                      canonicalize to this?" is a range-intersection
    ///                                      predicate, and no fold answers it
    /// any of them WITHOUT `u`          STILL REFUSED -- Canonicalize MAPS rather than folds
    ///                                      in that mode, and there is no mapping table to read.
    ///                                      The same gap [`Fold::Ascii`] is published for.
    /// ```
    pub fold: Fold,
}

/// Compiles a tree into a program with `groups` capturing groups.
#[must_use]
pub fn compile(node: &Node, groups: u32, options: Options) -> Program {
    let mut builder = Builder {
        instructions: Vec::new(),
        classes: Vec::new(),
        counters: 0,
        registers: 0,
        fold: options.fold,
        options,
    };

    builder.emit(Instruction::Save { slot: 0 });
    builder.node(node, Direction::Forward);
    builder.emit(Instruction::Save { slot: 1 });
    builder.emit(Instruction::Match);

    Program {
        instructions: builder.instructions,
        classes: builder.classes,
        slots: (groups as usize + 1) * 2,
        counters: builder.counters,
        registers: builder.registers,
    }
}

struct Builder {
    instructions: Vec<Instruction>,
    classes: Vec<ClassEntry>,
    counters: usize,
    registers: usize,
    /// The canonicalization in force at the node being lowered.
    ///
    /// **THE SINGLE SITE THAT DECIDES `fold` FOR EVERY INSTRUCTION THAT COMPARES A CHARACTER.**
    /// Three emit sites read it and none of them decides it, which is what keeps one rule from
    /// gaining a case in only one of its implementations -- without making the rule pattern-wide,
    /// which is a different thing and would foreclose a scoped inline modifier. A construct that
    /// scopes folding sets this over its subexpression and restores it after.
    fold: Fold,
    options: Options,
}

impl Builder {
    fn here(&self) -> u32 {
        self.instructions.len() as u32
    }

    fn emit(&mut self, instruction: Instruction) -> u32 {
        let at = self.here();
        self.instructions.push(instruction);
        at
    }

    /// Interns a class's ranges into the shared table, answering its span.
    fn intern(&mut self, entries: &[ClassEntry]) -> (u32, u32) {
        let start = self.classes.len() as u32;
        self.classes.extend_from_slice(entries);
        (start, entries.len() as u32)
    }

    fn node(&mut self, node: &Node, direction: Direction) {
        match node {
            Node::Empty => {}

            Node::Char(ch) => {
                self.emit(Instruction::Char { ch: *ch, direction, fold: self.fold });
            }

            Node::Class { entries, negated } => {
                let (start, len) = self.intern(entries);
                let fold = self.fold;
                self.emit(Instruction::Class { start, len, negated: *negated, direction, fold });
            }

            Node::Any { dot_all } => {
                self.emit(Instruction::Any { dot_all: *dot_all, direction });
            }

            Node::Concat(parts) => match direction {
                Direction::Forward => {
                    for part in parts {
                        self.node(part, direction);
                    }
                }
                Direction::Backward => {
                    for part in parts.iter().rev() {
                        self.node(part, direction);
                    }
                }
            },

            Node::Alternate(parts) => self.alternate(parts, direction),

            Node::Repeat { node, min, max, greed } => {
                self.repeat(node, *min, *max, *greed, direction);
            }

            Node::Group { index, node } => match index {
                None => self.node(node, direction),
                Some(index) => {
                    let (first, second) = match direction {
                        Direction::Forward => (index * 2, index * 2 + 1),
                        Direction::Backward => (index * 2 + 1, index * 2),
                    };
                    self.emit(Instruction::Save { slot: first });
                    self.node(node, direction);
                    self.emit(Instruction::Save { slot: second });
                }
            },

            Node::Assertion(assertion) => {
                self.emit(Instruction::Assert {
                    assertion: *assertion,
                    multiline: self.options.multiline,
                });
            }

            Node::Backreference(group) => {
                self.emit(Instruction::Backreference { group: *group, direction, fold: self.fold });
            }

            Node::Look { behind, negate, node } => {
                let inner = if *behind { Direction::Backward } else { Direction::Forward };
                let placeholder = self.emit(Instruction::Look { negate: *negate, body: 0 });
                let skip = self.emit(Instruction::Jump(0));

                let body = self.here();
                self.node(node, inner);
                self.emit(Instruction::LookEnd);

                let after = self.here();
                self.instructions[placeholder as usize] =
                    Instruction::Look { negate: *negate, body };
                self.instructions[skip as usize] = Instruction::Jump(after);
            }
        }
    }

    /// Emits an ordered choice. The FIRST alternative that leads to a match wins, so the chain is
    /// built so that each split prefers its own branch and falls through to the next.
    fn alternate(&mut self, parts: &[Node], direction: Direction) {
        let mut exits = Vec::new();

        for (index, part) in parts.iter().enumerate() {
            let last = index + 1 == parts.len();
            if last {
                self.node(part, direction);
                break;
            }

            let split = self.emit(Instruction::Split { first: 0, second: 0 });
            let first = self.here();
            self.node(part, direction);
            exits.push(self.emit(Instruction::Jump(0)));

            let second = self.here();
            self.instructions[split as usize] = Instruction::Split { first, second };
        }

        let end = self.here();
        for exit in exits {
            self.instructions[exit as usize] = Instruction::Jump(end);
        }
    }

    fn repeat(
        &mut self,
        body: &Node,
        min: u32,
        max: Option<u32>,
        greed: Greed,
        direction: Direction,
    ) {
        if max == Some(0) {
            return;
        }

        let counter = self.counters as u32;
        self.counters += 1;
        self.emit(Instruction::CounterInit { counter });

        let guarded = max.is_none() && body.matches_empty();
        let register = if guarded {
            let register = self.registers as u32;
            self.registers += 1;
            Some(register)
        } else {
            None
        };

        let head = self.here();
        let split = self.emit(Instruction::CounterSplit {
            counter,
            min,
            max,
            body: 0,
            exit: 0,
            greedy: matches!(greed, Greed::Greedy),
        });

        let body_pc = self.here();

        if let Some((from, to)) = group_slots(body) {
            self.emit(Instruction::ClearCaptures { from, to });
        }

        if let Some(register) = register {
            self.emit(Instruction::Mark { register });
        }

        self.node(body, direction);

        if let Some(register) = register {
            self.emit(Instruction::Progress { register, counter, min });
        }

        self.emit(Instruction::CounterNext { counter, head });

        let exit = self.here();
        self.instructions[split as usize] = Instruction::CounterSplit {
            counter,
            min,
            max,
            body: body_pc,
            exit,
            greedy: matches!(greed, Greed::Greedy),
        };
    }
}

/// The half-open span of capture slots a subtree writes, or `None` when it has no groups.
fn group_slots(node: &Node) -> Option<(u32, u32)> {
    let mut lowest = u32::MAX;
    let mut highest = 0u32;
    walk_groups(node, &mut lowest, &mut highest);
    if lowest == u32::MAX {
        None
    } else {
        Some((lowest * 2, highest * 2 + 2))
    }
}

fn walk_groups(node: &Node, lowest: &mut u32, highest: &mut u32) {
    match node {
        Node::Group { index, node } => {
            if let Some(index) = index {
                *lowest = (*lowest).min(*index);
                *highest = (*highest).max(*index);
            }
            walk_groups(node, lowest, highest);
        }
        Node::Concat(parts) | Node::Alternate(parts) => {
            for part in parts {
                walk_groups(part, lowest, highest);
            }
        }
        Node::Repeat { node, .. } | Node::Look { node, .. } => walk_groups(node, lowest, highest),
        Node::Empty
        | Node::Char(_)
        | Node::Class { .. }
        | Node::Any { .. }
        | Node::Assertion(_)
        | Node::Backreference(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Assertion;
    use crate::Box;

    fn program_of(node: &Node, groups: u32) -> Program {
        compile(node, groups, Options::default())
    }

    /// TWO FOLDS IN ONE PROGRAM, DISAGREEING -- the capability a pattern-wide field cannot express.
    ///
    /// ECMA-262 17th ed REQUIRES it: `(?i:...)` is the production
    /// `Atom :: (? RegularExpressionModifiers : Disjunction )` (22.2.1), whose runtime semantics
    /// `UpdateModifiers` (22.2.2.7.4) turn `i` on for a SUBEXPRESSION. Python's `re` ships the same
    /// construct. **A `Fold` held once per program cannot represent either**, and that is why this
    /// crate carries it per instruction even though no front end here emits two yet.
    #[test]
    fn two_instructions_in_one_program_can_fold_differently() {
        let program = Program {
            instructions: crate::vec![
                Instruction::Save { slot: 0 },
                Instruction::Char {
                    ch: b'a' as u32,
                    direction: Direction::Forward,
                    fold: Fold::Simple,
                },
                Instruction::Char {
                    ch: b'a' as u32,
                    direction: Direction::Forward,
                    fold: Fold::None,
                },
                Instruction::Save { slot: 1 },
                Instruction::Match,
            ],
            classes: Vec::new(),
            slots: 2,
            counters: 0,
            registers: 0,
        };

        let matched = |subject: &str| {
            let units: Vec<u16> = subject.encode_utf16().collect();
            let input = crate::haystack::CodeUnitInput::new(&units);
            matches!(
                crate::matcher::run(&program, &input, 0, crate::Fuel::UNLIMITED),
                crate::Outcome::Match(_)
            )
        };

        assert!(matched("Aa"), "the first instruction folds, so `A` is `a` THERE");
        assert!(matched("aa"), "and folding does not stop it matching its own spelling");
        assert!(!matched("aA"), "the second does NOT fold, so `A` is not `a` there");
        assert!(!matched("AA"), "which one field for the whole program could not have said");
    }

    /// The whole match is a capture like any other, so a bare pattern still writes slots 0 and 1.
    #[test]
    fn the_whole_match_is_saved_around_the_body() {
        let program = program_of(&Node::Char(b'a' as u32), 0);
        assert_eq!(program.instructions[0], Instruction::Save { slot: 0 });
        assert!(matches!(program.instructions[1], Instruction::Char { .. }));
        assert_eq!(program.instructions[2], Instruction::Save { slot: 1 });
        assert_eq!(program.instructions[3], Instruction::Match);
        assert_eq!(program.slots, 2);
    }

    /// A lookbehind reverses its concatenation. Without this the engine matches the body's
    /// characters in the wrong order and every one-character test still passes.
    #[test]
    fn a_lookbehind_emits_its_concatenation_in_reverse() {
        let body = Node::Concat(crate::vec![Node::Char(b'a' as u32), Node::Char(b'b' as u32)]);
        let node = Node::Look { behind: true, negate: false, node: Box::new(body) };
        let program = program_of(&node, 0);

        let chars: Vec<u32> = program
            .instructions
            .iter()
            .filter_map(|i| match i {
                Instruction::Char { ch, direction, .. } => {
                    assert_eq!(*direction, Direction::Backward, "inside a lookbehind");
                    Some(*ch)
                }
                _ => None,
            })
            .collect();
        assert_eq!(chars, crate::vec![b'b' as u32, b'a' as u32], "b is met first");
    }

    /// The group's saves follow the direction too, or a lookbehind reports start after end.
    #[test]
    fn a_group_inside_a_lookbehind_saves_its_end_first() {
        let group = Node::Group { index: Some(1), node: Box::new(Node::Char(b'a' as u32)) };
        let node = Node::Look { behind: true, negate: false, node: Box::new(group) };
        let program = program_of(&node, 1);

        let saves: Vec<u32> = program
            .instructions
            .iter()
            .filter_map(|i| match i {
                Instruction::Save { slot } if *slot >= 2 => Some(*slot),
                _ => None,
            })
            .collect();
        assert_eq!(saves, crate::vec![3, 2], "the end slot is written first");
    }

    /// The guard costs two instructions, so it is emitted only where a loop could spin.
    #[test]
    fn only_a_nullable_unbounded_body_gets_the_progress_guard() {
        let plain = Node::Repeat {
            node: Box::new(Node::Char(b'a' as u32)),
            min: 0,
            max: None,
            greed: Greed::Greedy,
        };
        let program = program_of(&plain, 0);
        assert!(
            !program.instructions.iter().any(|i| matches!(i, Instruction::Progress { .. })),
            "a body that always consumes cannot spin"
        );
        assert_eq!(program.registers, 0);

        let nullable = Node::Repeat {
            node: Box::new(Node::Assertion(Assertion::WordBoundary)),
            min: 0,
            max: None,
            greed: Greed::Greedy,
        };
        let guarded = program_of(&nullable, 0);
        assert!(
            guarded.instructions.iter().any(|i| matches!(i, Instruction::Progress { .. })),
            "an assertion consumes nothing, so this one can"
        );
        assert_eq!(guarded.registers, 1);
    }

    /// A bounded repetition cannot spin either, however nullable its body is.
    #[test]
    fn a_bounded_repetition_needs_no_guard() {
        let node = Node::Repeat {
            node: Box::new(Node::Empty),
            min: 0,
            max: Some(3),
            greed: Greed::Greedy,
        };
        let program = program_of(&node, 0);
        assert!(!program.instructions.iter().any(|i| matches!(i, Instruction::Progress { .. })));
    }

    /// The clear covers every slot the body can write, which is what resets a group per iteration.
    #[test]
    fn a_repetition_clears_the_groups_in_its_body() {
        let group = Node::Group { index: Some(1), node: Box::new(Node::Char(b'a' as u32)) };
        let node = Node::Repeat {
            node: Box::new(group),
            min: 0,
            max: None,
            greed: Greed::Greedy,
        };
        let program = program_of(&node, 1);
        assert!(
            program
                .instructions
                .iter()
                .any(|i| matches!(i, Instruction::ClearCaptures { from: 2, to: 4 })),
            "group one's two slots"
        );
    }

    /// A body with no groups pays nothing for the rule.
    #[test]
    fn a_repetition_without_groups_emits_no_clear() {
        let node = Node::Repeat {
            node: Box::new(Node::Char(b'a' as u32)),
            min: 0,
            max: None,
            greed: Greed::Greedy,
        };
        let program = program_of(&node, 0);
        assert!(!program
            .instructions
            .iter()
            .any(|i| matches!(i, Instruction::ClearCaptures { .. })));
    }
}
