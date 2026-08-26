//! Boards presenting a BOOTLOADER VOLUME rather than a port.

/// How a volume relates to the board behind it.
///
/// **A VOLUME APPEARING MEANS TWO DIFFERENT THINGS AND ONLY ONE OF THEM IS ABOUT BOARD STATE.**
/// A UF2 volume exists because the chip is HALTED in its bootloader: it is waiting, and nothing is
/// running. An on-board programmer's volume -- DAPLink, ST-LINK -- is present whenever the board is
/// plugged in, while its program runs perfectly well. Reporting both as "waiting for an image"
/// tells somebody their running board has stopped -- a false claim about hardware they are looking
/// at, and the easiest one to make here.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Via {
    /// The chip is halted in a UF2 bootloader. Nothing is running.
    Bootloader,
    /// An on-board programmer offers a copy-to-flash volume. The board may be running.
    Programmer,
}

/// A board reachable by copying a file to a volume.
pub struct Waiting {
    /// Whether the board is halted in a bootloader or merely offering a programming volume.
    pub via: Via,
    /// The volume root, as the operating system names it today (`D:\`, `/media/user/RP2350`).
    pub volume: String,
    /// The volume label, which on some families is the only thing distinguishing a board and on
    /// others -- RP2350 -- distinguishes nothing.
    pub label: String,
    /// What the bootloader says it is: the model line out of its own info file.
    pub model: String,
    /// The board's own serial, where one is recoverable. `None` is reported rather than hidden: a
    /// board that cannot be named cannot be safely written when a sibling is attached.
    pub serial: Option<String>,
}

impl Waiting {
    /// What to print in the listing's target column.
    ///
    /// **A BOOTLOADER VOLUME IS NOT A `--target`, AND THIS MUST NOT PRETEND OTHERWISE.** Nothing
    /// speaks Lamella Link to a board in this state; it takes an image and reboots. Printing
    /// something that looked pasteable would send a reader to a verb that cannot reach it, so the
    /// column says what the board IS instead.
    #[must_use]
    pub fn describe(&self) -> String {
        let what = match self.via {
            Via::Bootloader => "bootloader",
            Via::Programmer => "volume",
        };
        match &self.serial {
            Some(serial) => format!("({what}) {serial}"),
            None => format!("({what}) {}", self.volume),
        }
    }

    /// What this board is doing, in one phrase for the listing.
    #[must_use]
    pub fn state(&self) -> String {
        let model = if self.model.is_empty() { "a board" } else { &self.model };
        match self.via {
            Via::Bootloader => format!("{model}  HALTED in its bootloader, waiting for an image"),
            Via::Programmer => format!("{model}  takes an image by copying one here"),
        }
    }
}

/// Every attached board that is sitting in a bootloader.
///
/// Detected by the file the bootloader itself puts on the volume, not by the label: a label is
/// chosen by the vendor and shared across every board of a family, while the presence of
/// `INFO_UF2.TXT` or `DETAILS.TXT` is the bootloader saying what it is.
#[must_use]
pub fn waiting() -> Vec<Waiting> {
    let mut found = Vec::new();
    for root in roots() {
        for (marker, via) in [("INFO_UF2.TXT", Via::Bootloader), ("DETAILS.TXT", Via::Programmer)] {
            let path = std::path::Path::new(&root).join(marker);
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            found.push(Waiting {
                via,
                volume: root.clone(),
                label: label_of(&root),
                model: model_of(&text),
                serial: serial_of(&text),
            });
            break;
        }
    }
    found
}

/// The model a bootloader's info file states, or an empty string when it states none.
fn model_of(text: &str) -> String {
    for key in ["Model:", "Board name:"] {
        for line in text.lines() {
            if let Some(rest) = line.trim().strip_prefix(key) {
                let value = rest.trim();
                if !value.is_empty() {
                    return value.to_owned();
                }
            }
        }
    }
    String::new()
}

/// The per-board serial a bootloader's info file states, where it states one.
///
/// **DAPLink STATES ONE AND UF2 DOES NOT**, and that difference is the whole reason this returns an
/// `Option` rather than a string. A micro:bit's `DETAILS.TXT` carries a unique HIC id; an RP2350's
/// `INFO_UF2.TXT` carries the chip model and nothing else, so two of them are indistinguishable
/// from anything on the volume. Reporting `None` is what lets a caller say so instead of implying
/// an identity it does not have.
fn serial_of(text: &str) -> Option<String> {
    for line in text.lines() {
        for key in ["Unique ID:", "HIC ID:"] {
            if let Some(rest) = line.trim().strip_prefix(key) {
                let value = rest.trim();
                if !value.is_empty() {
                    return Some(value.to_owned());
                }
            }
        }
    }
    None
}

/// Every mounted volume root that could be a board.
#[cfg(windows)]
fn roots() -> Vec<String> {
    ('A'..='Z').map(|letter| format!("{letter}:\\")).collect()
}

/// Every mounted volume root that could be a board.
#[cfg(not(windows))]
fn roots() -> Vec<String> {
    let mut roots = Vec::new();
    let mut bases = vec![std::path::PathBuf::from("/Volumes")];
    for base in ["/media", "/run/media"] {
        let base = std::path::PathBuf::from(base);
        match std::fs::read_dir(&base) {
            Ok(entries) => bases.extend(entries.flatten().map(|entry| entry.path())),
            Err(_) => bases.push(base),
        }
    }
    for base in bases {
        if let Ok(entries) = std::fs::read_dir(&base) {
            roots.extend(entries.flatten().map(|entry| entry.path().display().to_string()));
        }
    }
    roots
}

/// The volume's label, in whatever terms the platform makes cheap.
#[cfg(windows)]
fn label_of(_root: &str) -> String {
    String::new()
}

/// The volume's label, in whatever terms the platform makes cheap.
#[cfg(not(windows))]
fn label_of(root: &str) -> String {
    std::path::Path::new(root)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **THE TWO FAMILIES DIFFER IN EXACTLY ONE WAY THAT MATTERS, AND IT IS NOT THE MODEL.** A
    /// micro:bit's DAPLink states a unique id; an RP2350's UF2 bootloader does not, so two of them
    /// cannot be told apart from anything on the volume. These are the real files, byte for byte
    /// as they were read off this bench.
    #[test]
    fn a_uf2_volume_has_no_identity_and_a_daplink_one_does() {
        let uf2 = "UF2 Bootloader v1.0\nModel: Raspberry Pi RP2350\nBoard-ID: RP2350\n";
        assert_eq!(model_of(uf2), "Raspberry Pi RP2350");
        assert_eq!(
            serial_of(uf2),
            None,
            "two RP2350s in BOOTSEL are indistinguishable from the volume, and saying so is the point"
        );

        let daplink = "# DAPLink Firmware - see https://mbed.com/daplink\n\
                       Unique ID: 9900000031864e45004440180000004f00000000\n\
                       HIC ID: 97969901\n\
                       Daplink Mode: Interface\n\
                       Board name: micro:bit\n";
        assert_eq!(serial_of(daplink).as_deref(), Some("9900000031864e45004440180000004f00000000"));
        assert_eq!(model_of(daplink), "micro:bit");
    }

    /// A volume with neither marker is not a board, and must not be reported as one.
    #[test]
    fn text_from_something_that_is_not_a_bootloader_names_no_model_and_no_serial() {
        assert_eq!(serial_of("hello\n"), None);
        assert_eq!(
            model_of("hello\n"),
            "",
            "an unrecognized file names no model, and says so by saying nothing"
        );
    }

    /// **A BOOTLOADER VOLUME MUST NOT PRINT SOMETHING THAT LOOKS PASTEABLE.** Nothing speaks
    /// Lamella Link to a board in this state, so a target-shaped string would send the reader to a
    /// verb that cannot reach it.
    #[test]
    fn the_listing_text_does_not_look_like_a_target() {
        let named = Waiting {
            via: Via::Bootloader,
            volume: "D:\\".to_owned(),
            label: String::new(),
            model: "Raspberry Pi RP2350".to_owned(),
            serial: Some("C2E506C2673D626B".to_owned()),
        };
        assert!(named.describe().starts_with("(bootloader)"), "got {}", named.describe());
        assert!(named.describe().contains("C2E506C2673D626B"));

        let anonymous = Waiting { serial: None, ..named };
        assert!(anonymous.describe().contains("D:\\"), "with no serial, the volume is all there is");
    }

    /// **THE ONE THING THIS LISTING MUST NOT DO IS TELL SOMEBODY A RUNNING BOARD HAS STOPPED.** An
    /// ST-LINK volume is present while the board's program runs; a UF2 volume exists only because
    /// the chip is halted. Reporting both as "waiting" was the first thing this got wrong, and it
    /// is a false claim about hardware the reader is looking at.
    #[test]
    fn only_a_halted_board_is_described_as_waiting() {
        let halted = Waiting {
            via: Via::Bootloader,
            volume: "D:\\".to_owned(),
            label: String::new(),
            model: "Raspberry Pi RP2350".to_owned(),
            serial: None,
        };
        assert!(halted.state().contains("HALTED"), "got {}", halted.state());
        assert!(halted.state().contains("waiting"), "got {}", halted.state());

        let running = Waiting { via: Via::Programmer, ..halted };
        assert!(!running.state().contains("waiting"), "got {}", running.state());
        assert!(!running.state().contains("HALTED"), "got {}", running.state());
        assert!(running.state().contains("takes an image"), "it says what it ACCEPTS: {}", running.state());
    }
}
