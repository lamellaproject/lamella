//! Compressing what the target's own ROM will inflate: DEFLATE (RFC 1951) inside a zlib wrapper
//! (RFC 1950).

use alloc::vec::Vec;

/// How hard the encoder tries.
///
/// Both are conformant streams. The choice is a diagnostic ladder rather than a speed knob: see the
/// module docs for why the rung that compresses nothing is worth keeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    /// Store the input in uncompressed blocks. Conformant, and slightly LARGER than the input.
    Stored,
    /// Compress with the format's fixed code tables, falling back to [`Method::Stored`] for the whole
    /// stream when fixed codes would not be smaller.
    ///
    /// **The fallback is not a nicety.** Fixed codes spend eight bits on the commonest half of the
    /// byte range and nine on the rest, so an input with no structure to find -- an already-compressed
    /// payload, say -- comes out about six percent LARGER. A compressed write that inflates its own
    /// transfer would be a silent pessimization, since nothing in the protocol reports the ratio.
    Fixed,
}

/// The zlib stream of `data`, ready to be sent as the compressed write's payload.
///
/// Reversible by any conformant inflater, including the target's: the returned bytes carry the
/// wrapper's two header bytes, the deflate blocks, and the Adler-32 of `data` that the target
/// checks.
#[must_use]
pub fn zlib(data: &[u8], method: Method) -> Vec<u8> {
    let blocks = match method {
        Method::Stored => stored(data),
        Method::Fixed => {
            let compressed = fixed(data);
            if compressed.len() < data.len() {
                compressed
            } else {
                let plain = stored(data);
                if compressed.len() < plain.len() {
                    compressed
                } else {
                    plain
                }
            }
        }
    };
    let (cmf, flg) = wrapper(method);
    let mut out = Vec::with_capacity(blocks.len() + WRAPPER_LEN);
    out.push(cmf);
    out.push(flg);
    out.extend_from_slice(&blocks);
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

/// The bytes the wrapper adds: two of header and four of checksum.
const WRAPPER_LEN: usize = 6;

/// The wrapper's two header bytes for `method`.
///
/// The first names the compression method in its low nibble and the window size in its high one; the
/// second carries a level hint, a preset-dictionary flag this never sets, and five check bits.
///
/// **The check bits are a constraint, not a value**: the specification requires the two bytes, read
/// together as one big-endian integer, to be a multiple of thirty-one, so they are SOLVED for rather
/// than transcribed. A header failing that is refused as a parameter error.
fn wrapper(method: Method) -> (u8, u8) {
    /// Compression method eight: deflate.
    const DEFLATE: u8 = 8;
    /// The base-two logarithm of the window size, minus eight. Seven is the 32 KiB the search below
    /// uses, and the largest this version of the format allows.
    const WINDOW_LOG: u8 = 7;
    let cmf = (WINDOW_LOG << 4) | DEFLATE;
    let level: u8 = match method {
        Method::Stored => 0,
        Method::Fixed => 1,
    };
    let partial = level << 6;
    let remainder = ((u16::from(cmf) << 8) | u16::from(partial)) % 31;
    let check = ((31 - remainder) % 31) as u8;
    (cmf, partial | check)
}

/// The checksum the wrapper carries: the specification's two running sums over the UNCOMPRESSED
/// bytes, combined as the second times 65,536 plus the first.
///
/// Written as the specification states it, one modulo per byte, rather than with the deferred
/// reduction a compressor in a hurry would use. There is nothing to gain here -- the input is an
/// image being sent down a serial line -- and the deferred form's whole risk is an overflow that
/// changes the checksum without changing anything else.
fn adler32(data: &[u8]) -> u32 {
    /// The largest prime below 65,536, which both sums are taken modulo.
    const BASE: u32 = 65_521;
    let mut low: u32 = 1;
    let mut high: u32 = 0;
    for &byte in data {
        low = (low + u32::from(byte)) % BASE;
        high = (high + low) % BASE;
    }
    (high << 16) | low
}

/// The most bytes one stored block can carry: the block declares its length in sixteen bits.
const MAX_STORED: usize = 0xFFFF;

/// `data` as stored blocks.
fn stored(data: &[u8]) -> Vec<u8> {
    let mut bits = BitWriter::with_capacity(data.len() + 5 * (data.len() / MAX_STORED + 1));
    let mut at = 0;
    loop {
        let len = (data.len() - at).min(MAX_STORED);
        let last = at + len == data.len();
        bits.bits(u32::from(last), 1);
        bits.bits(BTYPE_STORED, 2);
        bits.align();
        let declared = len as u16;
        bits.aligned_bytes(&declared.to_le_bytes());
        bits.aligned_bytes(&(!declared).to_le_bytes());
        bits.aligned_bytes(&data[at..at + len]);
        at += len;
        if last {
            return bits.finish();
        }
    }
}

/// `data` as one fixed-Huffman block.
///
/// One block for the whole input: a compressed block has no size limit, and the fixed tables are the
/// same for every block, so splitting would add framing and change nothing else.
fn fixed(data: &[u8]) -> Vec<u8> {
    let mut bits = BitWriter::with_capacity(data.len());
    bits.bits(1, 1);
    bits.bits(BTYPE_FIXED, 2);
    let mut chains = Chains::new(data.len());
    let mut at = 0;
    while at < data.len() {
        let from = chains.advance_to(data, at);
        match longest_match(data, &chains, at, from) {
            None => {
                literal(&mut bits, data[at]);
                at += 1;
            }
            Some((len, distance)) => {
                let ahead = if at + 1 < data.len() {
                    let from = chains.advance_to(data, at + 1);
                    longest_match(data, &chains, at + 1, from)
                } else {
                    None
                };
                match ahead {
                    Some((ahead_len, ahead_distance)) if ahead_len > len => {
                        literal(&mut bits, data[at]);
                        reference(&mut bits, ahead_len, ahead_distance);
                        at += 1 + ahead_len;
                    }
                    _ => {
                        reference(&mut bits, len, distance);
                        at += len;
                    }
                }
            }
        }
    }
    literal_code(&mut bits, END_OF_BLOCK);
    bits.finish()
}

/// The block types this encoder emits.
const BTYPE_STORED: u32 = 0b00;
const BTYPE_FIXED: u32 = 0b01;

/// The literal/length alphabet's end-of-block symbol.
const END_OF_BLOCK: u16 = 256;

/// The first length code, which the length table is indexed from.
const FIRST_LENGTH_CODE: u16 = 257;

/// Writes one literal byte.
fn literal(bits: &mut BitWriter, byte: u8) {
    literal_code(bits, u16::from(byte));
}

/// Writes one symbol of the literal/length alphabet in the fixed code.
fn literal_code(bits: &mut BitWriter, symbol: u16) {
    let (code, len) = fixed_literal(symbol);
    bits.code(code, len);
}

/// Writes one back-reference: a length code with its extra bits, then a distance code with its own.
fn reference(bits: &mut BitWriter, length: usize, distance: usize) {
    let (index, extra, extra_bits) = pick(&LENGTHS, length as u16);
    literal_code(bits, FIRST_LENGTH_CODE + index as u16);
    bits.bits(extra, extra_bits);
    let (index, extra, extra_bits) = pick(&DISTANCES, distance as u16);
    bits.code(index as u16, 5);
    bits.bits(extra, extra_bits);
}

/// The fixed literal/length code: the code assigned to `symbol` and its length in bits.
///
/// Transcribed from the specification's table, which gives four ranges of code lengths and the first
/// and last code of each -- both endpoints of all four are asserted in this module's tests, since a
/// table like this is wrong in a way that produces a stream an inflater decodes into different bytes
/// rather than into an error.
fn fixed_literal(symbol: u16) -> (u16, u32) {
    match symbol {
        0..=143 => (0b0011_0000 + symbol, 8),
        144..=255 => (0b1_1001_0000 + (symbol - 144), 9),
        256..=279 => (symbol - 256, 7),
        _ => (0b1100_0000 + (symbol - 280), 8),
    }
}

/// The length codes, as the shortest length each covers and how many extra bits follow it.
///
/// The last entry is the reason a lookup scans from the END: the longest expressible length is
/// covered both by the entry before it, whose five extra bits reach one short of it, and by its own
/// entry with no extra bits. The tests require the table to tile the whole length range exactly,
/// which no single mistranscribed entry survives.
const LENGTHS: [(u16, u32); 29] = [
    (3, 0),
    (4, 0),
    (5, 0),
    (6, 0),
    (7, 0),
    (8, 0),
    (9, 0),
    (10, 0),
    (11, 1),
    (13, 1),
    (15, 1),
    (17, 1),
    (19, 2),
    (23, 2),
    (27, 2),
    (31, 2),
    (35, 3),
    (43, 3),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 4),
    (115, 4),
    (131, 5),
    (163, 5),
    (195, 5),
    (227, 5),
    (258, 0),
];

/// The distance codes, in the same shape as [`LENGTHS`]. The last two the specification names as
/// unreachable are absent, since nothing here can emit a distance beyond the window.
const DISTANCES: [(u16, u32); 30] = [
    (1, 0),
    (2, 0),
    (3, 0),
    (4, 0),
    (5, 1),
    (7, 1),
    (9, 2),
    (13, 2),
    (17, 3),
    (25, 3),
    (33, 4),
    (49, 4),
    (65, 5),
    (97, 5),
    (129, 6),
    (193, 6),
    (257, 7),
    (385, 7),
    (513, 8),
    (769, 8),
    (1025, 9),
    (1537, 9),
    (2049, 10),
    (3073, 10),
    (4097, 11),
    (6145, 11),
    (8193, 12),
    (12289, 12),
    (16385, 13),
    (24577, 13),
];

/// Which entry of `table` covers `value`, the extra-bit value that selects `value` within it, and how
/// many extra bits that takes.
///
/// Scans from the end, so the entry with the largest base that still covers `value` wins.
fn pick(table: &[(u16, u32)], value: u16) -> (usize, u32, u32) {
    let mut index = table.len() - 1;
    while table[index].0 > value {
        index -= 1;
    }
    let (base, extra_bits) = table[index];
    (index, u32::from(value - base), extra_bits)
}

/// How far back a reference may reach, which is what the wrapper's header declares.
const WINDOW: usize = 32_768;
/// The shortest run the format can express as a reference.
const MIN_MATCH: usize = 3;
/// The longest run one length code can express.
const MAX_MATCH: usize = 258;
/// How many earlier positions on one chain to compare before settling for the best found.
///
/// A bound rather than an exhaustive search, as the specification's own algorithm prescribes: a chain
/// over a repetitive input grows without limit and the marginal candidate is almost never the best
/// one. It costs ratio, not correctness -- every candidate is compared byte by byte, so a bad hash or
/// an early stop can only fail to FIND a match.
const MAX_CHAIN: usize = 128;
/// How many bits of hash index the chains are bucketed by.
const HASH_BITS: u32 = 15;

/// The absent-position sentinel, which no real position can take: the format's window is far smaller.
const NONE: u32 = u32::MAX;

/// The chained hash table the match search walks: for each three-byte sequence, the most recent
/// position it began at, and from each position the one before it.
struct Chains {
    /// The newest position in each bucket.
    head: Vec<u32>,
    /// For each position, the previous position in its bucket.
    previous: Vec<u32>,
    /// The lowest position not yet recorded.
    ///
    /// **Insertion is monotonic and happens exactly once per position**, which is what keeps the
    /// chains acyclic: recording a position twice would make it its own predecessor and the search
    /// would never terminate.
    next: usize,
}

impl Chains {
    /// Empty chains for an input of `len` bytes.
    fn new(len: usize) -> Chains {
        Chains { head: alloc::vec![NONE; 1 << HASH_BITS], previous: alloc::vec![NONE; len], next: 0 }
    }

    /// Records every position up to and including `at`, and returns the position a match search for
    /// `at` should start from -- the most recent EARLIER position whose three bytes hash the same.
    ///
    /// Positions inside an emitted match are recorded too, rather than skipped: they cost one hash
    /// each and they are exactly the positions a later repetition wants to point at.
    fn advance_to(&mut self, data: &[u8], at: usize) -> u32 {
        while self.next <= at {
            let position = self.next;
            self.next += 1;
            if position + MIN_MATCH <= data.len() {
                let bucket = hash3(&data[position..]);
                self.previous[position] = self.head[bucket];
                self.head[bucket] = position as u32;
            }
        }
        if at + MIN_MATCH <= data.len() {
            self.previous[at]
        } else {
            NONE
        }
    }
}

/// Which bucket the three bytes at the start of `window` belong to.
///
/// Any function of those three bytes would be correct -- a collision costs a comparison, never a
/// wrong match -- so this is chosen for spread rather than derived from anything.
fn hash3(window: &[u8]) -> usize {
    let key = (u32::from(window[0]) << 16) | (u32::from(window[1]) << 8) | u32::from(window[2]);
    (key.wrapping_mul(0x9E37_79B1) >> (32 - HASH_BITS)) as usize
}

/// The longest run at `at` that repeats an earlier one, as its length and how far back it starts,
/// searching the chain from `from`.
fn longest_match(data: &[u8], chains: &Chains, at: usize, from: u32) -> Option<(usize, usize)> {
    let limit = (data.len() - at).min(MAX_MATCH);
    if limit < MIN_MATCH {
        return None;
    }
    let earliest = at.saturating_sub(WINDOW);
    let mut best = 0;
    let mut best_distance = 0;
    let mut candidate = from;
    let mut tries = MAX_CHAIN;
    while candidate != NONE && tries > 0 {
        let position = candidate as usize;
        if position < earliest {
            break;
        }
        let mut len = 0;
        while len < limit && data[position + len] == data[at + len] {
            len += 1;
        }
        if len > best {
            best = len;
            best_distance = at - position;
            if len == limit {
                break;
            }
        }
        candidate = chains.previous[position];
        tries -= 1;
    }
    (best >= MIN_MATCH).then_some((best, best_distance))
}

/// Packs data elements into bytes the way the format requires.
///
/// # The two packing rules, which differ, and the one that surprises
///
/// Elements go into bytes from the least-significant bit up. Within an element the rule DEPENDS ON
/// WHAT THE ELEMENT IS: an ordinary integer -- a length's extra bits, a stored block's declared
/// length -- goes in least-significant bit first, and a Huffman code goes in MOST-significant bit
/// first. So the two writers below are not conveniences over each other; a code written as an integer
/// is a different code, usually a valid one, and the stream decodes into different bytes rather than
/// into an error.
///
/// The specification's own illustration is the way to check this: print the bytes right to left, and
/// integers read most-significant bit first while Huffman codes read reversed.
struct BitWriter {
    /// The bytes completed so far.
    out: Vec<u8>,
    /// Bits written but not yet in a whole byte, with the next bit to emit at the bottom.
    held: u32,
    /// How many bits `held` holds, always fewer than eight between calls.
    count: u32,
}

impl BitWriter {
    /// An empty writer with room for `capacity` bytes.
    fn with_capacity(capacity: usize) -> BitWriter {
        BitWriter { out: Vec::with_capacity(capacity), held: 0, count: 0 }
    }

    /// Writes the low `width` bits of `value`, least-significant first: the rule for everything that
    /// is not a Huffman code.
    fn bits(&mut self, value: u32, width: u32) {
        debug_assert!(width <= 25, "an element wider than the accumulator");
        self.held |= (value & ((1u32 << width) - 1)) << self.count;
        self.count += width;
        while self.count >= 8 {
            self.out.push((self.held & 0xFF) as u8);
            self.held >>= 8;
            self.count -= 8;
        }
    }

    /// Writes a Huffman `code` of `width` bits, most-significant bit first.
    fn code(&mut self, code: u16, width: u32) {
        let mut reversed = 0;
        for bit in 0..width {
            reversed |= ((u32::from(code) >> bit) & 1) << (width - 1 - bit);
        }
        self.bits(reversed, width);
    }

    /// Pads to the next byte boundary with zeros.
    fn align(&mut self) {
        if self.count > 0 {
            self.bits(0, 8 - self.count);
        }
    }

    /// Appends whole bytes, which is only valid on a byte boundary.
    fn aligned_bytes(&mut self, bytes: &[u8]) {
        debug_assert_eq!(self.count, 0, "bytes appended mid-byte would be shifted");
        self.out.extend_from_slice(bytes);
    }

    /// The finished bytes, padded to a whole byte.
    fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wrapper's header must satisfy the specification's three constraints on it, and the check
    /// bits are the one that is SOLVED rather than written down -- so it is asserted for both methods
    /// rather than for the one that happened to be tried.
    #[test]
    fn the_wrapper_header_names_deflate_and_satisfies_its_own_check() {
        for method in [Method::Stored, Method::Fixed] {
            let (cmf, flg) = wrapper(method);
            assert_eq!(cmf & 0x0F, 8, "compression method is deflate");
            assert_eq!(cmf >> 4, 7, "a 32 KiB window, which is what the search uses");
            assert_eq!(flg & 0b0010_0000, 0, "no preset dictionary");
            assert_eq!(
                ((u16::from(cmf) << 8) | u16::from(flg)) % 31,
                0,
                "the header pair is a multiple of thirty-one"
            );
        }
    }

    /// The checksum, against an independently shaped computation of the same definition: the sums
    /// accumulated without reduction and reduced once at the end. **That is a different algorithm
    /// with the same answer**, not a transcription of the one under test, so it can disagree.
    #[test]
    fn the_checksum_matches_an_unreduced_accumulation() {
        fn unreduced(data: &[u8]) -> u32 {
            let mut low: u64 = 1;
            let mut high: u64 = 0;
            for &byte in data {
                low += u64::from(byte);
                high += low;
            }
            (((high % 65_521) as u32) << 16) | ((low % 65_521) as u32)
        }
        for case in [vec![], vec![0], vec![0xFF; 4096], (0..=255u8).cycle().take(9000).collect()] {
            assert_eq!(adler32(&case), unreduced(&case), "over {} bytes", case.len());
        }
    }

    /// The empty input's checksum is fixed by the specification's initial values alone: the first sum
    /// starts at one and the second at zero, so nothing to sum leaves exactly one.
    #[test]
    fn the_checksum_of_nothing_is_the_initial_state() {
        assert_eq!(adler32(&[]), 1);
    }

    /// **Both endpoints of all four ranges of the fixed code**, which the specification prints beside
    /// the code lengths. A mistranscribed range start shifts every code in it, and the stream still
    /// decodes -- into different bytes.
    #[test]
    fn the_fixed_code_matches_the_published_endpoints() {
        assert_eq!(fixed_literal(0), (0b0011_0000, 8));
        assert_eq!(fixed_literal(143), (0b1011_1111, 8));
        assert_eq!(fixed_literal(144), (0b1_1001_0000, 9));
        assert_eq!(fixed_literal(255), (0b1_1111_1111, 9));
        assert_eq!(fixed_literal(256), (0b000_0000, 7));
        assert_eq!(fixed_literal(279), (0b001_0111, 7));
        assert_eq!(fixed_literal(280), (0b1100_0000, 8));
        assert_eq!(fixed_literal(287), (0b1100_0111, 8));
    }

    /// A canonical code assigns every symbol of a given length a distinct code, and shorter codes
    /// never prefix longer ones. Asserted over the whole alphabet, because the four ranges above
    /// could each be right at their endpoints and still collide with one another.
    #[test]
    fn the_fixed_code_is_prefix_free_across_the_whole_alphabet() {
        let codes: Vec<(u16, u32)> = (0..=287).map(fixed_literal).collect();
        for (i, &(code, width)) in codes.iter().enumerate() {
            for (j, &(other, other_width)) in codes.iter().enumerate() {
                if i == j {
                    continue;
                }
                if width <= other_width {
                    assert_ne!(
                        code,
                        other >> (other_width - width),
                        "symbol {i} prefixes symbol {j}"
                    );
                }
            }
        }
    }

    /// **The length table tiles the whole expressible length range exactly.** Every length from the
    /// shortest to the longest must resolve to one entry and be reconstructible from that entry's
    /// base plus its extra bits -- which no single wrong base or extra-bit count survives, and which
    /// does not require transcribing the table a second time to check it against.
    #[test]
    fn every_expressible_length_round_trips_through_its_code() {
        for length in MIN_MATCH..=MAX_MATCH {
            let value = length as u16;
            let (index, extra, extra_bits) = pick(&LENGTHS, value);
            assert!(index < LENGTHS.len());
            assert!(
                extra < (1 << extra_bits),
                "length {length} needs {extra} in {extra_bits} extra bits"
            );
            assert_eq!(LENGTHS[index].0 + extra as u16, value, "length {length} reconstructs");
        }
    }

    /// **The tables must be CONTIGUOUS, not merely coverable.** Each entry's range has to begin
    /// exactly where the previous entry's extra bits stop reaching -- a base transcribed one too low
    /// still reconstructs every value, because the overlapping entry wins the scan, so the test above
    /// passes and the encoder emits a length one short of what the match actually was.
    ///
    /// The last length entry is the specification's own documented exception: the longest length gets
    /// its own code with no extra bits, overlapping the entry before it, which is exactly why lookups
    /// scan from the end.
    #[test]
    fn the_tables_tile_their_ranges_without_gaps_or_overlaps() {
        for window in LENGTHS[..LENGTHS.len() - 1].windows(2) {
            let (base, extra_bits) = window[0];
            assert_eq!(window[1].0, base + (1 << extra_bits), "length entry after base {base}");
        }
        let last = LENGTHS[LENGTHS.len() - 1];
        assert_eq!(last, (MAX_MATCH as u16, 0), "the longest length has its own code");

        for window in DISTANCES.windows(2) {
            let (base, extra_bits) = window[0];
            assert_eq!(window[1].0, base + (1 << extra_bits), "distance entry after base {base}");
        }
        let (base, extra_bits) = DISTANCES[DISTANCES.len() - 1];
        assert_eq!(
            usize::from(base) + (1 << extra_bits) - 1,
            WINDOW,
            "the distance codes stop exactly at the window the header declares"
        );
    }

    /// The same for distances, over the whole window the header declares.
    #[test]
    fn every_expressible_distance_round_trips_through_its_code() {
        for distance in 1..=WINDOW {
            let value = distance as u16;
            let (index, extra, extra_bits) = pick(&DISTANCES, value);
            assert!(index < DISTANCES.len());
            assert!(extra < (1 << extra_bits), "distance {distance} needs {extra} bits");
            assert_eq!(DISTANCES[index].0 + extra as u16, value, "distance {distance} reconstructs");
        }
    }

    /// The longest length is the one place the tables overlap, and the specification resolves it by
    /// giving it its own code with no extra bits. Taking the other entry would emit a length one
    /// short of what was asked for -- a stream that inflates to almost the right thing.
    #[test]
    fn the_longest_length_takes_its_own_code_rather_than_the_previous_one() {
        let (index, extra, extra_bits) = pick(&LENGTHS, MAX_MATCH as u16);
        assert_eq!(FIRST_LENGTH_CODE + index as u16, 285);
        assert_eq!((extra, extra_bits), (0, 0));
        let (index, extra, _) = pick(&LENGTHS, (MAX_MATCH - 1) as u16);
        assert_eq!(FIRST_LENGTH_CODE + index as u16, 284, "one short still uses the extra bits");
        assert_eq!(extra, 30);
    }

    /// The two packing rules produce different bytes for the same value, which is the whole reason
    /// they are separate methods. Written out by hand: three bits of value five, as an integer and as
    /// a code.
    #[test]
    fn an_integer_and_a_huffman_code_of_the_same_value_pack_differently() {
        let mut integer = BitWriter::with_capacity(1);
        integer.bits(0b101, 3);
        let mut huffman = BitWriter::with_capacity(1);
        huffman.code(0b100, 3);
        assert_eq!(integer.finish(), vec![0b0000_0101]);
        assert_eq!(huffman.finish(), vec![0b0000_0001]);
    }

    /// A stored block's header, length pair and alignment, read back by hand from the front of the
    /// stream rather than through the inflater below -- so the byte layout is pinned independently of
    /// anything that decodes it.
    #[test]
    fn a_stored_block_declares_its_length_and_its_complement_after_the_header_byte() {
        let stream = zlib(&[0xAA; 10], Method::Stored);
        assert_eq!(stream[0] & 0x0F, 8, "the wrapper's method nibble");
        assert_eq!(stream[2], 0b0000_0001);
        assert_eq!(u16::from_le_bytes([stream[3], stream[4]]), 10);
        assert_eq!(u16::from_le_bytes([stream[5], stream[6]]), !10u16);
        assert_eq!(&stream[7..17], &[0xAA; 10]);
        assert_eq!(stream.len(), 2 + 5 + 10 + 4, "wrapper, block header, body, checksum");
    }

    /// **The block-length cap is real and it is off by one from a round number.** An input one byte
    /// past it must become two blocks, with only the second marked final.
    #[test]
    fn an_input_past_the_block_cap_becomes_two_blocks_and_only_the_last_is_final() {
        let stream = zlib(&vec![0x5A; MAX_STORED + 1], Method::Stored);
        assert_eq!(stream[2] & 1, 0, "the first block is not the last");
        assert_eq!(u16::from_le_bytes([stream[3], stream[4]]), MAX_STORED as u16);
        let second = 2 + 5 + MAX_STORED;
        assert_eq!(stream[second] & 1, 1, "the second block is the last");
        assert_eq!(u16::from_le_bytes([stream[second + 1], stream[second + 2]]), 1);
        assert_eq!(inflate(&stream), vec![0x5A; MAX_STORED + 1]);
    }

    /// Every method round-trips every shape of input, through the inflater below.
    #[test]
    fn both_methods_round_trip() {
        for case in cases() {
            for method in [Method::Stored, Method::Fixed] {
                let stream = zlib(&case, method);
                assert_eq!(
                    inflate(&stream),
                    case,
                    "{method:?} over {} bytes did not round-trip",
                    case.len()
                );
            }
        }
    }

    /// The checksum in the trailer is over the ORIGINAL bytes, not the compressed ones -- which is
    /// the mistake that makes a stream every inflater rejects at the very last step, after decoding
    /// all of it correctly.
    #[test]
    fn the_trailer_checksums_the_input_rather_than_the_stream() {
        for case in cases() {
            let stream = zlib(&case, Method::Fixed);
            let trailer = &stream[stream.len() - 4..];
            assert_eq!(
                u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]),
                adler32(&case),
                "over {} bytes",
                case.len()
            );
        }
    }

    /// **Compression has to actually compress.** Round-tripping is satisfied by an encoder that emits
    /// nothing but literals, and one whose match search never fires would pass every test above --
    /// so the ratio is asserted, on input whose structure is not in question.
    #[test]
    fn the_match_search_finds_repetition_rather_than_emitting_literals() {
        let repetitive: Vec<u8> = b"the same phrase.".iter().copied().cycle().take(16_000).collect();
        let compressed = zlib(&repetitive, Method::Fixed);
        assert!(
            compressed.len() < repetitive.len() / 50,
            "16,000 repetitive bytes compressed to {}",
            compressed.len()
        );
        assert_eq!(inflate(&compressed), repetitive);

        let mut far: Vec<u8> = (0..20_000u32).map(|n| (n.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
        far.extend_from_within(..);
        let compressed = zlib(&far, Method::Fixed);
        assert!(
            compressed.len() < far.len() * 6 / 10,
            "a doubled block compressed to {} of {}",
            compressed.len(),
            far.len()
        );
        assert_eq!(inflate(&compressed), far);
    }

    /// Input with no structure to find must not come out LARGER, which is what fixed codes alone
    /// would do to it. The fallback is what prevents a compressed write from costing transfer time.
    #[test]
    fn incompressible_input_falls_back_rather_than_expanding() {
        let noise: Vec<u8> =
            (0..32_768u32).map(|n| (n.wrapping_mul(2_654_435_761) >> 16) as u8).collect();
        let compressed = zlib(&noise, Method::Fixed);
        let stored_only = zlib(&noise, Method::Stored);
        assert!(
            compressed.len() <= stored_only.len(),
            "{} bytes of noise became {} compressed against {} stored",
            noise.len(),
            compressed.len(),
            stored_only.len()
        );
        assert!(compressed.len() <= noise.len() + 16);
        assert_eq!(inflate(&compressed), noise);
    }

    /// The inputs every round-trip test runs over: the boundaries where an off-by-one lives.
    fn cases() -> Vec<Vec<u8>> {
        alloc::vec![
            Vec::new(),
            alloc::vec![0x42],
            alloc::vec![0x42; 2],
            alloc::vec![0x42; MIN_MATCH],
            alloc::vec![0x42; MIN_MATCH - 1],
            alloc::vec![0x7F; MAX_MATCH + 5],
            alloc::vec![0x00; MAX_STORED],
            alloc::vec![0x01; MAX_STORED + 1],
            (0..=255u8).collect(),
            (0..=255u8).cycle().take(5000).collect(),
            b"a phrase that repeats: a phrase that repeats: a phrase".to_vec(),
        ]
    }

    /// Two streams produced by an INDEPENDENT implementation of this format, one of each block type
    /// this encoder emits, which this module's inflater must decode.
    ///
    /// **This is what stops the round-trip tests above from being two copies of one misconception.**
    /// An encoder and a decoder written together can agree on a wrong bit order and round-trip each
    /// other perfectly, so the decoder is anchored to bytes that were not written here.
    ///
    /// What that anchors, transitively, is everything this encoder emits: the wrapper's two bytes, the
    /// trailer and its byte order, the packing rules, the fixed code's lengths, the length and
    /// distance tables, and a stored block's length pair.
    #[test]
    fn the_inflater_decodes_streams_this_project_did_not_produce() {
        const FOREIGN_FIXED: [u8; 17] = [
            0x78, 0xDA, 0xCB, 0x48, 0xCD, 0xC9, 0xC9, 0x57, 0xC8, 0xC0, 0x4E, 0x02, 0x00, 0xA3,
            0x10, 0x0A, 0xE5,
        ];
        const FOREIGN_STORED: [u8; 33] = [
            0x78, 0x01, 0x01, 0x16, 0x00, 0xE9, 0xFF, 0x73, 0x74, 0x6F, 0x72, 0x65, 0x64, 0x2C,
            0x20, 0x6E, 0x6F, 0x74, 0x20, 0x63, 0x6F, 0x6D, 0x70, 0x72, 0x65, 0x73, 0x73, 0x65,
            0x64, 0x60, 0xA8, 0x08, 0x84,
        ];
        assert_eq!((FOREIGN_FIXED[2] >> 1) & 0b11, BTYPE_FIXED as u8, "a fixed-Huffman block");
        assert_eq!((FOREIGN_STORED[2] >> 1) & 0b11, BTYPE_STORED as u8, "a stored block");
        assert_eq!(inflate(&FOREIGN_FIXED), b"hello hello hello hello hello");
        assert_eq!(inflate(&FOREIGN_STORED), b"stored, not compressed");

        assert_eq!(wrapper(Method::Stored), (FOREIGN_STORED[0], FOREIGN_STORED[1]));
    }

    /// A zlib inflater, for the tests only.
    ///
    /// It reads the two block types this encoder emits and rejects the third, which is all the foreign
    /// streams above need. It checks the wrapper and the trailer, because a decoder that ignored them
    /// could not catch the two mistakes most worth catching -- a header that no inflater will accept,
    /// and a checksum taken over the compressed bytes instead of the original ones.
    fn inflate(stream: &[u8]) -> Vec<u8> {
        assert!(stream.len() >= WRAPPER_LEN, "shorter than the wrapper");
        assert_eq!(stream[0] & 0x0F, 8, "not deflate");
        assert_eq!(((u16::from(stream[0]) << 8) | u16::from(stream[1])) % 31, 0, "bad header check");
        assert_eq!(stream[1] & 0b0010_0000, 0, "a preset dictionary is not supported here");
        let body = &stream[2..stream.len() - 4];
        let out = inflate_blocks(body);
        let trailer = &stream[stream.len() - 4..];
        assert_eq!(
            u32::from_be_bytes([trailer[0], trailer[1], trailer[2], trailer[3]]),
            adler32(&out),
            "the trailer disagrees with what was decoded"
        );
        out
    }

    /// A bit reader mirroring [`BitWriter`]'s two rules: integers least-significant bit first,
    /// Huffman codes most-significant bit first.
    struct BitReader<'a> {
        bytes: &'a [u8],
        /// The next bit's position, counted in bits from the start.
        at: usize,
    }

    impl BitReader<'_> {
        /// Reads `width` bits as an integer.
        fn bits(&mut self, width: u32) -> u32 {
            let mut value = 0;
            for bit in 0..width {
                value |= self.bit() << bit;
            }
            value
        }

        /// Reads one bit.
        fn bit(&mut self) -> u32 {
            let byte = self.bytes[self.at / 8];
            let bit = u32::from(byte >> (self.at % 8)) & 1;
            self.at += 1;
            bit
        }

        /// Discards the rest of the current byte.
        fn align(&mut self) {
            self.at = (self.at + 7) / 8 * 8;
        }

        /// Reads one symbol of a canonical code given every symbol's code length, most-significant
        /// bit first -- the construction the format specifies for its code lengths.
        fn symbol(&mut self, lengths: &[u32]) -> u16 {
            let mut code = 0u32;
            let mut width = 0u32;
            loop {
                code = (code << 1) | self.bit();
                width += 1;
                assert!(width <= 15, "no symbol matched in fifteen bits");
                let mut next = 0u32;
                for length in 1..=width {
                    let mut assigned = 0u32;
                    for (symbol, &symbol_length) in lengths.iter().enumerate() {
                        if symbol_length == length {
                            if length == width && next + assigned == code {
                                return symbol as u16;
                            }
                            assigned += 1;
                        }
                    }
                    next = (next + assigned) << 1;
                }
            }
        }
    }

    /// The fixed literal/length code's lengths, as the specification's four ranges.
    fn fixed_literal_lengths() -> Vec<u32> {
        (0..=287u16)
            .map(|symbol| match symbol {
                0..=143 => 8,
                144..=255 => 9,
                256..=279 => 7,
                _ => 8,
            })
            .collect()
    }

    /// Decodes the deflate blocks between the wrapper's header and its trailer.
    fn inflate_blocks(body: &[u8]) -> Vec<u8> {
        let mut reader = BitReader { bytes: body, at: 0 };
        let mut out: Vec<u8> = Vec::new();
        loop {
            let last = reader.bits(1) == 1;
            let kind = reader.bits(2);
            match kind {
                0 => {
                    reader.align();
                    let len = reader.bits(16) as usize;
                    let nlen = reader.bits(16) as u16;
                    assert_eq!(nlen, !(len as u16), "the length pair disagrees");
                    for _ in 0..len {
                        out.push(reader.bits(8) as u8);
                    }
                }
                1 => decode_symbols(&mut reader, &fixed_literal_lengths(), &[5; 32], &mut out),
                other => panic!("block type {other} is not one this encoder emits"),
            }
            if last {
                return out;
            }
        }
    }

    /// Decodes literals and back-references until the end-of-block symbol.
    fn decode_symbols(
        reader: &mut BitReader<'_>,
        literals: &[u32],
        distances: &[u32],
        out: &mut Vec<u8>,
    ) {
        loop {
            let symbol = reader.symbol(literals);
            if symbol < 256 {
                out.push(symbol as u8);
            } else if symbol == END_OF_BLOCK {
                return;
            } else {
                let entry = LENGTHS[usize::from(symbol - FIRST_LENGTH_CODE)];
                let length = usize::from(entry.0) + reader.bits(entry.1) as usize;
                let code = reader.symbol(distances);
                let entry = DISTANCES[usize::from(code)];
                let distance = usize::from(entry.0) + reader.bits(entry.1) as usize;
                assert!(distance <= out.len(), "a reference past the start of the output");
                let from = out.len() - distance;
                for offset in 0..length {
                    let byte = out[from + offset];
                    out.push(byte);
                }
            }
        }
    }

}
