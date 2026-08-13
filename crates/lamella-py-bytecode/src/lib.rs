#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

//! Lamella's Python bytecode contract -- the single source of truth.


extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;

/// The four bytes that open a serialized module: "LPYC" (Lamella PYthon Code).
pub const MAGIC: [u8; 4] = *b"LPYC";

/// The binary format version. Bumped when the container or instruction encoding
/// changes incompatibly; readers reject a version they do not recognize.
///
/// Version 14 added closures: the three deref ops, `CodeObject`'s `cellvars`/`freevars`,
/// and the `CLOSURE` bit on [`Op::MakeFunction`]'s flags.
///
/// Version 15 added the class-body namespace: `SetupClassNamespace` / `StoreName` / `LoadName`,
/// so a class body can read a name it just bound (namespace -> global -> built-in).
///
/// Version 16 added the import system: `ImportName` (import a module and push it) and
/// `ImportFrom` (read a name off the module on top of the stack), so `import m` and
/// `from m import a` bind their names.
///
/// Version 18 added `StoreGlobal` (a `global x` assignment inside a function stores to the module
/// namespace). 17 is skipped: it was once a bundle container's version, back when the two numbers
/// were independent and shared one dispatch space. See [`BUNDLE_FORMAT_VERSION`].
///
/// Version 19 added `YieldFrom` (`yield from iterable` -- a generator delegates to a sub-iterator).
///
/// Version 20 added `ImportStar` (`from m import *` -- bind a module's public names into the current
/// module namespace).
///
/// Version 21 added `InplaceBinOp` (augmented assignment `x OP= y` -- the in-place binary operator).
///
/// Version 22 added a code object's `doc` (a function's docstring, which `__doc__` reads) and
/// `BuildClassKw` (keyword arguments in a class header).
///
/// Version 23 added a code object's `is_coroutine` (an `async def` body) and [`Op::Await`]. The
/// coroutine bit is INDEPENDENT of `is_generator` rather than a refinement of it -- CPython's
/// `CO_COROUTINE` and `CO_GENERATOR` are separate flags, and an `async def` with no `yield` has
/// only the former set.
///
/// Version 24 packed a code object's four boolean properties into ONE [`CodeFlags`] byte, where
/// each had cost a whole byte. Done while 23 was hours old and nothing persisted had been built
/// against it -- and done as a BUMP rather than by redefining 23, because a version identifies a
/// layout, and the check below compares for EQUALITY: a silently-changed 23 would have decoded as
/// garbage where a bumped one is refused. The four spare bits are the point, not the three bytes;
/// see [`CodeFlags`].
///
/// Version 25 added [`Op::ListGrow`], which separates a growable list's CAPACITY step from its
/// STORE so a heap exhaustion can be caught between them.
///
/// Version 26 dropped [`Op::CallEx`]'s `argc`, which restated the length of the argument-tag list it
/// already points at. The four wire bytes are incidental; what it buys is in memory, where an enum
/// is as wide as its widest variant and a three-word payload sets that width.
///
/// This version gave a module a trailing length-prefixed DEBUG SECTION, empty in every artifact this
/// build writes. It costs four bytes a module and buys the ability to add source positions later
/// without moving this number. It shipped EMPTY and now carries line tables, which is the
/// reservation paying out rather than a second format change.
pub const FORMAT_VERSION: u16 = 27;

/// The capability bits an artifact's header carries: what its bytecode REQUIRES of the runtime that
/// loads it. A reader that does not implement a required capability refuses the artifact by name
/// rather than mis-executing it -- see [`SUPPORTED_FEATURES`] and [`DecodeError::UnsupportedFeatures`].
///
/// A bit means "the reader must implement this to run me", never "the writer had this turned on".
/// An artifact that requires NOTHING declares zero and stays loadable by any reader that understands
/// the base format, however much older that reader is than the toolchain that wrote it. That is the
/// property the mask exists for, and it is why this is a set of capabilities rather than a minimum
/// version: a floor refuses artifacts an old reader could actually have run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FeatureFlags(pub u16);

impl FeatureFlags {
    /// The first-light subset: a typed integer function plus one dynamic attribute
    /// access. The only flag defined so far.
    pub const FIRST_LIGHT: FeatureFlags = FeatureFlags(0x0001);

    /// Whether every bit in `other` is also set here.
    #[must_use]
    pub fn contains(self, other: FeatureFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// The bits set here that are absent from `supported`.
    #[must_use]
    pub fn missing_from(self, supported: FeatureFlags) -> FeatureFlags {
        FeatureFlags(self.0 & !supported.0)
    }

    /// Whether no bits are set.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

/// The capabilities THIS build's reader implements. An artifact requiring anything outside this is
/// refused.
///
/// Deliberately a separate constant from whatever a writer puts in a header, and they are not the
/// same question even while they hold the same value: a toolchain that gains a capability raises what
/// it may WRITE, a reader that gains one raises what it may READ, and the whole point of the mask is
/// that those two move independently. Collapsing them into one constant is how a reader ends up
/// claiming to support whatever it happens to emit.
pub const SUPPORTED_FEATURES: FeatureFlags = FeatureFlags::FIRST_LIGHT;

/// One capability an image either provides or does not -- the vocabulary a [`Profile`] is written in.
///
/// Each name here is a knob that EXISTS: a cargo feature on `lamella-py-runtime`, whose absence a
/// running interpreter already answers by name. This enum does not invent capabilities; it lets a
/// compiler be told about the ones the runtime already enforces, so a developer hears about them
/// while typing instead of when the program runs.
///
/// Deliberately short. `bundled-stdlib` and the GC tier are real knobs and are ABSENT, because
/// neither changes what the front end accepts or what an editor should offer -- one is the caller's
/// import resolver and the other is invisible to the language. Minting a name before there is a
/// consequence for it is what this lane argued against for [`FeatureFlags`]' bits, and the argument
/// does not change because the enum is a different one. Adding a variant is additive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Capability {
    /// The `float` type and everything that produces one (`lamella-py-runtime`'s `float` feature).
    /// Off is the no-float tier, where the interpreter answers `FloatUnavailable` by name.
    Float,
    /// The `complex` type, `1j` literals and complex arithmetic (the `complex` feature).
    ///
    /// A complex is a PAIR of `f64`, so it cannot outlive [`Capability::Float`] -- the cargo feature
    /// says `complex = ["float"]` and [`Profile`] enforces the same implication.
    Complex,
    /// `dir()` and the per-type name lists it reports from (the `introspection` feature). Off, every
    /// method stays callable and only the ASKING goes away, so this gates what an editor offers and
    /// refuses nothing at compile time.
    Introspection,
}

impl Capability {
    /// Every capability, in declaration order.
    ///
    /// [`Profile::FULL`] is derived from this, so a variant missing HERE narrows the default
    /// profile for every caller and narrows it silently. The guard below is what makes that a
    /// compile error rather than a behavior change.
    pub const ALL: &'static [Capability] =
        &[Capability::Float, Capability::Complex, Capability::Introspection];

    /// The name this capability is spelled with in a diagnostic and in the runtime's cargo manifest --
    /// the SAME string in both, so a developer who reads "float is not available" in an editor can
    /// find the knob that turned it off.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Float => "float",
            Capability::Complex => "complex",
            Capability::Introspection => "introspection",
        }
    }

    /// This capability's bit in a [`Profile`].
    const fn bit(self) -> u16 {
        match self {
            Capability::Float => 0x0001,
            Capability::Complex => 0x0002,
            Capability::Introspection => 0x0004,
        }
    }
}

/// Ties [`Capability::ALL`]'s LENGTH to the variant list, which no test can do.
///
/// `ALL` is hand-written and [`Profile::FULL`] is derived from it, so a variant left out of `ALL`
/// narrows the default profile while every runtime check agrees with the narrowed answer -- there
/// is nothing for a test to disagree with. `as_str` and `bit` are exhaustive, so adding a variant
/// does force two edits; `ALL` is the third, and nothing else forces it.
///
/// The match below is exhaustive too, so adding a variant fails to compile HERE, in the one place
/// that also states the length the assertion checks.
const fn _every_capability_is_in_all(capability: Capability) -> usize {
    match capability {
        Capability::Float => 0,
        Capability::Complex => 1,
        Capability::Introspection => 2,
    }
}
const _: () = assert!(Capability::ALL.len() == 3, "a capability was added without extending ALL");

impl core::fmt::Display for Capability {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// What an image PROVIDES: the capability set a compiler is compiling against.
///
/// # The dual of [`FeatureFlags`], and why both exist
///
/// [`FeatureFlags`] travels ON an artifact and says what a runtime must implement to RUN it; it is
/// checked at LOAD, on the device, after the compile is over. A `Profile` travels INTO a compile and
/// says what the runtime will turn out to have; it is checked while there is still a source line to
/// point at. Same subject, opposite directions, and neither substitutes for the other -- an
/// `eval` on a device produces no artifact at all, so a capability list that could only describe
/// artifacts would have nothing to check.
///
/// # Why a value rather than a cargo feature
///
/// The knobs are cargo features on the RUNTIME. One front-end build compiles for every profile --
/// the same process serves a host and a device in the browser IDE -- so a `cfg!` in this crate could
/// only ever describe the machine the compiler was built for. Worse, a front end compiled INTO a
/// device image with a mismatched feature set would disagree with its own runtime silently, because
/// nothing compares two build configurations. A value cannot drift from itself, and it can carry a
/// profile that is not knowable when the compiler is built: one read from a BSP, or chosen at bake
/// time.
///
/// # The invariant
///
/// A `Profile` cannot express an image that cannot be built. [`Capability::Complex`] requires
/// [`Capability::Float`] in the cargo manifest, so dropping `float` here drops `complex` with it and
/// adding `complex` adds `float`. Without that, a profile could say "no floats, but imaginary
/// literals are fine" and the compiler would faithfully accept a program no image can run.
///
/// Matches the sibling C# front end's [`LanguageVersion::supports(Feature)`] shape -- a value asked
/// a predicate -- so the two languages gate the same way rather than inventing two conventions.
///
/// [`LanguageVersion::supports(Feature)`]: https://docs.rs/lamella-syntax
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile(u16);

impl Profile {
    /// Every capability: a host, a browser, and any device image built with the runtime's default
    /// features. The default, so a caller that has no profile to give compiles exactly as it did
    /// before this type existed.
    ///
    /// DERIVED from [`Capability::ALL`] rather than written as a literal. `bit` and `as_str` are
    /// exhaustive matches the compiler checks, so a new capability cannot skip them; a literal here
    /// could be skipped, and the result would be a `FULL` that quietly means "every capability
    /// except the newest" while its own documentation still says otherwise. That is the direction
    /// that looks like it worked, so the fact is computed once rather than written twice.
    pub const FULL: Profile = {
        let mut bits = 0;
        let mut index = 0;
        while index < Capability::ALL.len() {
            bits |= Capability::ALL[index].bit();
            index += 1;
        }
        Profile(bits)
    };

    /// No optional capability at all -- the smallest tier, for building a profile up by name.
    pub const BARE: Profile = Profile(0);

    /// Whether this image provides `capability`.
    #[must_use]
    pub fn supports(self, capability: Capability) -> bool {
        self.0 & capability.bit() != 0
    }

    /// This profile with `capability` added, plus anything it requires.
    #[must_use]
    pub fn with(self, capability: Capability) -> Profile {
        let mut bits = self.0 | capability.bit();
        if capability == Capability::Complex {
            bits |= Capability::Float.bit();
        }
        Profile(bits)
    }

    /// This profile with `capability` removed, plus anything that requires it.
    ///
    /// Removing [`Capability::Float`] removes [`Capability::Complex`] too. The alternative is a
    /// value describing an image nobody can build, and a compiler that then accepts `1j` for it.
    #[must_use]
    pub fn without(self, capability: Capability) -> Profile {
        let mut bits = self.0 & !capability.bit();
        if capability == Capability::Float {
            bits &= !Capability::Complex.bit();
        }
        Profile(bits)
    }

}

impl Default for Profile {
    fn default() -> Self {
        Profile::FULL
    }
}

/// A Python string constant's value: a sequence of CODE POINTS, held as WTF-8 bytes.
///
/// # Why not `String`
///
/// A Python `str` is a sequence of code points, U+0000..=U+10FFFF, and that range INCLUDES the
/// surrogates U+D800..=U+DFFF -- `len('\ud800')` is 1 and its `ord` is `0xd800`. A Rust `String` is
/// UTF-8, whose value space is the Unicode SCALAR VALUES: every code point EXCEPT those. The
/// difference is exactly the surrogate block, so a `String` cannot hold what a Python `str` can, and
/// no choice of Rust string type fixes that.
///
/// The bytes are WTF-8, written by [`lamella_wtf8`] so this and the C# string tiers cannot drift in
/// the byte form. **Each code point is encoded INDEPENDENTLY -- a surrogate PAIR is NOT combined
/// into the supplementary 4-byte form.** That combining is what makes C#'s tier WTF-8 rather than
/// CESU-8 and is correct there, because a `System.String` is UTF-16 and a pair IS one character. In
/// Python the same two surrogates are TWO characters, so combining them would change `len` and `ord`
/// under a running program. This is the "generalized" form, and it is byte-for-byte what CPython's
/// own `surrogatepass` produces.
///
/// **Text with no surrogate in it is byte-identical to UTF-8**, which is what makes [`Self::as_str`]
/// answer `Some` for every string any ordinary program builds.
#[derive(Debug, Clone, PartialEq, Eq, Default, PartialOrd, Ord, Hash)]
pub struct PyStr(Vec<u8>);

impl PyStr {
    /// The WTF-8 bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Take ownership of the WTF-8 bytes as they stand.
    ///
    /// The caller is responsible for their shape: they must be what [`lamella_wtf8::push_code_point`]
    /// would have written, or a reader gets [`lamella_wtf8::REPLACEMENT`] where it expected a
    /// character. Used by the decoder, which reads bytes this type wrote.
    #[must_use]
    pub fn from_wtf8(bytes: Vec<u8>) -> PyStr {
        PyStr(bytes)
    }

    /// Build from code points, encoding each one independently.
    pub fn from_code_points(codes: impl IntoIterator<Item = u32>) -> PyStr {
        let mut bytes = Vec::new();
        for code in codes {
            lamella_wtf8::push_code_point(code, &mut bytes);
        }
        PyStr(bytes)
    }

    /// Append another string's code points -- adjacent-literal concatenation, `"ab" "cd"`.
    ///
    /// **Byte concatenation is correct here precisely BECAUSE pairs are not combined.** A high
    /// surrogate ending one string and a low surrogate starting the next stay TWO characters, which
    /// is what Python does; a pair-combining encoder would have to re-encode across the join, and
    /// joining its output byte-wise would silently produce a different string than CPython's.
    pub fn append(&mut self, other: &PyStr) {
        self.0.extend_from_slice(&other.0);
    }

    /// The value as a `&str`, or `None` when it holds a surrogate and therefore has no UTF-8 form.
    ///
    /// `None` is not an error path a consumer can skip: it is the case this type exists for. A
    /// consumer that can only handle UTF-8 must decide what to do about a Python string it cannot
    /// represent, and that decision is the consumer's rather than this type's.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        core::str::from_utf8(&self.0).ok()
    }

    /// The code points, in order -- the sequence a Python `str` actually is, so `count()` is `len()`
    /// and the nth item is what `ord(s[n])` answers.
    pub fn code_points(&self) -> impl Iterator<Item = u32> + '_ {
        let mut at = 0;
        core::iter::from_fn(move || {
            let (code, width) = lamella_wtf8::next_code_point(&self.0, at)?;
            at += width;
            Some(code)
        })
    }

    /// Whether any code point is a surrogate -- i.e. whether this string is one the UTF-8 world
    /// cannot represent.
    #[must_use]
    pub fn has_surrogate(&self) -> bool {
        self.code_points().any(|c| (0xD800..=0xDFFF).contains(&c))
    }
}

impl From<&str> for PyStr {
    fn from(s: &str) -> PyStr {
        PyStr(s.as_bytes().to_vec())
    }
}

/// Compare against Rust text directly.
///
/// Byte equality IS code-point equality here: surrogate-free WTF-8 is byte-identical to UTF-8, and a
/// `str` can never hold a surrogate, so a surrogate-bearing value correctly compares unequal to
/// every `str` rather than to some lossy rendering of itself.
impl PartialEq<str> for PyStr {
    fn eq(&self, other: &str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl PartialEq<&str> for PyStr {
    fn eq(&self, other: &&str) -> bool {
        self.0 == other.as_bytes()
    }
}

impl From<String> for PyStr {
    fn from(s: String) -> PyStr {
        PyStr(s.into_bytes())
    }
}

/// A code object's boolean properties, packed into ONE wire byte -- the same shape as
/// [`FeatureFlags`] one level up, rather than a second convention.
///
/// It replaced four adjacent `bool`s that each cost a whole byte. The three bytes saved per code
/// object, in every bundle on every device, are the smaller half of the reason. **The larger half is
/// the four SPARE BITS**: a reader checks the format version for STRICT EQUALITY (see
/// [`FORMAT_VERSION`]), so once a version ships on a device, bumping it is a device-compatibility
/// event and not merely a code change -- every board in the field would need reflashing to load a
/// bundle a newer toolchain built. A flag that fits in a spare bit needs no bump at all, which is
/// what turns "settle every flag before the freeze" back into a decision that can wait for its
/// feature.
///
/// The [`CodeObject`] itself keeps its four NAMED fields; this type exists only at the wire
/// boundary, so nothing that reads `co.is_generator` had to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CodeFlags(pub u8);

impl CodeFlags {
    /// The body contains `yield` -- calling it returns a generator object.
    pub const GENERATOR: CodeFlags = CodeFlags(0x01);
    /// An `async def` -- calling it returns a coroutine object. Independent of [`Self::GENERATOR`]
    /// (see [`CodeObject::is_coroutine`]); a body setting BOTH would be an async generator, which
    /// nothing emits.
    pub const COROUTINE: CodeFlags = CodeFlags(0x02);
    /// The parameter list has a `*args` slot.
    pub const VARARGS: CodeFlags = CodeFlags(0x04);
    /// The parameter list has a `**kwargs` slot.
    pub const VARKWARGS: CodeFlags = CodeFlags(0x08);

    /// The flags of `co`, for encoding.
    #[must_use]
    pub fn of(co: &CodeObject) -> CodeFlags {
        let mut bits = 0;
        for (set, flag) in [
            (co.is_generator, Self::GENERATOR),
            (co.is_coroutine, Self::COROUTINE),
            (co.has_varargs, Self::VARARGS),
            (co.has_varkwargs, Self::VARKWARGS),
        ] {
            if set {
                bits |= flag.0;
            }
        }
        CodeFlags(bits)
    }

    /// Whether every bit in `other` is also set here.
    #[must_use]
    pub fn contains(self, other: CodeFlags) -> bool {
        self.0 & other.0 == other.0
    }
}

/// The built-in exception hierarchy, `(name, base-name)` pairs -- the ONE definition every
/// engine derives from: the interpreter builds its exception classes from this table, and a
/// static lowering derives per-type tags and subtype closures from it, so `except LookupError:`
/// catches an `IndexError` identically everywhere.
///
/// Invariants: each entry's base appears EARLIER in the table (`""` marks the root,
/// `BaseException`); the table is APPEND-ONLY (new types are added at the end, existing entries
/// are never reordered or renamed, so derived artifacts stay stable).
///
/// `GeneratorExit` derives from `BaseException` (NOT `Exception`), so `except Exception:` around
/// a `yield` does not swallow a generator's `close()`.
pub const EXCEPTION_HIERARCHY: &[(&str, &str)] = &[
    ("BaseException", ""),
    ("Exception", "BaseException"),
    ("ArithmeticError", "Exception"),
    ("ZeroDivisionError", "ArithmeticError"),
    ("OverflowError", "ArithmeticError"),
    ("LookupError", "Exception"),
    ("IndexError", "LookupError"),
    ("KeyError", "LookupError"),
    ("AttributeError", "Exception"),
    ("NameError", "Exception"),
    ("UnboundLocalError", "NameError"),
    ("TypeError", "Exception"),
    ("ValueError", "Exception"),
    ("AssertionError", "Exception"),
    ("RuntimeError", "Exception"),
    ("RecursionError", "RuntimeError"),
    ("NotImplementedError", "RuntimeError"),
    ("StopIteration", "Exception"),
    ("ImportError", "Exception"),
    ("ModuleNotFoundError", "ImportError"),
    ("OSError", "Exception"),
    ("TimeoutError", "OSError"),
    ("GeneratorExit", "BaseException"),
    ("SystemExit", "BaseException"),
    ("FileNotFoundError", "OSError"),
    ("FileExistsError", "OSError"),
    ("IsADirectoryError", "OSError"),
    ("NotADirectoryError", "OSError"),
    ("PermissionError", "OSError"),
    ("MemoryError", "Exception"),
    ("StopAsyncIteration", "Exception"),
    ("UnicodeError", "ValueError"),
    ("UnicodeEncodeError", "UnicodeError"),
    ("BlockingIOError", "OSError"),
];

/// The AOT exception TAG of a Python exception `name`: FNV-1a-32 over `"python." + name`, the high
/// bit forced set. A tag is therefore never zero -- zero is the "no exception in flight" sentinel the
/// tag-dispatch exception model reserves. This is the decentralized name-tag convention: the AOT
/// throw site, the catch dispatch, and a synthesized bounds-or-divide-by-zero check all derive the
/// SAME value from the type's name with no shared registry, so a tag never diverges across engines or
/// deployment tiers. The formula is the shared metadata name-tag hash specialized to the `python`
/// namespace (so a Python tag can never collide with a `System.*` one); a unit test pins the results.
#[must_use]
pub fn exception_tag(name: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in b"python.".iter().chain(name.as_bytes()) {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash | 0x8000_0000
}

/// A binary arithmetic/bitwise operator carried by [`Op::Binary`] -- add/sub/mul, floor-division
/// and modulo, true division (`/`, float-producing), exponentiation (`**`), and the bitwise operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum BinOp {
    /// `a + b`.
    Add = 0,
    /// `a - b`.
    Sub = 1,
    /// `a * b`.
    Mul = 2,
    /// `a // b` -- floor division.
    FloorDiv = 3,
    /// `a % b` -- modulo (the result takes the sign of the divisor, per Python).
    Mod = 4,
    /// `a & b` -- bitwise AND.
    BitAnd = 5,
    /// `a | b` -- bitwise OR.
    BitOr = 6,
    /// `a ^ b` -- bitwise XOR.
    BitXor = 7,
    /// `a << b` -- left shift.
    LShift = 8,
    /// `a >> b` -- right shift (arithmetic: Python ints are signed).
    RShift = 9,
    /// `a / b` -- true division; always produces a float (even for integer operands).
    TrueDiv = 10,
    /// `a ** b` -- exponentiation, right-associative. A non-negative integer exponent gives an
    /// integer (promoting past the fixnum range to a long); a negative exponent produces a float.
    Pow = 11,
    /// `a @ b` -- matrix multiplication (`__matmul__` / `__rmatmul__`). No builtin numeric type
    /// implements it (int/float `@` is a `TypeError`); it exists for user classes that define the
    /// dunder.
    MatMul = 12,
}

impl BinOp {
    /// The operator for a raw byte, or `None` if it is not defined.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<BinOp> {
        match byte {
            0 => Some(BinOp::Add),
            1 => Some(BinOp::Sub),
            2 => Some(BinOp::Mul),
            3 => Some(BinOp::FloorDiv),
            4 => Some(BinOp::Mod),
            5 => Some(BinOp::BitAnd),
            6 => Some(BinOp::BitOr),
            7 => Some(BinOp::BitXor),
            8 => Some(BinOp::LShift),
            9 => Some(BinOp::RShift),
            10 => Some(BinOp::TrueDiv),
            11 => Some(BinOp::Pow),
            12 => Some(BinOp::MatMul),
            _ => None,
        }
    }
}

/// A comparison operator carried by [`Op::Compare`]. Each compares the two values
/// below it and pushes a Python boolean.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum CmpOp {
    /// `a == b`.
    Eq = 0,
    /// `a != b`.
    Ne = 1,
    /// `a < b`.
    Lt = 2,
    /// `a <= b`.
    Le = 3,
    /// `a > b`.
    Gt = 4,
    /// `a >= b`.
    Ge = 5,
    /// `a is b` -- object identity (not value equality).
    Is = 6,
    /// `a is not b` -- object non-identity.
    IsNot = 7,
}

impl CmpOp {
    /// The operator for a raw byte, or `None` if it is not defined.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<CmpOp> {
        match byte {
            0 => Some(CmpOp::Eq),
            1 => Some(CmpOp::Ne),
            2 => Some(CmpOp::Lt),
            3 => Some(CmpOp::Le),
            4 => Some(CmpOp::Gt),
            5 => Some(CmpOp::Ge),
            6 => Some(CmpOp::Is),
            7 => Some(CmpOp::IsNot),
            _ => None,
        }
    }
}

/// A unary operator carried by [`Op::Unary`]. Pops the operand and pushes the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum UnaryOp {
    /// `-a` -- arithmetic negation (`__neg__`).
    Neg = 0,
    /// `+a` -- unary plus (`__pos__`; identity for ints).
    Pos = 1,
    /// `~a` -- bitwise inversion (`__invert__`; `-a - 1` for ints).
    Invert = 2,
}

impl UnaryOp {
    /// The operator for a raw byte, or `None` if it is not defined.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<UnaryOp> {
        match byte {
            0 => Some(UnaryOp::Neg),
            1 => Some(UnaryOp::Pos),
            2 => Some(UnaryOp::Invert),
            _ => None,
        }
    }
}


/// One bytecode instruction -- the decoded, in-memory form the interpreter
/// dispatches and the lowering walks. The set is deliberately small and orthogonal
/// for first light; it grows behind the version stamp as the language surface
/// widens. Operand indices reference the owning [`CodeObject`]'s pools.
///
/// # Op-tag registry
///
/// The wire tag for each op -- the leading `u8` in `encode_op` and the matching `decode`
/// arm. The binary encoding is stable and versioned; a reader rejects an unknown tag. Tags
/// 22 and 23 are unused (a historical gap).
///
/// | tag(s) | ops | group |
/// |-------:|-----|-------|
/// |   0-13 | LoadConst, LoadFast, StoreFast, LoadGlobal, LoadAttr, Binary, Compare, PopTop, Jump, PopJumpIfFalse, Call, Return, Unary, Subscript | core |
/// |  14-21 | BuildSlice, BuildList, BuildTuple, BuildDict, GetIter, ForIter, Setitem, Contains | containers + iteration |
/// |  22-23 | (free) | |
/// |  24-29 | Raise, MatchExc, LoadExc, PopExcept, Reraise, DeleteFast | exceptions |
/// |  30-32 | MakeFunction, BuildClass, SetAttr | classes |
/// |     33 | UnpackSequence | tuple-unpacking |
/// |  34-36 | ListAppend, SetAdd, DictInsert | comprehensions |
/// |     37 | LoadSuper | super() |
/// |     38 | BuildSet | set literals |
/// |     39 | UnpackEx | starred unpacking |
/// |     40 | CallKw | keyword calls |
/// |     41 | Yield | generators |
/// |     42 | CallEx | star-call unpacking |
/// |  43-44 | DeleteItem, DeleteAttr | del subscript/attribute |
/// |  45-47 | LoadDeref, StoreDeref, LoadClosure | closures |
/// |  48-50 | SetupClassNamespace, StoreName, LoadName | class-body namespace |
/// |  51-52 | ImportName, ImportFrom | imports |
/// |     53 | StoreGlobal | `global` |
/// |     54 | YieldFrom | generators |
/// |     55 | ImportStar | imports |
/// |     56 | InplaceBinOp | augmented assignment |
/// |     57 | BuildClassKw | class keyword arguments |
/// |     58 | Await | async |
/// |     59 | ListGrow | growable lists |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// Push `consts[idx]`.
    LoadConst(u32),
    /// Push the local variable in slot `idx`.
    LoadFast(u32),
    /// Pop and store into the local variable in slot `idx`.
    StoreFast(u32),
    /// Push the global or built-in named `names[idx]`. DEFINED for completeness;
    /// the first-light parity slice emits function bodies only (no globals), so the
    /// interpreter may leave this unimplemented until typed calls are enabled.
    LoadGlobal(u32),
    /// Pop an object and push `getattr(object, names[name])` -- the one dynamic
    /// operation in first light, lowering to the `py_getattr` intrinsic. `cache` is
    /// this site's inline-cache slot (RAM side array; see [`CodeObject::cache_count`]).
    LoadAttr {
        /// This site's entry in [`CodeObject::wide_operands`]: `[name, cache]`, where `name` indexes
        /// the code object's `names` pool and `cache` is the inline-cache slot.
        site: u32,
    },
    /// Pop the index then the container, and push `container[index]` (subscript),
    /// lowering to the `py_getitem` intrinsic. `cache` is this site's inline-cache slot.
    Subscript {
        /// This site's inline-cache slot, assigned by ascending static position.
        cache: u32,
    },
    /// Pop the step, then upper, then lower bound (each a `None` on the stack when
    /// omitted) and push `slice(lower, upper, step)`; the slice then feeds a `Subscript`.
    /// Used for `s[i:j]` / `s[i:j:k]`.
    BuildSlice,
    /// Pop `count` values (pushed left to right) and push a new list of them. For a list
    /// display `[a, b, c]`.
    BuildList(u32),
    /// Pop `count` values (pushed left to right) and push a new tuple. For `(a, b, c)`.
    BuildTuple(u32),
    /// Pop `count` key-value pairs (each pushed key then value, pairs left to right) and
    /// push a new dict. For a dict display `{k: v, ...}`.
    BuildDict(u32),
    /// Pop an iterable and push an iterator over it (`iter(obj)`). For `for x in obj`.
    GetIter,
    /// Pop the iterator and advance it: on a value, push the value; on exhaustion, set the
    /// instruction pointer to `target` (absolute). The loop reloads the iterator each pass
    /// (it lives in a local), so the stack stays balanced. For `for x in obj`.
    ForIter(u32),
    /// Pop the index, then the container, then the value, and do `container[index] = value`
    /// (a side-effecting store; nothing is pushed). For `c[i] = v` on a mutable container.
    Setitem,
    /// Pop the container, then the element, and push whether the container contains the
    /// element. For the membership test `x in c` (`negate` flips it to `x not in c`).
    Contains {
        /// Whether this is `not in` (the boolean result is inverted).
        negate: bool,
    },
    /// Raise an exception: `argc` 1 pops the exception value (a class is instantiated with
    /// no arguments); `argc` 0 re-raises the active exception. For `raise`.
    Raise(u8),
    /// Pop a type and push whether the active exception is an instance of it -- the
    /// `except E` type test.
    MatchExc,
    /// Push the active exception, to bind it in `except ... as name`.
    LoadExc,
    /// Clear the active-exception state once a handler has dealt with it.
    PopExcept,
    /// Re-raise the active exception (a handler chain ended with no matching clause).
    Reraise,
    /// Make local slot `slot` unbound (a `del`); a later `LoadFast` of it raises `NameError`.
    /// Emitted for the `except ... as name` auto-deletion at the end of the handler.
    DeleteFast(u32),
    /// Push a function value for the Module function `names[func]`. `flags` (mirroring CPython's
    /// MAKE_FUNCTION bits) says what sits on the stack below, popped top-down: bit0 (`0x01`) a
    /// positional-defaults TUPLE, bit1 (`0x02`) a kwdefaults DICT, bit2 (`0x04`, `CLOSURE`) the
    /// captured cells for a closure. The stack layout, bottom to top, is
    /// `[defaults-tuple?] [kwdefaults-dict?] [cell0 .. cell{f-1}]`, so the cells (on top) are popped
    /// first, then the kwdefaults dict, then the defaults tuple. When `CLOSURE` is set, exactly
    /// `functions[func].freevars.len()` cells were pushed (by [`Op::LoadClosure`], in freevar order)
    /// and are stored in the new function object's captured-cells field -- a closure is just a
    /// function object WITH cells. `flags == 0` is a plain stateless function reference; `flags != 0`
    /// builds a function object carrying the def-time defaults and/or captured cells.
    MakeFunction {
        /// The index into `names` of the Module function to make.
        func: u32,
        /// What sits on the stack below (bit0 a defaults tuple, bit1 a kwdefaults dict, bit2/`0x04`
        /// `CLOSURE` the captured cells).
        flags: u8,
    },
    /// Pop the namespace dict, then the base, then the name, and push a new class object (a
    /// type). For a `class` definition.
    BuildClass,
    /// [`Op::BuildClass`] for a class header carrying KEYWORD arguments -- `class C(Base, tag="x")`.
    ///
    /// At the op the stack is `[name, base, kwval0 .. kwval{k-1}]` and the NAMESPACE is in the
    /// class-body register: [`Op::SetupClassNamespace`] runs after the keyword values are pushed. A
    /// reader that takes the register when one is set and pops the namespace otherwise -- which is
    /// what [`Op::BuildClass`]'s own dict-display form already needs -- reads this correctly either
    /// way, since a stack-carried namespace would sit above the keyword values.
    ///
    /// `consts[kwnames]` (a [`Const::KwNames`]) gives the `k` keyword names in the same order, exactly
    /// as [`Op::CallKw`] names a call's keywords -- source order, which is the order a base's
    /// `__init_subclass__(cls, **kw)` needs to build its keyword arguments. `metaclass` is not among
    /// them, being outside this subset.
    ///
    /// A class header WITHOUT keywords still emits the plain [`Op::BuildClass`], so this op is
    /// purely additive: no other class-header encoding changes shape.
    BuildClassKw {
        /// The index into `consts` of the [`Const::KwNames`] naming the keywords in order.
        kwnames: u32,
    },
    /// Pop the object, then the value, and do `object.<names[name]> = value` (`cache` is the
    /// inline-cache slot). For an attribute assignment `obj.attr = value`.
    SetAttr {
        /// This site's entry in [`CodeObject::wide_operands`]: `[name, cache]`.
        site: u32,
    },
    /// Pop a sequence and push its `count` elements in REVERSE, so following `StoreFast`s bind
    /// the first element first. A length mismatch raises `ValueError`. For tuple-unpacking
    /// (`a, b = expr` and `for a, b in iter`).
    UnpackSequence(u32),
    /// Pop the value, then the list, and append the value to the list (in place). For a list
    /// comprehension.
    ListAppend,
    /// Pop the element, then the set, and add the element. For a set comprehension.
    SetAdd,
    /// Pop the value, then the key, then the dict, and insert `key -> value`. For a dict
    /// comprehension.
    DictInsert,
    /// Push a super object bound to the enclosing class `names[name]` and the frame's first
    /// local (`self`). For a no-arg `super()` in a method; a following `LoadAttr` finds the
    /// base class's attribute bound to `self`.
    LoadSuper(u32),
    /// Pop `count` elements and push a new set (deduped). For a set literal `{a, b, c}`; a
    /// set comprehension builds `BuildSet(0)` then `SetAdd`s.
    BuildSet(u32),
    /// Pop a sequence and unpack it for a starred target `a, *b, c = seq`: push (reversed, so
    /// the following `StoreFast`s take them left-to-right) the `before` head elements, then a
    /// LIST of the middle (`len - before - after` elements), then the `after` tail elements.
    /// Requires `len >= before + after`, else `ValueError`. For `a, *b = seq`.
    UnpackEx {
        /// This site's entry in [`CodeObject::wide_operands`]: `[before, after]`, the target counts
        /// either side of the star.
        site: u32,
    },
    /// Pop the right operand then the left, and push `left <op> right`.
    Binary(BinOp),
    /// Pop the value then the target, apply the AUGMENTED (in-place) binary operator `op`, and push
    /// the result -- for `target OP= value`. Distinct from [`Op::Binary`] because CPython augmented
    /// assignment uses the in-place dunder (`__iadd__`, `__ior__`, ...): the target's in-place method
    /// if it defines one, else a builtin mutable container's in-place op (a list extends, a dict
    /// updates, ...) returning the SAME object, else the plain binary op (immutables are unchanged).
    InplaceBinOp(BinOp),
    /// Pop the right operand then the left, and push the boolean `left <cmp> right`.
    Compare(CmpOp),
    /// Pop the operand and push `<op> operand`.
    Unary(UnaryOp),
    /// Pop and discard the top of the stack -- used after an expression statement
    /// whose value is unused.
    PopTop,
    /// Set the instruction pointer to op index `target` (absolute).
    Jump(u32),
    /// Pop a value; if it is not truthy, set the instruction pointer to op index
    /// `target` (absolute).
    PopJumpIfFalse(u32),
    /// Call a callable: the stack holds `[callable, arg0, .., arg{argc-1}]`; pop them
    /// and push the result. DEFINED for completeness; deferred for the first-light
    /// parity slice (the harness drives the call boundary), like [`Op::LoadGlobal`].
    Call(u32),
    /// Call a callable with keyword arguments: the stack holds `[callable, pos0 .. pos{argc-1},
    /// kwval0 .. kwval{k-1}]`, where `consts[kwnames]` (a [`Const::KwNames`]) gives the `k` keyword
    /// NAMES in the order their values were pushed. Pop them all plus the callable, bind by CPython's
    /// call rules, and push the result. [`Op::Call`] stays the positional-only fast path.
    CallKw {
        /// This site's entry in [`CodeObject::wide_operands`]: `[argc, kwnames]`, the positional
        /// count and the `Const::KwNames` index.
        site: u32,
    },
    /// Pop a value and return it from the current function.
    Return,
    /// Suspend the current generator, yielding the popped value to the caller; on resume, push the
    /// value the caller injected (`None` under `next`). A `yield` expression -- appears only in a
    /// generator function (one whose [`CodeObject::is_generator`] is set).
    Yield,
    /// Delegate to the sub-iterator on top of the stack (`yield from`). Drives it to exhaustion:
    /// each value the sub yields is re-yielded to THIS generator's caller (suspending here), and the
    /// value the caller injects (via `send`) -- or an exception it throws in -- is forwarded into the
    /// sub. When the sub is exhausted, its return value (its `StopIteration.value`) is pushed as the
    /// `yield from` expression's result. Stack: pops the iterator, pushes the result. Appears only in
    /// a generator function. The operand is produced by a preceding [`Op::GetIter`].
    YieldFrom,
    /// Ensure the growable list in local slot `list` has room for one more element -- the CAPACITY
    /// half of `xs.append(v)`, emitted immediately before the `append` call itself.
    ///
    /// It exists to be a place a block can END. A typed lowering grows the backing by calling out to
    /// the runtime, and that call can FAIL (a full heap); the store that follows must then not
    /// happen, and skipping it needs a branch, and a branch needs a block boundary. Grow and store
    /// inside one op leave nowhere to put it -- so they are two ops, and this is the first.
    ///
    /// **It changes nothing observable and pushes nothing.** An engine whose lists manage their own
    /// capacity has nothing to do here: the following `append` call is still the whole operation, and
    /// this is a no-op for it. A mis-emitted one is therefore harmless rather than wrong, which is
    /// what lets the compiler emit it from a static guess about the receiver's type.
    ListGrow {
        /// The local slot holding the list, which the following call appends to.
        list: u32,
    },
    /// Await the awaitable on top of the stack (`await expr`). Pop it, suspend this coroutine until
    /// it completes, and push its result. Appears only in a code object whose
    /// [`CodeObject::is_coroutine`] is set.
    ///
    /// A DISTINCT op rather than a [`Op::YieldFrom`] on the same operand, because `await` enforces
    /// rules `yield from` does not: the operand must be AWAITABLE (a coroutine, or an object with
    /// `__await__` returning an iterator) and a plain iterable is a `TypeError` -- so the
    /// awaitable-or-not decision belongs to the op rather than to whatever produced the operand.
    /// Unlike `yield from`, no [`Op::GetIter`] precedes it: the operand is pushed as it stands and
    /// the op does its own `__await__` resolution.
    ///
    /// Two consequences of that split are worth stating, because they are what an implementer would
    /// otherwise have to infer: a coroutine driven to completion here delivers its RETURN value (not
    /// a yielded one), and an exception thrown in from the driving loop propagates into the awaited
    /// operand first, so a `try` inside the awaited coroutine sees it before this frame does.
    Await,
    /// A call with `*args` / `**kwargs` unpacking. `consts[kinds]` (a [`Const::ArgKinds`]) tags each
    /// argument slot (positional / `*` / keyword / `**`), and `consts[kwnames]` (a
    /// [`Const::KwNames`]) names the keyword-tagged slots in order. The stack holds
    /// `[callee, arg0 .. arg{n-1}]` for the `n` slots that tag names. The runtime flattens the
    /// `*`/`**` operands and binds the result; pushes the result.
    ///
    /// The argument COUNT is that tag list's length rather than a field of its own. It was once
    /// both, and the two could disagree -- a reader had to compare them and refuse a mismatch. Two
    /// spellings of one number is a state that can be malformed; one spelling cannot be.
    ///
    /// This is also the variant that sets the WIDTH OF EVERY OP. An enum is as wide as its
    /// widest variant, so three `u32` payloads here cost four bytes on all of them -- and in a real
    /// program this op is rare enough to be a rounding error while the ops it widened are not.
    CallEx {
        /// This site's entry in [`CodeObject::wide_operands`]: `[kinds, kwnames]`, the
        /// [`Const::ArgKinds`] and [`Const::KwNames`] indices.
        site: u32,
    },
    /// `del container[index]`. Pop the index, then the container, and delete the element (a step-1
    /// slice deletes a range). Pushes nothing.
    DeleteItem,
    /// `del object.attr` -- delete `object.<names[name]>`. Pop the object. Pushes nothing.
    DeleteAttr {
        /// The index into `names` of the attribute name.
        name: u32,
    },
    /// Push the value held by the cell at deref index `idx` (`cell[idx].get()`). The deref
    /// index space is `[0 .. cellvars.len())` -- this frame's OWN cells (locals captured by a
    /// nested function) -- then `[cellvars.len() .. cellvars.len() + freevars.len())` -- the cells
    /// CAPTURED from the enclosing function (free variables). One index space covers both. For
    /// reading a cell variable or a free variable.
    LoadDeref(u32),
    /// Pop a value and store it into the cell at deref index `idx` (`cell[idx].set(v)`). Same
    /// deref index space as [`Op::LoadDeref`]. For writing a cell variable (or, with `nonlocal`,
    /// a captured free variable), so the enclosing frame and the closure see one shared box.
    StoreDeref(u32),
    /// Push the CELL object itself at deref index `idx` (not its contents), to hand to a following
    /// [`Op::MakeFunction`] carrying the `CLOSURE` flag. Emitted once per free variable of the
    /// nested function, in that function's freevar order. For building a closure's captured cells.
    LoadClosure(u32),
    /// Install a fresh, empty class-body namespace dict as the frame's active namespace. Emitted at
    /// the start of a `class` body; [`Op::StoreName`] / [`Op::LoadName`] then target it, and
    /// [`Op::BuildClass`] consumes it. This is how a class body reads a name it just bound
    /// (`class C: a = 5; b = a + 1`, `@radius.setter`), which a plain dict-display cannot.
    SetupClassNamespace,
    /// Pop a value and bind it as `names[idx]` in the active class-body namespace (the class-body
    /// form of a name store). A member assignment / method definition in a `class` body.
    StoreName(u32),
    /// Push the value bound to `names[idx]`, resolving the active class-body namespace FIRST, then
    /// the module global / built-in namespace (a `NameError` if unbound). The class-body read that
    /// sees earlier class-body bindings and falls back outward, mirroring CPython's `LOAD_NAME`.
    LoadName(u32),
    /// Import the module named `names[idx]` and push the module object: resolve it (a cached
    /// `sys.modules` entry, else a native stdlib module, else `ModuleNotFoundError`), running its
    /// body once. Emitted for BOTH `import m` (the pushed module is then stored) and the lead-in of
    /// `from m import ...` (the module is the source for the following [`Op::ImportFrom`]s).
    ///
    /// A DOTTED name (`a.b`) is a single string here, and the module pushed is the one the string
    /// NAMES -- the leaf. Resolving it must import each ancestor first, so `a` is in the module
    /// table by the time `a.b` is. **`import a.b` therefore does NOT bind what this pushes**: it
    /// binds the ROOT, which the emitter gets by discarding the leaf and importing `a` again (a
    /// cache hit), because CPython binds the root and reaches `a.b` as an attribute of it.
    /// `import a.b as x` binds the leaf, so there the pushed module is used directly.
    ///
    /// Package machinery beyond that -- `__init__.py` bodies, package-relative imports -- is a
    /// later addition; a name that resolves to nothing is a `ModuleNotFoundError`, as in CPython.
    ImportName(u32),
    /// Read `names[idx]` off the module on top of the stack (WITHOUT popping it) and push the value
    /// -- `getattr(module, name)`, an `ImportError` if the module has no such member. For each name in
    /// `from m import a, b`; the frontend emits a following store, then one [`Op::PopTop`] after the
    /// last to discard the module.
    ImportFrom(u32),
    /// Pop the module on top of the stack and bind all of its public names into the CURRENT module's
    /// namespace (its `__all__` if defined, else every name not starting with `_`) -- `from m import
    /// *`. Takes no operand (the module carries its own name); the frontend emits it after an
    /// [`Op::ImportName`], and it consumes that module (no trailing `PopTop`).
    ImportStar,
    /// Pop a value and store it into the CURRENT module's globals under `names[idx]` -- the store
    /// counterpart of [`Op::LoadGlobal`] (CPython's `STORE_GLOBAL`). Emitted for an assignment to a
    /// name declared `global` inside a function, whose write must reach the module namespace rather
    /// than a frame local.
    StoreGlobal(u32),
}

/// A compile-time constant in a code object's constant pool. Every value the running
/// program needs that is not a name -- integers and the singletons -- is referenced
/// by [`Op::LoadConst`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Const {
    /// The singleton `None`.
    None,
    /// `True` or `False`.
    Bool(bool),
    /// An integer literal. The compiler keeps it in an `i64`; the interpreter materializes it as a
    /// tagged 31-bit fixnum, widening to a heap `long` (and arbitrary-precision bignum) as needed.
    Int(i64),
    /// A string literal (its decoded value); the interpreter materializes it as a heap `str`.
    ///
    /// A [`PyStr`] rather than a `String`, because a Python string's value space includes the
    /// surrogate code points and a `String`'s does not.
    Str(PyStr),
    /// A per-argument tag list for [`Op::CallEx`]: one byte per argument slot -- 0 positional,
    /// 1 `*` unpack, 2 keyword, 3 `**` unpack. Compile-time metadata, never a runtime value.
    ArgKinds(Vec<u8>),
    /// A compile-time tuple of keyword-argument names -- the kwnames a [`Op::CallKw`] references.
    /// Never a runtime Python value; only `CallKw` reads it out of the const pool.
    KwNames(Vec<String>),
    /// A floating-point literal -- the `f64` value's raw bits (`to_bits`), so `Const` stays `Eq`.
    /// The interpreter materializes it as a heap float; the typed AOT lane rejects it.
    Float(u64),
    /// An imaginary literal `Nj` -- the imaginary part's `f64` bits (real part is 0). The interpreter
    /// materializes it as a heap `complex` (when the `complex` feature is on); the typed lane rejects it.
    Imaginary(u64),
    /// An integer literal too large for `i64` -- its decimal digits (no sign, no separators). The
    /// interpreter materializes it as an arbitrary-precision `int` (bigint); the typed lane rejects it.
    BigInt(String),
    /// A bytes literal `b"..."` -- its raw bytes. The interpreter materializes a heap `bytes`; the
    /// typed lane rejects it.
    Bytes(Vec<u8>),
}

impl Const {
    /// The [`Capability`] an image must provide to MATERIALIZE this constant, or `None` for one every
    /// image can hold.
    ///
    /// This is the whole refusal set a [`Profile`] gives a compiler, and it is deliberately expressed
    /// on the CONSTANT rather than on the syntax that produced it: the front end refuses exactly what
    /// it cannot ENCODE and nothing it merely cannot PREDICT, and what can be encoded is a property
    /// of the constant pool. `1 / 3` is a float and is NOT here -- it can sit in
    /// a branch that never runs, and a compiler that refuses it turns a working program into one that
    /// will not build. The interpreter answers that case by name at the one choke point every
    /// float-producing path passes.
    #[must_use]
    pub fn required_capability(&self) -> Option<Capability> {
        match self {
            Const::Float(_) => Some(Capability::Float),
            Const::Imaginary(_) => Some(Capability::Complex),
            Const::None
            | Const::Bool(_)
            | Const::Int(_)
            | Const::Str(_)
            | Const::ArgKinds(_)
            | Const::KwNames(_)
            | Const::BigInt(_)
            | Const::Bytes(_) => None,
        }
    }
}

/// The first-light type lattice for an annotated value. Annotations (PEP 484), inert
/// at runtime in CPython, are honored here at compile time as the contract that
/// drives the typed fast path (the mypyc model). First light distinguishes only "a
/// machine integer" from "anything dynamic"; the lattice widens later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
#[repr(u8)]
pub enum StaticType {
    /// No usable static type: a boxed, dynamically-typed value. The default.
    #[default]
    Dynamic = 0,
    /// Annotated (or inferred) `int`: lowers to a machine integer on the typed path.
    /// First light maps it to MIR `i32` with bignum overflow deferred.
    Int = 1,
    /// Annotated (or inferred) `float`: lowers to a native `f64` on the typed path -- the shared
    /// MIR's F64, the same the C#/.NET `double` codegen targets. A dynamic (un-annotated) float is
    /// a heap object in the interpreter instead.
    Float = 2,
    /// A homogeneous `list` of `int`: a fixed packed array of machine integers on the typed path (a
    /// GC `ObjectRef` to `[u32 len][i32 elems...]`, the same array MIR the C# `int[]` lowering emits).
    /// The interpreter treats it as an ordinary dynamic list; this static type only drives the AOT
    /// lane's zero-runtime-call container path.
    ListInt = 3,
    /// A homogeneous `list` of `float`: a fixed packed array of `f64` elements on the typed path
    /// (`[u32 len][f64 elems...]`). The AOT-lane twin of [`StaticType::ListInt`]; the interpreter
    /// treats it as an ordinary dynamic list.
    ListFloat = 4,
    /// A homogeneous `tuple` of `int`: the SAME fixed packed array as [`StaticType::ListInt`] on the
    /// typed path, but IMMUTABLE -- `t[i] = v` is a `TypeError` (rejected by the lowering), so it is a
    /// distinct type the element-store path can refuse. The interpreter treats it as an ordinary
    /// dynamic tuple.
    TupleInt = 5,
    /// A homogeneous `tuple` of `float`: the immutable twin of [`StaticType::ListFloat`] (a fixed
    /// packed array of `f64` elements that rejects item assignment). The interpreter treats it as an
    /// ordinary dynamic tuple.
    TupleFloat = 6,
    /// A GROWABLE homogeneous `list` of `int`: a list the function `append`s to. On the typed path it
    /// is a small heap HEADER (`[i32 len][i32 cap][ObjectRef backing]`) whose backing is a resized
    /// packed array, so the list's identity is stable across a grow and aliases observe each other's
    /// appends. The interpreter treats it as an ordinary dynamic list.
    GrowListInt = 7,
    /// A GROWABLE homogeneous `list` of `float`: the `f64` twin of [`StaticType::GrowListInt`].
    GrowListFloat = 8,
}

impl StaticType {
    /// The type for a raw byte, or `None` if it is not defined.
    #[must_use]
    pub fn from_u8(byte: u8) -> Option<StaticType> {
        match byte {
            0 => Some(StaticType::Dynamic),
            1 => Some(StaticType::Int),
            2 => Some(StaticType::Float),
            3 => Some(StaticType::ListInt),
            4 => Some(StaticType::ListFloat),
            5 => Some(StaticType::TupleInt),
            6 => Some(StaticType::TupleFloat),
            7 => Some(StaticType::GrowListInt),
            8 => Some(StaticType::GrowListFloat),
            _ => None,
        }
    }
}

/// A function parameter: its name and its annotated type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    /// The parameter's name (it also occupies the matching leading local slot).
    pub name: String,
    /// The parameter's annotated type, or [`StaticType::Dynamic`] if unannotated.
    pub ty: StaticType,
}

/// Encode one line per op into the run-length form [`CodeObject::line_table`] holds.
///
/// Returns an EMPTY table when every line is 0 -- which is what a build that tracked no positions
/// produces, so such an artifact carries no line bytes at all rather than a table of zeroes.
#[must_use]
pub fn encode_line_table(lines: &[u32]) -> Vec<u8> {
    if lines.iter().all(|&l| l == 0) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut previous: i64 = 0;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let mut run = 1usize;
        while index + run < lines.len() && lines[index + run] == line {
            run += 1;
        }
        put_uvarint(&mut out, run as u64);
        put_svarint(&mut out, i64::from(line) - previous);
        previous = i64::from(line);
        index += run;
    }
    out
}

fn put_uvarint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_svarint(out: &mut Vec<u8>, value: i64) {
    put_uvarint(out, ((value << 1) ^ (value >> 63)) as u64);
}

fn take_uvarint(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(*at)?;
        *at += 1;
        value |= u64::from(byte & 0x7f).checked_shl(shift)?;
        if byte & 0x80 == 0 {
            return Some(value);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

fn take_svarint(bytes: &[u8], at: &mut usize) -> Option<i64> {
    let raw = take_uvarint(bytes, at)?;
    Some(((raw >> 1) as i64) ^ -((raw & 1) as i64))
}

/// A code object's identifiers, held as ONE text buffer plus a span per name rather than as a
/// `Vec<String>` of separately allocated strings.
///
/// The shape is chosen for the device rather than the host. A decoded bundle's names are numerous
/// and short -- for a program importing `asyncio` there are 383 of them averaging under nine
/// characters -- and the device allocator hands out SIZE CLASSES whose smallest is 16 bytes. So every
/// one of those names cost a 16-byte block plus a 12-byte `String` header on a 32-bit target, for a
/// mean of nine bytes of text. Interning them collapses that to one block for the text and one for
/// the spans.
///
/// The saving that matters is not only bytes. It is 296 fewer live allocations on that bundle, on a
/// part whose whole RAM is 128 KiB, and allocation COUNT is what a segregated allocator charges for
/// twice -- once in rounding and again in the fragmentation that outlives the block.
///
/// It reads like a `Vec<String>` on purpose: indexing yields `str`, so `&pool[i]` is a `&str` and the
/// consumers that only ever read a name did not move when this replaced the `Vec`.
///
/// # Why each pool owns its own buffer
///
/// One shared buffer per MODULE, with every pool holding spans into it, is the obvious next step and
/// it was measured rather than assumed: **85 fewer allocations and 304 bytes** on a bundle importing
/// `asyncio`. It is not worth what it costs. Sharing needs the buffer behind an `Rc`, which takes
/// `Send` off every bytecode type and the threading model needs it; equality has to become
/// content-based, because the compiler's offsets and the decoder's differ for identical names; and
/// the decoder grows a module-level buffer threaded through three signatures.
///
/// Freezing these two fields instead -- `Box` rather than `String`/`Vec` -- returned **2,752 bytes**
/// on the same bundle for none of that, because a code object has FOUR pools and most of them are
/// empty, so 176 of them pay for a capacity word they will never use. Nine times the bytes, no shared
/// ownership, and the pool stays self-contained, which is what let another crate compile against
/// interning without changing a line.
///
/// Recorded here rather than in a post so the next person to have the idea finds the number.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct NamePool {
    /// Every name's bytes, concatenated in index order.
    ///
    /// Boxed rather than a `String`, and that is worth four bytes per pool: a `String` carries a
    /// CAPACITY it will never use again, because a pool is built once and then only read. Four
    /// bytes sounds like nothing until you count the pools -- a code object has FOUR, most of them
    /// empty, so a 44-code-object bundle carries 176 of them and pays for every one.
    text: Box<str>,
    /// `(offset, length)` into `text`, one per name. Boxed for the same reason.
    spans: Box<[(u32, u32)]>,
}

impl NamePool {
    /// An empty pool, allocating nothing.
    #[must_use]
    pub fn new() -> NamePool {
        NamePool::default()
    }

    /// The number of names.
    #[must_use]
    pub fn len(&self) -> usize {
        self.spans.len()
    }

    /// Whether the pool holds no names.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }

    /// The name at `index`, or `None` when it is out of range.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&str> {
        let &(offset, length) = self.spans.get(index)?;
        self.text
            .get(offset as usize..offset as usize + length as usize)
    }

    /// Freeze a built-up text buffer and span table into a pool, releasing the capacity both grew.
    fn freeze(text: String, spans: Vec<(u32, u32)>) -> NamePool {
        NamePool {
            text: text.into_boxed_str(),
            spans: spans.into_boxed_slice(),
        }
    }

    /// The names in index order.
    pub fn iter(&self) -> impl Iterator<Item = &str> + '_ {
        (0..self.spans.len()).map(|i| self.index_or_empty(i))
    }

    /// The index of the first name equal to `name`, or `None`.
    #[must_use]
    pub fn position(&self, name: &str) -> Option<usize> {
        self.iter().position(|n| n == name)
    }

    /// Whether the pool holds `name`.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.position(name).is_some()
    }

    /// The spans are written by `push` alone, so every one of them is in range and on a character
    /// boundary; a missing span would mean the pool's own invariant had been broken rather than that
    /// the caller asked for something reasonable.
    fn index_or_empty(&self, index: usize) -> &str {
        self.get(index).unwrap_or("")
    }
}

impl core::ops::Index<usize> for NamePool {
    type Output = str;

    fn index(&self, index: usize) -> &str {
        self.index_or_empty(index)
    }
}

impl core::fmt::Debug for NamePool {
    /// Prints as the list of names it stands for, so a failing assertion reads like the `Vec<String>`
    /// this replaced rather than exposing offsets nobody wrote by hand.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<S: AsRef<str>> FromIterator<S> for NamePool {
    /// The one way a pool is built. It fills a growable buffer and then FREEZES it, which is not
    /// tidiness: `String` and `Vec` grow geometrically, so a pool filled name by name holds close to
    /// twice the bytes its text needs -- and a decoded bundle keeps every pool for the whole
    /// program, so that slack would be permanent rather than transient.
    fn from_iter<I: IntoIterator<Item = S>>(names: I) -> NamePool {
        let names = names.into_iter();
        let mut text = String::new();
        let mut spans: Vec<(u32, u32)> = Vec::with_capacity(names.size_hint().0);
        for name in names {
            let name = name.as_ref();
            spans.push((text.len() as u32, name.len() as u32));
            text.push_str(name);
        }
        NamePool::freeze(text, spans)
    }
}

impl<'a> IntoIterator for &'a NamePool {
    type Item = &'a str;
    type IntoIter = alloc::boxed::Box<dyn Iterator<Item = &'a str> + 'a>;

    fn into_iter(self) -> Self::IntoIter {
        alloc::boxed::Box::new(self.iter())
    }
}

impl CodeObject {
    /// The source line of the op at `index`, or `None` when this artifact carries no line
    /// information -- which is the honest answer for an older build, not a guess of 0.
    ///
    /// Scans the run-length table from the start. That is deliberate: a traceback formats a handful
    /// of frames once, on the way to reporting an error a program is already failing on, so the scan
    /// is paid at the only moment anyone wants it. An index to make this O(log n) would cost RAM on
    /// every program including the ones that never raise.
    #[must_use]
    pub fn line_for(&self, index: usize) -> Option<u32> {
        let mut at = 0usize;
        let mut line: i64 = 0;
        let mut first = 0usize;
        while at < self.line_table.len() {
            let run = take_uvarint(&self.line_table, &mut at)? as usize;
            line += take_svarint(&self.line_table, &mut at)?;
            if index < first + run {
                return u32::try_from(line).ok();
            }
            first += run;
        }
        None
    }
}

/// A compiled code object: the bytecode and tables for one function (or the module's
/// top-level body). It is what the interpreter executes and what the typed lowering
/// consumes. The interpreter ignores the typing fields (`params`/`ret_ty`/
/// `local_types`); the lowering uses them to drive the typed fast path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeObject {
    /// The function's name, or `"<module>"` for a module's top-level body.
    pub name: String,
    /// The parameters, in order (`[positional-only | positional-or-keyword | keyword-only]`). They
    /// occupy the first `params.len()` local slots.
    pub params: Vec<Param>,
    /// How many leading `params` are positional-only (bindable only by position). 0 until the
    /// positional-only `/` marker is supported.
    pub posonly_count: u32,
    /// How many trailing `params` are keyword-only (bindable only by name, after a `*`). 0 until
    /// keyword-only parameters are supported.
    pub kwonly_count: u32,
    /// Whether this is a generator function (its body contains `yield`). A CALL of a generator
    /// function does not run the body -- it returns a generator object; the body runs on `next`.
    pub is_generator: bool,
    /// Whether this is a coroutine function (an `async def`). A CALL does not run the body -- it
    /// returns a coroutine object, which runs when it is awaited or scheduled.
    ///
    /// INDEPENDENT of `is_generator`, not a refinement of it: an `async def` with no `yield` sets
    /// this alone, matching CPython, where `CO_COROUTINE` and `CO_GENERATOR` are separate flags. A
    /// body that sets BOTH would be an async generator (CPython gives that its own third flag);
    /// nothing emits that pair today, because `yield` inside an `async def` is refused at compile
    /// time rather than compiled as something else.
    pub is_coroutine: bool,
    /// Whether the parameter list has a `*args` slot -- the param at index
    /// `params.len() - kwonly_count - (has_varkwargs as usize) - 1`, which collects surplus
    /// positional arguments into a tuple. When set, extra positionals are NOT a TypeError.
    pub has_varargs: bool,
    /// Whether the parameter list has a `**kwargs` slot (the last param), which collects
    /// unmatched keyword arguments into a dict. 0 until `**kwargs` is supported.
    pub has_varkwargs: bool,
    /// The return annotation, or [`StaticType::Dynamic`] if unannotated.
    pub ret_ty: StaticType,
    /// The total number of local-variable slots (parameters first, then the
    /// function's other assigned names). [`Op::LoadFast`] / [`Op::StoreFast`] index
    /// this range.
    pub n_locals: usize,
    /// The name of each local slot, indexed by slot number; `local_names.len() ==
    /// n_locals`. Kept for diagnostics and for the typed lowering.
    pub local_names: NamePool,
    /// The locals of this function that a nested function captures, so they must live in heap
    /// cells rather than plain slots. Their order defines the low half of the deref index space
    /// (`0 .. cellvars.len()`), which [`Op::LoadDeref`] / [`Op::StoreDeref`] / [`Op::LoadClosure`]
    /// index. Empty for a function that no nested function reads. A cellvar that is also a
    /// parameter has its bound argument copied into its fresh cell at frame setup.
    pub cellvars: NamePool,
    /// The names this function uses that are cellvars of an enclosing function -- reached through
    /// captured cells, not this frame's own locals. Their order continues the deref index space
    /// after `cellvars` (`cellvars.len() .. cellvars.len() + freevars.len()`). A closure built with
    /// [`Op::MakeFunction`]'s `CLOSURE` flag receives exactly `freevars.len()` cells in this order.
    pub freevars: NamePool,
    /// The annotated/inferred type of each local slot, indexed by slot number;
    /// `local_types.len() == n_locals`. Drives the typed fast path.
    pub local_types: Vec<StaticType>,
    /// The constant pool, indexed by [`Op::LoadConst`].
    pub consts: Vec<Const>,
    /// The attribute/global name pool, indexed by [`Op::LoadAttr`] and
    /// [`Op::LoadGlobal`].
    pub names: NamePool,
    /// The instructions, in order.
    pub ops: Vec<Op>,
    /// The body's docstring: a leading bare-string statement, else `None`. It rides the code
    /// object because a function object is built at its def site rather than by running a body, so
    /// this is where `__doc__` can be read from -- the same place `__name__` and `__qualname__`
    /// already come from. A MODULE and a CLASS bind their own `__doc__` as a name instead, since
    /// both have a namespace to bind it into.
    pub doc: Option<String>,
    /// The source line of each op, run-length encoded -- EMPTY when this artifact carries none.
    ///
    /// Held as BYTES and scanned on demand rather than decoded into a table, because that is what it
    /// is for: nothing reads a line until an exception is being formatted. Decoding it into one entry
    /// per run would cost roughly three times these bytes in RAM, permanently, for data a program
    /// that never raises will never look at.
    ///
    /// The encoding is a sequence of `(run, delta)` pairs: `run` is how many consecutive ops share
    /// the line, as an unsigned varint, and `delta` is the change from the previous line, zigzag
    /// signed -- a line runs backwards as often as a loop does. Roughly four ops share an entry in
    /// real code, which is what makes this about half a byte per op rather than four.
    pub line_table: Vec<u8>,
    /// How many inline-cache slots a running frame allocates for this code: the count
    /// The two-word operands of the ops too wide to carry them inline, one entry per such site.
    ///
    /// An enum is as wide as its widest variant, so a variant holding two `u32`s sets the size of
    /// EVERY op in the array -- and the ops that pay for it are the common ones while the ones that
    /// need it are not. Moving those payloads here leaves every variant at a single word, which took
    /// `size_of::<Op>()` from 12 bytes to 8: on a bundle importing `asyncio` that is 1,263 ops and
    /// about 5 KiB of device RAM, against a table of 136 entries.
    ///
    /// It is NOT on the wire. Each op still encodes its own pair inline exactly as before, and the
    /// table is rebuilt while decoding -- so this costs no format version and no deployed artifact.
    ///
    /// The index is assigned in ascending static op order by whatever built the array, and the
    /// decoder reproduces that by appending as it reads. Nothing else may reorder the ops without
    /// rebuilding this alongside them.
    pub wide_operands: Vec<[u32; 2]>,
    /// of cacheable sites (each [`Op::LoadAttr`]), numbered in ascending static order.
    pub cache_count: usize,
    /// The exception table: covering `[start, end)` op ranges mapped to a handler op
    /// index, innermost first. Empty for a function with no `try`. A raise searches it
    /// for the tightest entry covering the faulting op; the try body itself costs nothing.
    pub exc_table: Vec<ExcEntry>,
}

/// One entry in a [`CodeObject::exc_table`]: a protected op range and where to go when an
/// exception is raised within it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExcEntry {
    /// First op index of the protected (try-body) range.
    pub start: u32,
    /// One past the last op index of the protected range (`[start, end)`).
    pub end: u32,
    /// The handler's op index -- where an in-range raise jumps.
    pub target: u32,
    /// The value-stack depth to truncate to before entering the handler.
    pub depth: u32,
}

/// A module's function table, addressed by index and QUERYABLE BY NAME WITHOUT READING A BODY.
///
/// **The split between what this answers cheaply and what it answers by materialising is the whole
/// reason it is a type rather than a `Vec`.** A module's functions are reached two ways and only one
/// of them needs a body:
///
/// - **By NAME, to find out whether a function exists and at what index** -- resolving a global,
///   building a module's namespace on import, and the front end's own "is there already a function
///   called this" checks. [`Functions::names`], [`Functions::name`] and [`Functions::position`]
///   serve these and are guaranteed never to need a body.
/// - **By INDEX, to CALL it** -- [`Functions::get`] and `Index`, which do need one.
///
/// Every caller in both crates was measured before this shape was chosen: of the front end's 54 uses
/// about 29 are `iter().any(|f| f.name == ..)`-shaped, and BOTH of the interpreter's scans
/// (`resolve_global`'s lookup and `build_module_namespace`'s import-time walk) read `name` and
/// nothing else. **A container that offered only `iter()` would therefore have forced every body in
/// a module to materialise on that module's first global lookup**, which is the one access pattern
/// that would make a lazy body worth nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Functions {
    /// Every function's code object, materialised.
    eager: Vec<CodeObject>,
}

impl Functions {
    /// How many functions the module defines.
    #[must_use]
    pub fn len(&self) -> usize {
        self.eager.len()
    }

    /// Whether the module defines none.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.eager.is_empty()
    }

    /// Function `index`'s name, or `None` if there is no such function. **Never needs a body.**
    #[must_use]
    pub fn name(&self, index: usize) -> Option<&str> {
        self.eager.get(index).map(|code| code.name.as_str())
    }

    /// Every function's name, in table order. **Never needs a body**, so an import-time walk that
    /// binds each name to its index costs the names alone.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.eager.iter().map(|code| code.name.as_str())
    }

    /// The index of the FIRST function with this name, or `None`. **Never needs a body.**
    ///
    /// First rather than only: a module may rebind a name, and the front end emits both definitions.
    /// Taking the first matches what a positional scan over the table did before this type existed.
    #[must_use]
    pub fn position(&self, name: &str) -> Option<usize> {
        self.eager.iter().position(|code| code.name == name)
    }

    /// Function `index`'s code object, or `None`. **Materialises the body.**
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&CodeObject> {
        self.eager.get(index)
    }

    /// Appends a function -- the front end building a module up as it compiles.
    pub fn push(&mut self, code: CodeObject) {
        self.eager.push(code);
    }

    /// Every code object, materialised, for a consumer that genuinely needs them all: the encoder
    /// writing the table out, and the differential harness comparing two of them.
    ///
    /// **Named for what it costs.** A caller that wants names wants [`Functions::names`].
    pub fn iter_bodies(&self) -> impl Iterator<Item = &CodeObject> {
        self.eager.iter()
    }

    /// The same, mutably: the front end rewriting a table in place (a fold, a late binding).
    /// Producer-side only -- a materialised table is the only kind there is on that side.
    pub fn iter_bodies_mut(&mut self) -> impl Iterator<Item = &mut CodeObject> {
        self.eager.iter_mut()
    }

    /// Every code object as an owned vector. **Materialises the whole table**, so it is the one call
    /// here that a deferred table cannot serve cheaply.
    ///
    /// It exists because the interpreter's module handle is an `Rc<[CodeObject]>` -- a CONTIGUOUS
    /// slice, which by construction cannot hold anything unmaterialised. Every caller of this is
    /// therefore a place that has to become `Rc<Functions>` before a deferred body is worth
    /// anything, and there are exactly two of them.
    #[must_use]
    pub fn into_bodies(self) -> Vec<CodeObject> {
        self.eager
    }

    /// The table as a contiguous slice. **Materialises the whole table**, and is the borrowed twin of
    /// [`Functions::into_bodies`] with the same one-way property: a slice is contiguous, so a table
    /// holding anything unmaterialised cannot produce one.
    ///
    /// It exists for the callers that hand a whole function table to something typed
    /// `&[CodeObject]` -- the interpreter's `run_module`/`run` seam and the harnesses around it.
    #[must_use]
    pub fn as_bodies(&self) -> &[CodeObject] {
        &self.eager
    }
}

impl From<Vec<CodeObject>> for Functions {
    fn from(eager: Vec<CodeObject>) -> Functions {
        Functions { eager }
    }
}

impl FromIterator<CodeObject> for Functions {
    fn from_iter<I: IntoIterator<Item = CodeObject>>(iter: I) -> Functions {
        Functions { eager: iter.into_iter().collect() }
    }
}

impl core::ops::Index<usize> for Functions {
    type Output = CodeObject;

    /// **Materialises the body.** Panics on an out-of-range index, as a slice does.
    fn index(&self, index: usize) -> &CodeObject {
        &self.eager[index]
    }
}

/// A compiled module: its top-level function definitions plus the code object for its
/// top-level statements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    /// The module's name (for diagnostics; e.g. the source stem).
    pub name: String,
    /// The functions defined at module scope, in source order.
    pub functions: Functions,
    /// The `"<module>"` code object: the top-level statements, run on import. Never part of
    /// [`Functions`]: an import ALWAYS enters it, so it is the one code object that is never the
    /// deferred case.
    pub body: CodeObject,
}


/// Why decoding a serialized [`Module`] failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The data ran out before a field was complete.
    UnexpectedEof,
    /// The leading four bytes were not [`MAGIC`].
    BadMagic,
    /// The format version is not one this build understands.
    UnsupportedVersion(u16),
    /// A tagged union (an [`Op`], [`Const`], [`StaticType`], ...) held an unknown tag.
    BadTag(&'static str, u8),
    /// A string field was not valid UTF-8.
    BadUtf8,
    /// The artifact requires capabilities this reader does not implement. Carries what it asked for
    /// and the subset that is missing, so the refusal names the gap rather than only reporting one.
    UnsupportedFeatures {
        /// Everything the artifact declared it needs.
        required: FeatureFlags,
        /// The bits of `required` this build does not implement.
        missing: FeatureFlags,
    },
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            DecodeError::UnexpectedEof => f.write_str("unexpected end of bytecode"),
            DecodeError::BadMagic => f.write_str("not a Lamella Python bytecode module (bad magic)"),
            DecodeError::UnsupportedVersion(v) => {
                write!(f, "unsupported bytecode format version {v}")
            }
            DecodeError::UnsupportedFeatures { required, missing } => write!(
                f,
                "the artifact requires capabilities this build does not implement:                  required {:#06x}, missing {:#06x}",
                required.0, missing.0
            ),
            DecodeError::BadTag(what, tag) => write!(f, "invalid {what} tag {tag}"),
            DecodeError::BadUtf8 => f.write_str("invalid UTF-8 in bytecode string"),
        }
    }
}

fn put_u16(buf: &mut Vec<u8>, v: u16) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_u32(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn put_len(buf: &mut Vec<u8>, n: usize) {
    put_u32(buf, n as u32);
}

fn put_str(buf: &mut Vec<u8>, s: &str) {
    put_len(buf, s.len());
    buf.extend_from_slice(s.as_bytes());
}

fn put_const(buf: &mut Vec<u8>, c: &Const) {
    match c {
        Const::None => buf.push(0),
        Const::Bool(b) => {
            buf.push(1);
            buf.push(u8::from(*b));
        }
        Const::Int(v) => {
            buf.push(2);
            buf.extend_from_slice(&v.to_le_bytes());
        }
        Const::Str(s) => {
            buf.push(3);
            put_len(buf, s.as_bytes().len());
            buf.extend_from_slice(s.as_bytes());
        }
        Const::KwNames(names) => {
            buf.push(4);
            put_len(buf, names.len());
            for n in names {
                put_str(buf, n);
            }
        }
        Const::Float(bits) => {
            buf.push(5);
            buf.extend_from_slice(&bits.to_le_bytes());
        }
        Const::Imaginary(bits) => {
            buf.push(6);
            buf.extend_from_slice(&bits.to_le_bytes());
        }
        Const::ArgKinds(kinds) => {
            buf.push(7);
            put_len(buf, kinds.len());
            buf.extend_from_slice(kinds);
        }
        Const::BigInt(digits) => {
            buf.push(8);
            put_str(buf, digits);
        }
        Const::Bytes(data) => {
            buf.push(9);
            put_len(buf, data.len());
            buf.extend_from_slice(data);
        }
    }
}

fn put_op(buf: &mut Vec<u8>, op: &Op, wide: &[[u32; 2]]) {
    match op {
        Op::LoadConst(i) => {
            buf.push(0);
            put_u32(buf, *i);
        }
        Op::LoadFast(i) => {
            buf.push(1);
            put_u32(buf, *i);
        }
        Op::StoreFast(i) => {
            buf.push(2);
            put_u32(buf, *i);
        }
        Op::LoadGlobal(i) => {
            buf.push(3);
            put_u32(buf, *i);
        }
        Op::LoadAttr { site } => {
            buf.push(4);
            let [name, cache] = wide[*site as usize];
            put_u32(buf, name);
            put_u32(buf, cache);
        }
        Op::Binary(b) => {
            buf.push(5);
            buf.push(*b as u8);
        }
        Op::InplaceBinOp(b) => {
            buf.push(56);
            buf.push(*b as u8);
        }
        Op::Compare(c) => {
            buf.push(6);
            buf.push(*c as u8);
        }
        Op::Unary(u) => {
            buf.push(12);
            buf.push(*u as u8);
        }
        Op::PopTop => buf.push(7),
        Op::Jump(t) => {
            buf.push(8);
            put_u32(buf, *t);
        }
        Op::PopJumpIfFalse(t) => {
            buf.push(9);
            put_u32(buf, *t);
        }
        Op::Call(argc) => {
            buf.push(10);
            put_u32(buf, *argc);
        }
        Op::Return => buf.push(11),
        Op::Subscript { cache } => {
            buf.push(13);
            put_u32(buf, *cache);
        }
        Op::BuildSlice => buf.push(14),
        Op::BuildList(count) => {
            buf.push(15);
            put_u32(buf, *count);
        }
        Op::BuildTuple(count) => {
            buf.push(16);
            put_u32(buf, *count);
        }
        Op::BuildDict(count) => {
            buf.push(17);
            put_u32(buf, *count);
        }
        Op::GetIter => buf.push(18),
        Op::ForIter(target) => {
            buf.push(19);
            put_u32(buf, *target);
        }
        Op::Setitem => buf.push(20),
        Op::Contains { negate } => {
            buf.push(21);
            buf.push(*negate as u8);
        }
        Op::Raise(argc) => {
            buf.push(24);
            buf.push(*argc);
        }
        Op::MatchExc => buf.push(25),
        Op::LoadExc => buf.push(26),
        Op::PopExcept => buf.push(27),
        Op::Reraise => buf.push(28),
        Op::DeleteFast(slot) => {
            buf.push(29);
            put_u32(buf, *slot);
        }
        Op::MakeFunction { func, flags } => {
            buf.push(30);
            put_u32(buf, *func);
            buf.push(*flags);
        }
        Op::BuildClass => buf.push(31),
        Op::BuildClassKw { kwnames } => {
            buf.push(57);
            put_u32(buf, *kwnames);
        }
        Op::SetAttr { site } => {
            buf.push(32);
            let [name, cache] = wide[*site as usize];
            put_u32(buf, name);
            put_u32(buf, cache);
        }
        Op::UnpackSequence(count) => {
            buf.push(33);
            put_u32(buf, *count);
        }
        Op::ListAppend => buf.push(34),
        Op::SetAdd => buf.push(35),
        Op::DictInsert => buf.push(36),
        Op::LoadSuper(name) => {
            buf.push(37);
            put_u32(buf, *name);
        }
        Op::BuildSet(count) => {
            buf.push(38);
            put_u32(buf, *count);
        }
        Op::UnpackEx { site } => {
            buf.push(39);
            let [before, after] = wide[*site as usize];
            put_u32(buf, before);
            put_u32(buf, after);
        }
        Op::CallKw { site } => {
            buf.push(40);
            let [argc, kwnames] = wide[*site as usize];
            put_u32(buf, argc);
            put_u32(buf, kwnames);
        }
        Op::Yield => buf.push(41),
        Op::YieldFrom => buf.push(54),
        Op::Await => buf.push(58),
        Op::ListGrow { list } => {
            buf.push(59);
            put_u32(buf, *list);
        }
        Op::ImportStar => buf.push(55),
        Op::CallEx { site } => {
            buf.push(42);
            let [kinds, kwnames] = wide[*site as usize];
            put_u32(buf, kinds);
            put_u32(buf, kwnames);
        }
        Op::DeleteItem => buf.push(43),
        Op::DeleteAttr { name } => {
            buf.push(44);
            put_u32(buf, *name);
        }
        Op::LoadDeref(i) => {
            buf.push(45);
            put_u32(buf, *i);
        }
        Op::StoreDeref(i) => {
            buf.push(46);
            put_u32(buf, *i);
        }
        Op::LoadClosure(i) => {
            buf.push(47);
            put_u32(buf, *i);
        }
        Op::SetupClassNamespace => buf.push(48),
        Op::StoreName(name) => {
            buf.push(49);
            put_u32(buf, *name);
        }
        Op::LoadName(name) => {
            buf.push(50);
            put_u32(buf, *name);
        }
        Op::ImportName(name) => {
            buf.push(51);
            put_u32(buf, *name);
        }
        Op::ImportFrom(name) => {
            buf.push(52);
            put_u32(buf, *name);
        }
        Op::StoreGlobal(name) => {
            buf.push(53);
            put_u32(buf, *name);
        }
    }
}

fn put_code_object(buf: &mut Vec<u8>, co: &CodeObject) {
    put_str(buf, &co.name);
    put_len(buf, co.params.len());
    for p in &co.params {
        put_str(buf, &p.name);
        buf.push(p.ty as u8);
    }
    put_u32(buf, co.posonly_count);
    put_u32(buf, co.kwonly_count);
    buf.push(CodeFlags::of(co).0);
    buf.push(co.ret_ty as u8);
    put_len(buf, co.n_locals);
    put_len(buf, co.local_names.len());
    for n in &co.local_names {
        put_str(buf, n);
    }
    put_len(buf, co.cellvars.len());
    for n in &co.cellvars {
        put_str(buf, n);
    }
    put_len(buf, co.freevars.len());
    for n in &co.freevars {
        put_str(buf, n);
    }
    put_len(buf, co.local_types.len());
    for t in &co.local_types {
        buf.push(*t as u8);
    }
    put_len(buf, co.consts.len());
    for c in &co.consts {
        put_const(buf, c);
    }
    put_len(buf, co.names.len());
    for n in &co.names {
        put_str(buf, n);
    }
    match &co.doc {
        Some(text) => {
            buf.push(1);
            put_str(buf, text);
        }
        None => buf.push(0),
    }
    put_len(buf, co.cache_count);
    put_len(buf, co.ops.len());
    for op in &co.ops {
        put_op(buf, op, &co.wide_operands);
    }
    put_len(buf, co.exc_table.len());
    for e in &co.exc_table {
        put_u32(buf, e.start);
        put_u32(buf, e.end);
        put_u32(buf, e.target);
        put_u32(buf, e.depth);
    }
}

/// Write a module's CONTENT (name + functions + body) with no container header -- shared by a bare
/// [`Module`] and each module inside a [`Bundle`].
fn put_module_content(buf: &mut Vec<u8>, module: &Module) {
    put_str(buf, &module.name);
    put_len(buf, module.functions.len());
    for f in module.functions.iter_bodies() {
        put_code_object(buf, f);
    }
    put_code_object(buf, &module.body);
    put_debug_section(buf, module);
}

/// Write the module's debug section: one line table per code object, in the order the code objects
/// were just written (functions, then the body).
///
/// This fills the debug section the format reserves WITHOUT moving [`FORMAT_VERSION`], which is the whole
/// reason the four bytes were spent: a reader built before line tables existed skips the section by
/// its declared length and loses only diagnostics it never had.
fn put_debug_section(buf: &mut Vec<u8>, module: &Module) {
    let tables: Vec<&[u8]> = module
        .functions
        .iter_bodies()
        .chain(core::iter::once(&module.body))
        .map(|co| co.line_table.as_slice())
        .collect();
    if tables.iter().all(|t| t.is_empty()) {
        put_len(buf, 0);
        return;
    }
    let mut section = Vec::new();
    put_len(&mut section, tables.len());
    for table in tables {
        put_len(&mut section, table.len());
        section.extend_from_slice(table);
    }
    put_len(buf, section.len());
    buf.extend_from_slice(&section);
}

/// Read a debug section into one line table per code object, positionally.
///
/// LENIENT BY DESIGN. Anything unexpected -- a count that does not match the code objects, a
/// truncated table, trailing bytes -- yields NO line information rather than an error or a partial
/// assignment. A reader that cannot make sense of a debug section has to degrade to the state it was
/// in before the section existed: WRONG lines are worse than none, and refusing to load a runnable
/// program because its diagnostics are malformed is worse still.
fn take_debug_section(section: &[u8], code_objects: usize) -> Vec<Vec<u8>> {
    let none = || alloc::vec![Vec::new(); code_objects];
    if section.is_empty() {
        return none();
    }
    let mut at = 0usize;
    let u32_at = |at: &mut usize| -> Option<usize> {
        let bytes = section.get(*at..*at + 4)?;
        *at += 4;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
    };
    let Some(count) = u32_at(&mut at) else {
        return none();
    };
    if count != code_objects {
        return none();
    }
    let mut tables = Vec::with_capacity(count);
    for _ in 0..count {
        let Some(len) = u32_at(&mut at) else {
            return none();
        };
        let Some(bytes) = section.get(at..at + len) else {
            return none();
        };
        at += len;
        tables.push(bytes.to_vec());
    }
    if at != section.len() {
        return none();
    }
    tables
}


impl Module {
    /// Drop every code object's line information, so this module encodes with no debug section.
    ///
    /// Line tables are not free and a device profile may not want them: on a bundle importing
    /// `asyncio` they are about 709 bytes of wire and 1,557 of decoded structure, the difference
    /// being that each table is its own allocation. A build that will never format a traceback --
    /// no REPL, no host attached, a program whose only failure mode is a trap -- can spend neither.
    ///
    /// It is a method rather than a compiler flag on purpose. Whether a PROFILE strips line tables
    /// is a knob question that spans three languages rather than one front end; this method is the
    /// mechanism such a knob would drive, available now and committing no caller to it.
    pub fn strip_line_tables(&mut self) {
        for function in self.functions.iter_bodies_mut() {
            function.line_table = Vec::new();
        }
        self.body.line_table = Vec::new();
    }

    /// Serialize this module to the versioned binary container.
    #[must_use]
    pub fn encode(&self, features: FeatureFlags) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        put_u16(&mut buf, FORMAT_VERSION);
        put_u16(&mut buf, features.0);
        put_module_content(&mut buf, self);
        buf
    }

    /// Decode a module from the versioned binary container, also returning the
    /// feature flags the artifact declared.
    pub fn decode(data: &[u8]) -> Result<(Module, FeatureFlags), DecodeError> {
        let mut r = Reader { data, pos: 0 };
        if r.bytes(4)? != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let version = r.u16()?;
        if version != FORMAT_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let features = FeatureFlags(r.u16()?);
        let missing = features.missing_from(SUPPORTED_FEATURES);
        if !missing.is_empty() {
            return Err(DecodeError::UnsupportedFeatures {
                required: features,
                missing,
            });
        }
        let module = r.module_content()?;
        Ok((module, features))
    }
}

/// Set in a container's version word when the payload is a [`Bundle`] rather than a bare [`Module`],
/// leaving the low 15 bits to carry [`FORMAT_VERSION`]. One word still says both things, so a reader
/// tells the two containers apart exactly as before.
const BUNDLE_KIND_BIT: u16 = 0x8000;

/// The binary format version of a [`Bundle`] container -- a program's entry module plus its
/// importable managed (Python-authored) modules.
///
/// It is DERIVED from [`FORMAT_VERSION`] rather than chosen, and that is the whole point. A bundle's
/// entry and modules are written by the same writer a bare module uses, so [`FORMAT_VERSION`] is what
/// identifies the layout of a bundle's payload. While the two numbers were independent, nothing made
/// anyone move this one when that one moved -- and nothing did: the module layout advanced through
/// several byte-level changes while this stayed put, so a bundle declared a version that described
/// its container and said nothing about its contents. A reader compares for strict equality precisely
/// to refuse a stale payload, and on the bundle path there was no number for it to refuse.
///
/// Deriving it closes that by construction rather than by remembering: a bump to [`FORMAT_VERSION`]
/// moves this in the same edit, and the two cannot drift apart again.
pub const BUNDLE_FORMAT_VERSION: u16 = FORMAT_VERSION | BUNDLE_KIND_BIT;

/// A compiled multi-module program: the `entry` module (run at startup) plus the importable managed
/// modules an `import` resolves to (by `name`). The Python analog of a corlib bundle.
#[derive(Debug, Clone, PartialEq)]
pub struct Bundle {
    /// The program's main module -- run at startup.
    pub entry: Module,
    /// The importable managed modules, resolved by their `name`.
    pub modules: Vec<Module>,
}

impl Bundle {
    /// Drop every module's line information, so this bundle encodes with no debug sections.
    ///
    /// The bundle form is the one a device is actually sent, so this is the level the decision gets
    /// made at: a bundle carries its entry and every module its imports reach, and stripping one
    /// module while leaving another would give a traceback that names a line in some frames and not
    /// others. See [`Module::strip_line_tables`] for what it costs and why it is a method rather
    /// than a compiler flag.
    pub fn strip_line_tables(&mut self) {
        self.entry.strip_line_tables();
        for module in &mut self.modules {
            module.strip_line_tables();
        }
    }

    /// Serialize this bundle to the versioned binary container (magic + [`BUNDLE_FORMAT_VERSION`]).
    #[must_use]
    pub fn encode(&self, features: FeatureFlags) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&MAGIC);
        put_u16(&mut buf, BUNDLE_FORMAT_VERSION);
        put_u16(&mut buf, features.0);
        put_module_content(&mut buf, &self.entry);
        put_len(&mut buf, self.modules.len());
        for m in &self.modules {
            put_module_content(&mut buf, m);
        }
        buf
    }

    /// Decode a bundle from the versioned binary container, also returning the declared feature flags.
    pub fn decode(data: &[u8]) -> Result<(Bundle, FeatureFlags), DecodeError> {
        let mut r = Reader { data, pos: 0 };
        if r.bytes(4)? != MAGIC {
            return Err(DecodeError::BadMagic);
        }
        let version = r.u16()?;
        if version != BUNDLE_FORMAT_VERSION {
            return Err(DecodeError::UnsupportedVersion(version));
        }
        let features = FeatureFlags(r.u16()?);
        let missing = features.missing_from(SUPPORTED_FEATURES);
        if !missing.is_empty() {
            return Err(DecodeError::UnsupportedFeatures {
                required: features,
                missing,
            });
        }
        let entry = r.module_content()?;
        let n_modules = r.u32()? as usize;
        let mut modules = Vec::with_capacity(n_modules);
        for _ in 0..n_modules {
            modules.push(r.module_content()?);
        }
        Ok((Bundle { entry, modules }, features))
    }
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let end = self.pos.checked_add(n).ok_or(DecodeError::UnexpectedEof)?;
        let slice = self.data.get(self.pos..end).ok_or(DecodeError::UnexpectedEof)?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, DecodeError> {
        Ok(self.bytes(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, DecodeError> {
        let b = self.bytes(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, DecodeError> {
        let b = self.bytes(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn i64(&mut self) -> Result<i64, DecodeError> {
        let b = self.bytes(8)?;
        Ok(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn string(&mut self) -> Result<String, DecodeError> {
        self.str_slice().map(String::from)
    }

    /// A length-prefixed name, borrowed out of the artifact rather than copied.
    fn str_slice(&mut self) -> Result<&'a str, DecodeError> {
        let len = self.u32()? as usize;
        let bytes = self.bytes(len)?;
        core::str::from_utf8(bytes).map_err(|_| DecodeError::BadUtf8)
    }

    /// A count-prefixed run of names, read straight into one [`NamePool`].
    ///
    /// This is where the device saving is taken rather than merely represented: the names never
    /// exist as separate `String`s even briefly, so decoding a bundle does not allocate one heap
    /// block per name -- which on a segregated allocator costs a 16-byte class for a name averaging
    /// nine characters, and costs it again in the fragmentation the freed blocks leave behind.
    fn name_pool(&mut self) -> Result<NamePool, DecodeError> {
        let count = self.u32()? as usize;
        let mut text = String::new();
        let mut spans: Vec<(u32, u32)> = Vec::new();
        for _ in 0..count {
            let name = self.str_slice()?;
            spans.push((text.len() as u32, name.len() as u32));
            text.push_str(name);
        }
        Ok(NamePool::freeze(text, spans))
    }

    /// Read a module's CONTENT (name + functions + body) -- the header-less body of a bare module or
    /// of one module inside a bundle.
    fn module_content(&mut self) -> Result<Module, DecodeError> {
        let name = self.string()?;
        let n_functions = self.u32()? as usize;
        let mut functions = Vec::with_capacity(n_functions);
        for _ in 0..n_functions {
            functions.push(self.code_object()?);
        }
        let mut body = self.code_object()?;
        let debug_len = self.u32()? as usize;
        let debug = self.bytes(debug_len)?;
        let mut tables = take_debug_section(debug, functions.len() + 1).into_iter();
        for function in &mut functions {
            function.line_table = tables.next().unwrap_or_default();
        }
        body.line_table = tables.next().unwrap_or_default();
        Ok(Module {
            name,
            functions: functions.into(),
            body,
        })
    }

    fn py_type(&mut self) -> Result<StaticType, DecodeError> {
        let tag = self.u8()?;
        StaticType::from_u8(tag).ok_or(DecodeError::BadTag("StaticType", tag))
    }

    fn const_value(&mut self) -> Result<Const, DecodeError> {
        let tag = self.u8()?;
        let c = match tag {
            0 => Const::None,
            1 => Const::Bool(self.u8()? != 0),
            2 => Const::Int(self.i64()?),
            3 => {
                let len = self.u32()? as usize;
                Const::Str(PyStr::from_wtf8(self.bytes(len)?.to_vec()))
            }
            4 => {
                let n = self.u32()? as usize;
                let mut names = Vec::with_capacity(n);
                for _ in 0..n {
                    names.push(self.string()?);
                }
                Const::KwNames(names)
            }
            5 => Const::Float(self.i64()? as u64),
            6 => Const::Imaginary(self.i64()? as u64),
            7 => {
                let n = self.u32()? as usize;
                Const::ArgKinds(self.bytes(n)?.to_vec())
            }
            8 => Const::BigInt(self.string()?),
            9 => {
                let n = self.u32()? as usize;
                Const::Bytes(self.bytes(n)?.to_vec())
            }
            _ => return Err(DecodeError::BadTag("Const", tag)),
        };
        Ok(c)
    }

    fn op(&mut self, wide: &mut Vec<[u32; 2]>) -> Result<Op, DecodeError> {
        let tag = self.u8()?;
        let op = match tag {
            0 => Op::LoadConst(self.u32()?),
            1 => Op::LoadFast(self.u32()?),
            2 => Op::StoreFast(self.u32()?),
            3 => Op::LoadGlobal(self.u32()?),
            4 => {
                let first = self.u32()?;
                let second = self.u32()?;
                wide.push([first, second]);
                Op::LoadAttr {
                    site: (wide.len() - 1) as u32,
                }
            }
            5 => {
                let b = self.u8()?;
                Op::Binary(BinOp::from_u8(b).ok_or(DecodeError::BadTag("BinOp", b))?)
            }
            6 => {
                let c = self.u8()?;
                Op::Compare(CmpOp::from_u8(c).ok_or(DecodeError::BadTag("CmpOp", c))?)
            }
            7 => Op::PopTop,
            8 => Op::Jump(self.u32()?),
            9 => Op::PopJumpIfFalse(self.u32()?),
            10 => Op::Call(self.u32()?),
            11 => Op::Return,
            12 => {
                let u = self.u8()?;
                Op::Unary(UnaryOp::from_u8(u).ok_or(DecodeError::BadTag("UnaryOp", u))?)
            }
            13 => Op::Subscript {
                cache: self.u32()?,
            },
            14 => Op::BuildSlice,
            15 => Op::BuildList(self.u32()?),
            16 => Op::BuildTuple(self.u32()?),
            17 => Op::BuildDict(self.u32()?),
            18 => Op::GetIter,
            19 => Op::ForIter(self.u32()?),
            20 => Op::Setitem,
            21 => Op::Contains {
                negate: self.u8()? != 0,
            },
            24 => Op::Raise(self.u8()?),
            25 => Op::MatchExc,
            26 => Op::LoadExc,
            27 => Op::PopExcept,
            28 => Op::Reraise,
            29 => Op::DeleteFast(self.u32()?),
            30 => Op::MakeFunction {
                func: self.u32()?,
                flags: self.u8()?,
            },
            31 => Op::BuildClass,
            32 => {
                let first = self.u32()?;
                let second = self.u32()?;
                wide.push([first, second]);
                Op::SetAttr {
                    site: (wide.len() - 1) as u32,
                }
            }
            33 => Op::UnpackSequence(self.u32()?),
            34 => Op::ListAppend,
            35 => Op::SetAdd,
            36 => Op::DictInsert,
            37 => Op::LoadSuper(self.u32()?),
            38 => Op::BuildSet(self.u32()?),
            39 => {
                let first = self.u32()?;
                let second = self.u32()?;
                wide.push([first, second]);
                Op::UnpackEx {
                    site: (wide.len() - 1) as u32,
                }
            }
            40 => {
                let first = self.u32()?;
                let second = self.u32()?;
                wide.push([first, second]);
                Op::CallKw {
                    site: (wide.len() - 1) as u32,
                }
            }
            41 => Op::Yield,
            42 => {
                let kinds = self.u32()?;
                let kwnames = self.u32()?;
                wide.push([kinds, kwnames]);
                Op::CallEx {
                    site: (wide.len() - 1) as u32,
                }
            }
            43 => Op::DeleteItem,
            44 => Op::DeleteAttr { name: self.u32()? },
            45 => Op::LoadDeref(self.u32()?),
            46 => Op::StoreDeref(self.u32()?),
            47 => Op::LoadClosure(self.u32()?),
            48 => Op::SetupClassNamespace,
            49 => Op::StoreName(self.u32()?),
            50 => Op::LoadName(self.u32()?),
            51 => Op::ImportName(self.u32()?),
            52 => Op::ImportFrom(self.u32()?),
            53 => Op::StoreGlobal(self.u32()?),
            54 => Op::YieldFrom,
            55 => Op::ImportStar,
            58 => Op::Await,
            59 => Op::ListGrow { list: self.u32()? },
            57 => Op::BuildClassKw { kwnames: self.u32()? },
            56 => {
                let b = self.u8()?;
                Op::InplaceBinOp(BinOp::from_u8(b).ok_or(DecodeError::BadTag("BinOp", b))?)
            }
            _ => return Err(DecodeError::BadTag("Op", tag)),
        };
        Ok(op)
    }

    fn code_object(&mut self) -> Result<CodeObject, DecodeError> {
        let name = self.string()?;
        let n_params = self.u32()? as usize;
        let mut params = Vec::with_capacity(n_params);
        for _ in 0..n_params {
            let pname = self.string()?;
            let ty = self.py_type()?;
            params.push(Param { name: pname, ty });
        }
        let posonly_count = self.u32()?;
        let kwonly_count = self.u32()?;
        let flags = CodeFlags(self.u8()?);
        let is_generator = flags.contains(CodeFlags::GENERATOR);
        let is_coroutine = flags.contains(CodeFlags::COROUTINE);
        let has_varargs = flags.contains(CodeFlags::VARARGS);
        let has_varkwargs = flags.contains(CodeFlags::VARKWARGS);
        let ret_ty = self.py_type()?;
        let n_locals = self.u32()? as usize;
        let local_names = self.name_pool()?;
        let cellvars = self.name_pool()?;
        let freevars = self.name_pool()?;
        let n_local_types = self.u32()? as usize;
        let mut local_types = Vec::with_capacity(n_local_types);
        for _ in 0..n_local_types {
            local_types.push(self.py_type()?);
        }
        let n_consts = self.u32()? as usize;
        let mut consts = Vec::with_capacity(n_consts);
        for _ in 0..n_consts {
            consts.push(self.const_value()?);
        }
        let names = self.name_pool()?;
        let doc = match self.u8()? {
            0 => None,
            _ => Some(self.string()?),
        };
        let cache_count = self.u32()? as usize;
        let n_ops = self.u32()? as usize;
        let mut ops = Vec::with_capacity(n_ops);
        let mut wide_operands: Vec<[u32; 2]> = Vec::new();
        for _ in 0..n_ops {
            ops.push(self.op(&mut wide_operands)?);
        }
        wide_operands.shrink_to_fit();
        let n_exc = self.u32()? as usize;
        let mut exc_table = Vec::with_capacity(n_exc);
        for _ in 0..n_exc {
            exc_table.push(ExcEntry {
                start: self.u32()?,
                end: self.u32()?,
                target: self.u32()?,
                depth: self.u32()?,
            });
        }
        Ok(CodeObject {
            name,
            params,
            posonly_count,
            kwonly_count,
            is_generator,
            is_coroutine,
            has_varargs,
            has_varkwargs,
            ret_ty,
            n_locals,
            local_names,
            cellvars,
            freevars,
            local_types,
            consts,
            names,
            ops,
            wide_operands,
            line_table: Vec::new(),
            doc,
            cache_count,
            exc_table,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn exception_tags_match_the_ratified_values() {
        assert_eq!(exception_tag("IndexError"), 0xA7B8_5DD5);
        assert_eq!(exception_tag("Exception"), 0x8391_09C4);
        let mut tags: Vec<u32> = EXCEPTION_HIERARCHY.iter().map(|(n, _)| exception_tag(n)).collect();
        assert!(tags.iter().all(|&t| t != 0));
        tags.sort_unstable();
        let count = tags.len();
        tags.dedup();
        assert_eq!(tags.len(), count, "exception tags must be collision-free");
    }

    #[test]
    fn a_bundle_embeds_the_bare_module_payload_verbatim() {
        let m = sample_module();
        let features = FeatureFlags::FIRST_LIGHT;
        let bare = m.encode(features);
        let bundle = Bundle {
            entry: m,
            modules: Vec::new(),
        }
        .encode(features);

        assert_eq!(&bare[..4], &bundle[..4], "the same magic");
        assert_eq!(u16::from_le_bytes([bare[4], bare[5]]), FORMAT_VERSION);
        assert_eq!(
            u16::from_le_bytes([bundle[4], bundle[5]]),
            BUNDLE_FORMAT_VERSION
        );
        assert_eq!(
            &bare[8..],
            &bundle[8..bare.len()],
            "a bundle's entry payload is the bare module's payload, byte for byte"
        );
    }

    /// An artifact requiring a capability this build does not implement is REFUSED, and the refusal
    /// names the gap.
    ///
    /// This is the row the mechanism was missing. The header word was written, read, and handed to
    /// the caller -- and every caller on the device path binds it as `_features` and drops it, so an
    /// artifact could declare anything at all and still run. The type's own documentation said a
    /// reader "rejects the artifact rather than mis-executing it", which described an intention
    /// rather than the code. That is the same shape as the bundle version that identified nothing:
    /// a field faithfully carried, believed to be a gate, gating nothing.
    #[test]
    fn an_artifact_requiring_an_unimplemented_capability_is_refused() {
        let m = sample_module();

        assert!(Module::decode(&m.encode(FeatureFlags::FIRST_LIGHT)).is_ok());
        assert!(
            Module::decode(&m.encode(FeatureFlags::default())).is_ok(),
            "an artifact that requires NOTHING must load on any reader that knows the base format"
        );

        let unknown = FeatureFlags(!SUPPORTED_FEATURES.0);
        assert!(!unknown.is_empty(), "there must be a bit this build does not implement");
        let demanding = FeatureFlags(SUPPORTED_FEATURES.0 | unknown.0);

        let bundle = Bundle {
            entry: m.clone(),
            modules: Vec::new(),
        };
        let cases: [(&str, DecodeError); 2] = [
            (
                "bare module",
                Module::decode(&m.encode(demanding)).expect_err("a bare module must be refused"),
            ),
            (
                "bundle",
                Bundle::decode(&bundle.encode(demanding)).expect_err("a bundle must be refused"),
            ),
        ];
        for (container, refused) in cases {
            match Some(refused) {
                Some(DecodeError::UnsupportedFeatures { required, missing }) => {
                    assert_eq!(required, demanding, "{container}");
                    assert_eq!(
                        missing, unknown,
                        "the {container} refusal must name the MISSING bits, not merely that                          something was wrong"
                    );
                }
                other => panic!("expected an UnsupportedFeatures refusal for a {container}, got {other:?}"),
            }
        }
    }

    /// One line per op goes in, and every op reports its own line back.
    ///
    /// Checked against the INPUT rather than against a re-encode: a round trip through one
    /// implementation's own inverse passes on two consistent misreadings, and this codec has a
    /// zigzag and a run length that could both be wrong in the same direction.
    #[test]
    fn a_line_table_reports_the_line_of_every_op() {
        let lines: Vec<u32> = vec![1, 1, 1, 2, 2, 7, 3, 3, 3, 3, 200, 1];
        let table = encode_line_table(&lines);
        let mut co = sample_module().body;
        co.line_table = table;

        for (index, &expected) in lines.iter().enumerate() {
            assert_eq!(
                co.line_for(index),
                Some(expected),
                "op {index} should report line {expected}"
            );
        }
        assert_eq!(co.line_for(lines.len()), None, "past the last op there is no line");
    }

    /// A build that tracked no positions produces NO table, not a table of zeroes -- and a reader
    /// then says it does not know rather than claiming line 0.
    #[test]
    fn no_line_information_is_absent_rather_than_zero() {
        assert!(encode_line_table(&[0, 0, 0]).is_empty());
        assert!(encode_line_table(&[]).is_empty());
        let mut co = sample_module().body;
        co.line_table = encode_line_table(&[0, 0, 0]);
        assert_eq!(co.line_for(0), None);
    }

    /// The encoding is COMPACT, which is the only reason it is affordable on a device: a program
    /// whose ops group into runs must cost far less than a word per op.
    #[test]
    fn a_line_table_costs_far_less_than_a_word_per_op() {
        let lines: Vec<u32> = (0..400).map(|i| 1 + i / 4).collect();
        let table = encode_line_table(&lines);
        assert!(
            table.len() * 4 < lines.len() * 4,
            "the table is {} bytes for {} ops, which is no better than a u32 each",
            table.len(),
            lines.len()
        );
        assert_eq!(co_line(&table, 0), Some(1));
        assert_eq!(co_line(&table, 399), Some(100));
    }

    fn co_line(table: &[u8], index: usize) -> Option<u32> {
        let mut co = sample_module().body;
        co.line_table = table.to_vec();
        co.line_for(index)
    }

    /// Stripping the lines makes the artifact byte-identical to one from a build that never had
    /// them -- so a profile that does not want diagnostics pays nothing at all, not merely less.
    #[test]
    fn stripping_the_lines_leaves_no_debug_section() {
        let mut m = sample_module();
        m.body.line_table = encode_line_table(&[1, 1, 2, 2, 3, 3]);
        let with_lines = m.encode(FeatureFlags::FIRST_LIGHT);

        let mut bare = m.clone();
        bare.strip_line_tables();
        let without = bare.encode(FeatureFlags::FIRST_LIGHT);

        assert!(without.len() < with_lines.len(), "lines cost wire bytes");
        assert_eq!(
            &without[without.len() - 4..],
            &0u32.to_le_bytes(),
            "a stripped module declares an EMPTY section, exactly as a build with no lines does"
        );
        let (back, _) = Module::decode(&without).expect("decodes");
        assert_eq!(back.body.line_for(0), None, "and reports no line rather than line 0");
    }

    /// Stripping a BUNDLE reaches every module, entry included -- a half-stripped bundle would give
    /// a traceback that names a line in some frames and not others, which is worse than either.
    #[test]
    fn stripping_a_bundle_reaches_the_entry_and_every_module() {
        let mut m = sample_module();
        m.body.line_table = encode_line_table(&[1, 1, 2, 2, 3, 3]);
        let mut bundle = Bundle {
            entry: m.clone(),
            modules: alloc::vec![m.clone(), m],
        };
        let with_lines = bundle.encode(FeatureFlags::FIRST_LIGHT);

        bundle.strip_line_tables();
        let without = bundle.encode(FeatureFlags::FIRST_LIGHT);
        assert!(without.len() < with_lines.len());

        let (back, _) = Bundle::decode(&without).expect("decodes");
        assert_eq!(back.entry.body.line_for(0), None, "the entry is stripped");
        for module in &back.modules {
            assert_eq!(module.body.line_for(0), None, "and every imported module too");
        }
    }

    /// A pool reads like the `Vec<String>` it replaced: same order, same names, indexable.
    #[test]
    fn a_name_pool_reads_like_the_vec_it_replaced() {
        let pool: NamePool = ["alpha", "b", "", "gamma"].into_iter().collect();
        assert_eq!(pool.len(), 4);
        assert!(!pool.is_empty());
        assert_eq!(&pool[0], "alpha");
        assert_eq!(&pool[1], "b");
        assert_eq!(&pool[2], "", "an empty name keeps its slot rather than vanishing");
        assert_eq!(&pool[3], "gamma");
        assert_eq!(pool.get(4), None);
        assert_eq!(pool.iter().collect::<Vec<_>>(), ["alpha", "b", "", "gamma"]);
        assert_eq!(pool.position("gamma"), Some(3));
        assert_eq!(pool.position("delta"), None);
        assert!(pool.contains("b"));
        assert!(NamePool::new().is_empty());
    }

    /// THE SLACK IS UNREPRESENTABLE, AND THIS PINS THE REPRESENTATION THAT MAKES IT SO.
    ///
    /// A pool built from `String` + `Vec` fields CAN hold spare capacity: both grow GEOMETRICALLY,
    /// so a pool filled name by name sits close to twice the bytes its text needs. Asserting the
    /// absence of that slack checks a RUNTIME value, which a later edit can quietly reintroduce.
    ///
    /// Frozen into `Box<str>` and `Box<[_]>`, a pool has nowhere to PUT slack -- and it also drops
    /// the capacity words themselves, four bytes per field on a 32-bit target. That sounds
    /// negligible until the pools are counted: a code object has FOUR, most of them empty, so a
    /// 44-code-object bundle carries 176 of them and pays for every one.
    ///
    /// So the assertion is on the TYPE: two fat pointers and nothing else. It holds at any pointer
    /// width, and it fails the moment someone restores a growable field.
    #[test]
    fn a_pool_is_two_fat_pointers_and_cannot_hold_slack() {
        assert_eq!(
            core::mem::size_of::<NamePool>(),
            2 * core::mem::size_of::<&[u8]>(),
            "a pool must be exactly its two frozen buffers -- a capacity word here is paid 176 times              on a bundle importing `asyncio`, and it lets the geometric slack back in"
        );
        let names: Vec<String> = (0..40).map(|i| alloc::format!("identifier_{i}")).collect();
        let pool: NamePool = names.iter().collect();
        assert_eq!(pool.len(), 40);
        assert_eq!(&pool[39], "identifier_39");
    }

    /// Every artifact this build writes declares an EMPTY debug section, and it round-trips. This is
    /// the half that ordinary use exercises, and on its own it proves nothing about the reservation.
    #[test]
    fn a_module_declares_an_empty_debug_section_and_round_trips() {
        let m = sample_module();
        let bytes = m.encode(FeatureFlags::FIRST_LIGHT);
        assert_eq!(
            &bytes[bytes.len() - 4..],
            &0u32.to_le_bytes(),
            "module content must end with the reserved section's length, and this build writes none"
        );
        let (back, _) = Module::decode(&bytes).expect("an empty section must decode");
        assert_eq!(back, m);
    }

    /// THE ONLY PART OF THE RESERVATION ACTUALLY UNDER TEST: a reader built to TODAY's layout is
    /// handed a module carrying a section LONGER than any this build can write, and must still read
    /// every field that FOLLOWS it.
    ///
    /// The whole value of reserving four bytes a module is the claim "a reader that skips by declared
    /// length skips a longer one", and that is a claim about a producer which does not exist yet --
    /// so nothing in ordinary use can exercise it, and it would sit unproven until the day it was
    /// relied on. An escape hatch nobody has opened is worse than none, because it gets trusted.
    ///
    /// The following field is a real one rather than a contrivance: a bundle stores its modules back
    /// to back, so the SECOND module is what lands immediately after the first module's section. If
    /// the reader stepped over a fixed zero instead of the declared length, the second module would
    /// decode from the middle of the first one's debug bytes.
    #[test]
    fn a_reader_steps_over_a_debug_section_longer_than_it_can_write() {
        let m = sample_module();
        let features = FeatureFlags::FIRST_LIGHT;

        let bare = m.encode(features);
        let content = &bare[8..];
        assert_eq!(&content[content.len() - 4..], &0u32.to_le_bytes());
        let without_section = &content[..content.len() - 4];

        let payload: Vec<u8> = (0..37u8).collect();
        let mut fat = without_section.to_vec();
        put_len(&mut fat, payload.len());
        fat.extend_from_slice(&payload);

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        put_u16(&mut bytes, BUNDLE_FORMAT_VERSION);
        put_u16(&mut bytes, features.0);
        bytes.extend_from_slice(&fat);
        put_len(&mut bytes, 1);
        bytes.extend_from_slice(content);

        let (decoded, back_features) =
            Bundle::decode(&bytes).expect("a longer debug section must be stepped over, not refused");
        assert_eq!(back_features, features);
        assert_eq!(decoded.entry, m, "the module carrying the section still reads");
        assert_eq!(
            decoded.modules,
            vec![m],
            "and so does everything AFTER it -- which is the property being reserved"
        );
    }

    /// An enum is as wide as its widest variant, so ONE op's payload sets the size of every op in
    /// every array a device decodes. That is a cost with no local cause: the variant that pays it is
    /// not the variant that spends it, and the ops that get wider are the common ones. This pins the
    /// width so adding a fat variant is a decision someone makes on purpose.
    ///
    /// Every payload here is a `u32`, a `u8` or a C-like enum, so the layout does not depend on
    /// pointer width and this measurement holds for a 32-bit target as well as the host it runs on.
    /// A `usize` or a reference in a variant would break that, and would break this test first.
    #[test]
    fn no_op_variant_is_wider_than_one_payload_word() {
        assert_eq!(
            core::mem::align_of::<Op>(),
            4,
            "a payload wider than 4-byte alignment would pad every op"
        );
        assert!(
            core::mem::size_of::<Op>() <= 8,
            "size_of::<Op>() is {}; a variant carrying a second payload word widens EVERY op, and at \
             roughly 1,300 ops in a real bundle each byte is another 1.3 KiB of device RAM",
            core::mem::size_of::<Op>()
        );
    }

    /// The payload of both containers is written by one function, so a bundle has to declare the
    /// layout version of what it embeds. This asserts the DERIVATION rather than a literal, because a
    /// literal is what let the two drift in the first place: a test naming today's number passes
    /// forever while `FORMAT_VERSION` walks away from it.
    #[test]
    fn a_bundle_version_carries_the_module_layout_version_it_embeds() {
        assert_eq!(
            BUNDLE_FORMAT_VERSION & !BUNDLE_KIND_BIT,
            FORMAT_VERSION,
            "a bundle must declare the layout version of the module content it carries"
        );
        assert_ne!(
            BUNDLE_FORMAT_VERSION, FORMAT_VERSION,
            "the two containers must still be distinguishable by their version word alone"
        );
        assert_eq!(
            FORMAT_VERSION & BUNDLE_KIND_BIT,
            0,
            "the kind bit must stay outside the range the layout version can reach"
        );
    }

    /// A bundle written before the version carried its payload's layout is REFUSED rather than
    /// decoded with today's field expectations. 17 is that artifact's version, and it is the case the
    /// strict-equality check exists for.
    #[test]
    fn a_bundle_declaring_the_old_container_version_is_refused() {
        let m = sample_module();
        let mut bytes = Bundle {
            entry: m,
            modules: Vec::new(),
        }
        .encode(FeatureFlags::FIRST_LIGHT);
        bytes[4..6].copy_from_slice(&17u16.to_le_bytes());

        assert_eq!(
            Bundle::decode(&bytes),
            Err(DecodeError::UnsupportedVersion(17)),
            "a bundle whose header predates the derived version must be refused, not decoded"
        );
    }

    fn sample_module() -> Module {
        let func = CodeObject {
            name: String::from("inc"),
            doc: None,
            params: vec![Param {
                name: String::from("n"),
                ty: StaticType::Int,
            }],
            posonly_count: 0,
            kwonly_count: 0,
            is_generator: false,
            is_coroutine: false,
            has_varargs: false,
            has_varkwargs: false,
            ret_ty: StaticType::Int,
            n_locals: 1,
            local_names: ["n"].into_iter().collect(),
            cellvars: NamePool::new(),
            freevars: NamePool::new(),
            local_types: vec![StaticType::Int],
            consts: vec![Const::Int(1), Const::None, Const::KwNames(vec![String::from("x")])],
            names: ["x"].into_iter().collect(),
            ops: vec![
                Op::LoadFast(0),
                Op::LoadConst(0),
                Op::Binary(BinOp::Add),
                Op::Return,
                Op::LoadAttr { site: 0 },
                Op::PopTop,
            ],
            cache_count: 1,
            wide_operands: vec![[0, 0]],
            line_table: Vec::new(),
            exc_table: vec![ExcEntry {
                start: 0,
                end: 5,
                target: 8,
                depth: 0,
            }],
        };
        Module {
            name: String::from("m"),
            functions: vec![func].into(),
            body: CodeObject {
                name: String::from("<module>"),
                doc: None,
                params: Vec::new(),
                posonly_count: 0,
                kwonly_count: 0,
                is_generator: false,
                is_coroutine: false,
                has_varargs: false,
                has_varkwargs: false,
                ret_ty: StaticType::Dynamic,
                n_locals: 0,
                local_names: NamePool::new(),
                cellvars: NamePool::new(),
                freevars: NamePool::new(),
                local_types: Vec::new(),
                consts: vec![Const::None],
                names: NamePool::new(),
                ops: vec![Op::LoadConst(0), Op::Return],
                cache_count: 0,
                wide_operands: Vec::new(),
                line_table: Vec::new(),
                exc_table: Vec::new(),
            },
        }
    }

    #[test]
    fn module_container_round_trips() {
        let module = sample_module();
        let bytes = module.encode(FeatureFlags::FIRST_LIGHT);
        assert_eq!(&bytes[..4], &MAGIC);
        let (decoded, features) = Module::decode(&bytes).expect("decodes");
        assert_eq!(decoded, module);
        assert!(features.contains(FeatureFlags::FIRST_LIGHT));
    }

    #[test]
    fn bundle_container_round_trips() {
        let named = |name: &str| {
            let mut m = sample_module();
            m.name = String::from(name);
            m
        };
        let bundle = Bundle {
            entry: named("__main__"),
            modules: vec![named("helpers"), named("config")],
        };
        let bytes = bundle.encode(FeatureFlags::FIRST_LIGHT);
        assert_eq!(&bytes[..4], &MAGIC);
        let (decoded, features) = Bundle::decode(&bytes).expect("decodes");
        assert_eq!(decoded, bundle);
        assert_eq!(decoded.modules.len(), 2);
        assert_eq!(decoded.modules[0].name, "helpers");
        assert!(features.contains(FeatureFlags::FIRST_LIGHT));
        assert!(matches!(
            Module::decode(&bytes),
            Err(DecodeError::UnsupportedVersion(v)) if v == BUNDLE_FORMAT_VERSION
        ));
        let solo = Bundle {
            entry: named("__main__"),
            modules: Vec::new(),
        };
        let (d2, _) = Bundle::decode(&solo.encode(FeatureFlags::FIRST_LIGHT)).expect("solo decodes");
        assert!(d2.modules.is_empty());
    }

    #[test]
    fn every_op_variant_round_trips() {
        let ops = vec![
            Op::LoadConst(7),
            Op::LoadFast(1),
            Op::StoreFast(2),
            Op::LoadGlobal(3),
            Op::LoadAttr { site: 0 },
            Op::Binary(BinOp::Mod),
            Op::Compare(CmpOp::Le),
            Op::PopTop,
            Op::Jump(9),
            Op::PopJumpIfFalse(10),
            Op::Call(2),
            Op::Unary(UnaryOp::Neg),
            Op::Subscript { cache: 6 },
            Op::BuildSlice,
            Op::BuildList(3),
            Op::BuildTuple(2),
            Op::BuildDict(1),
            Op::GetIter,
            Op::ForIter(7),
            Op::Setitem,
            Op::Contains { negate: true },
            Op::Raise(1),
            Op::MatchExc,
            Op::LoadExc,
            Op::PopExcept,
            Op::Reraise,
            Op::DeleteFast(2),
            Op::MakeFunction { func: 0, flags: 1 },
            Op::BuildClass,
            Op::SetAttr { site: 1 },
            Op::UnpackSequence(2),
            Op::ListAppend,
            Op::SetAdd,
            Op::DictInsert,
            Op::LoadSuper(3),
            Op::BuildSet(2),
            Op::UnpackEx { site: 2 },
            Op::CallKw { site: 3 },
            Op::Yield,
            Op::CallEx { site: 4 },
            Op::DeleteItem,
            Op::DeleteAttr { name: 4 },
            Op::LoadDeref(2),
            Op::StoreDeref(1),
            Op::LoadClosure(0),
            Op::SetupClassNamespace,
            Op::StoreName(1),
            Op::LoadName(2),
            Op::ImportName(3),
            Op::ImportFrom(4),
            Op::StoreGlobal(5),
            Op::YieldFrom,
            Op::ImportStar,
            Op::InplaceBinOp(BinOp::Add),
            Op::Await,
            Op::ListGrow { list: 2 },
            Op::Return,
        ];
        let wide = [[4, 5], [0, 7], [1, 1], [2, 1], [2, 1]];

        let mut buf = Vec::new();
        for op in &ops {
            put_op(&mut buf, op, &wide);
        }
        let mut r = Reader {
            data: &buf,
            pos: 0,
        };
        let mut decoded_wide: Vec<[u32; 2]> = Vec::new();
        for expected in &ops {
            assert_eq!(r.op(&mut decoded_wide).unwrap(), *expected);
        }
        assert_eq!(
            decoded_wide, wide,
            "the side table must come back with the same entries in the same order"
        );
    }

    #[test]
    fn code_object_cellvars_freevars_round_trip() {
        let co = CodeObject {
            name: String::from("inner"),
            doc: None,
            params: Vec::new(),
            posonly_count: 0,
            kwonly_count: 0,
            is_generator: false,
            is_coroutine: false,
            has_varargs: false,
            has_varkwargs: false,
            ret_ty: StaticType::Dynamic,
            n_locals: 1,
            local_names: ["n"].into_iter().collect(),
            cellvars: ["n"].into_iter().collect(),
            freevars: ["outer_x"].into_iter().collect(),
            local_types: vec![StaticType::Dynamic],
            consts: vec![Const::Int(1)],
            names: NamePool::new(),
            ops: vec![
                Op::LoadDeref(1),
                Op::LoadConst(0),
                Op::Binary(BinOp::Add),
                Op::StoreDeref(0),
                Op::LoadClosure(0),
                Op::MakeFunction { func: 0, flags: 0x04 },
                Op::Return,
            ],
            cache_count: 0,
            wide_operands: Vec::new(),
            line_table: Vec::new(),
            exc_table: Vec::new(),
        };
        let mut buf = Vec::new();
        put_code_object(&mut buf, &co);
        let mut r = Reader { data: &buf, pos: 0 };
        assert_eq!(r.code_object().unwrap(), co);
    }

    #[test]
    fn coroutine_bit_round_trips_independently_of_the_generator_bit() {
        for (generator, coroutine) in [(false, false), (true, false), (false, true), (true, true)] {
            let co = CodeObject {
                name: String::from("f"),
                doc: None,
                params: Vec::new(),
                posonly_count: 0,
                kwonly_count: 0,
                is_generator: generator,
                is_coroutine: coroutine,
                has_varargs: false,
                has_varkwargs: false,
                ret_ty: StaticType::Dynamic,
                n_locals: 0,
                local_names: NamePool::new(),
                cellvars: NamePool::new(),
                freevars: NamePool::new(),
                local_types: Vec::new(),
                consts: Vec::new(),
                names: NamePool::new(),
                ops: vec![Op::Await, Op::Return],
                cache_count: 0,
                wide_operands: Vec::new(),
                line_table: Vec::new(),
                exc_table: Vec::new(),
            };
            let mut buf = Vec::new();
            put_code_object(&mut buf, &co);
            let mut r = Reader { data: &buf, pos: 0 };
            let back = r.code_object().unwrap();
            assert_eq!(back, co);
            assert_eq!(back.is_generator, generator);
            assert_eq!(back.is_coroutine, coroutine);
        }
    }

    #[test]
    fn all_sixteen_flag_combinations_survive_one_packed_byte() {
        for bits in 0u8..16 {
            let mut co = CodeObject {
                name: String::from("f"),
                doc: None,
                params: Vec::new(),
                posonly_count: 0,
                kwonly_count: 0,
                is_generator: bits & 1 != 0,
                is_coroutine: bits & 2 != 0,
                has_varargs: bits & 4 != 0,
                has_varkwargs: bits & 8 != 0,
                ret_ty: StaticType::Dynamic,
                n_locals: 0,
                local_names: NamePool::new(),
                cellvars: NamePool::new(),
                freevars: NamePool::new(),
                local_types: Vec::new(),
                consts: Vec::new(),
                names: NamePool::new(),
                ops: vec![Op::Return],
                cache_count: 0,
                wide_operands: Vec::new(),
                line_table: Vec::new(),
                exc_table: Vec::new(),
            };
            let mut buf = Vec::new();
            put_code_object(&mut buf, &co);
            let mut r = Reader { data: &buf, pos: 0 };
            assert_eq!(r.code_object().unwrap(), co, "flags {bits:#06b} round trip");
            assert_eq!(CodeFlags::of(&co).0, bits, "packed bit values");
            assert_eq!(CodeFlags::of(&co).0 & 0xF0, 0, "bits 4-7 stay spare");
            co.is_coroutine = true;
            assert!(CodeFlags::of(&co).contains(CodeFlags::COROUTINE));
        }
    }

    #[test]
    fn a_string_constant_carries_code_points_cpython_can_hold_and_utf8_cannot() {
        let rows: &[(&[u32], &[u8], usize)] = &[
            (&[0xD800], &[0xED, 0xA0, 0x80], 1),
            (&[0xD800, 0xDC00], &[0xED, 0xA0, 0x80, 0xED, 0xB0, 0x80], 2),
            (&[0x10000], &[0xF0, 0x90, 0x80, 0x80], 1),
            (&[0x20AC], &[0xE2, 0x82, 0xAC], 1),
            (&[0x61, 0xDFFF, 0x62], &[0x61, 0xED, 0xBF, 0xBF, 0x62], 3),
        ];
        for (codes, bytes, length) in rows {
            let s = PyStr::from_code_points(codes.iter().copied());
            assert_eq!(s.as_bytes(), *bytes, "byte form for {codes:04X?}");
            assert_eq!(s.code_points().count(), *length, "len for {codes:04X?}");
            assert_eq!(s.code_points().collect::<Vec<_>>(), *codes, "round trip");

            let module = Module {
                name: String::from("m"),
                functions: Functions::default(),
                body: CodeObject {
                    name: String::from("<module>"),
                    doc: None,
                    params: Vec::new(),
                    posonly_count: 0,
                    kwonly_count: 0,
                    is_generator: false,
                    is_coroutine: false,
                    has_varargs: false,
                    has_varkwargs: false,
                    ret_ty: StaticType::Dynamic,
                    n_locals: 0,
                    local_names: NamePool::new(),
                    cellvars: NamePool::new(),
                    freevars: NamePool::new(),
                    local_types: Vec::new(),
                    consts: vec![Const::Str(s.clone())],
                    names: NamePool::new(),
                    ops: vec![Op::LoadConst(0), Op::Return],
                    cache_count: 0,
                    wide_operands: Vec::new(),
                    line_table: Vec::new(),
                    exc_table: Vec::new(),
                },
            };
            let (back, _) =
                Module::decode(&module.encode(FeatureFlags::default())).expect("decodes");
            assert_eq!(back.body.consts, vec![Const::Str(s.clone())], "survives the container");
        }
    }

    #[test]
    fn a_surrogate_free_string_is_byte_identical_to_what_utf8_would_have_written() {
        for text in ["", "hi", "caf\u{e9}", "\u{20AC}\u{1F600}", "line\nbreak"] {
            let s = PyStr::from(text);
            assert_eq!(s.as_bytes(), text.as_bytes(), "identical bytes for {text:?}");
            assert_eq!(s.as_str(), Some(text), "and it still reads as a &str");
            assert!(!s.has_surrogate());
        }
        let lone = PyStr::from_code_points([0xD800]);
        assert_eq!(lone.as_str(), None);
        assert!(lone.has_surrogate());
    }

    #[test]
    fn stop_async_iteration_is_in_the_hierarchy_under_exception() {
        let base = EXCEPTION_HIERARCHY
            .iter()
            .find(|(name, _)| *name == "StopAsyncIteration")
            .map(|(_, base)| *base);
        assert_eq!(base, Some("Exception"));
    }

    #[test]
    fn decode_rejects_bad_magic_and_version() {
        assert_eq!(Module::decode(b"XXXX...."), Err(DecodeError::BadMagic));
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        put_u16(&mut bytes, FORMAT_VERSION + 1);
        put_u16(&mut bytes, 0);
        assert_eq!(
            Module::decode(&bytes),
            Err(DecodeError::UnsupportedVersion(FORMAT_VERSION + 1))
        );
    }

    #[test]
    fn selector_bytes_round_trip() {
        for byte in 0u8..=12 {
            assert_eq!(BinOp::from_u8(byte).unwrap() as u8, byte);
        }
        for byte in 0u8..=7 {
            assert_eq!(CmpOp::from_u8(byte).unwrap() as u8, byte);
        }
        for byte in 0u8..=2 {
            assert_eq!(UnaryOp::from_u8(byte).unwrap() as u8, byte);
        }
        assert_eq!(BinOp::from_u8(13), None);
        assert_eq!(CmpOp::from_u8(8), None);
        assert_eq!(UnaryOp::from_u8(3), None);
    }

    #[test]
    fn a_full_profile_is_exactly_the_capabilities_that_exist() {
        let built = Capability::ALL.iter().fold(Profile::BARE, |p, c| p.with(*c));
        assert_eq!(built, Profile::FULL, "FULL is every capability in ALL, and no others");
        for capability in Capability::ALL {
            assert!(Profile::FULL.supports(*capability));
            assert!(!Profile::BARE.supports(*capability));
            assert!(!capability.as_str().is_empty());
        }
    }

    #[test]
    fn a_profile_cannot_describe_an_image_that_cannot_be_built() {
        let no_float = Profile::FULL.without(Capability::Float);
        assert!(!no_float.supports(Capability::Float));
        assert!(!no_float.supports(Capability::Complex), "no float means no complex");
        assert!(no_float.supports(Capability::Introspection), "and nothing else moved");

        let complex_only = Profile::BARE.with(Capability::Complex);
        assert!(complex_only.supports(Capability::Float), "asking for complex asks for float");

        let no_complex = Profile::FULL.without(Capability::Complex);
        assert!(no_complex.supports(Capability::Float));
        assert!(!no_complex.supports(Capability::Complex));
    }

    #[test]
    fn only_the_constants_an_image_cannot_materialize_need_a_capability() {
        assert_eq!(Const::Float(0).required_capability(), Some(Capability::Float));
        assert_eq!(Const::Imaginary(0).required_capability(), Some(Capability::Complex));
        for konst in [
            Const::None,
            Const::Bool(true),
            Const::Int(7),
            Const::Str(PyStr::from_wtf8(vec![b's'])),
            Const::ArgKinds(vec![0]),
            Const::KwNames(vec![String::from("k")]),
            Const::BigInt(String::from("123")),
            Const::Bytes(vec![1, 2]),
        ] {
            assert_eq!(konst.required_capability(), None, "{konst:?} needs nothing");
        }
    }
}
