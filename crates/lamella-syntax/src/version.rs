//! Language versioning and feature gating.

use core::fmt;

/// A version of the C# language.
///
/// Every variant through [`LanguageVersion::SELECTABLE_MAX`] can be SELECTED, which is a statement
/// about what this build can gate against rather than what it implements -- see
/// [`Feature::is_implemented`] for the other half.
///
/// Ordering follows release order, so `>=` is the natural way to ask whether a
/// version is recent enough for a given feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum LanguageVersion {
    /// C# 1.0, as standardized by ECMA-334 1st edition (December 2001).
    CSharp1,
    /// C# 2.0 (ECMA-334 3rd ed). Present only to gate post-1.0 features; not implemented.
    CSharp2,
    /// C# 3.0. A gating label only; not implemented.
    CSharp3,
    /// C# 4.0. A gating label only; not implemented.
    CSharp4,
    /// C# 5.0. A gating label only; not implemented. Present because the DIAGNOSTIC CODE for a
    /// too-new feature is keyed on the version being compiled (see
    /// [`LanguageVersion::feature_gate_code`]) and C# 5 has its own -- a gap in this enum would
    /// silently borrow its neighbor's code.
    CSharp5,
    /// C# 6.0 (ECMA-334 6th ed, 2022). A gating label only; not implemented.
    CSharp6,
    /// C# 7.0 (ECMA-334 7th ed, 2023 -- ISO/IEC 20619:2023 -- the latest ratified standard). A
    /// gating label only; not implemented.
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
    /// C# 8.0. A gating label only; not implemented.
    CSharp8,
    /// C# 9.0 -- the rung that carries TOP-LEVEL STATEMENTS, which is why the ceiling reaches here:
    /// every dotnet/iot and nanoFramework sample program is a bare statement file with no `Main`.
    /// A gating label only; not implemented.
    CSharp9,
    /// C# 10.0 -- the rung that carries FILE-SCOPED NAMESPACES, which 115 files of dotnet/iot use.
    /// A gating label only; not implemented.
    CSharp10,
    /// C# 11.0 -- the rung that carries REQUIRED MEMBERS, the one C# 11 feature both compatibility
    /// targets adopted (64 uses in dotnet/iot, 56 in nanoFramework). A gating label only; not
    /// implemented.
    CSharp11,
}

/// The dialect a `LexOptions` carries when nothing sets one: [`LanguageVersion::DEFAULT`].
///
/// **WRITTEN OUT RATHER THAN DERIVED, SO THE TWO SPELLINGS OF "DEFAULT" CANNOT COME APART.**
/// `LanguageVersion::DEFAULT` (the associated const) and `LanguageVersion::default()` (this trait)
/// are different spellings a reader takes for one thing. `LexOptions` derives `Default`, so every
/// compilation that sets no version arrives through this impl rather than through the const -- and
/// a `#[default]` attribute on a variant would answer that question independently of the const.
/// There is one answer and the const is it.
impl Default for LanguageVersion {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl LanguageVersion {
    /// The dialect compiled when no `/langversion` is given: **the lowest rung that permits every
    /// feature this build implements**, derived rather than chosen.
    ///
    /// Compiling an ordinary program must not produce a wall of *"not available in C# 1.0"*. A
    /// default below what the build implements refuses a construct by DIALECT that the compiler
    /// can produce, and says *"use language version 9 or greater"* to someone who cannot fix it
    /// with a switch -- the exact lie [`Feature::gate_against`]'s two-bit rule exists to prevent,
    /// told in reverse.
    ///
    /// **DERIVED, SO IT CANNOT GO STALE, AND NOT ALIASED TO [`Self::SELECTABLE_MAX`].** The two
    /// answer different questions -- the ceiling is what `/langversion` may SELECT, this is what a
    /// compilation gets when it does not ask -- and they are equal today only because the newest
    /// implemented feature happens to sit at the newest selectable rung. Aliasing would make a
    /// raised ceiling silently move every unqualified compilation; deriving moves this one only
    /// when a feature actually lands.
    ///
    pub const DEFAULT: LanguageVersion = Self::lowest_rung_admitting_every_built_feature();

    /// The rung [`Self::DEFAULT`] derives to: the HIGHEST `introduced_in` among the features this
    /// build implements.
    ///
    /// That maximum IS "the lowest rung that permits them all", because
    /// [`Self::supports`] is `self >= feature.introduced_in()` -- a rung admits a feature exactly
    /// when it is at least the feature's own, so the lowest rung admitting every one of them is the
    /// greatest of theirs. Written as the maximum rather than as a search so the reason is on the
    /// page instead of in a loop.
    const fn lowest_rung_admitting_every_built_feature() -> LanguageVersion {
        let mut highest = LanguageVersion::CSharp1;
        let mut index = 0;
        while index < Feature::ALL.len() {
            let feature = Feature::ALL[index];
            if feature.is_implemented() {
                let rung = feature.introduced_in();
                if rung as u8 > highest as u8 {
                    highest = rung;
                }
            }
            index += 1;
        }
        highest
    }

    /// The newest dialect `/langversion` will select.
    ///
    /// **Selecting a dialect is NOT a claim that lcsc implements all of it.** A dialect decides
    /// which constructs are PERMITTED; [`Feature::is_implemented`] decides which of those this build
    /// can actually produce, and a construct needs both. So the ceiling can name every version
    /// this build can gate against, and a permitted-but-unbuilt construct is refused by name
    /// (`LAM0001`) rather than by pretending the dialect forbids it.
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
/// language but is beyond what this compiler implements.
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
/// The table is seeded with a few post-1.0 features to fix the pattern: a feature
/// gains a variant and the version that introduced it, and the parser or binder
/// calls [`LanguageVersion::supports`] before accepting the construct. C# 1.0
/// features need no gate and so do not appear here.
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
    NullCoalescing,
    /// An EXCEPTION FILTER -- `catch (E e) when (cond)`. Introduced in C# 6.0.
    ///
    /// **IT IS NOT AN `if` AT THE TOP OF THE HANDLER, AND THE DIFFERENCE IS OBSERVABLE.** A filter
    /// runs BEFORE the stack unwinds, in the two-pass exception model the CLI has always had
    /// (II.12.4.2): if it answers false the exception keeps travelling from its ORIGINAL throw
    /// point, so an outer handler sees the frames that were there. Rewriting one as `catch { if
    /// (!cond) throw; }` unwinds first and rethrows from the wrong place, which is why the feature
    /// exists at all rather than being sugar.
    ///
    ExceptionFilter,
    /// A GETTER-ONLY automatically implemented property, `int P { get; }`. Introduced in C# 6.0.
    ///
    ReadonlyAutoProperty,
    /// A PARTIAL TYPE -- `partial class C` written more than once, the declarations merging into
    /// one type (ECMA-334 4th ed 17.1.4). Introduced in C# 2.0.
    ///
    PartialTypes,
    /// A `#pragma` directive (`#pragma warning disable 649`). Introduced in C# 2.0.
    ///
    PragmaDirective,
    /// The namespace alias qualifier `::` (`global::System`). Introduced in C# 2.0.
    NamespaceAlias,
    /// An access modifier on a property, indexer, or event accessor, for example `private set`.
    /// Introduced in C# 2.0.
    AccessorAccessibility,
    /// A lambda expression, `x => x`. Introduced in C# 3.0.
    ///
    LambdaExpression,
    /// An expression-bodied method, `int M() => 1;`. Introduced in **C# 6.0**.
    ///
    ///
    /// **A lexer cannot make this distinction** -- `=>` is one token and which feature it belongs
    /// to is a question about the enclosing declaration. So the lexer keeps gating
    /// [`Feature::LambdaExpression`], which is the correct LOWER bound (`=>` does not exist at all
    /// before C# 3), and the C# 6 half needs a parser-side site that arrives with the feature.
    ExpressionBodiedMethod,
    /// An expression-bodied property, `int P => 1;`. Introduced in **C# 6.0**.
    ///
    ExpressionBodiedProperty,
    /// An expression-bodied INDEXER, `int this[int i] => 1;`. Introduced in **C# 6.0**.
    ///
    ExpressionBodiedIndexer,
    /// An expression-bodied ACCESSOR, `get => 1;`. Introduced in **C# 7.0** -- a rung LATER than
    /// the member forms that share the token, which is why it is a fourth variant rather than a
    /// spelling of the other three.
    ///
    ExpressionBodiedAccessor,
    /// An expression-bodied CONSTRUCTOR, static constructor or destructor. Introduced in
    /// **C# 7.0**, a rung later than the method and property forms.
    ///
    /// The body is a statement expression rather than a returned value -- none of these three
    /// members returns anything -- so it desugars to `{ e; }` and not to `{ return e; }`.
    ///
    ExpressionBodiedConstructor,
    /// A CONSTANT pattern: `x is null`, `x is 3`, `s is "a"`, `c is 'x'`. Introduced in
    /// **C# 7.0**.
    ///
    ///
    ///
    ConstantPattern,
    /// An OUT VARIABLE DECLARATION, `M(out int x)` and `M(out var x)`. Introduced in **C# 7.0**.
    ///
    /// **THE DECLARATION IS THE FEATURE, NOT THE `out`.** Passing `out` to an already-declared
    /// variable is C# 1.0 and stays there; what arrives at 7.0 is a variable declared INSIDE an
    /// expression, whose scope is the enclosing block rather than the argument list. `ref` has no
    /// such form at any version -- `M(ref int x)` is `CS1525`, measured -- so this is `out` alone.
    ///
    OutVariableDeclaration,
    /// A TUPLE type or expression -- `(int, string)`, `(1, "x")`, `(a: 1, b: 2)`. Introduced in
    /// **C# 7.0**.
    ///
    ///
    ///
    Tuples,
    /// A DISCARD, `_`, in a deconstruction target list. Introduced in **C# 7.0**.
    ///
    ///
    Discards,
    /// A DECLARATION PATTERN, `x is T t`. Introduced in **C# 7.0**.
    ///
    DeclarationPattern,
    /// A SWITCH EXPRESSION, `governing switch { pattern => value, ... }`. Introduced in
    /// **C# 8.0**.
    ///
    ///
    ///
    /// **WHAT IS BUILT IS THE CONSTANT / DISCARD / DECLARATION ARM WITH AN OPTIONAL `when`.**
    /// Measured on dotnet/iot: 130 of its 172 switch expressions use only those forms and are
    /// closed by a catch-all. The C# 9 forms -- `or`, `and`, `not`, relational (`< n`) and
    /// recursive (`{ P: v }`, `(a, b)`) -- are refused, each as its own gap.
    SwitchExpression,
    /// A USING DECLARATION, `using T x = e;` without parentheses or a body. Introduced in
    /// **C# 8.0**.
    ///
    /// **IT IS THE `using` STATEMENT WITH ITS BODY IMPLIED**, and that is measured rather than
    /// assumed: `using T x = e;` followed by the rest of a block compiles to the same IL as
    /// `using (T x = e) { <the rest of that block> }`. Under `/optimize+` seven of eight measured
    /// pairs are BYTE-IDENTICAL; the eighth differs only in which local slot number the variable
    /// gets, because the declaration form is declared in the enclosing block's scope and the
    /// statement form opens a new one. Same `try`/`finally`, same null test, same `Dispose` call,
    /// same `leave`.
    ///
    /// So the parser desugars it and no later pass learns a new shape -- the same treatment an
    /// expression body gets, and for the same reason.
    ///
    /// **THE POSITIONS IT MAY NOT STAND IN ARE NOT THE `using` STATEMENT'S.** A `using` statement
    /// is legal as an embedded statement and inside a switch section; the DECLARATION is refused
    /// in both -- `CS1023` in the first, `CS8647` in the second.
    ///
    UsingDeclaration,
    /// A THROW EXPRESSION, `s ?? throw new ArgumentNullException(nameof(s))`. Introduced in
    /// **C# 7.0**.
    ///
    ///
    /// `int M() => throw e;` needs none of this: an expression body already desugars to the block
    /// it means, and the block a throw expression means is `{ throw e; }` -- the STATEMENT, which
    /// every pass has always handled.
    ThrowExpression,
    /// A `ref struct` declaration, `ref struct S { }`. Introduced in **C# 7.2**.
    ///
    ///
    /// **THE RESTRICTIONS ARE THE FEATURE.** Declaring one is a parser change of a few lines;
    /// what makes it correct is refusing every position that would put it on the heap.
    RefStruct,
    /// A `stackalloc` anywhere but a local's initializer, where C# 7.2 admits it: as an argument,
    /// an operand, a parenthesized expression, a receiver. There its type is `Span<T>`, and it
    /// converts as any span does. Introduced in C# 8.0.
    StackAllocNestedExpressions,
    /// A `stackalloc` with an initializer -- `stackalloc T[] { ... }`, `stackalloc T[n] { ... }`
    /// and `stackalloc[] { ... }`, which infers its element type. Introduced in C# 7.3.
    StackAllocInitializer,
    /// A by-reference local or return: `ref T M()`, `ref T this[int i]`, `ref T P { get; }` and
    /// `ref T r = ref x;`. Introduced in C# 7.0.
    ///
    ///
    /// The declaring half is what a device corlib needs: `System.Span<T>`'s indexer is
    /// `ref T this[int index]`, and a board has no .NET to consume such a member from, so being
    /// able to CONSUME one is a separate capability that does not substitute for this.
    ByRefLocalsAndReturns,
    /// The `readonly` of a `ref readonly T` return. Introduced in C# 7.2.
    ///
    ///
    /// It is a real signature difference and not a spelling: `ref readonly T` is `T&` plus a
    /// `modreq` on `System.Runtime.InteropServices.InAttribute`, which is why `ReadOnlySpan<T>`'s
    /// indexer needed its own work on the consuming side too.
    ReadOnlyReferences,
    /// Ref REASSIGNMENT -- `r = ref a[1];`, rebinding a `ref` local or `ref` parameter to different
    /// storage. Introduced in C# 7.3.
    ///
    ///
    /// It REBINDS rather than writes: `r = ref x` points `r` at `x`, where `r = x` writes through
    /// `r` into whatever it already points at. Both are legal on the same local and they are
    /// different statements, which is why the gate is at the RHS `ref` and not at the `=`.
    RefReassignment,
    /// An AUTO-PROPERTY INITIALIZER -- `public int P { get; set; } = 5;`. Introduced in C# 6.0.
    ///
    /// The value initializes the property's BACKING FIELD directly, not through the setter, which
    /// is what lets a GETTER-ONLY auto-property have one at all (`int P { get; } = 5;` has no
    /// setter to call) and what keeps a virtual setter from being invoked before the derived
    /// constructor has run.
    ///
    ///
    /// **AN INITIALIZER ON A PROPERTY THAT IS NOT AUTOMATICALLY IMPLEMENTED IS `CS8050`, AND ON
    /// AN INTERFACE'S INSTANCE PROPERTY IT IS `CS8053`.** Both are rules about where an initializer
    /// may be WRITTEN rather than about this gate, so both are reported whatever the rung.
    AutoPropertyInitializer,
    /// A `ref` FIELD -- `public ref int f;`. Introduced in C# 11, and legal ONLY in a
    /// `ref struct`.
    ///
    /// **A SEPARATE FEATURE FROM [`Feature::ByRefLocalsAndReturns`] AND FOUR RUNGS LATER**, which
    /// matters because the member-type grammar is shared: the same `ref` that makes a method
    /// return by reference sits in front of a field declaration, so admitting one admits the
    /// spelling of the other. Measured: `public ref int f;` in a class draws `CS8107` naming
    /// *ref fields* and version 11.0, plus `CS9059` -- *A ref field can only be declared in a ref
    /// struct.* -- which is a second rule and not this gate.
    ///
    RefFields,
    /// An object initializer, `new C { F = 1 }`. Introduced in C# 3.0.
    ///
    ObjectInitializer,
    /// A collection initializer, `new ArrayList { 1, 2 }`. Introduced in C# 3.0.
    ///
    CollectionInitializer,
    /// An anonymous object creation `new { A = 1, ... }`, producing an instance of a
    /// compiler-synthesized anonymous type. Introduced in C# 3.0.
    AnonymousObjectCreation,
    /// An implicitly typed local variable -- `var x = 5;`, whose type is inferred from the
    /// initializer. Introduced in C# 3.0 (C# 3.0 spec, 8.5.1).
    ///
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
    /// The null-forgiving operator, `s!`. Introduced in **C# 8.0**.
    ///
    /// It suppresses a nullable-analysis warning about its operand and does nothing else: the
    /// operand's type, value and evaluation are unchanged, and it emits no IL. A build with no
    /// nullable analysis has nothing for it to suppress, so parsing it and discarding it is the
    /// whole implementation -- and refusing it would refuse ordinary modern C#.
    ///
    NullForgivingOperator,
    /// A nullable reference type annotation, `string?`, and the `#nullable` directive that sets
    /// where annotations and nullable warnings apply. Introduced in **C# 8.0**.
    ///
    /// **THE ANNOTATION IS NOT A TYPE.** `string?` is the type `string`, with a note that `null`
    /// is an expected value; it is not `System.Nullable<string>`, which is not a type at all
    /// (CS0453). So `T?` means two different things by what `T` turns out to be: over a value type
    /// it is `Nullable<T>` (C# 2.0), and over a reference type, or a type parameter not known to be
    /// a value type (C# 9.0), it is `T` itself. The difference is decided where `T` is resolved.
    NullableReferenceTypes,
    /// `T?` over a type parameter that is not known to be a value type or a non-nullable
    /// reference type, as an annotation. Introduced in **C# 9.0**, with `where T : default`, under
    /// the one feature csc names "default type parameter constraints"; at C# 8.0 it is CS8627.
    UnconstrainedTypeParameterAnnotation,
    /// The `notnull` constraint, `where T : notnull`: the argument must be a non-nullable type,
    /// value or reference. Introduced in **C# 8.0**. It writes no `GenericParamConstraint` row --
    /// csc records it only in the nullable metadata this compiler does not produce -- so what it
    /// constrains is checked by nullable analysis alone, which is not built either.
    NotNullConstraint,
    /// The `unmanaged` constraint, `where T : unmanaged`: the argument must be a non-nullable value
    /// type holding no reference at any depth of its fields (ECMA-334 7th ed, 15.2.5). Introduced
    /// in **C# 7.3**.
    UnmanagedConstraint,
    /// A constructed struct as an unmanaged type: `G<int>` over `struct G<T> { T F; }`, a struct
    /// nested in a generic type, or a struct holding one. Before **C# 8.0** only a struct that is
    /// not constructed was unmanaged.
    UnmanagedConstructedTypes,
    /// `System.Enum` as a constraint type, `where T : System.Enum`. Introduced in **C# 7.3**.
    EnumConstraint,
    /// `System.Delegate` or `System.MulticastDelegate` as a constraint type. Introduced in
    /// **C# 7.3**.
    DelegateConstraint,
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
    /// A `global using` directive -- `global using N;`, `global using static T;` or
    /// `global using A = T;` -- in scope in every file of the compilation. Introduced in C# 10.0.
    /// **What a .NET project's generated `GlobalUsings.g.cs` is made of**, so a program built the
    /// way the SDK builds one needs it before its first line binds.
    GlobalUsingDirective,
    /// A `required` member -- an initializer the caller MUST supply. Introduced in C# 11.0.
    ///
    /// **The one C# 11 feature both compatibility targets adopted**: 64 uses in dotnet/iot, 56 in
    /// nanoFramework, all of them in drivers and internals rather than on the public surface -- a
    /// census that reads the public surface alone reports zero. Unlike the two above it is a real
    /// feature rather than syntax -- it needs `RequiredMemberAttribute` and an
    /// initialization-safety rule.
    RequiredMembers,
    /// A digit separator IMMEDIATELY AFTER a base prefix -- `0x_FF`, `0b_1010`. Introduced in
    /// C# 7.2, a full release AFTER separators themselves.
    ///
    LeadingDigitSeparator,
    /// A record CLASS -- `record R(int X);` and `record R { ... }` -- with its generated value
    /// equality, `<Clone>$`, `ToString`/`PrintMembers` and `Deconstruct`. C# 9.0.
    ///
    ///
    ///
    /// **Ordered after generics**: a record is a code generator whose output binds
    /// `IEquatable<T>`, so it cannot land before the thing it generates against. 92 files and 93
    /// declarations in dotnet/iot, 11 in nanoFramework; 38 of the 92 are positional.
    Records,
    /// The `record class` / `record struct` KEYWORD forms -- C# 10, and a csc feature separate
    /// from [`Feature::Records`].
    ///
    ///
    RecordStructs,
    /// A record that INHERITS -- `record D(int Y) : B(X)` -- C# 9.0, and gated separately from
    /// [`Feature::Records`] because the members csc generates for it are not the ones it generates
    /// for a base record.
    ///
    ///
    RecordInheritance,
    /// An `init` accessor in place of a property or indexer `set` -- C# 9.0.
    ///
    InitOnlySetters,
    /// TARGET-TYPED OBJECT CREATION, `C c = new();` -- C# 9.0, and csc's own name for it is
    /// `'target-typed object creation'`, measured at 7.3 (CS8370) and 8.0 (CS8400).
    ///
    /// **THE FEATURE IS NOT THE SYNTAX, IT IS WHERE THE TYPE COMES FROM.** `new(args)` has no type
    /// of its own; it takes the one the CONTEXT is converting it to, exactly as a lambda takes its
    /// delegate. So it enters the language through [`Binder::bind_target_typed`] rather than
    /// through the general expression binder, and every position that supplies a target is a
    /// position this feature reaches.
    TargetTypedNew,
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
    ///
    AsyncFunction,
    /// The `default` LITERAL (C# 7.1): `default` with no type, which takes the type its context
    /// converts it to -- `x = default;`, `return default;`, `CancellationToken token = default`.
    /// `default(T)` is [`Feature::DefaultOperator`], six releases earlier.
    DefaultLiteral,
    /// An async entry point -- `static async Task Main()`. Introduced in **C# 7.1**, two rungs
    /// after async functions themselves. `static async void Main` is CS4009 at every version: an
    /// entry point can never be async-void.
    ///
    AsyncMain,
    /// An async method returning `Task<T>` -- one of the three return types 15.15.1 admits, and
    /// the one that is generic.
    ///
    AsyncTaskOfT,
    /// A GENERIC async method -- `async Task M<T>(...)`.
    ///
    AsyncGenericMethod,
    /// A lambda whose body reads `this` and nothing else from around it.
    ///
    ///
    LambdaCapturingThis,
    /// A lambda whose body reads an enclosing LOCAL or PARAMETER.
    ///
    ///
    ///
    /// **THIS VARIANT IS THE METHOD-BODY SCOPE ONLY**; a capture from anywhere else is
    /// [`Feature::LambdaCapturingNestedScope`]. The two are one construct to a reader and two
    /// lowerings to an emitter, which is the same split the three capture shapes already are.
    LambdaCapturingLocals,
    /// A lambda capturing a local declared somewhere OTHER than the enclosing method's own body
    /// scope: an inner block, a `for`/`foreach`/`using`/`catch`/`fixed` header, or an enclosing
    /// LAMBDA's parameter.
    ///
    /// **THE SECOND DISPLAY CLASS IS THE FEATURE, NOT THE SECOND SCOPE.** One capturing scope
    /// needs one class, allocated at the top of the body. A second needs a class per scope, each
    /// allocated where its scope OPENS, and every one but the outermost carries a
    /// `CS$<>8__locals{S}` field pointing at its parent -- because a body that reads an outer
    /// capture WALKS that chain (`ldarg.0; ldfld CS$<>8__locals1; ldfld a`) rather than holding a
    /// copy. Measured: copying would break the shared storage that makes a write through one path
    /// visible through the other.
    ///
    /// The suffix is not a depth: two SIBLING scopes under one captured outer local get `_1` and
    /// `_2`, so the classes are counted in the order scopes OPEN, over the whole method.
    LambdaCapturingNestedScope,
    /// A lambda written inside a GENERIC type or a generic method.
    ///
    ///
    /// **REFUSED MORE BROADLY THAN THE DEFECT STRICTLY REQUIRES.** A lambda in a generic type
    /// that touches no type parameter would lower correctly, and is refused anyway: the safe
    /// subset is a claim about every type the lambda's body can reach, and nothing here can check
    /// that claim.
    LambdaInGenericScope,
    /// A CALLER-INFO ATTRIBUTE on an optional parameter -- `[CallerMemberName]`,
    /// `[CallerFilePath]`, `[CallerLineNumber]` (**C# 5.0**).
    ///
    /// **ONE VARIANT FOR ALL THREE**, because they are one feature: the call site substitutes a
    /// constant for an omitted argument, and which constant it is does not change what has to be
    /// built.
    ///
    ///
    CallerInfoAttribute,
    /// The `nameof` operator -- `nameof(x)`, whose value is the FINAL IDENTIFIER of its operand
    /// as a compile-time constant string. Introduced in **C# 6.0**.
    ///
    ///
    NameOf,
    /// An INTERPOLATED STRING -- `$"a{b}c"` and its verbatim form `$@"a{b}c"`. Introduced in
    /// **C# 6.0**.
    ///
    ///
    ///
    InterpolatedStrings,
    /// An interpolated string used where a CONSTANT is required -- a `const` initializer, a
    /// `case` label, an attribute argument. Introduced in **C# 10.0**, four rungs after the
    /// interpolated string itself.
    ///
    ///
    ConstantInterpolatedStrings,
    /// An `await` inside a `catch` or `finally` block -- introduced in **C# 6.0**. Below 6 the
    /// refusal is csc's own CS1985/CS1984 (measured, and the two texts are asymmetric); at 6 and
    /// above csc compiles it (measured at both rungs), so from 6 up the refusal is the
    /// permitted-but-unbuilt half: resuming inside a handler needs exception spilling and
    /// pending-fault rethrow, which this build does not implement.
    AwaitInCatchOrFinally,
    /// An IMPLICITLY TYPED ARRAY, `new[] { 1, 2, 3 }`, whose element type is the best common type
    /// of its elements. Introduced in **C# 3.0**.
    ///
    ImplicitlyTypedArray,
    /// An EXTENSION METHOD -- a static method whose first parameter carries `this`, so that
    /// `3.Twice()` binds to `E.Twice(3)`. Introduced in **C# 3.0**, and csc's gate is at the
    /// `this`.
    ExtensionMethod,
    /// An ITERATOR -- a method body containing `yield return` or `yield break`, which the compiler
    /// rewrites into an enumerator class. Introduced in **C# 2.0**.
    ///
    Iterators,
    /// A LOCAL FUNCTION -- a method declared inside a block, `int Sq(int a) { return a * a; }`.
    /// Introduced in **C# 7.0**, and csc's gate is at the function's NAME rather than at its
    /// return type.
    LocalFunctions,
    /// A `static` local function, which may not capture. Introduced in **C# 8.0**, one rung after
    /// the local function itself; csc's gate is at the `static`.
    StaticLocalFunctions,
    /// A PATTERN in a `switch` statement's `case` label -- `case int n:`, or any label with a
    /// `when` clause. Introduced in **C# 7.0**.
    ///
    PatternCaseLabel,
    /// A TYPE PATTERN -- a type with no designation, `o switch { int => .., _ => .. }`.
    /// Introduced in **C# 9.0**, where it is csc's `type pattern`, reported at the type.
    ///
    /// Only a switch arm can spell one: after `is`, a bare type is the C# 1.0 type test and stays
    /// that.
    TypePattern,
    /// A `with` EXPRESSION, `p with { Y = 5 }`, which copies a record and then assigns the named
    /// members. Introduced in **C# 9.0**.
    ///
    WithExpression,
    /// The `record class` spelling of a record. Introduced in **C# 10.0**, where csc gates it --
    /// and `record struct` beside it -- as `record structs`, the class form included.
    ///
    /// **A SEPARATE VARIANT FROM [`Feature::RecordStructs`] BECAUSE ONLY THE STRUCT IS UNBUILT.**
    /// `record class R` is `record R` written out in full: the same declaration, the same members.
    RecordClass,
    /// A LAMBDA written as a method or constructor ARGUMENT, `Run(() => Work())`, where it
    /// converts to the delegate type of the parameter overload resolution chooses. Introduced
    /// with the lambda, in **C# 3.0**.
    ///
    /// **THE LAMBDA ITSELF IS [`Feature::LambdaExpression`]; THIS IS THE POSITION.** A lambda has
    /// no type until it has a target, and in an argument position the target is a parameter that
    /// only overload resolution can name. The positions whose target is known without resolution
    /// -- a local's declared type, `new D(lambda)`, `e += lambda` on an event or a delegate -- are
    /// the lambda feature and bind through it.
    LambdaArgument,
    /// A member reached through a type parameter's INTERFACE constraint -- `v.Area()` under
    /// `where T : IShape`, which binds to `IShape.Area` and calls it with the `constrained.` prefix
    /// so a struct argument is not boxed. Part of generics, **C# 2.0**.
    ///
    /// Distinct from a member of the constraint's effective BASE CLASS, which binds: an interface
    /// constraint is a second root for member lookup, beside the base class, rather than a
    /// replacement for it.
    InterfaceConstraintMember,
    /// A GENERIC local function -- one declaring type parameters of its own, `T Id<T>(T x)`. Part
    /// of local functions, **C# 7.0**, and refused by name until its lowering, a generic
    /// synthesized method, is built.
    GenericLocalFunction,
    /// A local function written inside a generic type or method, whose lowered method and display
    /// struct would carry the enclosing type parameters. **C# 7.0**.
    LocalFunctionInGenericScope,
    /// An `async` local function, whose state machine is named for the function. **C# 7.0**.
    AsyncLocalFunction,
    /// An `extern` local function, `static extern int F();`, whose body P/Invoke supplies.
    /// Introduced in **C# 9.0**, with the attributes it needs.
    ExternLocalFunction,
    /// An ATTRIBUTE on a local function or one of its parameters, `[Conditional("DEBUG")] void
    /// Log() { }`, which csc writes on the synthesized method. **C# 9.0**; csc gates it at the
    /// `[`.
    LocalFunctionAttributes,
    /// A local function returning `ref readonly`, whose return csc marks with
    /// `modreq(InAttribute)` and `[IsReadOnlyAttribute]`. **C# 7.2**.
    ReadOnlyRefLocalFunction,
    /// An ATTRIBUTE on a type parameter of a type, method or delegate, `class C<[A] T>`, which csc
    /// writes on the `GenericParam` row. Part of generics, **C# 2.0**.
    TypeParameterAttributes,
    /// A local function converted to a delegate -- or reached from a lambda -- that captures a
    /// local of a NESTED scope, itself or through a function it calls. Its captures need a display
    /// class in that scope and a chain to the enclosing one: lambdas' unbuilt shape
    /// ([`Feature::LambdaCapturingNestedScope`]), reached another way. **C# 7.0**.
    LocalFunctionDelegateNestedCapture,
}

impl Feature {
    /// Whether **this build implements** this feature, as distinct from whether a language
    /// version PERMITS it ([`Self::introduced_in`]).
    ///
    /// **The two questions are independent and conflating them is the trap this method exists to
    /// prevent.** A selectable `/langversion:7` must not be read as "lcsc implements C# 7": it
    /// selects a DIALECT, and this table says which of that dialect's features this build can
    /// actually compile. A feature the dialect permits and this build does not implement has to be
    /// refused by NAME --
    /// telling the user to "use language version 7 or greater" when they already did would be a
    /// lie, and one that sends them looking for a compiler switch that cannot help.
    ///
    /// `static class` is the shape to keep in mind: its emit path is complete (see the
    /// `GATED FEATURE (ISO-2)` markers in `compile.rs` and `declaration.rs`) and it is refused
    /// anyway, because ISO-1 is the dialect. Implemented and permitted are two bits, and only both
    /// together admit a construct.
    #[must_use]
    pub const fn is_implemented(self) -> bool {
        match self {
            Feature::StaticClasses | Feature::BinaryLiterals | Feature::DigitSeparators => true,
            Feature::FileScopedNamespaces => true,
            Feature::GlobalUsingDirective => true,
            Feature::ObjectInitializer | Feature::CollectionInitializer => true,
            Feature::ImplicitlyTypedLocalVariable => true,
            Feature::Generics => true,
            Feature::Records => true,

            Feature::DefaultOperator => true,
            Feature::NullCoalescing => true,
            Feature::PragmaDirective | Feature::AutoProperties => true,
            Feature::AccessorAccessibility => true,
            Feature::NullableValueTypes => true,
            Feature::ExpressionBodiedMethod
            | Feature::ExpressionBodiedProperty
            | Feature::ExpressionBodiedIndexer
            | Feature::ExpressionBodiedAccessor
            | Feature::ExpressionBodiedConstructor
            | Feature::ConstantPattern
            | Feature::OutVariableDeclaration
            | Feature::DeclarationPattern
            | Feature::Tuples
            | Feature::Discards
            | Feature::UsingDeclaration
            | Feature::SwitchExpression
            | Feature::ThrowExpression
            | Feature::RefStruct
            | Feature::StackAllocNestedExpressions
            | Feature::StackAllocInitializer => true,
            Feature::RequiredMembers | Feature::LeadingDigitSeparator => true,
            Feature::NullConditional => true,
            Feature::NullForgivingOperator => true,
            Feature::NullableReferenceTypes => true,
            Feature::UnconstrainedTypeParameterAnnotation => true,
            Feature::NotNullConstraint => true,
            Feature::UnmanagedConstraint
            | Feature::UnmanagedConstructedTypes
            | Feature::EnumConstraint
            | Feature::DelegateConstraint => true,
            Feature::UsingStatic => true,
            Feature::DefaultParameterValues => true,
            Feature::AutoPropertyInitializer | Feature::ReadonlyAutoProperty => true,
            Feature::ExceptionFilter => true,
            Feature::LambdaExpression => true,
            Feature::NamedArguments => true,
            Feature::NamespaceAlias => true,
            Feature::TopLevelStatements => true,
            Feature::AnonymousMethods
            | Feature::AnonymousObjectCreation
            | Feature::SwitchOnBool
            | Feature::RecordStructs
            | Feature::RecordInheritance
            | Feature::DefaultInterfaceImplementation
            | Feature::ParameterlessStructConstructor
            | Feature::RefFields => false,
            Feature::ReadOnlyReferences => true,
            Feature::ByRefLocalsAndReturns => true,
            Feature::RefReassignment => true,
            Feature::InitOnlySetters => true,
            Feature::TargetTypedNew => true,
            Feature::AsyncFunction => true,
            Feature::PartialTypes => true,
            Feature::AsyncMain => false,
            Feature::DefaultLiteral => true,
            Feature::AsyncTaskOfT => true,
            Feature::AsyncGenericMethod
            | Feature::AwaitInCatchOrFinally
            | Feature::CallerInfoAttribute => false,
            Feature::LambdaCapturingThis => true,
            Feature::LambdaCapturingLocals => true,
            Feature::LambdaCapturingNestedScope | Feature::LambdaInGenericScope => false,
            Feature::NameOf => true,
            Feature::InterpolatedStrings => true,
            Feature::ConstantInterpolatedStrings => true,
            Feature::RecordClass => true,
            Feature::PatternCaseLabel => true,
            Feature::LocalFunctions | Feature::StaticLocalFunctions => true,
            Feature::ImplicitlyTypedArray
            | Feature::ExtensionMethod
            | Feature::Iterators
            | Feature::TypePattern
            | Feature::WithExpression
            | Feature::LambdaArgument
            | Feature::InterfaceConstraintMember
            | Feature::GenericLocalFunction
            | Feature::LocalFunctionInGenericScope
            | Feature::AsyncLocalFunction
            | Feature::ExternLocalFunction
            | Feature::LocalFunctionAttributes
            | Feature::ReadOnlyRefLocalFunction
            | Feature::TypeParameterAttributes
            | Feature::LocalFunctionDelegateNestedCapture => false,
        }
    }

    /// Every feature this compiler gates on, so a check over "all of them" is DERIVED rather than
    /// transcribed.
    ///
    ///
    /// **A HAND-WRITTEN LIST OF FEATURES IS A LIST OF THE FEATURES SOMEBODY REMEMBERED.** Rung
    /// tests naming their own table drift silently: features such as `Records`, `RequiredMembers`
    /// and `TopLevelStatements` end up asserted about by nothing at all, while the gate stays green.
    /// **A gate whose population is typed out by hand tests the day it was written.**
    ///
    /// Two things keep this honest and they are both compiler-enforced, not remembered: the
    /// exhaustive `match` in `every_feature_is_in_all` fails to compile when a variant is added,
    /// and the length assertion beside it fails until the variant is added HERE too.
    pub const ALL: [Feature; 99] = [
        Feature::Generics,
        Feature::StaticClasses,
        Feature::AnonymousMethods,
        Feature::NullableValueTypes,
        Feature::DefaultOperator,
        Feature::NullCoalescing,
        Feature::PragmaDirective,
        Feature::ReadonlyAutoProperty,
        Feature::AutoPropertyInitializer,
        Feature::ExceptionFilter,
        Feature::NamespaceAlias,
        Feature::AccessorAccessibility,
        Feature::LambdaExpression,
        Feature::ExpressionBodiedMethod,
        Feature::ExpressionBodiedProperty,
        Feature::ExpressionBodiedIndexer,
        Feature::ExpressionBodiedAccessor,
        Feature::ThrowExpression,
        Feature::RefStruct,
        Feature::StackAllocNestedExpressions,
        Feature::StackAllocInitializer,
        Feature::ByRefLocalsAndReturns,
        Feature::RefReassignment,
        Feature::ReadOnlyReferences,
        Feature::RefFields,
        Feature::ObjectInitializer,
        Feature::CollectionInitializer,
        Feature::AnonymousObjectCreation,
        Feature::ImplicitlyTypedLocalVariable,
        Feature::DefaultParameterValues,
        Feature::NamedArguments,
        Feature::NullConditional,
        Feature::NullForgivingOperator,
        Feature::NullableReferenceTypes,
        Feature::UnconstrainedTypeParameterAnnotation,
        Feature::NotNullConstraint,
        Feature::UnmanagedConstraint,
        Feature::UnmanagedConstructedTypes,
        Feature::EnumConstraint,
        Feature::DelegateConstraint,
        Feature::ExpressionBodiedConstructor,
        Feature::ConstantPattern,
        Feature::OutVariableDeclaration,
        Feature::Tuples,
        Feature::Discards,
        Feature::DeclarationPattern,
        Feature::SwitchExpression,
        Feature::UsingDeclaration,
        Feature::UsingStatic,
        Feature::AutoProperties,
        Feature::SwitchOnBool,
        Feature::BinaryLiterals,
        Feature::DigitSeparators,
        Feature::TopLevelStatements,
        Feature::FileScopedNamespaces,
        Feature::GlobalUsingDirective,
        Feature::RequiredMembers,
        Feature::LeadingDigitSeparator,
        Feature::Records,
        Feature::RecordStructs,
        Feature::RecordInheritance,
        Feature::InitOnlySetters,
        Feature::TargetTypedNew,
        Feature::DefaultInterfaceImplementation,
        Feature::ParameterlessStructConstructor,
        Feature::AsyncFunction,
        Feature::AsyncMain,
        Feature::DefaultLiteral,
        Feature::AsyncTaskOfT,
        Feature::AsyncGenericMethod,
        Feature::LambdaCapturingThis,
        Feature::LambdaCapturingLocals,
        Feature::LambdaCapturingNestedScope,
        Feature::LambdaInGenericScope,
        Feature::AwaitInCatchOrFinally,
        Feature::PartialTypes,
        Feature::CallerInfoAttribute,
        Feature::NameOf,
        Feature::InterpolatedStrings,
        Feature::ConstantInterpolatedStrings,
        Feature::ImplicitlyTypedArray,
        Feature::ExtensionMethod,
        Feature::Iterators,
        Feature::LocalFunctions,
        Feature::StaticLocalFunctions,
        Feature::PatternCaseLabel,
        Feature::TypePattern,
        Feature::WithExpression,
        Feature::RecordClass,
        Feature::LambdaArgument,
        Feature::InterfaceConstraintMember,
        Feature::GenericLocalFunction,
        Feature::LocalFunctionInGenericScope,
        Feature::AsyncLocalFunction,
        Feature::ExternLocalFunction,
        Feature::LocalFunctionAttributes,
        Feature::ReadOnlyRefLocalFunction,
        Feature::TypeParameterAttributes,
        Feature::LocalFunctionDelegateNestedCapture,
    ];

    /// The first language version in which this feature is available.
    #[must_use]
    pub const fn introduced_in(self) -> LanguageVersion {
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
            Feature::ExpressionBodiedMethod
            | Feature::ExpressionBodiedProperty
            | Feature::ExpressionBodiedIndexer => LanguageVersion::CSharp6,
            Feature::ExpressionBodiedAccessor => LanguageVersion::CSharp7,
            Feature::ThrowExpression => LanguageVersion::CSharp7,
            Feature::RefStruct => LanguageVersion::CSharp7_2,
            Feature::StackAllocNestedExpressions => LanguageVersion::CSharp8,
            Feature::StackAllocInitializer => LanguageVersion::CSharp7_3,
            Feature::AutoPropertyInitializer => LanguageVersion::CSharp6,
            Feature::ByRefLocalsAndReturns => LanguageVersion::CSharp7,
            Feature::RefReassignment => LanguageVersion::CSharp7_3,
            Feature::UnmanagedConstraint
            | Feature::EnumConstraint
            | Feature::DelegateConstraint => LanguageVersion::CSharp7_3,
            Feature::UnmanagedConstructedTypes => LanguageVersion::CSharp8,
            Feature::ReadOnlyReferences => LanguageVersion::CSharp7_2,
            Feature::RefFields => LanguageVersion::CSharp11,
            Feature::ExpressionBodiedConstructor => LanguageVersion::CSharp7,
            Feature::ConstantPattern
            | Feature::OutVariableDeclaration
            | Feature::Tuples
            | Feature::Discards
            | Feature::DeclarationPattern => LanguageVersion::CSharp7,
            Feature::NullForgivingOperator
            | Feature::NullableReferenceTypes
            | Feature::NotNullConstraint
            | Feature::UsingDeclaration
            | Feature::SwitchExpression => LanguageVersion::CSharp8,
            Feature::NullConditional
            | Feature::UsingStatic
            | Feature::NameOf
            | Feature::InterpolatedStrings
            | Feature::ExceptionFilter
            | Feature::ReadonlyAutoProperty => LanguageVersion::CSharp6,
            Feature::ConstantInterpolatedStrings => LanguageVersion::CSharp10,
            Feature::BinaryLiterals | Feature::DigitSeparators => LanguageVersion::CSharp7,
            Feature::LeadingDigitSeparator => LanguageVersion::CSharp7_2,
            Feature::TopLevelStatements | Feature::Records => LanguageVersion::CSharp9,
            Feature::UnconstrainedTypeParameterAnnotation => LanguageVersion::CSharp9,
            Feature::RecordStructs => LanguageVersion::CSharp10,
            Feature::RecordInheritance => LanguageVersion::CSharp9,
            Feature::InitOnlySetters => LanguageVersion::CSharp9,
            Feature::TargetTypedNew => LanguageVersion::CSharp9,
            Feature::FileScopedNamespaces => LanguageVersion::CSharp10,
            Feature::GlobalUsingDirective => LanguageVersion::CSharp10,
            Feature::RequiredMembers => LanguageVersion::CSharp11,
            Feature::DefaultInterfaceImplementation => LanguageVersion::CSharp8,
            Feature::ParameterlessStructConstructor => LanguageVersion::CSharp10,
            Feature::AsyncFunction => LanguageVersion::CSharp5,
            Feature::AsyncMain | Feature::DefaultLiteral => LanguageVersion::CSharp7_1,
            Feature::AsyncTaskOfT | Feature::AsyncGenericMethod => LanguageVersion::CSharp5,
            Feature::LambdaCapturingThis
            | Feature::LambdaCapturingLocals
            | Feature::LambdaCapturingNestedScope
            | Feature::LambdaInGenericScope => LanguageVersion::CSharp3,
            Feature::CallerInfoAttribute => LanguageVersion::CSharp5,
            Feature::AwaitInCatchOrFinally => LanguageVersion::CSharp6,
            Feature::Iterators
            | Feature::InterfaceConstraintMember
            | Feature::TypeParameterAttributes => LanguageVersion::CSharp2,
            Feature::ImplicitlyTypedArray
            | Feature::ExtensionMethod
            | Feature::LambdaArgument => LanguageVersion::CSharp3,
            Feature::LocalFunctions
            | Feature::PatternCaseLabel
            | Feature::GenericLocalFunction
            | Feature::LocalFunctionInGenericScope
            | Feature::AsyncLocalFunction
            | Feature::LocalFunctionDelegateNestedCapture => LanguageVersion::CSharp7,
            Feature::ExternLocalFunction | Feature::LocalFunctionAttributes => LanguageVersion::CSharp9,
            Feature::ReadOnlyRefLocalFunction => LanguageVersion::CSharp7_2,
            Feature::StaticLocalFunctions => LanguageVersion::CSharp8,
            Feature::TypePattern | Feature::WithExpression => LanguageVersion::CSharp9,
            Feature::RecordClass => LanguageVersion::CSharp10,
        }
    }

    /// The noun phrase csc quotes in `Feature '<name>' is not available in C# N`.
    ///
    ///
    ///
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
            Feature::ExceptionFilter => "exception filter",
            Feature::PragmaDirective => "#pragma",
            Feature::NamespaceAlias => "namespace alias qualifier",
            Feature::AccessorAccessibility => "access modifiers on properties",
            Feature::LambdaExpression => "lambda expression",
            Feature::ExpressionBodiedMethod => "expression-bodied method",
            Feature::ExpressionBodiedProperty => "expression-bodied property",
            Feature::ExpressionBodiedIndexer => "expression-bodied indexer",
            Feature::ExpressionBodiedAccessor => "expression body property accessor",
            Feature::ExpressionBodiedConstructor => "expression body constructor and destructor",
            Feature::ConstantPattern => "pattern matching",
            Feature::OutVariableDeclaration => "out variable declaration",
            Feature::Tuples => "tuples",
            Feature::Discards => "discards",
            Feature::DeclarationPattern => "pattern matching",
            Feature::ThrowExpression => "throw expression",
            Feature::RefStruct => "ref structs",
            Feature::StackAllocNestedExpressions => "stackalloc in nested expressions",
            Feature::StackAllocInitializer => "stackalloc initializer",
            Feature::UsingDeclaration => "using declarations",
            Feature::SwitchExpression => "recursive patterns",
            Feature::AutoPropertyInitializer => "auto property initializer",
            Feature::ByRefLocalsAndReturns => "byref locals and returns",
            Feature::RefReassignment => "ref reassignment",
            Feature::ReadOnlyReferences => "readonly references",
            Feature::RefFields => "ref fields",
            Feature::ObjectInitializer => "object initializer",
            Feature::CollectionInitializer => "collection initializer",
            Feature::AnonymousObjectCreation => "anonymous types",
            Feature::ImplicitlyTypedLocalVariable => "implicitly typed local variable",
            Feature::DefaultParameterValues => "optional parameter",
            Feature::NamedArguments => "named argument",
            Feature::NullConditional => "null propagating operator",
            Feature::NullForgivingOperator | Feature::NullableReferenceTypes => {
                "nullable reference types"
            }
            Feature::StaticClasses => "static classes",
            Feature::UsingStatic => "using static",
            Feature::AutoProperties => "automatically implemented properties",
            Feature::SwitchOnBool => "switch on boolean type",
            Feature::BinaryLiterals => "binary literals",
            Feature::DigitSeparators => "digit separators",
            Feature::LeadingDigitSeparator => "leading digit separator",
            Feature::TopLevelStatements => "top-level statements",
            Feature::UnconstrainedTypeParameterAnnotation => "default type parameter constraints",
            Feature::NotNullConstraint => "notnull generic type constraint",
            Feature::UnmanagedConstraint => "unmanaged generic type constraints",
            Feature::UnmanagedConstructedTypes => "unmanaged constructed types",
            Feature::EnumConstraint => "enum generic type constraints",
            Feature::DelegateConstraint => "delegate generic type constraints",
            Feature::FileScopedNamespaces => "file-scoped namespace",
            Feature::GlobalUsingDirective => "global using directive",
            Feature::RequiredMembers => "required members",
            Feature::Records => "records",
            Feature::RecordStructs => "record structs",
            Feature::RecordInheritance => "records",
            Feature::InitOnlySetters => "init-only setters",
            Feature::TargetTypedNew => "target-typed object creation",
            Feature::DefaultInterfaceImplementation => "default interface implementation",
            Feature::ParameterlessStructConstructor => "parameterless struct constructors",
            Feature::AsyncFunction => "async function",
            Feature::PartialTypes => "partial types",
            Feature::AsyncMain => "async main",
            Feature::DefaultLiteral => "default literal",
            Feature::LambdaCapturingThis => "lambda capturing the enclosing instance",
            Feature::LambdaCapturingLocals => "lambda capturing a local variable",
            Feature::LambdaCapturingNestedScope => {
                "lambda capturing a local from a nested scope"
            }
            Feature::LambdaInGenericScope => "lambda in a generic type or method",
            Feature::AsyncTaskOfT => "async method returning Task<T>",
            Feature::AsyncGenericMethod => "generic async method",
            Feature::CallerInfoAttribute => "caller information attribute",
            Feature::AwaitInCatchOrFinally => "await in a catch or finally clause",
            Feature::NameOf => "nameof operator",
            Feature::InterpolatedStrings => "interpolated strings",
            Feature::ConstantInterpolatedStrings => "constant interpolated strings",
            Feature::ImplicitlyTypedArray => "implicitly typed array",
            Feature::ExtensionMethod => "extension method",
            Feature::Iterators => "iterators",
            Feature::LocalFunctions => "local functions",
            Feature::StaticLocalFunctions => "static local functions",
            Feature::PatternCaseLabel => "pattern matching",
            Feature::TypePattern => "type pattern",
            Feature::WithExpression => "records",
            Feature::RecordClass => "record structs",
            Feature::LambdaArgument => "lambda expression",
            Feature::InterfaceConstraintMember => "generics",
            Feature::TypeParameterAttributes => "attributes on type parameters",
            Feature::GenericLocalFunction => "generic local function",
            Feature::LocalFunctionInGenericScope => "local function in a generic type or method",
            Feature::AsyncLocalFunction => "async local function",
            Feature::ExternLocalFunction => "extern local function",
            Feature::LocalFunctionAttributes => "local function attributes",
            Feature::ReadOnlyRefLocalFunction => "local function returning ref readonly",
            Feature::LocalFunctionDelegateNestedCapture => {
                "local function converted to a delegate that captures a local from a nested scope"
            }
        }
    }

    /// The noun `LAM0001` quotes when the dialect permits this feature and this build lacks it.
    ///
    ///
    #[must_use]
    pub fn refusal_description(self) -> &'static str {
        match self {
            Feature::WithExpression => "with expression",
            Feature::PatternCaseLabel => "patterns in case labels",
            Feature::RecordInheritance => "records with a base list",
            Feature::LambdaArgument => "lambda expression as an argument",
            Feature::InterfaceConstraintMember => "member access through an interface constraint",
            _ => self.description(),
        }
    }

    /// What to write instead, where this build offers an equivalent, as a sentence `LAM0001`
    /// appends to its refusal. `None` where there is nothing to point to.
    ///
    /// **ONLY A REPLACEMENT THIS BUILD COMPILES IS NAMED.** A suggestion that also fails would cost
    /// the reader a second round trip, and advice that is merely idiomatic C# is not the point.
    #[must_use]
    pub fn instead(self) -> Option<&'static str> {
        match self {
            Feature::AnonymousMethods => Some("Write it as a lambda expression instead."),
            Feature::ImplicitlyTypedArray => {
                Some("Name the element type instead, as in 'new int[] { 1, 2 }'.")
            }
            Feature::ExtensionMethod => {
                Some("Declare it without 'this' and call it as a static method instead.")
            }
            Feature::Iterators => Some("Build and return a collection instead."),
            Feature::LocalFunctions
            | Feature::StaticLocalFunctions
            | Feature::GenericLocalFunction
            | Feature::LocalFunctionInGenericScope
            | Feature::AsyncLocalFunction
            | Feature::ExternLocalFunction
            | Feature::LocalFunctionAttributes
            | Feature::ReadOnlyRefLocalFunction => {
                Some("Declare it as a method of the enclosing type instead.")
            }
            Feature::LocalFunctionDelegateNestedCapture => Some(
                "Declare the variables it captures in the method's outermost block instead.",
            ),
            Feature::PatternCaseLabel => {
                Some("Use a switch expression, or an 'if' with an 'is' pattern, instead.")
            }
            Feature::RecordStructs => Some("Declare a 'record' or an ordinary 'struct' instead."),
            Feature::LambdaArgument => Some(
                "Assign it to a local of the parameter's delegate type and pass that instead.",
            ),
            _ => None,
        }
    }

    /// Whether `version` admits this feature, and if not, WHICH of the two bits refused it.
    ///
    /// **THE TWO-BIT RULE, STATED ONCE.** A construct needs its dialect to PERMIT it
    /// ([`Self::introduced_in`]) and this build to IMPLEMENT it ([`Self::is_implemented`]), and
    /// the two failures want different diagnostics because they want different actions from the
    /// reader: raising `/langversion` fixes the first and cannot touch the second.
    ///
    ///
    #[must_use]
    pub fn gate_against(self, version: LanguageVersion) -> Option<FeatureGate> {
        if !version.supports(self) {
            Some(FeatureGate::RequiresLaterVersion {
                feature: self.description(),
                required: self.introduced_in().required_name(),
            })
        } else if !self.is_implemented() {
            Some(FeatureGate::NotInThisBuild {
                feature: self.refusal_description(),
                instead: self.instead(),
            })
        } else {
            None
        }
    }
}

/// Why a [`Feature`] is refused, from [`Feature::gate_against`].
///
/// The two variants are the two bits, and they are disjoint by construction: a dialect that does
/// not permit a construct never reaches the question of whether it was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureGate {
    /// The dialect being compiled predates the feature. csc's `CS8022` family; the message names
    /// the version that would work, and moving `/langversion` up fixes it.
    RequiresLaterVersion {
        /// csc's noun for the feature, [`Feature::description`].
        feature: &'static str,
        /// The version that introduced it, as csc renders a REQUIRED version -- `"2"`, `"7.0"`.
        required: &'static str,
    },
    /// The dialect permits it and this build cannot produce it. `LAM0001`, and **moving
    /// `/langversion` up does NOT fix it** -- which is exactly why it must not borrow the other
    /// message.
    NotInThisBuild {
        /// The construct being refused, [`Feature::refusal_description`].
        feature: &'static str,
        /// What to write instead, [`Feature::instead`].
        instead: Option<&'static str>,
    },
}

/// The reason a `/langversion` value could not be turned into a
/// [`LanguageVersion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageVersionError {
    /// The value names a real C# version that this compiler does not implement,
    /// for example `ISO-2` while only C# 1.0 is supported.
    Unsupported,
    /// The value names no known C# version.
    Invalid,
}

impl fmt::Display for LanguageVersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LanguageVersionError::Unsupported => {
                f.write_str("that C# version is not supported by this compiler")
            }
            LanguageVersionError::Invalid => f.write_str("unrecognized C# language version"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_is_the_lowest_rung_that_admits_everything_built() {
        let expected = Feature::ALL
            .into_iter()
            .filter(|feature| feature.is_implemented())
            .map(Feature::introduced_in)
            .max()
            .expect("some feature is implemented");
        assert_eq!(LanguageVersion::DEFAULT, expected);
        assert!(LanguageVersion::DEFAULT.is_selectable());
        assert_eq!(LanguageVersion::SELECTABLE_MAX, LanguageVersion::CSharp11);
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

        let permitted_but_unbuilt = Feature::ALL
            .into_iter()
            .filter(|feature| LanguageVersion::DEFAULT.supports(*feature) && !feature.is_implemented())
            .count();
        assert!(
            permitted_but_unbuilt > 0,
            "no feature is permitted-but-unbuilt at the default rung, so the two bits have stopped \
             being distinguishable here and this test proves nothing"
        );
        for feature in Feature::ALL {
            if !feature.is_implemented() {
                continue;
            }
            assert!(
                LanguageVersion::DEFAULT.supports(feature),
                "{feature:?} is implemented and the default dialect refuses it"
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
                | Feature::ExpressionBodiedIndexer
                | Feature::ExpressionBodiedAccessor
                | Feature::ThrowExpression
                | Feature::RefStruct
                | Feature::StackAllocNestedExpressions
                | Feature::StackAllocInitializer
                | Feature::ByRefLocalsAndReturns
                | Feature::RefReassignment
                | Feature::AutoPropertyInitializer
                | Feature::ExceptionFilter
                | Feature::ReadOnlyReferences
                | Feature::RefFields
                | Feature::ObjectInitializer
                | Feature::CollectionInitializer
                | Feature::AnonymousObjectCreation
                | Feature::ImplicitlyTypedLocalVariable
                | Feature::DefaultParameterValues
                | Feature::NamedArguments
                | Feature::NullConditional
                | Feature::NullForgivingOperator
                | Feature::NullableReferenceTypes
                | Feature::UnconstrainedTypeParameterAnnotation
                | Feature::NotNullConstraint
                | Feature::UnmanagedConstraint
                | Feature::UnmanagedConstructedTypes
                | Feature::EnumConstraint
                | Feature::DelegateConstraint
                | Feature::ExpressionBodiedConstructor
                | Feature::ConstantPattern
                | Feature::OutVariableDeclaration
                | Feature::Tuples
                | Feature::Discards
                | Feature::DeclarationPattern
                | Feature::SwitchExpression
                | Feature::UsingDeclaration
                | Feature::UsingStatic
                | Feature::AutoProperties
                | Feature::SwitchOnBool
                | Feature::BinaryLiterals
                | Feature::DigitSeparators
                | Feature::TopLevelStatements
                | Feature::FileScopedNamespaces
                | Feature::GlobalUsingDirective
                | Feature::RequiredMembers
                | Feature::LeadingDigitSeparator
                | Feature::Records
                | Feature::RecordStructs
                | Feature::RecordInheritance
                | Feature::InitOnlySetters
                | Feature::TargetTypedNew
                | Feature::DefaultInterfaceImplementation
                | Feature::ParameterlessStructConstructor
                | Feature::AsyncFunction
                | Feature::AsyncMain
                | Feature::DefaultLiteral
                | Feature::AsyncTaskOfT
                | Feature::AsyncGenericMethod
                | Feature::LambdaCapturingThis
                | Feature::LambdaCapturingLocals
                | Feature::LambdaCapturingNestedScope
                | Feature::LambdaInGenericScope
                | Feature::AwaitInCatchOrFinally
                | Feature::CallerInfoAttribute
                | Feature::NameOf
                | Feature::InterpolatedStrings
                | Feature::ConstantInterpolatedStrings
                | Feature::PartialTypes
                | Feature::ImplicitlyTypedArray
                | Feature::ExtensionMethod
                | Feature::Iterators
                | Feature::LocalFunctions
                | Feature::StaticLocalFunctions
                | Feature::PatternCaseLabel
                | Feature::TypePattern
                | Feature::WithExpression
                | Feature::RecordClass
                | Feature::LambdaArgument
                | Feature::InterfaceConstraintMember
                | Feature::GenericLocalFunction
                | Feature::LocalFunctionInGenericScope
                | Feature::AsyncLocalFunction
                | Feature::ExternLocalFunction
                | Feature::LocalFunctionAttributes
                | Feature::ReadOnlyRefLocalFunction
                | Feature::TypeParameterAttributes
                | Feature::LocalFunctionDelegateNestedCapture => {}
            }
        }
        assert_eq!(
            Feature::ALL.len(),
            99,
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
    fn the_default_rung_admits_every_feature_this_build_implements() {
        for feature in Feature::ALL {
            if !feature.is_implemented() {
                continue;
            }
            assert!(
                LanguageVersion::DEFAULT.supports(feature),
                "{feature:?} is implemented but the default dialect does not permit it, so an \
                 unflagged compilation refuses a construct this build can produce"
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
        for value in ["ISO-1", "iso-1", "1", "1.0", " 1 "] {
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
        assert_eq!(
            LanguageVersion::parse_flag("default"),
            Ok(LanguageVersion::DEFAULT),
            "`default` is the derived answer -- what this build implements"
        );
        assert_eq!(
            LanguageVersion::parse_flag("latest"),
            Ok(LanguageVersion::SELECTABLE_MAX),
            "`latest` is the capability answer -- the newest dialect we can gate against"
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

    /// A FEATURE THE CEILING PERMITS AND THIS BUILD LACKS MUST NEVER BE TOLD TO RAISE ITS
    /// LANGUAGE VERSION -- there is no higher version to raise it to.
    ///
    /// **RED-PROOF: revert `gate_against` to asking `supports` alone and this fails**, because
    /// every unbuilt feature at the ceiling comes back `None` -- silently admitted -- and the
    /// `NotInThisBuild` arm is never reached. That is the state this test was written against:
    /// six of the seventy-eight `feature-matrix` probes answered *"not available in C# 11.0.
    /// Please use language version 4 or greater"* at `/langversion:11`.
    ///
    /// Derived over [`Feature::ALL`] rather than over a list, so a feature added tomorrow is
    /// covered without anyone remembering to add it here.
    #[test]
    fn a_permitted_but_unbuilt_feature_is_never_told_to_raise_its_language_version() {
        let ceiling = LanguageVersion::SELECTABLE_MAX;
        let mut unbuilt = 0;
        for feature in Feature::ALL {
            match feature.gate_against(ceiling) {
                None => assert!(
                    feature.is_implemented(),
                    "{} is admitted at the ceiling while is_implemented() says it is not built",
                    feature.description()
                ),
                Some(FeatureGate::NotInThisBuild { .. }) => {
                    unbuilt += 1;
                    assert!(ceiling.supports(feature));
                    assert!(!feature.is_implemented());
                }
                Some(FeatureGate::RequiresLaterVersion { required, .. }) => assert!(
                    !ceiling.supports(feature),
                    "{} asks for language version {required} at a compilation that is already at                      the ceiling -- advice the reader has already taken",
                    feature.description()
                ),
            }
        }
        assert!(unbuilt > 0, "no unbuilt feature left for this test to be about");
    }

    /// The two bits are asked in the order that makes the message right: a dialect that does not
    /// permit a construct never reaches the question of whether it was built.
    #[test]
    fn the_version_bit_is_asked_first() {
        assert!(!Feature::RecordInheritance.is_implemented());
        assert!(matches!(
            Feature::RecordInheritance.gate_against(LanguageVersion::CSharp1),
            Some(FeatureGate::RequiresLaterVersion { required: "9.0", .. })
        ));
        assert!(matches!(
            Feature::RecordInheritance.gate_against(LanguageVersion::CSharp9),
            Some(FeatureGate::NotInThisBuild { .. })
        ));
    }

    /// Below the rung a gate quotes csc's noun, and above it the construct the reader wrote --
    /// which differ exactly where csc's noun covers something this build has.
    #[test]
    fn a_refusal_names_the_missing_construct_where_csc_names_a_built_one() {
        assert_eq!(
            Feature::WithExpression.gate_against(LanguageVersion::CSharp8),
            Some(FeatureGate::RequiresLaterVersion {
                feature: "records",
                required: "9.0"
            })
        );
        assert_eq!(
            Feature::WithExpression.gate_against(LanguageVersion::CSharp11),
            Some(FeatureGate::NotInThisBuild {
                feature: "with expression",
                instead: None
            })
        );
        assert_eq!(
            Feature::PatternCaseLabel.gate_against(LanguageVersion::CSharp6),
            Some(FeatureGate::RequiresLaterVersion {
                feature: "pattern matching",
                required: "7.0"
            })
        );
        assert_eq!(Feature::PatternCaseLabel.gate_against(LanguageVersion::CSharp7), None);
        assert!(matches!(
            Feature::RecordInheritance.gate_against(LanguageVersion::CSharp9),
            Some(FeatureGate::NotInThisBuild {
                feature: "records with a base list",
                ..
            })
        ));
        assert!(matches!(
            Feature::Iterators.gate_against(LanguageVersion::CSharp11),
            Some(FeatureGate::NotInThisBuild { feature: "iterators", .. })
        ));
        assert_eq!(Feature::RecordClass.gate_against(LanguageVersion::CSharp10), None);
        assert!(matches!(
            Feature::RecordClass.gate_against(LanguageVersion::CSharp9),
            Some(FeatureGate::RequiresLaterVersion {
                feature: "record structs",
                ..
            })
        ));
        for feature in Feature::ALL {
            let name = feature.refusal_description();
            assert!(
                !name.is_empty() && !name.contains('\'') && !name.ends_with('.'),
                "{feature:?}'s refusal noun is not a bare noun phrase: {name:?}"
            );
            if let Some(sentence) = feature.instead() {
                assert!(
                    sentence.ends_with('.') && sentence.starts_with(char::is_uppercase),
                    "{feature:?}'s replacement is appended as a sentence: {sentence:?}"
                );
            }
        }
    }

    /// `is_implemented` is the only record that a feature is emittable, so one the compiler emits
    /// must read `true` here: a stale `false` refuses a working construct as `NotInThisBuild` at
    /// every rung, including the one that introduced it.
    #[test]
    fn a_feature_the_compiler_emits_reads_implemented_and_its_gate_admits_it() {
        assert!(Feature::RequiredMembers.is_implemented());
        assert!(Feature::LeadingDigitSeparator.is_implemented());
        assert!(Feature::Records.is_implemented());
        assert_eq!(Feature::Records.gate_against(LanguageVersion::CSharp9), None);
        assert_eq!(Feature::RequiredMembers.gate_against(LanguageVersion::CSharp11), None);
        assert_eq!(Feature::LeadingDigitSeparator.gate_against(LanguageVersion::CSharp7_2), None);
    }
}
