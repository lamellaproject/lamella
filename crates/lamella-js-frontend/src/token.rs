//! Tokens, and the goal symbol the parser must supply to get one.

use crate::{String};
use crate::source::Span;
use crate::string_value::JsString;

/// Which lexical goal the parser expects at this position.
///
/// Passed in at every request rather than inferred, because inferring it is the heuristic table
/// above and the table is wrong in cases real programs contain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Goal {
    /// `InputElementDiv`: a `/` here is division or `/=`.
    Div,
    /// `InputElementRegExp`: a `/` here opens a RegExp literal.
    RegExp,
    /// `InputElementTemplateTail`: a `}` here closes a substitution and resumes template text.
    TemplateTail,
}

/// What a token is.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// Any `IdentifierName`, reserved words included. See the module note on why.
    Identifier(String),
    /// A private name, `#x`. Lexed as one token because `# x` is not one, and because an undeclared
    /// private access is an EARLY error -- which needs the name available at parse time.
    PrivateName(String),
    Punctuator(Punctuator),
    /// A numeric literal's value, already converted.
    Number(f64),
    /// A `BigInt` literal, kept as its digit text.
    ///
    /// Recognized rather than evaluated: BigInt is a second numeric tower and is excluded from the
    /// profile. Parsing it and refusing loudly is the model; lexing `1n` as the number 1
    /// followed by the identifier `n` would be the silent wrong answer.
    BigInt(String),
    /// A string literal's value: UTF-16 code units, lone surrogates included.
    /// See [`JsString`] for why this is not a Rust `String`.
    String(JsString),
    Template(TemplatePart),
    /// A RegExp literal, kept as body and flags text. The pattern grammar is a separate subject and
    /// is not parsed here.
    RegExp { body: String, flags: String },
    EndOfFile,
}

/// Which piece of a template literal this is.
///
/// `` `a${b}c${d}e` `` is Head(`a`), then an expression, then Middle(`c`), then an expression, then
/// Tail(`e`). `` `plain` `` is a single [`TemplatePart::NoSubstitution`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateKind {
    NoSubstitution,
    Head,
    Middle,
    Tail,
}

/// One template chunk, carrying **both** of its values.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplatePart {
    pub kind: TemplateKind,
    /// The escape-processed value.
    ///
    /// `None` when the chunk contains an invalid escape sequence. **That is not an error here**:
    /// since ES2018 an invalid escape in a TAGGED template is legal and its cooked value is
    /// `undefined`, while the raw text survives. Untagged, the same chunk is a syntax error. The
    /// lexer cannot know which it is -- `[Tagged]` is a grammar parameter -- so it records the fact
    /// and lets the parser, which knows, decide. Collapsing this to an error in the lexer would make
    /// legal tagged templates unparseable.
    pub cooked: Option<JsString>,
    /// The source text between the delimiters, with `\r\n` and `\r` normalized to `\n`.
    ///
    /// The normalization applies to **raw as well as cooked** -- a rule that surprises people who
    /// assume "raw" means "the bytes as written".
    pub raw: String,
}

/// One scanned token.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// **Whether a LineTerminator appeared before this token.** The single input automatic semicolon
    /// insertion runs on, and the reason it is on the token rather than recomputed later: by the
    /// time the parser wants to know, the whitespace is behind it.
    pub preceded_by_line_terminator: bool,
    /// Whether the token's text contained a Unicode escape.
    ///
    /// Only meaningful for identifiers, where it decides that the token **cannot** be used where a
    /// keyword is required -- `if` spelled `if` is a SyntaxError in statement position and a
    /// fine property name two characters later.
    pub had_escape: bool,
}

impl Token {
    #[must_use]
    pub fn is_punctuator(&self, punctuator: Punctuator) -> bool {
        self.kind == TokenKind::Punctuator(punctuator)
    }

    /// The identifier's text, if this token is one **and could be a keyword**.
    ///
    /// Returns `None` for an escaped identifier, which is what makes the keyword rule a property of
    /// this accessor rather than a check every caller has to remember.
    #[must_use]
    pub fn keyword_text(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Identifier(name) if !self.had_escape => Some(name),
            _ => None,
        }
    }
}

/// Every punctuator in the lexical grammar.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Punctuator {
    OpenBrace, CloseBrace, OpenParen, CloseParen, OpenBracket, CloseBracket,
    Dot, Ellipsis, Semicolon, Comma, Colon, Question,
    /// `?.` -- and see [`crate::lexer`] for the digit lookahead that keeps `a?.3:b` a ternary.
    QuestionDot,
    QuestionQuestion, QuestionQuestionEquals,
    Arrow,
    LessThan, GreaterThan, LessThanEquals, GreaterThanEquals,
    EqualsEquals, ExclamationEquals, EqualsEqualsEquals, ExclamationEqualsEquals,
    Plus, Minus, Star, Percent, StarStar,
    PlusPlus, MinusMinus,
    LessThanLessThan, GreaterThanGreaterThan, GreaterThanGreaterThanGreaterThan,
    Ampersand, Bar, Caret, Exclamation, Tilde,
    AmpersandAmpersand, BarBar,
    Equals,
    PlusEquals, MinusEquals, StarEquals, PercentEquals, StarStarEquals,
    LessThanLessThanEquals, GreaterThanGreaterThanEquals, GreaterThanGreaterThanGreaterThanEquals,
    AmpersandEquals, BarEquals, CaretEquals,
    AmpersandAmpersandEquals, BarBarEquals,
    Slash, SlashEquals,
}

impl Punctuator {
    /// The source spelling, for diagnostics.
    #[must_use]
    pub fn spelling(self) -> &'static str {
        use Punctuator::*;
        match self {
            OpenBrace => "{", CloseBrace => "}", OpenParen => "(", CloseParen => ")",
            OpenBracket => "[", CloseBracket => "]",
            Dot => ".", Ellipsis => "...", Semicolon => ";", Comma => ",", Colon => ":",
            Question => "?", QuestionDot => "?.", QuestionQuestion => "??",
            QuestionQuestionEquals => "??=", Arrow => "=>",
            LessThan => "<", GreaterThan => ">", LessThanEquals => "<=", GreaterThanEquals => ">=",
            EqualsEquals => "==", ExclamationEquals => "!=",
            EqualsEqualsEquals => "===", ExclamationEqualsEquals => "!==",
            Plus => "+", Minus => "-", Star => "*", Percent => "%", StarStar => "**",
            PlusPlus => "++", MinusMinus => "--",
            LessThanLessThan => "<<", GreaterThanGreaterThan => ">>",
            GreaterThanGreaterThanGreaterThan => ">>>",
            Ampersand => "&", Bar => "|", Caret => "^", Exclamation => "!", Tilde => "~",
            AmpersandAmpersand => "&&", BarBar => "||",
            Equals => "=",
            PlusEquals => "+=", MinusEquals => "-=", StarEquals => "*=", PercentEquals => "%=",
            StarStarEquals => "**=",
            LessThanLessThanEquals => "<<=", GreaterThanGreaterThanEquals => ">>=",
            GreaterThanGreaterThanGreaterThanEquals => ">>>=",
            AmpersandEquals => "&=", BarEquals => "|=", CaretEquals => "^=",
            AmpersandAmpersandEquals => "&&=", BarBarEquals => "||=",
            Slash => "/", SlashEquals => "/=",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identifier(name: &str, had_escape: bool) -> Token {
        Token {
            kind: TokenKind::Identifier(name.to_string()),
            span: Span::default(),
            preceded_by_line_terminator: false,
            had_escape,
        }
    }

    /// THE DEFECT THIS GUARDS: `if` spells `if`, and a parser that compares the decoded
    /// text against a keyword table accepts it as the keyword. The standard says it is an ordinary
    /// identifier there and a SyntaxError where a keyword is required.
    #[test]
    fn an_escaped_identifier_can_never_be_read_as_a_keyword() {
        assert_eq!(identifier("if", false).keyword_text(), Some("if"));
        assert_eq!(
            identifier("if", true).keyword_text(),
            None,
            "spelled with an escape it is an identifier, whatever it decodes to"
        );
    }

    #[test]
    fn every_punctuator_has_a_distinct_spelling() {
        use Punctuator::*;
        let all = [
            OpenBrace, CloseBrace, OpenParen, CloseParen, OpenBracket, CloseBracket, Dot, Ellipsis,
            Semicolon, Comma, Colon, Question, QuestionDot, QuestionQuestion, QuestionQuestionEquals,
            Arrow, LessThan, GreaterThan, LessThanEquals, GreaterThanEquals, EqualsEquals,
            ExclamationEquals, EqualsEqualsEquals, ExclamationEqualsEquals, Plus, Minus, Star,
            Percent, StarStar, PlusPlus, MinusMinus, LessThanLessThan, GreaterThanGreaterThan,
            GreaterThanGreaterThanGreaterThan, Ampersand, Bar, Caret, Exclamation, Tilde,
            AmpersandAmpersand, BarBar, Equals, PlusEquals, MinusEquals, StarEquals, PercentEquals,
            StarStarEquals, LessThanLessThanEquals, GreaterThanGreaterThanEquals,
            GreaterThanGreaterThanGreaterThanEquals, AmpersandEquals, BarEquals, CaretEquals,
            AmpersandAmpersandEquals, BarBarEquals, Slash, SlashEquals,
        ];
        let mut seen = alloc::collections::BTreeSet::new();
        for punctuator in all {
            assert!(seen.insert(punctuator.spelling()), "{punctuator:?} duplicates a spelling");
        }
        assert_eq!(seen.len(), all.len());
    }
}
