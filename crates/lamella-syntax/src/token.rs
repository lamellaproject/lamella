//! Tokens: the lexical elements of C#.

use crate::span::Span;
use alloc::boxed::Box;

/// Defines a small enum over a fixed set of textual symbols, together with the
/// spelling lookups in both directions and the full list. Driving all three
/// from one table keeps a variant, its spelling, and the reverse lookup from
/// ever drifting apart, and a count test on the result guards against a missing
/// or duplicated entry.
macro_rules! spelled_enum {
    (
        $(#[$enum_meta:meta])*
        $vis:vis enum $name:ident {
            $( $text:literal => $variant:ident ),+ $(,)?
        }
    ) => {
        $(#[$enum_meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name {
            $(
                #[doc = concat!("The `", $text, "` token.")]
                $variant,
            )+
        }

        impl $name {
            /// The exact source spelling of this token.
            #[must_use]
            pub fn as_str(self) -> &'static str {
                match self {
                    $( $name::$variant => $text, )+
                }
            }

            /// Returns the token whose spelling is exactly `text`, if any. The
            /// match is case-sensitive, as the lexical grammar requires.
            #[must_use]
            pub fn from_text(text: &str) -> Option<$name> {
                match text {
                    $( $text => Some($name::$variant), )+
                    _ => None,
                }
            }

            /// Every member, in declaration order.
            #[must_use]
            pub fn all() -> &'static [$name] {
                &[ $( $name::$variant, )+ ]
            }
        }
    };
}

spelled_enum! {
    /// A C# keyword: a reserved, identifier-like word (ECMA-334 1st ed, 9.4.3).
    ///
    /// `true`, `false`, and `null` are keywords here, exactly as the
    /// specification lists them; the parser treats those three as literal
    /// expressions rather than introducing separate literal tokens for them.
    pub enum Keyword {
        "abstract" => Abstract,
        "as" => As,
        "base" => Base,
        "bool" => Bool,
        "break" => Break,
        "byte" => Byte,
        "case" => Case,
        "catch" => Catch,
        "char" => Char,
        "checked" => Checked,
        "class" => Class,
        "const" => Const,
        "continue" => Continue,
        "decimal" => Decimal,
        "default" => Default,
        "delegate" => Delegate,
        "do" => Do,
        "double" => Double,
        "else" => Else,
        "enum" => Enum,
        "event" => Event,
        "explicit" => Explicit,
        "extern" => Extern,
        "false" => False,
        "finally" => Finally,
        "fixed" => Fixed,
        "float" => Float,
        "for" => For,
        "foreach" => Foreach,
        "goto" => Goto,
        "if" => If,
        "implicit" => Implicit,
        "in" => In,
        "int" => Int,
        "interface" => Interface,
        "internal" => Internal,
        "is" => Is,
        "lock" => Lock,
        "long" => Long,
        "namespace" => Namespace,
        "new" => New,
        "null" => Null,
        "object" => Object,
        "operator" => Operator,
        "out" => Out,
        "override" => Override,
        "params" => Params,
        "private" => Private,
        "protected" => Protected,
        "public" => Public,
        "readonly" => Readonly,
        "ref" => Ref,
        "return" => Return,
        "sbyte" => Sbyte,
        "sealed" => Sealed,
        "short" => Short,
        "sizeof" => Sizeof,
        "stackalloc" => Stackalloc,
        "static" => Static,
        "string" => String,
        "struct" => Struct,
        "switch" => Switch,
        "this" => This,
        "throw" => Throw,
        "true" => True,
        "try" => Try,
        "typeof" => Typeof,
        "uint" => Uint,
        "ulong" => Ulong,
        "unchecked" => Unchecked,
        "unsafe" => Unsafe,
        "ushort" => Ushort,
        "using" => Using,
        "virtual" => Virtual,
        "void" => Void,
        "volatile" => Volatile,
        "while" => While,
    }
}

spelled_enum! {
    /// A C# operator or punctuator (ECMA-334 1st ed, 9.4.5).
    ///
    /// The scanner recognises these by maximal munch, always taking the longest
    /// match, so `>>=` wins over `>>` which wins over `>`.
    pub enum Punctuator {
        "{" => OpenBrace,
        "}" => CloseBrace,
        "[" => OpenBracket,
        "]" => CloseBracket,
        "(" => OpenParen,
        ")" => CloseParen,
        "." => Dot,
        "," => Comma,
        ":" => Colon,
        ";" => Semicolon,
        "+" => Plus,
        "-" => Minus,
        "*" => Asterisk,
        "/" => Slash,
        "%" => Percent,
        "&" => Ampersand,
        "|" => Bar,
        "^" => Caret,
        "!" => Exclamation,
        "~" => Tilde,
        "=" => Equals,
        "<" => LessThan,
        ">" => GreaterThan,
        "?" => Question,
        "++" => PlusPlus,
        "--" => MinusMinus,
        "&&" => AmpersandAmpersand,
        "||" => BarBar,
        "<<" => LessThanLessThan,
        ">>" => GreaterThanGreaterThan,
        "==" => EqualsEquals,
        "!=" => ExclamationEquals,
        "<=" => LessThanEquals,
        ">=" => GreaterThanEquals,
        "+=" => PlusEquals,
        "-=" => MinusEquals,
        "*=" => AsteriskEquals,
        "/=" => SlashEquals,
        "%=" => PercentEquals,
        "&=" => AmpersandEquals,
        "|=" => BarEquals,
        "^=" => CaretEquals,
        "<<=" => LessThanLessThanEquals,
        ">>=" => GreaterThanGreaterThanEquals,
        "->" => Arrow,
    }
}

/// The type suffix on an integer literal (9.4.4.2), which constrains its type.
/// The exact type (int, uint, long, or ulong) is chosen during binding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IntegerSuffix {
    /// No suffix.
    None,
    /// `u` or `U`: the literal is uint or ulong.
    Unsigned,
    /// `l` or `L`: the literal is long or ulong.
    Long,
    /// A `u`/`U` combined with an `l`/`L`, in either order: the literal is ulong.
    UnsignedLong,
}

/// The type suffix on a real literal (9.4.4.3), which fixes its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RealSuffix {
    /// No suffix: the literal is double.
    None,
    /// `f` or `F`: the literal is float.
    Float,
    /// `d` or `D`: the literal is double.
    Double,
    /// `m` or `M`: the literal is decimal.
    Decimal,
}

/// The kind of a [`Token`], with any decoded payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// White space that is not a line terminator (9.3.3).
    Whitespace,
    /// A line terminator (9.3.1).
    NewLine,
    /// A `//` comment running to the end of the line (9.3.2).
    SingleLineComment,
    /// A `/* ... */` comment (9.3.2).
    DelimitedComment,
    /// An identifier, reduced to its canonical text: any `@` prefix is removed
    /// and Unicode escapes are resolved (9.4.2).
    Identifier(Box<str>),
    /// A keyword (9.4.3).
    Keyword(Keyword),
    /// An operator or punctuator (9.4.5).
    Punctuator(Punctuator),
    /// An integer literal (9.4.4.2): its value and the type suffix that
    /// constrains its type. The final type is chosen during binding.
    IntegerLiteral {
        /// The numeric value. On overflow a diagnostic is reported and this is 0.
        value: u64,
        /// The `U` and/or `L` suffix, if any.
        suffix: IntegerSuffix,
    },
    /// A real literal (9.4.4.3). Only the suffix is kept here; the numeric value
    /// is computed during binding, where the target type's rounding applies.
    RealLiteral {
        /// The `F`, `D`, or `M` suffix, if any.
        suffix: RealSuffix,
    },
    /// A character literal (9.4.4.4): a single UTF-16 code unit, with escape
    /// sequences decoded. Held as `u16` rather than `char` because a literal
    /// such as `'\uD800'` denotes a lone surrogate, which `char` cannot hold.
    CharacterLiteral(u16),
    /// A string literal (9.4.4.5), regular or verbatim, decoded to its UTF-16
    /// code units. Held as `[u16]` for the same reason a character literal is a
    /// `u16`: a regular string may contain lone surrogates via `\u` escapes, so
    /// the value is not always well-formed UTF-8 and cannot be a `str`.
    StringLiteral(Box<[u16]>),
    /// A pre-processing directive line (9.5), consumed in full, leading `#`
    /// through to but not including the line terminator. Directives are not part
    /// of the syntactic grammar; the scanner resolves them and their effects, so
    /// this is trivia and never reaches the parser. A malformed directive is
    /// still scanned as one of these, with a diagnostic alongside.
    PreprocessingDirective,
    /// Source text excluded by conditional compilation (9.5.4): the body of a
    /// branch whose controlling condition was false. No tokens are produced from
    /// such text; it is surfaced as trivia so the stream still covers the source.
    SkippedText,
    /// The end of the source, emitted once after the final token.
    EndOfFile,
    /// A character that begins no valid token. Emitted for error recovery with
    /// an accompanying diagnostic, so the parser can keep making progress.
    Unknown,
}

impl TokenKind {
    /// Returns `true` for white space, line terminators, and comments: the
    /// lexical elements that separate tokens but carry no syntactic meaning.
    #[must_use]
    pub fn is_trivia(&self) -> bool {
        matches!(
            self,
            TokenKind::Whitespace
                | TokenKind::NewLine
                | TokenKind::SingleLineComment
                | TokenKind::DelimitedComment
                | TokenKind::PreprocessingDirective
                | TokenKind::SkippedText
        )
    }
}

/// A single lexical element: a [`TokenKind`] and the [`Span`] it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// What kind of token this is, with any decoded payload.
    pub kind: TokenKind,
    /// The byte range of the token in the source.
    pub span: Span,
}

impl Token {
    /// Creates a token of `kind` covering `span`.
    #[must_use]
    pub fn new(kind: TokenKind, span: Span) -> Token {
        Token { kind, span }
    }

    /// Returns `true` when this token is trivia (see [`TokenKind::is_trivia`]).
    #[must_use]
    pub fn is_trivia(&self) -> bool {
        self.kind.is_trivia()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_spellings_round_trip() {
        for &keyword in Keyword::all() {
            assert_eq!(
                Keyword::from_text(keyword.as_str()),
                Some(keyword),
                "{}",
                keyword.as_str()
            );
        }
    }

    #[test]
    fn there_are_seventy_seven_keywords() {
        assert_eq!(Keyword::all().len(), 77);
    }

    #[test]
    fn keyword_lookup_is_case_sensitive_and_exact() {
        assert_eq!(Keyword::from_text("class"), Some(Keyword::Class));
        assert_eq!(Keyword::from_text("Class"), None);
        assert_eq!(Keyword::from_text("clas"), None);
        assert_eq!(Keyword::from_text(""), None);
    }

    #[test]
    fn punctuator_spellings_round_trip() {
        for &punctuator in Punctuator::all() {
            assert_eq!(
                Punctuator::from_text(punctuator.as_str()),
                Some(punctuator),
                "{}",
                punctuator.as_str()
            );
        }
    }

    #[test]
    fn there_are_forty_five_operators_and_punctuators() {
        assert_eq!(Punctuator::all().len(), 45);
    }

    #[test]
    fn trivia_is_classified() {
        assert!(TokenKind::Whitespace.is_trivia());
        assert!(TokenKind::NewLine.is_trivia());
        assert!(TokenKind::SingleLineComment.is_trivia());
        assert!(TokenKind::DelimitedComment.is_trivia());
        assert!(TokenKind::PreprocessingDirective.is_trivia());
        assert!(TokenKind::SkippedText.is_trivia());

        assert!(!TokenKind::Keyword(Keyword::Class).is_trivia());
        assert!(!TokenKind::Punctuator(Punctuator::Semicolon).is_trivia());
        assert!(!TokenKind::EndOfFile.is_trivia());
    }
}
