//! What the Python firmware's deploy window holds, read from its bytes alone.
//!
//! A stored bundle's header is at the start of the window: the bundle's length and the CRC-32 of its
//! bytes, both `u32` little-endian. The bundle's bytes begin at `bundle_at`, on a page of their own.
//! A deploy writes the header after the bundle's last byte, so a deploy that stopped part way leaves
//! the header erased and the window holding nothing.

/// The header's length: the bundle's length and its CRC-32.
pub(crate) const HEADER_LEN: usize = 8;

/// What the deploy window holds.
pub(crate) enum Stored<'a> {
    /// Nothing: the header reads erased.
    Nothing,
    /// A bundle this firmware runs, its bytes read back to the CRC its header gives.
    Bundle(&'a [u8], u32),
    /// A bundle whose bytes read back whole, of another format than this firmware's: its bytes, so
    /// the board can say why it does not run them.
    OtherFormat(&'a [u8]),
    /// A C# program, which the C# firmware stored.
    OtherRuntime,
    /// A header whose bundle does not read back to it.
    Damaged,
}

/// What `window` holds, its bundle read from `bundle_at`, its bytes checked with `crc` and its format
/// with `is_bundle`.
pub(crate) fn stored<'a>(
    window: &'a [u8],
    bundle_at: usize,
    crc: impl Fn(&[u8]) -> u32,
    is_bundle: impl Fn(&[u8]) -> bool,
) -> Stored<'a> {
    let word = |at: usize| window.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let (Some(len), Some(check)) = (word(0), word(4)) else {
        return Stored::Damaged;
    };
    if len == u32::MAX && check == u32::MAX {
        return Stored::Nothing;
    }
    let len = len as usize;
    if len != 0 && len <= window.len().saturating_sub(bundle_at) {
        let bytes = &window[bundle_at..bundle_at + len];
        if crc(bytes) == check {
            return if is_bundle(bytes) { Stored::Bundle(bytes, check) } else { Stored::OtherFormat(bytes) };
        }
    }
    if matches!(window.get(..2), Some(b"LM" | b"MZ")) {
        return Stored::OtherRuntime;
    }
    Stored::Damaged
}
