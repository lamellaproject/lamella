//! A flash region written beside an image: bytes a board package carries for a fixed place in the
//! board's flash, such as a radio's firmware, described by a `flash-region` record and written as a
//! span of its own, never joined to the image's span by fill.
//!
//! The record is the sidecar beside the region's one packed file, named as an image's is
//! ([`crate::manifest::path_for`]): `wifi.region.bin` is described by `wifi.region.bin.manifest.json`.
//! It is schema 1 of the family the image record belongs to, of the kind `flash-region`:
//!
//! - `board`: the board the region was packed for;
//! - `device`: the board's flash region it lies in, by the name the board's facts give it;
//! - `base`: where it starts, in bytes from the start of that flash region;
//! - `bytes` and `sha256`: the packed file's length and digest. The file is the region as written,
//!   gaps and tail included, so the digest is of every byte the region holds;
//! - `images`: each image inside it, with its `name`, `offset`, `bytes`, `sha256` and `version`;
//! - `notices`: the license files that ride beside the region's file;
//! - `producer`: what packed it, reported and never acted on.
//!
//! Fields this does not know are tolerated, by the record's contract. Nothing branches on an image's
//! `name` or `version` either: both are shown.
//!
//! A region is written as [`write_through`] writes it wherever the route allows the order: nothing when
//! the flash already holds it, and otherwise its directory erased first and programmed last, so a write
//! stopped part way leaves no directory.

use std::path::{Path, PathBuf};

use lamella_stored_settings::{PAGE, RegionFlash};

use crate::manifest::{hex, optional, parse_base, string, SCHEMA};

/// The record kind this reads.
pub const KIND: &str = "flash-region";

/// One region, ready for a route: its bytes, where they belong, and what the record says of them.
///
/// Everything here has been checked before a route sees it: the bytes are the record's length and
/// hash, every image inside them is its own hash, and the span lies inside the place the board's
/// facts give its flash region, aligned to that place's erase unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionSpan {
    /// The region file's name, for the lines a person reads.
    pub name: String,
    /// The address the region's first byte belongs at, as the route addresses the flash.
    pub base: u32,
    /// The region's bytes as written: exactly the record's length, whose SHA-256 is [`sha256`](Self::sha256).
    pub bytes: Vec<u8>,
    /// The SHA-256 of [`bytes`](Self::bytes), as the record states it.
    pub sha256: [u8; 32],
    /// How many bytes at the region's start are its directory: erased before the rest of the region
    /// and programmed after it, so a write stopped part way leaves no directory to describe images
    /// that are not whole. A whole number of the route's erase units.
    pub header: u32,
    /// The license files the record names, each found beside the region file.
    pub notices: Vec<PathBuf>,
}

/// What a write did with one region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegionOutcome {
    /// The flash already held the region's bytes, so nothing of it was erased or programmed.
    Held,
    /// The region was written and read back: `sectors` erase units erased and `bytes` bytes of them
    /// written again.
    Written {
        /// The erase units erased.
        sectors: usize,
        /// The bytes of the erase units written again.
        bytes: usize,
    },
    /// The region's `bytes` went to the board's bootloader drive in a UF2, which that route cannot
    /// read back, so nothing says they reached the flash.
    Copied {
        /// The bytes the UF2 carried for the region.
        bytes: usize,
    },
}

/// A region file read for a write: the span a route writes, and the record a person is shown it by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionFile {
    /// The region file it was read from.
    pub path: PathBuf,
    /// What a route writes.
    pub span: RegionSpan,
    /// What the region's record says of it: its images, their versions, and what packed it.
    pub record: RegionRecord,
}

/// What a write of an image and its regions did, region by region.
#[derive(Debug)]
pub struct RegionsReport {
    /// The image's report, when the write had an image.
    pub image: Option<lamella_flash_backend::Report>,
    /// Whether the flash already held the image, so it was neither erased nor programmed: only a route
    /// that reads the flash before it writes can know it.
    pub image_held: bool,
    /// What the part said it was, when the route that wrote the regions asked it before writing; `None`
    /// for a route that cannot ask it. A write with an image says it from the image's report too.
    pub part: Option<lamella_flash_backend::PartIdentity>,
    /// Each region's name and outcome, in the order the write took them.
    pub regions: Vec<(String, RegionOutcome)>,
}

/// A `flash-region` record: what the packer says about the region file beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionRecord {
    /// The board the region was packed for.
    pub board: String,
    /// The board's flash region it lies in, by the name the board's facts give it.
    pub device: String,
    /// Where it starts, in bytes from the start of `device`.
    pub base: u32,
    /// The packed file's length: the region as written.
    pub bytes: u64,
    /// The packed file's SHA-256, lowercase hex.
    pub sha256: String,
    /// The images inside the region.
    pub images: Vec<RegionImage>,
    /// The license files that ride beside the region's file, by name.
    pub notices: Vec<String>,
    /// What packed it. Report-only: nothing may branch on it.
    pub producer: Option<String>,
}

/// One image inside a region.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegionImage {
    /// What the image is, as the packer names it: shown, and never branched on.
    pub name: String,
    /// Where it starts, in bytes from the region's start.
    pub offset: u64,
    /// Its length.
    pub bytes: u64,
    /// Its SHA-256, lowercase hex.
    pub sha256: String,
    /// The human-readable version its packer bound to that digest: shown, and never branched on.
    pub version: Option<String>,
}

/// The record `text` states.
///
/// # Errors
/// Not JSON, not an object, a schema this does not read, another kind, or a field missing or of the
/// wrong type.
pub fn parse_record(text: &str) -> Result<RegionRecord, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|why| format!("not JSON -- {why}"))?;
    let object = value.as_object().ok_or("not a JSON object")?;
    let schema = object
        .get("schema")
        .and_then(serde_json::Value::as_u64)
        .ok_or("no `schema` number, so there is no way to know what the rest means")?;
    if schema != SCHEMA {
        return Err(format!("schema {schema}, and this reads schema {SCHEMA}."));
    }
    let kind = string(object, "kind")?;
    if kind != KIND {
        return Err(format!("kind {kind:?}. This is the record of a flash region, of the kind `{KIND}`."));
    }
    let base = string(object, "base").and_then(|text| parse_base(&text))?;
    let images = object
        .get("images")
        .and_then(serde_json::Value::as_array)
        .ok_or("no `images` list, so nothing says what the region holds")?
        .iter()
        .enumerate()
        .map(|(at, image)| parse_image(image).map_err(|why| format!("images[{at}]: {why}")))
        .collect::<Result<Vec<_>, _>>()?;
    let notices = match object.get("notices") {
        None => Vec::new(),
        Some(serde_json::Value::Array(files)) => files
            .iter()
            .map(|file| file.as_str().map(str::to_owned).ok_or("`notices` holds something that is not a file name"))
            .collect::<Result<Vec<_>, _>>()?,
        Some(_) => return Err(String::from("`notices` must be a list of file names")),
    };
    Ok(RegionRecord {
        board: string(object, "board")?,
        device: string(object, "device")?,
        base,
        bytes: object.get("bytes").and_then(serde_json::Value::as_u64).ok_or("no `bytes` count")?,
        sha256: digest(object)?,
        images,
        notices,
        producer: optional(object, "producer"),
    })
}

/// One `images[]` entry.
fn parse_image(value: &serde_json::Value) -> Result<RegionImage, String> {
    let object = value.as_object().ok_or("not a JSON object")?;
    let number = |key: &str| object.get(key).and_then(serde_json::Value::as_u64).ok_or(format!("no `{key}` number"));
    Ok(RegionImage {
        name: string(object, "name")?,
        offset: number("offset")?,
        bytes: number("bytes")?,
        sha256: digest(object)?,
        version: match object.get("version") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(text)) => Some(text.clone()),
            Some(_) => return Err(String::from("`version` must be a string")),
        },
    })
}

/// The `sha256` field of `object`: 64 hex digits, lowercased.
fn digest(object: &serde_json::Map<String, serde_json::Value>) -> Result<String, String> {
    let sha256 = string(object, "sha256")?.to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("`sha256` is not 64 hex digits: {sha256:?}"));
    }
    Ok(sha256)
}

/// Check `bytes`, the region file's contents, against what `record` says they are: the length, the
/// digest, and each image's place and digest inside them.
///
/// # Errors
/// The first disagreement, naming both values.
pub fn check_bytes(record: &RegionRecord, bytes: &[u8]) -> Result<(), String> {
    let length = bytes.len() as u64;
    if length != record.bytes {
        return Err(format!(
            "its record gives {} bytes and the file has {length} bytes, so the file is not the region the record describes.",
            record.bytes
        ));
    }
    let found = hex(lamella_pe::sha256::sha256(bytes));
    if found != record.sha256 {
        return Err(format!(
            "its bytes differ from the ones its record describes: the record's SHA-256 is {} and the file's is {found}.",
            record.sha256
        ));
    }
    let mut spans: Vec<(u64, u64, &str)> = Vec::new();
    for image in &record.images {
        let end = image.offset.checked_add(image.bytes).filter(|end| *end <= length).ok_or_else(|| {
            format!(
                "its image {:?} runs from {} for {} bytes, past the region's {length} bytes.",
                image.name, image.offset, image.bytes
            )
        })?;
        let start = usize::try_from(image.offset).unwrap_or(usize::MAX);
        let stop = usize::try_from(end).unwrap_or(usize::MAX);
        let found = hex(lamella_pe::sha256::sha256(&bytes[start..stop]));
        if found != image.sha256 {
            return Err(format!(
                "its image {:?} differs from the one its record describes: the record's SHA-256 is {} and the bytes' is {found}.",
                image.name, image.sha256
            ));
        }
        if let Some((_, _, other)) = spans.iter().find(|(from, to, _)| image.offset < *to && *from < end) {
            return Err(format!("its images {other:?} and {:?} overlap.", image.name));
        }
        spans.push((image.offset, end, &image.name));
    }
    Ok(())
}

/// The license files `record` names, each beside `region`, the region's file.
///
/// # Errors
/// A named file that is not there: the license rides with the images, so a region without it is not
/// written.
pub fn notices_beside(record: &RegionRecord, region: &Path) -> Result<Vec<PathBuf>, String> {
    let folder = region.parent().unwrap_or_else(|| Path::new("."));
    record
        .notices
        .iter()
        .map(|name| {
            let path = folder.join(name);
            if path.is_file() {
                Ok(path)
            } else {
                Err(format!(
                    "its record names the license file {name:?}, and there is no {} beside it. The license rides with \
                     the images, so the region is not written without it.",
                    path.display()
                ))
            }
        })
        .collect()
}

/// The address `record`'s region starts at through a route whose flash begins at `flash_base`, in a
/// flash region of the board's facts that begins at `region_base` (`None` when the facts state none,
/// which is the chip's own window onto its flash).
///
/// The record's `base` is an offset in its device, so this is the one place it becomes an address.
#[must_use]
pub fn address_of(record: &RegionRecord, flash_base: u32, region_base: Option<u32>) -> u32 {
    region_base.unwrap_or(flash_base).saturating_add(record.base)
}

/// Where `record`'s region goes on `board`, through a route whose flash begins at `flash_base`: the
/// address it starts at and the erase unit its directory is written in.
///
/// # Errors
/// A record packed for another board; a board whose facts state no `[flash_region]`; a `device`
/// other than the flash region those facts name; and a span that is not inside the stated place or
/// not aligned to its erase unit.
pub fn place(record: &RegionRecord, board: &str, flash_base: u32) -> Result<(u32, u32), String> {
    if record.board != board {
        return Err(format!("its record says it is for {}, and this write is for {board}.", record.board));
    }
    let table = lamella_catalog::load_board(board).ok_or_else(|| format!("{board} is not a board this build knows."))?;
    let Some(row) = &table.flash_region else {
        return Err(format!("{board}'s facts state no place for a flash region, so none is written to it."));
    };
    if record.device != row.memory {
        return Err(format!(
            "its record places it in {:?}, and {board}'s facts keep a flash region in {:?}.",
            record.device, row.memory
        ));
    }
    let Some(memory) = table.memory.iter().find(|memory| memory.name == row.memory && memory.kind == "flash") else {
        return Err(format!("{board}'s facts place its flash region in {:?}, which is not one of its flash regions.", row.memory));
    };
    let (start, end) = (i64::from(record.base), i64::from(record.base) + i64::try_from(record.bytes).unwrap_or(i64::MAX));
    if start < row.offset || end > row.offset + row.size {
        return Err(format!(
            "its record places it from {start:#x} to {end:#x} in {:?}, and {board}'s facts keep a flash region from {:#x} to {:#x}.",
            record.device,
            row.offset,
            row.offset + row.size
        ));
    }
    if start % row.erase_unit != 0 || end % row.erase_unit != 0 {
        return Err(format!(
            "its record places it from {start:#x} to {end:#x}, which is not whole erase units of {:#x}.",
            row.erase_unit
        ));
    }
    let region_base = u32::try_from(memory.base).ok();
    let unit = u32::try_from(row.erase_unit).map_err(|_| format!("{board}'s flash region's erase unit is too large."))?;
    Ok((address_of(record, flash_base, region_base), unit))
}

/// The region in the file `region`, for a write to `board` through a route whose flash begins at
/// `flash_base`: its record read from beside it, its bytes checked, its place and its notices found.
///
/// # Errors
/// A region file or record that cannot be read, and every refusal of [`parse_record`],
/// [`check_bytes`], [`place`] and [`notices_beside`], each naming the file.
pub fn region_span(region: &Path, board: &str, flash_base: u32) -> Result<RegionSpan, String> {
    read_region(region, board, flash_base).map(|file| file.span)
}

/// [`region_span`], with the record kept beside the span for the lines a person reads.
///
/// # Errors
/// Every refusal of [`region_span`].
pub fn read_region(region: &Path, board: &str, flash_base: u32) -> Result<RegionFile, String> {
    let named = |why: String| format!("{}: {why}", region.display());
    let sidecar = crate::manifest::path_for(region);
    let text = std::fs::read_to_string(&sidecar).map_err(|error| {
        named(format!("its record {} could not be read ({error}), and a region is written only as its record places it.", sidecar.display()))
    })?;
    let record = parse_record(&text).map_err(|why| format!("{}: {why}", sidecar.display()))?;
    let bytes = std::fs::read(region).map_err(|error| named(error.to_string()))?;
    check_bytes(&record, &bytes).map_err(named)?;
    let (base, header) = place(&record, board, flash_base).map_err(named)?;
    let notices = notices_beside(&record, region).map_err(named)?;
    let mut sha256 = [0u8; 32];
    for (at, byte) in sha256.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&record.sha256[at * 2..at * 2 + 2], 16).unwrap_or_default();
    }
    let name = region.file_name().map_or_else(|| region.display().to_string(), |name| name.to_string_lossy().into_owned());
    Ok(RegionFile { path: region.to_path_buf(), span: RegionSpan { name, base, bytes, sha256, header, notices }, record })
}

/// What a write says of `file` before it opens anything: the region and where it goes, each image in
/// it with its version, and each notice that rides beside it.
#[must_use]
pub fn describe(file: &RegionFile) -> Vec<String> {
    let span = &file.span;
    let base = u64::from(span.base);
    let last = base + (span.bytes.len() as u64).saturating_sub(1);
    let packer = file.record.producer.as_deref().map(|producer| format!(", packed by {producer}")).unwrap_or_default();
    let mut lines = vec![format!("region {}: {} B for {base:#010x}-{last:#010x}{packer}", span.name, span.bytes.len())];
    for image in &file.record.images {
        let version = image.version.as_deref().map(|version| format!(", version {version}")).unwrap_or_default();
        lines.push(format!("  {}{version}: {} B at {:#010x}", image.name, image.bytes, base + image.offset));
    }
    lines.extend(span.notices.iter().map(|notice| format!("  notice: {}", notice.display())));
    lines
}

/// What a write says of the region `name` once it is done with it.
#[must_use]
pub fn outcome_line(name: &str, outcome: &RegionOutcome) -> String {
    match outcome {
        RegionOutcome::Held => format!("region {name}: the flash already holds it, so none of it was written."),
        RegionOutcome::Written { sectors, bytes } => {
            format!("region {name}: written and verified, {sectors} sectors erased and {bytes} B written again.")
        }
        RegionOutcome::Copied { bytes } => format!(
            "region {name}: {bytes} B copied to the bootloader drive in a UF2. Not verified: this route cannot read \
             the flash back."
        ),
    }
}

/// Writes `span` through `flash`, whose offsets count from `flash_base`, the address its first byte
/// has where `span.base` is given.
///
/// Nothing is written when the flash already holds the region. Otherwise its directory, the erase
/// unit of [`RegionSpan::header`] bytes at its start, is erased first; every other unit whose bytes
/// differ is erased and written again; the directory is written last; and the whole region is read
/// back and its SHA-256 checked against the record's. A page the region leaves erased is not
/// programmed, since erasing left it so. `say` takes a line as each step starts.
///
/// # Errors
/// A region below the flash or not whole units of its directory's size, refused before the flash is
/// touched; a step the flash refuses; and a read-back whose SHA-256 is not the record's, naming both.
pub fn write_through(
    flash: &mut dyn RegionFlash,
    span: &RegionSpan,
    flash_base: u32,
    say: &mut dyn FnMut(&str),
) -> Result<RegionOutcome, String> {
    let offset = span.base.checked_sub(flash_base).ok_or_else(|| {
        format!("{} belongs at {:#010x}, below the flash's start at {flash_base:#010x}.", span.name, span.base)
    })?;
    let unit = span.header as usize;
    if unit == 0 || !unit.is_multiple_of(PAGE) || span.bytes.is_empty() || !span.bytes.len().is_multiple_of(unit) {
        return Err(format!(
            "{} is {} B, which is not whole erase units of its directory's {} B.",
            span.name,
            span.bytes.len(),
            span.header
        ));
    }
    let held = flash.read(offset, span.bytes.len())?;
    if held.len() != span.bytes.len() {
        return Err(format!("{}: the flash answered {} of its {} B.", span.name, held.len(), span.bytes.len()));
    }
    if held == span.bytes {
        return Ok(RegionOutcome::Held);
    }
    let units = span.bytes.len() / unit;
    let at = |index: usize| offset + u32::try_from(index * unit).unwrap_or(u32::MAX);
    let bytes_of = |index: usize| &span.bytes[index * unit..(index + 1) * unit];
    let differ: Vec<usize> = (1..units).filter(|&index| held[index * unit..(index + 1) * unit] != *bytes_of(index)).collect();
    say(&format!("writing region {}: {} of its {units} sectors and its directory", span.name, differ.len()));
    flash.erase(at(0), span.header)?;
    for &index in &differ {
        flash.erase(at(index), span.header)?;
    }
    for &index in differ.iter().chain(std::iter::once(&0)) {
        for (page, bytes) in bytes_of(index).chunks(PAGE).enumerate() {
            if bytes.iter().all(|&byte| byte == 0xFF) {
                continue;
            }
            let page_offset = at(index) + u32::try_from(page * PAGE).unwrap_or(u32::MAX);
            flash.program(page_offset, bytes.try_into().map_err(|_| String::from("a page is 256 bytes"))?)?;
        }
    }
    let read = flash.read(offset, span.bytes.len())?;
    let found = lamella_pe::sha256::sha256(&read);
    if found != span.sha256 {
        return Err(format!(
            "{} read back from {:#010x} with the SHA-256 {}, and its record states {}: the region did not take.",
            span.name,
            span.base,
            hex(found),
            hex(span.sha256)
        ));
    }
    let rewritten = differ.len() + 1;
    Ok(RegionOutcome::Written { sectors: rewritten, bytes: rewritten * unit })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A region of `size` bytes holding `images` at their offsets, the rest 0xFF, and its record.
    fn region(size: usize, images: &[(&str, usize, &[u8])], extra: &str) -> (Vec<u8>, String) {
        let mut bytes = vec![0xFF; size];
        let mut entries = Vec::new();
        for (name, offset, image) in images {
            bytes[*offset..*offset + image.len()].copy_from_slice(image);
            entries.push(format!(
                "{{\"name\":\"{name}\",\"offset\":{offset},\"bytes\":{},\"sha256\":\"{}\",\"version\":\"v1\"}}",
                image.len(),
                hex(lamella_pe::sha256::sha256(image))
            ));
        }
        let record = format!(
            "{{\"schema\":1,\"kind\":\"flash-region\",\"board\":\"raspberrypi-pico-2-w\",\"device\":\"flash\",\
             \"base\":\"0x3C0000\",\"bytes\":{size},\"sha256\":\"{}\",\"images\":[{}],\"notices\":[\"LICENSE.RP\"],\
             \"producer\":\"test\"{extra}}}",
            hex(lamella_pe::sha256::sha256(&bytes)),
            entries.join(",")
        );
        (bytes, record)
    }

    /// The directory Lamella RHU states for today's radio corner, byte for byte as rhu-lead's 234th
    /// gives it: format 1, three entries, 144 bytes.
    fn rhu_directory() -> Vec<u8> {
        let mut directory = vec![0x4c, 0x57, 0x46, 0x44, 0x01, 0x90, 0x00, 0x03];
        for (head, digest) in [
            ("0100000000100000be6b0300", "dda7d2527e1cc9b1ebdd107d46932643617f6e83040f24cba722cbb0ded8c4a8"),
            ("0200000000a00300d8030000", "e712b3d218e8b1e2747b092e03b8b0afcb8c8c8e355d2a4a0d47b493800f3f89"),
            ("0300000000b00300e5020000", "0517e0a609291e10c264d7d470acf3d823c23a9e7f7d5b12816a6d45b5cf1f71"),
        ] {
            let text = format!("{head}{digest}");
            directory.extend((0..text.len() / 2).map(|at| u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).unwrap()));
        }
        directory.extend([0x24, 0x9c, 0xd9, 0x38]);
        directory
    }

    /// Today's corner as RHU packs it: the directory at 0, the firmware image at 0x1000, the
    /// regulatory blob at 0x3A000 and the settings image at 0x3B000, each of its stated length, 0xFF
    /// elsewhere, 60 sectors. The three images are stand-ins of their lengths; the directory is real.
    fn corner() -> (Vec<u8>, String) {
        let directory = rhu_directory();
        let (firmware, regulatory, settings) = (vec![0x5A; 224_190], vec![0x3C; 984], vec![0x7E; 741]);
        region(
            0x3_C000,
            &[("directory", 0, &directory), ("firmware", 0x1000, &firmware), ("regulatory", 0x3_A000, &regulatory), ("settings", 0x3_B000, &settings)],
            "",
        )
    }

    #[test]
    fn the_fixtures_directory_is_rhus_byte_for_byte() {
        let directory = rhu_directory();
        assert_eq!(directory.len(), 144);
        assert_eq!(hex(lamella_pe::sha256::sha256(&directory)), "7df597c60f85a18144dcb158bc328f5bccc27a6b0a1e6f5f306c417211e25465");
    }

    #[test]
    fn a_region_record_reads_and_its_bytes_check() {
        let (bytes, text) = corner();
        let record = parse_record(&text).expect("the record");
        assert_eq!((record.board.as_str(), record.device.as_str(), record.base, record.bytes), ("raspberrypi-pico-2-w", "flash", 0x3C_0000, 0x3_C000));
        assert_eq!(record.images.iter().map(|image| image.name.as_str()).collect::<Vec<_>>(), ["directory", "firmware", "regulatory", "settings"]);
        assert_eq!((record.notices.as_slice(), record.producer.as_deref()), (&[String::from("LICENSE.RP")][..], Some("test")));
        check_bytes(&record, &bytes).expect("the bytes are the record's");
    }

    /// The record's contract: a reader takes the fields it knows and ignores the rest.
    #[test]
    fn unknown_fields_are_tolerated() {
        let (bytes, text) = region(0x1000, &[("firmware", 0, &[1, 2, 3])], ",\"carrier\":\"region\"");
        check_bytes(&parse_record(&text).expect("a later field is tolerated"), &bytes).expect("and the bytes check");
    }

    #[test]
    fn a_record_that_is_not_a_region_record_is_refused() {
        let (_, text) = corner();
        let cases = [
            (text.replace("\"schema\":1", "\"schema\":2"), "schema 2"),
            (text.replace("\"kind\":\"flash-region\"", "\"kind\":\"flash-image\""), "of the kind `flash-region`"),
            (text.replace("\"device\":\"flash\",", ""), "no `device` string"),
            (text.replace("\"base\":\"0x3C0000\",", ""), "no `base` string"),
            (text.replace("\"base\":\"0x3C0000\"", "\"base\":\"far\""), "is not a hex address"),
            (text.replace("\"notices\":[\"LICENSE.RP\"]", "\"notices\":\"LICENSE.RP\""), "a list of file names"),
            (text.replace("\"version\":\"v1\"", "\"version\":1"), "images[0]"),
        ];
        for (text, expected) in cases {
            let refusal = parse_record(&text).expect_err(expected);
            assert!(refusal.contains(expected), "{expected:?} in {refusal:?}");
        }
        assert!(parse_record(&text.replace("\"version\":\"v1\"", "\"x\":0")).is_ok(), "a version is not required");
    }

    #[test]
    fn bytes_the_record_does_not_describe_are_refused_naming_both_values() {
        let (bytes, text) = corner();
        let record = parse_record(&text).unwrap();
        let mut changed = bytes.clone();
        changed[0x3_BFFF] = 0;
        let cases = [
            (bytes[..0x1000].to_vec(), "gives 245760 bytes and the file has 4096 bytes"),
            (changed, "differ from the ones its record describes"),
        ];
        for (bytes, expected) in cases {
            let refusal = check_bytes(&record, &bytes).expect_err(expected);
            assert!(refusal.contains(expected), "{expected:?} in {refusal:?}");
        }
        let mut wrong = record.clone();
        wrong.images[1].sha256 = "0".repeat(64);
        assert!(check_bytes(&wrong, &bytes).unwrap_err().contains("its image \"firmware\" differs"));
        let mut outside = record.clone();
        outside.images[1].offset = 0x3_BF00;
        assert!(check_bytes(&outside, &bytes).unwrap_err().contains("past the region's 245760 bytes"));
        let mut over = record;
        over.images[1] = RegionImage { name: String::from("again"), ..over.images[0].clone() };
        assert!(check_bytes(&over, &bytes).unwrap_err().contains("overlap"));
    }

    #[test]
    fn a_region_is_placed_only_where_the_boards_facts_keep_one() {
        let (_, text) = corner();
        let record = parse_record(&text).unwrap();
        assert_eq!(place(&record, "raspberrypi-pico-2-w", 0x1000_0000), Ok((0x103C_0000, 0x1000)));
        let refused = |record: &RegionRecord, board: &str| place(record, board, 0x1000_0000).unwrap_err();
        assert!(refused(&record, "pimoroni-pico-plus-2-w").contains("is for raspberrypi-pico-2-w"));
        let other = RegionRecord { board: String::from("raspberrypi-pico-2"), ..record.clone() };
        assert!(refused(&other, "raspberrypi-pico-2").contains("state no place for a flash region"));
        let device = RegionRecord { device: String::from("qspi"), ..record.clone() };
        assert!(refused(&device, "raspberrypi-pico-2-w").contains("keep a flash region in \"flash\""));
        let low = RegionRecord { base: 0x3B_F000, ..record.clone() };
        assert!(refused(&low, "raspberrypi-pico-2-w").contains("keep a flash region from 0x3c0000 to 0x3fe000"));
        let over = RegionRecord { bytes: 0x3_F000, ..record.clone() };
        assert!(refused(&over, "raspberrypi-pico-2-w").contains("to 0x3fe000"));
        let ragged = RegionRecord { bytes: 0x3_D800, ..record };
        assert!(refused(&ragged, "raspberrypi-pico-2-w").contains("not whole erase units of 0x1000"));
    }

    #[test]
    fn a_region_file_becomes_a_span_only_with_its_record_and_its_notices_beside_it() {
        let folder = std::env::temp_dir().join(format!("lamella-region-span-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("wifi.region.bin");
        let (bytes, text) = corner();
        std::fs::write(&file, &bytes).unwrap();
        let without = region_span(&file, "raspberrypi-pico-2-w", 0x1000_0000).unwrap_err();
        assert!(without.contains("wifi.region.bin.manifest.json could not be read"), "{without}");
        std::fs::write(crate::manifest::path_for(&file), &text).unwrap();
        let unlicensed = region_span(&file, "raspberrypi-pico-2-w", 0x1000_0000).unwrap_err();
        assert!(unlicensed.contains("names the license file \"LICENSE.RP\""), "{unlicensed}");
        std::fs::write(folder.join("LICENSE.RP"), "license").unwrap();
        let span = region_span(&file, "raspberrypi-pico-2-w", 0x1000_0000).expect("the span");
        assert_eq!((span.name.as_str(), span.base, span.header, span.bytes.len()), ("wifi.region.bin", 0x103C_0000, 0x1000, 0x3_C000));
        assert_eq!(hex(span.sha256), parse_record(&text).unwrap().sha256);
        assert_eq!(span.notices, [folder.join("LICENSE.RP")]);
        let read = read_region(&file, "raspberrypi-pico-2-w", 0x1000_0000).expect("the region with its record");
        assert_eq!((read.path, read.span, read.record), (file.clone(), span, parse_record(&text).unwrap()));
        let _ = std::fs::remove_dir_all(&folder);
    }

    /// Today's corner as a span on a Pico 2 W, written at 0x103C0000 through a route whose flash
    /// begins at 0x10000000.
    fn corner_span() -> (RegionSpan, RegionRecord) {
        let (bytes, text) = corner();
        let record = parse_record(&text).unwrap();
        let mut sha256 = [0u8; 32];
        sha256.copy_from_slice(&lamella_pe::sha256::sha256(&bytes));
        let notices = vec![PathBuf::from("LICENSE.RP")];
        (RegionSpan { name: String::from("wifi.region.bin"), base: 0x103C_0000, bytes, sha256, header: 0x1000, notices }, record)
    }

    /// Where the corner lies in a Pico 2 W's flash, in bytes from its start.
    const CORNER: usize = 0x3C_0000;

    /// A Pico 2 W's 4 MB of flash, which logs every step and can be told to refuse its `n`th program
    /// or to take none at all.
    struct Flash {
        bytes: Vec<u8>,
        log: Vec<String>,
        programs: usize,
        refuse_program: Option<usize>,
        takes_nothing: bool,
    }

    impl Flash {
        fn erased() -> Self {
            Flash { bytes: vec![0xFF; 0x40_0000], log: Vec::new(), programs: 0, refuse_program: None, takes_nothing: false }
        }

        /// The flash holding `region` at the corner.
        fn holding(region: &[u8]) -> Self {
            let mut flash = Flash::erased();
            flash.bytes[CORNER..CORNER + region.len()].copy_from_slice(region);
            flash
        }

        fn steps(&self, kind: &str) -> Vec<&str> {
            self.log.iter().filter(|line| line.starts_with(kind)).map(String::as_str).collect()
        }
    }

    impl RegionFlash for Flash {
        fn read(&mut self, offset: u32, len: usize) -> Result<Vec<u8>, String> {
            self.log.push(format!("read {offset:#x} {len:#x}"));
            Ok(self.bytes[offset as usize..offset as usize + len].to_vec())
        }

        fn erase(&mut self, offset: u32, len: u32) -> Result<(), String> {
            self.log.push(format!("erase {offset:#x} {len:#x}"));
            if !self.takes_nothing {
                self.bytes[offset as usize..(offset + len) as usize].fill(0xFF);
            }
            Ok(())
        }

        fn program(&mut self, offset: u32, page: &[u8; PAGE]) -> Result<(), String> {
            self.programs += 1;
            if self.refuse_program == Some(self.programs) {
                return Err(String::from("the flash refused the page"));
            }
            self.log.push(format!("program {offset:#x}"));
            if !self.takes_nothing {
                for (cell, byte) in self.bytes[offset as usize..offset as usize + PAGE].iter_mut().zip(page) {
                    *cell &= byte;
                }
            }
            Ok(())
        }
    }

    /// **A REGION THE FLASH ALREADY HOLDS IS HELD:** one read of it, and nothing erased or programmed.
    #[test]
    fn a_region_the_flash_holds_is_held_after_one_read() {
        let (span, _) = corner_span();
        let mut flash = Flash::holding(&span.bytes);
        assert_eq!(write_through(&mut flash, &span, 0x1000_0000, &mut |_| {}), Ok(RegionOutcome::Held));
        assert_eq!(flash.log, ["read 0x3c0000 0x3c000"]);
    }

    /// **ONLY THE UNITS THAT DIFFER ARE WRITTEN, AND THE DIRECTORY GOES FIRST AND LAST:** with only the
    /// settings image changed, the directory's unit is erased before the settings image's and written
    /// after it, nothing else is erased, and the region reads back as its record's.
    #[test]
    fn a_region_is_written_with_its_directory_erased_first_and_written_last() {
        let (span, _) = corner_span();
        let mut older = span.bytes.clone();
        older[0x3_B000] ^= 0xFF;
        let mut flash = Flash::holding(&older);
        let outcome = write_through(&mut flash, &span, 0x1000_0000, &mut |_| {});
        assert_eq!(outcome, Ok(RegionOutcome::Written { sectors: 2, bytes: 0x2000 }));
        assert_eq!(flash.steps("erase"), ["erase 0x3c0000 0x1000", "erase 0x3fb000 0x1000"], "{:?}", flash.log);
        let programs = flash.steps("program");
        assert_eq!(programs.first(), Some(&"program 0x3fb000"), "the settings image first: {programs:?}");
        assert_eq!(programs.last(), Some(&"program 0x3c0000"), "the directory last: {programs:?}");
        assert_eq!(flash.log.last().map(String::as_str), Some("read 0x3c0000 0x3c000"), "then read back whole");
        assert_eq!(&flash.bytes[CORNER..CORNER + span.bytes.len()], &span.bytes[..]);
    }

    /// **A WRITE STOPPED PART WAY LEAVES NO DIRECTORY**, so no directory describes images that are not
    /// whole: the directory is erased first and written only once the rest is.
    #[test]
    fn a_write_stopped_part_way_leaves_no_directory() {
        let (span, _) = corner_span();
        let mut flash = Flash::erased();
        flash.bytes[CORNER..CORNER + 0x1000].copy_from_slice(&span.bytes[..0x1000]);
        flash.refuse_program = Some(3);
        let refusal = write_through(&mut flash, &span, 0x1000_0000, &mut |_| {}).expect_err("the third page is refused");
        assert!(refusal.contains("refused the page"), "{refusal}");
        assert!(flash.bytes[CORNER..CORNER + 0x1000].iter().all(|&byte| byte == 0xFF), "the directory is erased");
    }

    /// **A REGION THAT DOES NOT READ BACK AS ITS RECORD's IS REFUSED**, naming both digests.
    #[test]
    fn a_region_that_does_not_read_back_as_its_records_is_refused_naming_both_digests() {
        let (span, _) = corner_span();
        let mut flash = Flash::erased();
        flash.takes_nothing = true;
        let refusal = write_through(&mut flash, &span, 0x1000_0000, &mut |_| {}).expect_err("nothing took");
        assert!(refusal.contains("did not take") && refusal.contains(&hex(span.sha256)), "{refusal}");
        assert!(refusal.contains(&hex(lamella_pe::sha256::sha256(&vec![0xFF; span.bytes.len()]))), "{refusal}");
    }

    /// A page the region leaves erased is not programmed: the regulatory blob's 984 B take four of
    /// its unit's sixteen pages.
    #[test]
    fn a_page_the_region_leaves_erased_is_not_programmed() {
        let (span, _) = corner_span();
        let mut older = span.bytes.clone();
        older[0x3_A000] ^= 0xFF;
        let mut flash = Flash::holding(&older);
        write_through(&mut flash, &span, 0x1000_0000, &mut |_| {}).expect("written");
        let regulatory: Vec<&str> = flash.steps("program").into_iter().filter(|line| line.starts_with("program 0x3fa")).collect();
        assert_eq!(regulatory, ["program 0x3fa000", "program 0x3fa100", "program 0x3fa200", "program 0x3fa300"]);
    }

    /// A region below the flash, or not whole units of its directory's size, is refused before the
    /// flash is touched.
    #[test]
    fn a_span_that_cannot_be_written_whole_is_refused_before_the_flash_is_touched() {
        let (span, _) = corner_span();
        let mut flash = Flash::erased();
        let low = RegionSpan { base: 0x0FFF_F000, ..span.clone() };
        assert!(write_through(&mut flash, &low, 0x1000_0000, &mut |_| {}).unwrap_err().contains("below the flash's start"));
        let ragged = RegionSpan { header: 0x1100, ..span };
        assert!(write_through(&mut flash, &ragged, 0x1000_0000, &mut |_| {}).unwrap_err().contains("not whole erase units"));
        assert!(flash.log.is_empty(), "{:?}", flash.log);
    }

    /// **WHAT A WRITE SAYS OF A REGION BEFORE IT OPENS ANYTHING:** where it goes, what packed it, each
    /// image with its version, and each notice; and once it is done, held, written or copied.
    #[test]
    fn a_region_is_described_with_its_images_their_versions_and_its_notices() {
        let (span, record) = corner_span();
        let lines = describe(&RegionFile { path: PathBuf::from("wifi.region.bin"), span, record });
        assert_eq!(
            lines,
            [
                "region wifi.region.bin: 245760 B for 0x103c0000-0x103fbfff, packed by test",
                "  directory, version v1: 144 B at 0x103c0000",
                "  firmware, version v1: 224190 B at 0x103c1000",
                "  regulatory, version v1: 984 B at 0x103fa000",
                "  settings, version v1: 741 B at 0x103fb000",
                "  notice: LICENSE.RP",
            ]
        );
        assert!(outcome_line("w", &RegionOutcome::Held).contains("already holds it"));
        assert!(outcome_line("w", &RegionOutcome::Written { sectors: 2, bytes: 8192 }).contains("2 sectors erased and 8192 B"));
        assert!(outcome_line("w", &RegionOutcome::Copied { bytes: 9 }).contains("Not verified"));
    }
}
