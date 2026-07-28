//! FAT short (8.3) directory entries.

use alloc::format;
use alloc::string::String;

use crate::{u16le, u32le};

/// One directory slot is 32 bytes.
pub(crate) const DIR_ENTRY_SIZE: usize = 32;

/// Read-only (`ATTR_READ_ONLY`).
pub(crate) const ATTR_READ_ONLY: u8 = 0x01;
/// Hidden (`ATTR_HIDDEN`).
pub(crate) const ATTR_HIDDEN: u8 = 0x02;
/// System (`ATTR_SYSTEM`).
pub(crate) const ATTR_SYSTEM: u8 = 0x04;
/// Volume label (`ATTR_VOLUME_ID`): a name-bearing slot that is not a file or directory.
pub(crate) const ATTR_VOLUME_ID: u8 = 0x08;
/// Subdirectory (`ATTR_DIRECTORY`).
pub(crate) const ATTR_DIRECTORY: u8 = 0x10;
/// Archive (`ATTR_ARCHIVE`).
#[allow(dead_code)]
pub(crate) const ATTR_ARCHIVE: u8 = 0x20;
/// A long-name slot carries `READ_ONLY | HIDDEN | SYSTEM | VOLUME_ID` (0x0F) in its attribute
/// byte -- a bit pattern no real short entry uses, which is what lets it share the directory.
pub(crate) const ATTR_LONG_NAME: u8 = ATTR_READ_ONLY | ATTR_HIDDEN | ATTR_SYSTEM | ATTR_VOLUME_ID;
/// The low six attribute bits, the ones an LFN slot is detected on (the top two are reserved).
pub(crate) const ATTR_LONG_NAME_MASK: u8 = 0x3F;

/// `DIR_Name[0] == 0x00`: this slot is free AND no allocated slot follows it -- enumeration stops.
const NAME_END: u8 = 0x00;
/// `DIR_Name[0] == 0xE5`: this slot is free (a deleted entry), but slots after it may be live.
const NAME_DELETED: u8 = 0xE5;
/// `DIR_Name[0] == 0x05`: the real first character is 0xE5 (escaped so it cannot look deleted).
const NAME_E5_ESCAPE: u8 = 0x05;

/// `DIR_NTRes` bit: the 8-character base was stored lowercase (a VFAT display-case flag, not a
/// long name -- purely cosmetic, carrying no long-name machinery).
const NTRES_LOWER_BASE: u8 = 0x08;
/// `DIR_NTRes` bit: the 3-character extension was stored lowercase.
const NTRES_LOWER_EXT: u8 = 0x10;

/// A borrowed view over one 32-byte directory slot. Zero-copy: it interprets the bytes in place.
pub(crate) struct RawEntry<'slot> {
    bytes: &'slot [u8],
}

impl<'slot> RawEntry<'slot> {
    /// Views the first [`DIR_ENTRY_SIZE`] bytes of `bytes` as a slot. The caller guarantees the
    /// slice is at least that long (the enumerator slices sectors into 32-byte windows).
    pub(crate) fn new(bytes: &'slot [u8]) -> RawEntry<'slot> {
        debug_assert!(bytes.len() >= DIR_ENTRY_SIZE);
        RawEntry { bytes }
    }

    /// The end-of-directory marker: this slot and every slot after it are free.
    pub(crate) fn is_end(&self) -> bool {
        self.bytes[0] == NAME_END
    }

    /// Whether the slot holds no live entry (end marker or a deleted entry).
    pub(crate) fn is_free(&self) -> bool {
        self.bytes[0] == NAME_END || self.bytes[0] == NAME_DELETED
    }

    /// The raw attribute byte (`DIR_Attr` @11).
    pub(crate) fn attr(&self) -> u8 {
        self.bytes[11]
    }

    /// Whether this is a long-name (VFAT) slot rather than a real 8.3 entry.
    pub(crate) fn is_long_name(&self) -> bool {
        self.attr() & ATTR_LONG_NAME_MASK == ATTR_LONG_NAME
    }

    /// Whether this is the volume-label slot (a name that is neither a file nor a directory).
    pub(crate) fn is_volume_id(&self) -> bool {
        self.attr() & ATTR_VOLUME_ID != 0
    }

    /// Whether this entry names a subdirectory.
    pub(crate) fn is_dir(&self) -> bool {
        self.attr() & ATTR_DIRECTORY != 0
    }

    /// The first cluster of the entry's data, combining `DIR_FstClusHI` @20 and
    /// `DIR_FstClusLO` @26. The high half is always zero on FAT12/16.
    pub(crate) fn first_cluster(&self) -> u32 {
        (u32::from(u16le(self.bytes, 20)) << 16) | u32::from(u16le(self.bytes, 26))
    }

    /// The file length in bytes (`DIR_FileSize` @28). Zero for directories.
    pub(crate) fn file_size(&self) -> u32 {
        u32le(self.bytes, 28)
    }

    /// Whether this slot is a live, nameable 8.3 entry (a file or directory): not free, not a
    /// long-name fragment, not the volume label. The directory walker inlines this classification
    /// (it must also fold long-name runs), so this stays as a tested predicate.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_short_entry(&self) -> bool {
        !self.is_free() && !self.is_long_name() && !self.is_volume_id()
    }

    /// Decodes the 8.3 name to its display string (`NAME.EXT`, or `NAME` with no extension),
    /// honoring the VFAT lowercase display flags. Only meaningful when [`is_short_entry`] holds.
    ///
    /// OEM bytes are rendered one-to-one as Latin-1 (exact for the ASCII names that dominate 8.3);
    /// a code-page mapping is a refinement the read path does not need.
    ///
    /// [`is_short_entry`]: RawEntry::is_short_entry
    pub(crate) fn short_name(&self) -> String {
        let ntres = self.bytes[12];
        let mut base_bytes = [0u8; 8];
        base_bytes.copy_from_slice(&self.bytes[0..8]);
        if base_bytes[0] == NAME_E5_ESCAPE {
            base_bytes[0] = 0xE5;
        }
        let base = decode_field(&base_bytes, ntres & NTRES_LOWER_BASE != 0);
        let ext = decode_field(&self.bytes[8..11], ntres & NTRES_LOWER_EXT != 0);
        if ext.is_empty() {
            base
        } else {
            format!("{base}.{ext}")
        }
    }

    /// Whether this is a `.` (self) or `..` (parent) link. A directory listing excludes them, and
    /// they never match a normalized path segment, so the enumerator drops them.
    pub(crate) fn is_dot_entry(&self) -> bool {
        self.bytes[0] == b'.'
    }

    /// The raw `LDIR_Ord` byte of a long entry: the ordinal plus the [`LAST_LONG_ENTRY`] bit.
    pub(crate) fn lfn_order(&self) -> u8 {
        self.bytes[0]
    }

    /// The short-name checksum a long entry carries (`LDIR_Chksum`), tying it to its 8.3 entry.
    pub(crate) fn lfn_checksum_byte(&self) -> u8 {
        self.bytes[13]
    }

    /// The 13 name code units a long entry carries.
    pub(crate) fn lfn_units(&self) -> [u16; LFN_CHARS_PER_ENTRY] {
        lfn_chars(self.bytes)
    }

    /// The raw 11-byte 8.3 name field, exactly as stored -- the bytes the LFN checksum is computed
    /// over.
    pub(crate) fn short_name_field(&self) -> [u8; 11] {
        let mut field = [0u8; 11];
        field.copy_from_slice(&self.bytes[0..11]);
        field
    }
}

/// If `name` fits an 8.3 short entry -- possibly needing the VFAT lowercase display flags for a
/// wholly-lowercase base or extension -- returns its 11-byte field and `DIR_NTRes` byte. Returns
/// `None` when a long-name entry is required: a mixed-case component, an over-long name, or an
/// illegal character. This keeps the common lowercase filename to a single 8.3 slot rather than a
/// long-name run.
pub(crate) fn short_only_encoding(name: &str) -> Option<([u8; 11], u8)> {
    let field = encode_short_name(name)?;
    let (base, ext) = match name.rfind('.') {
        Some(dot) => (&name[..dot], &name[dot + 1..]),
        None => (name, ""),
    };
    let ntres = case_flag(base, NTRES_LOWER_BASE)? | case_flag(ext, NTRES_LOWER_EXT)?;
    let base_render = decode_field(&field[0..8], ntres & NTRES_LOWER_BASE != 0);
    let ext_render = decode_field(&field[8..11], ntres & NTRES_LOWER_EXT != 0);
    let rendered = if ext_render.is_empty() {
        base_render
    } else {
        format!("{base_render}.{ext_render}")
    };
    (rendered == name).then_some((field, ntres))
}

/// The `DIR_NTRes` flag for one 8.3 component: `0` if it is uppercase or caseless, `flag` if wholly
/// lowercase, `None` if mixed-case (which only a long name can preserve).
fn case_flag(component: &str, flag: u8) -> Option<u8> {
    let has_lower = component.chars().any(|c| c.is_ascii_lowercase());
    let has_upper = component.chars().any(|c| c.is_ascii_uppercase());
    match (has_lower, has_upper) {
        (true, true) => None,
        (true, false) => Some(flag),
        _ => Some(0),
    }
}

/// Filters `source` to legal uppercase 8.3 characters (dropping the rest), keeping at most `max` --
/// the raw material for an 8.3 alias's base or extension.
pub(crate) fn clean_short_component(source: &str, max: usize) -> String {
    source
        .chars()
        .filter_map(encode_short_char)
        .take(max)
        .map(char::from)
        .collect()
}

/// Renders an 8.3 name field as its canonical `NAME.EXT` display (uppercase, trailing spaces
/// trimmed), ignoring the VFAT lowercase flags. Used to decide whether a requested name already IS
/// its own 8.3 form (so no long-name entry is needed).
pub(crate) fn short_field_display(field: &[u8; 11]) -> String {
    let base = decode_field(&field[0..8], false);
    let ext = decode_field(&field[8..11], false);
    if ext.is_empty() {
        base
    } else {
        format!("{base}.{ext}")
    }
}

/// Trims the trailing space padding from an 8.3 field and renders it, lowercasing if the VFAT
/// display flag says so.
fn decode_field(field: &[u8], lower: bool) -> String {
    let end = field
        .iter()
        .rposition(|&byte| byte != b' ')
        .map_or(0, |last| last + 1);
    let mut out = String::new();
    for &byte in &field[..end] {
        let ch = char::from(byte);
        out.push(if lower { ch.to_ascii_lowercase() } else { ch });
    }
    out
}

/// Encodes `name` as the 11-byte 8.3 on-disk name field: uppercased, space-padded, base and
/// extension split on the LAST dot. Returns `None` if the name is not representable as an 8.3 short
/// name -- empty, `.`/`..`, an over-long base (>8) or extension (>3), or an invalid character. A
/// long name is NEVER silently truncated; rejecting it is the honest answer until the VFAT
/// long-name increment lands.
pub(crate) fn encode_short_name(name: &str) -> Option<[u8; 11]> {
    if name.is_empty() || name == "." || name == ".." {
        return None;
    }
    let (base, ext) = match name.rfind('.') {
        Some(dot) => (&name[..dot], &name[dot + 1..]),
        None => (name, ""),
    };
    if base.is_empty() || base.len() > 8 || ext.len() > 3 {
        return None;
    }
    let mut field = [b' '; 11];
    for (i, ch) in base.chars().enumerate() {
        field[i] = encode_short_char(ch)?;
    }
    for (i, ch) in ext.chars().enumerate() {
        field[8 + i] = encode_short_char(ch)?;
    }
    if field[0] == 0xE5 {
        field[0] = 0x05;
    }
    Some(field)
}

/// Maps one character to its uppercased 8.3 on-disk byte, or `None` if it is not allowed in a short
/// name. Non-ASCII (OEM code-page) characters are refused -- code-page mapping is not needed for the
/// write path.
fn encode_short_char(ch: char) -> Option<u8> {
    if !ch.is_ascii() {
        return None;
    }
    let byte = (ch as u8).to_ascii_uppercase();
    const FORBIDDEN: &[u8] = b"\"*+,./:;<=>?[\\]| ";
    if byte < 0x20 || FORBIDDEN.contains(&byte) {
        return None;
    }
    Some(byte)
}


/// The bit set on the last (physically first) long entry of a set.
pub(crate) const LAST_LONG_ENTRY: u8 = 0x40;
/// The ordinal (sequence number) occupies the low five bits of `LDIR_Ord`.
pub(crate) const LFN_ORDINAL_MASK: u8 = 0x1F;
/// Each long entry carries 13 UTF-16 code units (5 + 6 + 2 across its three name fields).
pub(crate) const LFN_CHARS_PER_ENTRY: usize = 13;
/// The longest name VFAT represents (255 UTF-16 code units).
pub(crate) const MAX_LFN_CHARS: usize = 255;

/// The 8.3 checksum an LDIR carries to bind it to its short entry: an unsigned-byte rotate-right
/// then add, over the 11-byte short name. Verbatim from the spec's `ChkSum`.
pub(crate) fn lfn_checksum(short: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for &byte in short {
        sum = (if sum & 1 != 0 { 0x80u8 } else { 0 })
            .wrapping_add(sum >> 1)
            .wrapping_add(byte);
    }
    sum
}

/// The 13 UTF-16 code units an LDIR slot carries (from LDIR_Name1/Name2/Name3).
pub(crate) fn lfn_chars(slot: &[u8]) -> [u16; LFN_CHARS_PER_ENTRY] {
    let mut chars = [0u16; LFN_CHARS_PER_ENTRY];
    for (i, ch) in chars.iter_mut().enumerate() {
        let byte = match i {
            0..=4 => 1 + i * 2,
            5..=10 => 14 + (i - 5) * 2,
            _ => 28 + (i - 11) * 2,
        };
        *ch = crate::u16le(slot, byte);
    }
    chars
}

/// Writes one long directory entry: its order byte, the 13 name units, and the shared checksum.
/// `LDIR_Attr` is ATTR_LONG_NAME, `LDIR_Type` and `LDIR_FstClusLO` are zero.
pub(crate) fn put_lfn_entry(
    slot: &mut [u8],
    order: u8,
    chars: &[u16; LFN_CHARS_PER_ENTRY],
    checksum: u8,
) {
    slot[..DIR_ENTRY_SIZE].fill(0);
    slot[0] = order;
    slot[11] = ATTR_LONG_NAME;
    slot[13] = checksum;
    for (i, &ch) in chars.iter().enumerate() {
        let byte = match i {
            0..=4 => 1 + i * 2,
            5..=10 => 14 + (i - 5) * 2,
            _ => 28 + (i - 11) * 2,
        };
        slot[byte..byte + 2].copy_from_slice(&ch.to_le_bytes());
    }
}

/// Reassembles a long name from the concatenated (ordinal-ordered) code units: the name ends at
/// the first `0x0000`, with `0xFFFF` padding beyond it.
pub(crate) fn lfn_name_from_units(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0x0000).unwrap_or(units.len());
    let mut trimmed = &units[..end];
    while let [rest @ .., 0xFFFF] = trimmed {
        trimmed = rest;
    }
    String::from_utf16_lossy(trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assembles a 32-byte slot from an 11-byte 8.3 name field and the load-bearing numeric
    /// fields, at their spec offsets. Authored from the documented layout, independent of the
    /// reader.
    fn entry(name: &[u8; 11], attr: u8, ntres: u8, cluster: u32, size: u32) -> [u8; 32] {
        let mut e = [0u8; 32];
        e[0..11].copy_from_slice(name);
        e[11] = attr;
        e[12] = ntres;
        e[20..22].copy_from_slice(&((cluster >> 16) as u16).to_le_bytes());
        e[26..28].copy_from_slice(&(cluster as u16).to_le_bytes());
        e[28..32].copy_from_slice(&size.to_le_bytes());
        e
    }

    #[test]
    fn decodes_a_file_entry() {
        let e = entry(b"README  TXT", ATTR_ARCHIVE, 0, 0x0001_0002, 4096);
        let r = RawEntry::new(&e);
        assert!(r.is_short_entry());
        assert!(!r.is_dir());
        assert_eq!(r.short_name(), "README.TXT");
        assert_eq!(r.first_cluster(), 0x0001_0002);
        assert_eq!(r.file_size(), 4096);
        assert!("readme.txt".eq_ignore_ascii_case(&r.short_name()));
        assert!("ReadMe.Txt".eq_ignore_ascii_case(&r.short_name()));
        assert!(!"readme.tx".eq_ignore_ascii_case(&r.short_name()));
    }

    #[test]
    fn decodes_a_directory_entry_with_no_extension() {
        let e = entry(b"SUBDIR     ", ATTR_DIRECTORY, 0, 5, 0);
        let r = RawEntry::new(&e);
        assert!(r.is_short_entry());
        assert!(r.is_dir());
        assert_eq!(r.short_name(), "SUBDIR");
        assert_eq!(r.first_cluster(), 5);
    }

    #[test]
    fn honors_the_vfat_lowercase_display_flags() {
        let e = entry(b"README  TXT", ATTR_ARCHIVE, NTRES_LOWER_BASE | NTRES_LOWER_EXT, 2, 1);
        assert_eq!(RawEntry::new(&e).short_name(), "readme.txt");
        assert!("README.TXT".eq_ignore_ascii_case(&RawEntry::new(&e).short_name()));
    }

    #[test]
    fn unescapes_a_name_beginning_with_0xe5() {
        let e = entry(b"\x05ILE    BIN", ATTR_ARCHIVE, 0, 2, 0);
        let name = RawEntry::new(&e).short_name();
        assert_eq!(name.as_bytes()[0], 0xC3);
        assert_eq!(name.as_bytes()[1], 0xA5);
        assert!(name.ends_with("ILE.BIN"));
    }

    #[test]
    fn recognizes_free_and_end_slots() {
        let mut deleted = entry(b"OLD     TXT", ATTR_ARCHIVE, 0, 2, 0);
        deleted[0] = 0xE5;
        let r = RawEntry::new(&deleted);
        assert!(r.is_free());
        assert!(!r.is_end());
        assert!(!r.is_short_entry());

        let end = [0u8; 32];
        let r = RawEntry::new(&end);
        assert!(r.is_end());
        assert!(r.is_free());
    }

    #[test]
    fn recognizes_and_skips_long_name_and_volume_slots() {
        let lfn = entry(b"XXXXXXXXXXX", ATTR_LONG_NAME, 0, 0, 0);
        let r = RawEntry::new(&lfn);
        assert!(r.is_long_name());
        assert!(!r.is_short_entry());

        let vol = entry(b"VOLUMENAME ", ATTR_VOLUME_ID, 0, 0, 0);
        let r = RawEntry::new(&vol);
        assert!(r.is_volume_id());
        assert!(!r.is_long_name());
        assert!(!r.is_short_entry());
    }

    #[test]
    fn encodes_and_round_trips_short_names() {
        let field = encode_short_name("hello.txt").unwrap();
        let mut slot = [0u8; 32];
        slot[0..11].copy_from_slice(&field);
        slot[11] = ATTR_ARCHIVE;
        assert_eq!(RawEntry::new(&slot).short_name(), "HELLO.TXT");
        assert_eq!(&encode_short_name("readme").unwrap(), b"README     ");
        assert_eq!(&encode_short_name("A.B").unwrap(), b"A       B  ");
    }

    #[test]
    fn rejects_names_that_are_not_8_3() {
        assert!(encode_short_name("toolongbase.txt").is_none());
        assert!(encode_short_name("f.toolong").is_none());
        assert!(encode_short_name("bad*name.txt").is_none());
        assert!(encode_short_name("").is_none());
        assert!(encode_short_name(".").is_none());
        assert!(encode_short_name("..").is_none());
    }

    #[test]
    fn lfn_checksum_matches_a_hand_computed_reference() {
        assert_eq!(lfn_checksum(b"A          "), 0x80);
        assert_ne!(lfn_checksum(b"HELLO   TXT"), lfn_checksum(b"HELLO   BIN"));
    }

    #[test]
    fn lfn_entry_round_trips_its_units() {
        let chars: [u16; LFN_CHARS_PER_ENTRY] = [
            b'H' as u16, b'e' as u16, b'l' as u16, b'l' as u16, b'o' as u16, 0x0000, 0xFFFF,
            0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF, 0xFFFF,
        ];
        let mut slot = [0u8; 32];
        put_lfn_entry(&mut slot, LFN_ORDINAL_MASK & 1 | LAST_LONG_ENTRY, &chars, 0x5A);
        assert_eq!(slot[11], ATTR_LONG_NAME);
        assert_eq!(slot[13], 0x5A);
        let raw = RawEntry::new(&slot);
        assert_eq!(raw.lfn_order() & LFN_ORDINAL_MASK, 1);
        assert_ne!(raw.lfn_order() & LAST_LONG_ENTRY, 0);
        assert_eq!(raw.lfn_units(), chars);
        assert_eq!(lfn_name_from_units(&chars), "Hello");
    }

    #[test]
    fn lfn_name_reassembles_across_entries() {
        let name = "Some Rather Long Name.txt";
        let units: alloc::vec::Vec<u16> = name.encode_utf16().collect();
        let mut padded = units.clone();
        if padded.len() % LFN_CHARS_PER_ENTRY != 0 {
            padded.push(0x0000);
            while padded.len() % LFN_CHARS_PER_ENTRY != 0 {
                padded.push(0xFFFF);
            }
        }
        assert_eq!(lfn_name_from_units(&padded), name);
    }
}
