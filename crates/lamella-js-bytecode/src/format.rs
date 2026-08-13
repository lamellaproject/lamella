//! The wire format: header, tags, integers, errors.

/// The four magic bytes every artifact starts with.
pub const MAGIC: [u8; 4] = *b"LJSB";

/// The format revision this build writes, and the highest it can read.
///
/// **Bump this for ANY change to the meaning of a byte** -- a new tag, a changed field order, a
/// different span encoding.
///
/// **v2 added [`Tag::StmtWith`]**, which is a byte value v1 assigned to nothing.
///
/// **v3 added [`Tag::ExprNewTarget`]**, likewise -- `new.target` is a node of its own rather than
/// an identifier whose name happens to be `new.target`, which reads back correctly and is not what
/// the node is.
pub const FORMAT_VERSION: u16 = 3;

/// What the encoder stamps as an artifact's **compatibility floor**: the oldest reader that can
/// still read it correctly.
///
/// **THIS IS THE FIELD THAT MAKES A VERSION BUMP MEAN SOMETHING SPECIFIC.** Set it equal to
/// [`FORMAT_VERSION`] for a BREAKING change, so older readers refuse. Leave it at the older number
/// for a purely ADDITIVE one -- a new optional section, a longer header -- so older readers keep
/// working. Without the distinction every change is breaking, every field is re-read from scratch,
/// and a device fleet has to be updated in lockstep for a change that could not affect it.
///
/// **A NEW TAG IS BREAKING EVEN THOUGH IT LOOKS ADDITIVE, AND THAT IS WHY THIS MOVES WITH THE
/// FORMAT.** A v1 reader meeting [`Tag::StmtWith`] reports `BadTag` at whatever offset it
/// reached -- a byte-level complaint about a program that is perfectly well formed, arriving from
/// somewhere in the middle of a tree walk rather than from the header. Raising the floor turns that
/// into "update the interpreter", named at open, before anything is walked. A v2 artifact that does
/// not USE the tag is still refused by a v1 reader, which is the conservative direction: refusing a
/// program that would have worked is loud and recoverable, where running one that means something
/// else is neither.
///
/// v3 raises it again for [`Tag::ExprNewTarget`], on the same reasoning.
pub const MIN_READER: u16 = 3;

/// The oldest artifact this build still accepts.
///
/// Raising this DROPS support for programs already compiled and deployed. It is the other half
/// of the compatibility statement and it is deliberately a separate number, because "what we can
/// write" and "what we still accept" are different promises.
pub const MIN_SUPPORTED_FORMAT: u16 = 1;

/// The header this build writes. An artifact's OWN `header_len` is what a reader must skip --
/// see [`Header::parse`] -- because a newer artifact may carry a longer one.
pub const HEADER_LEN: usize = 48;

/// Set when the artifact carries its own source text, so a diagnostic can quote the line it points
/// at. See [`crate::Artifact::source`].
///
/// There is deliberately NO `FLAG_STRICT` beside it, though a device would find one convenient.
/// `Program.strict` is a field of the root node and it is stored there once. A header flag holding
/// the same fact would be a second place for it to be true, and two places disagree
/// -- the divergent-config trap, for a saving of one O(1) walk to a node at a known offset.
pub const FLAG_SOURCE: u16 = 1 << 0;

/// Sentinel for "no filename", in the header's `name` field.
pub const NO_STRING: u32 = u32::MAX;

/// Packed `Function` and `Arrow` flags -- bools that would otherwise cost a byte each.
///
/// These live here rather than in `encode.rs` because they are part of the WIRE FORMAT, and the
/// decoder must be able to link without the encoder. A constant shared across a feature boundary
/// belongs to the format both sides speak, not to whichever side happened to write it first.
pub const FN_GENERATOR: u8 = 1 << 0;
pub const FN_ASYNC: u8 = 1 << 1;
pub const FN_STRICT: u8 = 1 << 2;
/// **This function's own code never names `arguments`, so no arguments object need be built.**
///
/// # THE SENSE IS INVERTED ON PURPOSE, AND THAT IS WHAT MAKES IT VERSION-SAFE
///
/// Spelt the obvious way round -- `FN_USES_ARGUMENTS` -- a bit CLEAR would mean "does not need
/// one", which is exactly what an artifact written before this flag existed also says. Every old
/// program would silently lose its `arguments` binding and fail with `arguments is not defined`,
/// on a reader that opened it perfectly. **Inverted, a clear bit means "may need one", so an old
/// artifact gets the old eager behavior and the flag is a pure opt-in.** No version bump, and
/// nothing to remember at the call site.
///
/// It is set by the ENCODER, which is the only exhaustive walk of the tree there is -- a
/// hand-written scan that missed a node kind would clear this bit for a function that does use
/// `arguments`, which is the failing direction. Propagates out of an ARROW (an arrow has no
/// `arguments` of its own and sees the enclosing function's) and stops at a nested FUNCTION (which
/// gets its own).
pub const FN_NO_ARGUMENTS: u8 = 1 << 3;

/// What can be wrong with an artifact.
///
/// Every variant carries where it happened. A format error with no offset sends the next person
/// to a hex dump with nothing to search for, and this is a format whose whole job is to be read on
/// a part with no debugger attached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// Shorter than a header, or a section that runs off the end.
    Truncated { at: usize, needed: usize, len: usize },
    /// The first four bytes were not [`MAGIC`].
    BadMagic { found: [u8; 4] },
    /// **THE ARTIFACT NEEDS A NEWER READER THAN THIS ONE. THE REMEDY IS TO UPDATE THE
    /// INTERPRETER**, and [`FormatError::remedy`] says so without anyone parsing a message.
    ReaderTooOld { artifact_format: u16, needs_reader: u16, this_reader: u16 },
    /// The artifact was compiled for a format this build no longer accepts. The remedy is to
    /// recompile the program -- updating the interpreter would make it worse, not better.
    ArtifactTooOld { artifact_format: u16, min_supported: u16 },
    /// **THE PROGRAM NEEDS NEWER ENGINE SEMANTICS THAN THIS INTERPRETER IMPLEMENTS.** The format
    /// parsed perfectly; what is missing is meaning. Also an "update the interpreter".
    EngineTooOld { needs_engine: u16, this_engine: u16 },
    /// The header's total length disagrees with the bytes actually supplied -- a truncated or
    /// concatenated flash region, which is the failure a device is most likely to meet.
    LengthMismatch { declared: u32, actual: usize },
    /// A tag byte no version of this format has ever assigned.
    BadTag { at: usize, tag: u8 },
    /// A tag that is legal but not the one this position must hold.
    UnexpectedTag { at: usize, tag: u8, expected: &'static str },
    /// A varint that ran past ten bytes, or off the end of the artifact.
    BadVarint { at: usize },
    /// A string id past the end of the string directory.
    BadStringId { at: usize, id: u32, count: u32 },
    /// A node's declared payload length runs past its parent's end.
    BadNodeLength { at: usize, len: u32 },
    /// [`crate::Artifact::verify_checksum`] disagreed.
    ChecksumMismatch { declared: u32, computed: u32 },
    /// A UTF-8 blob that is not UTF-8, or a UTF-16 blob with an odd byte count.
    BadStringEncoding { at: usize },
}

/// What the person holding the failure should actually DO about it.
///
/// # A VERSION CHECK THAT ONLY SAYS "MISMATCH" HAS DONE HALF THE JOB
///
/// The two directions have **opposite** remedies, and an undifferentiated "version mismatch"
/// sends half the people who hit it the wrong way -- to update an interpreter that is already too
/// new, or to recompile a program that was fine. Worse, a caller wanting to act on it has to match
/// on a message string, which is not an interface.
///
/// So the remedy is a value. A REPL prints it, a deploy tool acts on it, and the device image
/// publishes a distinct status for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remedy {
    /// The interpreter is behind what this program needs. **Update the interpreter.**
    UpdateTheInterpreter,
    /// The program is older than this interpreter still accepts. Recompile it.
    RecompileTheProgram,
    /// Not a version problem: a corrupt, truncated or foreign artifact.
    NotAVersionProblem,
}

impl FormatError {
    /// The action that fixes this, as a value rather than as prose.
    #[must_use]
    pub fn remedy(&self) -> Remedy {
        match self {
            Self::ReaderTooOld { .. } | Self::EngineTooOld { .. } => Remedy::UpdateTheInterpreter,
            Self::ArtifactTooOld { .. } => Remedy::RecompileTheProgram,
            _ => Remedy::NotAVersionProblem,
        }
    }

    /// Whether this is a version disagreement at all, as opposed to damage.
    #[must_use]
    pub fn is_version_mismatch(&self) -> bool {
        self.remedy() != Remedy::NotAVersionProblem
    }
}

impl core::fmt::Display for FormatError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Truncated { at, needed, len } => {
                write!(f, "truncated at {at}: needed {needed} bytes, artifact is {len}")
            }
            Self::BadMagic { found } => write!(f, "not a Lamella JS artifact (magic {found:02x?})"),
            Self::ReaderTooOld { artifact_format, needs_reader, this_reader } => write!(
                f,
                "this program needs bytecode reader v{needs_reader} (it is format v{artifact_format}) \
                 and this interpreter speaks v{this_reader} -- UPDATE THE INTERPRETER"
            ),
            Self::ArtifactTooOld { artifact_format, min_supported } => write!(
                f,
                "this program was compiled for bytecode format v{artifact_format} and this \
                 interpreter no longer accepts anything below v{min_supported} -- RECOMPILE THE PROGRAM"
            ),
            Self::EngineTooOld { needs_engine, this_engine } => write!(
                f,
                "this program was compiled for engine semantics v{needs_engine} and this \
                 interpreter implements v{this_engine} -- UPDATE THE INTERPRETER"
            ),
            Self::LengthMismatch { declared, actual } => {
                write!(f, "header declares {declared} bytes, {actual} were supplied")
            }
            Self::BadTag { at, tag } => write!(f, "unknown tag {tag} at {at}"),
            Self::UnexpectedTag { at, tag, expected } => {
                write!(f, "tag {tag} at {at} where a {expected} was required")
            }
            Self::BadVarint { at } => write!(f, "malformed varint at {at}"),
            Self::BadStringId { at, id, count } => {
                write!(f, "string {id} at {at}, but the pool holds {count}")
            }
            Self::BadNodeLength { at, len } => write!(f, "node at {at} declares {len} bytes, which overruns its parent"),
            Self::ChecksumMismatch { declared, computed } => {
                write!(f, "checksum {computed:#010x} against the declared {declared:#010x}")
            }
            Self::BadStringEncoding { at } => write!(f, "malformed string blob at {at}"),
        }
    }
}

/// The 48-byte header. All fields little-endian; every offset is from the artifact's base.
///
/// # THE FOUR VERSION FIELDS, AND WHY IT IS FOUR RATHER THAN ONE
///
/// One number cannot express what a fleet needs to know. Both sibling languages carry a version
/// and check it (Python's `BUNDLE_FORMAT_VERSION`, C#'s `BAKED_IMAGE_VERSION`); what neither has is
/// a DIRECTION, so both can only say "mismatch" and not which side to fix.
///
/// | field | question it answers | who checks it |
/// |---|---|---|
/// | `version` | which layout revision wrote this | the reader, to know what to expect |
/// | `min_reader` | the OLDEST reader that can still read it | [`Header::parse`] -- hard gate |
/// | `header_len` | how far to skip to reach the body | every reader, including future ones |
/// | `min_engine` | the OLDEST interpreter that can RUN it correctly | the interpreter -- hard gate |
///
/// `engine` beside them is the version that COMPILED the artifact. It is advisory and exists so
/// a diagnostic can say what produced a program, not to gate anything.
///
/// **`header_len` IS WHAT KEEPS `min_reader` FROM BEING DECORATIVE.** The point of a
/// compatibility floor is that an ADDITIVE change can leave it alone and old readers keep working.
/// That is only possible if an old reader can skip a header grown by fields it has never heard of.
/// Without it, every change would have to be breaking, `min_reader` would always equal `version`,
/// and it would be a second field that looks like a promise and keeps none.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub version: u16,
    /// The oldest reader that can read this artifact correctly.
    pub min_reader: u16,
    /// This artifact's own header length. Read from the ARTIFACT, never assumed to be
    /// [`HEADER_LEN`], which is only what *this* build writes.
    pub header_len: u16,
    pub flags: u16,
    /// The engine that compiled it. Advisory.
    pub engine: u16,
    /// The oldest engine whose semantics can run it. Checked by [`crate::Artifact::check_engine`].
    pub min_engine: u16,
    /// The artifact's own length, so a truncated flash read is caught before anything is walked.
    pub total_len: u32,
    /// Offset of the root `Program` node.
    pub root: u32,
    /// Offset of the string directory: `str_count` little-endian `u32` blob offsets.
    pub str_dir: u32,
    pub str_count: u32,
    /// Offset and length of the source text, or `(0, 0)` when [`FLAG_SOURCE`] is clear.
    pub src: u32,
    pub src_len: u32,
    /// String id of the filename, or [`NO_STRING`].
    pub name: u32,
    /// FNV-1a over every byte after the header. Checked only on demand -- see
    /// [`crate::Artifact::verify_checksum`], which is O(n) and therefore the device's choice.
    pub checksum: u32,
}

impl Header {
    /// Reads and VALIDATES a header: magic, then COMPATIBILITY, then declared length.
    ///
    /// Magic first, deliberately. A file that is not ours at all should say so rather than
    /// report a version mismatch against two bytes of somebody else's data.
    ///
    /// **THE COMPATIBILITY CHECK IS DIRECTIONAL, NOT AN EQUALITY.** The old check here was
    /// `version != FORMAT_VERSION`, which is wrong in two ways at once: it refuses an artifact an
    /// additive bump left perfectly readable, and it cannot say WHICH SIDE is out of date -- so
    /// whoever hits it has a 50% chance of updating the wrong thing. The two directions have
    /// opposite remedies; see [`Remedy`].
    pub fn parse(bytes: &[u8]) -> Result<Self, FormatError> {
        const PREFIX: usize = 16;
        if bytes.len() < PREFIX {
            return Err(FormatError::Truncated { at: 0, needed: PREFIX, len: bytes.len() });
        }
        let magic = [bytes[0], bytes[1], bytes[2], bytes[3]];
        if magic != MAGIC {
            return Err(FormatError::BadMagic { found: magic });
        }

        let version = u16_at(bytes, 4);
        let min_reader = u16_at(bytes, 6);
        let header_len = u16_at(bytes, 8);

        if min_reader > FORMAT_VERSION {
            return Err(FormatError::ReaderTooOld {
                artifact_format: version,
                needs_reader: min_reader,
                this_reader: FORMAT_VERSION,
            });
        }
        if version < MIN_SUPPORTED_FORMAT {
            return Err(FormatError::ArtifactTooOld {
                artifact_format: version,
                min_supported: MIN_SUPPORTED_FORMAT,
            });
        }

        let header_len = header_len as usize;
        if header_len < PREFIX || bytes.len() < header_len {
            return Err(FormatError::Truncated { at: 0, needed: header_len, len: bytes.len() });
        }

        let total_len = u32_at(bytes, 16);
        if total_len as usize != bytes.len() {
            return Err(FormatError::LengthMismatch { declared: total_len, actual: bytes.len() });
        }
        Ok(Self {
            version,
            min_reader,
            header_len: header_len as u16,
            flags: u16_at(bytes, 10),
            engine: u16_at(bytes, 12),
            min_engine: u16_at(bytes, 14),
            total_len,
            root: u32_at(bytes, 20),
            str_dir: u32_at(bytes, 24),
            str_count: u32_at(bytes, 28),
            src: u32_at(bytes, 32),
            src_len: u32_at(bytes, 36),
            name: u32_at(bytes, 40),
            checksum: u32_at(bytes, 44),
        })
    }

    /// Writes the header into a [`HEADER_LEN`]-byte prefix.
    ///
    /// `pub` because the encoder is in `lamella-js-frontend` now -- the crate that owns the AST.
    pub fn write(&self, out: &mut [u8]) {
        out[0..4].copy_from_slice(&MAGIC);
        out[4..6].copy_from_slice(&self.version.to_le_bytes());
        out[6..8].copy_from_slice(&self.min_reader.to_le_bytes());
        out[8..10].copy_from_slice(&self.header_len.to_le_bytes());
        out[10..12].copy_from_slice(&self.flags.to_le_bytes());
        out[12..14].copy_from_slice(&self.engine.to_le_bytes());
        out[14..16].copy_from_slice(&self.min_engine.to_le_bytes());
        out[16..20].copy_from_slice(&self.total_len.to_le_bytes());
        out[20..24].copy_from_slice(&self.root.to_le_bytes());
        out[24..28].copy_from_slice(&self.str_dir.to_le_bytes());
        out[28..32].copy_from_slice(&self.str_count.to_le_bytes());
        out[32..36].copy_from_slice(&self.src.to_le_bytes());
        out[36..40].copy_from_slice(&self.src_len.to_le_bytes());
        out[40..44].copy_from_slice(&self.name.to_le_bytes());
        out[44..48].copy_from_slice(&self.checksum.to_le_bytes());
    }
}

/// Reads through `from_le_bytes` on a copied array rather than a cast, so no field in this format
/// has an alignment requirement. That is property 3 in the module docs and it is what lets an
/// artifact live wherever the linker put it.
#[inline]
pub fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

#[inline]
pub fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// FNV-1a, 32-bit. Chosen because it is eight lines and needs no table -- a device that wants to
/// check an artifact's integrity should not have to link a CRC table to do it.
pub fn checksum(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &byte in bytes {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// Reads an unsigned LEB128 varint, advancing `pos`.
///
/// Bounded at ten bytes. An unbounded loop over attacker-supplied or flash-corrupted bytes is
/// how a reader like this hangs, and the bound costs one comparison.
pub fn read_uvarint(bytes: &[u8], pos: &mut usize) -> Result<u64, FormatError> {
    let start = *pos;
    let mut value: u64 = 0;
    let mut shift = 0;
    loop {
        if *pos >= bytes.len() || shift > 63 {
            return Err(FormatError::BadVarint { at: start });
        }
        let byte = bytes[*pos];
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        shift += 7;
    }
}

/// Reads a `u32`-shaped varint, refusing one that does not fit.
#[inline]
pub fn read_u32varint(bytes: &[u8], pos: &mut usize) -> Result<u32, FormatError> {
    let start = *pos;
    let value = read_uvarint(bytes, pos)?;
    u32::try_from(value).map_err(|_| FormatError::BadVarint { at: start })
}

/// Writes an unsigned LEB128 varint.
///
/// Takes a `&mut Vec<u8>` from the CALLER's allocator rather than allocating: this crate has no
/// `alloc` of its own, which is what keeps the reader linkable on a device with no heap.
pub fn write_uvarint(out: &mut impl Extend<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.extend(core::iter::once(byte));
            return;
        }
        out.extend(core::iter::once(byte | 0x80));
    }
}

/// The number of bytes [`write_uvarint`] would emit. Used only by the size report.
#[must_use]
pub fn uvarint_len(mut value: u64) -> usize {
    let mut len = 1;
    while value >= 0x80 {
        value >>= 7;
        len += 1;
    }
    len
}

/// Zigzag, so a small negative number costs one byte rather than ten.
#[inline]
#[must_use]
pub fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

#[inline]
#[must_use]
pub fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Every node kind the format can hold.
///
/// **THE SET IS EXHAUSTIVE OVER `ast`, AND THAT IS LOAD-BEARING RATHER THAN THOROUGH.** A format
/// that covers most of the tree can only be measured on the programs it happens to encode, and a
/// measurement taken over the subset an instrument supports is a benchmark that cannot fail. The
/// round-trip test over the Test262 corpus is what holds this honest: a kind missing here shows up
/// as a refused program, not as a quietly smaller number.
///
/// Values are FROZEN. Appending is a version bump; renumbering is a version bump; a gap is fine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Tag {
    Program = 0,

    StmtExpression = 1,
    StmtBlock = 2,
    StmtEmpty = 3,
    StmtDeclaration = 4,
    StmtIf = 5,
    StmtWhile = 6,
    StmtDoWhile = 7,
    StmtFor = 8,
    StmtForIn = 9,
    StmtForOf = 10,
    StmtReturn = 11,
    StmtBreak = 12,
    StmtContinue = 13,
    StmtThrow = 14,
    StmtTry = 15,
    StmtLabeled = 16,
    StmtSwitch = 17,
    StmtFunction = 18,
    StmtClass = 19,
    StmtDebugger = 20,
    /// **ADDED IN FORMAT v2.** A v1 artifact can never contain one, so a v1 reader meeting this
    /// byte has met a v2 artifact -- which the header's `min_reader` now says before the walk.
    StmtWith = 21,

    ExprIdentifier = 32,
    /// An `f64` that is exactly a small integer, held as a zigzag varint. `-0.0` is NOT one of
    /// them; see `encode::number_tag` for why the test is on BITS rather than on value.
    ExprNumberSmall = 33,
    ExprNumber = 34,
    ExprString = 35,
    ExprBoolean = 36,
    ExprNull = 37,
    ExprThis = 38,
    ExprTemplate = 39,
    ExprTagged = 40,
    ExprRegExp = 41,
    ExprArray = 42,
    ExprObject = 43,
    ExprFunction = 44,
    ExprArrow = 45,
    ExprClass = 46,
    ExprSuper = 47,
    ExprUnary = 48,
    ExprUpdate = 49,
    ExprBinary = 50,
    ExprLogical = 51,
    ExprAssignment = 52,
    ExprConditional = 53,
    ExprCall = 54,
    ExprNew = 55,
    ExprMember = 56,
    ExprSequence = 57,
    ExprParenthesized = 58,
    /// **ADDED IN FORMAT v3.** `new.target` is a `MetaProperty` rather than an identifier. Encoding
    /// it as an identifier NAMED `new.target` -- a name no source text can produce -- is exact for
    /// reading the value back and wrong for every rule that asks what the operand IS.
    ExprNewTarget = 59,

    PatIdentifier = 70,
    PatMember = 71,
    PatArray = 72,
    PatObject = 73,
    PatDefault = 74,
    PatRest = 75,

    ForInitDeclaration = 80,
    ForInitExpression = 81,
    ForInitPattern = 82,

    KeyIdentifier = 90,
    KeyString = 91,
    KeyNumber = 92,
    KeyComputed = 93,
    KeyPrivate = 94,

    MemberIdentifier = 100,
    MemberComputed = 101,
    MemberPrivate = 102,

    ObjProperty = 110,
    ObjCoverInitializedName = 111,
    ObjMethod = 112,
    ObjSpread = 113,

    ElemHole = 120,
    ElemExpression = 121,
    ElemSpread = 122,

    ArgExpression = 130,
    ArgSpread = 131,

    ArrowBodyExpression = 140,
    ArrowBodyBlock = 141,

    TargetPattern = 150,
    TargetInvalid = 151,

    Declarator = 160,
    CatchClause = 161,
    SwitchCase = 162,
    Function = 163,
    Class = 164,
    ClassMember = 165,
    TemplateElement = 166,
    ObjectPatternProperty = 167,
    Arrow = 168,
}

impl Tag {
    /// The one place a byte becomes a [`Tag`]. Every unassigned value is an error rather than a
    /// silently-tolerated node, because a format that shrugs at a byte it does not know cannot
    /// detect the version skew the header exists to catch.
    pub fn from_u8(byte: u8) -> Option<Self> {
        use Tag::*;
        Some(match byte {
            0 => Program,
            1 => StmtExpression,
            2 => StmtBlock,
            3 => StmtEmpty,
            4 => StmtDeclaration,
            5 => StmtIf,
            6 => StmtWhile,
            7 => StmtDoWhile,
            8 => StmtFor,
            9 => StmtForIn,
            10 => StmtForOf,
            11 => StmtReturn,
            12 => StmtBreak,
            13 => StmtContinue,
            14 => StmtThrow,
            15 => StmtTry,
            16 => StmtLabeled,
            17 => StmtSwitch,
            18 => StmtFunction,
            19 => StmtClass,
            20 => StmtDebugger,
            21 => StmtWith,
            32 => ExprIdentifier,
            33 => ExprNumberSmall,
            34 => ExprNumber,
            35 => ExprString,
            36 => ExprBoolean,
            37 => ExprNull,
            38 => ExprThis,
            39 => ExprTemplate,
            40 => ExprTagged,
            41 => ExprRegExp,
            42 => ExprArray,
            43 => ExprObject,
            44 => ExprFunction,
            45 => ExprArrow,
            46 => ExprClass,
            47 => ExprSuper,
            48 => ExprUnary,
            49 => ExprUpdate,
            50 => ExprBinary,
            51 => ExprLogical,
            52 => ExprAssignment,
            53 => ExprConditional,
            54 => ExprCall,
            55 => ExprNew,
            56 => ExprMember,
            57 => ExprSequence,
            58 => ExprParenthesized,
            59 => ExprNewTarget,
            70 => PatIdentifier,
            71 => PatMember,
            72 => PatArray,
            73 => PatObject,
            74 => PatDefault,
            75 => PatRest,
            80 => ForInitDeclaration,
            81 => ForInitExpression,
            82 => ForInitPattern,
            90 => KeyIdentifier,
            91 => KeyString,
            92 => KeyNumber,
            93 => KeyComputed,
            94 => KeyPrivate,
            100 => MemberIdentifier,
            101 => MemberComputed,
            102 => MemberPrivate,
            110 => ObjProperty,
            111 => ObjCoverInitializedName,
            112 => ObjMethod,
            113 => ObjSpread,
            120 => ElemHole,
            121 => ElemExpression,
            122 => ElemSpread,
            130 => ArgExpression,
            131 => ArgSpread,
            140 => ArrowBodyExpression,
            141 => ArrowBodyBlock,
            150 => TargetPattern,
            151 => TargetInvalid,
            160 => Declarator,
            161 => CatchClause,
            162 => SwitchCase,
            163 => Function,
            164 => Class,
            165 => ClassMember,
            166 => TemplateElement,
            167 => ObjectPatternProperty,
            168 => Arrow,
            _ => return None,
        })
    }

    /// The bucket a tag belongs to, for the size report's per-kind census.
    #[must_use]
    pub fn category(self) -> &'static str {
        let byte = self as u8;
        match byte {
            0 => "program",
            1..=21 => "statement",
            32..=58 => "expression",
            70..=75 => "pattern",
            80..=82 => "for-init",
            90..=94 => "property-key",
            100..=102 => "member-property",
            110..=113 => "object-property",
            120..=122 => "array-element",
            130..=131 => "argument",
            140..=141 => "arrow-body",
            150..=151 => "assignment-target",
            _ => "struct",
        }
    }
}
