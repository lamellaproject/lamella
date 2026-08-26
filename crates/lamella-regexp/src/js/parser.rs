//! Reading an ECMA-262 pattern into the shared tree.

use crate::ast::{Assertion, ClassEntry, Greed, Node};
use crate::js::Flags;
use crate::{Box, String, Vec};

/// A parsed pattern.
#[derive(Debug, Clone)]
pub struct Pattern {
    pub node: Node,
    pub groups: u32,
    pub names: Vec<(String, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    pub kind: ErrorKind,
    /// The code-unit offset in the pattern source, so a diagnostic can point at it.
    pub at: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorKind {
    UnknownFlag(char),
    DuplicateFlag(char),
    UnicodeAndUnicodeSets,
    UnterminatedGroup,
    UnterminatedClass,
    UnmatchedCloseParen,
    /// A quantifier with no atom before it, such as a leading `*`.
    NothingToRepeat,
    /// `{2,1}`.
    QuantifierOutOfOrder,
    /// A `{` that does not begin a well-formed quantifier. A browser reads it as a literal under
    /// Annex B; this profile does not implement Annex B.
    LoneQuantifierBrace,
    /// A syntax character in a position where only a pattern character is allowed.
    UnexpectedSyntaxCharacter(char),
    /// A pattern ending in a backslash.
    TrailingBackslash,
    /// An escape this profile does not recognize. Annex B would read many of these as identities.
    InvalidEscape,
    /// `\1` where there is no group one.
    InvalidBackreference,
    InvalidNamedReference,
    DuplicateGroupName,
    InvalidGroupName,
    InvalidUnicodeEscape,
    /// `[z-a]`, or a range whose endpoint is a class escape.
    InvalidClassRange,
    /// An assertion with a quantifier on it.
    QuantifiedAssertion,
    /// `\p{...}`, which needs Unicode property tables the shared Unicode home does not ship.
    PropertyEscapesUnavailable,
    /// The `v` flag's set notation.
    UnicodeSetsUnavailable,
    /// A case-insensitive pattern reaching beyond ASCII, which needs a case-folding table the
    /// shared Unicode home does not ship.
    CaseFoldingUnavailable,
}

/// A refusal of surface this grammar does not implement, as opposed to a malformed pattern.
///
/// **A CONSUMER'S PUBLISHED LIST OF UNIMPLEMENTED SURFACE CANNOT SEE INTO THIS CRATE, SO A
/// REFUSAL DECLARED HERE IS INVISIBLE THERE UNLESS THIS LIST CARRIES IT.** A generated absence
/// list walked from the types a consumer's own refusals pass through is the right mechanism, and
/// it stops at the crate boundary. `\p{...}`, the `v` flag and a
/// case-insensitive pattern beyond ASCII are all legal ECMAScript that this parser rejects, and
/// none of the three appeared in that document.
///
/// So the classification lives HERE, where the refusals are, and a consumer walks it. It is a
/// `match` in [`ErrorKind::absence`], so a new variant does not compile until somebody has said
/// which of the two it is -- the same property that makes the consumer's own list exhaustive by
/// construction, extended across the boundary rather than duplicated on the far side of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Absent {
    /// A stable search key, never a sentence.
    pub id: &'static str,
    /// What is missing, for a reader who cannot reconstruct it from the name.
    pub reason: &'static str,
}

/// Written ONCE. [`ErrorKind::absence`] and [`ErrorKind::absences`] have to agree, and a second
/// copy of a literal is exactly how they would stop agreeing -- the same reason a consumer walks
/// this list rather than retyping it.
const PROPERTY_ESCAPES: Absent = Absent {
    id: "regexp-property-escapes",
    reason: "`\\p{...}` and `\\P{...}`, which need the General_Category and Script tables",
};
const V_FLAG: Absent = Absent {
    id: "regexp-v-flag",
    reason: "the `v` flag's set notation, which is a second pattern grammar",
};
const CASE_FOLDING: Absent = Absent {
    id: "regexp-case-folding",
    reason: "a class RANGE that crosses out of ASCII under `i`, in any mode; and WITHOUT `u`, any \
             cased character beyond ASCII -- under `u` a literal, a class member and a \
             backreference all fold correctly, and a caseless character always matched",
};

impl ErrorKind {
    /// The published absence this refusal IS, or `None` when it is a genuine malformed pattern.
    ///
    /// **THE TWO ARE NOT THE SAME BUCKET AND THE DIFFERENCE IS NOT COSMETIC.** `UnterminatedGroup`
    /// means the author wrote a bad pattern; `PropertyEscapesUnavailable` means the author wrote a
    /// correct one this build does not implement. Reporting them alike puts a documented gap in the
    /// same pile as a user's typo, and -- because both surface as `SyntaxError` -- lets a negative
    /// test expecting a `SyntaxError` be scored as passing when the wrong thing went wrong.
    #[must_use]
    pub fn absence(&self) -> Option<Absent> {
        match self {
            ErrorKind::PropertyEscapesUnavailable => Some(PROPERTY_ESCAPES),
            ErrorKind::UnicodeSetsUnavailable => Some(V_FLAG),
            ErrorKind::CaseFoldingUnavailable => Some(CASE_FOLDING),
            ErrorKind::UnknownFlag(_)
            | ErrorKind::DuplicateFlag(_)
            | ErrorKind::UnicodeAndUnicodeSets
            | ErrorKind::UnterminatedGroup
            | ErrorKind::UnterminatedClass
            | ErrorKind::UnmatchedCloseParen
            | ErrorKind::NothingToRepeat
            | ErrorKind::QuantifierOutOfOrder
            | ErrorKind::LoneQuantifierBrace
            | ErrorKind::UnexpectedSyntaxCharacter(_)
            | ErrorKind::TrailingBackslash
            | ErrorKind::InvalidEscape
            | ErrorKind::InvalidBackreference
            | ErrorKind::InvalidNamedReference
            | ErrorKind::DuplicateGroupName
            | ErrorKind::InvalidGroupName
            | ErrorKind::InvalidUnicodeEscape
            | ErrorKind::InvalidClassRange
            | ErrorKind::QuantifiedAssertion => None,
        }
    }

    /// Every absence this grammar can refuse, in published order.
    ///
    /// A plain array, so the compiler has nothing to say about it -- the same hazard the consumer's
    /// own `all()` carries and the same remedy: a count assertion beside it, deliberately bumped.
    #[must_use]
    pub fn absences() -> &'static [Absent] {
        &[PROPERTY_ESCAPES, V_FLAG, CASE_FOLDING]
    }

    /// A sentence naming what is wrong, for a diagnostic.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            ErrorKind::UnknownFlag(ch) => crate::format!("`{ch}` is not a regular expression flag"),
            ErrorKind::DuplicateFlag(ch) => crate::format!("the flag `{ch}` appears twice"),
            ErrorKind::UnicodeAndUnicodeSets => {
                String::from("the `u` and `v` flags select different grammars and are exclusive")
            }
            ErrorKind::UnterminatedGroup => String::from("a group is not closed"),
            ErrorKind::UnterminatedClass => String::from("a character class is not closed"),
            ErrorKind::UnmatchedCloseParen => String::from("a `)` closes nothing"),
            ErrorKind::NothingToRepeat => String::from("a quantifier has nothing to repeat"),
            ErrorKind::QuantifierOutOfOrder => {
                String::from("a quantifier's minimum is greater than its maximum")
            }
            ErrorKind::LoneQuantifierBrace => {
                String::from("`{` does not begin a quantifier, and this profile omits Annex B")
            }
            ErrorKind::UnexpectedSyntaxCharacter(ch) => {
                crate::format!("`{ch}` must be escaped to be matched literally")
            }
            ErrorKind::TrailingBackslash => String::from("the pattern ends in a backslash"),
            ErrorKind::InvalidEscape => {
                String::from("this escape is not recognized, and this profile omits Annex B")
            }
            ErrorKind::InvalidBackreference => {
                String::from("a backreference names a group that does not exist")
            }
            ErrorKind::InvalidNamedReference => {
                String::from("`\\k` names a group that does not exist")
            }
            ErrorKind::DuplicateGroupName => String::from("two groups have the same name"),
            ErrorKind::InvalidGroupName => String::from("a group name is not an identifier"),
            ErrorKind::InvalidUnicodeEscape => String::from("a `\\u` escape is malformed"),
            ErrorKind::InvalidClassRange => String::from("a character class range is not in order"),
            ErrorKind::QuantifiedAssertion => String::from("an assertion cannot be quantified"),
            ErrorKind::PropertyEscapesUnavailable => String::from(
                "Unicode property escapes need General_Category and Script tables that are not \
                 present in this build",
            ),
            ErrorKind::UnicodeSetsUnavailable => {
                String::from("the `v` flag's set notation is not implemented")
            }
            ErrorKind::CaseFoldingUnavailable => String::from(
                "a case-insensitive class RANGE beyond ASCII needs a range-intersection predicate, \
                 and without `u` a cased character beyond ASCII needs a case MAPPING table; \
                 neither is present in this build",
            ),
        }
    }
}

/// The first code point outside ASCII, past which this profile cannot fold case.
const ASCII_LIMIT: u32 = 0x80;

/// Parses `source` under `flags`.
pub fn parse(source: &str, flags: Flags) -> Result<Pattern, Error> {
    if flags.unicode_sets {
        return Err(Error { kind: ErrorKind::UnicodeSetsUnavailable, at: 0 });
    }

    let units: Vec<u16> = source.encode_utf16().collect();
    let (groups, names) = prescan(&units)?;

    let mut parser = Parser { units: &units, pos: 0, flags, groups, names: &names, next_group: 0 };
    let node = parser.disjunction()?;
    if parser.pos < parser.units.len() {
        return Err(Error { kind: ErrorKind::UnmatchedCloseParen, at: parser.pos });
    }
    Ok(Pattern { node, groups, names })
}

/// Counts capturing groups and collects names without parsing.
///
/// It has to know just three things -- whether a backslash escaped the character, whether it is
/// inside a class, and whether a `(` is followed by `?` -- and nothing else about the grammar. A
/// prescan that tried to understand more would be a second parser, and the two would disagree.
///
/// It takes the mode for one reason: a group NAME may contain a `\u` escape, and resolving it is
/// what makes `(?<A>)` and its escaped spelling the same name to the duplicate check here.
fn prescan(units: &[u16]) -> Result<(u32, Vec<(String, u32)>), Error> {
    let mut groups = 0u32;
    let mut names: Vec<(String, u32)> = Vec::new();
    let mut index = 0usize;
    let mut in_class = false;

    while index < units.len() {
        let unit = units[index];
        match unit {
            0x5C => index += 1,
            0x5B if !in_class => in_class = true,
            0x5D if in_class => in_class = false,
            0x28 if !in_class => {
                let next = units.get(index + 1).copied();
                if next == Some(0x3F) {
                    let third = units.get(index + 2).copied();
                    let fourth = units.get(index + 3).copied();
                    if third == Some(0x3C) && fourth != Some(0x3D) && fourth != Some(0x21) {
                        groups += 1;
                        let (name, after) = read_group_name(units, index + 3)?;
                        if names.iter().any(|(existing, _)| *existing == name) {
                            return Err(Error {
                                kind: ErrorKind::DuplicateGroupName,
                                at: index + 3,
                            });
                        }
                        names.push((name, groups));
                        index = after - 1;
                    }
                } else {
                    groups += 1;
                }
            }
            _ => {}
        }
        index += 1;
    }

    Ok((groups, names))
}

/// Reads a group name starting after `(?<`, answering it and the index past the closing `>`.
///
/// It takes no mode. A name's alphabet is the language's identifier alphabet and its escapes are
/// always unicode-mode ([`name_escape`] says why), so nothing here depends on the pattern's flags.
fn read_group_name(units: &[u16], start: usize) -> Result<(String, usize), Error> {
    let mut name = String::new();
    let mut index = start;
    let mut first = true;
    loop {
        let unit = match units.get(index) {
            Some(unit) => *unit,
            None => return Err(Error { kind: ErrorKind::InvalidGroupName, at: start }),
        };
        if unit == 0x3E {
            if name.is_empty() {
                return Err(Error { kind: ErrorKind::InvalidGroupName, at: start });
            }
            return Ok((name, index + 1));
        }

        let (ch, width) = if unit == 0x5C {
            name_escape(units, index)?
        } else {
            read_code_point(units, index)
        };
        let value = char::from_u32(ch)
            .ok_or(Error { kind: ErrorKind::InvalidGroupName, at: index })?;
        let allowed =
            if first { lamella_identifier_start(ch) } else { lamella_identifier_continue(ch) };
        if !allowed {
            return Err(Error { kind: ErrorKind::InvalidGroupName, at: index });
        }
        name.push(value);
        first = false;
        index += width;
    }
}

/// `\ RegExpUnicodeEscapeSequence` inside a group name: the code point, and its width in units.
///
/// # THE ESCAPE IS ALWAYS READ IN UNICODE MODE, EVEN WHEN THE PATTERN IS NOT
///
/// ECMA-262 17th ed, 22.2.1 writes the production as
/// `RegExpIdentifierStart[UnicodeMode] :: \ RegExpUnicodeEscapeSequence[+UnicodeMode]`.
/// **The `[+UnicodeMode]` FOLLOWS the nonterminal, so it SETS the parameter rather than guarding
/// the alternative** -- unlike the `[~UnicodeMode]` that PRECEDES the surrogate-pair alternative
/// below it, which is a condition. So `u{ CodePoint }` and a lead-plus-trail pair of escapes are
/// available in a group name whatever flags the pattern carries, and `/(?<\u{1d4d1}>a)/` with no
/// `u` is legal.
///
/// Reading that as a condition costs exactly one test and looks right in every other position,
/// which is why the distinction is written down here rather than left to the grammar.
fn name_escape(units: &[u16], at: usize) -> Result<(u32, usize), Error> {
    let malformed = || Error { kind: ErrorKind::InvalidGroupName, at };
    if units.get(at + 1).copied() != Some(0x75) {
        return Err(malformed());
    }

    if units.get(at + 2).copied() == Some(0x7B) {
        let mut value: u32 = 0;
        let mut index = at + 3;
        let digits_from = index;
        while let Some(digit) = units.get(index).copied().and_then(hex_value) {
            value = value.saturating_mul(16).saturating_add(digit);
            if value > 0x10FFFF {
                return Err(malformed());
            }
            index += 1;
        }
        if index == digits_from || units.get(index).copied() != Some(0x7D) {
            return Err(malformed());
        }
        return Ok((value, index + 1 - at));
    }

    let lead = hex4(units, at + 2).ok_or_else(malformed)?;
    if (0xD800..0xDC00).contains(&lead) {
        let trail_at = at + 6;
        if units.get(trail_at).copied() == Some(0x5C)
            && units.get(trail_at + 1).copied() == Some(0x75)
        {
            if let Some(trail) = hex4(units, trail_at + 2) {
                if (0xDC00..0xE000).contains(&trail) {
                    let combined = 0x10000 + ((lead - 0xD800) << 10) + (trail - 0xDC00);
                    return Ok((combined, 12));
                }
            }
        }
    }
    Ok((lead, 6))
}

/// Exactly four hex digits at `at`, or `None` if any of them is missing or not a digit.
fn hex4(units: &[u16], at: usize) -> Option<u32> {
    let mut value = 0u32;
    for offset in 0..4 {
        value = value * 16 + hex_value(*units.get(at + offset)?)?;
    }
    Some(value)
}

/// `IdentifierStartChar`: `UnicodeIDStart`, plus `$` and `_` which the grammar names itself.
///
/// ECMA-262 17th ed, 22.2.1 -- `RegExpIdentifierStart :: IdentifierStartChar`, and 12.7 gives
/// `IdentifierStartChar :: UnicodeIDStart | $ | _`. A group name is an identifier, so the alphabet
/// is the LANGUAGE's, not a narrower one chosen here.
fn lamella_identifier_start(ch: u32) -> bool {
    if ch < 0x80 {
        return matches!(ch, 0x41..=0x5A | 0x61..=0x7A | 0x5F | 0x24);
    }
    lamella_unicode::is_id_start(ch)
}

/// `IdentifierPartChar`: `UnicodeIDContinue`, plus `$`, ZWNJ and ZWJ.
///
/// The last two are named by the grammar rather than read from a table. They are also in
/// ID_Continue as of UCD 16.0.0, so the branch is redundant today -- it stays because the standard
/// requires them whatever a later UCD revision does with the property.
fn lamella_identifier_continue(ch: u32) -> bool {
    if ch < 0x80 {
        return matches!(ch, 0x41..=0x5A | 0x61..=0x7A | 0x30..=0x39 | 0x5F | 0x24);
    }
    ch == 0x200C || ch == 0x200D || lamella_unicode::is_id_continue(ch)
}

/// Reads a code point at `index`, pairing surrogates. Used where the standard reads a code point
/// regardless of mode, such as inside a group name.
fn read_code_point(units: &[u16], index: usize) -> (u32, usize) {
    let first = units[index];
    if (0xD800..0xDC00).contains(&first) {
        if let Some(&second) = units.get(index + 1) {
            if (0xDC00..0xE000).contains(&second) {
                let combined =
                    0x10000 + ((u32::from(first) - 0xD800) << 10) + (u32::from(second) - 0xDC00);
                return (combined, 2);
            }
        }
    }
    (u32::from(first), 1)
}

struct Parser<'a> {
    units: &'a [u16],
    pos: usize,
    flags: Flags,
    groups: u32,
    names: &'a [(String, u32)],
    next_group: u32,
}

impl<'a> Parser<'a> {
    fn error<T>(&self, kind: ErrorKind) -> Result<T, Error> {
        Err(Error { kind, at: self.pos })
    }

    fn peek(&self) -> Option<u16> {
        self.units.get(self.pos).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u16> {
        self.units.get(self.pos + offset).copied()
    }

    fn eat(&mut self, unit: u16) -> bool {
        if self.peek() == Some(unit) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Reads one pattern character, pairing surrogates only in code-point mode.
    fn take_char(&mut self) -> u32 {
        if self.flags.code_point_mode() {
            let (ch, width) = read_code_point(self.units, self.pos);
            self.pos += width;
            ch
        } else {
            let unit = self.units[self.pos];
            self.pos += 1;
            u32::from(unit)
        }
    }

    fn disjunction(&mut self) -> Result<Node, Error> {
        let mut parts = Vec::new();
        parts.push(self.alternative()?);
        while self.eat(0x7C) {
            parts.push(self.alternative()?);
        }
        Ok(Node::alternate(parts))
    }

    fn alternative(&mut self) -> Result<Node, Error> {
        let mut parts = Vec::new();
        loop {
            match self.peek() {
                None | Some(0x7C) | Some(0x29) => break,
                _ => parts.push(self.term()?),
            }
        }
        Ok(Node::concat(parts))
    }

    fn term(&mut self) -> Result<Node, Error> {
        if let Some(assertion) = self.assertion()? {
            if self.at_quantifier() {
                return self.error(ErrorKind::QuantifiedAssertion);
            }
            return Ok(assertion);
        }

        let atom = self.atom()?;
        match self.quantifier()? {
            None => Ok(atom),
            Some((min, max, greed)) => {
                if let Some(max) = max {
                    if min > max {
                        return self.error(ErrorKind::QuantifierOutOfOrder);
                    }
                }
                Ok(Node::Repeat { node: Box::new(atom), min, max, greed })
            }
        }
    }

    /// Whether a quantifier begins here, without consuming it.
    fn at_quantifier(&self) -> bool {
        matches!(self.peek(), Some(0x2A) | Some(0x2B) | Some(0x3F) | Some(0x7B))
    }

    fn assertion(&mut self) -> Result<Option<Node>, Error> {
        match self.peek() {
            Some(0x5E) => {
                self.pos += 1;
                Ok(Some(Node::Assertion(Assertion::Start)))
            }
            Some(0x24) => {
                self.pos += 1;
                Ok(Some(Node::Assertion(Assertion::End)))
            }
            Some(0x5C) if self.peek_at(1) == Some(0x62) => {
                self.pos += 2;
                Ok(Some(Node::Assertion(Assertion::WordBoundary)))
            }
            Some(0x5C) if self.peek_at(1) == Some(0x42) => {
                self.pos += 2;
                Ok(Some(Node::Assertion(Assertion::NotWordBoundary)))
            }
            Some(0x28) if self.peek_at(1) == Some(0x3F) => {
                let (behind, negate) = match (self.peek_at(2), self.peek_at(3)) {
                    (Some(0x3D), _) => (false, false),
                    (Some(0x21), _) => (false, true),
                    (Some(0x3C), Some(0x3D)) => (true, false),
                    (Some(0x3C), Some(0x21)) => (true, true),
                    _ => return Ok(None),
                };
                self.pos += if behind { 4 } else { 3 };
                let node = self.disjunction()?;
                if !self.eat(0x29) {
                    return self.error(ErrorKind::UnterminatedGroup);
                }
                Ok(Some(Node::Look { behind, negate, node: Box::new(node) }))
            }
            _ => Ok(None),
        }
    }

    fn atom(&mut self) -> Result<Node, Error> {
        let unit = match self.peek() {
            Some(unit) => unit,
            None => return self.error(ErrorKind::NothingToRepeat),
        };

        match unit {
            0x2E => {
                self.pos += 1;
                Ok(Node::Any { dot_all: self.flags.dot_all })
            }
            0x5C => self.atom_escape(),
            0x5B => self.character_class(),
            0x28 => self.group(),
            0x2A | 0x2B | 0x3F => self.error(ErrorKind::NothingToRepeat),
            0x7B => self.error(ErrorKind::LoneQuantifierBrace),
            0x7D => self.error(ErrorKind::UnexpectedSyntaxCharacter('}')),
            0x5D => self.error(ErrorKind::UnexpectedSyntaxCharacter(']')),
            0x29 => self.error(ErrorKind::UnmatchedCloseParen),
            _ => {
                let ch = self.take_char();
                self.literal(ch)
            }
        }
    }

    /// Builds the node for a literal character, widening it when the pattern ignores case.
    fn literal(&mut self, ch: u32) -> Result<Node, Error> {
        if !self.flags.ignore_case {
            return Ok(Node::Char(ch));
        }
        if !affected_by_ignore_case(ch) {
            return Ok(Node::Char(ch));
        }
        if ch >= ASCII_LIMIT {
            if !self.flags.code_point_mode() {
                return self.error(ErrorKind::CaseFoldingUnavailable);
            }
            return Ok(Node::Char(ch));
        }
        let entries = self.fold_entries(ch);
        if entries.len() == 1 {
            Ok(Node::Char(ch))
        } else {
            Ok(Node::Class { entries, negated: false })
        }
    }

    /// Widens a member list when the pattern ignores case, and leaves it alone otherwise.
    ///
    /// Every producer of class members goes through here, so a member kind added later is widened
    /// without anyone having to remember that it should be.
    fn folded(&self, entries: Vec<ClassEntry>) -> Vec<ClassEntry> {
        if !self.flags.ignore_case {
            return entries;
        }
        widen_for_ignore_case(&entries, self.flags.code_point_mode())
    }

    /// Every character that canonicalizes to the same value as `ch`.
    fn fold_entries(&self, ch: u32) -> Vec<ClassEntry> {
        widen_for_ignore_case(&[ClassEntry::Single(ch)], self.flags.code_point_mode())
    }

    fn group(&mut self) -> Result<Node, Error> {
        self.pos += 1;

        if self.eat(0x3F) {
            if self.eat(0x3A) {
                let node = self.disjunction()?;
                if !self.eat(0x29) {
                    return self.error(ErrorKind::UnterminatedGroup);
                }
                return Ok(Node::Group { index: None, node: Box::new(node) });
            }
            if self.peek() == Some(0x3C) {
                let (_, after) = read_group_name(self.units, self.pos + 1)?;
                self.pos = after;
                self.next_group += 1;
                let index = self.next_group;
                let node = self.disjunction()?;
                if !self.eat(0x29) {
                    return self.error(ErrorKind::UnterminatedGroup);
                }
                return Ok(Node::Group { index: Some(index), node: Box::new(node) });
            }
            return self.error(ErrorKind::InvalidEscape);
        }

        self.next_group += 1;
        let index = self.next_group;
        let node = self.disjunction()?;
        if !self.eat(0x29) {
            return self.error(ErrorKind::UnterminatedGroup);
        }
        Ok(Node::Group { index: Some(index), node: Box::new(node) })
    }

    fn quantifier(&mut self) -> Result<Option<(u32, Option<u32>, Greed)>, Error> {
        let (min, max) = match self.peek() {
            Some(0x2A) => {
                self.pos += 1;
                (0, None)
            }
            Some(0x2B) => {
                self.pos += 1;
                (1, None)
            }
            Some(0x3F) => {
                self.pos += 1;
                (0, Some(1))
            }
            Some(0x7B) => match self.braced_quantifier()? {
                Some(bounds) => bounds,
                None => return self.error(ErrorKind::LoneQuantifierBrace),
            },
            _ => return Ok(None),
        };

        let greed = if self.eat(0x3F) { Greed::Lazy } else { Greed::Greedy };
        Ok(Some((min, max, greed)))
    }

    /// Reads `{n}`, `{n,}` or `{n,m}`. Answers `None` without consuming when the braces do not
    /// form one, so the caller can report the position of the `{` itself.
    fn braced_quantifier(&mut self) -> Result<Option<(u32, Option<u32>)>, Error> {
        let start = self.pos;
        self.pos += 1;

        let min = match self.decimal_digits() {
            Some(value) => value,
            None => {
                self.pos = start;
                return Ok(None);
            }
        };

        if self.eat(0x7D) {
            return Ok(Some((min, Some(min))));
        }

        if self.eat(0x2C) {
            if self.eat(0x7D) {
                return Ok(Some((min, None)));
            }
            let max = match self.decimal_digits() {
                Some(value) => value,
                None => {
                    self.pos = start;
                    return Ok(None);
                }
            };
            if self.eat(0x7D) {
                return Ok(Some((min, Some(max))));
            }
        }

        self.pos = start;
        Ok(None)
    }

    /// Reads a run of decimal digits, saturating rather than overflowing.
    ///
    /// A bound larger than the subject can ever be is indistinguishable from any other bound that
    /// large, so saturating keeps `{1,4294967296}` meaningful instead of wrapping it to something
    /// smaller than its minimum.
    fn decimal_digits(&mut self) -> Option<u32> {
        let start = self.pos;
        let mut value: u32 = 0;
        while let Some(unit) = self.peek() {
            if !(0x30..=0x39).contains(&unit) {
                break;
            }
            value = value.saturating_mul(10).saturating_add(u32::from(unit) - 0x30);
            self.pos += 1;
        }
        if self.pos == start {
            None
        } else {
            Some(value)
        }
    }
}

/// The remaining productions: escapes, and the character class.
impl Parser<'_> {
    /// `\` followed by something that is not `b` or `B`, both of which are assertions.
    fn atom_escape(&mut self) -> Result<Node, Error> {
        let start = self.pos;
        self.pos += 1;

        let unit = match self.peek() {
            Some(unit) => unit,
            None => {
                self.pos = start;
                return self.error(ErrorKind::TrailingBackslash);
            }
        };

        if let Some((entries, negated)) = self.class_escape_set(unit)? {
            return Ok(Node::Class { entries, negated });
        }

        if (0x31..=0x39).contains(&unit) {
            let value = self.decimal_digits().unwrap_or(0);
            if value == 0 || value > self.groups {
                self.pos = start;
                return self.error(ErrorKind::InvalidBackreference);
            }
            return Ok(Node::Backreference(value));
        }

        if unit == 0x6B && !self.names.is_empty() {
            self.pos += 1;
            if self.peek() != Some(0x3C) {
                self.pos = start;
                return self.error(ErrorKind::InvalidNamedReference);
            }
            let (name, after) = read_group_name(self.units, self.pos + 1)?;
            self.pos = after;
            match self.names.iter().find(|(existing, _)| *existing == name) {
                Some((_, index)) => return Ok(Node::Backreference(*index)),
                None => {
                    self.pos = start;
                    return self.error(ErrorKind::InvalidNamedReference);
                }
            }
        }

        let ch = self.character_escape()?;
        self.literal(ch)
    }

    /// A `CharacterEscape`, which is every escape that denotes one character.
    fn character_escape(&mut self) -> Result<u32, Error> {
        let unit = self.units[self.pos];
        match unit {
            0x66 => {
                self.pos += 1;
                Ok(0x0C)
            }
            0x6E => {
                self.pos += 1;
                Ok(0x0A)
            }
            0x72 => {
                self.pos += 1;
                Ok(0x0D)
            }
            0x74 => {
                self.pos += 1;
                Ok(0x09)
            }
            0x76 => {
                self.pos += 1;
                Ok(0x0B)
            }
            0x63 => {
                let letter = self.peek_at(1).unwrap_or(0);
                let is_letter = (0x41..=0x5A).contains(&letter) || (0x61..=0x7A).contains(&letter);
                if !is_letter {
                    return self.error(ErrorKind::InvalidEscape);
                }
                self.pos += 2;
                Ok(u32::from(letter) % 32)
            }
            0x30 => {
                let next = self.peek_at(1).unwrap_or(0);
                if (0x30..=0x39).contains(&next) {
                    return self.error(ErrorKind::InvalidEscape);
                }
                self.pos += 1;
                Ok(0)
            }
            0x78 => {
                self.pos += 1;
                match self.hex_digits(2) {
                    Some(value) => Ok(value),
                    None => self.error(ErrorKind::InvalidEscape),
                }
            }
            0x75 => self.unicode_escape(),
            _ => self.identity_escape(),
        }
    }

    /// `\uHHHH`, a surrogate PAIR of them in code-point mode, or `\u{...}`.
    ///
    /// The pair case is the one worth naming: under `u` a lead escape followed by a trail escape
    /// denotes one astral character rather than two lone surrogates, so the second escape has to be
    /// consumed by the first. Without `u` the same source is two atoms, which is why this reads the
    /// mode.
    fn unicode_escape(&mut self) -> Result<u32, Error> {
        self.pos += 1;

        if self.peek() == Some(0x7B) {
            if !self.flags.code_point_mode() {
                return self.error(ErrorKind::InvalidUnicodeEscape);
            }
            self.pos += 1;
            let mut value: u32 = 0;
            let start = self.pos;
            while let Some(unit) = self.peek() {
                match hex_value(unit) {
                    Some(digit) => {
                        value = value.saturating_mul(16).saturating_add(digit);
                        if value > 0x10FFFF {
                            return self.error(ErrorKind::InvalidUnicodeEscape);
                        }
                        self.pos += 1;
                    }
                    None => break,
                }
            }
            if self.pos == start || !self.eat(0x7D) {
                return self.error(ErrorKind::InvalidUnicodeEscape);
            }
            return Ok(value);
        }

        let first = match self.hex_digits(4) {
            Some(value) => value,
            None => return self.error(ErrorKind::InvalidUnicodeEscape),
        };

        if self.flags.code_point_mode() && (0xD800..0xDC00).contains(&first) {
            let saved = self.pos;
            if self.eat(0x5C) && self.eat(0x75) {
                if let Some(second) = self.hex_digits(4) {
                    if (0xDC00..0xE000).contains(&second) {
                        return Ok(0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00));
                    }
                }
            }
            self.pos = saved;
        }

        Ok(first)
    }

    /// An escape that stands for the character itself.
    ///
    /// Under `u` only the syntax characters and `/` may be escaped this way, so `\q` is an error.
    /// Without `u` almost anything may be, which is the standard's own concession to existing
    /// patterns -- but `c` is excluded so a malformed control escape does not become a literal
    /// `c`, and `k` is excluded when the pattern has named groups so `\k` cannot mean the letter.
    fn identity_escape(&mut self) -> Result<u32, Error> {
        let ch = self.take_char();
        if self.flags.code_point_mode() {
            let allowed = matches!(
                ch,
                0x5E | 0x24 | 0x5C | 0x2E | 0x2A | 0x2B | 0x3F | 0x28 | 0x29 | 0x5B | 0x5D | 0x7B
                    | 0x7D | 0x7C | 0x2F
            );
            if !allowed {
                self.pos -= 1;
                return self.error(ErrorKind::InvalidEscape);
            }
            return Ok(ch);
        }
        if ch == 0x63 || (ch == 0x6B && !self.names.is_empty()) {
            self.pos -= 1;
            return self.error(ErrorKind::InvalidEscape);
        }
        Ok(ch)
    }

    /// The set an escape like `\d` denotes, or `None` when the escape is not one of them.
    fn class_escape_set(&mut self, unit: u16) -> Result<Option<(Vec<ClassEntry>, bool)>, Error> {
        let set = match unit {
            0x64 => (self.folded(digit_entries()), false),
            0x44 => (self.folded(digit_entries()), true),
            0x77 => (self.folded(word_entries()), false),
            0x57 => (self.folded(word_entries()), true),
            0x73 => (self.folded(space_entries()), false),
            0x53 => (self.folded(space_entries()), true),
            0x70 | 0x50 if self.flags.code_point_mode() => {
                return self.error(ErrorKind::PropertyEscapesUnavailable)
            }
            _ => return Ok(None),
        };
        self.pos += 1;
        Ok(Some(set))
    }

    fn hex_digits(&mut self, count: usize) -> Option<u32> {
        let mut value: u32 = 0;
        for offset in 0..count {
            let digit = hex_value(self.peek_at(offset)?)?;
            value = value * 16 + digit;
        }
        self.pos += count;
        Some(value)
    }

    /// `[ ^? ClassRanges ]`
    fn character_class(&mut self) -> Result<Node, Error> {
        self.pos += 1;
        let negated = self.eat(0x5E);
        let mut entries: Vec<ClassEntry> = Vec::new();

        loop {
            match self.peek() {
                None => return self.error(ErrorKind::UnterminatedClass),
                Some(0x5D) => {
                    self.pos += 1;
                    break;
                }
                _ => {}
            }

            let low = self.class_atom()?;

            let is_range = self.peek() == Some(0x2D) && self.peek_at(1) != Some(0x5D);
            if !is_range {
                self.push_atom(&mut entries, low)?;
                continue;
            }

            self.pos += 1;
            let high = self.class_atom()?;

            match (low, high) {
                (ClassAtom::Char(low), ClassAtom::Char(high)) => {
                    if low > high {
                        return self.error(ErrorKind::InvalidClassRange);
                    }
                    if self.flags.ignore_case && (low >= ASCII_LIMIT || high >= ASCII_LIMIT) {
                        return self.error(ErrorKind::CaseFoldingUnavailable);
                    }
                    entries.extend(self.folded(crate::vec![ClassEntry::Range(low, high)]));
                }
                _ => return self.error(ErrorKind::InvalidClassRange),
            }
        }

        Ok(Node::Class { entries, negated })
    }

    /// One member of a class: a character, or a set an escape names.
    fn class_atom(&mut self) -> Result<ClassAtom, Error> {
        if self.peek() != Some(0x5C) {
            let ch = self.take_char();
            return Ok(ClassAtom::Char(ch));
        }

        self.pos += 1;
        let unit = match self.peek() {
            Some(unit) => unit,
            None => return self.error(ErrorKind::TrailingBackslash),
        };

        if let Some((entries, negated)) = self.class_escape_set(unit)? {
            return Ok(ClassAtom::Set { entries, negated });
        }

        if unit == 0x62 {
            self.pos += 1;
            return Ok(ClassAtom::Char(0x08));
        }

        if unit == 0x2D {
            self.pos += 1;
            return Ok(ClassAtom::Char(0x2D));
        }

        Ok(ClassAtom::Char(self.character_escape()?))
    }

    /// Adds a single class member, widening it when the pattern ignores case.
    fn push_atom(&self, entries: &mut Vec<ClassEntry>, atom: ClassAtom) -> Result<(), Error> {
        match atom {
            ClassAtom::Char(ch) => {
                if self.flags.ignore_case {
                    if !affected_by_ignore_case(ch) {
                        entries.push(ClassEntry::Single(ch));
                        return Ok(());
                    }
                    if ch >= ASCII_LIMIT {
                        if !self.flags.code_point_mode() {
                            return Err(Error {
                                kind: ErrorKind::CaseFoldingUnavailable,
                                at: self.pos,
                            });
                        }
                        entries.push(ClassEntry::Single(ch));
                        return Ok(());
                    }
                    entries.extend(self.fold_entries(ch));
                } else {
                    entries.push(ClassEntry::Single(ch));
                }
                Ok(())
            }
            ClassAtom::Set { entries: members, negated } => {
                if negated {
                    entries.extend(complement(&members));
                } else {
                    entries.extend(members);
                }
                Ok(())
            }
        }
    }
}

/// A class member before it is folded into the entry list.
enum ClassAtom {
    Char(u32),
    Set { entries: Vec<ClassEntry>, negated: bool },
}

/// The complement of a member list over the whole code-point space.
fn complement(members: &[ClassEntry]) -> Vec<ClassEntry> {
    let mut bounds: Vec<(u32, u32)> = members
        .iter()
        .map(|entry| match entry {
            ClassEntry::Single(value) => (*value, *value),
            ClassEntry::Range(low, high) => (*low, *high),
        })
        .collect();
    bounds.sort_unstable();

    let mut out = Vec::new();
    let mut cursor: u32 = 0;
    for (low, high) in bounds {
        if low > cursor {
            out.push(ClassEntry::Range(cursor, low - 1));
        }
        cursor = cursor.max(high.saturating_add(1));
    }
    if cursor <= 0x10FFFF {
        out.push(ClassEntry::Range(cursor, 0x10FFFF));
    }
    out
}

/// Widens a member list so it also contains the case counterparts of everything in it.
///
/// # ONE FUNCTION, BECAUSE EVERY CALLER THAT REPEATS IT IS A CHANCE TO FORGET
///
/// Case widening is needed wherever a pattern can name a character: a literal, an explicit range,
/// and the digit, word and space shorthands. Written per caller it goes missing at whichever site
/// is added last, and nothing about the sites that DO have it advertises the one that does not.
///
/// So the widening is one function over a member LIST, and every producer of members ends in it.
/// A new kind of class member is widened by construction rather than by remembering.
///
/// Under `u` two characters fold into ASCII from outside it -- the Kelvin sign onto `k` and the
/// long s onto `s`. Without `u` the standard's canonicalization deliberately excludes both, by
/// refusing a mapping that takes a non-ASCII character to an ASCII one, so the same pattern must
/// NOT match them.
/// Whether `i` mode can change what this code point matches.
///
/// # ONE IMPLEMENTATION, BECAUSE SEVERAL CALLERS ASK THE SAME QUESTION
///
/// A bare literal and a class member both need it, and a rule with more than one implementation gains a new
/// case in neither. ASCII is folded by the tables below and is never the caller's concern here.
///
/// **The predicate is the UCD's `Cased` derived property**, which is a CONSERVATIVE
/// over-approximation of what the caller needs, and conservative in the safe direction: a few code
/// points are `Cased` yet have no case mapping, so they are refused where they could have been
/// emitted. Refusing too much is a published gap; emitting too much is a wrong answer.
///
/// The other direction is what would be unsafe -- a code point reported caseless that some OTHER
/// code point canonicalizes to, which `/X/i` would then have to match and would not. **Checked
/// exhaustively rather than reasoned about: of the 2,822 non-ASCII code points that are the target
/// of any other code point's case mapping, `is_cased` reports every single one as cased.**
fn affected_by_ignore_case(ch: u32) -> bool {
    ch < ASCII_LIMIT || lamella_unicode::is_cased(ch)
}

fn widen_for_ignore_case(entries: &[ClassEntry], code_point_mode: bool) -> Vec<ClassEntry> {
    let mut out: Vec<ClassEntry> = entries.to_vec();

    for entry in entries {
        let (low, high) = match entry {
            ClassEntry::Single(value) => (*value, *value),
            ClassEntry::Range(low, high) => (*low, *high),
        };
        let overlaps = |a: u32, b: u32| low.max(a) <= high.min(b);

        if overlaps(0x41, 0x5A) {
            out.push(ClassEntry::Range(low.max(0x41) + 0x20, high.min(0x5A) + 0x20));
        }
        if overlaps(0x61, 0x7A) {
            out.push(ClassEntry::Range(low.max(0x61) - 0x20, high.min(0x7A) - 0x20));
        }
        if code_point_mode {
            if overlaps(0x4B, 0x4B) || overlaps(0x6B, 0x6B) {
                out.push(ClassEntry::Single(0x212A));
            }
            if overlaps(0x53, 0x53) || overlaps(0x73, 0x73) {
                out.push(ClassEntry::Single(0x017F));
            }
        }
    }

    out
}

fn hex_value(unit: u16) -> Option<u32> {
    let value = u32::from(unit);
    match unit {
        0x30..=0x39 => Some(value - 0x30),
        0x41..=0x46 => Some(value - 0x41 + 10),
        0x61..=0x66 => Some(value - 0x61 + 10),
        _ => None,
    }
}

/// `\d` -- fixed to ASCII digits in every mode, which is a place ECMAScript and Python differ.
fn digit_entries() -> Vec<ClassEntry> {
    crate::vec![ClassEntry::Range(0x30, 0x39)]
}

/// `\w` -- fixed to ASCII word characters, likewise.
fn word_entries() -> Vec<ClassEntry> {
    crate::vec![
        ClassEntry::Range(0x30, 0x39),
        ClassEntry::Range(0x41, 0x5A),
        ClassEntry::Single(0x5F),
        ClassEntry::Range(0x61, 0x7A),
    ]
}

/// `\s` -- the standard's WhiteSpace joined with its LineTerminator.
///
/// It is NOT the Unicode White_Space property and the difference runs both ways: U+0085 is
/// White_Space and is not ECMAScript whitespace, while U+FEFF is ECMAScript whitespace and is not
/// White_Space. Substituting the Unicode property would be wrong in two directions on a small,
/// specific set of inputs.
fn space_entries() -> Vec<ClassEntry> {
    let mut entries: Vec<ClassEntry> = WHITE_SPACE.to_vec();
    entries.extend_from_slice(LINE_TERMINATOR);
    entries
}

/// ECMAScript `WhiteSpace`, as the lexical grammar defines it.
///
/// # IT IS PUBLISHED BECAUSE A LEXER NEEDS THE SAME SET
///
/// A regular expression needs these code points for `\s`, and a tokenizer needs them to skip
/// between tokens. They are the same set from the same clause of the standard, so a scanner that
/// keeps its own copy is one clause revision away from disagreeing with the matcher beside it.
/// A consumer depends on this crate, so the set lives at the bottom and both read it.
///
/// The members are enumerated rather than looked up in a Unicode table because there are eleven of
/// them and they are stable, unlike the identifier properties, which are tens of thousands.
pub const WHITE_SPACE: &[ClassEntry] = &[
    ClassEntry::Single(0x09),
    ClassEntry::Single(0x0B),
    ClassEntry::Single(0x0C),
    ClassEntry::Single(0x20),
    ClassEntry::Single(0xA0),
    ClassEntry::Single(0x1680),
    ClassEntry::Range(0x2000, 0x200A),
    ClassEntry::Single(0x202F),
    ClassEntry::Single(0x205F),
    ClassEntry::Single(0x3000),
    ClassEntry::Single(0xFEFF),
];

/// ECMAScript `LineTerminator`, which is a separate production from [`WHITE_SPACE`].
///
/// The two are joined for `\s` and kept apart everywhere else: a line terminator ends a
/// single-line comment and triggers automatic semicolon insertion, and ordinary whitespace does
/// neither. An engine that folded them together would insert semicolons where a space appears.
pub const LINE_TERMINATOR: &[ClassEntry] = &[
    ClassEntry::Single(0x0A),
    ClassEntry::Single(0x0D),
    ClassEntry::Range(0x2028, 0x2029),
];

/// Whether a code point is ECMAScript `WhiteSpace`.
#[must_use]
pub fn is_white_space(ch: u32) -> bool {
    WHITE_SPACE.iter().any(|entry| entry.contains(ch))
}

/// Whether a code point is an ECMAScript `LineTerminator`.
#[must_use]
pub fn is_line_terminator(ch: u32) -> bool {
    LINE_TERMINATOR.iter().any(|entry| entry.contains(ch))
}
