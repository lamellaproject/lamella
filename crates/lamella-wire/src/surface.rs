//! How much of a class-library contract a target actually carries, as a bitmap over the capability
//! symbols the library was compiled with. It answers the one question a VERSION cannot: a version
//! states which contract a library was built against, and says nothing about how much of that
//! contract a particular build kept. Bit positions here are a WIRE CONTRACT -- append-only, never
//! renumbered -- because a second consumer reads the same bitmap and a renumbering would silently
//! re-point both.

/// The .NET 1.1 era members of types the base contract already carries.
pub const NETFX_1_1: u64 = 1 << 0;
/// The .NET 2.0 era members of types the base contract already carries.
pub const NETFX_2_0: u64 = 1 << 1;
/// The .NET 4.0 era members of types the base contract already carries.
pub const NETFX_4_0: u64 = 1 << 2;
/// The .NET 4.5 era members of types the base contract already carries.
pub const NETFX_4_5: u64 = 1 << 3;

/// Binary floating point: `System.Double`, `System.Single`, and the execution model's built-in
/// float element types.
pub const FLOAT: u64 = 1 << 4;
/// The transcendental functions on `System.Math`.
pub const MATH_TRANSCENDENTAL: u64 = 1 << 5;
/// `System.Decimal`.
pub const DECIMAL: u64 = 1 << 6;
/// The garbage-collector surface a program can reach: `System.GC`, finalization, weak references.
pub const GC: u64 = 1 << 7;
/// Threads.
pub const THREADS: u64 = 1 << 8;
/// Wait handles and the synchronization primitives built on them.
pub const WAIT_HANDLES: u64 = 1 << 9;
/// Sockets and the network surface built on them.
pub const NET: u64 = 1 << 10;
/// Transport-layer security over the network surface.
pub const NET_TLS: u64 = 1 << 11;
/// File and directory input and output.
pub const FILE_IO: u64 = 1 << 12;
/// Serial ports.
pub const SERIAL: u64 = 1 << 13;
/// Metadata INSPECTION: the member lookup on `System.Type`, the reflection types, and activation.
///
/// It does not gate `System.Type` itself or type identity: a type reference, a cast and a type
/// literal are part of the base contract at every tier.
pub const REFLECTION: u64 = 1 << 14;
/// Generic types and methods.
pub const GENERICS: u64 = 1 << 15;
/// Culture- and ordinal-sensitive string comparison, which is a capability of its own rather than a
/// member of any era.
pub const STRING_COMPARISON: u64 = 1 << 16;
/// Typed references.
pub const TYPED_REFERENCES: u64 = 1 << 17;
/// Variable-argument methods.
pub const VARARGS: u64 = 1 << 18;
/// `System.Span<T>` and `System.ReadOnlySpan<T>`.
///
/// A capability of its own rather than a member of an era: no NETMF or nanoFramework generation
/// declares a span, and it reaches this surface through the device-API clause instead. It requires
/// GENERICS, which `build-corlib.ps1` enforces as a refusal.
pub const SPAN: u64 = 1 << 19;

/// The era bits together, so "which generation was this built against" is one mask rather than four
/// tests.
///
/// It also cross-checks the version a target reports beside this bitmap for free: which era bits are
/// set determines the generation, so a manifest whose bitmap and version disagree is detectably
/// wrong rather than quietly wrong.
pub const NETFX_MASK: u64 = NETFX_1_1 | NETFX_2_0 | NETFX_4_0 | NETFX_4_5;

/// Every named symbol, as its bit and the compilation symbol it stands for.
///
/// The SYMBOL SET has one home -- the profile-to-symbol map the library is built from -- and this
/// table is the wire ORDER over that set. Keeping the order here rather than deriving it from the
/// map's row order is what makes it append-only: a map that is reordered for legibility must not
/// renumber a wire field.
pub const NAMED: &[(u64, &str)] = &[
    (NETFX_1_1, "LAMELLA_SURFACE_NETFX_1_1"),
    (NETFX_2_0, "LAMELLA_SURFACE_NETFX_2_0"),
    (NETFX_4_0, "LAMELLA_SURFACE_NETFX_4_0"),
    (NETFX_4_5, "LAMELLA_SURFACE_NETFX_4_5"),
    (FLOAT, "LAMELLA_SURFACE_FLOAT"),
    (MATH_TRANSCENDENTAL, "LAMELLA_SURFACE_MATH_TRANSCENDENTAL"),
    (DECIMAL, "LAMELLA_SURFACE_DECIMAL"),
    (GC, "LAMELLA_SURFACE_GC"),
    (THREADS, "LAMELLA_SURFACE_THREADS"),
    (WAIT_HANDLES, "LAMELLA_SURFACE_WAIT_HANDLES"),
    (NET, "LAMELLA_SURFACE_NET"),
    (NET_TLS, "LAMELLA_SURFACE_NET_TLS"),
    (FILE_IO, "LAMELLA_SURFACE_FILE_IO"),
    (SERIAL, "LAMELLA_SURFACE_SERIAL"),
    (REFLECTION, "LAMELLA_SURFACE_REFLECTION"),
    (GENERICS, "LAMELLA_SURFACE_GENERICS"),
    (STRING_COMPARISON, "LAMELLA_SURFACE_STRING_COMPARISON"),
    (TYPED_REFERENCES, "LAMELLA_SURFACE_TYPED_REFERENCES"),
    (VARARGS, "LAMELLA_SURFACE_VARARGS"),
    (SPAN, "LAMELLA_SURFACE_SPAN"),
];

/// The bit a compilation symbol stands for, or `None` when this build does not name it.
#[must_use]
pub fn bit_of(symbol: &str) -> Option<u64> {
    let mut index = 0;
    while index < NAMED.len() {
        let (bit, name) = NAMED[index];
        if name.as_bytes() == symbol.as_bytes() {
            return Some(bit);
        }
        index += 1;
    }
    None
}

/// The symbols a program's library needs that the board's does not carry.
///
/// The check a host runs before sending a program: `program & !board`. Zero means every symbol the
/// program's library had, the board's library has.
#[must_use]
pub const fn missing(program: u64, board: u64) -> u64 {
    program & !board
}

/// Whether a board carrying `board` can resolve a program built against `program`.
///
/// It is CONSERVATIVE and that is deliberate: a false refusal is recoverable and a false pass is
/// not. A program built against a large surface that only touches a small part of it is refused on
/// a board carrying the small part, even though it would have run.
///
/// **So a refusal built on this must NAME the symbols that differ and say the check is
/// conservative** ([`missing`] returns exactly those bits, and [`NAMED`] spells them). Reporting it
/// as *your program does not fit* would be a wrong answer for a program that would have run, and it
/// is the one message a reader has no way to check.
///
/// A per-member demand set is a genuine refinement of this and remains available -- it is not the
/// large computation it might look like, since the members a program uses are already in its own
/// metadata -- but it cannot replace this one: a type reached by name at run time appears in no
/// reference table, so a whole-library bitmap stays the fallback wherever a static demand set
/// cannot see.
#[must_use]
pub const fn accepts(program: u64, board: u64) -> bool {
    missing(program, board) == 0
}
