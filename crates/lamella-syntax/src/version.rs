//! Language versioning and feature gating.

use core::fmt;

/// A version of the C# language.
///
/// Every variant through [`LanguageVersion::SELECTABLE_MAX`] can be SELECTED, which is a statement
/// about what we can gate against rather than about what we have built -- see
/// [`Feature::is_implemented`] for the other half.
///
/// Ordering follows release order, so `>=` is the natural way to ask whether a
/// version is recent enough for a given feature.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum LanguageVersion {
    /// C# 1.0, as standardised by ECMA-334 1st edition (December 2001).
    #[default]
    CSharp1,
    /// C# 2.0 (ECMA-334 3rd ed). Present only to gate post-1.0 features; not yet implemented.
    CSharp2,
    /// C# 3.0. A gating label only; not yet implemented.
    CSharp3,
    /// C# 4.0. A gating label only; not yet implemented.
    CSharp4,
    /// C# 5.0. A gating label only; not yet implemented. Present because the DIAGNOSTIC CODE for a
    /// too-new feature is keyed on the version being compiled (see
    /// [`LanguageVersion::feature_gate_code`]) and C# 5 has its own -- a gap in this enum would
    /// silently borrow its neighbour's code.
    CSharp5,
    /// C# 6.0 (ECMA-334 6th ed, 2022). A gating label only; not yet implemented.
    CSharp6,
    /// C# 7.0 (ECMA-334 7th ed, 2023 -- ISO/IEC 20619:2023 -- the latest ratified standard). A
    /// gating label only; not yet implemented.
    CSharp7,
    /// C# 7.1. A gating label only. Present so a 7.x feature can be gated precisely -- see
    /// `CSharp7_2`.
    CSharp7_1,
    /// C# 7.2. A gating label only, and it earns its place: a LEADING digit separator (`0x_FF`) is
    /// a separate csc feature introduced here, so without this variant a compiler that implements
    /// C# 7.0 separators would have to either accept `0x_FF` (which csc rejects at 7.0) or reject
    /// it with the wrong required version.
    CSharp7_2,
    /// C# 7.3. A gating label only; present so the 7.x run is contiguous rather than having a hole
    /// where 7.1 and 7.3 fall back to `Unsupported` while 7.2 works.
    CSharp7_3,
    /// C# 8.0. A gating label only; not yet implemented.
    CSharp8,
    /// C# 9.0 -- the rung that carries TOP-LEVEL STATEMENTS, which is why the ceiling reaches here:
    /// every dotnet/iot and nanoFramework sample program is a bare statement file with no `Main`.
    /// A gating label only; not yet implemented.
    CSharp9,
    /// C# 10.0 -- the rung that carries FILE-SCOPED NAMESPACES, which 115 files of dotnet/iot use.
    /// A gating label only; not yet implemented.
    CSharp10,
    /// C# 11.0 -- the rung that carries REQUIRED MEMBERS, the one C# 11 feature both compatibility
    /// targets adopted (64 uses in dotnet/iot, 56 in nanoFramework). A gating label only; not yet
    /// implemented.
    CSharp11,
}

impl LanguageVersion {
    /// The dialect compiled when no `/langversion` is given.
    ///
    /// **STAYS C# 1.0, and is deliberately NOT derived from [`Self::SELECTABLE_MAX`].** It was
    /// defined as the ceiling while the two were the same value, and leaving it that way would have
    /// meant that raising the ceiling silently changed every compilation that never asked for a
    /// dialect -- including the whole differential corpus, whose job is to hold lcsc against
    /// ECMA-334 1st edition. The default is a conformance decision; the ceiling is a capability
    /// one. They coincided once and must not be spelled as if they always will.
    pub const DEFAULT: LanguageVersion = LanguageVersion::CSharp1;

    /// The newest dialect `/langversion` will select.
    ///
    /// **Selecting a dialect is NOT a claim that lcsc implements all of it.** A dialect decides
    /// which constructs are PERMITTED; [`Feature::is_implemented`] decides which of those this build
    /// can actually produce, and a construct needs both. So the ceiling can name every version we
    /// can gate against, and a permitted-but-unbuilt construct is refused by name (`LAM0001`)
    /// rather than by pretending the dialect forbids it.
    pub const SELECTABLE_MAX: LanguageVersion = LanguageVersion::CSharp11;

    /// Every selectable dialect, in release order, so a check across "all the rungs" is DERIVED
    /// rather than transcribed. Kept complete by the same two compiler-enforced halves as
    /// [`Feature::ALL`]: an exhaustive `match` that stops compiling when a variant is added, and a
    /// length assertion that fails until the variant is listed here too.
    pub const ALL_SELECTABLE: [LanguageVersion; 14] = [
        LanguageVersion::CSharp1,
        LanguageVersion::CSharp2,
        LanguageVersion::CSharp3,
        LanguageVersion::CSharp4,
        LanguageVersion::CSharp5,
        LanguageVersion::CSharp6,
        LanguageVersion::CSharp7,
        LanguageVersion::CSharp7_1,
        LanguageVersion::CSharp7_2,
        LanguageVersion::CSharp7_3,
        LanguageVersion::CSharp8,
        LanguageVersion::CSharp9,
        LanguageVersion::CSharp10,
        LanguageVersion::CSharp11,
    ];

    /// Returns `true` when `feature` is available in this language version.
    #[must_use]
    pub fn supports(self, feature: Feature) -> bool {
        self >= feature.introduced_in()
    }

    /// Returns `true` when `/langversion` will select this dialect.
    ///
    /// This replaced an `is_implemented` that asked whether the compiler could compile the whole
    /// version. That question stopped being answerable per-VERSION the moment capability moved to
    /// [`Feature::is_implemented`]: lcsc gates C# 7 and builds a handful of its features, so "is
    /// C# 7 implemented" has no true answer while "is C# 7 selectable" and "is this feature built"
    /// both do.
    #[must_use]
    pub fn is_selectable(self) -> bool {
        self <= Self::SELECTABLE_MAX
    }

    /// The diagnostic code csc reports when a feature is not available in THIS version.
    ///
    /// **The code names the version being COMPILED, not the version the feature needs** -- the
    /// required version appears only in the message ("Please use language version 7.0 or greater").
    /// A single hard-coded code is therefore right for exactly one language version and wrong for
    /// every other, which is the shape this method exists to prevent.
    ///
    /// MEASURED, not read from a standard: a gating diagnostic is not described by ECMA-334 at all,
    /// so csc is the only oracle there is. One compilation per row against
    /// `csc /langversion:<v>`:
    ///
    /// | version | code | | version | code |
    /// |---|---|---|---|---|
    /// | C# 1 (ISO-1) | `CS8022` | | C# 7 | `CS8107` |
    /// | C# 2 (ISO-2) | `CS8023` | | C# 8 | `CS8400` |
    /// | C# 3 | `CS8024` | | C# 9 | `CS8773` |
    /// | C# 4 | `CS8025` | | C# 10 | `CS8936` |
    /// | C# 5 | `CS8026` | | C# 11 | `CS9058` |
    /// | C# 6 | `CS8059` | | | |
    ///
    /// The first six rows came from a file using binary literals and digit separators, which C# 7
    /// accepts -- so that run could not reach past 6, and the arms above 6 read C# 6's code for a
    /// while on the reasoning that they were unreachable. **Raising the ceiling made them
    /// reachable, and they were wrong.** The rows from 7 up are a second run, against a C# 12
    /// construct, which every dialect below 12 gates.
    ///
    /// The lesson is worth more than the table: **"unreachable, so the value does not matter" has a
    /// shelf life exactly as long as the input space stays fixed.** Two arms in this file were
    /// wrong that way in one afternoon -- this one and [`Self::message_name`]'s C# 7 rendering.
    ///
    /// The 7.x point releases are here too -- 7.1 `CS8302`, 7.2 `CS8320`, 7.3 `CS8370` -- because a
    /// LEADING digit separator (`0x_FF`) is a csc feature introduced at 7.2, and gating it needs a
    /// version to name.
    #[must_use]
    pub fn feature_gate_code(self) -> u16 {
        match self {
            LanguageVersion::CSharp1 => 8022,
            LanguageVersion::CSharp2 => 8023,
            LanguageVersion::CSharp3 => 8024,
            LanguageVersion::CSharp4 => 8025,
            LanguageVersion::CSharp5 => 8026,
            LanguageVersion::CSharp6 => 8059,
            LanguageVersion::CSharp7 => 8107,
            LanguageVersion::CSharp7_1 => 8302,
            LanguageVersion::CSharp7_2 => 8320,
            LanguageVersion::CSharp7_3 => 8370,
            LanguageVersion::CSharp8 => 8400,
            LanguageVersion::CSharp9 => 8773,
            LanguageVersion::CSharp10 => 8936,
            LanguageVersion::CSharp11 => 9058,
        }
    }

    /// How csc spells this version inside a diagnostic message: `C# 1`, not `C# 1.0`.
    ///
    /// Measured from the message text, which reads "is not available in C# 1." A trailing `.0`
    /// would differ from csc's for a diagnostic whose whole purpose is to be searched for
    /// verbatim.
    #[must_use]
    pub fn message_name(self) -> &'static str {
        match self {
            LanguageVersion::CSharp1 => "1",
            LanguageVersion::CSharp2 => "2",
            LanguageVersion::CSharp3 => "3",
            LanguageVersion::CSharp4 => "4",
            LanguageVersion::CSharp5 => "5",
            LanguageVersion::CSharp6 => "6",
            LanguageVersion::CSharp7 => "7.0",
            LanguageVersion::CSharp7_1 => "7.1",
            LanguageVersion::CSharp7_2 => "7.2",
            LanguageVersion::CSharp7_3 => "7.3",
            LanguageVersion::CSharp8 => "8.0",
            LanguageVersion::CSharp9 => "9.0",
            LanguageVersion::CSharp10 => "10.0",
            LanguageVersion::CSharp11 => "11.0",
        }
    }

    /// How csc spells this version as the REQUIRED one, in "Please use language version N or
    /// greater".
    ///
    /// **A BARE MAJOR UP TO 6, AND `N.0` FROM 7 ON.** Measured rather than generalized: reading
    /// generalized "7.0" from one feature's message into "always carries a minor part", and three
    /// more measurements falsified it -- `static classes` asks for "2", `automatically implemented
    /// properties` for "3", `async` for "5", `using static` for "6", while default interface
    /// implementations ask for "8.0" and parameterless struct constructors for "10.0". The boundary
    /// is real and it sits between 6 and 7.
    ///
    /// Deliberately a different rendering from [`Self::message_name`], which is the CURRENT version
    /// in the same sentence: "not available in C# 1. Please use language version 7.0 or greater."
    /// The asymmetry is csc's; matching it is the point, because the message is a search key.
    #[must_use]
    pub fn required_name(self) -> &'static str {
        match self {
            LanguageVersion::CSharp1 => "1",
            LanguageVersion::CSharp2 => "2",
            LanguageVersion::CSharp3 => "3",
            LanguageVersion::CSharp4 => "4",
            LanguageVersion::CSharp5 => "5",
            LanguageVersion::CSharp6 => "6",
            LanguageVersion::CSharp7 => "7.0",
            LanguageVersion::CSharp7_1 => "7.1",
            LanguageVersion::CSharp7_2 => "7.2",
            LanguageVersion::CSharp7_3 => "7.3",
            LanguageVersion::CSharp8 => "8.0",
            LanguageVersion::CSharp9 => "9.0",
            LanguageVersion::CSharp10 => "10.0",
            LanguageVersion::CSharp11 => "11.0",
        }
    }

    /// Parses a csc-compatible `/langversion` value such as `ISO-1`, `1`,
    /// `default`, or `latest`.
    ///
    /// Matching is case-insensitive and ignores surrounding whitespace. A value
    /// that names a real but unimplemented version yields
    /// [`LanguageVersionError::Unsupported`]; a value that names no known
    /// version yields [`LanguageVersionError::Invalid`]. Separating the two lets
    /// the driver explain the difference precisely.
    pub fn parse_flag(value: &str) -> Result<LanguageVersion, LanguageVersionError> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("default") {
            return Ok(Self::DEFAULT);
        }
        if value.eq_ignore_ascii_case("latest") || value.eq_ignore_ascii_case("latestmajor") {
            return Ok(Self::SELECTABLE_MAX);
        }
        if value.eq_ignore_ascii_case("iso-1") || value == "1" || value == "1.0" {
            return Ok(Self::CSharp1);
        }
        if value.eq_ignore_ascii_case("iso-2") || value == "2" || value == "2.0" {
            return Ok(Self::CSharp2);
        }
        if value == "3" || value == "3.0" {
            return Ok(Self::CSharp3);
        }
        if value == "4" || value == "4.0" {
            return Ok(Self::CSharp4);
        }
        if value == "5" || value == "5.0" {
            return Ok(Self::CSharp5);
        }
        if value == "6" || value == "6.0" {
            return Ok(Self::CSharp6);
        }
        if value == "7" || value == "7.0" {
            return Ok(Self::CSharp7);
        }
        if value == "7.1" {
            return Ok(Self::CSharp7_1);
        }
        if value == "7.2" {
            return Ok(Self::CSharp7_2);
        }
        if value == "7.3" {
            return Ok(Self::CSharp7_3);
        }
        if value == "8" || value == "8.0" {
            return Ok(Self::CSharp8);
        }
        if value == "9" || value == "9.0" {
            return Ok(Self::CSharp9);
        }
        if value == "10" || value == "10.0" {
            return Ok(Self::CSharp10);
        }
        if value == "11" || value == "11.0" {
            return Ok(Self::CSharp11);
        }
        if is_known_future_version(value) {
            return Err(LanguageVersionError::Unsupported);
        }
        Err(LanguageVersionError::Invalid)
    }

    /// The csc-compatible flag value that selects this version.
    #[must_use]
    pub fn flag_value(self) -> &'static str {
        match self {
            LanguageVersion::CSharp1 => "ISO-1",
            LanguageVersion::CSharp2 => "ISO-2",
            LanguageVersion::CSharp3 => "3",
            LanguageVersion::CSharp4 => "4",
            LanguageVersion::CSharp5 => "5",
            LanguageVersion::CSharp6 => "6",
            LanguageVersion::CSharp7 => "7",
            LanguageVersion::CSharp7_1 => "7.1",
            LanguageVersion::CSharp7_2 => "7.2",
            LanguageVersion::CSharp7_3 => "7.3",
            LanguageVersion::CSharp8 => "8",
            LanguageVersion::CSharp9 => "9",
            LanguageVersion::CSharp10 => "10",
            LanguageVersion::CSharp11 => "11",
        }
    }

    /// A human-readable name such as `C# 1.0`, for this compiler's own prose (driver errors,
    /// help text).
    ///
    /// Not for a csc-matched diagnostic: csc spells the same version two other ways depending on
    /// where in the sentence it lands. See [`Self::message_name`] and [`Self::required_name`].
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            LanguageVersion::CSharp1 => "C# 1.0",
            LanguageVersion::CSharp2 => "C# 2.0",
            LanguageVersion::CSharp3 => "C# 3.0",
            LanguageVersion::CSharp4 => "C# 4.0",
            LanguageVersion::CSharp5 => "C# 5.0",
            LanguageVersion::CSharp6 => "C# 6.0",
            LanguageVersion::CSharp7 => "C# 7.0",
            LanguageVersion::CSharp7_1 => "C# 7.1",
            LanguageVersion::CSharp7_2 => "C# 7.2",
            LanguageVersion::CSharp7_3 => "C# 7.3",
            LanguageVersion::CSharp8 => "C# 8.0",
            LanguageVersion::CSharp9 => "C# 9.0",
            LanguageVersion::CSharp10 => "C# 10.0",
            LanguageVersion::CSharp11 => "C# 11.0",
        }
    }
}

impl fmt::Display for LanguageVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Returns `true` when `value` names a C# version that exists in the wider
/// language but is beyond what we implement today.
///
/// This is a flat list rather than a parsed number so the driver can reject,
/// say, `-langversion:7.3` with a precise message long before that version's
/// [`LanguageVersion`] variant exists.
fn is_known_future_version(value: &str) -> bool {
    const KNOWN: &[&str] = &[
        "12", "13", "14", "preview",
    ];
    KNOWN.iter().any(|known| value.eq_ignore_ascii_case(known))
}

/// A language feature that the front end gates on a [`LanguageVersion`].
///
/// The table is seeded with a few post-1.0 features to fix the pattern: when we
/// implement a feature we add its variant and the version that introduced it,
/// and the parser or binder calls [`LanguageVersion::supports`] before accepting
/// the construct. C# 1.0 features need no gate and so do not appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Feature {
    /// Generic types and methods, for example `List<T>`. Introduced in C# 2.0.
    Generics,
    /// A `static` class -- abstract and sealed, with no synthesized default constructor.
    /// Introduced in C# 2.0. **The one feature whose emit path is complete while the dialect still
    /// refuses it**; see [`Feature::is_implemented`].
    StaticClasses,
    /// Anonymous methods, for example `delegate (int x) { return x; }`.
    /// Introduced in C# 2.0.
    AnonymousMethods,
    /// Nullable value types, for example `int?`. Introduced in C# 2.0.
    NullableValueTypes,
    /// The `default` operator, `default(T)`. Introduced in C# 2.0.
    ///
    /// **It is the only spelling of a TYPE PARAMETER's zero**, which is why it arrived with
    /// generics rather than beside them: `T` may close over a reference type or a value type, so
    /// neither `null` nor `0` covers it and the choice has to be made where `T` is known.
    DefaultOperator,
    /// The null-coalescing operator `??` (`a ?? b`). Introduced in C# 2.0.
    ///
    /// **The one feature in this table whose name could not be measured**, because csc's ISO-1 is
    /// lenient about `??` and accepts it at every selectable dialect -- so it never emits a gate to
    /// read a name off. lcsc is deliberately stricter here; the description is therefore OURS,
    /// written in csc's style rather than copied from it.
    NullCoalescing,
    /// A GETTER-ONLY automatically implemented property, `int P { get; }`. Introduced in C# 6.0.
    ///
    /// **A DIFFERENT FEATURE FROM [`Feature::AutoProperties`], AND csc NAMES IT DIFFERENTLY** --
    /// *readonly automatically implemented properties*, measured. The backing field is `initonly`
    /// and only a constructor may assign it, which is a rule the C# 3.0 form does not have; a
    /// compiler that treated the two as one feature would either refuse the common form or emit a
    /// writable field for the readonly one.
    ReadonlyAutoProperty,
    /// A PARTIAL TYPE -- `partial class C` written more than once, the declarations merging into
    /// one type (ECMA-334 4th ed 17.1.4). Introduced in C# 2.0.
    ///
    /// **THE TYPE, NOT THE METHOD.** A partial METHOD (`partial void OnThing();` with its
    /// implementation in another part) is a C# 3.0 feature and csc gates it separately -- measured,
    /// *Feature 'partial method' is not available in C# 2. Please use language version 3 or
    /// greater.* One `partial` token, two features, three years apart.
    PartialTypes,
    /// A `#pragma` directive (`#pragma warning disable 649`). Introduced in C# 2.0.
    ///
    /// **csc NAMES IT `#pragma`, WITH THE HASH** -- measured, `Feature '#pragma' is not available
    /// in C# 1` -- which is why this description is punctuation rather than a noun phrase like
    /// every other entry in the table.
    PragmaDirective,
    /// The namespace alias qualifier `::` (`global::System`). Introduced in C# 2.0.
    NamespaceAlias,
    /// An access modifier on a property, indexer, or event accessor, for example `private set`.
    /// Introduced in C# 2.0.
    AccessorAccessibility,
    /// A lambda expression, `x => x`. Introduced in C# 3.0.
    ///
    /// **NOT the same feature as an expression-bodied member, though both are spelled `=>`.**
    /// This variant covers the LAMBDA only: csc gates the two separately and THREE VERSIONS APART
    /// -- see [`Feature::ExpressionBodiedMethod`].
    LambdaExpression,
    /// An expression-bodied method, `int M() => 1;`. Introduced in **C# 6.0**.
    ///
    /// **Split out of the old `LambdaArrow` after measuring csc, which reports
    /// `'expression-bodied method' ... Please use language version 6 or greater` -- three releases
    /// after the lambda that shares its token.** While the two were one variant, a dialect of C# 3,
    /// 4 or 5 PERMITTED an expression-bodied member that csc rejects; that is the accepts-invalid
    /// column, and it was harmless only because neither half is built yet.
    ///
    /// **A lexer cannot make this distinction** -- `=>` is one token and which feature it belongs
    /// to is a question about the enclosing declaration. So the lexer keeps gating
    /// [`Feature::LambdaExpression`], which is the correct LOWER bound (`=>` does not exist at all
    /// before C# 3), and the C# 6 half needs a parser-side site that arrives with the feature.
    ExpressionBodiedMethod,
    /// An expression-bodied property, `int P => 1;`. Introduced in **C# 6.0**.
    ///
    /// A separate variant because csc names the MEMBER KIND in the message -- `'expression-bodied
    /// property'`, not `'expression-bodied method'` -- and this message is a search key. Measured
    /// beside the method form. (csc has further kinds, notably expression-bodied constructors and
    /// accessors at C# 7.0; they get variants when there is a site to raise them from.)
    ExpressionBodiedProperty,
    /// An object initializer, `new C { F = 1 }`. Introduced in C# 3.0.
    ///
    /// **csc names the two initializer forms differently and this one is the DEFAULT**: an empty
    /// `new C { }` is reported as an object initializer, measured. See
    /// [`Feature::CollectionInitializer`].
    ObjectInitializer,
    /// A collection initializer, `new ArrayList { 1, 2 }`. Introduced in C# 3.0.
    ///
    /// Same version as [`Feature::ObjectInitializer`] and a different NOUN, which is the whole
    /// reason the two are separate variants: one enum entry could carry the right version and only
    /// ever half the right message. csc tells them apart by whether the first element is an
    /// assignment, and so do we.
    CollectionInitializer,
    /// An anonymous object creation `new { A = 1, ... }`, producing an instance of a
    /// compiler-synthesized anonymous type. Introduced in C# 3.0.
    AnonymousObjectCreation,
    /// An implicitly typed local variable -- `var x = 5;`, whose type is inferred from the
    /// initializer. Introduced in C# 3.0 (C# 3.0 spec, 8.5.1).
    ///
    /// **ONE variant covers the `foreach` iteration variable too** (8.8.4), because csc gates both
    /// under this single name -- measured, `foreach (var v in a)` at ISO-1 reports `Feature
    /// 'implicitly typed local variable'`, naming a LOCAL VARIABLE for an iteration variable. A
    /// second variant would have produced a message no csc user ever sees.
    ///
    /// **`var` is a CONTEXTUAL keyword and this feature does not own the word.** A type genuinely
    /// named `var` takes precedence (C# 3.0 spec, 8.5.1: *"and no type named var is in scope"*), so
    /// nothing here fires for a program that declares one -- which is why the gate is raised from
    /// the binder, after resolution has been tried, rather than from the lexer.
    ImplicitlyTypedLocalVariable,
    /// A default (optional) parameter value, for example `void M(int x = 5)`. Introduced in C# 4.0.
    DefaultParameterValues,
    /// A named argument, for example `M(name: value)`. Introduced in C# 4.0.
    NamedArguments,
    /// The null-conditional operators `?.` and `?[`. Introduced in C# 6.0.
    NullConditional,
    /// A `using static` directive (`using static System.Math;`), importing a type's static
    /// members into scope. Introduced in C# 6.0.
    UsingStatic,
    /// An automatically implemented property -- a concrete `{ get; set; }` whose accessors have no
    /// bodies. Introduced in C# 3.0.
    AutoProperties,
    /// `switch` on a `bool` governing expression. Introduced in C# 2.0. (C# 1.0 restricts the
    /// governing type to the integral types, `char`, `string` and enums.)
    SwitchOnBool,
    /// Binary integer literals, `0b1010_0101`. Introduced in C# 7.0.
    BinaryLiterals,
    /// Digit separators in a numeric literal, `1_000_000`. Introduced in C# 7.0.
    DigitSeparators,
    /// Top-level statements -- a compilation unit whose statements ARE the program, with no
    /// enclosing type and no `Main`. Introduced in C# 9.0.
    ///
    /// **The reason the ceiling reaches C# 9 at all.** Every dotnet/iot and nanoFramework sample
    /// program is written this way, so the dialect a drop-in sample needs is set by the samples
    /// rather than by the libraries -- whose own public surface is C# 6-8.
    TopLevelStatements,
    /// A file-scoped namespace declaration -- `namespace N;` rather than `namespace N { ... }`.
    /// Introduced in C# 10.0. **115 files of dotnet/iot are written this way**, and it is pure
    /// syntax: no metadata, no runtime, no binder change, so it is the highest ratio of
    /// files-unblocked to work on the compatibility list.
    FileScopedNamespaces,
    /// A `required` member -- an initializer the caller MUST supply. Introduced in C# 11.0.
    ///
    /// **The one C# 11 feature both compatibility targets adopted**: 64 uses in dotnet/iot, 56 in
    /// nanoFramework. Earlier research reported zero, having measured only the PUBLIC surface; the
    /// uses are in drivers and internals. Unlike the two above it is a real feature rather than
    /// syntax -- it needs `RequiredMemberAttribute` and an initialization-safety rule.
    RequiredMembers,
    /// A digit separator IMMEDIATELY AFTER a base prefix -- `0x_FF`, `0b_1010`. Introduced in
    /// C# 7.2, a full release AFTER separators themselves.
    ///
    /// **csc treats this as its own feature and rejects it at C# 7.0** ("Feature ''leading digit
    /// separator'' is not available in C# 7.0"), measured. So a compiler that implemented C# 7.0
    /// separators and allowed a leading one would ACCEPT what csc rejects -- the serious column.
    LeadingDigitSeparator,
    /// A record type -- a class or struct with generated value equality, `with`, `ToString` and
    /// `Deconstruct`. Introduced in C# 9.0.
    ///
    /// **Sequenced AFTER generics deliberately**: a record is a code generator whose output binds
    /// `IEquatable<T>`, so it cannot land before the thing it generates against. 44 uses in
    /// dotnet/iot, 11 in nanoFramework.
    Records,
    /// A default (bodied) member on an interface -- C# 8.0. Gated in the BINDER rather than the
    /// parser: the syntax is an ordinary member with a body and only the enclosing type's kind
    /// makes it a feature.
    DefaultInterfaceImplementation,
    /// An explicit parameterless constructor on a struct -- C# 10.0. Also binder-gated, and for
    /// the same reason: the declaration is well-formed and the enclosing type's kind is what
    /// makes it late.
    ParameterlessStructConstructor,
    /// An async function -- the `async` modifier and the `await` operator (ECMA-334 5th ed,
    /// 12.8.8 and 15.15). Introduced in C# 5.0, and unlike `var` this rung IS ECMA-standardized.
    ///
    /// **ONE variant covers the modifier and the operator**, because csc gates both under this
    /// single name -- measured, one compilation each: the `async` modifier below 5 reports
    /// `Feature 'async function'`, and an `await` OPERATOR in a non-async method below 5 reports
    /// the same gate at the `await` token (beside CS4033). Inside an async method the modifier's
    /// gate is the only one raised -- an `await` in the body adds nothing below 5, also measured.
    ///
    /// **Both words are CONTEXTUAL.** `async` is a modifier only by lookahead (a program may
    /// declare `class async` and a method `async async()` returning it -- both compile in csc);
    /// `await` is reserved only inside a method whose modifiers include `async`, and the verbatim
    /// `@await` is an ordinary identifier even there (12.8.8.1).
    AsyncFunction,
    /// An async entry point -- `static async Task Main()`. Introduced in **C# 7.1**, two rungs
    /// after async functions themselves, and gated separately by csc under its own name --
    /// measured: at `/langversion:5` an async `Task Main` reports `Feature 'async main' is not
    /// available in C# 5. Please use language version 7.1 or greater.` alongside CS5001, while
    /// `static async void Main` is CS4009 at every version (an entry point can never be
    /// async-void). Without this variant the 7.1 rung would have nothing to name.
    AsyncMain,
    /// An async method returning `Task<T>` -- one of the three return types 15.15.1 admits, and
    /// the one that is generic. csc has no separate gate (it is simply part of async functions);
    /// the variant exists so lcsc's PHASE SPLIT can refuse it by name: `Task<T>` and its builder
    /// sit behind the generics-costed corlib work, so `void` and `Task` land first
    /// (the phase-1 design records the split). The description is OURS, in csc's style, because
    /// there is no csc message to copy.
    AsyncTaskOfT,
    /// A GENERIC async method -- `async Task M<T>(...)`. Also csc-gateless and also a phase
    /// split: the state machine for a generic method is a synthesized generic type, which lands
    /// beside `Task<T>` (in real code the two populations are nearly the same methods).
    AsyncGenericMethod,
    /// An `await` inside a `catch` or `finally` block -- introduced in **C# 6.0**. Below 6 the
    /// refusal is csc's own CS1985/CS1984 (measured, and the two texts are asymmetric); at 6 and
    /// above csc compiles it (measured at both rungs), so what remains for lcsc is the
    /// permitted-but-unbuilt half: the machinery that resumes inside a handler (exception
    /// spilling, pending-fault rethrow) is not part of phase 1.
    AwaitInCatchOrFinally,
}

impl Feature {
    /// Whether **lcsc has built** this feature, as distinct from whether a language version
    /// PERMITS it ([`Self::introduced_in`]).
    ///
    /// **The two questions are independent and conflating them is the trap this method exists to
    /// prevent.** A selectable `/langversion:7` must not be read as "lcsc implements C# 7": it
    /// selects a DIALECT, and this table says which of that dialect's features we can actually
    /// compile. A feature the dialect permits and we have not built has to be refused by NAME --
    /// telling the user to "use language version 7 or greater" when they already did would be a
    /// lie, and one that sends them looking for a compiler switch that cannot help.
    ///
    /// `static class` is the shape to keep in mind: its emit path is complete (see the
    /// `GATED FEATURE (ISO-2)` markers in `compile.rs` and `declaration.rs`) and it is refused
    /// anyway, because ISO-1 is the dialect. Implemented and permitted are two bits, and only both
    /// together admit a construct.
    #[must_use]
    pub fn is_implemented(self) -> bool {
        match self {
            Feature::StaticClasses | Feature::BinaryLiterals | Feature::DigitSeparators => true,
            Feature::FileScopedNamespaces => true,
            Feature::ObjectInitializer | Feature::CollectionInitializer => true,
            Feature::ImplicitlyTypedLocalVariable => true,
            Feature::Generics => true,
            Feature::DefaultOperator => true,
            Feature::NullCoalescing => true,
            Feature::PragmaDirective | Feature::AutoProperties => true,
            Feature::AccessorAccessibility => true,
            Feature::NullableValueTypes => true,
            Feature::AnonymousMethods
            | Feature::NamespaceAlias
            | Feature::LambdaExpression
            | Feature::ExpressionBodiedMethod
            | Feature::ExpressionBodiedProperty
            | Feature::AnonymousObjectCreation
            | Feature::DefaultParameterValues
            | Feature::NamedArguments
            | Feature::NullConditional
            | Feature::UsingStatic
            | Feature::ReadonlyAutoProperty
            | Feature::SwitchOnBool

            | Feature::TopLevelStatements
            | Feature::LeadingDigitSeparator
            | Feature::RequiredMembers
            | Feature::Records
            | Feature::DefaultInterfaceImplementation
            | Feature::ParameterlessStructConstructor => false,
            Feature::AsyncFunction => true,
            Feature::PartialTypes => true,
            Feature::AsyncMain => false,
            Feature::AsyncTaskOfT
            | Feature::AsyncGenericMethod
            | Feature::AwaitInCatchOrFinally => false,
        }
    }

    /// Every feature this compiler gates on, so a check over "all of them" is DERIVED rather than
    /// transcribed.
    ///
    ///
    /// **A HAND-WRITTEN LIST OF FEATURES IS A LIST OF THE FEATURES SOMEBODY REMEMBERED.** The
    /// rung tests below used to name their own table, and it had drifted to seventeen of
    /// twenty-seven -- ten features, including `Records`, `RequiredMembers` and
    /// `TopLevelStatements`, were asserted about by nothing at all. A gate whose population is
    /// typed out by hand tests the day it was written.
    ///
    /// Two things keep this honest and they are both compiler-enforced, not remembered: the
    /// exhaustive `match` in `every_feature_is_in_all` fails to compile when a variant is added,
    /// and the length assertion beside it fails until the variant is added HERE too.
    pub const ALL: [Feature; 38] = [
        Feature::Generics,
        Feature::StaticClasses,
        Feature::AnonymousMethods,
        Feature::NullableValueTypes,
        Feature::DefaultOperator,
        Feature::NullCoalescing,
        Feature::PragmaDirective,
        Feature::ReadonlyAutoProperty,
        Feature::NamespaceAlias,
        Feature::AccessorAccessibility,
        Feature::LambdaExpression,
        Feature::ExpressionBodiedMethod,
        Feature::ExpressionBodiedProperty,
        Feature::ObjectInitializer,
        Feature::CollectionInitializer,
        Feature::AnonymousObjectCreation,
        Feature::ImplicitlyTypedLocalVariable,
        Feature::DefaultParameterValues,
        Feature::NamedArguments,
        Feature::NullConditional,
        Feature::UsingStatic,
        Feature::AutoProperties,
        Feature::SwitchOnBool,
        Feature::BinaryLiterals,
        Feature::DigitSeparators,
        Feature::TopLevelStatements,
        Feature::FileScopedNamespaces,
        Feature::RequiredMembers,
        Feature::LeadingDigitSeparator,
        Feature::Records,
        Feature::DefaultInterfaceImplementation,
        Feature::ParameterlessStructConstructor,
        Feature::AsyncFunction,
        Feature::AsyncMain,
        Feature::AsyncTaskOfT,
        Feature::AsyncGenericMethod,
        Feature::AwaitInCatchOrFinally,
        Feature::PartialTypes,
    ];

    /// The first language version in which this feature is available.
    #[must_use]
    pub fn introduced_in(self) -> LanguageVersion {
        match self {
            Feature::Generics
            | Feature::StaticClasses
            | Feature::AnonymousMethods
            | Feature::NullableValueTypes
            | Feature::NullCoalescing
            | Feature::AccessorAccessibility
            | Feature::DefaultOperator
            | Feature::PragmaDirective
            | Feature::PartialTypes
            | Feature::NamespaceAlias => LanguageVersion::CSharp2,
            Feature::LambdaExpression
            | Feature::ObjectInitializer
            | Feature::CollectionInitializer
            | Feature::AnonymousObjectCreation
            | Feature::ImplicitlyTypedLocalVariable => LanguageVersion::CSharp3,
            Feature::DefaultParameterValues | Feature::NamedArguments => LanguageVersion::CSharp4,
            Feature::SwitchOnBool => LanguageVersion::CSharp2,
            Feature::AutoProperties => LanguageVersion::CSharp3,
            Feature::ExpressionBodiedMethod | Feature::ExpressionBodiedProperty => {
                LanguageVersion::CSharp6
            }
            Feature::NullConditional
            | Feature::UsingStatic
            | Feature::ReadonlyAutoProperty => LanguageVersion::CSharp6,
            Feature::BinaryLiterals | Feature::DigitSeparators => LanguageVersion::CSharp7,
            Feature::LeadingDigitSeparator => LanguageVersion::CSharp7_2,
            Feature::TopLevelStatements | Feature::Records => LanguageVersion::CSharp9,
            Feature::FileScopedNamespaces => LanguageVersion::CSharp10,
            Feature::RequiredMembers => LanguageVersion::CSharp11,
            Feature::DefaultInterfaceImplementation => LanguageVersion::CSharp8,
            Feature::ParameterlessStructConstructor => LanguageVersion::CSharp10,
            Feature::AsyncFunction => LanguageVersion::CSharp5,
            Feature::AsyncMain => LanguageVersion::CSharp7_1,
            Feature::AsyncTaskOfT | Feature::AsyncGenericMethod => LanguageVersion::CSharp5,
            Feature::AwaitInCatchOrFinally => LanguageVersion::CSharp6,
        }
    }

    /// The noun phrase csc quotes in `Feature '<name>' is not available in C# N`.
    ///
    /// **EVERY STRING HERE IS csc's, MEASURED one compilation each**, because the message is a
    /// search key: a user who pastes it into a search engine has to land on the same results a csc
    /// user does.
    ///
    /// | we said | csc says |
    /// |---|---|
    /// | nullable value types | **nullable types** |
    /// | the namespace alias qualifier `'::'` | **namespace alias qualifier** |
    /// | accessor access modifiers | **access modifiers on properties** |
    /// | lambda and expression-bodied members (`'=>'`) | **lambda expression** *and* **expression-bodied method** / **expression-bodied property**, separately |
    /// | object and collection initializers | **object initializer** *and* **collection initializer**, separately |
    /// | optional parameters | **optional parameter** (singular) |
    /// | named arguments | **named argument** (singular) |
    /// | null-conditional operators (`'?.'` and `'?['`) | **null propagating operator** (one name, both operators -- measured) |
    ///
    /// **THE PATTERN IS THAT A DESCRIPTION OF THE LANGUAGE IS NOT csc's NAME FOR THE CONSTRUCT**:
    /// a parenthesized operator spelling, a plural where csc reports one occurrence,
    /// and two features merged where the message has to pick one noun. The last kind is the one
    /// that cost something real -- see [`Feature::ExpressionBodiedMethod`], where the merge also
    /// carried the wrong VERSION.
    ///
    /// **Do NOT add quotes here.** The renderers supply them, and a string carrying its own
    /// would nest.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Feature::Generics => "generics",
            Feature::DefaultOperator => "default operator",
            Feature::AnonymousMethods => "anonymous methods",
            Feature::NullableValueTypes => "nullable types",
            Feature::NullCoalescing => "null coalescing operator",
            Feature::ReadonlyAutoProperty => "readonly automatically implemented properties",
            Feature::PragmaDirective => "#pragma",
            Feature::NamespaceAlias => "namespace alias qualifier",
            Feature::AccessorAccessibility => "access modifiers on properties",
            Feature::LambdaExpression => "lambda expression",
            Feature::ExpressionBodiedMethod => "expression-bodied method",
            Feature::ExpressionBodiedProperty => "expression-bodied property",
            Feature::ObjectInitializer => "object initializer",
            Feature::CollectionInitializer => "collection initializer",
            Feature::AnonymousObjectCreation => "anonymous types",
            Feature::ImplicitlyTypedLocalVariable => "implicitly typed local variable",
            Feature::DefaultParameterValues => "optional parameter",
            Feature::NamedArguments => "named argument",
            Feature::NullConditional => "null propagating operator",
            Feature::StaticClasses => "static classes",
            Feature::UsingStatic => "using static",
            Feature::AutoProperties => "automatically implemented properties",
            Feature::SwitchOnBool => "switch on boolean type",
            Feature::BinaryLiterals => "binary literals",
            Feature::DigitSeparators => "digit separators",
            Feature::LeadingDigitSeparator => "leading digit separator",
            Feature::TopLevelStatements => "top-level statements",
            Feature::FileScopedNamespaces => "file-scoped namespace",
            Feature::RequiredMembers => "required members",
            Feature::Records => "records",
            Feature::DefaultInterfaceImplementation => "default interface implementation",
            Feature::ParameterlessStructConstructor => "parameterless struct constructors",
            Feature::AsyncFunction => "async function",
            Feature::PartialTypes => "partial types",
            Feature::AsyncMain => "async main",
            Feature::AsyncTaskOfT => "async method returning Task<T>",
            Feature::AsyncGenericMethod => "generic async method",
            Feature::AwaitInCatchOrFinally => "await in a catch or finally clause",
        }
    }
}

/// The reason a `/langversion` value could not be turned into a
/// [`LanguageVersion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageVersionError {
    /// The value names a real C# version that this compiler does not implement
    /// yet, for example `ISO-2` while only C# 1.0 is supported.
    Unsupported,
    /// The value names no known C# version.
    Invalid,
}

impl fmt::Display for LanguageVersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LanguageVersionError::Unsupported => {
                f.write_str("that C# version is not supported by this compiler yet")
            }
            LanguageVersionError::Invalid => f.write_str("unrecognised C# language version"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_latest_are_csharp1() {
        assert_eq!(LanguageVersion::DEFAULT, LanguageVersion::CSharp1);
        assert_eq!(LanguageVersion::SELECTABLE_MAX, LanguageVersion::CSharp11);
        assert_ne!(LanguageVersion::DEFAULT, LanguageVersion::SELECTABLE_MAX);
        assert!(LanguageVersion::CSharp1.is_selectable());
        assert!(LanguageVersion::CSharp11.is_selectable());
    }

    #[test]
    fn csharp1_does_not_support_post_1_0_features() {
        let v1 = LanguageVersion::CSharp1;
        assert!(!v1.supports(Feature::Generics));
        assert!(!v1.supports(Feature::AnonymousMethods));
        assert!(!v1.supports(Feature::NullableValueTypes));
    }

    #[test]
    fn csharp2_label_supports_its_own_features() {
        assert!(LanguageVersion::CSharp2.supports(Feature::Generics));
    }

    #[test]
    fn permitted_and_implemented_are_two_independent_bits() {
        assert!(Feature::StaticClasses.is_implemented());
        assert!(LanguageVersion::CSharp2.supports(Feature::StaticClasses));
        assert!(!LanguageVersion::CSharp1.supports(Feature::StaticClasses));

        assert!(Feature::Generics.is_implemented());
        assert!(LanguageVersion::CSharp2.supports(Feature::Generics));
        assert!(!LanguageVersion::CSharp1.supports(Feature::Generics));

        assert!(LanguageVersion::CSharp2.supports(Feature::AnonymousMethods));
        assert!(!Feature::AnonymousMethods.is_implemented());

        for feature in Feature::ALL {
            let admitted = LanguageVersion::DEFAULT.supports(feature) && feature.is_implemented();
            assert!(
                !admitted,
                "{feature:?} would be admitted under the default dialect; every feature in this \
                 table is post-1.0 and the default is ISO-1"
            );
        }
    }

    #[test]
    fn every_selectable_version_is_in_all_selectable() {
        for version in LanguageVersion::ALL_SELECTABLE {
            match version {
                LanguageVersion::CSharp1
                | LanguageVersion::CSharp2
                | LanguageVersion::CSharp3
                | LanguageVersion::CSharp4
                | LanguageVersion::CSharp5
                | LanguageVersion::CSharp6
                | LanguageVersion::CSharp7
                | LanguageVersion::CSharp7_1
                | LanguageVersion::CSharp7_2
                | LanguageVersion::CSharp7_3
                | LanguageVersion::CSharp8
                | LanguageVersion::CSharp9
                | LanguageVersion::CSharp10
                | LanguageVersion::CSharp11 => {}
            }
            assert!(
                version.is_selectable(),
                "{version:?} is listed in ALL_SELECTABLE but is not selectable"
            );
        }
        assert_eq!(
            LanguageVersion::ALL_SELECTABLE.len(),
            14,
            "a LanguageVersion variant was added without being added to ALL_SELECTABLE"
        );
        assert_eq!(
            *LanguageVersion::ALL_SELECTABLE.last().expect("non-empty"),
            LanguageVersion::SELECTABLE_MAX,
            "ALL_SELECTABLE must end at the ceiling"
        );
    }

    #[test]
    fn every_feature_is_in_all() {
        for feature in Feature::ALL {
            match feature {
                Feature::Generics
                | Feature::StaticClasses
                | Feature::AnonymousMethods
                | Feature::NullableValueTypes
                | Feature::DefaultOperator
                | Feature::NullCoalescing
                | Feature::PragmaDirective
                | Feature::ReadonlyAutoProperty
                | Feature::NamespaceAlias
                | Feature::AccessorAccessibility
                | Feature::LambdaExpression
                | Feature::ExpressionBodiedMethod
                | Feature::ExpressionBodiedProperty
                | Feature::ObjectInitializer
                | Feature::CollectionInitializer
                | Feature::AnonymousObjectCreation
                | Feature::ImplicitlyTypedLocalVariable
                | Feature::DefaultParameterValues
                | Feature::NamedArguments
                | Feature::NullConditional
                | Feature::UsingStatic
                | Feature::AutoProperties
                | Feature::SwitchOnBool
                | Feature::BinaryLiterals
                | Feature::DigitSeparators
                | Feature::TopLevelStatements
                | Feature::FileScopedNamespaces
                | Feature::RequiredMembers
                | Feature::LeadingDigitSeparator
                | Feature::Records
                | Feature::DefaultInterfaceImplementation
                | Feature::ParameterlessStructConstructor
                | Feature::AsyncFunction
                | Feature::AsyncMain
                | Feature::AsyncTaskOfT
                | Feature::AsyncGenericMethod
                | Feature::AwaitInCatchOrFinally
                | Feature::PartialTypes => {}
            }
        }
        assert_eq!(
            Feature::ALL.len(),
            38,
            "a Feature variant was added without being added to Feature::ALL"
        );
    }

    #[test]
    fn async_gates_are_the_measured_ones() {
        assert_eq!(Feature::AsyncFunction.description(), "async function");
        assert_eq!(Feature::AsyncFunction.introduced_in(), LanguageVersion::CSharp5);
        assert_eq!(Feature::AsyncFunction.introduced_in().required_name(), "5");
        assert_eq!(Feature::AsyncMain.description(), "async main");
        assert_eq!(Feature::AsyncMain.introduced_in(), LanguageVersion::CSharp7_1);
        assert_eq!(Feature::AsyncMain.introduced_in().required_name(), "7.1");
        assert_eq!(LanguageVersion::CSharp4.feature_gate_code(), 8025);
        assert_eq!(LanguageVersion::CSharp5.feature_gate_code(), 8026);
        assert!(!LanguageVersion::CSharp4.supports(Feature::AsyncFunction));
        assert!(LanguageVersion::CSharp5.supports(Feature::AsyncFunction));
        assert!(!LanguageVersion::CSharp7.supports(Feature::AsyncMain));
        assert!(LanguageVersion::CSharp7_1.supports(Feature::AsyncMain));
    }

    #[test]
    fn supports_is_exactly_at_or_after_the_introducing_version() {
        for feature in Feature::ALL {
            let introduced = feature.introduced_in();
            for version in LanguageVersion::ALL_SELECTABLE {
                assert_eq!(
                    version.supports(feature),
                    version >= introduced,
                    "{feature:?} (introduced in {introduced:?}) is mis-gated at {version:?}"
                );
            }
        }
    }

    #[test]
    fn the_default_rung_admits_no_post_1_0_feature() {
        for feature in Feature::ALL {
            if feature.introduced_in() == LanguageVersion::CSharp1 {
                continue;
            }
            let admitted = LanguageVersion::DEFAULT.supports(feature) && feature.is_implemented();
            assert!(
                !admitted,
                "{feature:?} would be admitted under the default dialect; it is post-1.0 and the                  default is ISO-1"
            );
        }
    }

    #[test]
    fn the_feature_gate_code_names_the_version_being_compiled() {
        assert_eq!(LanguageVersion::CSharp1.feature_gate_code(), 8022);
        assert_eq!(LanguageVersion::CSharp2.feature_gate_code(), 8023);
        assert_eq!(LanguageVersion::CSharp3.feature_gate_code(), 8024);
        assert_eq!(LanguageVersion::CSharp4.feature_gate_code(), 8025);
        assert_eq!(LanguageVersion::CSharp5.feature_gate_code(), 8026);
        assert_eq!(LanguageVersion::CSharp6.feature_gate_code(), 8059);
        let codes = [
            LanguageVersion::CSharp1,
            LanguageVersion::CSharp2,
            LanguageVersion::CSharp3,
            LanguageVersion::CSharp4,
            LanguageVersion::CSharp5,
            LanguageVersion::CSharp6,
        ]
        .map(LanguageVersion::feature_gate_code);
        for window in codes.windows(2) {
            assert_ne!(window[0], window[1], "each version has its OWN gate code");
        }
    }

    #[test]
    fn a_version_renders_differently_as_current_and_as_required() {
        assert_eq!(LanguageVersion::CSharp1.message_name(), "1");
        assert_eq!(LanguageVersion::CSharp2.message_name(), "2");

        assert_eq!(LanguageVersion::CSharp2.required_name(), "2");
        assert_eq!(LanguageVersion::CSharp5.required_name(), "5");
        assert_eq!(LanguageVersion::CSharp6.required_name(), "6");
        assert_eq!(LanguageVersion::CSharp7.required_name(), "7.0");

        assert_eq!(LanguageVersion::CSharp6.message_name(), "6");
        assert_eq!(LanguageVersion::CSharp7.message_name(), "7.0");
        assert_eq!(LanguageVersion::CSharp9.message_name(), "9.0");
    }

    #[test]
    fn every_feature_name_is_the_one_csc_quotes() {
        assert_eq!(Feature::NullableValueTypes.description(), "nullable types");
        assert_eq!(Feature::NamespaceAlias.description(), "namespace alias qualifier");
        assert_eq!(
            Feature::AccessorAccessibility.description(),
            "access modifiers on properties"
        );
        assert_eq!(Feature::LambdaExpression.description(), "lambda expression");
        assert_eq!(Feature::ObjectInitializer.description(), "object initializer");
        assert_eq!(Feature::CollectionInitializer.description(), "collection initializer");
        assert_eq!(Feature::DefaultParameterValues.description(), "optional parameter");
        assert_eq!(Feature::NamedArguments.description(), "named argument");
        assert_eq!(Feature::NullConditional.description(), "null propagating operator");

        for feature in Feature::ALL {
            let description = feature.description();
            assert!(
                !description.contains('\''),
                "{feature:?}'s description carries its own quotes ({description:?}); the renderer \
                 supplies them"
            );
            assert!(
                !description.is_empty() && !description.ends_with('.'),
                "{feature:?}'s description is a noun phrase, not a sentence: {description:?}"
            );
        }
    }

    #[test]
    fn a_lambda_and_an_expression_bodied_member_share_a_token_and_not_a_version() {
        assert_eq!(Feature::LambdaExpression.introduced_in(), LanguageVersion::CSharp3);
        assert_eq!(
            Feature::ExpressionBodiedMethod.introduced_in(),
            LanguageVersion::CSharp6
        );
        assert_eq!(
            Feature::ExpressionBodiedProperty.introduced_in(),
            LanguageVersion::CSharp6
        );
        assert!(!LanguageVersion::CSharp5.supports(Feature::ExpressionBodiedMethod));
        assert!(LanguageVersion::CSharp5.supports(Feature::LambdaExpression));

        assert_eq!(
            Feature::ObjectInitializer.introduced_in(),
            Feature::CollectionInitializer.introduced_in()
        );
        assert_ne!(
            Feature::ObjectInitializer.description(),
            Feature::CollectionInitializer.description()
        );
    }

    #[test]
    fn parse_flag_accepts_csharp1_spellings() {
        for value in ["ISO-1", "iso-1", "1", "1.0", " 1 ", "default"] {
            assert_eq!(
                LanguageVersion::parse_flag(value),
                Ok(LanguageVersion::CSharp1),
                "value was {value:?}"
            );
        }
        for value in ["latest", "LATESTMAJOR"] {
            assert_eq!(
                LanguageVersion::parse_flag(value),
                Ok(LanguageVersion::SELECTABLE_MAX),
                "value was {value:?}"
            );
        }
        assert_ne!(
            LanguageVersion::parse_flag("default"),
            LanguageVersion::parse_flag("latest"),
            "default and latest answer different questions"
        );
    }

    #[test]
    fn parse_flag_reports_unimplemented_versions_distinctly() {
        for (value, expected) in [
            ("ISO-2", LanguageVersion::CSharp2),
            ("2", LanguageVersion::CSharp2),
            ("2.0", LanguageVersion::CSharp2),
            ("6", LanguageVersion::CSharp6),
            ("7", LanguageVersion::CSharp7),
            ("7.0", LanguageVersion::CSharp7),
            ("7.2", LanguageVersion::CSharp7_2),
            ("7.3", LanguageVersion::CSharp7_3),
            ("11", LanguageVersion::CSharp11),
        ] {
            assert_eq!(LanguageVersion::parse_flag(value), Ok(expected), "value was {value:?}");
        }
        for value in ["12", "14", "preview"] {
            assert_eq!(
                LanguageVersion::parse_flag(value),
                Err(LanguageVersionError::Unsupported),
                "value was {value:?}"
            );
        }
    }

    #[test]
    fn parse_flag_rejects_nonsense() {
        for value in ["", "csharp", "99", "1.5", "iso"] {
            assert_eq!(
                LanguageVersion::parse_flag(value),
                Err(LanguageVersionError::Invalid),
                "value was {value:?}"
            );
        }
    }

    #[test]
    fn flag_value_round_trips_for_supported_versions() {
        let v1 = LanguageVersion::CSharp1;
        assert_eq!(LanguageVersion::parse_flag(v1.flag_value()), Ok(v1));
    }
}
