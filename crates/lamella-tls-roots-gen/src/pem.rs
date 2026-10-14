//! Reads curl's `cacert.pem`: the line naming the date of the Mozilla store it was extracted from,
//! and the DER of each `CERTIFICATE` block (RFC 7468), decoded from base64 (RFC 4648). Text
//! outside the blocks is commentary and is skipped. Any other block type, an unterminated block
//! and a malformed base64 body are refused, with the line they are on.

/// The text that introduces the snapshot date in curl's header, as in
/// `## Certificate data from Mozilla as of: Thu May 14 03:12:02 2026 GMT`.
const SNAPSHOT_MARKER: &str = "Certificate data from Mozilla as of:";

const BEGIN_CERTIFICATE: &str = "-----BEGIN CERTIFICATE-----";
const END_CERTIFICATE: &str = "-----END CERTIFICATE-----";

/// One `CERTIFICATE` block: the line its `BEGIN` boundary is on, counting from 1, and its DER.
pub(crate) struct Block {
    pub(crate) line: usize,
    pub(crate) der: Vec<u8>,
}

/// The snapshot date: the rest of the first line that carries [`SNAPSHOT_MARKER`], without the
/// surrounding blanks or any trailing `#`. It must be printable ASCII, because the store's Rust
/// source carries it as a string literal.
pub(crate) fn snapshot(text: &str) -> Result<&str, String> {
    let marked = text.lines().enumerate().find_map(|(index, line)| {
        line.split_once(SNAPSHOT_MARKER)
            .map(|(_, date)| (index + 1, date))
    });
    let Some((number, date)) = marked else {
        return Err(format!(
            "no \"{SNAPSHOT_MARKER}\" line, which curl's cacert.pem opens with"
        ));
    };
    let date = date.trim().trim_end_matches('#').trim();
    if date.is_empty() {
        return Err(format!("line {number}: the snapshot line names no date"));
    }
    if !date
        .bytes()
        .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return Err(format!(
            "line {number}: the snapshot date is not printable ASCII"
        ));
    }
    Ok(date)
}

/// The DER of every `CERTIFICATE` block, in file order.
pub(crate) fn certificates(text: &str) -> Result<Vec<Block>, String> {
    let mut blocks = Vec::new();
    let mut open: Option<(usize, String)> = None;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        let line = line.trim();
        match open.as_mut() {
            None if line == BEGIN_CERTIFICATE => open = Some((number, String::new())),
            None if line.starts_with("-----BEGIN ") => {
                return Err(format!(
                    "line {number}: {line} -- only CERTIFICATE blocks belong in a CA store"
                ));
            }
            None if line.starts_with("-----END ") => {
                return Err(format!(
                    "line {number}: {line} closes a block that never began"
                ));
            }
            None => {}
            Some((begin, body)) if line == END_CERTIFICATE => {
                let der = decode_base64(body)
                    .map_err(|error| format!("the block at line {begin}: {error}"))?;
                blocks.push(Block { line: *begin, der });
                open = None;
            }
            Some((begin, _)) if line.starts_with("-----") => {
                return Err(format!(
                    "line {number}: {line} inside the block that begins at line {begin}"
                ));
            }
            Some((_, body)) => body.push_str(line),
        }
    }
    if let Some((begin, _)) = open {
        return Err(format!(
            "the block at line {begin} has no {END_CERTIFICATE} line"
        ));
    }
    if blocks.is_empty() {
        return Err("no CERTIFICATE blocks".to_string());
    }
    Ok(blocks)
}

/// Decodes base64 in the standard alphabet with `=` padding, ignoring ASCII whitespace.
fn decode_base64(text: &str) -> Result<Vec<u8>, String> {
    let symbols: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if !symbols.len().is_multiple_of(4) {
        return Err(format!(
            "{} base64 symbols, which is not a whole number of 4-symbol groups",
            symbols.len()
        ));
    }
    let groups = symbols.len() / 4;
    let mut bytes = Vec::with_capacity(groups * 3);
    for (index, group) in symbols.chunks_exact(4).enumerate() {
        let padding = group
            .iter()
            .rev()
            .take_while(|&&symbol| symbol == b'=')
            .count();
        if padding > 2 || (padding > 0 && index + 1 != groups) {
            return Err("misplaced '=' padding".to_string());
        }
        let mut value = 0u32;
        for &symbol in &group[..4 - padding] {
            value = value << 6 | u32::from(sextet(symbol)?);
        }
        value <<= 6 * padding;
        let [_, high, middle, low] = value.to_be_bytes();
        bytes.extend_from_slice(&[high, middle, low][..3 - padding]);
    }
    Ok(bytes)
}

/// The 6-bit value of one symbol of the standard base64 alphabet.
fn sextet(symbol: u8) -> Result<u8, String> {
    match symbol {
        b'A'..=b'Z' => Ok(symbol - b'A'),
        b'a'..=b'z' => Ok(symbol - b'a' + 26),
        b'0'..=b'9' => Ok(symbol - b'0' + 52),
        b'+' => Ok(62),
        b'/' => Ok(63),
        _ => Err(format!(
            "'{}' is not a base64 symbol",
            symbol.escape_ascii()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_decodes_every_padding_length() {
        assert_eq!(decode_base64("").unwrap(), b"");
        assert_eq!(decode_base64("Zg==").unwrap(), b"f");
        assert_eq!(decode_base64("Zm8=").unwrap(), b"fo");
        assert_eq!(decode_base64("Zm9v").unwrap(), b"foo");
        assert_eq!(decode_base64("Zm9vYmFy").unwrap(), b"foobar");
        assert_eq!(decode_base64("Zm9v\r\nYg==").unwrap(), b"foob");
        assert_eq!(decode_base64("+/+/").unwrap(), [0xfb, 0xff, 0xbf]);
    }

    #[test]
    fn base64_refuses_what_is_not_base64() {
        assert!(decode_base64("Zm9").is_err(), "a partial group");
        assert!(
            decode_base64("Zg==Zm9v").is_err(),
            "padding before the last group"
        );
        assert!(decode_base64("Z===").is_err(), "three padding symbols");
        assert!(decode_base64("Zm=v").is_err(), "padding inside a group");
        assert!(
            decode_base64("Zm9-").is_err(),
            "a symbol of the URL-safe alphabet"
        );
    }

    #[test]
    fn the_snapshot_is_the_date_on_the_marker_line() {
        let text = "##\n## Bundle of CA Root Certificates\n##\n\
                    ## Certificate data from Mozilla as of: Thu May 14 03:12:02 2026 GMT\n##\n";
        assert_eq!(snapshot(text).unwrap(), "Thu May 14 03:12:02 2026 GMT");
        assert_eq!(
            snapshot("## Certificate data from Mozilla as of: Mon Jan 5 2026 ##\r\n").unwrap(),
            "Mon Jan 5 2026"
        );
        assert!(
            snapshot("## Bundle of CA Root Certificates\n").is_err(),
            "no marker line"
        );
        assert!(
            snapshot("## Certificate data from Mozilla as of: ##\n").is_err(),
            "no date"
        );
        assert!(
            snapshot("## Certificate data from Mozilla as of: Thu\tMay\n").is_err(),
            "a control character"
        );
    }

    #[test]
    fn certificate_blocks_decode_in_file_order_and_commentary_is_skipped() {
        let text = "## header\r\n\r\nFirst Root CA\r\n=============\r\n\
                    -----BEGIN CERTIFICATE-----\r\nMAA=\r\n-----END CERTIFICATE-----\r\n\r\n\
                    Second Root CA\r\n-----BEGIN CERTIFICATE-----\r\nMAEF\r\n\
                    -----END CERTIFICATE-----\r\n";
        let blocks = certificates(text).unwrap();
        assert_eq!(blocks.len(), 2);
        assert_eq!(
            (blocks[0].line, blocks[0].der.as_slice()),
            (5, &[0x30, 0x00][..])
        );
        assert_eq!(
            (blocks[1].line, blocks[1].der.as_slice()),
            (10, &[0x30, 0x01, 0x05][..])
        );
    }

    #[test]
    fn a_malformed_store_is_refused_with_its_line() {
        let open = certificates("-----BEGIN CERTIFICATE-----\nMAA=\n")
            .err()
            .unwrap();
        assert!(
            open.contains("line 1") && open.contains("no -----END CERTIFICATE----- line"),
            "{open}"
        );
        let other = certificates("-----BEGIN PRIVATE KEY-----\nMAA=\n-----END PRIVATE KEY-----\n")
            .err()
            .unwrap();
        assert!(other.starts_with("line 1:"), "{other}");
        let stray = certificates("x\n-----END CERTIFICATE-----\n")
            .err()
            .unwrap();
        assert!(stray.starts_with("line 2:"), "{stray}");
        let nested = certificates("-----BEGIN CERTIFICATE-----\n-----BEGIN CERTIFICATE-----\n")
            .err()
            .unwrap();
        assert!(nested.starts_with("line 2:"), "{nested}");
        let bad = certificates("\n-----BEGIN CERTIFICATE-----\nMA*=\n-----END CERTIFICATE-----\n")
            .err()
            .unwrap();
        assert!(bad.starts_with("the block at line 2:"), "{bad}");
        assert!(
            certificates("## only commentary\n").is_err(),
            "no blocks at all"
        );
    }
}
