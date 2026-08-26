//! Tokens: the lexical elements of C#.

use crate::span::Span;
use alloc::boxed::Box;
use alloc::vec::Vec;

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
        "??" => QuestionQuestion,
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
        "=>" => EqualsGreaterThan,
    }
}

spelled_enum! {
    /// An undocumented csc typed-reference operator keyword. These are NOT part of
    /// ECMA-334; they are csc's own `__`-prefixed operators over the typed-reference
    /// family (`System.TypedReference` and the CLI vararg machinery), each lowering to
    /// an ECMA-335 typed-reference opcode (`mkrefany`/`refanyval`/`refanytype`) or, for
    /// `__arglist`, the vararg calling convention + the `arglist` opcode. The lexer
    /// recognizes them only when [`LexOptions::typedref`] is on (the csc-parity knob);
    /// in strict ISO-1 mode they scan as ordinary identifiers.
    ///
    /// [`LexOptions::typedref`]: crate::lexer::LexOptions::typedref
    pub enum TypedRefKeyword {
        "__makeref" => MakeRef,
        "__refvalue" => RefValue,
        "__reftype" => RefType,
        "__arglist" => ArgList,
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
    /// One of the undocumented csc typed-reference operators (`__makeref`/`__refvalue`/
    /// `__reftype`). Produced only in csc-parity mode ([`LexOptions::typedref`]); not an
    /// ECMA-334 keyword, so it is a kind of its own rather than a [`Keyword`].
    ///
    /// [`LexOptions::typedref`]: crate::lexer::LexOptions::typedref
    TypedRefKeyword(TypedRefKeyword),
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
    /// A real literal (9.4.4.3): its value as `f64` bits (see [`f64::from_bits`];
    /// stored as bits so the token stays `Eq`/`Hash`) and the type suffix. A `float`
    /// narrows the value at emit. A `decimal` (`m`) literal is a [`TokenKind::DecimalLiteral`].
    RealLiteral {
        /// The value's `f64` bit pattern. On a malformed literal this is 0.
        bits: u64,
        /// The `F`, `D`, or `M` suffix, if any.
        suffix: RealSuffix,
    },
    /// A `decimal` (`m`-suffixed) literal (9.4.4.3), kept EXACTLY as its 96-bit integer mantissa
    /// (`lo`/`mid`/`hi`) and power-of-ten `scale` -- `f64` cannot represent every decimal, nor
    /// preserve the scale (`0.10m` vs `0.1m`).
    DecimalLiteral {
        /// Bits 0..32 of the mantissa.
        lo: u32,
        /// Bits 32..64 of the mantissa.
        mid: u32,
        /// Bits 64..96 of the mantissa.
        hi: u32,
        /// The power-of-ten scale (0..=28).
        scale: u8,
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
    /// An interpolated string literal (`$"..."`, `$@"..."`, `@$"..."`) -- C# 6.0. Boxed because it
    /// is by far the largest payload here and every other token would otherwise carry its width.
    InterpolatedString(Box<InterpolatedString>),
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

/// An interpolated string literal (`$"a{b}c"`, `$@"a{b}c"`) as the scanner resolved it: the
/// literal pieces with their escapes already decoded, and the holes with their expression TOKENS
/// already scanned.
///
/// **THE HOLES CARRY TOKENS RATHER THAN TEXT, AND THAT IS THE WHOLE REASON THIS TYPE EXISTS.** A
/// hole holds an arbitrary expression, so something has to lex it; doing that in a second pass
/// over a slice of the hole's text would give every token inside a hole a span measured from the
/// hole rather than from the file, and a diagnostic inside an interpolation would point at the
/// wrong place -- or at nothing, once the offsets ran past the end of a short file. Scanning the
/// hole with the same scanner, in place, keeps every span a real file offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpolatedString {
    /// The literal pieces and holes, in source order. Adjacent literals are never produced --
    /// `{{` merges into the piece around it -- so a piece is always maximal.
    pub parts: Vec<InterpolatedPart>,
    /// Whether it was written verbatim (`$@"..."` or `@$"..."`): no backslash escapes, `""` for a
    /// quote, and line terminators belong to the value.
    pub verbatim: bool,
}

/// One piece of an [`InterpolatedString`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterpolatedPart {
    /// Literal text between holes, with `{{`/`}}` and (when not verbatim) backslash escapes
    /// already decoded to their characters. May be empty only when the whole string is.
    Literal(Box<[u16]>),
    /// A `{ ... }` hole.
    Hole(InterpolatedHole),
}

/// A `{ expression [, alignment] [: format] }` hole in an interpolated string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterpolatedHole {
    /// The expression's tokens, trivia included, ending WITHOUT an end-of-file marker. Spans are
    /// offsets into the original file.
    pub tokens: Vec<Token>,
    /// The alignment expression's tokens, when a `,` was present; empty otherwise. It is an
    /// expression rather than a number because csc binds it as one -- `$"{n,99999999999}"` draws
    /// `CS0266`/`CS0150`, the ordinary constant-conversion pair, measured.
    pub alignment: Vec<Token>,
    /// The format specifier's text after a `:`, taken literally. `None` when there was no `:`;
    /// never `Some("")`, which is `CS8089`.
    pub format: Option<Box<str>>,
    /// The hole's extent in the source, `{` through `}`.
    pub span: Span,
}

/// A single lexical element: a [`TokenKind`] and the [`Span`] it covers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// What kind of token this is, with any decoded payload.
    pub kind: TokenKind,
    /// The byte range of the token in the source.
    pub span: Span,
    /// Whether this token was written with the `@` verbatim prefix (9.4.2). Meaningful only for
    /// [`TokenKind::Identifier`]; always `false` for every other kind.
    ///
    /// The identifier's canonical TEXT drops the `@` -- `@name` and `name` denote the SAME name
    /// for binding and metadata, which 9.4.2 requires -- so the prefix has to survive somewhere
    /// else for the one decision it affects: a CONTEXTUAL keyword is forced back to an ordinary
    /// identifier by `@`. `int @await = 4;` inside an async method is legal exactly because the
    /// parser can see this flag where the text alone reads as the keyword (ECMA-334 5th ed,
    /// 12.8.8.1).
    pub verbatim: bool,
}

impl Token {
    /// Creates a token of `kind` covering `span`.
    #[must_use]
    pub fn new(kind: TokenKind, span: Span) -> Token {
        Token {
            kind,
            span,
            verbatim: false,
        }
    }

    /// Creates a token of `kind` covering `span`, recording whether it carried the `@`
    /// verbatim prefix. Only the lexer's identifier path passes `true`.
    #[must_use]
    pub fn with_verbatim(kind: TokenKind, span: Span, verbatim: bool) -> Token {
        Token {
            kind,
            span,
            verbatim,
        }
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

    /// The table's size, pinned so a stray addition or a lost row is loud. It is NOT the C# 1.0
    /// operator set alone: `??` is C# 2.0's and the LEXER is what keeps it out of a C# 1 dialect
    /// (`try_gate_post_1_0_operator`), not its absence from this table. 45 -> 46 when `??` landed.
    #[test]
    fn there_are_forty_six_operators_and_punctuators() {
        assert_eq!(Punctuator::all().len(), 47);
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
