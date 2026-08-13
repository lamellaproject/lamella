//! The in-place reader: **the level a device ships, and the level the tier's price is measured at.**

use crate::format::{
    read_u32varint, read_uvarint, u32_at, unzigzag, FormatError, Header, Tag, FLAG_SOURCE,
    NO_STRING,
};

/// A precompiled program, borrowed wherever it lies.
#[derive(Debug, Clone, Copy)]
pub struct Artifact<'a> {
    bytes: &'a [u8],
    header: Header,
}

impl<'a> Artifact<'a> {
    /// Opens an artifact: magic, version, declared length. **O(1)** -- it reads the header and
    /// validates three fields, so a device pays nothing at boot for a program it has not run.
    ///
    /// It does NOT verify the checksum. That is O(n) and therefore the caller's decision; see
    /// [`Artifact::verify_checksum`].
    pub fn open(bytes: &'a [u8]) -> Result<Self, FormatError> {
        let header = Header::parse(bytes)?;
        Ok(Self { bytes, header })
    }

    #[must_use]
    pub fn header(&self) -> &Header {
        &self.header
    }

    #[must_use]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// FNV-1a over everything after the header, against the value the encoder stored.
    ///
    /// Separate from [`Artifact::open`] on purpose: a device that trusts its own flash should not
    /// pay a linear pass at every boot, and one that does not trust it -- an over-the-air update,
    /// an external SPI part -- should be able to ask.
    ///
    /// Spans from the ARTIFACT's own `header_len`, not from this build's
    /// [`HEADER_LEN`](crate::format::HEADER_LEN). A
    /// newer artifact with a longer header would otherwise checksum a different range than the
    /// encoder did and read as corrupt -- a version skew wearing a corruption error, which is
    /// exactly the confusion the version fields exist to prevent.
    pub fn verify_checksum(&self) -> Result<(), FormatError> {
        let computed = crate::format::checksum(&self.bytes[self.header.header_len as usize..]);
        if computed == self.header.checksum {
            Ok(())
        } else {
            Err(FormatError::ChecksumMismatch { declared: self.header.checksum, computed })
        }
    }

    /// **THE SECOND GATE, AND IT IS THE INTERPRETER'S TO RUN RATHER THAN THE READER'S.**
    ///
    /// [`Artifact::open`] settles whether these bytes can be PARSED. This settles whether the
    /// interpreter about to walk them implements what they MEAN. The two are independent: a build
    /// that adds `Symbol`, fixes a coercion or changes what an operator does has not moved a byte
    /// of the format, and an artifact compiled against it would open perfectly on an older engine
    /// and then quietly compute something else.
    ///
    /// It takes the running engine's version as an argument rather than reading a constant,
    /// because **this crate is the format and does not know the semantics.** The interpreter passes
    /// its own `lamella_js_frontend::ENGINE_VERSION`; a device with no parser still links that
    /// constant, and nothing else, to answer it.
    ///
    /// The comparison is deliberately CONSERVATIVE: the encoder stamps `min_engine` as the engine
    /// that compiled the program, so an older interpreter refuses even when the program happens to
    /// use nothing new. Refusing a program that would have run is recoverable and loud; running one
    /// that means something else is neither. Narrowing this needs real per-program feature
    /// analysis, which does not exist yet.
    pub fn check_engine(&self, running_engine: u16) -> Result<(), FormatError> {
        if self.header.min_engine > running_engine {
            return Err(FormatError::EngineTooOld {
                needs_engine: self.header.min_engine,
                this_engine: running_engine,
            });
        }
        Ok(())
    }

    /// Everything a diagnostic needs to explain a version disagreement, without a second `open`.
    #[must_use]
    pub fn versions(&self) -> Versions {
        Versions {
            format: self.header.version,
            min_reader: self.header.min_reader,
            engine: self.header.engine,
            min_engine: self.header.min_engine,
            this_reader: crate::format::FORMAT_VERSION,
        }
    }

    /// The root `Program` node.
    pub fn root(&self) -> Result<Node<'a>, FormatError> {
        Node::at(self.bytes, self.header.root as usize, self.bytes.len())
    }

    /// The node at a byte offset recorded earlier.
    ///
    /// THIS IS WHAT LETS A CLOSURE HOLD CODE WITHOUT HOLDING MEMORY. A function value records
    /// the OFFSET of its own node and nothing else; calling it re-derives the node by arithmetic.
    /// A `Node` is three offsets, so there is nothing to cache between calls and nothing resident
    /// per closure -- which is the whole per-closure win, and the reason `Closure` can stop owning
    /// a cloned `Vec<Statement>`.
    ///
    /// The offset must have come from [`Node::offset`] on THIS artifact. A bogus one is refused
    /// rather than misread: the tag is validated and the length is bounded by the artifact.
    pub fn node_at(&self, offset: usize) -> Result<Node<'a>, FormatError> {
        Node::at(self.bytes, offset, self.bytes.len())
    }

    /// The source text, when the artifact carries it.
    ///
    /// **THIS IS WHY DIAGNOSTICS SURVIVE PRECOMPILATION.** The Python bytecode carries
    /// no filename and no line numbers, so a program shipped without its source cannot produce a
    /// diagnostic that points at any. Here spans are on every node unconditionally, and the source
    /// text is an OPTIONAL section -- so a build picks its own point on the trade, and the honest
    /// floor (spans, no text) still yields an offset a host-side tool can resolve against the
    /// original file.
    ///
    /// It costs one flash byte per source byte and, being part of the artifact, costs no RAM.
    #[must_use]
    pub fn source(&self) -> Option<&'a str> {
        if self.header.flags & FLAG_SOURCE == 0 {
            return None;
        }
        let start = self.header.src as usize;
        let end = start.checked_add(self.header.src_len as usize)?;
        core::str::from_utf8(self.bytes.get(start..end)?).ok()
    }

    /// The name the program was compiled from, when one was given.
    pub fn filename(&self) -> Result<Option<&'a str>, FormatError> {
        if self.header.name == NO_STRING {
            return Ok(None);
        }
        self.str_utf8(self.header.name).map(Some)
    }

    /// The bytes of string `id`.
    pub fn str_bytes(&self, id: u32) -> Result<&'a [u8], FormatError> {
        if id >= self.header.str_count {
            return Err(FormatError::BadStringId {
                at: self.header.str_dir as usize,
                id,
                count: self.header.str_count,
            });
        }
        let entry = self.header.str_dir as usize + id as usize * 4;
        if entry + 4 > self.bytes.len() {
            return Err(FormatError::Truncated { at: entry, needed: 4, len: self.bytes.len() });
        }
        let mut at = u32_at(self.bytes, entry) as usize;
        let len = read_u32varint(self.bytes, &mut at)? as usize;
        let end = at.checked_add(len).ok_or(FormatError::BadStringId {
            at,
            id,
            count: self.header.str_count,
        })?;
        self.bytes
            .get(at..end)
            .ok_or(FormatError::Truncated { at, needed: len, len: self.bytes.len() })
    }

    /// String `id` as UTF-8 -- an identifier, a label, a RegExp body, a template's raw text.
    pub fn str_utf8(&self, id: u32) -> Result<&'a str, FormatError> {
        let bytes = self.str_bytes(id)?;
        core::str::from_utf8(bytes)
            .map_err(|_| FormatError::BadStringEncoding { at: self.header.str_dir as usize })
    }

    /// String `id` as ECMAScript String code units.
    ///
    /// **UTF-16 units, lone surrogates included** -- the reason `JsString` exists at all. The
    /// blob is a flat run of little-endian `u16`, so this iterates the flash slice without
    /// building anything.
    pub fn str_utf16(&self, id: u32) -> Result<Utf16<'a>, FormatError> {
        let bytes = self.str_bytes(id)?;
        if bytes.len() % 2 != 0 {
            return Err(FormatError::BadStringEncoding { at: self.header.str_dir as usize });
        }
        Ok(Utf16 { bytes, pos: 0 })
    }

    /// How many code units string `id` holds. O(1) -- half its blob length.
    pub fn str_utf16_len(&self, id: u32) -> Result<usize, FormatError> {
        Ok(self.str_bytes(id)?.len() / 2)
    }
}

/// What an artifact says about the versions it needs, beside what this build offers.
///
/// Exists so a REPL, a deploy tool or a device image can REPORT the disagreement rather than
/// only refuse it. "Update the interpreter" is far more actionable next to "the program wants
/// engine version 3 and this build is version 1".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Versions {
    /// The layout revision the artifact was written to.
    pub format: u16,
    /// The oldest reader that can read it.
    pub min_reader: u16,
    /// The engine that compiled it. Advisory.
    pub engine: u16,
    /// The oldest engine that can run it.
    pub min_engine: u16,
    /// What this build's reader speaks.
    pub this_reader: u16,
}

/// The code units of an ECMAScript String, read straight out of the artifact.
#[derive(Debug, Clone)]
pub struct Utf16<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Iterator for Utf16<'_> {
    type Item = u16;

    fn next(&mut self) -> Option<u16> {
        if self.pos + 2 > self.bytes.len() {
            return None;
        }
        let unit = u16::from_le_bytes([self.bytes[self.pos], self.bytes[self.pos + 1]]);
        self.pos += 2;
        Some(unit)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let left = (self.bytes.len() - self.pos) / 2;
        (left, Some(left))
    }
}

impl ExactSizeIterator for Utf16<'_> {}

/// A half-open source range, in UTF-8 byte offsets -- the same domain as `ast::Span`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

/// One node, borrowed in place.
///
/// A `Node` is three `usize`s and a tag. Constructing one copies nothing and allocates nothing;
/// [`Node::fields`] hands back a cursor over its payload.
#[derive(Debug, Clone, Copy)]
pub struct Node<'a> {
    bytes: &'a [u8],
    tag: Tag,
    /// Offset of the tag byte -- what a diagnostic about the ARTIFACT should quote.
    at: usize,
    payload: usize,
    payload_len: u32,
}

impl<'a> Node<'a> {
    /// Reads the node whose tag byte is at `at`, refusing one that overruns `limit`.
    fn at(bytes: &'a [u8], at: usize, limit: usize) -> Result<Self, FormatError> {
        if at >= limit || at >= bytes.len() {
            return Err(FormatError::Truncated { at, needed: 1, len: bytes.len() });
        }
        let raw = bytes[at];
        let tag = Tag::from_u8(raw).ok_or(FormatError::BadTag { at, tag: raw })?;
        let mut pos = at + 1;
        let payload_len = read_u32varint(bytes, &mut pos)?;
        let end = pos
            .checked_add(payload_len as usize)
            .ok_or(FormatError::BadNodeLength { at, len: payload_len })?;
        if end > limit {
            return Err(FormatError::BadNodeLength { at, len: payload_len });
        }
        Ok(Self { bytes, tag, at, payload: pos, payload_len })
    }

    #[must_use]
    pub fn tag(&self) -> Tag {
        self.tag
    }

    /// Where this node's tag byte lives, for an error about the artifact itself.
    #[must_use]
    pub fn offset(&self) -> usize {
        self.at
    }

    /// **O(1).** The offset just past this node -- what makes skipping a subtree free.
    #[must_use]
    pub fn end(&self) -> usize {
        self.payload + self.payload_len as usize
    }

    /// The node's own encoded size, tag and length field included.
    #[must_use]
    pub fn encoded_len(&self) -> usize {
        self.end() - self.at
    }

    /// A cursor over this node's payload.
    #[must_use]
    pub fn fields(&self) -> Fields<'a> {
        Fields { bytes: self.bytes, pos: self.payload, end: self.end() }
    }

    /// Refuses a node that is not the tag this position requires.
    pub fn expect(&self, tag: Tag, expected: &'static str) -> Result<(), FormatError> {
        if self.tag == tag {
            Ok(())
        } else {
            Err(FormatError::UnexpectedTag { at: self.at, tag: self.tag as u8, expected })
        }
    }
}

/// A sequential cursor over one node's payload.
///
/// Reads are ORDERED and must match the encoder exactly. That is the format's one real
/// discipline, and it is why `encode.rs` and `decode.rs` are written as adjacent mirror images:
/// a field added to one and forgotten in the other is caught by the round-trip test on the first
/// program that uses it, not by review.
#[derive(Debug, Clone, Copy)]
pub struct Fields<'a> {
    bytes: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Fields<'a> {
    /// Whether anything is left. A payload with bytes still in it after the reader is done is a
    /// desync between encoder and reader -- see [`Fields::finish`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos >= self.end
    }

    /// Asserts the payload was consumed exactly. `decode.rs` calls this on every node, which
    /// turns "the encoder wrote a field the decoder does not read" from a silent misparse of the
    /// NEXT node into a named error at the node that caused it.
    pub fn finish(&self, at: usize) -> Result<(), FormatError> {
        if self.pos == self.end {
            Ok(())
        } else {
            Err(FormatError::BadNodeLength { at, len: (self.end - self.pos) as u32 })
        }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], FormatError> {
        let next = self.pos.checked_add(n).ok_or(FormatError::Truncated {
            at: self.pos,
            needed: n,
            len: self.end,
        })?;
        if next > self.end {
            return Err(FormatError::Truncated { at: self.pos, needed: n, len: self.end });
        }
        let slice = &self.bytes[self.pos..next];
        self.pos = next;
        Ok(slice)
    }

    pub fn byte(&mut self) -> Result<u8, FormatError> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool, FormatError> {
        Ok(self.byte()? != 0)
    }

    pub fn u32(&mut self) -> Result<u32, FormatError> {
        let mut pos = self.pos;
        let value = read_u32varint(self.bytes, &mut pos)?;
        if pos > self.end {
            return Err(FormatError::Truncated { at: self.pos, needed: pos - self.pos, len: self.end });
        }
        self.pos = pos;
        Ok(value)
    }

    pub fn u64(&mut self) -> Result<u64, FormatError> {
        let mut pos = self.pos;
        let value = read_uvarint(self.bytes, &mut pos)?;
        if pos > self.end {
            return Err(FormatError::Truncated { at: self.pos, needed: pos - self.pos, len: self.end });
        }
        self.pos = pos;
        Ok(value)
    }

    pub fn i64(&mut self) -> Result<i64, FormatError> {
        Ok(unzigzag(self.u64()?))
    }

    /// A count, for a run of repeated items.
    pub fn count(&mut self) -> Result<u32, FormatError> {
        self.u32()
    }

    /// A string id.
    pub fn str_id(&mut self) -> Result<u32, FormatError> {
        self.u32()
    }

    /// The span is `(start, end - start)`, not `(start, end)`. A node's extent is small where its
    /// offset is not, so the second varint is almost always one byte instead of three.
    pub fn span(&mut self) -> Result<Span, FormatError> {
        let start = self.u32()?;
        let len = self.u32()?;
        Ok(Span { start, end: start.saturating_add(len) })
    }

    /// The eight bytes of an `f64`, **uninterpreted**.
    ///
    /// Handed back as bits rather than as a float on purpose: it keeps this module free of
    /// floating point entirely, so a no-float device profile can link the reader, and it makes the
    /// round-trip BIT-exact -- which is the only kind of exact that distinguishes `-0.0` from
    /// `0.0`, and `NaN` payloads from each other.
    pub fn f64_bits(&mut self) -> Result<u64, FormatError> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    /// The next child node. Advances past it.
    pub fn child(&mut self) -> Result<Node<'a>, FormatError> {
        let node = Node::at(self.bytes, self.pos, self.end)?;
        self.pos = node.end();
        Ok(node)
    }

    /// The next child, when a presence byte says there is one.
    pub fn option_child(&mut self) -> Result<Option<Node<'a>>, FormatError> {
        if self.bool()? {
            Ok(Some(self.child()?))
        } else {
            Ok(None)
        }
    }

    /// Steps over the next child without looking inside it. **O(1).**
    pub fn skip_child(&mut self) -> Result<(), FormatError> {
        let node = Node::at(self.bytes, self.pos, self.end)?;
        self.pos = node.end();
        Ok(())
    }
}
