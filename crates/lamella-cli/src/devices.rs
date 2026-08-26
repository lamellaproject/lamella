//! `lamella devices`: what is attached, and what to type to reach it.

use crate::args::{self, Spec};
use lamella_wire::{Capabilities, Negotiated};
use lamella_wire_host::{UsbTransport, hello_blocking, list_serial, open_target};
use std::process::ExitCode;
use std::time::Duration;

/// The default serial baud for a Lamella Link carrier (USB-CDC ignores it; a real UART wants it).
const BAUD: u32 = 115_200;

/// How long to wait for a HELLO_ACK under `--identify`.
///
/// Short on purpose. This asks EVERY listed board at once, and most of them are not running Lamella
/// firmware -- so the common case is a wait, not an answer, and a generous timeout multiplied by
/// the number of attached boards is the whole command's runtime.
const IDENTIFY_TIMEOUT: Duration = Duration::from_millis(1500);

/// One attached board, as the listing sees it.
struct Attached {
    /// What to paste into `--target`.
    target: String,
    /// Which carrier the target names.
    carrier: &'static str,
    /// What this is, in whatever terms the operating system reported.
    what: String,
}

const USAGE: &str = "\
usage: lamella devices [--identify]

Lists what is attached and, for each, THE WORD YOU PASS TO ANOTHER VERB -- so the first column can
be copied straight into --target or --probe rather than translated.

A board in its bootloader is listed here too. It is still attached, and it is the one somebody
holding a new board is most likely to be looking for.

--identify asks each board what it is, over the wire, rather than reporting what the operating
system said about the port. That costs a round trip per board and is the answer worth having when
two boards look alike.";

pub fn devices_command(args: &[String]) -> ExitCode {
    let spec =
        Spec { verb: "devices", usage: Some(USAGE), values: &[], flags: &["--identify"] };
    let parsed = match args::parse_or_halt(args, &spec) {
        Ok(parsed) => parsed,
        Err(halt) => return halt.code(),
    };

    let mut attached = enumerate();
    for waiting in lamella_flash_routes::bootsel::waiting() {
        let what = format!("{}  ({})", waiting.state(), waiting.volume);
        attached.push(Attached { target: waiting.describe(), carrier: "volume", what });
    }
    if attached.is_empty() {
        print!("{}", nothing_found());
        return ExitCode::SUCCESS;
    }

    let width = attached.iter().map(|board| board.target.len()).max().unwrap_or(0).max(6);
    println!("{:<width$}  {:<7}  {}", "TARGET", "CARRIER", "WHAT", width = width);
    for board in &attached {
        println!("{:<width$}  {:<7}  {}", board.target, board.carrier, board.what, width = width);
    }
    println!("\n{} attached. Paste a TARGET into --target.", attached.len());
    if attached.iter().any(|board| board.carrier == "volume") {
        println!(
            "A `(bootloader)` or `(volume)` row takes an image by having one COPIED to it, so it\n\
             has no --target -- nothing there speaks a protocol. `(bootloader)` means the chip is\n\
             halted waiting; `(volume)` is an on-board programmer, and that board may be running."
        );
    }

    if parsed.flag("--identify") {
        println!("\nidentifying (a board answers only while Lamella firmware is running on it):");
        for board in &attached {
            println!("\n  {}", board.target);
            for line in identify(&board.target).lines() {
                println!("    {line}");
            }
        }
    } else {
        println!(
            "`lamella devices --identify` asks each one what it is -- board, profile and chip id."
        );
    }
    ExitCode::SUCCESS
}

/// Every attached board, native-USB Lamella Link devices first and then the OS serial ports.
fn enumerate() -> Vec<Attached> {
    let mut attached = Vec::new();
    let probes = lamella_probe::list();
    for probe in &probes {
        let target = match &probe.serial {
            Some(serial) => format!("--probe {serial}"),
            None => format!("--probe ? ({:04x}:{:04x})", probe.vendor_id, probe.product_id),
        };
        let product = probe.product.clone().unwrap_or_else(|| "debug probe".to_owned());
        attached.push(Attached {
            target,
            carrier: "probe",
            what: format!("{product}  a board may be wired to this over SWD"),
        });
    }
    if let Ok(boards) = UsbTransport::list() {
        for board in boards {
            let target = match &board.serial_number {
                Some(serial) => {
                    format!("usb:{:04x}:{:04x}:{serial}", board.vendor_id, board.product_id)
                }
                None => format!("usb:{:04x}:{:04x}", board.vendor_id, board.product_id),
            };
            let what = board.product.clone().unwrap_or_else(|| "(no product string)".to_owned());
            attached.push(Attached { target, carrier: "usb", what });
        }
    }
    let ports = list_serial();
    for port in &ports {
        let (target, note) = serial_target(port, &ports);
        let mut what = describe(port);
        if port.serial_number.as_deref().is_some_and(|serial| {
            probes.iter().any(|probe| probe.serial.as_deref() == Some(serial))
        }) {
            what.push_str("  [the UART bridge of the probe above, not a board]");
        }
        if let Some(note) = note {
            what.push_str(&format!("  {note}"));
        }
        attached.push(Attached { target, carrier: "serial", what });
    }
    attached
}

/// The target to print for `port`, and a note when it is not the obvious one.
///
/// **A USB SERIAL NUMBER IS NOT UNIQUE PER PORT, AND THE FIRST BENCH THIS RAN ON PROVED IT.** One
/// MCU-Link presents TWO virtual COM ports under a single serial number, so `serial:<that number>`
/// matches both -- and the resolver refuses an ambiguous match rather than picking one, which is
/// correct. A listing that printed that target anyway would be handing the reader a string that
/// cannot be opened, which is worse than printing the unstable port name: the reader would blame
/// the board.
///
/// So the serial-based target is used only when the serial identifies exactly ONE port. Otherwise
/// the port name is the target, and the note says why, because a reader comparing two rows of this
/// listing will notice they are addressed differently and deserves the reason.
fn serial_target(
    port: &lamella_wire_host::SerialPortDesc,
    all: &[lamella_wire_host::SerialPortDesc],
) -> (String, Option<String>) {
    let Some(serial) = &port.serial_number else {
        return (port.port.clone(), None);
    };
    let sharing = all
        .iter()
        .filter(|other| other.serial_number.as_deref() == Some(serial.as_str()))
        .count();
    if sharing <= 1 {
        return (format!("serial:{serial}"), None);
    }
    (
        port.port.clone(),
        Some(format!(
            "[serial {serial} names {sharing} ports, so the port name addresses this one]"
        )),
    )
}

/// A serial port in the operating system's own terms: its port name, product string and USB ids.
fn describe(port: &lamella_wire_host::SerialPortDesc) -> String {
    let mut what = port.port.clone();
    if let Some(product) = &port.product {
        what.push_str(&format!("  {product}"));
    }
    if let (Some(vid), Some(pid)) = (port.vid, port.pid) {
        what.push_str(&format!("  ({vid:04x}:{pid:04x})"));
    } else {
        what.push_str("  (not a USB port)");
    }
    what
}

/// HELLO `target` and render what came back, or why nothing did.
///
/// The target is opened through [`open_target`], which is the ONE place that knows the target
/// syntax. Reading a target string here instead would put a second copy of that grammar behind the
/// listing that prints it, which is the one place it must not be.
fn identify(target: &str) -> String {
    let mut transport = match open_target(target, BAUD, IDENTIFY_TIMEOUT) {
        Ok(transport) => transport,
        Err(error) => return format!("cannot open it: {error:?}"),
    };
    match hello_blocking(&mut transport, 1, host_caps(), IDENTIFY_TIMEOUT) {
        Ok(negotiated) => render_identity(&negotiated),
        Err(_) => "no answer -- this is normal unless Lamella firmware is running on it".to_owned(),
    }
}

/// A generous host capability set to advertise on HELLO -- offering `PROFILE_CHIPID` so a target
/// that fills its chip identity sends it. The negotiated set is this intersected with the target's,
/// so what comes back describes the board.
fn host_caps() -> Capabilities {
    Capabilities(
        Capabilities::PROFILE_CHIPID
            | Capabilities::DEBUG_BASIC
            | Capabilities::BREAKPOINTS
            | Capabilities::STEPPING
            | Capabilities::REPL_RUN
            | Capabilities::BAKED_IMAGE
            | Capabilities::DEBUG_ATTACH,
    )
}

/// Render a negotiated identity: the board name, the profile, and the chip id where the firmware
/// reports one.
fn render_identity(negotiated: &Negotiated) -> String {
    let mut text = format!(
        "Lamella Link version {}, capabilities {:#010x}\n",
        negotiated.version, negotiated.caps.0
    );
    let Some(profile) = &negotiated.profile else {
        text.push_str("the target reported no profile identity\n");
        return text;
    };
    let board = lamella_wire::board_model::name(profile.board_model)
        .unwrap_or("(a board_model this build does not recognize)");
    text.push_str(&format!("board {board} (board_model {})\n", profile.board_model));
    text.push_str(&format!("profile {} (abi {})\n", profile.name(), profile.abi));
    if profile.chip_idcode == 0 {
        text.push_str("chip id: not reported by this firmware\n");
        return text;
    }
    text.push_str(&format!("chip IDCODE {:#010x}\n", profile.chip_idcode));
    if profile.chip_devid != 0 {
        text.push_str(&format!("chip devid {:#010x}\n", profile.chip_devid));
    }
    text
}

/// What to print when nothing is attached.
///
/// It names what was enumerated and what a board has to do to appear, because the reader's next
/// move depends on which of several unrelated things happened, and the bare sentence tells them
/// none of it.
fn nothing_found() -> String {
    "no boards found.\n\n\
     looked for: native-USB Lamella Link devices, and every serial port the operating system\n\
     reports. A board appears here as soon as it enumerates over USB -- it does NOT need Lamella\n\
     firmware on it, so an empty list means the machine cannot see the hardware at all.\n\n\
     if a board IS plugged in: the commonest cause is a charge-only USB cable, which powers the\n\
     board and carries no data. A board held in a bootloader mode may also present as a disk\n\
     rather than as a port.\n"
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The target grammar this listing PRINTS has to be the grammar the transports PARSE. They are
    /// separate crates, so nothing but a test holds them together -- and a target string that does
    /// not round-trip sends the reader to a board that cannot be opened by the name we gave them.
    #[test]
    fn every_printed_target_shape_classifies_as_the_carrier_it_names() {
        use lamella_wire_host::{TargetKind, classify_target};
        assert_eq!(classify_target("usb:39e9:0001:ABC123"), TargetKind::Usb);
        assert_eq!(classify_target("usb:39e9:0001"), TargetKind::Usb);
        assert_eq!(classify_target("serial:ABC123"), TargetKind::Serial);
        assert_eq!(classify_target("COM8"), TargetKind::Serial);
        assert_eq!(classify_target("/dev/ttyACM0"), TargetKind::Serial);
    }

    fn port(name: &str, serial: Option<&str>) -> lamella_wire_host::SerialPortDesc {
        lamella_wire_host::SerialPortDesc {
            port: name.to_owned(),
            vid: Some(0x1fc9),
            pid: Some(0x0143),
            serial_number: serial.map(str::to_owned),
            product: None,
        }
    }

    /// **MEASURED ON A REAL BENCH, WHICH IS WHERE THIS CASE CAME FROM.** An MCU-Link presents two
    /// virtual COM ports under one USB serial number, so `serial:<that number>` resolves to two
    /// ports and the resolver refuses it. Printing it anyway would give the reader a target that
    /// cannot be opened -- and they would go looking at the board.
    #[test]
    fn a_serial_number_shared_by_two_ports_is_not_printed_as_a_target() {
        let all = vec![
            port("COM11", Some("STKVKH3CMA5YD")),
            port("COM76", Some("STKVKH3CMA5YD")),
            port("COM8", Some("E6614103E760132F")),
            port("COM3", None),
        ];

        let (target, note) = serial_target(&all[0], &all);
        assert_eq!(target, "COM11", "the shared serial cannot address one port");
        assert!(note.expect("it explains itself").contains("2 ports"), "and says how many");

        let (target, note) = serial_target(&all[2], &all);
        assert_eq!(target, "serial:E6614103E760132F", "a unique serial is the stable handle");
        assert!(note.is_none(), "and needs no explanation");

        let (target, note) = serial_target(&all[3], &all);
        assert_eq!(target, "COM3", "a port with no serial has only its name");
        assert!(note.is_none());
    }

    /// A carrier this build was compiled without must be reported as such rather than as a board
    /// that would not answer -- so the listing knows which carriers it actually has.
    #[test]
    fn the_build_declares_the_carriers_it_can_open() {
        let carriers = lamella_wire_host::available_carriers();
        assert!(carriers.contains(&"serial"), "the listing enumerates serial ports: {carriers:?}");
        assert!(carriers.contains(&"usb"), "and native-USB boards: {carriers:?}");
    }

    /// **THE EMPTY CASE IS THE ONE MOST READERS WILL SEE FIRST**, and it must not read as a fault
    /// in the board. Asserting on the text keeps a later edit from shortening it back to a bare
    /// "no boards found", which is the sentence this function exists to replace.
    #[test]
    fn the_empty_listing_says_what_was_looked_for_and_what_to_check() {
        let text = nothing_found();
        assert!(text.contains("looked for"), "it says what was enumerated: {text}");
        assert!(text.contains("does NOT need Lamella"), "and that firmware is not the reason");
        assert!(text.contains("charge-only"), "and names the commonest real cause");
    }
    /// **A VERB WITH NO USAGE TEXT ANSWERS `--help` BY PRINTING NOTHING AND EXITING 0**, which
    /// reads to a person as "this tool has no help" and to a script as success.
    ///
    /// Asserting the FIRST LINE rather than the presence of a string also catches the likelier
    /// drift: a usage block copied from a neighbouring verb and not renamed.
    #[test]
    fn the_usage_opens_with_the_verb_it_belongs_to() {
        assert!(
            USAGE.starts_with("usage: lamella devices"),
            "`devices` must open with the line a reader retypes: {}",
            USAGE.lines().next().unwrap_or_default()
        );
    }

}
