//! Provisioning against synthetic upstream files and pins of the tests' own, with a downloader that
//! serves from memory and records what it was asked: the upstream's shapes parsed, the conversion,
//! the mirror as the second source, the named refusals, and a folder fetched earlier.

use super::*;
use std::collections::HashMap;
use std::string::String;
use std::vec::Vec;

/// A scratch directory of the test's own.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lamella-wifi-images-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

fn pin_of(name: &'static str, bytes: &[u8]) -> Pin {
    Pin { name, len: bytes.len(), sha256: leak(hex(&sha256(bytes))) }
}

const FIRMWARE_BYTES: [u8; 6] = [1, 2, 3, 4, 5, 6];
const REGULATORY_BYTES: [u8; 4] = *b"BLOB";

/// The combined array: the firmware, two zeros, the regulatory blob.
fn combined() -> Vec<u8> {
    [&FIRMWARE_BYTES[..], &[0, 0], &REGULATORY_BYTES[..]].concat()
}

fn firmware_header(bytes: &[u8]) -> String {
    let list: Vec<String> = bytes.iter().map(|byte| format!("0x{byte:02x}")).collect();
    format!(
        "static const unsigned char test_combined[] CYW43_RESOURCE_ATTRIBUTE = {{\n  {}\n}};\n\
         const uintptr_t fw_data = (uintptr_t)&test_combined[0];\n",
        list.join(", ")
    )
}

fn settings_header(lines: &[&str]) -> String {
    let mut text = String::from("static const uint8_t wifi_nvram_4343[] CYW43_RESOURCE_ATTRIBUTE =\n");
    for line in lines {
        text.push_str(&format!("        \"{line}\"   \"\\x00\"\n"));
    }
    text.push_str("        \"\\x00\\x00\";\n");
    text
}

const LINES: [&str; 3] = ["manfid=0x2d0", "xtalfreq=37400", "ccode=ALL"];

/// Pins for the synthetic files.
fn test_pins() -> Pins {
    let settings_text: Vec<u8> = LINES.iter().flat_map(|line| line.bytes().chain(Some(b'\n'))).collect();
    let lines: Vec<String> = LINES.iter().map(|line| line.to_string()).collect();
    Pins {
        firmware: pin_of(FIRMWARE.name, &FIRMWARE_BYTES),
        regulatory: pin_of(REGULATORY.name, &REGULATORY_BYTES),
        settings: pin_of(SETTINGS.name, &settings_image(&lines)),
        license: pin_of(LICENSE.name, b"the license\n"),
        combined: pin_of("test_combined", &combined()),
        regulatory_at: 8,
        settings_text: pin_of("the settings text", &settings_text),
        ..PINS
    }
}

/// A downloader serving `files` by URL, recording every URL asked for.
struct Server {
    files: HashMap<String, Vec<u8>>,
    asked: Vec<String>,
}

impl Server {
    fn upstream() -> Self {
        let mut files = HashMap::new();
        files.insert(format!("up/{}", PINS.firmware_header), firmware_header(&combined()).into_bytes());
        files.insert(format!("up/{}", PINS.settings_header), settings_header(&LINES).into_bytes());
        files.insert(format!("up/{}", PINS.license_file), b"the license\n".to_vec());
        Server { files, asked: Vec::new() }
    }

    fn fetch(&mut self, url: &str, to: &Path) -> Result<(), Error> {
        self.asked.push(url.to_string());
        match self.files.get(url) {
            Some(bytes) => {
                fs::write(to, bytes).unwrap();
                Ok(())
            }
            None => Err(Error::Download { url: url.to_string(), detail: String::from("404") }),
        }
    }
}

fn provide_from(dest: &Path, server: &mut Server, mirrors: &[&str]) -> Result<(), Error> {
    provide_with(dest, None, &test_pins(), "up", mirrors, &mut |url, to| server.fetch(url, to))
}

fn assert_provided(dest: &Path) {
    let pins = test_pins();
    for pin in pins.outputs() {
        let bytes = fs::read(dest.join(pin.name)).unwrap();
        check(&pin, pin.name, &bytes).unwrap();
    }
    assert_eq!(fs::read(dest.join(SETTINGS.name)).unwrap(), b"manfid=0x2d0\0xtalfreq=37400\0ccode=ALL\0\0");
}

#[test]
fn every_real_pin_is_a_lowercase_sha256() {
    for pin in [FIRMWARE, REGULATORY, SETTINGS, LICENSE, PINS.combined, PINS.settings_text] {
        assert_eq!(pin.sha256.len(), 64, "{}", pin.name);
        assert!(pin.sha256.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()), "{}", pin.name);
    }
}

#[test]
fn the_upstream_files_are_cut_and_converted_into_the_four_pinned_files() {
    let dest = scratch("upstream");
    let mut server = Server::upstream();
    provide_from(&dest, &mut server, &[]).unwrap();
    assert_provided(&dest);
    assert_eq!(server.asked.len(), 3);
    provide_from(&dest, &mut server, &[]).unwrap();
    assert_eq!(server.asked.len(), 3, "files that match their pins are not downloaded again");
}

#[test]
fn a_mirror_is_asked_only_when_the_upstream_fails_or_does_not_match() {
    let pins = test_pins();
    let mut mirrored = Server { files: HashMap::new(), asked: Vec::new() };
    let lines: Vec<String> = LINES.iter().map(|line| line.to_string()).collect();
    for (pin, bytes) in [
        (pins.firmware, FIRMWARE_BYTES.to_vec()),
        (pins.regulatory, REGULATORY_BYTES.to_vec()),
        (pins.settings, settings_image(&lines)),
        (pins.license, b"the license\n".to_vec()),
    ] {
        mirrored.files.insert(format!("mirror/{}", pin.name), bytes);
    }

    let dest = scratch("mirror-after-a-failure");
    let mut server = Server { files: mirrored.files.clone(), asked: Vec::new() };
    provide_from(&dest, &mut server, &["mirror"]).unwrap();
    assert_provided(&dest);
    assert!(server.asked[0].starts_with("up/"), "the upstream first: {:?}", server.asked);

    let dest = scratch("mirror-after-a-mismatch");
    let mut server = Server::upstream();
    server.files.insert(format!("up/{}", PINS.settings_header), settings_header(&["xtalfreq=37400", "ccode=XX"]).into_bytes());
    server.files.extend(mirrored.files.clone());
    provide_from(&dest, &mut server, &["mirror"]).unwrap();
    assert_provided(&dest);

    let dest = scratch("upstream-good");
    let mut server = Server::upstream();
    provide_from(&dest, &mut server, &["mirror"]).unwrap();
    assert!(server.asked.iter().all(|url| url.starts_with("up/")), "no mirror while the upstream matches");
}

#[test]
fn when_every_source_fails_each_is_named_with_its_reason() {
    let dest = scratch("all-fail");
    let mut server = Server::upstream();
    server.files.insert(format!("up/{}", PINS.firmware_header), firmware_header(&[9, 9, 9]).into_bytes());
    let error = provide_from(&dest, &mut server, &["mirror"]).unwrap_err();
    let Error::Sources(tried) = &error else { panic!("{error}") };
    assert_eq!(tried.len(), 2);
    assert!(matches!(tried[0].1, Error::Mismatch { .. }), "{error}");
    assert!(matches!(tried[1].1, Error::Download { .. }), "{error}");
    assert!(!dest.join(FIRMWARE.name).exists(), "nothing is written that did not match");
}

#[test]
fn a_missing_curl_is_refused_at_once_by_name_and_says_what_to_install() {
    let dest = scratch("no-curl");
    let mut asked = 0;
    let error = provide_with(&dest, None, &test_pins(), "up", &["mirror"], &mut |_, _| {
        asked += 1;
        Err(Error::NoCurl(io::Error::new(io::ErrorKind::NotFound, "program not found")))
    })
    .unwrap_err();
    assert_eq!(asked, 1, "no mirror is tried without a downloader");
    let text = error.to_string();
    assert!(text.contains("Install curl") && text.contains(IMAGES_DIR_ENV), "{text}");
}

#[test]
fn a_folder_fetched_earlier_is_checked_by_the_same_pins_and_never_downloaded_around() {
    let good = scratch("prefetched-good");
    provide_from(&good, &mut Server::upstream(), &[]).unwrap();
    let dest = scratch("prefetched-dest");
    let mut asked = 0;
    let mut never = |_: &str, _: &Path| -> Result<(), Error> {
        asked += 1;
        Ok(())
    };
    provide_with(&dest, Some(&good), &test_pins(), "up", &[], &mut never).unwrap();
    assert_provided(&dest);

    fs::write(good.join(SETTINGS.name), b"xtalfreq=37400\0\0").unwrap();
    let dest = scratch("prefetched-bad");
    let error = provide_with(&dest, Some(&good), &test_pins(), "up", &[], &mut never).unwrap_err();
    assert!(matches!(error, Error::Mismatch { .. }), "{error}");
    assert_eq!(asked, 0);
}

#[test]
fn the_settings_reader_refuses_a_line_with_a_space_or_no_crystal_setting() {
    let spaced = settings_lines(&settings_header(&["xtalfreq=37400", "a b=1"]), "h").unwrap_err();
    assert!(spaced.to_string().contains("`a b=1`"), "{spaced}");
    let no_crystal = settings_lines(&settings_header(&["ccode=ALL"]), "h").unwrap_err();
    assert!(no_crystal.to_string().contains("xtalfreq=37400"), "{no_crystal}");
}

#[test]
fn the_array_reader_refuses_a_token_that_is_not_a_byte() {
    let error = c_array("unsigned char x[] = { 0x01, 0x1g };", "x", "h").unwrap_err();
    assert!(error.to_string().contains("`0x1g`"), "{error}");
    assert_eq!(c_array("unsigned char x[] = { 0x01, 0xff, };", "x", "h").unwrap(), [1, 255]);
}
