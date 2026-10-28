//! The sidecar record beside an image, which says what the image IS.

use std::path::{Path, PathBuf};

/// The flash-image sidecar record: what the producer says about the artifact beside it.
///
/// Field for field the Lamella RHU record, including the ones nothing here acts on. `producer` is
/// carried because a report that cannot name what built an image is a report a person cannot
/// follow up.
#[derive(Clone, Debug)]
pub struct Manifest {
    /// The record's own version. A newer one is refused rather than read hopefully.
    pub schema: u64,
    /// `"flash-image"`. The sidecar carrier holds other kinds; this reads one.
    pub kind: String,
    /// The execution model the image runs under: [`NATIVE`] for a linked image, [`INTERPRETER`] for
    /// one an interpreter runs. Required in schema 1, written third.
    pub model: String,
    /// The catalog row the image was built for.
    pub board: String,
    /// `"elf"` or `"bin"`, or Lamella's own `"lmli"`, the image its firmware runs.
    pub format: String,
    /// The address the bytes belong at -- present for `bin`, absent for `elf` and `lmli`.
    pub base: Option<u32>,
    /// The digest of the artifact bytes as shipped, lowercase hex.
    pub sha256: String,
    /// The artifact's length in bytes.
    pub bytes: u64,
    /// Report-only provenance. **NOTHING MAY BRANCH ON IT** -- the record says so, and the reason
    /// is the payload-agnostic principle this verb is built on: a board does not care what wrote
    /// its bytes, so neither may the code that writes them.
    pub producer: Option<String>,
    /// The file name of the linked ELF the build wrote beside the artifact, when it names one: the
    /// file whose symbols give a `bin` artifact's addresses.
    pub elf: Option<String>,
    /// The channel the program's console stream was built to use: `memory`, `probe`, `uart0`,
    /// `uart1` or `off`.
    pub console_channel: Option<String>,
    /// The channel the log stream was built to use, in the same words.
    pub log_channel: Option<String>,
}

/// The schema version this reads.
///
/// A record numbered higher describes something written after this was; reading it as if the extra
/// were absent is how a tool honors half a contract. The refusal names the number so the reader can
/// tell "too new" from "malformed".
pub(crate) const SCHEMA: u64 = 1;

/// The execution model of an image whose code was compiled ahead of time and linked: every image
/// Lamella RHU writes, and every one `lamella build --format` writes.
pub const NATIVE: &str = "native";

/// The execution model of an image an interpreter runs.
pub const INTERPRETER: &str = "interpreter";

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
/// A sidecar that exists and cannot be read as the Lamella RHU record. Absence is `Ok(None)`.
pub fn read(image: &Path) -> Result<Option<Manifest>, String> {
    let path = path_for(image);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    parse(&text).map(Some).map_err(|why| format!("{}: {why}", path.display()))
}

/// The record for a linked ELF a toolchain wrote: `shipped` is the file as written, and `producer`
/// names what wrote it. It carries no `base`, because an ELF states its own addresses.
#[must_use]
pub fn for_elf(board: &str, shipped: &[u8], producer: &str) -> Manifest {
    of_artifact(board, "elf", None, shipped, producer)
}

/// The record for flat bytes a toolchain wrote, which belong at `base`: `shipped` is the file as
/// written, and `producer` names what wrote it.
#[must_use]
pub fn for_bin(board: &str, base: u32, shipped: &[u8], producer: &str) -> Manifest {
    of_artifact(board, "bin", Some(base), shipped, producer)
}

/// The record for an image Lamella's firmware runs, a `.lmli`, which `dotnet publish` writes for a
/// `lamella1.0` program when `LamellaPublishAot` is false: `shipped` is the file as written, and
/// `producer` names what wrote it. Lamella's own format: the firmware places the image, so it
/// carries no `base`, and its model is [`INTERPRETER`].
#[must_use]
pub fn for_lmli(board: &str, shipped: &[u8], producer: &str) -> Manifest {
    Manifest { model: INTERPRETER.to_owned(), ..of_artifact(board, LMLI, None, shipped, producer) }
}

/// The format word of an image Lamella's firmware runs.
pub const LMLI: &str = "lmli";

/// The record for `shipped` in `format`, the one shape every format shares.
fn of_artifact(board: &str, format: &str, base: Option<u32>, shipped: &[u8], producer: &str) -> Manifest {
    Manifest {
        schema: SCHEMA,
        kind: "flash-image".to_owned(),
        model: NATIVE.to_owned(),
        board: board.to_owned(),
        format: format.to_owned(),
        base,
        sha256: hex(lamella_pe::sha256::sha256(shipped)),
        bytes: shipped.len() as u64,
        producer: Some(producer.to_owned()),
        elf: None,
        console_channel: None,
        log_channel: None,
    }
}

/// `manifest` as the text a reader parses: one JSON object, its fields in the record's order and
/// each optional one only when it is present, with `base` spelled as eight hex digits.
#[must_use]
pub fn render(manifest: &Manifest) -> String {
    let quoted = |text: &str| serde_json::Value::String(text.to_owned()).to_string();
    let mut fields = vec![
        ("schema", manifest.schema.to_string()),
        ("kind", quoted(&manifest.kind)),
        ("model", quoted(&manifest.model)),
        ("board", quoted(&manifest.board)),
        ("format", quoted(&manifest.format)),
    ];
    if let Some(base) = manifest.base {
        fields.push(("base", quoted(&format!("{base:#010x}"))));
    }
    fields.push(("sha256", quoted(&manifest.sha256)));
    fields.push(("bytes", manifest.bytes.to_string()));
    for (key, value) in [
        ("producer", &manifest.producer),
        ("elf", &manifest.elf),
        ("console_channel", &manifest.console_channel),
        ("log_channel", &manifest.log_channel),
    ] {
        if let Some(value) = value {
            fields.push((key, quoted(value)));
        }
    }
    let lines: Vec<String> = fields.iter().map(|(key, value)| format!("  \"{key}\": {value}")).collect();
    format!("{{\n{}\n}}\n", lines.join(",\n"))
}

/// Writes `manifest` beside `image`, where [`read`] finds it, and returns where that is.
///
/// The text goes to a file of its own first and then takes the record's name, so a write that
/// stops part way leaves no half record: a present record that does not parse stops every write of
/// the image beside it.
///
/// # Errors
/// A file that could not be written or renamed, naming the path.
pub fn write(image: &Path, manifest: &Manifest) -> Result<PathBuf, String> {
    let path = path_for(image);
    let mut partial = path.clone().into_os_string();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    std::fs::write(&partial, render(manifest)).map_err(|error| format!("write {}: {error}", partial.display()))?;
    std::fs::rename(&partial, &path).map_err(|error| {
        let _ = std::fs::remove_file(&partial);
        format!("write {}: {error}", path.display())
    })?;
    Ok(path)
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
    let model = object.get("model").and_then(serde_json::Value::as_str).ok_or(
        "no `model` string. A schema 1 record states the execution model its image runs under, \
         `native` or `interpreter`.",
    )?;
    if model != NATIVE && model != INTERPRETER {
        return Err(format!("model {model:?}, and the record defines `{NATIVE}` and `{INTERPRETER}`"));
    }
    let model = model.to_owned();

    let format = string(object, "format")?;
    if format != "elf" && format != "bin" && format != LMLI {
        return Err(format!("format {format:?}, and the record defines `elf`, `bin` and `{LMLI}` only"));
    }
    if format == LMLI && model != INTERPRETER {
        return Err(format!(
            "format `{LMLI}` with model {model:?}. A `{LMLI}` image is the one Lamella's firmware runs, so its \
             model is `{INTERPRETER}`."
        ));
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
        (LMLI, Some(_)) => {
            return Err(format!(
                "format `{LMLI}` with a `base`. Lamella's firmware places the image it runs, so an address \
                 beside it is a claim nothing reads."
            ));
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
        model,
        board: string(object, "board")?,
        format,
        base,
        sha256,
        bytes,
        producer: optional(object, "producer"),
        elf: optional(object, "elf"),
        console_channel: optional(object, "console_channel"),
        log_channel: optional(object, "log_channel"),
    })
}

/// An optional string field.
fn optional(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    object.get(key).and_then(serde_json::Value::as_str).map(str::to_owned)
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
                 The Lamella RHU record describes `elf` and `bin`; an image in another format has no \
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
        attestation, check_board, check_identity, for_bin, for_elf, hex, parse, path_for, read,
        render, write, for_lmli, Manifest, INTERPRETER, NATIVE,
    };
    use std::path::Path;

    /// **A RECORD THIS TREE WRITES READS BACK AS IT WAS WRITTEN**, through the reader every consumer
    /// uses, and the bytes it describes pass the identity check a write makes.
    #[test]
    fn an_elf_record_this_tree_writes_reads_back_and_checks() {
        let shipped = b"\x7fELF and the rest of an image";
        let written = render(&for_elf("rpi-pico2", shipped, "lamella 0.2.0"));
        let manifest = parse(&written).expect("the written record parses");
        assert_eq!(manifest.board, "rpi-pico2");
        assert_eq!(manifest.format, "elf");
        assert_eq!(manifest.base, None);
        assert_eq!(manifest.bytes, shipped.len() as u64);
        assert_eq!(manifest.sha256, hex(lamella_pe::sha256::sha256(shipped)));
        assert_eq!(manifest.producer.as_deref(), Some("lamella 0.2.0"));
        assert_eq!(manifest.elf, None);
        assert!(!written.contains("\"base\""), "{written}");
        check_identity(&manifest, shipped, Some("elf")).expect("the bytes it describes");
        assert!(check_identity(&manifest, b"other bytes", Some("elf")).is_err());
    }

    /// **A `bin` RECORD CARRIES ITS BASE**, spelled as eight hex digits as the other producer spells
    /// it, and it reads back as the address it was given.
    #[test]
    fn a_bin_record_this_tree_writes_carries_its_base() {
        let shipped = [0u8; 512];
        let written = render(&for_bin("rpi-pico2", 0x1000_0000, &shipped, "lamella 0.2.0"));
        assert!(written.contains("\"base\": \"0x10000000\""), "{written}");
        let manifest = parse(&written).expect("the written record parses");
        assert_eq!(manifest.format, "bin");
        assert_eq!(manifest.base, Some(0x1000_0000));
        check_identity(&manifest, &shipped, Some("bin")).expect("the bytes it describes");
        let low = parse(&render(&for_bin("bbc-micro-bit-v2", 0, &shipped, "lamella 0.2.0"))).expect("parses");
        assert_eq!(low.base, Some(0));
    }

    /// What the record quotes is escaped, so a name holding a quote or a backslash reads back whole.
    #[test]
    fn the_written_record_escapes_what_it_quotes() {
        let producer = r#"lamella "dev" C:\tools"#;
        let manifest = parse(&render(&for_elf("rpi-pico2", b"x", producer))).expect("parses");
        assert_eq!(manifest.producer.as_deref(), Some(producer));
    }

    /// **THE RECORD IS WRITTEN WHERE IT IS READ FROM**, beside the image under the whole file name,
    /// and a second write replaces the first rather than leaving either half behind.
    #[test]
    fn write_puts_the_record_where_read_finds_it() {
        let dir = std::env::temp_dir().join(format!("lamella-manifest-write-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        let image = dir.join("app.elf");
        std::fs::write(&image, b"first").expect("the image");
        let path = write(&image, &for_elf("rpi-pico2", b"first", "lamella 0.2.0")).expect("written");
        assert_eq!(path, path_for(&image));
        let first = read(&image).expect("readable").expect("present");
        assert_eq!(first.bytes, 5);
        write(&image, &for_elf("rpi-pico2-w", b"second!", "lamella 0.2.0")).expect("written again");
        let second = read(&image).expect("readable").expect("present");
        assert_eq!((second.board.as_str(), second.bytes), ("rpi-pico2-w", 7));
        let left: Vec<_> = std::fs::read_dir(&dir).expect("listed").map(|entry| entry.expect("an entry").file_name()).collect();
        assert_eq!(left.len(), 2, "the image and its record, nothing partial: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A record for `bytes`, correct in every field, so a test can spoil exactly one.
    fn record_for(bytes: &[u8], format: &str, base: &str) -> String {
        format!(
            "{{\"schema\":1,\"kind\":\"flash-image\",\"model\":\"native\",\"board\":\"bbc-micro-bit-v2\",\
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
        assert_eq!(manifest.board, "bbc-micro-bit-v2");
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

    /// **`model` IS REQUIRED, AND IT IS ONE OF TWO WORDS**: a record without it is refused rather
    /// than read as either model, and the writer puts it third, after `kind`.
    #[test]
    fn the_model_is_required_and_written_third() {
        let without = record_for(b"image", "elf", "null").replace("\"model\":\"native\",", "");
        let why = parse(&without).unwrap_err();
        assert!(why.contains("no `model`"), "{why}");
        let other = record_for(b"image", "elf", "null").replace("\"native\"", "\"jit\"");
        assert!(parse(&other).unwrap_err().contains("model \"jit\""));
        let interpreted = record_for(b"image", "elf", "null").replace("\"native\"", "\"interpreter\"");
        assert_eq!(parse(&interpreted).expect("an interpreter's record").model, INTERPRETER);
        let rendered = render(&for_elf("rpi-pico2", b"image", "lamella 0.2.0"));
        let keys: Vec<&str> = rendered.lines().filter_map(|line| line.trim().strip_prefix('"')?.split('"').next()).collect();
        assert_eq!(&keys[..4], ["schema", "kind", "model", "board"], "{rendered}");
        assert_eq!(parse(&rendered).expect("reads back").model, NATIVE);
    }

    /// **A `.lmli` HAS ITS RECORD TOO**, Lamella's own format: no base, since the firmware places it,
    /// and the interpreter's model, which a record of that format may not deny.
    #[test]
    fn an_lmli_record_is_the_interpreters_and_carries_no_base() {
        let rendered = render(&for_lmli("rpi-pico2", b"baked", "lamella 0.2.0"));
        let manifest = parse(&rendered).expect("an lmli record reads");
        assert_eq!((manifest.format.as_str(), manifest.model.as_str(), manifest.base), ("lmli", INTERPRETER, None));
        assert!(!rendered.contains("\"base\""), "{rendered}");
        let interpreted = record_for(b"image", "lmli", "null").replace("\"native\"", "\"interpreter\"");
        assert!(parse(&interpreted).is_ok(), "{interpreted}");
        let based = record_for(b"image", "lmli", "\"0x10000000\"").replace("\"native\"", "\"interpreter\"");
        assert!(parse(&based).unwrap_err().contains("with a `base`"));
        let native = record_for(b"image", "lmli", "null");
        assert!(parse(&native).unwrap_err().contains("model \"native\""));
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

    /// THE OTHER PRODUCER'S OWN RECORD, COPIED RATHER THAN COMPOSED.
    ///
    /// Every test above builds a record from `record_for`, which is this tree's idea of the shape.
    /// **A fixture written from one side of a contract agrees with that side by construction**, and
    /// what a two-party record needs is the other party's bytes. This is the illustrative record
    /// from RHU's build contract, field for field and in its order, with only its two placeholders
    /// filled in -- so nine fields this reader does not know arrive together, an emulator board id
    /// it cannot route arrives in `board`, and `base` arrives as the padded `0x00000000` rather
    /// than the `0x0` every test above writes.
    #[test]
    fn the_other_producers_record_reads() {
        let bytes = b"image";
        let text = r#"{
  "schema": 1,
  "kind": "flash-image",
  "model": "native",
  "board": "mps2-an500",
  "format": "bin",
  "base": "0x00000000",
  "sha256": "<sha256>",
  "bytes": <bytes>,
  "producer": "ilc+rhu-link",
  "elf": "app.elf",
  "map": "app.map",
  "image_kind": "monolith",
  "layout_version": 1,
  "abi": 31,
  "debug_layout_version": 2,
  "codegen_profile": "ship",
  "runtime": "11.0.0-preview.7.26381.103",
  "runtime_tested": true
}"#
        .replace("<sha256>", &hex(lamella_pe::sha256::sha256(bytes)))
        .replace("<bytes>", &bytes.len().to_string());

        let manifest = parse(&text).expect("the other producer's record reads");
        assert_eq!(manifest.board, "mps2-an500");
        assert_eq!(manifest.base, Some(0), "the padded spelling is the same address");
        assert_eq!(manifest.producer.as_deref(), Some("ilc+rhu-link"));
        assert_eq!(manifest.elf.as_deref(), Some("app.elf"));
        assert_eq!((manifest.console_channel.as_deref(), manifest.log_channel.as_deref()), (None, None));
        assert!(check_identity(&manifest, bytes, Some("bin")).is_ok());
    }

    /// The two channel words the image was built with, under the keys its producer names them by,
    /// and a record without them still reads: the words are read where a reader needs them.
    #[test]
    fn the_channel_words_read_when_present_and_are_not_required() {
        let text = record_for(b"image", "elf", "null").replace(
            "\"schema\":1",
            "\"schema\":1,\"console_channel\":\"memory\",\"log_channel\":\"off\"",
        );
        let manifest = parse(&text).expect("a record with its channels");
        assert_eq!(manifest.console_channel.as_deref(), Some("memory"));
        assert_eq!(manifest.log_channel.as_deref(), Some("off"));
        let without = parse(&record_for(b"image", "elf", "null")).expect("a record without them");
        assert_eq!((without.console_channel, without.log_channel, without.elf), (None, None, None));
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

    /// The Lamella RHU record describes `elf` and `bin`. `flash` also takes `.hex`, `.s19` and
    /// `.uf2`, and a sidecar beside one of those cannot describe it -- so it is refused by name
    /// rather than ignored, because an unchecked claim is worse than no claim.
    #[test]
    fn a_sidecar_beside_a_format_the_record_cannot_describe_is_refused() {
        let manifest = parse(&record_for(b"image", "bin", "\"0x0\"")).unwrap();
        let why = check_identity(&manifest, b"image", Some("hex")).unwrap_err();
        assert!(why.contains("`elf` and `bin`"), "{why}");
    }

    #[test]
    fn the_wrong_board_is_refused_and_both_are_named() {
        let manifest = parse(&record_for(b"image", "elf", "null")).unwrap();
        assert!(check_board(&manifest, "bbc-micro-bit-v2").is_ok());
        let why = check_board(&manifest, "rpi-pico2").unwrap_err();
        assert!(why.contains("bbc-micro-bit-v2") && why.contains("rpi-pico2"), "{why}");
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
        assert_eq!(manifest.board, "bbc-micro-bit-v2");
        let _ = std::fs::remove_file(&sidecar);
        let _ = std::fs::remove_file(&image);
    }

    /// five shipped sentences printed columns of stray spaces before anyone looked at the output.
    #[test]
    fn every_message_a_person_reads_renders_without_stray_columns() {
        let manifest = parse(&record_for(b"image", "elf", "null")).unwrap();
        let rendered = [
            attestation(&manifest),
            check_board(&manifest, "rpi-pico2").unwrap_err(),
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
                assert!(
                    !line.contains("\x20\x20\x20\x20\x20\x20\x20\x20\x20\x20"),
                    "a run of spaces sits mid-sentence, so a continuation collapsed:\n\
                     {message}"
                );
            }
        }
    }

    /// The line a person reads before the write, which is the only place the sidecar is visible
    /// when everything agrees.
    #[test]
    fn the_attestation_names_the_board_and_the_build() {
        let manifest = parse(&record_for(b"image", "elf", "null")).unwrap();
        let line = attestation(&manifest);
        assert!(line.contains("bbc-micro-bit-v2"), "{line}");
        assert!(line.contains("lamella-aot"), "{line}");
        assert!(line.contains("5 B"), "{line}");

        let anonymous = Manifest { producer: None, ..manifest };
        assert!(!attestation(&anonymous).contains("built by"), "{}", attestation(&anonymous));
    }
}
