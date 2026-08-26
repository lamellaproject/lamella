//! Reading a PREBUILT image so it can be written to a board.

use std::path::Path;

/// A prebuilt image: flat bytes and the address they belong at.
#[derive(Debug)]
pub struct Artifact {
    /// The image, contiguous from [`base`](Self::base).
    pub bytes: Vec<u8>,
    /// The address the first byte belongs at, as the file stated it. A raw `.bin` states nothing,
    /// so it reads as 0 and the caller's own base applies.
    /// The address the artifact SAYS it belongs at, where its format can say.
    ///
    /// **`None` IS NOT ZERO AND THE DIFFERENCE DECIDES A REFUSAL.** A flat `.bin` carries no
    /// address at all -- it is bytes, and where they go is the caller's to know. `.hex`, `.s19` and
    /// `.elf` each state one, and a stated address that disagrees with the board is worth stopping
    /// for. Recording the absence as `0` made a flat binary look like a claim of address zero,
    /// which is correct for a part that boots from 0 and refuses every image on a part that does
    /// not.
    pub base: Option<u32>,
    /// The format, for the line that reports what was written.
    pub format: &'static str,
}

/// What KIND of thing a file is, which is what decides the verb that takes it.
///
/// **THERE ARE THREE KINDS AND THERE HAVE TO BE, BECAUSE TWO PRODUCED A LOOP.** Classifying files
/// as merely "source or not" made `deploy` refuse a `.lmli` toward `flash` and `flash` refuse it
/// back toward `deploy` -- a circular refusal, with no way forward for anybody holding one. The
/// missing distinction is between an image a CHIP takes and a payload a running FIRMWARE takes:
/// they are both prebuilt, and they go to different verbs over different transports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A program to compile: `run`, `build`, or `deploy` from source.
    Source,
    /// Bytes a chip takes, written by a probe: `flash`.
    ChipImage,
    /// A payload the firmware already on a board loads: `deploy --target`.
    WirePayload,
}

/// The extension `path` carries, lowercased, for a caller checking it against a format it requires.
#[must_use]
pub fn classify_format(path: &Path) -> Option<String> {
    extension_of(path)
}

/// `path`'s extension, lowercased, which is the only form anything here compares.
///
/// **AN EXTENSION IS NOT CASE-SENSITIVE WHERE THESE FILES COME FROM.** A vendor ships `FIRMWARE.BIN`
/// and a release archive ships `IMAGE.HEX`; a Windows filesystem does not distinguish them from the
/// lowercase spellings, so a reader who typed the name they can see gets a file that exists. Matched
/// literally, `VENDOR.BIN` fell through every image arm and read as SOURCE -- and the refusal then
/// offered `lamella deploy VENDOR.BIN`, which hands a binary to a C# compiler.
///
fn extension_of(path: &Path) -> Option<String> {
    path.extension().and_then(|extension| extension.to_str()).map(str::to_ascii_lowercase)
}

/// The kind of thing `path` is.
///
/// Decided by extension, the same way `run` and `build` decide a language. There is no sniffing of
/// contents: a wrong guess here writes the wrong bytes to hardware, and an extension is something
/// the person who named the file chose deliberately.
#[must_use]
pub fn classify(path: &Path) -> Kind {
    match extension_of(path).as_deref() {
        Some("bin" | "hex" | "s19" | "srec" | "uf2" | "elf") => Kind::ChipImage,
        Some("lmli" | "lpyc") => Kind::WirePayload,
        _ => Kind::Source,
    }
}

/// Read `path` as a prebuilt image.
///
/// # Errors
/// A file that cannot be read, a format this cannot resolve to a flat span, or a malformed one.
pub fn read(path: &Path) -> Result<Artifact, String> {
    match extension_of(path).as_deref() {
        Some("bin") => {
            let bytes = read_bytes(path)?;
            Ok(Artifact { bytes, base: None, format: "raw binary" })
        }
        Some("hex") => {
            let bytes = read_bytes(path)?;
            parse_intel_hex(&bytes).map_err(|why| format!("{}: {why}", path.display()))
        }
        Some("s19" | "srec") => {
            let bytes = read_bytes(path)?;
            parse_srecord(&bytes).map_err(|why| format!("{}: {why}", path.display()))
        }
        Some("elf") => {
            let bytes = read_bytes(path)?;
            let image = lamella_elf::flat_image(&bytes).map_err(|why| {
                let because = match why {
                    lamella_elf::ElfError::NotExecutableElf32 => {
                        "it is not a linked ELF32 executable -- most often a relocatable object \
                         that has not been linked yet"
                    }
                    lamella_elf::ElfError::NoLoadableContent => {
                        "it declares no loadable bytes, so writing it would flash nothing"
                    }
                    lamella_elf::ElfError::Truncated => "it is truncated",
                    _ => "it is not an ELF this can read",
                };
                format!("{}: {because}.", path.display())
            })?;
            Ok(Artifact { bytes: image.bytes, base: Some(image.base), format: "ELF" })
        }
        Some("uf2") => Err(format!(
            "{}: a UF2 is written by copying it to a bootloader volume, and this board is written \
             over a probe.\nBuild the image in a format a probe takes: `--format hex` or `bin`.",
            path.display()
        )),
        _ => Err(format!("{}: not a format this can write", path.display())),
    }
}

/// The file's bytes, or a message naming it.
fn read_bytes(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))
}

/// A format an image can be WRITTEN in.
///
/// **THESE ARE THE FORMATS OTHER PEOPLE'S TOOLS ACCEPT**, which is the whole reason to emit
/// anything but raw bytes: a bootloader, a production programmer, or a vendor utility takes a text
/// record format and not a `.bin`, and a project that cannot hand one over cannot be part of
/// somebody's existing flow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// Flat bytes, no addresses.
    Bin,
    /// Intel HEX -- the micro:bit, Nordic and Arm-toolchain convention.
    IntelHex,
    /// Motorola S-records (S19) -- the NXP, STMicro and automotive convention.
    SRecord,
    /// UF2 -- what a mass-storage bootloader takes by having a file COPIED to it. Carries the
    /// target chip's family id, so a bootloader refuses an image built for another part instead of
    /// running it.
    Uf2 {
        /// The chip family the receiving bootloader will check this against.
        family: u32,
    },
}

impl Format {
    /// The format `name` selects, or the list of names that work.
    ///
    /// # Errors
    /// A name no format claims. The message lists every one, because the reader is reading it
    /// precisely because they guessed.
    pub fn parse(name: &str) -> Result<Format, String> {
        match name {
            "bin" => Ok(Format::Bin),
            "hex" | "ihex" => Ok(Format::IntelHex),
            "s19" | "srec" => Ok(Format::SRecord),
            "uf2" => Ok(Format::Uf2 { family: 0 }),
            other => Err(format!(
                "{other:?} is not a format this writes. Try: bin, hex (Intel HEX), s19 (Motorola \
                 S-records), uf2 (a bootloader volume)."
            )),
        }
    }

    /// This format with `family` filled in, where it carries one.
    #[must_use]
    pub fn for_family(self, family: u32) -> Format {
        match self {
            Format::Uf2 { .. } => Format::Uf2 { family },
            other => other,
        }
    }

    /// The extension a file of this format conventionally carries.
    #[must_use]
    pub fn extension(self) -> &'static str {
        match self {
            Format::Bin => "bin",
            Format::IntelHex => "hex",
            Format::SRecord => "s19",
            Format::Uf2 { .. } => "uf2",
        }
    }

    /// What this format is, for the line reporting what was written.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Format::Bin => "raw binary",
            Format::IntelHex => "Intel HEX",
            Format::SRecord => "Motorola S-records",
            Format::Uf2 { .. } => "UF2",
        }
    }

    /// Render `image`, which belongs at `base`, in this format.
    #[must_use]
    pub fn render(self, image: &[u8], base: u32) -> Vec<u8> {
        match self {
            Format::Bin => image.to_vec(),
            Format::IntelHex => write_intel_hex(image, base).into_bytes(),
            Format::SRecord => write_srecord(image, base).into_bytes(),
            Format::Uf2 { family } => write_uf2(image, base, family),
        }
    }
}

/// One UF2 block. Fixed by the format: 512 bytes on the wire, 256 of them payload.
const UF2_BLOCK: usize = 512;
/// The payload each UF2 block carries.
const UF2_PAYLOAD: usize = 256;

/// Render `image` as UF2, to be COPIED to a bootloader volume.
///
/// **EVERY BLOCK CARRIES THE WHOLE FILE'S SHAPE, AND THAT IS THE FORMAT'S POINT.** A mass-storage
/// write arrives out of order and in pieces, so each block states its own address, its index, and
/// how many blocks there are in total -- the bootloader reassembles without needing them in
/// sequence and knows when it has them all. It is the one image format designed to survive being
/// delivered by a filesystem.
///
/// The family id rides every block too, and a bootloader checks it: an image built for another
/// chip is REFUSED rather than run, which is the whole reason this carries one.
#[must_use]
pub fn write_uf2(image: &[u8], base: u32, family: u32) -> Vec<u8> {
    const MAGIC_START0: u32 = 0x0A32_4655;
    const MAGIC_START1: u32 = 0x9E5D_5157;
    const MAGIC_END: u32 = 0x0AB1_6F30;
    const FLAG_FAMILY_ID: u32 = 0x0000_2000;

    let blocks = image.len().div_ceil(UF2_PAYLOAD);
    let mut out = Vec::with_capacity(blocks * UF2_BLOCK);
    for (index, chunk) in image.chunks(UF2_PAYLOAD).enumerate() {
        let header = [
            MAGIC_START0,
            MAGIC_START1,
            FLAG_FAMILY_ID,
            base + (index * UF2_PAYLOAD) as u32,
            u32::try_from(chunk.len()).unwrap_or(0),
            u32::try_from(index).unwrap_or(0),
            u32::try_from(blocks).unwrap_or(0),
            family,
        ];
        for value in header {
            out.extend_from_slice(&value.to_le_bytes());
        }
        out.extend_from_slice(chunk);
        out.resize(out.len() + (476 - chunk.len()), 0);
        out.extend_from_slice(&MAGIC_END.to_le_bytes());
    }
    out
}

/// The data bytes per record. 16 is what every toolchain emits and what every reader has been fed
/// for decades; a longer record is legal and less widely exercised.
const RECORD_BYTES: usize = 16;

/// Render `image` as Intel HEX, starting at `base`.
///
/// Emits an extended-linear-address record whenever the high half of the address changes, which is
/// what makes an image above 64 KB land where it belongs -- data records carry only the low 16
/// bits, so without one an image at `0x0800_0000` would be read back at the bottom of memory.
#[must_use]
pub fn write_intel_hex(image: &[u8], base: u32) -> String {
    let mut out = String::new();
    let mut upper = 0u16;
    for (index, chunk) in image.chunks(RECORD_BYTES).enumerate() {
        let address = base + (index * RECORD_BYTES) as u32;
        let high = (address >> 16) as u16;
        if high != upper {
            let bytes = [0x02, 0x00, 0x00, 0x04, (high >> 8) as u8, high as u8];
            out.push_str(&record(&bytes));
            upper = high;
        }
        let mut bytes = Vec::with_capacity(chunk.len() + 4);
        bytes.push(u8::try_from(chunk.len()).unwrap_or(u8::MAX));
        bytes.push((address >> 8) as u8);
        bytes.push(address as u8);
        bytes.push(0x00);
        bytes.extend_from_slice(chunk);
        out.push_str(&record(&bytes));
    }
    out.push_str(":00000001FF\n");
    out
}

/// One Intel HEX line: `:`, the bytes in hex, then the two's-complement checksum of them.
fn record(bytes: &[u8]) -> String {
    let mut line = String::from(":");
    for byte in bytes {
        line.push_str(&format!("{byte:02X}"));
    }
    let sum = bytes.iter().fold(0u8, |total, byte| total.wrapping_add(*byte));
    line.push_str(&format!("{:02X}\n", sum.wrapping_neg()));
    line
}

/// Render `image` as Motorola S-records, starting at `base`.
///
/// **THE ADDRESS WIDTH IS CHOSEN FROM THE HIGHEST ADDRESS AND USED FOR EVERY RECORD.** The format
/// allows 16-, 24- and 32-bit data records (S1/S2/S3) with a matching terminator (S9/S8/S7), and a
/// file that changes width partway through is legal and reliably confuses readers. One width for
/// the whole file costs a few bytes and is what every emitter does.
#[must_use]
pub fn write_srecord(image: &[u8], base: u32) -> String {
    let highest = base + u32::try_from(image.len().saturating_sub(1)).unwrap_or(0);
    let (width, data_type, end_type) = if highest <= 0xFFFF {
        (2usize, '1', '9')
    } else if highest <= 0x00FF_FFFF {
        (3, '2', '8')
    } else {
        (4, '3', '7')
    };
    let mut out = String::new();
    for (index, chunk) in image.chunks(RECORD_BYTES).enumerate() {
        let address = base + (index * RECORD_BYTES) as u32;
        out.push_str(&srecord(data_type, address, width, chunk));
    }
    out.push_str(&srecord(end_type, base, width, &[]));
    out
}

/// One S-record: `S`, the type, a byte count, the address, the data, and the one's-complement
/// checksum of everything after the type.
fn srecord(kind: char, address: u32, width: usize, data: &[u8]) -> String {
    let mut bytes = Vec::with_capacity(width + data.len() + 1);
    bytes.push(u8::try_from(width + data.len() + 1).unwrap_or(u8::MAX));
    for shift in (0..width).rev() {
        bytes.push((address >> (shift * 8)) as u8);
    }
    bytes.extend_from_slice(data);
    let sum = bytes.iter().fold(0u8, |total, byte| total.wrapping_add(*byte));
    let mut line = format!("S{kind}");
    for byte in &bytes {
        line.push_str(&format!("{byte:02X}"));
    }
    line.push_str(&format!("{:02X}\n", !sum));
    line
}

/// Parse Intel HEX into one contiguous span.
///
/// **IT REFUSES A GAP RATHER THAN FILLING ONE.** The format can describe several disjoint regions,
/// and a flash writer that takes a base and a length cannot express that -- so padding the hole
/// would write bytes the file never contained, at addresses it never mentioned, into a part of
/// flash something else may own. A file this cannot represent flatly is named as such.
fn parse_intel_hex(bytes: &[u8]) -> Result<Artifact, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "not text, so not Intel HEX".to_owned())?;
    let mut upper: u32 = 0;
    let mut base: Option<u32> = None;
    let mut image: Vec<u8> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record = Record::parse(line)
            .map_err(|why| format!("line {}: {why}", number + 1))?;
        match record.kind {
            0x00 => {
                let address = upper + u32::from(record.address);
                match base {
                    None => base = Some(address),
                    Some(start) => {
                        let expected = start + u32::try_from(image.len()).unwrap_or(u32::MAX);
                        if address != expected {
                            return Err(format!(
                                "line {}: the data jumps from {expected:#010x} to {address:#010x}. \
                                 This file describes more than one region, which cannot be written \
                                 as one flat image.",
                                number + 1
                            ));
                        }
                    }
                }
                image.extend_from_slice(&record.data);
            }
            0x01 => break,
            0x04 => {
                let [high, low] = record.data[..] else {
                    return Err(format!("line {}: an extended address wants two bytes", number + 1));
                };
                upper = (u32::from(high) << 24) | (u32::from(low) << 16);
            }
            0x02 => {
                let [high, low] = record.data[..] else {
                    return Err(format!("line {}: a segment address wants two bytes", number + 1));
                };
                upper = ((u32::from(high) << 8) | u32::from(low)) << 4;
            }
            0x03 | 0x05 => {}
            other => return Err(format!("line {}: record type {other:#04x} is not one this reads", number + 1)),
        }
    }
    let Some(base) = base else {
        return Err("no data records, so there is nothing to write".to_owned());
    };
    Ok(Artifact { bytes: image, base: Some(base), format: "Intel HEX" })
}

/// Read Motorola S-records into one flat image, the inverse of [`write_srecord`].
///
/// **THE ADDRESS WIDTH IS CARRIED BY THE RECORD TYPE**, which is the whole reason there are three
/// data types: `S1` states a 16-bit address, `S2` a 24-bit one and `S3` a 32-bit one, and the
/// writer picks the narrowest that reaches the highest byte. A reader that assumed one width would
/// work on small images and place a large one at a wrong address, which a board reports as nothing
/// at all.
fn parse_srecord(bytes: &[u8]) -> Result<Artifact, String> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| "not text, so not S-records".to_owned())?;
    let mut base: Option<u32> = None;
    let mut image: Vec<u8> = Vec::new();
    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let record = SRecord::parse(line).map_err(|why| format!("line {}: {why}", number + 1))?;
        match record.kind {
            '0' | '5' | '6' => {}
            '1' | '2' | '3' => match base {
                None => {
                    base = Some(record.address);
                    image.extend_from_slice(&record.data);
                }
                Some(start) => {
                    let expected = start + u32::try_from(image.len()).unwrap_or(u32::MAX);
                    if record.address != expected {
                        return Err(format!(
                            "line {}: the data jumps from {expected:#010x} to {:#010x}. This file \
                             describes more than one region, which cannot be written as one flat \
                             image.",
                            number + 1,
                            record.address
                        ));
                    }
                    image.extend_from_slice(&record.data);
                }
            },
            '7' | '8' | '9' => break,
            other => {
                return Err(format!(
                    "line {}: record type S{other} is not one this reads",
                    number + 1
                ));
            }
        }
    }
    let Some(base) = base else {
        return Err("no data records, so there is nothing to write".to_owned());
    };
    Ok(Artifact { bytes: image, base: Some(base), format: "Motorola S-records" })
}

/// One S-record: `S`, the type, a byte count, the address, the data, and the checksum.
struct SRecord {
    kind: char,
    address: u32,
    data: Vec<u8>,
}

impl SRecord {
    /// Parse `S<t><CC><address><data><checksum>`, checking the count and the checksum.
    ///
    /// **THE CHECKSUM IS VERIFIED RATHER THAN SKIPPED**, for the same reason the Intel HEX reader
    /// verifies its own: these bytes are going into flash on real hardware, and a truncated
    /// download stays well-formed right up to where it stops.
    fn parse(line: &str) -> Result<Self, String> {
        let rest = line.strip_prefix('S').ok_or("an S-record begins with 'S'")?;
        let mut chars = rest.chars();
        let kind = chars.next().ok_or("a record needs a type digit")?;
        let body = chars.as_str();
        if body.len() % 2 != 0 {
            return Err("a record is an even number of hex digits after the type".to_owned());
        }
        let mut raw = Vec::with_capacity(body.len() / 2);
        for pair in body.as_bytes().chunks(2) {
            let text = std::str::from_utf8(pair).map_err(|_| "not hex".to_owned())?;
            raw.push(u8::from_str_radix(text, 16).map_err(|_| format!("{text:?} is not hex"))?);
        }
        let (&count, after) = raw.split_first().ok_or("a record needs a count byte")?;
        if usize::from(count) != after.len() {
            return Err(format!(
                "the count says {count} bytes follow it and {} do",
                after.len()
            ));
        }
        let width = match kind {
            '0' | '1' | '9' => 2,
            '2' | '8' => 3,
            '3' | '7' => 4,
            '5' => 2,
            '6' => 3,
            other => return Err(format!("record type S{other} is not one this reads")),
        };
        let (&checksum, payload) = after.split_last().ok_or("a record needs a checksum")?;
        let sum = std::iter::once(count)
            .chain(payload.iter().copied())
            .fold(0u8, |total, byte| total.wrapping_add(byte));
        if !sum != checksum {
            return Err(format!(
                "checksum {checksum:#04x} does not match the data, which sums to {:#04x}",
                !sum
            ));
        }
        if payload.len() < width {
            return Err("the record is too short for its address".to_owned());
        }
        let (address_bytes, data) = payload.split_at(width);
        let address = address_bytes
            .iter()
            .fold(0u32, |total, byte| (total << 8) | u32::from(*byte));
        Ok(SRecord { kind, address, data: data.to_vec() })
    }
}

/// One Intel HEX record.
struct Record {
    address: u16,
    kind: u8,
    data: Vec<u8>,
}

impl Record {
    /// Parse `:LLAAAATT<data>CC`, checking the length byte and the checksum.
    ///
    /// **THE CHECKSUM IS VERIFIED RATHER THAN SKIPPED.** These bytes are going into flash on real
    /// hardware, and a truncated download is the ordinary way a hex file goes wrong -- it stays
    /// well-formed right up to where it stops.
    fn parse(line: &str) -> Result<Record, String> {
        let body = line.strip_prefix(':').ok_or("a record must begin with ':'")?;
        if body.len() < 10 || body.len() % 2 != 0 {
            return Err("too short, or an odd number of hex digits".to_owned());
        }
        let raw: Vec<u8> = (0..body.len() / 2)
            .map(|index| u8::from_str_radix(&body[index * 2..index * 2 + 2], 16))
            .collect::<Result<_, _>>()
            .map_err(|_| "not hexadecimal".to_owned())?;
        let length = usize::from(raw[0]);
        if raw.len() != length + 5 {
            return Err(format!(
                "the length byte says {length} data bytes, and the record carries {}",
                raw.len().saturating_sub(5)
            ));
        }
        let sum = raw.iter().fold(0u8, |total, byte| total.wrapping_add(*byte));
        if sum != 0 {
            return Err("the checksum does not agree with the record".to_owned());
        }
        Ok(Record {
            address: (u16::from(raw[1]) << 8) | u16::from(raw[2]),
            kind: raw[3],
            data: raw[4..4 + length].to_vec(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A four-byte image at 0x0000, then end-of-file. Checksums computed by the rule the parser
    /// checks, so a wrong one here would fail rather than pass silently.
    const SIMPLE: &str = ":04000000DEADBEEFC4\n:00000001FF\n";

    /// **A STATED ZERO AND NO STATEMENT AT ALL ARE DIFFERENT FACTS**, and the flash verb refuses on
    /// one and not the other.
    ///
    /// A flat `.bin` is bytes: where they belong is the board's to say. Intel HEX, S-records and
    /// ELF each carry an address, and one that disagrees with the board is worth stopping for.
    /// Recording the absence as `0` made every flat image look like a claim of address zero --
    /// invisible on a part that boots from zero, and a refusal of every flat image on one that
    /// does not.
    #[test]
    fn a_flat_binary_states_no_address_and_a_hex_at_zero_states_one() {
        let flat = Artifact { bytes: vec![1, 2, 3, 4], base: None, format: "raw binary" };
        let hex = parse_intel_hex(SIMPLE.as_bytes()).expect("valid Intel HEX");
        assert_eq!(flat.base, None, "a flat binary carries no address");
        assert_eq!(hex.base, Some(0), "and a HEX record at zero carries one that happens to be 0");
        assert_ne!(flat.base, hex.base, "so the two must not compare equal");
    }

    #[test]
    fn a_simple_hex_reads_back_as_its_bytes() {
        let artifact = parse_intel_hex(SIMPLE.as_bytes()).expect("valid Intel HEX");
        assert_eq!(artifact.bytes, vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(artifact.base, Some(0), "these records state address zero");
        assert_eq!(artifact.format, "Intel HEX");
    }

    /// **THE WRITER AND THE READER MUST BE INVERSES, AT EVERY ADDRESS WIDTH.** `build --format
    /// s19` produced S-records that `flash` then refused as "not a format this can write" -- the
    /// CLI emitting an artifact its own verb could not take, while the usage claimed *"`build`
    /// produces what `flash` consumes"*.
    ///
    /// The three bases are the point rather than padding: the record TYPE carries the address
    /// width, so the writer picks `S1`/`S2`/`S3` by how high the image reaches. A reader that
    /// assumed one width round-trips a small image perfectly and puts a large one at a wrong
    /// address, which a board reports as nothing at all.
    #[test]
    fn s_records_round_trip_at_every_address_width() {
        let image = [0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        for base in [0x0000_0000u32, 0x0000_1000, 0x0080_0000, 0x0800_0000, 0x1000_0000] {
            let text = write_srecord(&image, base);
            let read = parse_srecord(text.as_bytes())
                .unwrap_or_else(|why| panic!("base {base:#010x} did not read back: {why}"));
            assert_eq!(read.base, Some(base), "base {base:#010x} moved");
            assert_eq!(read.bytes, image, "base {base:#010x} changed the bytes");
            assert_eq!(read.format, "Motorola S-records");
        }
    }

    /// The same image written as Intel HEX and as S-records must read back identically. Two
    /// readers agreeing is a stronger claim than either one round-tripping against its own writer,
    /// which can share a mistake with it.
    #[test]
    fn the_two_text_formats_read_back_the_same_image() {
        let image = [0x11, 0x22, 0x33, 0x44, 0x55];
        let base = 0x0800_0000;
        let hex = parse_intel_hex(write_intel_hex(&image, base).as_bytes()).expect("hex");
        let srec = parse_srecord(write_srecord(&image, base).as_bytes()).expect("s19");
        assert_eq!(hex.bytes, srec.bytes);
        assert_eq!(hex.base, srec.base);
    }

    /// A truncated S-record download stays well-formed right up to where it stops, exactly as a
    /// hex one does, so the checksum is what catches it before the bytes reach flash.
    #[test]
    fn a_bad_s_record_checksum_is_refused() {
        let good = write_srecord(&[0xDE, 0xAD, 0xBE, 0xEF], 0);
        let first = good.lines().next().expect("a data record").to_owned();
        let mut corrupt = first[..first.len() - 1].to_owned();
        corrupt.push(if first.ends_with('0') { '1' } else { '0' });
        let error = parse_srecord(corrupt.as_bytes()).expect_err("the checksum disagrees");
        assert!(error.contains("checksum"), "got {error}");
    }

    /// A gap is refused rather than filled, for the same reason the hex reader refuses one.
    #[test]
    fn two_disjoint_s_record_regions_are_refused_by_name() {
        let mut text = write_srecord(&[0xDE, 0xAD], 0x0000).replace("S9030000FC\n", "");
        text.push_str(&write_srecord(&[0x12, 0x34], 0x1000));
        let error = parse_srecord(text.as_bytes()).expect_err("not one flat image");
        assert!(error.contains("more than one region"), "got {error}");
    }

    /// **A LINKED ELF IS WHAT SOMEBODY ELSE'S TOOLCHAIN HANDS YOU**, and `flash` exists to write
    /// bytes whoever built them. The flattening takes segments by PHYSICAL address, which is the
    /// address a programmer needs on a part whose initialized data is copied from flash to RAM.
    #[test]
    fn a_linked_elf_flattens_to_its_physical_base() {
        let elf = lamella_elf::write_executable_arm_thumb(&[0x00, 0xBF, 0x00, 0xBF], 0, 0x0800_0000);
        let image = lamella_elf::flat_image(&elf).expect("a linked executable");
        assert_eq!(image.base, 0x0800_0000);
        assert!(image.bytes.ends_with(&[0x00, 0xBF, 0x00, 0xBF]));
    }

    /// **AN UNLINKED OBJECT IS THE MISTAKE THIS WILL ACTUALLY MEET**, because it is what a
    /// compiler emits and what somebody reaches for first. It must be refused by name rather than
    /// flattened into whatever its sections happen to say.
    #[test]
    fn an_unlinked_object_is_not_mistaken_for_an_image() {
        let object = lamella_elf::write_relocatable_object(
            lamella_elf::Machine::Arm,
            &[0x00, 0xBF],
            &[],
            &[],
        );
        assert_eq!(
            lamella_elf::flat_image(&object),
            Err(lamella_elf::ElfError::NotExecutableElf32)
        );
    }

    /// **A TRUNCATED DOWNLOAD IS THE ORDINARY WAY A HEX FILE GOES WRONG**, and it stays
    /// well-formed right up to where it stops -- so the checksum is the only thing that catches
    /// it before the bytes reach flash.
    #[test]
    fn a_bad_checksum_is_refused() {
        let corrupt = ":04000000DEADBEEF00\n:00000001FF\n";
        let error = parse_intel_hex(corrupt.as_bytes()).expect_err("the checksum disagrees");
        assert!(error.contains("checksum"), "got {error}");
    }

    /// **A GAP IS REFUSED RATHER THAN FILLED.** Padding one would write bytes the file never
    /// contained, at addresses it never mentioned, into flash something else may own.
    #[test]
    fn two_disjoint_regions_are_refused_by_name() {
        let split = ":04000000DEADBEEFC4\n:0410000012345678D8\n:00000001FF\n";
        let error = parse_intel_hex(split.as_bytes()).expect_err("not one flat image");
        assert!(error.contains("more than one region"), "got {error}");
        assert!(error.contains("0x00001000"), "and says where it jumped to: {error}");
    }

    /// An extended linear address moves every following record above 64 KB. Without it, an image
    /// at 0x0800_0000 would stack at the bottom of memory -- the records repeat their low
    /// addresses, so nothing about them alone looks wrong.
    #[test]
    fn an_extended_linear_address_sets_the_high_half_of_the_base() {
        let high = ":020000040800F2\n:04000000DEADBEEFC4\n:00000001FF\n";
        let artifact = parse_intel_hex(high.as_bytes()).expect("valid Intel HEX");
        assert_eq!(artifact.base, Some(0x0800_0000));
        assert_eq!(artifact.bytes.len(), 4);
    }

    #[test]
    fn a_file_with_no_data_records_is_refused() {
        let error = parse_intel_hex(b":00000001FF\n").expect_err("nothing to write");
        assert!(error.contains("nothing to write"), "got {error}");
    }

    /// **THE WRITER IS CHECKED AGAINST A HAND-COMPUTED LINE, NOT ONLY AGAINST THE READER.**
    /// Round-tripping through this file's own reader would agree with itself whatever checksum
    /// rule both sides used -- and the file's whole purpose is being read by somebody else's
    /// programmer.
    #[test]
    fn intel_hex_is_written_exactly_as_the_format_specifies() {
        let text = write_intel_hex(&[0xDE, 0xAD, 0xBE, 0xEF], 0);
        assert_eq!(text, ":04000000DEADBEEFC4\n:00000001FF\n");
    }

    /// Above 64 KB the writer must emit an extended-address record, or every data record's low 16
    /// bits read back at the bottom of memory.
    #[test]
    fn intel_hex_states_the_high_half_of_an_address_above_64k() {
        let text = write_intel_hex(&[0xDE, 0xAD, 0xBE, 0xEF], 0x0800_0000);
        assert!(text.starts_with(":020000040800F2\n"), "got {text}");
    }

    /// The same discipline for S-records: a hand-computed line, including the canonical `S9030000FC`
    /// terminator that every emitter produces for a 16-bit file.
    #[test]
    fn s_records_are_written_exactly_as_the_format_specifies() {
        let text = write_srecord(&[0xDE, 0xAD, 0xBE, 0xEF], 0);
        assert_eq!(text, "S1070000DEADBEEFC0\nS9030000FC\n");
    }

    /// The address width comes from the HIGHEST address and applies to every record, so an image
    /// high in the map is `S3`/`S7` throughout rather than changing width partway.
    #[test]
    fn s_record_width_is_chosen_once_from_the_highest_address() {
        let high = write_srecord(&[0x00, 0x01], 0x0800_0000);
        assert!(high.starts_with("S3"), "a 32-bit address wants S3 records: {high}");
        assert!(high.trim_end().ends_with(|_c: char| true) && high.contains("\nS7"), "and an S7 terminator: {high}");
        let low = write_srecord(&[0x00, 0x01], 0x1000);
        assert!(low.starts_with("S1"), "a 16-bit address wants S1: {low}");
        assert!(low.contains("\nS9"), "and an S9 terminator: {low}");
    }

    /// **WHAT `build` WRITES, `flash` MUST READ.** The two halves are separate code and the loop
    /// between them is the point of having both, so a round trip is asserted on top of the literal
    /// checks above -- those settle the format, this settles that the pair agree.
    #[test]
    fn an_image_written_as_intel_hex_reads_back_unchanged() {
        let image: Vec<u8> = (0..70u8).collect();
        for base in [0u32, 0x1000, 0x0800_0000] {
            let text = write_intel_hex(&image, base);
            let read_back = parse_intel_hex(text.as_bytes())
                .unwrap_or_else(|error| panic!("base {base:#x}: {error}"));
            assert_eq!(read_back.bytes, image, "base {base:#x}");
            assert_eq!(read_back.base, Some(base));
        }
    }

    /// **THE BLOCK LAYOUT IS FIXED AND A BOOTLOADER SEEKS BY MULTIPLYING**, so a short final chunk
    /// must pad rather than shorten its block. Checked on an image that is deliberately NOT a
    /// multiple of the payload size, which is the only case that can get this wrong.
    #[test]
    fn every_uf2_block_is_512_bytes_even_when_the_last_is_short() {
        let image: Vec<u8> = (0..300u32).map(|byte| byte as u8).collect();
        let uf2 = write_uf2(&image, 0x1000_0000, 0xe48b_ff59);
        assert_eq!(uf2.len(), 2 * 512, "two blocks, both full width");

        assert_eq!(&uf2[0..4], &0x0A32_4655u32.to_le_bytes(), "the first magic word");
        assert_eq!(&uf2[12..16], &0x1000_0000u32.to_le_bytes(), "block 0 lands at the base");
        assert_eq!(&uf2[16..20], &256u32.to_le_bytes(), "and carries a full payload");
        assert_eq!(&uf2[24..28], &2u32.to_le_bytes(), "and says there are two blocks in all");
        assert_eq!(&uf2[28..32], &0xe48b_ff59u32.to_le_bytes(), "and names the chip family");
        assert_eq!(&uf2[508..512], &0x0AB1_6F30u32.to_le_bytes(), "and ends with the end magic");

        assert_eq!(&uf2[512 + 12..512 + 16], &0x1000_0100u32.to_le_bytes());
        assert_eq!(&uf2[512 + 16..512 + 20], &44u32.to_le_bytes(), "the short tail");
        assert_eq!(&uf2[512 + 20..512 + 24], &1u32.to_le_bytes(), "block index 1");
        assert_eq!(&uf2[1020..1024], &0x0AB1_6F30u32.to_le_bytes(), "still a full 512-byte block");
    }

    /// A UF2's family is a property of the CHIP, so the format name cannot supply it and the board
    /// must. Parsing `uf2` yields a placeholder that `for_family` fills; shipping the placeholder
    /// would produce an image every bootloader refuses.
    #[test]
    fn a_uf2_family_comes_from_the_board_and_not_the_format_name() {
        let parsed = Format::parse("uf2").expect("a format this writes");
        assert_eq!(parsed, Format::Uf2 { family: 0 });
        assert_eq!(parsed.for_family(0xe48b_ff59), Format::Uf2 { family: 0xe48b_ff59 });
        assert_eq!(Format::IntelHex.for_family(0xe48b_ff59), Format::IntelHex);
    }

    #[test]
    fn a_format_name_that_is_not_one_lists_the_names_that_are() {
        assert_eq!(Format::parse("hex"), Ok(Format::IntelHex));
        assert_eq!(Format::parse("s19"), Ok(Format::SRecord));
        assert_eq!(Format::parse("bin"), Ok(Format::Bin));
        let error = Format::parse("elf").expect_err("not a format this writes");
        assert!(error.contains("hex") && error.contains("s19"), "got {error}");
    }

    /// **THE THREE KINDS EXIST BECAUSE TWO PRODUCED A CIRCULAR REFUSAL.** With only "source or
    /// prebuilt", a `.lmli` was refused by `deploy` toward `flash` and by `flash` back toward
    /// `deploy`, and nobody holding one could get it onto a board at all. This asserts the
    /// distinction that broke the loop: a chip image and a wire payload are both prebuilt and go
    /// to different verbs.
    #[test]
    fn a_wire_payload_is_neither_source_nor_a_chip_image() {
        assert_eq!(classify(Path::new("firmware.hex")), Kind::ChipImage);
        assert_eq!(classify(Path::new("firmware.bin")), Kind::ChipImage);
        assert_eq!(classify(Path::new("image.s19")), Kind::ChipImage);
        assert_eq!(classify(Path::new("serve.uf2")), Kind::ChipImage);

        assert_eq!(classify(Path::new("Program.lmli")), Kind::WirePayload);
        assert_eq!(classify(Path::new("main.lpyc")), Kind::WirePayload);

        assert_eq!(classify(Path::new("Program.cs")), Kind::Source);
        assert_eq!(classify(Path::new("main.py")), Kind::Source);
    }

    /// A format recognized as a chip image but not yet written must say why, because a reader
    /// cannot otherwise tell a gap from a mistake.
    ///
    /// **UF2 IS NOW THE ONLY ONE**, and the second half of this test is what says so: `.s19` and
    /// `.elf` were both in this set, and both are read now. The way to prove a format is READ
    /// without having a file to hand is that it fails on the FILE rather than on the format --
    /// which is also a real behavior change, because a format refused by extension never opened
    /// the file at all.
    #[test]
    fn the_one_recognized_but_unwritable_format_says_why() {
        let error = read(Path::new("serve.uf2")).expect_err("not written");
        assert!(error.contains("bootloader volume"), "got {error}");

        for name in ["serve.s19", "serve.elf"] {
            let error = read(Path::new(name)).expect_err("no such file");
            assert!(
                !error.contains("not a format this can write"),
                "{name} is a format this reads now, so it must fail on the file: {error}"
            );
        }
    }
}
