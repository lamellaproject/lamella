//! The ECMA-262 pattern grammar, its flags, and the errors it reports.

mod parser;

pub use parser::{
    is_line_terminator, is_white_space, parse, Absent, Error, ErrorKind, Pattern, LINE_TERMINATOR,
    WHITE_SPACE,
};

use crate::compile::Options;
use crate::program::Program;

/// The flag set, in the standard's own order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flags {
    /// `d` -- record the index of each capture.
    pub has_indices: bool,
    /// `g` -- successive matches advance `lastIndex`.
    pub global: bool,
    /// `i` -- case-insensitive.
    pub ignore_case: bool,
    /// `m` -- `^` and `$` match at line terminators.
    pub multiline: bool,
    /// `s` -- `.` matches a line terminator.
    pub dot_all: bool,
    /// `u` -- match by code point, with the stricter grammar.
    pub unicode: bool,
    /// `v` -- the set-notation superset of `u`.
    pub unicode_sets: bool,
    /// `y` -- the match must start exactly at `lastIndex`.
    pub sticky: bool,
}

impl Flags {
    /// Parses a flags string.
    ///
    /// A repeated flag and an unknown flag are both errors, and so is `u` together with `v`: they
    /// select different grammars, so a pattern carrying both has no defined reading.
    pub fn parse(text: &str) -> Result<Flags, Error> {
        let mut flags = Flags::default();
        for (index, ch) in text.chars().enumerate() {
            let seen = match ch {
                'd' => &mut flags.has_indices,
                'g' => &mut flags.global,
                'i' => &mut flags.ignore_case,
                'm' => &mut flags.multiline,
                's' => &mut flags.dot_all,
                'u' => &mut flags.unicode,
                'v' => &mut flags.unicode_sets,
                'y' => &mut flags.sticky,
                _ => return Err(Error { kind: ErrorKind::UnknownFlag(ch), at: index }),
            };
            if *seen {
                return Err(Error { kind: ErrorKind::DuplicateFlag(ch), at: index });
            }
            *seen = true;
        }
        if flags.unicode && flags.unicode_sets {
            return Err(Error { kind: ErrorKind::UnicodeAndUnicodeSets, at: 0 });
        }
        Ok(flags)
    }

    /// Whether the pattern reads and matches by code point.
    #[must_use]
    pub fn code_point_mode(self) -> bool {
        self.unicode || self.unicode_sets
    }

    /// The flags string a program observes, which the standard fixes in this order.
    #[must_use]
    pub fn to_text(self) -> crate::String {
        let mut text = crate::String::new();
        for (present, letter) in [
            (self.has_indices, 'd'),
            (self.global, 'g'),
            (self.ignore_case, 'i'),
            (self.multiline, 'm'),
            (self.dot_all, 's'),
            (self.unicode, 'u'),
            (self.unicode_sets, 'v'),
            (self.sticky, 'y'),
        ] {
            if present {
                text.push(letter);
            }
        }
        text
    }
}

/// A parsed and compiled pattern, with everything the built-in surface needs to answer about it.
#[derive(Debug, Clone)]
pub struct Compiled {
    pub program: Program,
    pub flags: Flags,
    pub groups: u32,
    /// Group names in source order, paired with the group they name.
    pub names: crate::Vec<(crate::String, u32)>,
}

/// Parses and compiles a pattern.
pub fn compile_pattern(source: &str, flags: Flags) -> Result<Compiled, Error> {
    let pattern = parse(source, flags)?;
    let program = crate::compile::compile(
        &pattern.node,
        pattern.groups,
        Options { multiline: flags.multiline, fold: pattern_fold(flags) },
    );
    Ok(Compiled { program, flags, groups: pattern.groups, names: pattern.names })
}

/// Which canonicalization this pattern uses AT ITS TOP LEVEL, for the instructions compiled from it.
///
/// ECMA-262 17th ed, 22.2.2.7.3 (`Canonicalize`) branches on the `u` flag: with it the answer is
/// Unicode simple case folding, without it the character's `toUppercase` mapping under a rule that
/// refuses to carry a non-ASCII character into ASCII -- step 9, which returns the character
/// unchanged when a non-ASCII input would uppercase into ASCII.
///
/// **This is not pattern-wide, despite the flag being one.** 22.2.2.7.4 `UpdateModifiers` scopes
/// `i` to a subexpression for `(?i:...)`, so this answers only what the pattern opens with; the
/// fold that decides a comparison rides on the instruction that makes it.
///
/// **The front end resolves the ASCII range at COMPILE time and the matcher resolves the rest**,
/// and the split is not arbitrary: widening enumerates counterparts, which needs the fold
/// equivalence class and is available only for ASCII, while comparing needs the forward function
/// alone. So `[a-z]` arrives widened -- it is why [`widen_for_ignore_case`] adds U+212A and U+017F
/// only in code-point mode -- and `σ` arrives as itself, to be folded against whatever the subject
/// offers.
///
/// [`Fold::Ascii`] is therefore a published deviation rather than the standard's rule, and it is on
/// the list under `regexp-backreference-case-is-ascii-without-u`. The mapping table it would need
/// does not exist to read: the shared Unicode home ships the case PROPERTIES and the FOLD tables,
/// and no `toUppercase` data at all.
fn pattern_fold(flags: Flags) -> crate::program::Fold {
    use crate::program::Fold;
    match (flags.ignore_case, flags.code_point_mode()) {
        (false, _) => Fold::None,
        (true, true) => Fold::Simple,
        (true, false) => Fold::Ascii,
    }
}

/// The result of searching a subject.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Search {
    /// The slots of the first match found, in the layout the program describes.
    Found(crate::Vec<Option<usize>>),
    NotFound,
    /// The step budget ran out. Deliberately distinct from [`Search::NotFound`], because
    /// collapsing the two would report a pattern as not matching when the answer is unknown.
    Fuel,
    /// Lookarounds nested past what the matcher will run.
    TooDeep,
}

impl Compiled {
    /// Finds the first match at or after `start`.
    ///
    /// SCANNING ADVANCES BY CHARACTER, NOT BY POSITION. Under `u` a failed attempt inside a
    /// surrogate pair moves past the whole pair, so a match can never begin between the halves of
    /// one character. Without `u` the same string is two characters and both are tried.
    ///
    /// A sticky pattern does not scan at all: it matches at `start` or not at all, which is the
    /// entire difference between `y` and `g`.
    pub fn find(&self, subject: &[u16], start: usize, fuel: crate::Fuel) -> Search {
        let mut index = start;
        loop {
            if index > subject.len() {
                return Search::NotFound;
            }

            let outcome = if self.flags.code_point_mode() {
                crate::matcher::run(
                    &self.program,
                    &crate::CodePointInput::new(subject),
                    index,
                    fuel,
                )
            } else {
                crate::matcher::run(
                    &self.program,
                    &crate::CodeUnitInput::new(subject),
                    index,
                    fuel,
                )
            };

            match outcome {
                crate::Outcome::Match(found) => return Search::Found(found.slots),
                crate::Outcome::Fuel => return Search::Fuel,
                crate::Outcome::TooDeep => return Search::TooDeep,
                crate::Outcome::NoMatch => {
                    if self.flags.sticky {
                        return Search::NotFound;
                    }
                    index += self.step_width(subject, index);
                }
            }
        }
    }

    /// How far a failed attempt moves on, which is one character in whichever unit the mode reads.
    fn step_width(&self, subject: &[u16], index: usize) -> usize {
        if !self.flags.code_point_mode() {
            return 1;
        }
        match subject.get(index) {
            Some(&unit) if (0xD800..0xDC00).contains(&unit) => {
                match subject.get(index + 1) {
                    Some(&next) if (0xDC00..0xE000).contains(&next) => 2,
                    _ => 1,
                }
            }
            _ => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// EVERY LISTED ABSENCE MUST ACTUALLY REFUSE, AND THE ID IT PUBLISHES MUST BE THE ONE IT RAISES.
    ///
    /// The consumer's document is generated from [`ErrorKind::absences`], so an entry that stopped
    /// being true, or was never true, publishes a refusal this grammar does not perform -- which is
    /// the exact defect the ECMAScript engine's own list records about `RegExpLiterals`. Each row
    /// here compiles a pattern that must reach that refusal, so the list is checked against the
    /// code rather than against itself.
    #[test]
    fn every_listed_absence_actually_refuses_under_its_own_id() {
        let cases: [(&str, &str, &str); 4] = [
            ("regexp-property-escapes", r"\p{L}", "u"),
            ("regexp-v-flag", "a", "v"),
            ("regexp-case-folding", "[\u{3B1}-\u{3C9}]", "ui"),
            ("regexp-case-folding", "\u{3C3}", "i"),
        ];
        for (id, source, flags) in cases {
            let flags = Flags::parse(flags).expect("legal flags");
            let error = parse(source, flags).expect_err("this pattern must be refused");
            let absent = error.kind.absence().expect("the refusal must BE an absence");
            assert_eq!(absent.id, id, "{source:?} refused under the wrong id");
        }
    }

    /// THE TWO HALVES ARE SEPARATE AND NOTHING MAKES THEM AGREE, so they are checked against each
    /// other -- the same shape the engine's `knob`/`refusable` pair uses for the same reason.
    /// A count guards the array, which is the half the compiler cannot see: `absence()` is an
    /// exhaustive `match` and stops compiling when a variant is added, and `absences()` is a plain
    /// list that would simply stay short.
    #[test]
    fn the_published_array_holds_exactly_what_the_match_can_produce() {
        assert_eq!(
            ErrorKind::absences().len(),
            3,
            "an absence was added to or removed from this grammar -- update `absences()` and this \
             count TOGETHER, or the consumer's published document goes short"
        );
        for kind in [
            ErrorKind::PropertyEscapesUnavailable,
            ErrorKind::UnicodeSetsUnavailable,
            ErrorKind::CaseFoldingUnavailable,
        ] {
            let absent = kind.absence().expect("a refusal of excluded surface is an absence");
            assert!(
                ErrorKind::absences().contains(&absent),
                "{kind:?} names an absence the published array does not carry"
            );
        }
        assert_eq!(ErrorKind::UnterminatedGroup.absence(), None);
        assert_eq!(ErrorKind::NothingToRepeat.absence(), None);
        assert_eq!(ErrorKind::InvalidBackreference.absence(), None);

        let mut ids: alloc::vec::Vec<&str> = ErrorKind::absences().iter().map(|a| a.id).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "an absence id is duplicated");
        for id in ids {
            assert!(id.chars().all(|c| c.is_ascii_lowercase() || c == '-'), "{id} is not a key");
        }
    }

    /// A CASELESS PATTERN UNDER `i` COMPILES, AND IT DID NOT BEFORE.
    ///
    /// `i` cannot widen a code point that has no case, so such a pattern is matchable exactly as
    /// written. EVERY position that refuses is exercised here, because a rule with several
    /// implementations gains a new case in none of them: a bare literal, a class member, and the
    /// range form that deliberately still refuses.
    ///
    /// Patterns are built from code points rather than written as escapes, so that what is under
    /// test is the parser rather than this file's own escaping.
    #[test]
    fn ignore_case_accepts_caseless_text_and_still_refuses_cased_text() {
        fn pattern(points: &[u32]) -> alloc::string::String {
            points.iter().map(|c| char::from_u32(*c).expect("a scalar value")).collect()
        }
        let i = || Flags::parse("i").expect("legal flags");

        assert!(compile_pattern(&pattern(&[0x65E5, 0x672C]), i()).is_ok(), "CJK literal");
        assert!(compile_pattern(&pattern(&[0x5B, 0x65E5, 0x672C, 0x5D]), i()).is_ok(), "CJK class");
        assert!(compile_pattern(&pattern(&[0x1F600]), i()).is_ok(), "emoji, outside the BMP");

        assert!(compile_pattern(&pattern(&[0x3B1]), i()).is_err(), "Greek literal");
        assert!(compile_pattern(&pattern(&[0x5B, 0x430, 0x5D]), i()).is_err(), "Cyrillic class");

        assert!(
            compile_pattern(&pattern(&[0x5B, 0x4E00, 0x2D, 0x9FFF, 0x5D]), i()).is_err(),
            "a non-ASCII class range still refuses"
        );

        let plain = || Flags::parse("").expect("legal flags");
        assert!(compile_pattern(&pattern(&[0x3B1]), plain()).is_ok(), "no `i`, no refusal");
    }

    /// THE FIGURE THE CASE-INSENSITIVE REFUSAL RESTS ON, TAKEN WITH THE PREDICATE THAT SHIPS.
    ///
    /// The figure is taken against the exact function the parser consults, so that it describes the
    /// code rather than some other implementation's notion of case. **A number that does not carry
    /// what produced it cannot be checked by whoever quotes it next.**
    ///
    /// Surrogates are skipped because they are not characters: a pattern literal's code point can
    /// never be one, so counting them would put 2,048 free caseless points in the numerator and
    /// flatter the figure.
    #[test]
    fn almost_no_non_ascii_code_point_has_case() {
        let (mut cased, mut total) = (0usize, 0usize);
        for cp in 0x80..=0x10FFFF_u32 {
            if (0xD800..=0xDFFF).contains(&cp) {
                continue;
            }
            total += 1;
            if lamella_unicode::is_cased(cp) {
                cased += 1;
            }
        }
        let caseless = 100.0 - (cased as f64) * 100.0 / (total as f64);
        std::eprintln!("non-ASCII {total}, cased {cased}, caseless {caseless:.2}%");
        assert!(
            caseless > 99.0,
            "only {caseless:.2}% of {total} non-ASCII code points are caseless ({cased} cased) --              the refusal this figure justifies narrowing is no longer mostly unnecessary"
        );
    }

    #[test]
    fn flags_parse_and_render_in_the_standards_order() {
        let flags = Flags::parse("yig").expect("legal flags");
        assert!(flags.sticky && flags.ignore_case && flags.global);
        assert_eq!(flags.to_text(), "giy", "rendered in the fixed order, not the written one");
    }

    #[test]
    fn a_repeated_flag_is_an_error() {
        let error = Flags::parse("gg").expect_err("g twice");
        assert!(matches!(error.kind, ErrorKind::DuplicateFlag('g')));
    }

    #[test]
    fn an_unknown_flag_is_an_error() {
        assert!(matches!(
            Flags::parse("q").expect_err("no such flag").kind,
            ErrorKind::UnknownFlag('q')
        ));
    }

    /// They select different grammars, so carrying both has no defined reading.
    #[test]
    fn u_and_v_together_are_an_error() {
        assert!(matches!(
            Flags::parse("uv").expect_err("mutually exclusive").kind,
            ErrorKind::UnicodeAndUnicodeSets
        ));
    }
}
