//! Diagnostics produced by the front end.

use crate::span::Span;
use alloc::boxed::Box;
use core::fmt;

/// How serious a diagnostic is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// A problem that does not, by itself, prevent compilation.
    Warning,
    /// A problem that prevents successful compilation.
    Error,
}

/// A particular diagnostic, with any detail needed to render its message.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiagnosticKind {
    /// `CS1547`: `void` written where a type is required -- here, the referent of a `ref` return
    /// (`ref void M()`).
    ///
    /// **A `ref` RETURN NAMES STORAGE, AND `void` NAMES NONE.** csc reports it at the `void` token
    /// rather than at the `ref`, measured, and with the same text it uses for a `void` local: the
    /// keyword is not usable in this context, whichever context that is.
    VoidByReference,
    /// A delimited comment was not closed with `*/` before the end of the file.
    UnterminatedDelimitedComment,
    /// A character was found that cannot begin any token.
    UnexpectedCharacter {
        /// The offending character.
        character: char,
    },
    /// An integer literal is larger than `ulong` can represent.
    IntegerLiteralTooLarge,
    /// A numeric literal is malformed, for example `0x` with no hex digits or an
    /// exponent with no digits. (Code to be confirmed against csc.)
    MalformedNumericLiteral,
    /// A backslash escape was not one of the recognised forms, or a `\x`, `\u`,
    /// or `\U` escape had too few hex digits or named a value above U+10FFFF.
    UnrecognizedEscapeSequence,
    /// A character or string literal ran to a line terminator or end of file
    /// before its closing quote.
    NewlineInConstant,
    /// A character literal had no character between its quotes.
    EmptyCharacterLiteral,
    /// A character literal held more than one character (more than one UTF-16
    /// code unit, counting an escape that expands to a surrogate pair as two).
    TooManyCharactersInCharacterLiteral,
    /// A verbatim string literal (`@"..."`) ran to end of file before its
    /// closing quote.
    UnterminatedStringLiteral,
    /// A `#` that is not the first non-white-space character on its line: a
    /// pre-processing directive must begin its own line (9.5).
    DirectiveNotFirstOnLine,
    /// A `#` was followed by something other than a known directive name (9.5).
    PreprocessorDirectiveExpected,
    /// A directive line carried tokens past its content where only white space,
    /// a single-line comment, or the end of the line was allowed (9.5).
    EndOfLineExpected,
    /// An identifier was required but not found: a `#define`/`#undef` symbol name
    /// (9.5.3), the member after a `.`, or any other identifier the grammar
    /// demands. (`true`/`false` are not valid symbol names.)
    IdentifierExpected,
    /// A `#define` or `#undef` appeared after the first real token of the file,
    /// which 9.5.3 forbids.
    SymbolAfterFirstToken,
    /// An `#elif`, `#else`, `#endif`, or `#endregion` had no open construct to
    /// match, or appeared where it was not allowed (9.5.4, 9.5.6).
    UnexpectedDirective,
    /// An `#if` (or `#region` whose body holds an `#if`) reached the end of the
    /// file, or a directive that may not appear, without its `#endif` (9.5.4).
    EndIfDirectiveExpected,
    /// A `#region` reached the end of the file, or an `#endif` where an
    /// `#endregion` was due, without its `#endregion` (9.5.6).
    EndRegionDirectiveExpected,
    /// A pre-processing expression in an `#if` or `#elif` was malformed (9.5.2).
    InvalidPreprocessorExpression,
    /// A parenthesised pre-processing expression was missing its `)` (9.5.2).
    CloseParenExpected,
    /// A `#line` directive had no valid line number, file name, or `default`
    /// indicator (9.5.7).
    InvalidLineDirective,
    /// A `#line` line number parsed as an integer but lay past the range a
    /// `#line` directive accepts (9.5.7).
    LineNumberOutOfRange,
    /// A `#error` directive, carrying its message text (9.5.5).
    ErrorDirective {
        /// The text following `#error` on the directive line.
        message: Box<str>,
    },
    /// `CS1633`: a `#pragma` this compiler does not recognize. A WARNING, as in csc: the
    /// directive is ignored and compilation continues, because a pragma is advice to the compiler
    /// and an unknown one is advice it cannot take.
    UnrecognizedPragma,
    /// A `#warning` directive, carrying its message text (9.5.5).
    WarningDirective {
        /// The text following `#warning` on the directive line.
        message: Box<str>,
    },
    /// An interpolation hole, or its alignment after the `,`, held no expression at all --
    /// `$"{}"`, `$"{n,}"` (csc `CS1733`).
    ///
    /// A DIFFERENT CODE FROM `CS1525`, measured: csc says *Expected expression* where a term is
    /// simply ABSENT and *Invalid expression term* where one is present and cannot begin an
    /// expression. Both sentences exist in csc and they are not interchangeable.
    ExpectedExpression,
    /// An interpolated string's hole was opened with `{` and the string ended before its `}`
    /// (csc `CS8076`).
    InterpolationCloseDelimiterExpected,
    /// CS1007: a property or indexer declaring the same accessor twice.
    ///
    /// **`set` AND `init` ARE THE SAME ACCESSOR**, so `int P { init { } set { } }` draws this and
    /// not a "cannot mix" complaint of its own -- measured. That is also what says the two spellings
    /// name ONE slot rather than two, which is why the tree keeps `init` as a flag on the setter.
    DuplicateAccessor,
    /// CS8856: an `init` accessor on a STATIC property or indexer.
    ///
    /// An `init` accessor exists to be callable during construction of an INSTANCE -- in an object
    /// initializer, or on `this`/`base` in a constructor -- and a static property has no instance
    /// to be constructed, so the spelling has no meaning there rather than merely being unusual.
    InitAccessorOnStaticMember,
    /// A lone `}` in an interpolated string's literal text: outside a hole it has to be doubled
    /// (csc `CS8086`).
    ///
    /// Asymmetric with `{`, and that asymmetry is the language's: a lone `{` OPENS a hole, so it
    /// is reported as the unclosed-hole `CS8076` above, while a lone `}` can open nothing and gets
    /// this. One character class, two diagnostics.
    UnescapedCloseBraceInInterpolation,
    /// An interpolation's `:` was followed immediately by the closing `}` -- `$"{n:}"` (csc
    /// `CS8089`). An absent specifier is written by leaving the `:` out.
    EmptyFormatSpecifier,
    /// A conditional expression written directly in an interpolation hole, where its `:` is read
    /// as the start of the format specifier -- `$"{c ? a : b}"` (csc `CS8361`).
    ///
    /// **THE `:` ALWAYS ENDS THE HOLE; csc DOES NOT DISAMBIGUATE, IT REPORTS.** Measured, and it
    /// is the reason this diagnostic exists at all rather than a cleverer scan: the fix csc names
    /// -- parenthesize -- is the only one, because inside `(...)` the `:` is at a nesting depth
    /// the format specifier never reaches.
    ConditionalInInterpolation,
    /// An interpolated verbatim string spelled `@$"..."` rather than `$@"..."`, under a dialect
    /// before C# 8.0 (csc `CS8401`).
    ///
    /// **NOT the `Feature '...'` gate family**, which is why it is its own kind: csc gives the
    /// two orderings different rungs (`$@` is C# 6.0, `@$` is C# 8.0) and refuses the late one
    /// with a sentence of its own that names neither a feature nor the CURRENT version. Measured
    /// at every dialect from 1 to 8.
    AtDollarRequiresLaterVersion,
    /// A primary expression was expected, but the token there cannot begin one
    /// (ECMA-334 1st ed, 14.5).
    ExpressionExpected,
    /// A specific token the grammar required was not present.
    TokenExpected {
        /// The expected token's spelling, for example `]` or `:`.
        expected: &'static str,
    },
    /// A type was expected, for example inside `typeof( )` or after `is`/`as`
    /// (ECMA-334 1st ed, clause 11).
    TypeExpected,
    /// A statement was not terminated by the required `;` (clause 15).
    SemicolonExpected,
    /// A block or similar construct was not closed by the required `}`.
    CloseBraceExpected,
    /// A token appeared in a class/struct/interface member declaration where a member name
    /// (or `operator`/`this`) was required, so it can begin no member (clause 17).
    InvalidTokenInMemberDeclaration {
        /// The offending token's source spelling, for example `}` or `;`.
        token: Box<str>,
    },
    /// A `partial` modifier stood somewhere other than immediately before the declaration's
    /// keyword (ECMA-334 4th ed 17.1.4). `partial public class C` is this; `public partial class C`
    /// is not.
    PartialModifierPosition,
    /// The same declaration modifier appeared twice (clause 10.2.2 / 17.2).
    DuplicateModifier {
        /// The repeated modifier's keyword, for example `public`.
        modifier: Box<str>,
    },
    /// A block was required (for example a `try`, `catch`, or `finally` body) but
    /// no `{` was found.
    OpenBraceExpected,
    /// A `try` block was followed by neither a `catch` nor a `finally` (15.10).
    ExpectedCatchOrFinally,
    /// A namespace member was expected to begin a type declaration but the
    /// `class`/`struct`/`interface`/`enum`/`delegate` keyword was missing (16.4).
    TypeDeclarationExpected,
    /// `CS1671`: a namespace declaration carried an attribute section or a modifier (16.2).
    ///
    /// **ONE PER OFFENDING ITEM, AT ITS OWN START** -- measured: `[Obsolete] public namespace N { }`
    /// draws TWO, at the `[` and at the `public`, not one for the declaration.
    ///
    /// **TOP-LEVEL ONLY.** Inside a namespace BODY csc answers `CS0116` instead, at a different
    /// position -- a nested `[Obsolete] namespace Inner { }` is parsed as an attempt at a member, so
    /// the diagnostic is about what a namespace may CONTAIN rather than about the declaration.
    /// Applying this rule uniformly would report a code csc does not.
    ///
    /// **AND THE NESTED CASE IS STILL AN ACCEPTS-INVALID, MEASURED: lcsc reports NOTHING for
    /// `namespace Outer { [Obsolete] namespace Inner { } }` where csc reports CS0116.** This rule
    /// did not close that; it is a separate `CS0116` item and is recorded here rather than left to
    /// look closed by association.
    NamespaceCannotHaveModifiersOrAttributes,
    /// `CS1730`: an `[assembly:]` or `[module:]` section appeared after a member (24.2).
    ///
    /// Measured: legal after `using` directives and `extern alias` declarations, illegal after a
    /// type OR a namespace -- which is exactly the condition the file-scoped-namespace work already
    /// tracks as `members_precede`. Reported at the TARGET keyword, not at the `[`.
    GlobalAttributeMustPrecedeMembers,
    /// `operator` was not followed by an overloadable operator (17.9).
    OverloadableOperatorExpected,
    /// A `foreach` header was missing the `in` keyword (15.8.4).
    InExpected,
    /// A post-1.0 operator (`=>`, `??`, `?.`, `?[`, `::`) was used while targeting a language
    /// version that predates it. Carries the feature's description and the version that introduced
    /// it, both already rendered to text (the lexer builds them from [`crate::version::Feature`]),
    /// so this stays decoupled from the version model.
    FeatureRequiresLaterVersion {
        /// The feature's noun phrase, e.g. "the null-coalescing operator '??'".
        feature: &'static str,
        /// The version that introduced it, as csc renders a REQUIRED version -- "2", "7.0".
        required: &'static str,
        /// The version being COMPILED, which selects the code and the message''s "in C# N".
        current: crate::version::LanguageVersion,
    },
    /// The selected dialect PERMITS this construct and this build cannot produce it: `LAM0001`,
    /// the other half of the two-bit rule in [`crate::version::Feature::gate_against`].
    ///
    /// **The message must NOT send the reader after a language version.** That is the whole reason
    /// this is not a `CS8022`: they already passed the version that permits the construct, so
    /// *"please use language version N or greater"* is advice they have already taken.
    ///
    FeatureNotInThisBuild {
        /// The feature's noun phrase, from [`crate::version::Feature::description`].
        feature: &'static str,
        /// The dialect that permits it. Naming it is the load-bearing half: without it the
        /// reader's first guess is the language version, which is the one thing that is not wrong.
        permitted_by: crate::version::LanguageVersion,
    },
    /// A second file-scoped namespace declaration (`namespace N;`) appeared inside the first one's
    /// body (csc CS8954).
    ///
    /// MEASURED, and the shape is not the one the message suggests: a file-scoped namespace's body
    /// runs to the end of the file, so a second one is never a SIBLING -- it is always written
    /// inside the first. csc reports it at the second declaration's NAME.
    OnlyOneFileScopedNamespace,
    /// A file-scoped namespace declaration and a brace-delimited one were nested inside one another
    /// (csc CS8955), in either order.
    ///
    /// MEASURED: this is a rule about the immediate CONTAINER, not about the file as a whole. A
    /// `namespace M { }` written after a file-scoped namespace is inside its body and draws this; a
    /// `namespace M { }` written BEFORE one does not, because nothing is nested. csc reports it at
    /// the inner declaration's NAME.
    BothFileScopedAndNormalNamespaces,
    /// A file-scoped namespace declaration was preceded by a type or namespace declaration (csc
    /// CS8956).
    ///
    /// MEASURED: `using` directives, `extern alias` declarations and `[assembly:]` / `[module:]`
    /// attribute sections may precede it; a type or a namespace may not. csc reports it at the
    /// file-scoped declaration's NAME.
    FileScopedNamespaceMustPrecedeMembers,
    /// An `__arglist` parameter marker appeared before other parameters; it must
    /// close the list (csc CS0257). Tokenized only under the typedref knob.
    ArglistMustBeLast,
    /// An `__arglist` parameter marker appeared in a declaration that cannot be
    /// vararg -- a delegate, an operator, or an indexer (csc CS1669).
    ArglistNotValidInThisContext,
    /// `CS4033`: the `await` operator outside an async method (ECMA-334 5th ed, 12.8.8.1).
    ///
    /// MEASURED in three shapes (statement, initializer, and inside `Main`): csc reports 4033 --
    /// the "async method" wording -- for every one lcsc can produce, because 4032's wording
    /// belongs to async lambdas, which do not exist here. Raised by the parser, which owns the
    /// context bit; the operand still parses and binds, so an unawaitable operand reports its
    /// own diagnostic beside this one exactly as csc does.
    AwaitOutsideAsync,
    /// `CS4003`: `await` used as a declared identifier inside an async method (12.8.8.1, which
    /// reserves the word there and offers `@await` as the escape).
    AwaitAsIdentifier,
    /// `CS1994`: the `async` modifier on a method with no body (`abstract async Task M();`).
    /// MEASURED: the one diagnostic csc reports for that program, text with a trailing period.
    AsyncRequiresBody,
    /// `CS4004`: `await` inside an `unsafe { }` block (measured). Parser-raised, because the
    /// parser lowers the block to a plain one and is the last stage that can see it.
    AwaitInUnsafe,
}

/// The namespace a diagnostic code belongs to.
///
/// **`CS` MEANS WHAT csc MEANS BY IT, ALWAYS.** A code is a search key: a developer who hits
/// `CS0649` will look it up and land on
/// csc's documentation, so lcsc may only spell a condition `CS` when csc has that same concept. It
/// follows that a condition csc has NO concept of cannot borrow a `CS` number -- an unused one
/// today is one a future Roslyn release may claim, and then the same key means two things
/// depending on which compiler emitted it.
///
/// **`LAM` IS FOR EXACTLY THOSE CONDITIONS, AND IT IS A SMALL FAMILY.** Almost everything routes to
/// csc's own codes: a BCL surface we do not ship reports as `CS0246`/`CS0234` (which is also how
/// nanoFramework expresses a restricted platform, measured), a construct above the selected dialect
/// reports as the `CS8022` family, and a language feature missing a compiler-required member reports
/// as `CS0518`/`CS0656`. What is left is the condition none of those describe: the dialect permits
/// the construct and THIS BUILD cannot produce it.
///
/// **The user-visible payoff is that the prefix says which kind of problem it is.** A `CS` code is a
/// statement about the language; a `LAM` code is a statement about this build's coverage, and the
/// second kind changes as the compiler grows where the first does not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeNamespace {
    /// csc's namespace, for conditions csc also has.
    Cs,
    /// Lamella's, for conditions csc has no concept of.
    Lam,
}

impl CodeNamespace {
    /// The literal prefix that precedes the digits: `CS` or `LAM`.
    ///
    /// Chosen against the widths a Problems pane already carries -- `CS0649` and `CA1822` at six
    /// characters, `IDE0051` and `MSB3021` at seven -- so `LAM0001` costs nothing in a pane already
    /// sized for the built-in analyzers, and one character in raw terminal output where the path
    /// dominates the line anyway. Four digits rather than three because the extra character buys
    /// ranges (compiler coverage, backend, linker, runtime) and three would not.
    #[must_use]
    pub fn prefix(self) -> &'static str {
        match self {
            CodeNamespace::Cs => "CS",
            CodeNamespace::Lam => "LAM",
        }
    }
}



impl DiagnosticKind {
    /// Which namespace this kind's code belongs to.
    ///
    /// `Cs` for everything csc also has a concept of, which is nearly all of it. A new arm here is
    /// a claim that csc has NO code for the condition -- check before adding one.
    ///
    #[must_use]
    pub fn namespace(&self) -> CodeNamespace {
        match self {
            DiagnosticKind::FeatureNotInThisBuild { .. } => CodeNamespace::Lam,
            _ => CodeNamespace::Cs,
        }
    }

    /// The C# compiler code for this diagnostic, that is, the number `N` in
    /// `CSN`. Codes match the reference compiler where an equivalent exists.
    #[must_use]
    pub fn code(&self) -> u16 {
        match self {
            DiagnosticKind::VoidByReference => 1547,
            DiagnosticKind::UnterminatedDelimitedComment => 1035,
            DiagnosticKind::UnexpectedCharacter { .. } => 1056,
            DiagnosticKind::IntegerLiteralTooLarge => 1021,
            DiagnosticKind::MalformedNumericLiteral => 1013,
            DiagnosticKind::UnrecognizedEscapeSequence => 1009,
            DiagnosticKind::NewlineInConstant => 1010,
            DiagnosticKind::EmptyCharacterLiteral => 1011,
            DiagnosticKind::TooManyCharactersInCharacterLiteral => 1012,
            DiagnosticKind::UnterminatedStringLiteral => 1039,
            DiagnosticKind::IdentifierExpected => 1001,
            DiagnosticKind::PreprocessorDirectiveExpected => 1024,
            DiagnosticKind::EndOfLineExpected => 1025,
            DiagnosticKind::CloseParenExpected => 1026,
            DiagnosticKind::EndIfDirectiveExpected => 1027,
            DiagnosticKind::UnexpectedDirective => 1028,
            DiagnosticKind::ErrorDirective { .. } => 1029,
            DiagnosticKind::UnrecognizedPragma => 1633,
            DiagnosticKind::WarningDirective { .. } => 1030,
            DiagnosticKind::SymbolAfterFirstToken => 1032,
            DiagnosticKind::EndRegionDirectiveExpected => 1038,
            DiagnosticKind::DirectiveNotFirstOnLine => 1040,
            DiagnosticKind::InvalidPreprocessorExpression => 1517,
            DiagnosticKind::InvalidLineDirective => 1576,
            DiagnosticKind::LineNumberOutOfRange => 1687,
            DiagnosticKind::ExpectedExpression => 1733,
            DiagnosticKind::InterpolationCloseDelimiterExpected => 8076,
            DiagnosticKind::DuplicateAccessor => 1007,
            DiagnosticKind::InitAccessorOnStaticMember => 8856,
            DiagnosticKind::UnescapedCloseBraceInInterpolation => 8086,
            DiagnosticKind::EmptyFormatSpecifier => 8089,
            DiagnosticKind::ConditionalInInterpolation => 8361,
            DiagnosticKind::AtDollarRequiresLaterVersion => 8401,
            DiagnosticKind::ExpressionExpected => 1525,
            DiagnosticKind::TokenExpected { .. } => 1003,
            DiagnosticKind::TypeExpected => 1031,
            DiagnosticKind::SemicolonExpected => 1002,
            DiagnosticKind::CloseBraceExpected => 1513,
            DiagnosticKind::InvalidTokenInMemberDeclaration { .. } => 1519,
            DiagnosticKind::DuplicateModifier { .. } => 1004,
            DiagnosticKind::PartialModifierPosition => 267,
            DiagnosticKind::OpenBraceExpected => 1514,
            DiagnosticKind::ExpectedCatchOrFinally => 1524,
            DiagnosticKind::TypeDeclarationExpected => 1518,
            DiagnosticKind::NamespaceCannotHaveModifiersOrAttributes => 1671,
            DiagnosticKind::GlobalAttributeMustPrecedeMembers => 1730,
            DiagnosticKind::OverloadableOperatorExpected => 1037,
            DiagnosticKind::InExpected => 1515,
            DiagnosticKind::FeatureRequiresLaterVersion { current, .. } => current.feature_gate_code(),
            DiagnosticKind::FeatureNotInThisBuild { .. } => 1,
            DiagnosticKind::OnlyOneFileScopedNamespace => 8954,
            DiagnosticKind::BothFileScopedAndNormalNamespaces => 8955,
            DiagnosticKind::FileScopedNamespaceMustPrecedeMembers => 8956,
            DiagnosticKind::ArglistMustBeLast => 257,
            DiagnosticKind::ArglistNotValidInThisContext => 1669,
            DiagnosticKind::AwaitOutsideAsync => 4033,
            DiagnosticKind::AwaitAsIdentifier => 4003,
            DiagnosticKind::AsyncRequiresBody => 1994,
            DiagnosticKind::AwaitInUnsafe => 4004,
        }
    }

    /// Whether this diagnostic stops compilation.
    #[must_use]
    pub fn severity(&self) -> Severity {
        match self {
            DiagnosticKind::UnrecognizedPragma | DiagnosticKind::WarningDirective { .. } => {
                Severity::Warning
            }
            _ => Severity::Error,
        }
    }
}

impl fmt::Display for DiagnosticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DiagnosticKind::VoidByReference => {
                f.write_str("Keyword 'void' cannot be used in this context")
            }
            DiagnosticKind::UnterminatedDelimitedComment => f.write_str("End-of-comment expected"),
            DiagnosticKind::UnexpectedCharacter { character } => {
                write!(f, "Unexpected character '{character}'")
            }
            DiagnosticKind::IntegerLiteralTooLarge => f.write_str("Integer constant is too large"),
            DiagnosticKind::MalformedNumericLiteral => f.write_str("Invalid number"),
            DiagnosticKind::UnrecognizedEscapeSequence => f.write_str("Unrecognized escape sequence"),
            DiagnosticKind::NewlineInConstant => f.write_str("Newline in constant"),
            DiagnosticKind::EmptyCharacterLiteral => f.write_str("Empty character literal"),
            DiagnosticKind::TooManyCharactersInCharacterLiteral => {
                f.write_str("Too many characters in character literal")
            }
            DiagnosticKind::UnterminatedStringLiteral => {
                f.write_str("Unterminated string literal")
            }
            DiagnosticKind::IdentifierExpected => f.write_str("Identifier expected"),
            DiagnosticKind::PreprocessorDirectiveExpected => {
                f.write_str("Preprocessor directive expected")
            }
            DiagnosticKind::EndOfLineExpected => {
                f.write_str("Single-line comment or end-of-line expected")
            }
            DiagnosticKind::CloseParenExpected => f.write_str(") expected"),
            DiagnosticKind::EndIfDirectiveExpected => f.write_str("#endif directive expected"),
            DiagnosticKind::UnexpectedDirective => f.write_str("Unexpected preprocessor directive"),
            DiagnosticKind::ErrorDirective { message } => write!(f, "#error: '{message}'"),
            DiagnosticKind::UnrecognizedPragma => f.write_str("Unrecognized #pragma directive"),
            DiagnosticKind::WarningDirective { message } => write!(f, "#warning: '{message}'"),
            DiagnosticKind::SymbolAfterFirstToken => {
                f.write_str("Cannot define/undefine preprocessor symbols after first token in file")
            }
            DiagnosticKind::EndRegionDirectiveExpected => {
                f.write_str("#endregion directive expected")
            }
            DiagnosticKind::DirectiveNotFirstOnLine => f.write_str(
                "Preprocessor directives must appear as the first non-whitespace character on a line",
            ),
            DiagnosticKind::InvalidPreprocessorExpression => {
                f.write_str("Invalid preprocessor expression")
            }
            DiagnosticKind::InvalidLineDirective => {
                f.write_str("The line number specified for #line directive is missing or invalid")
            }
            DiagnosticKind::LineNumberOutOfRange => {
                f.write_str("The line number specified for #line directive is out of range")
            }
            DiagnosticKind::ExpectedExpression => f.write_str("Expected expression"),
            DiagnosticKind::DuplicateAccessor => f.write_str("Property accessor already defined"),
            DiagnosticKind::InitAccessorOnStaticMember => {
                f.write_str("The 'init' accessor is not valid on static members")
            }
            DiagnosticKind::InterpolationCloseDelimiterExpected => f.write_str(
                "Missing close delimiter '}' for interpolated expression started with '{'.",
            ),
            DiagnosticKind::UnescapedCloseBraceInInterpolation => f.write_str(
                "A '}' character must be escaped (by doubling) in an interpolated string.",
            ),
            DiagnosticKind::EmptyFormatSpecifier => f.write_str("Empty format specifier."),
            DiagnosticKind::ConditionalInInterpolation => f.write_str(
                concat!(
                    "A conditional expression cannot be used directly in a string ",
                    "interpolation because the ':' ends the interpolation. ",
                    "Parenthesize the conditional expression.",
                ),
            ),
            DiagnosticKind::AtDollarRequiresLaterVersion => f.write_str(
                concat!(
                    "To use '@$' instead of '$@' for an interpolated verbatim string, ",
                    "please use language version '8.0' or greater.",
                ),
            ),
            DiagnosticKind::ExpressionExpected => f.write_str("Invalid expression term"),
            DiagnosticKind::TokenExpected { expected } => {
                write!(f, "Syntax error, '{expected}' expected")
            }
            DiagnosticKind::TypeExpected => f.write_str("Type expected"),
            DiagnosticKind::SemicolonExpected => f.write_str("; expected"),
            DiagnosticKind::CloseBraceExpected => f.write_str("} expected"),
            DiagnosticKind::InvalidTokenInMemberDeclaration { token } => {
                write!(f, "Invalid token '{token}' in a member declaration")
            }
            DiagnosticKind::DuplicateModifier { modifier } => {
                write!(f, "Duplicate '{modifier}' modifier")
            }
            DiagnosticKind::PartialModifierPosition => f.write_str(
                "The 'partial' modifier can only appear immediately before 'class', 'record', \
                 'struct', 'interface', 'event', an instance constructor name, or a method or \
                 property return type",
            ),
            DiagnosticKind::OpenBraceExpected => f.write_str("{ expected"),
            DiagnosticKind::ExpectedCatchOrFinally => {
                f.write_str("Expected catch or finally")
            }
            DiagnosticKind::TypeDeclarationExpected => {
                f.write_str("Expected class, delegate, enum, interface, or struct")
            }
            DiagnosticKind::NamespaceCannotHaveModifiersOrAttributes => {
                f.write_str("A namespace declaration cannot have modifiers or attributes")
            }
            DiagnosticKind::GlobalAttributeMustPrecedeMembers => f.write_str(
                "Assembly and module attributes must precede all other elements defined in a \
                 file except using clauses and extern alias declarations",
            ),
            DiagnosticKind::OverloadableOperatorExpected => {
                f.write_str("Overloadable operator expected")
            }
            DiagnosticKind::InExpected => f.write_str("'in' expected"),
            DiagnosticKind::FeatureRequiresLaterVersion {
                feature,
                required,
                current,
            } => {
                write!(
                    f,
                    "Feature '{feature}' is not available in C# {}. Please use language version {required} or greater.",
                    current.message_name()
                )
            }
            DiagnosticKind::FeatureNotInThisBuild {
                feature,
                permitted_by,
            } => write!(
                f,
                "Feature '{feature}' is permitted by C# {} but is not provided by this build of Lamella.",
                permitted_by.message_name()
            ),
            DiagnosticKind::OnlyOneFileScopedNamespace => {
                f.write_str("Source file can only contain one file-scoped namespace declaration.")
            }
            DiagnosticKind::BothFileScopedAndNormalNamespaces => f.write_str(
                "Source file can not contain both file-scoped and normal namespace declarations.",
            ),
            DiagnosticKind::FileScopedNamespaceMustPrecedeMembers => {
                f.write_str("File-scoped namespace must precede all other members in a file.")
            }
            DiagnosticKind::ArglistMustBeLast => {
                write!(
                    f,
                    "An __arglist parameter must be the last parameter in a parameter list"
                )
            }
            DiagnosticKind::ArglistNotValidInThisContext => {
                write!(f, "__arglist is not valid in this context")
            }
            DiagnosticKind::AwaitOutsideAsync => f.write_str(
                "The 'await' operator can only be used within an async method. Consider marking \
                 this method with the 'async' modifier and changing its return type to 'Task'.",
            ),
            DiagnosticKind::AwaitAsIdentifier => f.write_str(
                "'await' cannot be used as an identifier within an async method or lambda expression",
            ),
            DiagnosticKind::AsyncRequiresBody => {
                f.write_str("The 'async' modifier can only be used in methods that have a body.")
            }
            DiagnosticKind::AwaitInUnsafe => f.write_str("Cannot await in an unsafe context"),
        }
    }
}

/// A diagnostic: what went wrong ([`DiagnosticKind`]) and where ([`Span`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The specific problem.
    pub kind: DiagnosticKind,
    /// The source location the diagnostic refers to.
    pub span: Span,
}

impl Diagnostic {
    /// Creates a diagnostic of `kind` at `span`.
    #[must_use]
    pub fn new(kind: DiagnosticKind, span: Span) -> Diagnostic {
        Diagnostic { kind, span }
    }

    /// The C# compiler code (`CSxxxx`) for this diagnostic.
    #[must_use]
    pub fn code(&self) -> u16 {
        self.kind.code()
    }

    /// This diagnostic's severity.
    #[must_use]
    pub fn severity(&self) -> Severity {
        self.kind.severity()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn codes_match_the_reference_compiler() {
        assert_eq!(DiagnosticKind::UnterminatedDelimitedComment.code(), 1035);
        assert_eq!(
            DiagnosticKind::UnexpectedCharacter { character: '#' }.code(),
            1056
        );
    }

    #[test]
    fn lexical_diagnostics_are_errors() {
        assert_eq!(
            DiagnosticKind::UnterminatedDelimitedComment.severity(),
            Severity::Error
        );
    }

    #[test]
    fn messages_render_their_detail() {
        let unexpected = DiagnosticKind::UnexpectedCharacter { character: '#' };
        assert_eq!(format!("{unexpected}"), "Unexpected character '#'");
        assert_eq!(
            format!("{}", DiagnosticKind::UnterminatedDelimitedComment),
            "End-of-comment expected"
        );
    }
}
