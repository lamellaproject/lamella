//! The allocation-free collection over a heap of SEVERAL SEGMENTS: regions at unrelated addresses,
//! such as a part's SRAM and a PSRAM that sits below it in the address map. One mark pass covers
//! every segment, each segment compacts in place, and no object moves from one segment to another.
//!
//! It is the one-segment allocation-free collection generalized, phase for phase: the same mark
//! with its storage-free finish, the same forwarding by replay, every reference rewritten before
//! anything moves, the same free blocks below a pin and the same zeroed tail. What changes is that
//! an address is first resolved to the segment that holds it. A reference may point from any
//! segment into any other, so marking and rewriting look the target up; walking an object, finding
//! where it lands and moving it all stay inside its own segment.
//!
//! A segment keeps its own bump pointer, its own free chain and its own mark bitmap, so each one is
//! a heap of the one-segment shape, and the collection answers one of each per segment.
//!
//! # The forwarding index
//!
//! Where a survivor lands is the cursor after every survivor below it has been placed, and the
//! one-segment collection finds it by replaying the segment from its base, once per reference it
//! rewrites. That costs the number of references times the number of objects, which a few kilobytes
//! of SRAM can afford and megabytes of PSRAM behind a cache that flash shares cannot: a collection
//! there grows with the square of what the segment holds, and reaches minutes. So a segment may carry
//! a FORWARDING INDEX: for each [`INDEX_CHUNK`]-byte chunk, the first object that starts in or after
//! it and where the replay's cursor stands there, built in one walk after marking. A lookup then
//! replays one chunk instead of the segment. The index changes where the replay starts and nothing
//! else, so a collection with one answers exactly what it answers without; a segment without one is
//! replayed from its base, as before.
//!
//! No step here makes an exclusive or atomic access to a heap word or a bitmap word. That is a
//! property to keep: some parts support exclusive accesses only in SRAM, and on the RP2350 a
//! store-exclusive to any other memory still writes and reports failure (datasheet 2.1.6), so a
//! load/store-exclusive loop on a PSRAM word would corrupt the word and never finish.

use crate::heap::{
    ALIGN, Compacted, FreeChain, HEADER_SIZE, MARK_STACK, MAX_LEADING, MarkBits, Ref, RootKind,
    TypeResolver, align_up, free_block_length, pinned_within,
};

/// One segment of a heap as a collection sees it: its storage, the address its first byte has, and
/// its bump pointer.
pub(crate) struct Segment<'a> {
    /// The segment's bytes, which the collection reads, rewrites and compacts.
    pub(crate) bytes: &'a mut [u8],
    /// The address of `bytes[0]`. An object in this segment is addressed `base + offset`.
    pub(crate) base: u32,
    /// The bump pointer, as an offset from `base`: nothing is allocated at or above it.
    pub(crate) top: u32,
    /// The segment's forwarding index, of [`forwarding_index_words_for`] words for its region, which
    /// the collection fills; anything shorter, the empty slice included, means none.
    pub(crate) index: &'a mut [u32],
}

/// The bytes of a segment one entry of its forwarding index covers. A lookup replays at most this
/// much of the segment: 32 objects at the smallest footprint.
pub(crate) const INDEX_CHUNK: u32 = 256;

/// How many `u32` words a forwarding index for a segment of `region_len` bytes needs: two per
/// [`INDEX_CHUNK`] bytes, about 3% of the region, as much as its mark bitmap.
///
/// **Public because the embedder owns the storage and therefore sizes it**, as it sizes the bitmap.
#[must_use]
pub const fn forwarding_index_words_for(region_len: usize) -> usize {
    region_len.div_ceil(INDEX_CHUNK as usize) * 2
}

impl Segment<'_> {
    /// The address one past the last allocated byte.
    fn end(&self) -> u32 {
        self.base + self.top
    }

    /// Whether `reference` is a payload address this segment's allocator can have handed out. The
    /// lower bound is inclusive: the smallest payload is one header above the first object start.
    fn holds(&self, reference: u32) -> bool {
        reference >= self.base + HEADER_SIZE && reference < self.end()
    }

    /// The little-endian word at address `addr`, which lies in this segment.
    fn word(&self, addr: u32) -> u32 {
        let at = (addr - self.base) as usize;
        u32::from_le_bytes([
            self.bytes[at],
            self.bytes[at + 1],
            self.bytes[at + 2],
            self.bytes[at + 3],
        ])
    }

    /// Writes `value` to the word at address `addr`, which lies in this segment.
    fn put(&mut self, addr: u32, value: u32) {
        let at = (addr - self.base) as usize;
        self.bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Whether this segment has a forwarding index long enough to use.
    fn indexed(&self) -> bool {
        self.index.len() >= forwarding_index_words_for(self.bytes.len())
    }

    /// Whether this segment's REGION, allocated or not, shares a byte with `other`'s.
    fn overlaps(&self, other: &Segment<'_>) -> bool {
        let (a, b) = (u64::from(self.base), u64::from(other.base));
        a < b + other.bytes.len() as u64 && b < a + self.bytes.len() as u64
    }
}

/// The segment whose objects include the payload `reference`, if any. A reference outside every
/// segment -- a literal in flash, a type object -- is not the heap's, and is skipped.
fn segment_of(segments: &[Segment<'_>], reference: u32) -> Option<usize> {
    segments.iter().position(|segment| segment.holds(reference))
}

/// Reads the leading payload words an object's footprint depends on into `buf`, clamped to
/// `MAX_LEADING` and to what lies below its segment's top, and answers how many it read.
fn read_leading(
    segment: &Segment<'_>,
    payload: u32,
    want: u32,
    buf: &mut [u32; MAX_LEADING],
) -> usize {
    let available = ((segment.end() - payload) / 4) as usize;
    let n = (want as usize).min(MAX_LEADING).min(available);
    for (i, slot) in buf.iter_mut().enumerate().take(n) {
        *slot = segment.word(payload + (i as u32) * 4);
    }
    n
}

/// The footprint of the object, or the free block, whose payload is at `payload` in `segment`: the
/// header plus the payload rounded to [`ALIGN`].
fn footprint(segment: &Segment<'_>, resolver: &dyn TypeResolver, payload: u32) -> u32 {
    let header_word = segment.word(Ref(payload).header_addr());
    if let Some(length) = free_block_length(header_word) {
        return length;
    }
    let mut leading = [0u32; MAX_LEADING];
    let n = read_leading(
        segment,
        payload,
        resolver.leading_words(header_word),
        &mut leading,
    );
    HEADER_SIZE + align_up(resolver.payload_size(header_word, &leading[..n]))
}

/// The payload of the object an interior address lies in: the one whose range, from its payload up
/// to the next object's payload, holds the address, found by walking the segment that holds it. An
/// address inside a free block, or in no segment's objects, has no owner.
fn owner_of(segments: &[Segment<'_>], resolver: &dyn TypeResolver, address: u32) -> Option<u32> {
    let segment = segments.iter().find(|segment| {
        address >= segment.base + ALIGN + HEADER_SIZE && address <= segment.end()
    })?;
    let top = segment.end();
    let mut payload = segment.base + ALIGN + HEADER_SIZE;
    while payload < top {
        let step = footprint(segment, resolver, payload);
        if address < payload + step {
            let header_word = segment.word(Ref(payload).header_addr());
            return free_block_length(header_word).is_none().then_some(payload);
        }
        payload += step;
    }
    None
}

/// Where the survivor whose payload is `want` lands: the cursor after every survivor below it in
/// ITS OWN segment has been placed, replayed by a walk from that segment's base -- or, when the
/// segment has a forwarding index, from the first object of the chunk `want`'s object starts in, with
/// the cursor the index recorded there. A pinned survivor stays where it is and the cursor resumes
/// above it. Answers 0 for an object that did not survive, and for an address no object starts at.
fn forwarded(
    segments: &[Segment<'_>],
    marks: &[MarkBits<'_>],
    resolver: &dyn TypeResolver,
    pinned: &[u32],
    want: u32,
) -> u32 {
    let Some(home) = segment_of(segments, want) else {
        return 0;
    };
    let (segment, bits) = (&segments[home], &marks[home]);
    let top = segment.end();
    let (mut cursor, mut payload) = if segment.indexed() {
        let start = want - HEADER_SIZE - segment.base;
        let chunk = (start / INDEX_CHUNK) as usize;
        let first = segment.index[2 * chunk];
        if first == u32::MAX || first > start {
            return 0;
        }
        (
            segment.base + segment.index[2 * chunk + 1],
            segment.base + first + HEADER_SIZE,
        )
    } else {
        (segment.base + ALIGN, segment.base + ALIGN + HEADER_SIZE)
    };
    while payload < top && payload <= want {
        let step = footprint(segment, resolver, payload);
        if bits.test(segment.base, payload) {
            if pinned_within(pinned, payload, step) {
                if payload == want {
                    return payload;
                }
                cursor = payload - HEADER_SIZE + step;
            } else {
                if payload == want {
                    return cursor + HEADER_SIZE;
                }
                cursor += step;
            }
        }
        payload += step;
    }
    0
}

/// Fills `index` for `segment` once its marks are final: for each chunk, the offset of the first
/// object that starts at or after the chunk's start, and the replay cursor's offset there -- exactly
/// what [`forwarded`]'s walk from the base would have reached. A chunk past the last object gets
/// `u32::MAX`, which no lookup reaches, since no object starts there.
fn build_index(
    segment: &Segment<'_>,
    bits: &MarkBits<'_>,
    resolver: &dyn TypeResolver,
    pinned: &[u32],
    index: &mut [u32],
) {
    let chunks = index.len() / 2;
    let top = segment.end();
    let mut next = 0usize;
    let mut cursor = segment.base + ALIGN;
    let mut payload = segment.base + ALIGN + HEADER_SIZE;
    while payload < top {
        let start = payload - HEADER_SIZE - segment.base;
        while next < chunks && (next as u64) * u64::from(INDEX_CHUNK) <= u64::from(start) {
            index[2 * next] = start;
            index[2 * next + 1] = cursor - segment.base;
            next += 1;
        }
        let step = footprint(segment, resolver, payload);
        if bits.test(segment.base, payload) {
            if pinned_within(pinned, payload, step) {
                cursor = payload - HEADER_SIZE + step;
            } else {
                cursor += step;
            }
        }
        payload += step;
    }
    for chunk in next..chunks {
        index[2 * chunk] = u32::MAX;
    }
}

/// Collects a heap of several segments without allocating: marks from the roots across every
/// segment, rewrites every reference, then compacts each segment in place. On success it writes
/// each segment's new top and the head of its free chain into `answers` (offsets from that
/// segment's base, as the one-segment collection answers them) and answers `true`.
///
/// `marks` holds one bitmap per segment, each covering its segment's whole region, and `answers`
/// one slot per segment. A segment's forwarding index, when it has one, is rebuilt from scratch, so
/// it needs no clearing between collections. `enumerate_roots` is called twice, once to mark and once to rewrite, and
/// must report the same slots both times. `pinned` names the addresses whose objects must not move;
/// they may lie in any segment.
///
/// **Answers `false` having touched nothing** when a bitmap is short of its segment, when the counts
/// disagree, or when two segments' regions overlap: an address in two segments has no single owner,
/// and a collector that cannot prove an object dead must not reclaim it.
pub(crate) fn mark_compact_segments<R>(
    segments: &mut [Segment<'_>],
    marks: &mut [MarkBits<'_>],
    resolver: &dyn TypeResolver,
    mut enumerate_roots: R,
    pinned: &[u32],
    answers: &mut [Compacted],
) -> bool
where
    R: FnMut(&mut dyn FnMut(&mut Ref, RootKind)),
{
    if marks.len() != segments.len() || answers.len() != segments.len() {
        return false;
    }
    let short = segments
        .iter()
        .zip(marks.iter())
        .any(|(segment, bits)| MarkBits::words_for(segment.bytes.len()) > bits.capacity_words());
    if short {
        return false;
    }
    for (i, segment) in segments.iter().enumerate() {
        if segments[i + 1..]
            .iter()
            .any(|other| segment.overlaps(other))
        {
            return false;
        }
    }

    // ---------------------------------------------------------------- MARK
    {
        let view: &[Segment<'_>] = segments;
        let mut stack = [0u32; MARK_STACK];
        let mut depth = 0usize;
        let mut overflowed = false;

        macro_rules! mark {
            ($reference:expr, $marks:expr, $stack:expr, $depth:expr, $overflowed:expr) => {{
                let reference: Ref = $reference;
                if !reference.is_null() {
                    if let Some(home) = segment_of(view, reference.0) {
                        if $marks[home].set(view[home].base, reference.0) {
                            if $depth < MARK_STACK {
                                $stack[$depth] = reference.0;
                                $depth += 1;
                            } else {
                                $overflowed = true;
                            }
                        }
                    }
                }
            }};
        }

        enumerate_roots(&mut |slot: &mut Ref, kind: RootKind| match kind {
            RootKind::Object => mark!(*slot, marks, stack, depth, overflowed),
            RootKind::Interior => {
                if let Some(owner) = owner_of(view, resolver, slot.0) {
                    mark!(Ref(owner), marks, stack, depth, overflowed);
                }
            }
        });

        macro_rules! scan {
            ($payload:expr, $marks:expr, $stack:expr, $depth:expr, $overflowed:expr) => {{
                let payload: u32 = $payload;
                if let Some(home) = segment_of(view, payload) {
                    let segment = &view[home];
                    let header_word = segment.word(Ref(payload).header_addr());
                    let mut leading = [0u32; MAX_LEADING];
                    let n = read_leading(
                        segment,
                        payload,
                        resolver.leading_words(header_word),
                        &mut leading,
                    );
                    resolver.for_each_ref_offset(header_word, &leading[..n], &mut |ref_offset| {
                        let child = Ref(segment.word(payload + ref_offset));
                        mark!(child, $marks, $stack, $depth, $overflowed);
                    });
                    resolver.for_each_tagged_offset(header_word, &mut |tagged_offset| {
                        let word = segment.word(payload + tagged_offset);
                        if word != 0 && word & 0b11 == 0 {
                            mark!(Ref(word), $marks, $stack, $depth, $overflowed);
                        }
                    });
                }
            }};
        }

        while depth > 0 {
            depth -= 1;
            let payload = stack[depth];
            scan!(payload, marks, stack, depth, overflowed);
        }

        while overflowed {
            overflowed = false;
            let mut added = false;
            for (home, segment) in view.iter().enumerate() {
                let top = segment.end();
                let mut payload = segment.base + ALIGN + HEADER_SIZE;
                while payload < top {
                    let step = footprint(segment, resolver, payload);
                    if marks[home].test(segment.base, payload) {
                        let before = depth;
                        scan!(payload, marks, stack, depth, overflowed);
                        if depth != before {
                            added = true;
                        }
                        while depth > 0 {
                            depth -= 1;
                            let next = stack[depth];
                            scan!(next, marks, stack, depth, overflowed);
                            added = true;
                        }
                    }
                    payload += step;
                }
            }
            if !added {
                break;
            }
        }
    }

    // ---------------------------------------------------------- INDEX
    for home in 0..segments.len() {
        if segments[home].indexed() {
            let index = core::mem::take(&mut segments[home].index);
            build_index(&segments[home], &marks[home], resolver, pinned, index);
            segments[home].index = index;
        }
    }

    // -------------------------------------------------------- REWRITE
    {
        let view: &[Segment<'_>] = segments;
        let bits: &[MarkBits<'_>] = marks;
        enumerate_roots(&mut |slot: &mut Ref, kind: RootKind| match kind {
            RootKind::Object => {
                if !slot.is_null() && segment_of(view, slot.0).is_some() {
                    slot.0 = forwarded(view, bits, resolver, pinned, slot.0);
                }
            }
            RootKind::Interior => {
                if let Some(owner) = owner_of(view, resolver, slot.0) {
                    slot.0 = forwarded(view, bits, resolver, pinned, owner) + (slot.0 - owner);
                }
            }
        });
    }

    for home in 0..segments.len() {
        let base = segments[home].base;
        let top = segments[home].end();
        let mut payload = base + ALIGN + HEADER_SIZE;
        while payload < top {
            let step = footprint(&segments[home], resolver, payload);
            if marks[home].test(base, payload) {
                let header_word = segments[home].word(Ref(payload).header_addr());
                let mut leading = [0u32; MAX_LEADING];
                let n = read_leading(
                    &segments[home],
                    payload,
                    resolver.leading_words(header_word),
                    &mut leading,
                );
                resolver.for_each_ref_offset(header_word, &leading[..n], &mut |ref_offset| {
                    let slot = payload + ref_offset;
                    let old = segments[home].word(slot);
                    if old != 0 && segment_of(segments, old).is_some() {
                        let new = forwarded(segments, marks, resolver, pinned, old);
                        segments[home].put(slot, new);
                    }
                });
                resolver.for_each_tagged_offset(header_word, &mut |tagged_offset| {
                    let slot = payload + tagged_offset;
                    let old = segments[home].word(slot);
                    if old != 0 && old & 0b11 == 0 && segment_of(segments, old).is_some() {
                        let new = forwarded(segments, marks, resolver, pinned, old);
                        segments[home].put(slot, new);
                    }
                });
                resolver.for_each_weak_offset(header_word, &mut |weak_offset| {
                    let slot = payload + weak_offset;
                    let old = segments[home].word(slot);
                    if old == 0 {
                        return;
                    }
                    if let Some(target) = segment_of(segments, old) {
                        let new = if marks[target].test(segments[target].base, old) {
                            forwarded(segments, marks, resolver, pinned, old)
                        } else {
                            0
                        };
                        segments[home].put(slot, new);
                    }
                });
            }
            payload += step;
        }
    }

    // ----------------------------------------------------------- MOVE
    for (home, segment) in segments.iter_mut().enumerate() {
        let base = segment.base;
        let top = segment.end();
        let bits = &marks[home];
        let mut cursor = base + ALIGN;
        let mut free = FreeChain::default();
        let mut payload = base + ALIGN + HEADER_SIZE;
        while payload < top {
            let step = footprint(segment, resolver, payload);
            if bits.test(base, payload) {
                let start = payload - HEADER_SIZE;
                if pinned_within(pinned, payload, step) {
                    if cursor < start {
                        free.add(
                            segment.bytes,
                            (cursor - base) as usize,
                            (start - base) as usize,
                        );
                    }
                    cursor = start + step;
                } else {
                    if cursor != start {
                        let from = (start - base) as usize;
                        segment
                            .bytes
                            .copy_within(from..from + step as usize, (cursor - base) as usize);
                    }
                    cursor += step;
                }
            }
            payload += step;
        }
        segment.bytes[(cursor - base) as usize..(top - base) as usize].fill(0);
        answers[home] = Compacted {
            top: cursor - base,
            free: free.head,
        };
    }
    true
}

#[cfg(all(test, feature = "host-heap"))]
mod tests {
    extern crate alloc;

    use super::*;
    use crate::heap::{FREE_BLOCK, TableResolver, TypeDesc};
    use alloc::collections::BTreeMap;
    use alloc::vec;
    use alloc::vec::Vec;

    /// The RP2350's layout, which is the reason this engine exists: the FIRST segment (SRAM) lies
    /// ABOVE the second (PSRAM) in the address map. Every test here uses it, so none can pass on an
    /// engine that assumes segments arrive in address order.
    const FIRST_BASE: u32 = 0x2000_0000;
    const SECOND_BASE: u32 = 0x1100_0000;

    /// A type with no references and a one-word payload.
    fn leaf() -> TypeDesc {
        TypeDesc {
            payload_size: 4,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        }
    }

    /// A type whose one-word payload is a reference.
    fn one_ref() -> TypeDesc {
        TypeDesc {
            payload_size: 4,
            ref_offsets: vec![0],
            tagged_offsets: Vec::new(),
        }
    }

    /// A type of `n` references, which overflows the mark stack when `n` passes it.
    fn many_refs(n: u32) -> TypeDesc {
        TypeDesc {
            payload_size: 4 * n,
            ref_offsets: (0..n).map(|i| 4 * i).collect(),
            tagged_offsets: Vec::new(),
        }
    }

    const LEAF: u32 = 0;
    const ONE_REF: u32 = 1;

    /// What one test collection leaves: both regions, the roots as rewritten, and both answers.
    type Collected = ([Vec<u8>; 2], Vec<(Ref, RootKind)>, [Compacted; 2]);

    /// Two segments built the way the allocator builds them, with table descriptors.
    struct Heap2 {
        regions: [Vec<u8>; 2],
        bases: [u32; 2],
        tops: [u32; 2],
        descs: Vec<TypeDesc>,
        weak: BTreeMap<u32, Vec<u32>>,
    }

    impl Heap2 {
        fn new(len: usize, descs: Vec<TypeDesc>) -> Heap2 {
            Heap2 {
                regions: [vec![0u8; len], vec![0u8; len]],
                bases: [FIRST_BASE, SECOND_BASE],
                tops: [ALIGN, ALIGN],
                descs,
                weak: BTreeMap::new(),
            }
        }

        /// Bump-allocates an object of type `ty` in segment `seg`, answering its payload address.
        fn alloc(&mut self, seg: usize, ty: u32) -> Ref {
            let start = self.tops[seg];
            let size = HEADER_SIZE + align_up(self.descs[ty as usize].payload_size);
            assert!(
                (start + size) as usize <= self.regions[seg].len(),
                "test heap too small"
            );
            self.regions[seg][start as usize..start as usize + 4]
                .copy_from_slice(&ty.to_le_bytes());
            self.tops[seg] = start + size;
            Ref(self.bases[seg] + start + HEADER_SIZE)
        }

        fn locate(&self, addr: u32) -> (usize, usize) {
            let seg = (0..2)
                .find(|&s| {
                    addr >= self.bases[s]
                        && ((addr - self.bases[s]) as usize) < self.regions[s].len()
                })
                .expect("an address in a test segment");
            (seg, (addr - self.bases[seg]) as usize)
        }

        fn read(&self, addr: u32) -> u32 {
            let (seg, at) = self.locate(addr);
            u32::from_le_bytes(self.regions[seg][at..at + 4].try_into().unwrap())
        }

        fn write(&mut self, addr: u32, value: u32) {
            let (seg, at) = self.locate(addr);
            self.regions[seg][at..at + 4].copy_from_slice(&value.to_le_bytes());
        }

        /// Collects with `roots` (rewritten in place) and `pinned`, answering each segment's answer --
        /// TWICE, with each segment's forwarding index and without, which must agree on every byte,
        /// every root and every answer: the index changes where a lookup starts and nothing else.
        fn collect(&mut self, roots: &mut [(Ref, RootKind)], pinned: &[u32]) -> [Compacted; 2] {
            let (plain_regions, plain_roots, plain_answers) = self.run(roots, pinned, false);
            let (regions, indexed_roots, answers) = self.run(roots, pinned, true);
            assert_eq!(
                answers, plain_answers,
                "the forwarding index changed an answer"
            );
            assert_eq!(
                indexed_roots, plain_roots,
                "the forwarding index changed where a root points"
            );
            assert_eq!(
                regions, plain_regions,
                "the forwarding index changed the heap"
            );
            self.regions = regions;
            roots.copy_from_slice(&indexed_roots);
            self.tops = [answers[0].top, answers[1].top];
            answers
        }

        /// One collection over copies of both segments, with forwarding indexes or without.
        fn run(&self, roots: &[(Ref, RootKind)], pinned: &[u32], indexed: bool) -> Collected {
            let resolver = TableResolver {
                type_descs: &self.descs,
                weak_offsets: &self.weak,
            };
            let mut regions = self.regions.clone();
            let mut roots = roots.to_vec();
            let mut answers = [Compacted { top: 0, free: 0 }; 2];
            {
                let mut first_storage = vec![0u32; MarkBits::words_for(regions[0].len())];
                let mut second_storage = vec![0u32; MarkBits::words_for(regions[1].len())];
                let mut marks = [
                    MarkBits::new(&mut first_storage),
                    MarkBits::new(&mut second_storage),
                ];
                let words = |len: usize| {
                    if indexed {
                        forwarding_index_words_for(len)
                    } else {
                        0
                    }
                };
                let mut first_index = vec![0u32; words(regions[0].len())];
                let mut second_index = vec![0u32; words(regions[1].len())];
                let [first, second] = &mut regions;
                let mut segments = [
                    Segment {
                        bytes: first,
                        base: self.bases[0],
                        top: self.tops[0],
                        index: &mut first_index,
                    },
                    Segment {
                        bytes: second,
                        base: self.bases[1],
                        top: self.tops[1],
                        index: &mut second_index,
                    },
                ];
                let ran = mark_compact_segments(
                    &mut segments,
                    &mut marks,
                    &resolver,
                    |visit: &mut dyn FnMut(&mut Ref, RootKind)| {
                        for (root, kind) in roots.iter_mut() {
                            visit(root, *kind);
                        }
                    },
                    pinned,
                    &mut answers,
                );
                assert!(ran, "the collection refused");
            }
            (regions, roots, answers)
        }
    }

    fn object_roots(roots: &[Ref]) -> Vec<(Ref, RootKind)> {
        roots.iter().map(|&root| (root, RootKind::Object)).collect()
    }

    /// MARKING CROSSES SEGMENTS BOTH WAYS. An object in the first segment reaches one in the second,
    /// which reaches back into the first; only the first is a root. All three survive, the garbage
    /// in both segments is reclaimed, and each reference names its target's new place.
    #[test]
    fn marking_crosses_segments_both_ways_and_each_segment_compacts() {
        let mut heap = Heap2::new(256, vec![leaf(), one_ref()]);
        let _dead_first = heap.alloc(0, LEAF);
        let a = heap.alloc(0, ONE_REF);
        let _dead_second = heap.alloc(1, ONE_REF);
        let b = heap.alloc(1, ONE_REF);
        let c = heap.alloc(0, LEAF);
        heap.write(a.0, b.0);
        heap.write(b.0, c.0);
        heap.write(c.0, 0xC0FF_EE00);

        let mut roots = object_roots(&[a]);
        let answers = heap.collect(&mut roots, &[]);

        let a = roots[0].0;
        assert_eq!(
            answers[0].top,
            ALIGN + 8 + 8,
            "the first segment keeps a and c"
        );
        assert_eq!(answers[1].top, ALIGN + 8, "the second segment keeps b");
        assert_eq!(
            a,
            Ref(FIRST_BASE + ALIGN + HEADER_SIZE),
            "a slides to the first segment's floor"
        );
        let b = Ref(heap.read(a.0));
        assert_eq!(
            b,
            Ref(SECOND_BASE + ALIGN + HEADER_SIZE),
            "a's field names b in the SECOND segment"
        );
        let c = Ref(heap.read(b.0));
        assert_eq!(
            c,
            Ref(FIRST_BASE + ALIGN + 8 + HEADER_SIZE),
            "b's field names c back in the first"
        );
        assert_eq!(heap.read(c.0), 0xC0FF_EE00, "c's payload moved with it");
    }

    /// NO OBJECT MOVES BETWEEN SEGMENTS: the second segment is emptier than the first could hold, and
    /// its survivor still compacts inside the second.
    #[test]
    fn a_survivor_stays_in_its_own_segment() {
        let mut heap = Heap2::new(256, vec![leaf(), one_ref()]);
        let first = heap.alloc(0, LEAF);
        let _dead = heap.alloc(1, LEAF);
        let second = heap.alloc(1, LEAF);
        let mut roots = object_roots(&[first, second]);
        heap.collect(&mut roots, &[]);
        assert_eq!(
            roots[0].0.0, first.0,
            "the first segment's survivor was already packed"
        );
        assert_eq!(
            roots[1].0.0,
            SECOND_BASE + ALIGN + HEADER_SIZE,
            "the second's survivor packs in the second"
        );
    }

    /// A ROOT INTO EACH SEGMENT, AND AN INTERIOR ROOT, are rewritten to their objects' new places, and
    /// an interior root keeps the object it points into alive.
    #[test]
    fn roots_into_each_segment_are_rewritten() {
        let wide = TypeDesc {
            payload_size: 12,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        };
        let mut heap = Heap2::new(256, vec![leaf(), one_ref(), wide]);
        let _dead_first = heap.alloc(0, LEAF);
        let in_first = heap.alloc(0, LEAF);
        let _dead_second = heap.alloc(1, LEAF);
        let held = heap.alloc(1, 2);
        heap.write(held.0 + 8, 0x1234_5678);
        let interior = held.0 + 8;

        let mut roots = vec![
            (in_first, RootKind::Object),
            (Ref(interior), RootKind::Interior),
        ];
        heap.collect(&mut roots, &[]);

        assert_eq!(roots[0].0.0, FIRST_BASE + ALIGN + HEADER_SIZE);
        let new_held = SECOND_BASE + ALIGN + HEADER_SIZE;
        assert_eq!(
            roots[1].0.0,
            new_held + 8,
            "the interior root keeps its offset inside the moved object"
        );
        assert_eq!(
            heap.read(roots[1].0.0),
            0x1234_5678,
            "and still reads its field"
        );
    }

    /// A PIN IN EACH SEGMENT LEAVES A FREE CHAIN IN EACH, headed in that segment's own answer as an
    /// offset from that segment's base.
    #[test]
    fn a_pin_in_each_segment_leaves_a_free_chain_in_each() {
        let mut heap = Heap2::new(256, vec![leaf(), one_ref()]);
        let _dead_first = heap.alloc(0, LEAF);
        let pinned_first = heap.alloc(0, LEAF);
        let _dead_second = heap.alloc(1, LEAF);
        let pinned_second = heap.alloc(1, LEAF);
        let mut roots = object_roots(&[pinned_first, pinned_second]);
        let answers = heap.collect(&mut roots, &[pinned_first.0, pinned_second.0]);

        assert_eq!(
            roots[0].0.0, pinned_first.0,
            "a pinned survivor in the first segment stays"
        );
        assert_eq!(
            roots[1].0.0, pinned_second.0,
            "a pinned survivor in the second segment stays"
        );
        for (seg, answer) in answers.iter().enumerate() {
            assert_eq!(
                answer.free, ALIGN,
                "segment {seg}'s chain starts at its own lowest gap"
            );
            let base = heap.bases[seg];
            assert_eq!(
                heap.read(base + answer.free),
                FREE_BLOCK | 8,
                "segment {seg}'s gap is a free block"
            );
            assert_eq!(
                heap.read(base + answer.free + 4),
                0,
                "and ends segment {seg}'s chain"
            );
            assert_eq!(
                answer.top,
                ALIGN + 16,
                "nothing lives above segment {seg}'s pin"
            );
        }
    }

    /// WHAT THE SURVIVORS VACATE READS ZERO in both segments, because it is the next allocation's
    /// payload.
    #[test]
    fn the_reclaimed_tail_of_each_segment_reads_zero() {
        let mut heap = Heap2::new(256, vec![leaf(), one_ref()]);
        let keep = heap.alloc(1, LEAF);
        for seg in 0..2 {
            for _ in 0..6 {
                let dead = heap.alloc(seg, ONE_REF);
                heap.write(dead.0, keep.0);
            }
        }
        let before = heap.tops;
        let mut roots = object_roots(&[keep]);
        let answers = heap.collect(&mut roots, &[]);
        for seg in 0..2 {
            assert!(
                answers[seg].top < before[seg],
                "segment {seg}'s garbage must be reclaimed"
            );
            let tail = &heap.regions[seg][answers[seg].top as usize..before[seg] as usize];
            assert!(
                tail.iter().all(|&b| b == 0),
                "segment {seg}'s reclaimed tail still holds dead words"
            );
        }
    }

    /// THE STORAGE-FREE FINISH REACHES ACROSS SEGMENTS. One object holds more references into the
    /// second segment than the mark stack holds, and each of those holds one back into the first:
    /// every object survives.
    #[test]
    fn a_mark_stack_overflow_finishes_across_segments() {
        let fan = (MARK_STACK as u32) + 16;
        let mut heap = Heap2::new(4096, vec![leaf(), one_ref(), many_refs(fan)]);
        let hub = heap.alloc(0, 2);
        let mut far = Vec::new();
        let mut back = Vec::new();
        for i in 0..fan {
            let f = heap.alloc(1, ONE_REF);
            let b = heap.alloc(0, LEAF);
            heap.write(b.0, 0xB000 + i);
            heap.write(f.0, b.0);
            heap.write(hub.0 + 4 * i, f.0);
            far.push(f);
            back.push(b);
        }
        let before = heap.tops;
        let mut roots = object_roots(&[hub]);
        let answers = heap.collect(&mut roots, &[]);
        assert_eq!(
            [answers[0].top, answers[1].top],
            before,
            "everything is reachable, so nothing is reclaimed"
        );
        for i in 0..fan {
            let f = heap.read(hub.0 + 4 * i);
            let b = heap.read(f);
            assert_eq!(
                heap.read(b),
                0xB000 + i,
                "object {i} across the segments survived intact"
            );
        }
    }

    /// A WEAK SLOT is cleared when its target in the other segment died and forwarded when it lived.
    #[test]
    fn a_weak_slot_into_the_other_segment_clears_or_forwards() {
        let holder = TypeDesc {
            payload_size: 8,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        };
        let mut heap = Heap2::new(256, vec![leaf(), one_ref(), holder]);
        heap.weak.insert(2, vec![0, 4]);
        let _dead = heap.alloc(1, LEAF);
        let dies = heap.alloc(1, LEAF);
        let lives = heap.alloc(1, LEAF);
        let weak = heap.alloc(0, 2);
        heap.write(weak.0, dies.0);
        heap.write(weak.0 + 4, lives.0);
        let mut roots = object_roots(&[weak, lives]);
        heap.collect(&mut roots, &[]);
        assert_eq!(
            heap.read(roots[0].0.0),
            0,
            "the weak slot to the dead object is cleared"
        );
        assert_eq!(
            heap.read(roots[0].0.0 + 4),
            roots[1].0.0,
            "the weak slot to the live one follows it"
        );
    }

    /// THE FORWARDING INDEX AGREES WITH THE REPLAY over many chunks: objects smaller than a chunk,
    /// objects that span several, garbage between every two survivors and a pin in the middle of each
    /// segment, with a holder that references every survivor so every one is looked up. `collect`
    /// runs it with the index and without and requires the same heap; this checks the data as well.
    #[test]
    fn the_forwarding_index_agrees_with_the_replay_across_chunks() {
        let wide = TypeDesc {
            payload_size: 300,
            ref_offsets: Vec::new(),
            tagged_offsets: Vec::new(),
        };
        let holder = many_refs(120);
        let mut heap = Heap2::new(16_384, vec![leaf(), one_ref(), wide, holder]);
        let hub = heap.alloc(0, 3);
        let mut kept = Vec::new();
        for i in 0..120u32 {
            let seg = (i % 2) as usize;
            let _dead = heap.alloc(seg, if i % 3 == 0 { 2 } else { LEAF });
            let ty = if i % 5 == 0 { 2 } else { LEAF };
            let live = heap.alloc(seg, ty);
            heap.write(live.0, 0xA000 + i);
            heap.write(hub.0 + 4 * i, live.0);
            kept.push(live);
        }
        let pins = [kept[60].0, kept[61].0];
        let mut roots = object_roots(&[hub]);
        heap.collect(&mut roots, &pins);
        let hub = roots[0].0.0;
        for i in 0..120u32 {
            let live = heap.read(hub + 4 * i);
            assert_eq!(heap.read(live), 0xA000 + i, "survivor {i} lost its word");
            let seg = (i % 2) as usize;
            assert!(
                live >= heap.bases[seg] && live < heap.bases[seg] + heap.tops[seg],
                "survivor {i} left its segment"
            );
        }
        assert_eq!(heap.read(hub + 4 * 60), pins[0], "a pinned survivor stays");
        assert_eq!(
            heap.read(hub + 4 * 61),
            pins[1],
            "and so does the other segment's"
        );
    }

    /// THE REFUSALS: a bitmap short of a segment, overlapping regions, or counts that disagree
    /// answer `false` and leave every byte as it was.
    #[test]
    fn a_collection_it_cannot_prove_is_refused_untouched() {
        let mut heap = Heap2::new(256, vec![leaf(), one_ref()]);
        let _dead = heap.alloc(0, LEAF);
        let _dead_too = heap.alloc(1, LEAF);
        let snapshot = heap.regions.clone();
        let resolver = TableResolver {
            type_descs: &heap.descs,
            weak_offsets: &heap.weak,
        };
        let run = |bases: [u32; 2], words: [usize; 2], answers: usize| -> bool {
            let mut regions = snapshot.clone();
            let mut a = vec![0u32; words[0]];
            let mut b = vec![0u32; words[1]];
            let mut marks = [MarkBits::new(&mut a), MarkBits::new(&mut b)];
            let [first, second] = &mut regions;
            let mut segments = [
                Segment {
                    bytes: first,
                    base: bases[0],
                    top: 12,
                    index: &mut [],
                },
                Segment {
                    bytes: second,
                    base: bases[1],
                    top: 12,
                    index: &mut [],
                },
            ];
            let mut slots = [Compacted { top: 0, free: 0 }; 2];
            let ran = mark_compact_segments(
                &mut segments,
                &mut marks,
                &resolver,
                |_| {},
                &[],
                &mut slots[..answers],
            );
            if !ran {
                assert_eq!(regions, snapshot, "a refused collection touched the heap");
            }
            ran
        };
        let full = MarkBits::words_for(256);
        assert!(
            run([FIRST_BASE, SECOND_BASE], [full, full], 2),
            "the control collects"
        );
        assert!(
            !run([FIRST_BASE, SECOND_BASE], [full, full - 1], 2),
            "a bitmap short of the second segment"
        );
        assert!(
            !run([FIRST_BASE, FIRST_BASE + 128], [full, full], 2),
            "regions that overlap"
        );
        assert!(
            !run([FIRST_BASE, SECOND_BASE], [full, full], 1),
            "one answer slot for two segments"
        );
    }
}
