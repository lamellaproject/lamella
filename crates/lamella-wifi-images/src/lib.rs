//! The images a Wi-Fi radio needs to run -- its firmware, its regulatory data and its board
//! settings -- downloaded at build time from the projects that publish them, each at a pinned
//! commit, and refused unless every byte matches a pinned SHA-256. None of the images is kept in
//! this repository; the license they are published under is downloaded with them.
//!
//! Today it carries the CYW43439's images, from the driver project that publishes them, for the
//! radio on the Raspberry Pi Pico 2 W and the Pimoroni Pico Plus 2 W. Other radios' images join it
//! with their boards.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use lamella_pe::sha256::sha256;

/// The published files of the driver project that publishes the images. Each file a build takes
/// from it names its own pinned commit (see [`PINS`]).
pub const UPSTREAM: &str = "https://raw.githubusercontent.com/georgerobotics/cyw43-driver";

/// Mirrors, tried in order only when the upstream download fails or does not match its pins. Each
/// serves the four output files by name, flat.
pub const MIRRORS: &[&str] =
    &["https://raw.githubusercontent.com/lamellaproject/firmware-mirror/main/cyw43439/7.95.49"];

/// The environment variable a build reads for a folder holding the four files, fetched earlier --
/// for a build with no network. The folder is checked against the same pins.
pub const IMAGES_DIR_ENV: &str = "LAMELLA_WIFI_IMAGES_DIR";

/// The version string the firmware's reply must contain, bound to [`FIRMWARE`]'s pin.
pub const VERSION: &str = "7.95.49";

/// A file's name, length and SHA-256, as lowercase hex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pin {
    /// The file's name.
    pub name: &'static str,
    /// Its length in bytes.
    pub len: usize,
    /// Its SHA-256, lowercase hex.
    pub sha256: &'static str,
}

/// The firmware image.
pub const FIRMWARE: Pin = Pin {
    name: "wifi-fw.bin",
    len: 224_190,
    sha256: "dda7d2527e1cc9b1ebdd107d46932643617f6e83040f24cba722cbb0ded8c4a8",
};

/// The regulatory blob.
pub const REGULATORY: Pin = Pin {
    name: "wifi-clm.blob",
    len: 984,
    sha256: "e712b3d218e8b1e2747b092e03b8b0afcb8c8c8e355d2a4a0d47b493800f3f89",
};

/// The board-settings image: one `key=value` string per setting, each closed by a NUL, and one
/// more NUL at the end.
pub const SETTINGS: Pin = Pin {
    name: "wifi-settings.img",
    len: 741,
    sha256: "0517e0a609291e10c264d7d470acf3d823c23a9e7f7d5b12816a6d45b5cf1f71",
};

/// The license the images are published under, which travels with them.
pub const LICENSE: Pin = Pin {
    name: "LICENSE.RP",
    len: 1_789,
    sha256: "67aca4f10d9edf489871f64cd8f0dcd6c5df3e4ce75bd39e1914fc54f99e40b3",
};

/// Everything a provision checks. [`PINS`] is the only set a build uses.
#[derive(Clone, Copy, Debug)]
pub struct Pins {
    /// The firmware image.
    pub firmware: Pin,
    /// The regulatory blob.
    pub regulatory: Pin,
    /// The board-settings image.
    pub settings: Pin,
    /// The license.
    pub license: Pin,
    /// The upstream's one C array holding the firmware, a gap of zeros, and the regulatory blob.
    combined: Pin,
    /// Where the regulatory blob starts in that array.
    regulatory_at: usize,
    /// The settings as text, one `key=value` line each with a line feed: what the image is built from.
    settings_text: Pin,
    /// The upstream files, relative to the upstream base, each under the commit it is pinned at.
    firmware_header: &'static str,
    settings_header: &'static str,
    license_file: &'static str,
}

/// The pins a build checks.
pub const PINS: Pins = Pins {
    firmware: FIRMWARE,
    regulatory: REGULATORY,
    settings: SETTINGS,
    license: LICENSE,
    combined: Pin {
        name: "w43439A0_7_95_49_00_combined",
        len: 225_240,
        sha256: "95fec95d0bdbe59ab0906cfc4f56d9dbea089fe5f8f668650d2d5c160ccc0c81",
    },
    regulatory_at: 224_256,
    settings_text: Pin {
        name: "the settings text",
        len: 740,
        sha256: "e993c88c79cd28ecd6ab2f06182e91910c22e9cbf29371b401d770113239e0e0",
    },
    firmware_header: "a1dc8859d09b8f547a4f4345fd0efb7300105e62/firmware/w43439A0_7_95_49_00_combined.h",
    settings_header: "67b125457843a8eb4d3c374e9c48b7538e1e334d/firmware/wifi_nvram_43439.h",
    license_file: "a1dc8859d09b8f547a4f4345fd0efb7300105e62/LICENSE.RP",
};

// The pins describe one array -- the firmware, a gap, the regulatory blob ending it -- and a settings
// image one byte longer than its text: a NUL for each line feed, and one more. A pin edited out of
// step with the others fails the build.
const _: () = assert!(PINS.combined.len == PINS.regulatory_at + REGULATORY.len && PINS.regulatory_at > FIRMWARE.len);
const _: () = assert!(PINS.settings_text.len + 1 == SETTINGS.len);

impl Pins {
    /// The four files a provision writes, in order.
    pub fn outputs(&self) -> [Pin; 4] {
        [self.firmware, self.regulatory, self.settings, self.license]
    }
}

/// Why the images could not be provided.
#[derive(Debug)]
pub enum Error {
    /// `curl`, the downloader, could not be run.
    NoCurl(io::Error),
    /// A download failed.
    Download {
        /// What was asked for.
        url: String,
        /// What curl said.
        detail: String,
    },
    /// A downloaded file did not have the shape the pinned commit publishes.
    Shape {
        /// The file.
        file: String,
        /// What was wrong.
        detail: String,
    },
    /// A file's bytes did not match its pin.
    Mismatch {
        /// The file.
        file: String,
        /// Its pinned length and SHA-256.
        expected: Pin,
        /// Its length.
        len: usize,
        /// Its SHA-256.
        sha256: String,
    },
    /// A local file could not be read or written.
    Io {
        /// The path.
        path: PathBuf,
        /// What failed.
        error: io::Error,
    },
    /// Every source failed: each one, and why, in the order tried.
    Sources(Vec<(String, Error)>),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NoCurl(error) => write!(
                f,
                "the Wi-Fi radio's images are downloaded with curl, and curl could not be run ({error}). \
                 Install curl -- it ships with Windows 10 and later, macOS and most Linux distributions -- \
                 or set {IMAGES_DIR_ENV} to a folder holding {}, {}, {} and {}",
                FIRMWARE.name, REGULATORY.name, SETTINGS.name, LICENSE.name
            ),
            Error::Download { url, detail } => write!(f, "downloading {url} failed: {detail}"),
            Error::Shape { file, detail } => write!(f, "{file} is not the file the pinned commit publishes: {detail}"),
            Error::Mismatch { file, expected, len, sha256 } => write!(
                f,
                "{file} is {len} bytes with SHA-256 {sha256}; its pin is {} bytes with SHA-256 {}",
                expected.len, expected.sha256
            ),
            Error::Io { path, error } => write!(f, "{}: {error}", path.display()),
            Error::Sources(tried) => {
                write!(f, "no source provided the Wi-Fi radio's images:")?;
                for (source, error) in tried {
                    write!(f, "\n  {source}: {error}")?;
                }
                if MIRRORS.is_empty() {
                    write!(f, "\n  (no mirror is configured)")?;
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for Error {}

/// Provide the four files in `dest`: from `prefetched` when a build names a folder, else as they
/// already are in `dest` when they match their pins, else downloaded -- from the upstream first and
/// from each mirror after it. Every file written matches its pin.
pub fn provide(dest: &Path, prefetched: Option<&Path>) -> Result<(), Error> {
    provide_with(dest, prefetched, &PINS, UPSTREAM, MIRRORS, &mut curl)
}

/// [`provide`], with the pins, the sources and the downloader as arguments.
fn provide_with(
    dest: &Path,
    prefetched: Option<&Path>,
    pins: &Pins,
    upstream: &str,
    mirrors: &[&str],
    fetch: &mut dyn FnMut(&str, &Path) -> Result<(), Error>,
) -> Result<(), Error> {
    create_dir(dest)?;
    if let Some(folder) = prefetched {
        for pin in pins.outputs() {
            let bytes = read(&folder.join(pin.name))?;
            write_checked(dest, &pin, &bytes)?;
        }
        return Ok(());
    }
    if pins.outputs().iter().all(|pin| read(&dest.join(pin.name)).is_ok_and(|bytes| check(pin, pin.name, &bytes).is_ok())) {
        return Ok(());
    }
    let mut tried = Vec::new();
    match from_upstream(dest, pins, upstream, fetch) {
        Ok(()) => return Ok(()),
        Err(error @ Error::NoCurl(_)) => return Err(error),
        Err(error) => tried.push((upstream.to_string(), error)),
    }
    for mirror in mirrors {
        match from_mirror(dest, pins, mirror, fetch) {
            Ok(()) => return Ok(()),
            Err(error @ Error::NoCurl(_)) => return Err(error),
            Err(error) => tried.push(((*mirror).to_string(), error)),
        }
    }
    Err(Error::Sources(tried))
}

/// The three upstream files downloaded, the images cut and converted out of them, each checked.
fn from_upstream(
    dest: &Path,
    pins: &Pins,
    base: &str,
    fetch: &mut dyn FnMut(&str, &Path) -> Result<(), Error>,
) -> Result<(), Error> {
    let work = dest.join("upstream");
    create_dir(&work)?;
    let mut download = |file: &str| -> Result<Vec<u8>, Error> {
        let path = work.join(file.rsplit('/').next().unwrap_or(file));
        fetch(&format!("{base}/{file}"), &path)?;
        read(&path)
    };
    let firmware_header = download(pins.firmware_header)?;
    let settings_header = download(pins.settings_header)?;
    let license = download(pins.license_file)?;

    let combined = c_array(&text(&firmware_header, pins.firmware_header)?, pins.combined.name, pins.firmware_header)?;
    check(&pins.combined, pins.firmware_header, &combined)?;
    let firmware = &combined[..pins.firmware.len];
    let gap = &combined[pins.firmware.len..pins.regulatory_at];
    if gap.iter().any(|&byte| byte != 0) {
        return Err(shape(pins.firmware_header, "the bytes between the firmware and the regulatory blob are not zero"));
    }
    let regulatory = combined
        .get(pins.regulatory_at..pins.regulatory_at + pins.regulatory.len)
        .ok_or_else(|| shape(pins.firmware_header, "the array ends before the regulatory blob does"))?;

    let lines = settings_lines(&text(&settings_header, pins.settings_header)?, pins.settings_header)?;
    let settings_text: Vec<u8> = lines.iter().flat_map(|line| line.bytes().chain(Some(b'\n'))).collect();
    check(&pins.settings_text, pins.settings_header, &settings_text)?;

    write_checked(dest, &pins.firmware, firmware)?;
    write_checked(dest, &pins.regulatory, regulatory)?;
    write_checked(dest, &pins.settings, &settings_image(&lines))?;
    write_checked(dest, &pins.license, &license)
}

/// The four files downloaded from a mirror as they are, each checked.
fn from_mirror(
    dest: &Path,
    pins: &Pins,
    base: &str,
    fetch: &mut dyn FnMut(&str, &Path) -> Result<(), Error>,
) -> Result<(), Error> {
    let work = dest.join("mirror");
    create_dir(&work)?;
    for pin in pins.outputs() {
        let path = work.join(pin.name);
        fetch(&format!("{base}/{}", pin.name), &path)?;
        write_checked(dest, &pin, &read(&path)?)?;
    }
    Ok(())
}

/// The bytes of the C array named `name`: its `0x..` initializers, in order.
fn c_array(text: &str, name: &str, file: &str) -> Result<Vec<u8>, Error> {
    let at = text.find(&format!("{name}[")).ok_or_else(|| shape(file, &format!("no array named {name}")))?;
    let open = at + text[at..].find('{').ok_or_else(|| shape(file, "the array has no initializer"))?;
    let close = open + text[open..].find('}').ok_or_else(|| shape(file, "the array's initializer is not closed"))?;
    let mut bytes = Vec::new();
    for token in text[open + 1..close].split(',').map(str::trim).filter(|token| !token.is_empty()) {
        let byte = token
            .strip_prefix("0x")
            .and_then(|hex| u8::from_str_radix(hex, 16).ok())
            .ok_or_else(|| shape(file, &format!("`{token}` is not a byte")))?;
        bytes.push(byte);
    }
    Ok(bytes)
}

/// The settings, one `key=value` string each, from the string literals of the settings array: each
/// literal's bytes joined, split at their NULs.
fn settings_lines(text: &str, file: &str) -> Result<Vec<String>, Error> {
    let start = text.find("CYW43_RESOURCE_ATTRIBUTE =").ok_or_else(|| shape(file, "no settings array"))?;
    let end = start + text[start..].find(';').ok_or_else(|| shape(file, "the settings array is not closed"))?;
    let mut joined = Vec::new();
    let mut chars = text[start..end].chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        loop {
            match chars.next() {
                Some('"') => break,
                Some('\\') => match chars.next() {
                    Some('x') => {
                        let hex: String = chars.by_ref().take(2).collect();
                        let byte = u8::from_str_radix(&hex, 16)
                            .map_err(|_| shape(file, &format!("`\\x{hex}` is not a byte")))?;
                        joined.push(byte);
                    }
                    Some(other) => return Err(shape(file, &format!("an escape `\\{other}` this reader does not take"))),
                    None => return Err(shape(file, "a string ends in an escape")),
                },
                Some(c) if c.is_ascii() => joined.push(c as u8),
                Some(c) => return Err(shape(file, &format!("a character outside ASCII, {c:?}"))),
                None => return Err(shape(file, "a string is not closed")),
            }
        }
    }
    let mut lines = Vec::new();
    for piece in joined.split(|&byte| byte == 0).filter(|piece| !piece.is_empty()) {
        let line = String::from_utf8_lossy(piece).into_owned();
        if !line.contains('=') || line.contains(' ') {
            return Err(shape(file, &format!("`{line}` is not one `key=value` with no space")));
        }
        lines.push(line);
    }
    if !lines.iter().any(|line| line == "xtalfreq=37400") {
        return Err(shape(file, "the settings do not set xtalfreq=37400"));
    }
    Ok(lines)
}

/// The settings image: each line closed by a NUL, and one more NUL to close the image.
fn settings_image(lines: &[String]) -> Vec<u8> {
    let mut image: Vec<u8> = lines.iter().flat_map(|line| line.bytes().chain(Some(0))).collect();
    image.push(0);
    image
}

/// `bytes` against `pin`.
fn check(pin: &Pin, file: &str, bytes: &[u8]) -> Result<(), Error> {
    let digest = hex(&sha256(bytes));
    if bytes.len() == pin.len && digest == pin.sha256 {
        return Ok(());
    }
    Err(Error::Mismatch { file: file.to_string(), expected: *pin, len: bytes.len(), sha256: digest })
}

/// `bytes` checked against `pin`, then written to `dir` under the pin's name: to a partial file
/// first, renamed into place, so a file of that name is never half written.
fn write_checked(dir: &Path, pin: &Pin, bytes: &[u8]) -> Result<(), Error> {
    check(pin, pin.name, bytes)?;
    let path = dir.join(pin.name);
    let partial = dir.join(format!("{}.partial", pin.name));
    fs::write(&partial, bytes).map_err(|error| Error::Io { path: partial.clone(), error })?;
    fs::rename(&partial, &path).map_err(|error| Error::Io { path, error })
}

/// Download `url` to `to` with curl.
fn curl(url: &str, to: &Path) -> Result<(), Error> {
    let output = Command::new("curl")
        .args(["--fail", "--silent", "--show-error", "--location", "--retry", "2", "--max-time", "120", "--output"])
        .arg(to)
        .arg(url)
        .output()
        .map_err(Error::NoCurl)?;
    if output.status.success() {
        return Ok(());
    }
    Err(Error::Download {
        url: url.to_string(),
        detail: String::from_utf8_lossy(&output.stderr).trim().to_string(),
    })
}

fn create_dir(dir: &Path) -> Result<(), Error> {
    fs::create_dir_all(dir).map_err(|error| Error::Io { path: dir.to_path_buf(), error })
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    fs::read(path).map_err(|error| Error::Io { path: path.to_path_buf(), error })
}

fn text(bytes: &[u8], file: &str) -> Result<String, Error> {
    String::from_utf8(bytes.to_vec()).map_err(|_| shape(file, "not text"))
}

fn shape(file: &str, detail: &str) -> Error {
    Error::Shape { file: file.to_string(), detail: detail.to_string() }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests;
