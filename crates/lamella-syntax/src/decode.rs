//! Decoding source bytes to text the way csc does (Roslyn's `EncodedStringText`).

use alloc::string::String;

/// The encoding [`decode_source`] selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// UTF-8 (with or without a BOM).
    Utf8,
    /// UTF-16 little-endian (had a `FF FE` BOM).
    Utf16Le,
    /// UTF-16 big-endian (had a `FE FF` BOM).
    Utf16Be,
    /// An ANSI code page (no BOM and not valid UTF-8), named by its Windows code-page number
    /// (e.g. 1252 for Western European).
    CodePage(u16),
}

/// Windows-1252's `0x80..=0x9F` range, where it diverges from Latin-1. The five
/// unmapped slots fall back to the C1 control of the same value, matching .NET.
const CP1252_HIGH: [char; 32] = [
    '\u{20AC}', '\u{0081}', '\u{201A}', '\u{0192}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{02C6}', '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{008D}', '\u{017D}', '\u{008F}',
    '\u{0090}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{02DC}', '\u{2122}', '\u{0161}', '\u{203A}', '\u{0153}', '\u{009D}', '\u{017E}', '\u{0178}',
];

/// Decodes `bytes` to source text, returning the text and the encoding chosen. `default_code_page`
/// is the host ANSI code page (Windows code-page number) used when the bytes have no BOM and are
/// not valid UTF-8 -- what csc would decode a non-Unicode source in.
#[must_use]
pub fn decode_source(bytes: &[u8], default_code_page: u16) -> (String, Encoding) {
    let (mut text, encoding) = if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        (decode_utf8(rest), Encoding::Utf8)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (decode_utf16(rest, false), Encoding::Utf16Le)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (decode_utf16(rest, true), Encoding::Utf16Be)
    } else if let Ok(text) = core::str::from_utf8(bytes) {
        (String::from(text), Encoding::Utf8)
    } else {
        (
            decode_code_page(bytes, default_code_page),
            Encoding::CodePage(default_code_page),
        )
    };
    if let Some(without) = text.strip_prefix('\u{FEFF}') {
        text = String::from(without);
    }
    (text, encoding)
}

/// Decodes UTF-8 bytes, replacing any invalid sequence with U+FFFD (csc would
/// instead report an error, which the driver can layer on later).
fn decode_utf8(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// Decodes UTF-16 code units (in the given byte order), replacing unpaired
/// surrogates with U+FFFD. A trailing odd byte is dropped.
fn decode_utf16(bytes: &[u8], big_endian: bool) -> String {
    let units = bytes.chunks_exact(2).map(|pair| {
        if big_endian {
            u16::from_be_bytes([pair[0], pair[1]])
        } else {
            u16::from_le_bytes([pair[0], pair[1]])
        }
    });
    char::decode_utf16(units)
        .map(|unit| unit.unwrap_or('\u{FFFD}'))
        .collect()
}

/// Decodes `bytes` in the given ANSI code page. Windows-1252 (the Western/US default) is
/// hand-written; the other single-byte pages (1250-1258, 874) come from generated tables
/// ([`crate::codepages`]). A page with no single-byte table -- notably the double-byte CJK pages
/// (932/936/949/950, a Phase 3 addition) -- falls back to Windows-1252.
fn decode_code_page(bytes: &[u8], code_page: u16) -> String {
    if code_page != 1252 {
        if let Some(high) = crate::codepages::single_byte_high(code_page) {
            return bytes
                .iter()
                .map(|&byte| {
                    if byte < 0x80 {
                        char::from(byte)
                    } else {
                        high[(byte - 0x80) as usize]
                    }
                })
                .collect();
        }
    }
    decode_windows_1252(bytes)
}

/// Decodes bytes in Windows-1252: ASCII below `0x80`, the divergent range from the
/// table, and Latin-1 (code point equals byte) for `0xA0..=0xFF`.
fn decode_windows_1252(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&byte| match byte {
            0x80..=0x9F => CP1252_HIGH[(byte - 0x80) as usize],
            other => char::from(other),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ascii_is_utf8() {
        let (text, encoding) = decode_source(b"class C {}", 1252);
        assert_eq!(text, "class C {}");
        assert_eq!(encoding, Encoding::Utf8);
    }

    #[test]
    fn a_utf8_bom_is_stripped() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice(b"class C {}");
        let (text, encoding) = decode_source(&bytes, 1252);
        assert_eq!(text, "class C {}");
        assert_eq!(encoding, Encoding::Utf8);
    }

    #[test]
    fn utf16_le_and_be_boms_decode() {
        let le = [0xFF, 0xFE, b'A', 0x00, b'B', 0x00];
        let be = [0xFE, 0xFF, 0x00, b'A', 0x00, b'B'];
        assert_eq!(decode_source(&le, 1252), (String::from("AB"), Encoding::Utf16Le));
        assert_eq!(decode_source(&be, 1252), (String::from("AB"), Encoding::Utf16Be));
    }

    #[test]
    fn invalid_utf8_falls_back_to_the_ansi_code_page() {
        let (text, encoding) = decode_source(&[b'x', 0x93, b'y', 0x94], 1252);
        assert_eq!(text, "x\u{201C}y\u{201D}");
        assert_eq!(encoding, Encoding::CodePage(1252));
    }

    #[test]
    fn a_single_byte_code_page_decodes_from_its_generated_table() {
        let (text, encoding) = decode_source(&[b'x', 0x80], 1251);
        assert_eq!(text, "x\u{0402}");
        assert_eq!(encoding, Encoding::CodePage(1251));
        let (fallback, enc) = decode_source(&[0x93], 932);
        assert_eq!(fallback, "\u{201C}");
        assert_eq!(enc, Encoding::CodePage(932));
    }
}
