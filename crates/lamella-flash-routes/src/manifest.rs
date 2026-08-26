//! The sidecar record beside an image, which says what the image IS.

use std::path::{Path, PathBuf};

/// The flash-image sidecar record: what the producer says about the artifact beside it.
///
#[derive(Clone, Debug)]
pub struct Manifest {
    /// The record's own version. A newer one is refused rather than read hopefully.
    pub schema: u64,
    /// `"flash-image"`. The sidecar carrier holds other kinds; this reads one.
    pub kind: String,
    /// The catalog row the image was built for.
    pub board: String,
    /// `"elf"` or `"bin"`.
    pub format: String,
    /// The address the bytes belong at -- present for `bin`, absent for `elf`.
    pub base: Option<u32>,
    /// The digest of the artifact bytes as shipped, lowercase hex.
    pub sha256: String,
    /// The artifact's length in bytes.
    pub bytes: u64,
    /// Report-only provenance. **NOTHING MAY BRANCH ON IT** -- the record says so, and the reason
    /// is the payload-agnostic principle this verb is built on: a board does not care what wrote
    /// its bytes, so neither may the code that writes them.
    pub producer: Option<String>,
}

/// The schema version this reads.
///
/// A record numbered higher describes something written after this was; reading it as if the extra
/// were absent is how a tool honors half a contract. The refusal names the number so the reader can
/// tell "too new" from "malformed".
const SCHEMA: u64 = 1;

/// Where the sidecar for `image` lives.
///
/// **THE WHOLE FILE NAME, NOT THE STEM.** `<image>.manifest.json` beside the artifact, so
/// `blink.elf` is described by `blink.elf.manifest.json`. Replacing the extension instead would put
/// `blink.elf` and `blink.bin` on ONE sidecar -- and the metadata contract requires both to exist
/// together, calling them the standing forensic pair, so that collision is not a corner case but
/// the ordinary output of a build.
#[must_use]
pub fn path_for(image: &Path) -> PathBuf {
    let mut name = image.file_name().unwrap_or_default().to_os_string();
    name.push(".manifest.json");
    image.with_file_name(name)
}

/// The sidecar beside `image`, if there is one.
///
/// # Errors
/// A sidecar that exists and cannot be read as the v0.1 record. Absence is `Ok(None)`.
pub fn read(image: &Path) -> Result<Option<Manifest>, String> {
    let path = path_for(image);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    parse(&text).map(Some).map_err(|why| format!("{}: {why}", path.display()))
}

/// The record `text` states.
///
/// # Errors
/// Not JSON, not an object, a missing required field, a field of the wrong type, a schema this does
/// not read, or a `base` that disagrees with the format rule.
pub fn parse(text: &str) -> Result<Manifest, String> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|why| format!("not JSON -- {why}"))?;
    let object = value.as_object().ok_or("not a JSON object")?;

    let schema = object
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .ok_or("no `schema` number, so there is no way to know what the rest means")?;
    if schema != SCHEMA {
        return Err(format!(
            "schema {schema}, and this reads schema {SCHEMA}. A record written to a later contract \
             may describe something this would ignore."
        ));
    }
    let kind = string(object, "kind")?;
    if kind != "flash-image" {
        return Err(format!(
            "kind {kind:?}. This is the sidecar for an image to be written to a chip, and a record \
             of another kind is not this verb's to act on."
        ));
    }

    let format = string(object, "format")?;
    if format != "elf" && format != "bin" {
        return Err(format!("format {format:?}, and the record defines `elf` and `bin` only"));
    }

    let base = match object.get("base") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::String(text)) => Some(parse_base(text)?),
        Some(_) => {
            return Err("`base` must be a hex string or null".to_owned());
        }
    };
    match (format.as_str(), base) {
        ("bin", None) => {
            return Err(
                "format `bin` with no `base`. Flat bytes carry no address, so the record has to \
                 supply one."
                    .to_owned(),
            );
        }
        ("elf", Some(_)) => {
            return Err(
                "format `elf` with a `base`. A linked ELF states its own addresses, and a second \
                 claim about one fact is a claim that can disagree."
                    .to_owned(),
            );
        }
        _ => {}
    }

    let sha256 = string(object, "sha256")?.to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("`sha256` is not 64 hex digits: {sha256:?}"));
    }
    let bytes = object
        .get("bytes")
        .and_then(serde_json::Value::as_u64)
        .ok_or("no `bytes` count, so the length claim cannot be checked")?;

    Ok(Manifest {
        schema,
        kind,
        board: string(object, "board")?,
        format,
        base,
        sha256,
        bytes,
        producer: object.get("producer").and_then(serde_json::Value::as_str).map(str::to_owned),
    })
}

/// A required string field.
fn string(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Result<String, String> {
    object
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("no `{key}` string"))
}

/// A `base` as the record spells it: hex, with or without the `0x` a reader expects.
fn parse_base(text: &str) -> Result<u32, String> {
    let digits = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")).unwrap_or(text);
    u32::from_str_radix(digits, 16)
        .map_err(|_| format!("`base` {text:?} is not a hex address"))
}

/// What the sidecar attests, in the words a person reads before a write.
///
/// The digest is abbreviated because the whole of it identifies nothing to a reader and the first
/// bytes distinguish one build from another, which is the question a person actually has.
#[must_use]
pub fn attestation(manifest: &Manifest) -> String {
    let short: String = manifest.sha256.chars().take(12).collect();
    match &manifest.producer {
        Some(producer) => format!(
            "sidecar: {} B for {}, sha256 {short}..., built by {producer}",
            manifest.bytes, manifest.board
        ),
        None => {
            format!("sidecar: {} B for {}, sha256 {short}...", manifest.bytes, manifest.board)
        }
    }
}

/// Check `image` against what the sidecar says it is.
///
/// `extension` is the artifact's own extension, which is how the file says its format; the record
/// says the same thing a second time and the two must agree.
///
/// # Errors
/// A length, digest or format the record disagrees with. Each refusal names the two values, because
/// "does not match" leaves a reader with no way to tell a stale sidecar from a wrong image.
pub fn check_identity(
    manifest: &Manifest,
    image: &[u8],
    extension: Option<&str>,
) -> Result<(), String> {
    match extension {
        Some(extension) if extension == manifest.format => {}
        Some(extension) => {
            return Err(format!(
                "the sidecar describes a file in {:?} format and this one is {extension:?}.\n\
                 The v0.1 record describes `elf` and `bin`; an image in another format has no \
                 sidecar that can\ndescribe it, so remove the sidecar or point at the artifact it \
                 belongs to.",
                manifest.format
            ));
        }
        None => {
            return Err(format!(
                "the sidecar describes a file in {:?} format, and this file has no extension to \
                 check it against.",
                manifest.format
            ));
        }
    }

    let length = image.len() as u64;
    if length != manifest.bytes {
        return Err(format!(
            "the sidecar says {} B and the file is {length} B, so this is not the artifact it \
             describes.",
            manifest.bytes
        ));
    }

    let digest = hex(lamella_pe::sha256::sha256(image));
    if digest != manifest.sha256 {
        return Err(format!(
            "the sidecar's digest does not match these bytes.\n\
             \x20   sidecar  {}\n\
             \x20   file     {digest}\n\
             The record identifies the LINKED ARTIFACT AS SHIPPED, so a rebuild does not produce \
             the same\ndigest and is not the same artifact.",
            manifest.sha256
        ));
    }
    Ok(())
}

/// Check that the sidecar and the caller name the same board.
///
/// # Errors
/// Two different boards, which is the mistake the record exists to catch.
pub fn check_board(manifest: &Manifest, board_id: &str) -> Result<(), String> {
    if manifest.board == board_id {
        return Ok(());
    }
    Err(format!(
        "this image was built for {}, and {board_id} was named.\n\
         Nothing was written. Flash it to the board it was built for, or point at the image built \
         for this one.",
        manifest.board
    ))
}

/// Lowercase hex, the spelling the record uses.
fn hex(digest: [u8; 32]) -> String {
    let mut text = String::with_capacity(64);
    for byte in digest {
        text.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        text.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::{
        attestation, check_board, check_identity, hex, parse, path_for, read, Manifest,
    };
    use std::path::Path;

    /// A record for `bytes`, correct in every field, so a test can spoil exactly one.
    fn record_for(bytes: &[u8], format: &str, base: &str) -> String {
        format!(
            "{{\"schema\":1,\"kind\":\"flash-image\",\"board\":\"micro-bit-v2\",\
             \"format\":\"{format}\",\"base\":{base},\"sha256\":\"{}\",\"bytes\":{},\
             \"producer\":\"lamella-aot\"}}",
            hex(lamella_pe::sha256::sha256(bytes)),
            bytes.len()
        )
    }

    /// THE DIGEST IS PINNED TO A VALUE THIS TREE DID NOT PRODUCE. SHA-256 of the empty input is a
    /// published constant, so this checks the hex spelling and the digest together against
    /// something outside the code under test -- which is the only way the pass case means anything.
    #[test]
    fn hex_matches_the_published_digest_of_the_empty_input() {
        assert_eq!(
            hex(lamella_pe::sha256::sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// stem would describe both with one file and one of them would be wrong.
    #[test]
    fn the_sidecar_is_named_after_the_whole_file() {
        assert_eq!(path_for(Path::new("out/blink.elf")), Path::new("out/blink.elf.manifest.json"));
        assert_eq!(path_for(Path::new("out/blink.bin")), Path::new("out/blink.bin.manifest.json"));
    }

    #[test]
    fn an_elf_record_reads() {
        let manifest = parse(&record_for(b"image", "elf", "null")).expect("a valid elf record");
        assert_eq!(manifest.board, "micro-bit-v2");
        assert_eq!(manifest.format, "elf");
        assert_eq!(manifest.base, None);
        assert_eq!(manifest.bytes, 5);
        assert_eq!(manifest.producer.as_deref(), Some("lamella-aot"));
    }

    #[test]
    fn a_bin_record_carries_its_base() {
        let manifest =
            parse(&record_for(b"image", "bin", "\"0x08000000\"")).expect("a valid bin record");
        assert_eq!(manifest.base, Some(0x0800_0000));
        let bare = parse(&record_for(b"image", "bin", "\"08000000\"")).expect("a bare hex base");
        assert_eq!(bare.base, Some(0x0800_0000));
    }

    /// The two halves of one rule, and BOTH directions are checked: a repair that adds a correct
    /// path is not proved by the correct path working.
    #[test]
    fn the_base_rule_is_refused_in_both_directions() {
        let no_base = parse(&record_for(b"image", "bin", "null")).unwrap_err();
        assert!(no_base.contains("no `base`"), "{no_base}");
        let extra_base = parse(&record_for(b"image", "elf", "\"0x08000000\"")).unwrap_err();
        assert!(extra_base.contains("with a `base`"), "{extra_base}");
    }

    #[test]
    fn a_later_schema_is_refused_rather_than_read_hopefully() {
        let text = record_for(b"image", "elf", "null").replace("\"schema\":1", "\"schema\":2");
        let why = parse(&text).unwrap_err();
        assert!(why.contains("schema 2"), "{why}");
    }

    #[test]
    fn a_record_of_another_kind_is_not_this_verbs_to_act_on() {
        let text = record_for(b"image", "elf", "null").replace("flash-image", "bundle");
        let why = parse(&text).unwrap_err();
        assert!(why.contains("kind"), "{why}");
    }

    #[test]
    fn a_digest_that_is_not_64_hex_digits_is_refused() {
        let text = record_for(b"image", "elf", "null")
            .replace(&hex(lamella_pe::sha256::sha256(b"image")), "abc123");
        let why = parse(&text).unwrap_err();
        assert!(why.contains("64 hex digits"), "{why}");
    }

    /// The record says unknown fields are tolerated, so a producer emitting the fuller section-1
    /// object must not be refused by a reader that only needs eight fields.
    #[test]
    fn unknown_fields_are_tolerated() {
        let text = record_for(b"image", "elf", "null")
            .replace("\"schema\":1", "\"schema\":1,\"set_id\":\"abc\",\"regions\":[]");
        assert!(parse(&text).is_ok(), "{text}");
    }

    #[test]
    fn identity_passes_on_the_bytes_the_record_describes() {
        let manifest = parse(&record_for(b"image", "bin", "\"0x0\"")).unwrap();
        assert!(check_identity(&manifest, b"image", Some("bin")).is_ok());
    }

    /// a truncated file, which are different problems with different fixes.
    #[test]
    fn a_different_length_is_refused_and_both_numbers_are_named() {
        let manifest = parse(&record_for(b"image", "bin", "\"0x0\"")).unwrap();
        let why = check_identity(&manifest, b"imag", Some("bin")).unwrap_err();
        assert!(why.contains("5 B") && why.contains("4 B"), "{why}");
    }

    #[test]
    fn different_bytes_of_the_same_length_are_refused_by_digest() {
        let manifest = parse(&record_for(b"image", "bin", "\"0x0\"")).unwrap();
        let why = check_identity(&manifest, b"imagf", Some("bin")).unwrap_err();
        assert!(why.contains("digest does not match"), "{why}");
    }

    /// A sidecar written for the `.elf` and left beside the `.bin` fails length and digest too, so
    /// the format is checked FIRST and the report says the one thing that explains all three.
    #[test]
    fn a_sidecar_for_the_other_half_of_the_pair_is_refused_by_format() {
        let manifest = parse(&record_for(b"image", "elf", "null")).unwrap();
        let why = check_identity(&manifest, b"image", Some("bin")).unwrap_err();
        assert!(why.contains("\"elf\"") && why.contains("\"bin\""), "{why}");
    }

    #[test]
    fn a_sidecar_beside_a_format_the_record_cannot_describe_is_refused() {
        let manifest = parse(&record_for(b"image", "bin", "\"0x0\"")).unwrap();
        let why = check_identity(&manifest, b"image", Some("hex")).unwrap_err();
        assert!(why.contains("`elf` and `bin`"), "{why}");
    }

    #[test]
    fn the_wrong_board_is_refused_and_both_are_named() {
        let manifest = parse(&record_for(b"image", "elf", "null")).unwrap();
        assert!(check_board(&manifest, "micro-bit-v2").is_ok());
        let why = check_board(&manifest, "pico2").unwrap_err();
        assert!(why.contains("micro-bit-v2") && why.contains("pico2"), "{why}");
        assert!(why.contains("Nothing was written"), "{why}");
    }

    /// sidecar that does not parse is a claim nobody can check.
    #[test]
    fn an_absent_sidecar_reads_as_absence_and_a_broken_one_as_a_refusal() {
        let dir = std::env::temp_dir().join("lamella-manifest-read-test");
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let image = dir.join("blink.bin");
        std::fs::write(&image, b"image").expect("write the image");
        let sidecar = path_for(&image);
        let _ = std::fs::remove_file(&sidecar);
        assert!(read(&image).expect("absence is not an error").is_none());

        std::fs::write(&sidecar, "{ not json").expect("write a broken sidecar");
        let why = read(&image).unwrap_err();
        assert!(why.contains("not JSON"), "{why}");
        assert!(why.contains("blink.bin.manifest.json"), "{why}");

        std::fs::write(&sidecar, record_for(b"image", "bin", "\"0x0\"")).expect("write a record");
        let manifest = read(&image).expect("a valid sidecar").expect("present");
        assert_eq!(manifest.board, "micro-bit-v2");
        let _ = std::fs::remove_file(&sidecar);
        let _ = std::fs::remove_file(&image);
    }

    /// five shipped sentences printed columns of stray spaces before anyone looked at the output.
    #[test]
    fn every_message_a_person_reads_renders_without_stray_columns() {
        let manifest = parse(&record_for(b"image", "elf", "null")).unwrap();
        let rendered = [
            attestation(&manifest),
            check_board(&manifest, "pico2").unwrap_err(),
            check_identity(&manifest, b"image", Some("bin")).unwrap_err(),
            check_identity(&manifest, b"imagf", Some("elf")).unwrap_err(),
            check_identity(&manifest, b"imag", Some("elf")).unwrap_err(),
        ];
        for message in rendered {
            for line in message.lines() {
                assert!(
                    !line.starts_with("  ") || line.starts_with("\x20\x20\x20 "),
                    "a continuation kept its source indentation:\n{message}"
                );
                assert!(!line.contains("  ") || line.contains("\x20\x20\x20 "), "{message}");
            }
        }
    }

    /// The line a person reads before the write, which is the only place the sidecar is visible
    /// when everything agrees.
    #[test]
    fn the_attestation_names_the_board_and_the_build() {
        let manifest = parse(&record_for(b"image", "elf", "null")).unwrap();
        let line = attestation(&manifest);
        assert!(line.contains("micro-bit-v2"), "{line}");
        assert!(line.contains("lamella-aot"), "{line}");
        assert!(line.contains("5 B"), "{line}");

        let anonymous = Manifest { producer: None, ..manifest };
        assert!(!attestation(&anonymous).contains("built by"), "{}", attestation(&anonymous));
    }
}
